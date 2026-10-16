//! Workspace label catalog with its compound-commit journal
//! (`workspace-labels/internal/catalog-store.ts`).
//!
//! A label mutation touches two files: `workspace-labels.json` (catalog) and
//! `workspaces.json` (assignments). The journal `workspace-labels.transaction.json` makes the
//! pair recoverable:
//!
//! 1. write journal (`prepared`, before/after images), then the catalog;
//! 2. the registry writes `workspaces.json`;
//! 3. rewrite the journal as `committed` (the commit point), then delete it.
//!
//! A `prepared` journal found after a failure or restart is rolled back to its before-image. A
//! `committed` one is only cleanup state. When the outcome cannot be determined the registry
//! is frozen until restart ([`StoreError::LabelStorageUncertain`]).

use crate::agent_record::JsonMap;
use crate::error::{Result, StoreError};
use crate::js_json::write_js_json_atomic;
use crate::registry::RecordMap;
use crate::workspace_registry::{LabelCommitHooks, LabelStage, WorkspaceRecord, WorkspaceRegistry};
use async_trait::async_trait;
use paseo_fs_atomic::FsyncPolicy;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LabelColor {
    Violet,
    Sky,
    Emerald,
    Orange,
    Pink,
    Indigo,
    Teal,
    Red,
    Amber,
    Blue,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LabelDefinition {
    pub name: String,
    pub color: LabelColor,
    #[serde(flatten)]
    pub extra: JsonMap,
}

impl LabelDefinition {
    pub fn new(name: &str, color: LabelColor) -> Self {
        LabelDefinition {
            name: name.into(),
            color,
            extra: JsonMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransactionPhase {
    Prepared,
    Committed,
}

/// The slice of a workspace record a label commit can change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceLabelState {
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelTransaction {
    pub phase: TransactionPhase,
    pub before_labels: Vec<LabelDefinition>,
    pub after_labels: Vec<LabelDefinition>,
    pub before_workspaces: Vec<WorkspaceLabelState>,
    pub after_workspaces: Vec<WorkspaceLabelState>,
}

/// File operations of the label store; tests inject failures through this seam.
#[async_trait]
pub trait LabelIo: Send + Sync {
    async fn write_catalog(&self, path: &Path, labels: &[LabelDefinition]) -> Result<()>;
    async fn write_transaction(&self, path: &Path, transaction: &LabelTransaction) -> Result<()>;
    async fn remove_transaction(&self, path: &Path) -> Result<()>;
}

pub struct FsLabelIo {
    pub fsync: FsyncPolicy,
}

#[async_trait]
impl LabelIo for FsLabelIo {
    async fn write_catalog(&self, path: &Path, labels: &[LabelDefinition]) -> Result<()> {
        let (path, labels, policy) = (path.to_path_buf(), labels.to_vec(), self.fsync);
        tokio::task::spawn_blocking(move || write_js_json_atomic(&path, &labels, policy))
            .await
            .map_err(|e| StoreError::Other(e.to_string()))??;
        Ok(())
    }

    async fn write_transaction(&self, path: &Path, transaction: &LabelTransaction) -> Result<()> {
        let (path, transaction, policy) = (path.to_path_buf(), transaction.clone(), self.fsync);
        tokio::task::spawn_blocking(move || write_js_json_atomic(&path, &transaction, policy))
            .await
            .map_err(|e| StoreError::Other(e.to_string()))??;
        Ok(())
    }

    async fn remove_transaction(&self, path: &Path) -> Result<()> {
        // `fs.rm` without `force`: a missing file rejects.
        tokio::fs::remove_file(path).await?;
        Ok(())
    }
}

/// What a planner decides: the next catalog, workspace rewrites and the caller's result.
pub struct LabelMutation<T> {
    pub labels: Vec<LabelDefinition>,
    pub workspace_updates: Vec<WorkspaceRecord>,
    pub result: T,
}

pub struct WorkspaceLabelCatalogStore {
    file_path: PathBuf,
    transaction_path: PathBuf,
    workspaces: Arc<WorkspaceRegistry>,
    io: Arc<dyn LabelIo>,
    labels: Mutex<Vec<LabelDefinition>>,
    blocked: AtomicBool,
    loaded: tokio::sync::Mutex<bool>,
}

struct CommitCtx<'a> {
    store: &'a WorkspaceLabelCatalogStore,
    next_labels: Mutex<Vec<LabelDefinition>>,
    transaction: Mutex<Option<LabelTransaction>>,
}

impl CommitCtx<'_> {
    fn staged(&self) -> Result<LabelTransaction> {
        self.transaction
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| StoreError::other("Workspace label transaction was not staged"))
    }
}

#[async_trait]
impl LabelCommitHooks for CommitCtx<'_> {
    async fn before_workspace_write(&self, _records: &[WorkspaceRecord]) -> Result<()> {
        let transaction = self.staged()?;
        self.store
            .io
            .write_transaction(&self.store.transaction_path, &transaction)
            .await?;
        self.store
            .io
            .write_catalog(&self.store.file_path, &transaction.after_labels)
            .await
    }

    async fn after_workspace_write(&self) -> Result<()> {
        let mut transaction = self.staged()?;
        transaction.phase = TransactionPhase::Committed;
        *self.transaction.lock().unwrap_or_else(|e| e.into_inner()) = Some(transaction.clone());
        self.store
            .io
            .write_transaction(&self.store.transaction_path, &transaction)
            .await
    }

    fn after_commit(&self) {
        let next = self
            .next_labels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        *self.store.labels.lock().unwrap_or_else(|e| e.into_inner()) = next;
    }
}

struct RecoverHooks<'a> {
    store: &'a WorkspaceLabelCatalogStore,
    labels: Vec<LabelDefinition>,
}

#[async_trait]
impl LabelCommitHooks for RecoverHooks<'_> {
    async fn before_workspace_write(&self, _records: &[WorkspaceRecord]) -> Result<()> {
        self.store
            .io
            .write_catalog(&self.store.file_path, &self.labels)
            .await
    }

    async fn after_workspace_write(&self) -> Result<()> {
        self.store
            .io
            .remove_transaction(&self.store.transaction_path)
            .await
    }

    fn after_commit(&self) {
        *self.store.labels.lock().unwrap_or_else(|e| e.into_inner()) = self.labels.clone();
    }
}

impl WorkspaceLabelCatalogStore {
    pub fn new(
        paseo_home: &Path,
        workspaces: Arc<WorkspaceRegistry>,
        io: Option<Arc<dyn LabelIo>>,
    ) -> Self {
        let projects = paseo_home.join("projects");
        Self::with_paths(
            projects.join("workspace-labels.json"),
            projects.join("workspace-labels.transaction.json"),
            workspaces,
            io,
        )
    }

    pub fn with_paths(
        file_path: PathBuf,
        transaction_path: PathBuf,
        workspaces: Arc<WorkspaceRegistry>,
        io: Option<Arc<dyn LabelIo>>,
    ) -> Self {
        WorkspaceLabelCatalogStore {
            file_path,
            transaction_path,
            workspaces,
            io: io.unwrap_or_else(|| {
                Arc::new(FsLabelIo {
                    fsync: FsyncPolicy::Off,
                })
            }),
            labels: Mutex::new(Vec::new()),
            blocked: AtomicBool::new(false),
            loaded: tokio::sync::Mutex::new(false),
        }
    }

    /// Loads the catalog, rolling back a `prepared` journal or cleaning a `committed` one.
    /// Must run before the registry serves label mutations.
    pub async fn initialize(&self) -> Result<()> {
        let mut loaded = self.loaded.lock().await;
        if *loaded {
            return Ok(());
        }
        self.load_and_recover().await?;
        *loaded = true;
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<LabelDefinition>> {
        self.initialize().await?;
        Ok(self.current_labels())
    }

    fn current_labels(&self) -> Vec<LabelDefinition> {
        self.labels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub async fn commit<T, P>(&self, planner: P) -> Result<T>
    where
        T: Send,
        P: FnOnce(&[LabelDefinition], &RecordMap<WorkspaceRecord>) -> Result<LabelMutation<T>>
            + Send,
    {
        self.initialize().await?;
        if self.blocked.load(Ordering::SeqCst) {
            return Err(StoreError::LabelStorageUncertain);
        }
        let ctx = CommitCtx {
            store: self,
            next_labels: Mutex::new(Vec::new()),
            transaction: Mutex::new(None),
        };
        let committed = self
            .workspaces
            .commit_workspace_label_mutation(
                |workspaces| {
                    let current = self.current_labels();
                    let mutation = planner(&current, workspaces)?;
                    let transaction = transaction_for(&current, &mutation, workspaces);
                    let force_persist = !mutation.workspace_updates.is_empty()
                        || !catalogs_equal(&current, &mutation.labels);
                    *ctx.next_labels.lock().unwrap_or_else(|e| e.into_inner()) = mutation.labels;
                    *ctx.transaction.lock().unwrap_or_else(|e| e.into_inner()) = Some(transaction);
                    Ok(LabelStage {
                        updates: mutation.workspace_updates,
                        result: mutation.result,
                        force_persist,
                    })
                },
                &ctx,
                true,
            )
            .await;
        match committed {
            Ok(result) => {
                let _ = self.io.remove_transaction(&self.transaction_path).await;
                Ok(result)
            }
            Err(error) => Err(self.resolve_failed_commit(error).await),
        }
    }

    /// Decides what a failed compound commit left behind. Always returns the error to raise.
    async fn resolve_failed_commit(&self, error: StoreError) -> StoreError {
        let durable = match self.read_transaction().await {
            Ok(durable) => durable,
            Err(_) => return self.block_until_restart(),
        };
        let Some(durable) = durable else {
            return error;
        };
        if durable.phase == TransactionPhase::Committed {
            return self.block_until_restart();
        }
        if self.recover(&durable).await.is_err() {
            return self.block_until_restart();
        }
        error
    }

    fn block_until_restart(&self) -> StoreError {
        self.blocked.store(true, Ordering::SeqCst);
        self.workspaces.block_all_mutations_until_restart();
        StoreError::LabelStorageUncertain
    }

    async fn load_and_recover(&self) -> Result<()> {
        if let Some(transaction) = self.read_transaction().await? {
            return self.recover(&transaction).await;
        }
        let labels = self.read_catalog().await?;
        *self.labels.lock().unwrap_or_else(|e| e.into_inner()) = labels;
        Ok(())
    }

    async fn read_catalog(&self) -> Result<Vec<LabelDefinition>> {
        match tokio::fs::read_to_string(&self.file_path).await {
            Ok(raw) => Ok(crate::js_json::from_js_json_str(&raw)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    async fn read_transaction(&self) -> Result<Option<LabelTransaction>> {
        match tokio::fs::read_to_string(&self.transaction_path).await {
            Ok(raw) => Ok(Some(crate::js_json::from_js_json_str(&raw)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    async fn recover(&self, transaction: &LabelTransaction) -> Result<()> {
        if transaction.phase == TransactionPhase::Committed {
            // The marker exists only after both data files are durable. It is cleanup state,
            // never an instruction to replay stale after-images over newer workspace writes.
            let labels = self.read_catalog().await?;
            *self.labels.lock().unwrap_or_else(|e| e.into_inner()) = labels;
            let _ = self.io.remove_transaction(&self.transaction_path).await;
            return Ok(());
        }
        let states = transaction.before_workspaces.clone();
        let hooks = RecoverHooks {
            store: self,
            labels: transaction.before_labels.clone(),
        };
        self.workspaces
            .commit_workspace_label_mutation(
                |workspaces| {
                    let updates = states
                        .iter()
                        .filter_map(|state| {
                            workspaces.get(&state.workspace_id).map(|current| {
                                let mut next = current.clone();
                                next.labels = state.labels.clone();
                                next.updated_at = state.updated_at.clone();
                                next
                            })
                        })
                        .collect();
                    Ok(LabelStage {
                        updates,
                        result: (),
                        force_persist: true,
                    })
                },
                &hooks,
                false,
            )
            .await
    }
}

fn transaction_for<T>(
    current_labels: &[LabelDefinition],
    mutation: &LabelMutation<T>,
    workspaces: &RecordMap<WorkspaceRecord>,
) -> LabelTransaction {
    let before_workspaces = mutation
        .workspace_updates
        .iter()
        .filter_map(|w| workspaces.get(&w.workspace_id).map(workspace_state))
        .collect();
    LabelTransaction {
        phase: TransactionPhase::Prepared,
        before_labels: current_labels.to_vec(),
        after_labels: mutation.labels.clone(),
        before_workspaces,
        after_workspaces: mutation
            .workspace_updates
            .iter()
            .map(workspace_state)
            .collect(),
    }
}

fn workspace_state(workspace: &WorkspaceRecord) -> WorkspaceLabelState {
    WorkspaceLabelState {
        workspace_id: workspace.workspace_id.clone(),
        labels: workspace.labels.clone(),
        updated_at: workspace.updated_at.clone(),
    }
}

fn catalogs_equal(left: &[LabelDefinition], right: &[LabelDefinition]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(a, b)| a.name == b.name && a.color == b.color)
}
