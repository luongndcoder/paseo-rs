//! `projects/projects.json` (`PersistedProjectRecordSchema`, `FileBackedProjectRegistry`).

use crate::agent_record::JsonMap;
use crate::error::{Result, StoreError};
use crate::nullable::required_nullable;
use crate::paths::are_equivalent_paths;
use crate::registry::{
    CommitHooks, FileBackedRegistry, Listeners, MutationListener, RegistryRecord, Staged,
    Subscription, WriteRecords,
};
use paseo_fs_atomic::FsyncPolicy;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectKind {
    Git,
    NonGit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRecord {
    pub project_id: String,
    pub root_path: String,
    pub kind: ProjectKind,
    pub display_name: String,
    // COMPAT(projectKey): added in v0.2.4 on 2026-07-28; remove optional after 2027-01-28.
    #[serde(default)]
    pub project_key: Option<String>,
    /// User-set override over the derived display name; reconciliation never touches it.
    #[serde(default)]
    pub custom_name: Option<String>,
    /// Identifies the project's stored custom icon; null means automatic.
    #[serde(default)]
    pub custom_icon_revision: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(deserialize_with = "required_nullable")]
    pub archived_at: Option<String>,
    #[serde(flatten)]
    pub extra: JsonMap,
}

impl ProjectRecord {
    /// `createPersistedProjectRecord`.
    pub fn new(
        project_id: &str,
        root_path: &str,
        kind: ProjectKind,
        display_name: &str,
        timestamp_created: &str,
        timestamp_updated: &str,
    ) -> Self {
        ProjectRecord {
            project_id: project_id.into(),
            root_path: root_path.into(),
            kind,
            display_name: display_name.into(),
            project_key: None,
            custom_name: None,
            custom_icon_revision: None,
            created_at: timestamp_created.into(),
            updated_at: timestamp_updated.into(),
            archived_at: None,
            extra: JsonMap::new(),
        }
    }

    /// `resolveProjectDisplayName`.
    pub fn resolved_display_name(&self) -> &str {
        self.custom_name.as_deref().unwrap_or(&self.display_name)
    }
}

impl RegistryRecord for ProjectRecord {
    fn id(&self) -> &str {
        &self.project_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationKind {
    Upsert,
    Archive,
    Remove,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectMutation {
    pub kind: MutationKind,
    pub project_id: String,
    pub project: Option<ProjectRecord>,
}

pub struct GetOrCreateByRootInput {
    pub root_path: String,
    pub kind: ProjectKind,
    pub display_name: String,
    pub project_key: Option<String>,
    pub timestamp: String,
}

/// `generateProjectId`: `prj_` + 16 lowercase hex.
pub fn generate_project_id() -> String {
    format!("prj_{}", random_hex8())
}

/// `generateWorkspaceId`: `wks_` + 16 lowercase hex.
pub fn generate_workspace_id() -> String {
    format!("wks_{}", random_hex8())
}

fn random_hex8() -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    bytes[..8].iter().map(|b| format!("{b:02x}")).collect()
}

pub struct ProjectRegistryOptions {
    pub fsync: FsyncPolicy,
    pub project_id_factory: Option<Box<dyn Fn() -> String + Send + Sync>>,
    pub write_records: Option<WriteRecords<ProjectRecord>>,
}

impl Default for ProjectRegistryOptions {
    fn default() -> Self {
        ProjectRegistryOptions {
            fsync: FsyncPolicy::Off,
            project_id_factory: None,
            write_records: None,
        }
    }
}

pub struct ProjectRegistry {
    inner: FileBackedRegistry<ProjectRecord>,
    allocation: Mutex<()>,
    id_factory: Box<dyn Fn() -> String + Send + Sync>,
    listeners: Listeners<ProjectMutation>,
}

impl ProjectRegistry {
    pub fn new(file_path: impl Into<PathBuf>) -> Self {
        Self::with_options(file_path, ProjectRegistryOptions::default())
    }

    pub fn with_options(file_path: impl Into<PathBuf>, options: ProjectRegistryOptions) -> Self {
        ProjectRegistry {
            inner: FileBackedRegistry::new(
                file_path.into(),
                "projects",
                options.fsync,
                options.write_records,
            ),
            allocation: Mutex::new(()),
            id_factory: options
                .project_id_factory
                .unwrap_or_else(|| Box::new(generate_project_id)),
            listeners: Listeners::new(),
        }
    }

    pub fn file_path(&self) -> &std::path::Path {
        self.inner.file_path()
    }

    pub async fn initialize(&self) {
        self.inner.initialize().await;
    }

    pub async fn exists_on_disk(&self) -> bool {
        self.inner.exists_on_disk().await
    }

    pub async fn list(&self) -> Vec<ProjectRecord> {
        self.inner.list().await
    }

    pub async fn get(&self, project_id: &str) -> Option<ProjectRecord> {
        self.inner.get(project_id).await
    }

    /// Picks the oldest active project whose root is lexically equivalent, refreshing its
    /// kind/projectKey when they differ. Archived-only matches are never resurrected.
    pub async fn get_or_create_active_by_root(
        &self,
        input: GetOrCreateByRootInput,
    ) -> Result<ProjectRecord> {
        let _allocation = self.allocation.lock().await;
        let mut active: Vec<ProjectRecord> = self
            .list()
            .await
            .into_iter()
            .filter(|p| {
                p.archived_at.is_none() && are_equivalent_paths(&p.root_path, &input.root_path)
            })
            .collect();
        active.sort_by(|left, right| {
            let l = crate::time::parse_js_date(&left.created_at);
            let r = crate::time::parse_js_date(&right.created_at);
            // `Date.parse(a) - Date.parse(b) || a.id.localeCompare(b.id)`; NaN falls through.
            // PARITY-QUIRK(project-id-collation): ids compare bytewise, ICU localeCompare
            // agrees for the generated `prj_<hex>` shape.
            match (l, r) {
                (Some(a), Some(b)) if a != b => a.cmp(&b),
                _ => left.project_id.cmp(&right.project_id),
            }
        });
        if let Some(existing) = active.into_iter().next() {
            if existing.kind == input.kind && existing.project_key == input.project_key {
                return Ok(existing);
            }
            let mut refreshed = existing;
            refreshed.kind = input.kind;
            refreshed.project_key = input.project_key.clone();
            refreshed.updated_at = input.timestamp.clone();
            self.upsert(refreshed.clone()).await?;
            return Ok(refreshed);
        }
        loop {
            let project_id = (self.id_factory)();
            if self.get(&project_id).await.is_some() {
                continue;
            }
            let mut record = ProjectRecord::new(
                &project_id,
                &input.root_path,
                input.kind,
                &input.display_name,
                &input.timestamp,
                &input.timestamp,
            );
            record.project_key = input.project_key.clone();
            self.upsert(record.clone()).await?;
            return Ok(record);
        }
    }

    pub async fn upsert(&self, record: ProjectRecord) -> Result<()> {
        self.inner.upsert(record.clone()).await?;
        self.notify(ProjectMutation {
            kind: MutationKind::Upsert,
            project_id: record.project_id.clone(),
            project: Some(record),
        })
        .await
    }

    pub async fn update(
        &self,
        project_id: &str,
        updater: impl FnOnce(&ProjectRecord) -> ProjectRecord + Send,
    ) -> Result<Option<ProjectRecord>> {
        let Some(project) = self.inner.update(project_id, updater).await? else {
            return Ok(None);
        };
        self.notify(ProjectMutation {
            kind: MutationKind::Upsert,
            project_id: project_id.to_string(),
            project: Some(project.clone()),
        })
        .await?;
        Ok(Some(project))
    }

    /// Archives an active project; a missing or already archived project is a silent no-op.
    pub async fn archive(&self, project_id: &str, archived_at: &str) -> Result<()> {
        let archived_at = archived_at.to_string();
        let archived = self
            .inner
            .mutate_cache(
                |records| {
                    let Some(existing) = records.get(project_id) else {
                        return Ok(Staged {
                            result: None,
                            changed: false,
                        });
                    };
                    if existing.archived_at.is_some() {
                        return Ok(Staged {
                            result: None,
                            changed: false,
                        });
                    }
                    let mut next = existing.clone();
                    next.updated_at = archived_at.clone();
                    next.archived_at = Some(archived_at);
                    records.insert(project_id.to_string(), next.clone());
                    Ok(Staged {
                        result: Some(next),
                        changed: true,
                    })
                },
                CommitHooks::default(),
            )
            .await?;
        let Some(project) = archived else {
            return Ok(());
        };
        self.notify(ProjectMutation {
            kind: MutationKind::Archive,
            project_id: project_id.to_string(),
            project: Some(project),
        })
        .await
    }

    pub async fn remove(&self, project_id: &str) -> Result<()> {
        if self.inner.remove_if_present(project_id).await?.is_none() {
            return Ok(());
        }
        self.notify(ProjectMutation {
            kind: MutationKind::Remove,
            project_id: project_id.to_string(),
            project: None,
        })
        .await
    }

    /// Central lifecycle seam for daemon-global project observers. A failing listener makes
    /// the originating call reject, after the write is durable.
    pub fn subscribe(&self, listener: MutationListener<ProjectMutation>) -> Subscription {
        self.listeners.subscribe(listener)
    }

    async fn notify(&self, mutation: ProjectMutation) -> Result<()> {
        let outcomes = self.listeners.notify(mutation).await;
        match outcomes.into_iter().find_map(|o| o.err()) {
            Some(StoreError::Listener(m)) => Err(StoreError::Listener(m)),
            Some(other) => Err(other),
            None => Ok(()),
        }
    }
}
