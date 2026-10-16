//! Port of `directory-sync/index.ts`: daemon-global latest-state sequence owner for the app
//! directory (projects, workspaces, agents).
//!
//! Pure and synchronous. Share as `Arc<DirectorySync>`; the internal `std::sync::Mutex` is never
//! held across an `.await` because no method here is async. Snapshot reads call `replace_all`
//! first, so reads mutate (and bump seqs); a read-then-respond sequence is atomic per call.
//!
//! PARITY-QUIRK(directory-removals-unfiltered): removals are returned for every tombstone after the
//! cursor, never filtered by the reader's visibility.
//! PARITY-QUIRK(directory-background-flip): a workspace/agent that flips to background disappears
//! from a non-background reader's snapshot via `replace_all` as a plain removal; the tombstone is
//! created by whichever reader's snapshot omitted it, not by the flip itself.
//! PARITY-QUIRK(directory-duplicate-observation): repeated identical observations re-emit the
//! same seq (no bump when the fingerprint is unchanged).

pub mod versioned_collection;

use std::sync::{Mutex, MutexGuard};

use serde::Serialize;
use serde_json::{Map, Value};

pub use versioned_collection::{
    CollectionRead, PublishedChange, ReadMode, ReadReason, VersionedCollection, VersionedRemoval,
    VersionedValue, DEFAULT_MAX_TOMBSTONES,
};

/// Client cursor from a fetch request. `generation` absent = no cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DirectorySyncCursor {
    pub generation: Option<String>,
    pub after_seq: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemovalPayload {
    pub id: String,
    pub seq: u64,
}

/// `sync` metadata on fetch responses. Key order: generation, headSeq, mode, reason?, removals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncMetadata {
    pub generation: String,
    #[serde(rename = "headSeq")]
    pub head_seq: u64,
    pub mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    pub removals: Vec<RemovalPayload>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProjectsSync {
    pub projects: Vec<Value>,
    pub sync: SyncMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PageInfo {
    #[serde(rename = "nextCursor")]
    pub next_cursor: Option<String>,
    #[serde(rename = "prevCursor")]
    pub prev_cursor: Option<String>,
    #[serde(rename = "hasMore")]
    pub has_more: bool,
}

impl PageInfo {
    fn empty() -> Self {
        Self {
            next_cursor: None,
            prev_cursor: None,
            has_more: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkspacesSync {
    pub entries: Vec<Value>,
    #[serde(rename = "emptyProjects")]
    pub empty_projects: Vec<Value>,
    #[serde(rename = "pageInfo")]
    pub page_info: PageInfo,
    pub sync: SyncMetadata,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentsSync {
    pub entries: Vec<Value>,
    #[serde(rename = "pageInfo")]
    pub page_info: PageInfo,
    pub sync: SyncMetadata,
}

/// A `project.update` payload.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectUpdate {
    Upsert { project: Value },
    Remove { project_id: String },
}

impl ProjectUpdate {
    /// Parses `{kind:"upsert",project}` / `{kind:"remove",projectId}`. `None` if malformed.
    pub fn from_payload(payload: &Value) -> Option<Self> {
        match payload.get("kind")?.as_str()? {
            "upsert" => Some(Self::Upsert {
                project: payload.get("project")?.clone(),
            }),
            "remove" => Some(Self::Remove {
                project_id: payload.get("projectId")?.as_str()?.to_string(),
            }),
            _ => None,
        }
    }

    pub fn to_payload(&self) -> Value {
        let mut map = Map::new();
        match self {
            Self::Upsert { project } => {
                map.insert("kind".into(), "upsert".into());
                map.insert("project".into(), project.clone());
            }
            Self::Remove { project_id } => {
                map.insert("kind".into(), "remove".into());
                map.insert("projectId".into(), project_id.clone().into());
            }
        }
        Value::Object(map)
    }

    fn id(&self) -> String {
        match self {
            Self::Upsert { project } => project_id(project),
            Self::Remove { project_id } => project_id.clone(),
        }
    }
}

fn str_at(value: &Value, path: &[&str]) -> String {
    let mut cur = value;
    for key in path {
        match cur.get(*key) {
            Some(next) => cur = next,
            None => return String::new(),
        }
    }
    cur.as_str().unwrap_or_default().to_string()
}

fn project_id(v: &Value) -> String {
    str_at(v, &["projectId"])
}
fn workspace_id(v: &Value) -> String {
    str_at(v, &["id"])
}
fn agent_entry_id(v: &Value) -> String {
    str_at(v, &["agent", "id"])
}

struct Inner {
    projects: VersionedCollection<Value>,
    workspaces: VersionedCollection<Value>,
    agents: VersionedCollection<Value>,
}

pub struct DirectorySync {
    generation: String,
    inner: Mutex<Inner>,
}

impl DirectorySync {
    pub fn new(generation: impl Into<String>) -> Self {
        Self {
            generation: generation.into(),
            inner: Mutex::new(Inner {
                projects: VersionedCollection::new(project_id),
                workspaces: VersionedCollection::new(workspace_id),
                agents: VersionedCollection::new(agent_entry_id),
            }),
        }
    }

    /// Random uuid generation, one per boot.
    pub fn with_random_generation() -> Self {
        Self::new(uuid::Uuid::new_v4().to_string())
    }

    pub fn generation(&self) -> &str {
        &self.generation
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A poisoned lock only means another thread panicked mid-call; the maps stay consistent.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn synchronize_projects(
        &self,
        snapshot: impl IntoIterator<Item = Value>,
        cursor: &DirectorySyncCursor,
    ) -> ProjectsSync {
        let mut inner = self.lock();
        inner.projects.replace_all(snapshot);
        let read = self.read(&inner.projects, cursor, false);
        ProjectsSync {
            projects: with_sync_seq(&read),
            sync: self.metadata(&read, false),
        }
    }

    pub fn synchronize_workspaces(
        &self,
        snapshot: impl IntoIterator<Item = Value>,
        cursor: &DirectorySyncCursor,
        include_background: bool,
    ) -> WorkspacesSync {
        let mut inner = self.lock();
        inner.workspaces.replace_all(snapshot);
        let read = self.read(&inner.workspaces, cursor, include_background);
        WorkspacesSync {
            entries: with_sync_seq(&read),
            empty_projects: Vec::new(),
            page_info: PageInfo::empty(),
            sync: self.metadata(&read, include_background),
        }
    }

    /// Snapshot entries are `{agent, project}`.
    pub fn synchronize_agents(
        &self,
        snapshot: impl IntoIterator<Item = Value>,
        cursor: &DirectorySyncCursor,
        include_background: bool,
    ) -> AgentsSync {
        let mut inner = self.lock();
        inner.agents.replace_all(snapshot);
        let read = self.read(&inner.agents, cursor, include_background);
        AgentsSync {
            entries: with_sync_seq(&read),
            page_info: PageInfo::empty(),
            sync: self.metadata(&read, include_background),
        }
    }

    /// Returns the payload with `generation`/`seq` appended when sequencing applies.
    pub fn sequence_project_update(&self, update: &ProjectUpdate, include_sequence: bool) -> Value {
        let mut inner = self.lock();
        match update {
            ProjectUpdate::Upsert { project } => {
                inner.projects.replace(project.clone());
            }
            ProjectUpdate::Remove { project_id } => {
                inner.projects.remove(project_id);
            }
        }
        let seq = inner.projects.sequence_for(&update.id());
        self.with_version(update.to_payload(), include_sequence, seq, false)
    }

    /// `value` None removes `id`. `payload` is returned with `generation`/`seq` merged in.
    pub fn sequence_workspace_update(
        &self,
        payload: Value,
        value: Option<Value>,
        id: &str,
        include_sequence: bool,
        include_background: bool,
    ) -> Value {
        let mut inner = self.lock();
        match value {
            Some(value) => {
                inner.workspaces.replace(value);
            }
            None => {
                inner.workspaces.remove(id);
            }
        }
        let seq = inner.workspaces.sequence_for(id);
        self.with_version(payload, include_sequence, seq, include_background)
    }

    pub fn sequence_agent_update(
        &self,
        payload: Value,
        value: Option<Value>,
        id: &str,
        include_sequence: bool,
        include_background: bool,
    ) -> Value {
        let mut inner = self.lock();
        match value {
            Some(value) => {
                inner.agents.replace(value);
            }
            None => {
                inner.agents.remove(id);
            }
        }
        let seq = inner.agents.sequence_for(id);
        self.with_version(payload, include_sequence, seq, include_background)
    }

    fn generation_for(&self, include_background: bool) -> String {
        if include_background {
            format!("{}:background", self.generation)
        } else {
            self.generation.clone()
        }
    }

    fn metadata(&self, read: &CollectionRead<Value>, include_background: bool) -> SyncMetadata {
        SyncMetadata {
            generation: self.generation_for(include_background),
            head_seq: read.head_seq,
            mode: read.mode.as_str(),
            reason: read.reason.map(ReadReason::as_str),
            removals: read
                .removals
                .iter()
                .map(|r| RemovalPayload {
                    id: r.id.clone(),
                    seq: r.seq,
                })
                .collect(),
        }
    }

    /// `{...payload, ...{generation, seq}, generation: generationFor(bg)}`. Existing keys keep
    /// their position; new keys append in order generation, seq. Non-object payloads pass through.
    fn with_version(
        &self,
        payload: Value,
        include_sequence: bool,
        seq: Option<u64>,
        include_background: bool,
    ) -> Value {
        let (true, Some(seq), Value::Object(mut map)) = (include_sequence, seq, payload.clone())
        else {
            return payload;
        };
        map.insert(
            "generation".into(),
            self.generation_for(include_background).into(),
        );
        map.insert("seq".into(), seq.into());
        Value::Object(map)
    }

    fn read(
        &self,
        collection: &VersionedCollection<Value>,
        cursor: &DirectorySyncCursor,
        include_background: bool,
    ) -> CollectionRead<Value> {
        let expected = self.generation_for(include_background);
        if cursor.generation.as_deref() != Some(expected.as_str()) {
            return collection.read_snapshot(if cursor.generation.is_none() {
                ReadReason::NoCursor
            } else {
                ReadReason::GenerationChanged
            });
        }
        match cursor.after_seq {
            None => collection.read_snapshot(ReadReason::NoCursor),
            Some(after) => collection.read_after(after),
        }
    }
}

/// `{...value, syncSeq: seq}`.
fn with_sync_seq(read: &CollectionRead<Value>) -> Vec<Value> {
    read.values
        .iter()
        .map(|entry| match &entry.value {
            Value::Object(map) => {
                let mut map = map.clone();
                map.insert("syncSeq".into(), entry.seq.into());
                Value::Object(map)
            }
            other => other.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests;
