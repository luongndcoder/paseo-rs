//! File-backed agent store (`agent/agent-storage.ts`).
//!
//! Layout: `<base>/<projectDirNameFromCwd>/<id>.json`, plus legacy `<base>/*.json` read on load.
//! Writes for one agent run through a FIFO lane; the mutation closure runs inside the lane
//! against the latest cached value so a snapshot cannot commit a stale pre-archive record.

use crate::agent_record::{AgentOwner, StoredAgentRecord};
use crate::error::{Result, StoreError};
use crate::js_json::write_js_json_atomic;
use crate::nullable::Nullable;
use crate::paths::project_dir_name_from_cwd;
use indexmap::{IndexMap, IndexSet};
use paseo_fs_atomic::FsyncPolicy;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;

#[derive(Default)]
struct State {
    cache: IndexMap<String, StoredAgentRecord>,
    path_by_id: HashMap<String, PathBuf>,
    paths_by_id: HashMap<String, IndexSet<PathBuf>>,
    daemon_agent_ids_by_execution: HashMap<String, String>,
    daemon_execution_keys_by_agent_id: HashMap<String, String>,
    deleting: HashSet<String>,
}

impl State {
    fn index_owner(&mut self, record: &StoredAgentRecord) {
        self.remove_owner_index(&record.id);
        if let Some(owner) = &record.owner {
            let key = owner.execution_key();
            if let Some(previous) = self.daemon_agent_ids_by_execution.get(&key).cloned() {
                if previous != record.id {
                    self.daemon_execution_keys_by_agent_id.remove(&previous);
                }
            }
            self.daemon_agent_ids_by_execution
                .insert(key.clone(), record.id.clone());
            self.daemon_execution_keys_by_agent_id
                .insert(record.id.clone(), key);
        }
    }

    fn remove_owner_index(&mut self, agent_id: &str) {
        let Some(key) = self
            .daemon_execution_keys_by_agent_id
            .get(agent_id)
            .cloned()
        else {
            return;
        };
        if self
            .daemon_agent_ids_by_execution
            .get(&key)
            .map(String::as_str)
            == Some(agent_id)
        {
            self.daemon_agent_ids_by_execution.remove(&key);
        }
        self.daemon_execution_keys_by_agent_id.remove(agent_id);
    }

    fn add_indexed_path(&mut self, agent_id: &str, path: &Path) {
        self.paths_by_id
            .entry(agent_id.to_string())
            .or_default()
            .insert(path.to_path_buf());
    }

    fn remove_indexed_path(&mut self, agent_id: &str, path: &Path) {
        if let Some(paths) = self.paths_by_id.get_mut(agent_id) {
            paths.shift_remove(path);
            if paths.is_empty() {
                self.paths_by_id.remove(agent_id);
            }
        }
    }
}

/// Overrides for [`AgentStore::apply_snapshot`]. `Some(x)` means the key was present in the
/// TS options object (`hasOwnProperty`), even when `x` is `None`.
#[derive(Debug, Clone, Default)]
pub struct SnapshotOptions {
    pub title: Option<Option<String>>,
    pub internal: Option<Option<bool>>,
}

/// What the projection closure of [`AgentStore::apply_snapshot`] needs besides the live agent:
/// the `ProjectionOptions` TS derives from the existing record.
#[derive(Debug, Clone)]
pub struct SnapshotInput {
    /// `options.title` when overridden, else the existing title, else none (stored as null).
    pub title: Option<String>,
    /// The existing record's `createdAt`, which wins over the agent's own.
    pub created_at: Option<String>,
    existing_internal: Option<bool>,
    internal_override: Option<Option<bool>>,
}

impl SnapshotInput {
    /// `hasInternalOverride ? options.internal : (agent.internal ?? existing?.internal)`.
    pub fn resolve_internal(&self, agent_internal: Option<bool>) -> Option<bool> {
        match self.internal_override {
            Some(value) => value,
            None => agent_internal.or(self.existing_internal),
        }
    }
}

pub struct AgentStore {
    base_dir: PathBuf,
    fsync: FsyncPolicy,
    state: Mutex<State>,
    lanes: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    loaded: OnceCell<()>,
}

impl AgentStore {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self::with_fsync(base_dir, FsyncPolicy::Off)
    }

    pub fn with_fsync(base_dir: impl Into<PathBuf>, fsync: FsyncPolicy) -> Self {
        AgentStore {
            base_dir: base_dir.into(),
            fsync,
            state: Mutex::new(State::default()),
            lanes: Mutex::new(HashMap::new()),
            loaded: OnceCell::new(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub async fn initialize(&self) -> Result<()> {
        self.load().await;
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<StoredAgentRecord>> {
        self.load().await;
        Ok(self.state().cache.values().cloned().collect())
    }

    pub async fn get(&self, agent_id: &str) -> Result<Option<StoredAgentRecord>> {
        self.load().await;
        Ok(self.state().cache.get(agent_id).cloned())
    }

    pub async fn list_by_provider_session(
        &self,
        provider: &str,
        provider_handle_id: &str,
    ) -> Result<Vec<StoredAgentRecord>> {
        self.load().await;
        let handle = Value::String(provider_handle_id.to_string());
        Ok(self
            .state()
            .cache
            .values()
            .filter(|record| match record.persistence.value() {
                Some(p) => {
                    p.provider == provider
                        && (p.session_id == provider_handle_id
                            || p.native_handle.value() == Some(&handle))
                }
                None => false,
            })
            .cloned()
            .collect())
    }

    pub async fn list_by_workspace(&self, workspace_id: &str) -> Result<Vec<StoredAgentRecord>> {
        self.load().await;
        Ok(self
            .state()
            .cache
            .values()
            .filter(|record| record.workspace_id.as_deref() == Some(workspace_id))
            .cloned()
            .collect())
    }

    pub async fn find_by_daemon_execution(
        &self,
        owner: &AgentOwner,
    ) -> Result<Option<StoredAgentRecord>> {
        self.load().await;
        let state = self.state();
        Ok(state
            .daemon_agent_ids_by_execution
            .get(&owner.execution_key())
            .and_then(|id| state.cache.get(id))
            .cloned())
    }

    /// Replaces the stored record with `record` (queued in the agent's lane).
    pub async fn upsert(&self, record: StoredAgentRecord) -> Result<()> {
        self.load().await;
        let id = record.id.clone();
        self.queue_record_mutation(&id, move |_| record).await
    }

    /// Marks an agent as being deleted: queued and future writes for it become no-ops.
    pub fn begin_delete(&self, agent_id: &str) {
        self.state().deleting.insert(agent_id.to_string());
    }

    pub async fn remove(&self, agent_id: &str) -> Result<()> {
        self.load().await;
        self.begin_delete(agent_id);
        self.wait_for_lane(agent_id).await;
        let paths: Vec<PathBuf> = self
            .state()
            .paths_by_id
            .get(agent_id)
            .map(|p| p.iter().cloned().collect())
            .unwrap_or_default();
        for path in paths {
            if let Err(error) = tokio::fs::remove_file(&path).await {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(agent_id, file = %path.display(), %error, "Failed to remove agent record file");
                }
            }
        }
        let mut state = self.state();
        state.cache.shift_remove(agent_id);
        state.remove_owner_index(agent_id);
        state.path_by_id.remove(agent_id);
        state.paths_by_id.remove(agent_id);
        Ok(())
    }

    /// Persists a managed-agent snapshot. `project` builds the record (the port of
    /// `toStoredAgentRecord`, owned by the caller) from the values derived out of the existing
    /// record. It runs inside the agent's write lane. Only `archivedAt` is carried over from
    /// the existing record afterwards.
    pub async fn apply_snapshot<F>(
        &self,
        agent_id: &str,
        options: SnapshotOptions,
        project: F,
    ) -> Result<()>
    where
        F: FnOnce(SnapshotInput) -> StoredAgentRecord + Send,
    {
        self.load().await;
        self.queue_record_mutation(agent_id, move |existing| {
            let title = match &options.title {
                Some(title) => title.clone(),
                None => existing.and_then(|e| e.title.value().cloned()),
            };
            let input = SnapshotInput {
                title,
                created_at: existing.map(|e| e.created_at.clone()),
                existing_internal: existing.and_then(|e| e.internal),
                internal_override: options.internal,
            };
            let mut record = project(input);
            if let Some(existing) = existing {
                if existing.archived_at.is_present() {
                    record.archived_at = existing.archived_at.clone();
                }
            }
            record
        })
        .await
    }

    pub async fn set_title(&self, agent_id: &str, title: &str) -> Result<()> {
        self.load().await;
        self.wait_for_lane(agent_id).await;
        let Some(mut record) = self.get(agent_id).await? else {
            return Err(StoreError::NotFound(format!("Agent {agent_id} not found")));
        };
        record.title = Nullable::Value(title.to_string());
        self.upsert(record).await
    }

    /// Waits for every write queued so far.
    pub async fn flush(&self) -> Result<()> {
        self.load().await;
        let lanes: Vec<_> = self
            .lanes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for lane in lanes {
            drop(lane.lock().await);
        }
        Ok(())
    }

    // -- lanes ------------------------------------------------------------------------------

    fn lane(&self, agent_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.lanes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(agent_id.to_string())
            .or_default()
            .clone()
    }

    fn release_lane(&self, agent_id: &str, lane: Arc<tokio::sync::Mutex<()>>) {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        // The map and `lane` are the only owners when nobody else is queued.
        if Arc::strong_count(&lane) == 2 {
            lanes.remove(agent_id);
        }
    }

    async fn wait_for_lane(&self, agent_id: &str) {
        let lane = self.lane(agent_id);
        drop(lane.lock().await);
        self.release_lane(agent_id, lane);
    }

    async fn queue_record_mutation<F>(&self, agent_id: &str, mutate: F) -> Result<()>
    where
        F: FnOnce(Option<&StoredAgentRecord>) -> StoredAgentRecord + Send,
    {
        let lane = self.lane(agent_id);
        let result = {
            let _guard = lane.lock().await;
            if self.state().deleting.contains(agent_id) {
                Ok(())
            } else {
                let record = {
                    let state = self.state();
                    mutate(state.cache.get(agent_id))
                };
                self.write_record(record).await
            }
        };
        self.release_lane(agent_id, lane);
        result
    }

    async fn write_record(&self, record: StoredAgentRecord) -> Result<()> {
        let agent_id = record.id.clone();
        let next_path = self.build_record_path(&record);
        let previous_path = self.state().path_by_id.get(&agent_id).cloned();

        let (path, policy, to_write) = (next_path.clone(), self.fsync, record.clone());
        tokio::task::spawn_blocking(move || write_js_json_atomic(&path, &to_write, policy))
            .await
            .map_err(|e| StoreError::Other(e.to_string()))??;

        self.state().add_indexed_path(&agent_id, &next_path);
        if let Some(previous) = previous_path.filter(|p| *p != next_path) {
            // cwd change: the new file is durable first, then the old one goes. Errors ignored.
            let _ = tokio::fs::remove_file(&previous).await;
            self.state().remove_indexed_path(&agent_id, &previous);
        }
        let mut state = self.state();
        state.index_owner(&record);
        state.cache.insert(agent_id.clone(), record);
        state.path_by_id.insert(agent_id, next_path);
        Ok(())
    }

    fn build_record_path(&self, record: &StoredAgentRecord) -> PathBuf {
        self.base_dir
            .join(project_dir_name_from_cwd(&record.cwd))
            .join(format!("{}.json", record.id))
    }

    // -- load -------------------------------------------------------------------------------

    async fn load(&self) {
        self.loaded.get_or_init(|| self.do_load()).await;
    }

    async fn do_load(&self) {
        let loaded = self.scan_disk().await;
        self.install_loaded(loaded);
    }

    /// Rebuilds the in-memory state.
    ///
    /// PARITY-QUIRK(duplicate-agent-files-last-wins): like TS, every file is indexed in scan
    /// order (root files first, then each project dir) and the LAST record for an id wins the
    /// cache; no file is unlinked on load. All paths stay indexed so `remove` deletes every
    /// copy, and the next write unlinks only the winner's old path when the cwd differs. TS
    /// scans in `readdir` order; here the scan is sorted, which is one legal `readdir` order.
    fn install_loaded(&self, loaded: Vec<(StoredAgentRecord, PathBuf)>) {
        let mut state = self.state();
        *state = State::default();
        for (record, path) in loaded {
            let id = record.id.clone();
            state.index_owner(&record);
            state.path_by_id.insert(id.clone(), path.clone());
            state.add_indexed_path(&id, &path);
            state.cache.insert(id, record);
        }
    }

    async fn scan_disk(&self) -> Vec<(StoredAgentRecord, PathBuf)> {
        let mut root_files: Vec<PathBuf> = Vec::new();
        let mut project_dirs: Vec<PathBuf> = Vec::new();
        let mut entries = match tokio::fs::read_dir(&self.base_dir).await {
            Ok(entries) => entries,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::error!(%error, "Failed to load agents");
                }
                return Vec::new();
            }
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            let path = entry.path();
            if file_type.is_file() && entry.file_name().to_string_lossy().ends_with(".json") {
                root_files.push(path);
            } else if file_type.is_dir() {
                project_dirs.push(path);
            }
        }
        root_files.sort();
        project_dirs.sort();

        let mut all_files = root_files;
        for dir in project_dirs {
            let Ok(mut files) = tokio::fs::read_dir(&dir).await else {
                continue;
            };
            let mut found = Vec::new();
            while let Ok(Some(entry)) = files.next_entry().await {
                let Ok(file_type) = entry.file_type().await else {
                    continue;
                };
                if file_type.is_file() && entry.file_name().to_string_lossy().ends_with(".json") {
                    found.push(entry.path());
                }
            }
            found.sort();
            all_files.extend(found);
        }

        let mut records = Vec::new();
        for path in all_files {
            if let Some(record) = read_record_file(&path).await {
                records.push((record, path));
            }
        }
        records
    }
}

async fn read_record_file(path: &Path) -> Option<StoredAgentRecord> {
    let content = match tokio::fs::read_to_string(path).await {
        Ok(content) => content,
        Err(error) => {
            tracing::error!(%error, file = %path.display(), "Skipping invalid agent record");
            return None;
        }
    };
    match crate::js_json::from_js_json_str::<StoredAgentRecord>(&content) {
        Ok(record) => Some(record),
        Err(error) => {
            tracing::error!(%error, file = %path.display(), "Skipping invalid agent record");
            None
        }
    }
}
