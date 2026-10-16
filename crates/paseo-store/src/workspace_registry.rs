//! `projects/workspaces.json` (`PersistedWorkspaceRecordSchema`, `FileBackedWorkspaceRegistry`).

use crate::agent_record::JsonMap;
use crate::error::Result;
use crate::nullable::{required_nullable, strict_optional};
use crate::project_registry::MutationKind;
use crate::registry::{
    BorrowedFuture, BoxFuture, CommitHooks, FileBackedRegistry, Listeners, MutationListener,
    RecordMap, RegistryRecord, Staged, Subscription, WriteRecords,
};
use paseo_fs_atomic::FsyncPolicy;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU64;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    LocalCheckout,
    Worktree,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UntrustedSourceKind {
    ChangeRequest,
}

/// `UntrustedWorkspaceSourceSchema`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UntrustedWorkspaceSource {
    pub kind: UntrustedSourceKind,
    pub forge: String,
    pub number: NonZeroU64,
    pub head_repository: String,
    #[serde(flatten)]
    pub extra: JsonMap,
}

fn default_false() -> bool {
    false
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRecord {
    pub workspace_id: String,
    pub project_id: String,
    pub cwd: String,
    pub kind: WorkspaceKind,
    pub display_name: String,
    /// User-set title over the derived displayName; null means "use displayName".
    #[serde(default)]
    pub title: Option<String>,
    /// The worktree's git branch; only worktree workspaces carry one.
    #[serde(default)]
    pub branch: Option<String>,
    /// Exact checkout/worktree root backing `cwd` (differs for a repo subdirectory).
    #[serde(default)]
    pub worktree_root: Option<String>,
    /// Comparison base preserved across worktree deletion and restore.
    #[serde(default)]
    pub base_branch: Option<String>,
    #[serde(default = "default_false")]
    pub is_paseo_owned_worktree: bool,
    #[serde(default)]
    pub main_repo_root: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(deserialize_with = "required_nullable")]
    pub archived_at: Option<String>,
    // COMPAT(autoArchivedChangeRequestUrl): added in v0.2.6, remove optional parsing after 2027-01-31.
    #[serde(default)]
    pub auto_archived_change_request_url: Option<String>,
    #[serde(default)]
    pub pinned_at: Option<String>,
    #[serde(
        default,
        deserialize_with = "strict_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub labels: Option<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "strict_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub untrusted_source: Option<UntrustedWorkspaceSource>,
    /// Public discovery visibility; contents keep their ordinary lifecycle.
    #[serde(default = "default_false")]
    pub background: bool,
    #[serde(flatten)]
    pub extra: JsonMap,
}

impl WorkspaceRecord {
    /// `createPersistedWorkspaceRecord` with every optional field at its default.
    pub fn new(
        workspace_id: &str,
        project_id: &str,
        cwd: &str,
        kind: WorkspaceKind,
        display_name: &str,
        created_at: &str,
        updated_at: &str,
    ) -> Self {
        WorkspaceRecord {
            workspace_id: workspace_id.into(),
            project_id: project_id.into(),
            cwd: cwd.into(),
            kind,
            display_name: display_name.into(),
            title: None,
            branch: None,
            worktree_root: None,
            base_branch: None,
            is_paseo_owned_worktree: false,
            main_repo_root: None,
            created_at: created_at.into(),
            updated_at: updated_at.into(),
            archived_at: None,
            auto_archived_change_request_url: None,
            pinned_at: None,
            labels: None,
            untrusted_source: None,
            background: false,
            extra: JsonMap::new(),
        }
    }

    /// `resolveWorkspaceDisplayName`: the title always wins over the derived name.
    pub fn resolved_display_name(&self) -> &str {
        resolve_workspace_name(self.title.as_deref(), &self.display_name)
    }
}

/// `resolveWorkspaceName`.
pub fn resolve_workspace_name<'a>(
    title: Option<&'a str>,
    derived_display_name: &'a str,
) -> &'a str {
    title.unwrap_or(derived_display_name)
}

impl RegistryRecord for WorkspaceRecord {
    fn id(&self) -> &str {
        &self.workspace_id
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceMutation {
    pub kind: MutationKind,
    pub workspace_id: String,
    pub workspace: Option<WorkspaceRecord>,
    pub expects_initial_agent: bool,
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceMutationContext {
    pub expects_initial_agent: bool,
}

#[derive(Debug, Clone, Default)]
pub struct WorkspaceArchiveContext {
    pub auto_archived_change_request_url: Option<String>,
}

/// What a label mutation stages: replacement records, the caller's result and whether to
/// write even with no record change (catalog-only edits).
pub struct LabelStage<T> {
    pub updates: Vec<WorkspaceRecord>,
    pub result: T,
    pub force_persist: bool,
}

/// Callbacks around the durable workspace write of a compound label commit.
#[async_trait::async_trait]
pub trait LabelCommitHooks: Send + Sync {
    async fn before_workspace_write(&self, records: &[WorkspaceRecord]) -> Result<()>;
    async fn after_workspace_write(&self) -> Result<()>;
    fn after_commit(&self);
}

#[derive(Default)]
pub struct WorkspaceRegistryOptions {
    pub fsync: Option<FsyncPolicy>,
    pub write_records: Option<WriteRecords<WorkspaceRecord>>,
}

pub struct WorkspaceRegistry {
    inner: FileBackedRegistry<WorkspaceRecord>,
    listeners: Listeners<WorkspaceMutation>,
}

impl WorkspaceRegistry {
    pub fn new(file_path: impl Into<PathBuf>) -> Self {
        Self::with_options(file_path, WorkspaceRegistryOptions::default())
    }

    pub fn with_options(file_path: impl Into<PathBuf>, options: WorkspaceRegistryOptions) -> Self {
        WorkspaceRegistry {
            inner: FileBackedRegistry::new(
                file_path.into(),
                "workspaces",
                options.fsync.unwrap_or(FsyncPolicy::Off),
                options.write_records,
            ),
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

    pub async fn list(&self) -> Vec<WorkspaceRecord> {
        self.inner.list().await
    }

    pub async fn get(&self, workspace_id: &str) -> Option<WorkspaceRecord> {
        self.inner.get(workspace_id).await
    }

    /// Central lifecycle seam for daemon-global workspace observers. Listener failures are
    /// logged and never fail the mutation.
    pub fn subscribe(&self, listener: MutationListener<WorkspaceMutation>) -> Subscription {
        self.listeners.subscribe(listener)
    }

    /// Convenience over [`subscribe`]: every mutation is forwarded, in commit order per
    /// publisher, to an unbounded channel.
    pub fn subscribe_channel(
        &self,
    ) -> (
        Subscription,
        tokio::sync::mpsc::UnboundedReceiver<WorkspaceMutation>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let subscription = self.subscribe(std::sync::Arc::new(move |mutation| {
            let tx = tx.clone();
            Box::pin(async move {
                let _ = tx.send(mutation);
                Ok(())
            }) as BoxFuture<Result<()>>
        }));
        (subscription, rx)
    }

    pub async fn update(
        &self,
        workspace_id: &str,
        updater: impl FnOnce(&WorkspaceRecord) -> WorkspaceRecord + Send,
    ) -> Result<Option<WorkspaceRecord>> {
        let workspace = self.inner.update(workspace_id, updater).await?;
        if let Some(workspace) = &workspace {
            self.notify(WorkspaceMutation {
                kind: MutationKind::Upsert,
                workspace_id: workspace_id.to_string(),
                workspace: Some(workspace.clone()),
                expects_initial_agent: false,
            })
            .await;
        }
        Ok(workspace)
    }

    /// Every upsert writes, even when the record is identical (parity), and notifies.
    pub async fn upsert(
        &self,
        record: WorkspaceRecord,
        context: Option<WorkspaceMutationContext>,
    ) -> Result<()> {
        self.inner.upsert(record.clone()).await?;
        self.notify(WorkspaceMutation {
            kind: MutationKind::Upsert,
            workspace_id: record.workspace_id.clone(),
            workspace: Some(record),
            expects_initial_agent: context.is_some_and(|c| c.expects_initial_agent),
        })
        .await;
        Ok(())
    }

    /// Archives (or re-archives, refreshing the timestamps) an existing workspace.
    pub async fn archive(
        &self,
        workspace_id: &str,
        archived_at: &str,
        context: Option<WorkspaceArchiveContext>,
    ) -> Result<()> {
        let archived_at = archived_at.to_string();
        let url = context.and_then(|c| c.auto_archived_change_request_url);
        let workspace = self
            .inner
            .update(workspace_id, move |existing| {
                let mut next = existing.clone();
                next.updated_at = archived_at.clone();
                next.archived_at = Some(archived_at);
                if let Some(url) = url.filter(|u| !u.is_empty()) {
                    next.auto_archived_change_request_url = Some(url);
                }
                next
            })
            .await?;
        let Some(workspace) = workspace else {
            return Ok(());
        };
        self.notify(WorkspaceMutation {
            kind: MutationKind::Archive,
            workspace_id: workspace_id.to_string(),
            workspace: Some(workspace),
            expects_initial_agent: false,
        })
        .await;
        Ok(())
    }

    pub async fn remove(&self, workspace_id: &str) -> Result<()> {
        if self.inner.remove_if_present(workspace_id).await?.is_none() {
            return Ok(());
        }
        self.notify(WorkspaceMutation {
            kind: MutationKind::Remove,
            workspace_id: workspace_id.to_string(),
            workspace: None,
            expects_initial_agent: false,
        })
        .await;
        Ok(())
    }

    /// Writes all `changed` records in one array write and swaps the cache. Like TS
    /// `mutateMany` it publishes nothing.
    pub async fn mutate_many(
        &self,
        updater: impl FnOnce(&RecordMap<WorkspaceRecord>) -> Vec<WorkspaceRecord> + Send,
    ) -> Result<Vec<WorkspaceRecord>> {
        self.inner.mutate_many(updater).await
    }

    /// One registry mutation that also drives the label catalog write: `hooks.before` runs
    /// before the workspace file is written, `after_workspace_write` after it, and
    /// `after_commit` once the cache is swapped. Updated workspaces are published afterwards
    /// unless `publish` is false.
    pub async fn commit_workspace_label_mutation<T: Send>(
        &self,
        stage: impl FnOnce(&RecordMap<WorkspaceRecord>) -> Result<LabelStage<T>> + Send,
        hooks: &dyn LabelCommitHooks,
        publish: bool,
    ) -> Result<T> {
        let mut changed: Vec<WorkspaceRecord> = Vec::new();
        let changed_ref = &mut changed;
        let before = |records: Vec<WorkspaceRecord>| -> BorrowedFuture<'_, Result<()>> {
            Box::pin(async move { hooks.before_workspace_write(&records).await })
        };
        let after = || -> BorrowedFuture<'_, Result<()>> {
            Box::pin(async move { hooks.after_workspace_write().await })
        };
        let commit = || hooks.after_commit();
        let force = |staged: &(T, bool)| staged.1;
        let committed = self
            .inner
            .mutate_cache(
                |records| {
                    let staged = stage(records)?;
                    for record in &staged.updates {
                        records.insert(record.workspace_id.clone(), record.clone());
                    }
                    let changed_any = !staged.updates.is_empty();
                    *changed_ref = staged.updates;
                    Ok(Staged {
                        result: (staged.result, staged.force_persist),
                        changed: changed_any,
                    })
                },
                CommitHooks {
                    force_persist: Some(&force),
                    before_write: Some(&before),
                    after_write: Some(&after),
                    after_commit: Some(&commit),
                },
            )
            .await?;
        if publish {
            for workspace in changed {
                self.notify(WorkspaceMutation {
                    kind: MutationKind::Upsert,
                    workspace_id: workspace.workspace_id.clone(),
                    workspace: Some(workspace),
                    expects_initial_agent: false,
                })
                .await;
            }
        }
        Ok(committed.0)
    }

    /// Stops every later mutation until the daemon restarts (uncertain label storage).
    pub fn block_all_mutations_until_restart(&self) {
        self.inner.freeze_mutations_until_restart();
    }

    async fn notify(&self, mutation: WorkspaceMutation) {
        for outcome in self.listeners.notify(mutation.clone()).await {
            if let Err(error) = outcome {
                // Publication happens after the registry commit and cannot make durable
                // state fail.
                tracing::error!(%error, workspace_id = %mutation.workspace_id, "Workspace mutation listener failed");
            }
        }
    }
}
