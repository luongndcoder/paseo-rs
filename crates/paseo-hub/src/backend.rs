//! What `DaemonExecutions` needs from the daemon: the agent store, the live manager and the
//! bound create command (TS `DaemonExecutionsOptions`). [`ExecutionBackend`] is that seam;
//! [`CoreExecutionBackend`] implements the manager and store halves over `paseo-core` and
//! delegates the rest (create with worktrees, interrupt, workspace archive) to
//! [`ExecutionActions`], which stay with their owning domains.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use paseo_core::agent_payload::{build_stored_agent_payload, to_agent_payload};
use paseo_core::{AgentManager, ManagerEvent, SubscribeOpts};
use paseo_store::{AgentOwner, AgentStore, StoredAgentRecord};
use serde_json::{Map, Value};

use crate::executions::{OwnedAgentEvent, OwnedEventListener, Unsubscribe};

/// An error crossing the execution seam. Cloneable because one create result is shared by
/// every caller that retried the same execution.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ExecError {
    /// `ProviderOptionsValidationError`.
    #[error("{message}")]
    ProviderOptionsInvalid {
        provider: String,
        issues: Vec<Value>,
        message: String,
    },
    /// `ToolPolicyUnsupportedError`.
    #[error("{message}")]
    ToolPolicyUnsupported { provider: String, message: String },
    #[error("{0}")]
    Other(String),
}

impl ExecError {
    pub fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeAction {
    BranchOff,
    Checkout,
}

/// `toCreateAgentWorktree` output.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionWorktree {
    pub action: WorktreeAction,
    pub worktree_name: Option<String>,
    pub base_branch: Option<String>,
    pub ref_name: Option<String>,
    pub github_pr_number: Option<i64>,
}

/// The worktree a create made. Only `created == true` worktrees are removed on failure.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatedWorktree {
    pub created: bool,
    pub value: Value,
}

/// Filled by the backend while it creates (`onWorktreeCreated`, `onCreated`) so the failure
/// path knows what to undo even though the create itself failed.
#[derive(Debug, Clone, Default)]
pub struct CreateProgress {
    inner: Arc<Mutex<(Option<CreatedWorktree>, Option<String>)>>,
}

impl CreateProgress {
    pub fn worktree_created(&self, worktree: CreatedWorktree) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).0 = Some(worktree);
    }

    pub fn agent_created(&self, agent_id: impl Into<String>) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).1 = Some(agent_id.into());
    }

    pub fn snapshot(&self) -> (Option<CreatedWorktree>, Option<String>) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The create command of a Hub execution. The backend adds the constants of the TS call:
/// `kind: "mcp"`, `promptFailure: "throw"`, `notifyOnFinish: false`.
#[derive(Debug, Clone)]
pub struct ExecutionCreateRequest {
    /// `provider` or `provider/model`.
    pub provider: String,
    pub title: String,
    pub initial_prompt: String,
    pub cwd: String,
    pub mode: Option<String>,
    pub thinking: Option<String>,
    pub features: Option<Value>,
    pub env: Option<Value>,
    pub mcp_servers: Option<Map<String, Value>>,
    pub provider_options: Option<Value>,
    pub tool_policy: Option<Value>,
    pub worktree: Option<ExecutionWorktree>,
    pub owner: AgentOwner,
}

#[derive(Debug, Clone)]
pub struct CreatedExecution {
    pub agent_id: String,
    pub workspace_id: Option<String>,
    /// `serializeAgentSnapshot(result.liveSnapshot)`.
    pub snapshot: Value,
}

#[async_trait]
pub trait ExecutionBackend: Send + Sync + 'static {
    /// `agentStorage.findByDaemonExecution`.
    async fn find_by_daemon_execution(
        &self,
        owner: &AgentOwner,
    ) -> Result<Option<StoredAgentRecord>, ExecError>;

    /// The bound create command. Must report the worktree and agent id through `progress`
    /// as soon as they exist.
    async fn create_agent(
        &self,
        request: ExecutionCreateRequest,
        progress: CreateProgress,
    ) -> Result<CreatedExecution, ExecError>;

    /// `agentManager.getAgent(id) != null`.
    fn is_live(&self, agent_id: &str) -> bool;
    async fn close_agent(&self, agent_id: &str) -> Result<(), ExecError>;
    async fn delete_agent_state(&self, agent_id: &str) -> Result<(), ExecError>;
    /// `cleanupFailedCreate`: archive the worktree this create made (`createdAgentId` is
    /// always null in TS). `None` when the worktree pre-existed.
    async fn cleanup_failed_create(&self, created_worktree: Option<Value>)
        -> Result<(), ExecError>;
    /// `agentStorage.remove`.
    async fn remove_agent_record(&self, agent_id: &str) -> Result<(), ExecError>;
    async fn interrupt_agent(&self, agent_id: &str) -> Result<(), ExecError>;
    async fn archive_workspace(
        &self,
        workspace_id: &str,
        request_id: &str,
    ) -> Result<(), ExecError>;
    /// `projectRecord().agent`: the live snapshot, else the stored payload with `status`
    /// forced to `closed`.
    fn project_record(&self, record: &StoredAgentRecord) -> Value;
    /// Observes the manager (with state replay) and forwards the events of agents owned by
    /// `daemon_id`.
    fn subscribe_owned(&self, daemon_id: String, listener: OwnedEventListener) -> Unsubscribe;
}

/// The parts of [`ExecutionBackend`] that live outside the manager and the store.
#[async_trait]
pub trait ExecutionActions: Send + Sync + 'static {
    async fn create_agent(
        &self,
        request: ExecutionCreateRequest,
        progress: CreateProgress,
    ) -> Result<CreatedExecution, ExecError>;
    async fn cleanup_failed_create(&self, created_worktree: Option<Value>)
        -> Result<(), ExecError>;
    async fn interrupt_agent(&self, agent_id: &str) -> Result<(), ExecError>;
    async fn archive_workspace(
        &self,
        workspace_id: &str,
        request_id: &str,
    ) -> Result<(), ExecError>;
}

/// [`ExecutionBackend`] over the real manager and store.
pub struct CoreExecutionBackend {
    manager: Arc<AgentManager>,
    store: Arc<AgentStore>,
    actions: Arc<dyn ExecutionActions>,
}

impl CoreExecutionBackend {
    pub fn new(
        manager: Arc<AgentManager>,
        store: Arc<AgentStore>,
        actions: Arc<dyn ExecutionActions>,
    ) -> Self {
        Self {
            manager,
            store,
            actions,
        }
    }
}

fn store_error(error: paseo_store::StoreError) -> ExecError {
    ExecError::other(error.to_string())
}

fn core_error(error: paseo_core::CoreError) -> ExecError {
    ExecError::other(error.to_string())
}

fn owned_execution_id(owner: Option<&AgentOwner>, daemon_id: &str) -> Option<String> {
    owner
        .filter(|o| o.daemon_id == daemon_id)
        .map(|o| o.execution_id.clone())
}

/// `projectEvent`.
fn project_event(
    manager: &AgentManager,
    daemon_id: &str,
    event: &ManagerEvent,
) -> Option<OwnedAgentEvent> {
    match event {
        ManagerEvent::AgentState { agent } => {
            let execution_id = owned_execution_id(agent.owner.as_ref(), daemon_id)?;
            Some(OwnedAgentEvent::Update {
                execution_id,
                agent: to_agent_payload(agent),
            })
        }
        ManagerEvent::AgentStream {
            agent_id, event, ..
        } => {
            // TS looks the agent up when the event is delivered, so an agent that left the
            // manager meanwhile yields nothing.
            let agent = manager.get_agent(agent_id)?;
            let execution_id = owned_execution_id(agent.owner.as_ref(), daemon_id)?;
            let wire = paseo_agent_types::to_wire(event)?;
            Some(OwnedAgentEvent::Stream {
                execution_id,
                agent_id: agent.id.clone(),
                event: wire.into_value(),
            })
        }
        _ => None,
    }
}

#[async_trait]
impl ExecutionBackend for CoreExecutionBackend {
    async fn find_by_daemon_execution(
        &self,
        owner: &AgentOwner,
    ) -> Result<Option<StoredAgentRecord>, ExecError> {
        self.store
            .find_by_daemon_execution(owner)
            .await
            .map_err(store_error)
    }

    async fn create_agent(
        &self,
        request: ExecutionCreateRequest,
        progress: CreateProgress,
    ) -> Result<CreatedExecution, ExecError> {
        self.actions.create_agent(request, progress).await
    }

    fn is_live(&self, agent_id: &str) -> bool {
        self.manager.get_agent(agent_id).is_some()
    }

    async fn close_agent(&self, agent_id: &str) -> Result<(), ExecError> {
        self.manager.close_agent(agent_id).await.map_err(core_error)
    }

    async fn delete_agent_state(&self, agent_id: &str) -> Result<(), ExecError> {
        self.manager.delete_agent_state(agent_id);
        Ok(())
    }

    async fn cleanup_failed_create(
        &self,
        created_worktree: Option<Value>,
    ) -> Result<(), ExecError> {
        self.actions.cleanup_failed_create(created_worktree).await
    }

    async fn remove_agent_record(&self, agent_id: &str) -> Result<(), ExecError> {
        self.store.remove(agent_id).await.map_err(store_error)
    }

    async fn interrupt_agent(&self, agent_id: &str) -> Result<(), ExecError> {
        self.actions.interrupt_agent(agent_id).await
    }

    async fn archive_workspace(
        &self,
        workspace_id: &str,
        request_id: &str,
    ) -> Result<(), ExecError> {
        self.actions
            .archive_workspace(workspace_id, request_id)
            .await
    }

    fn project_record(&self, record: &StoredAgentRecord) -> Value {
        if let Some(live) = self.manager.get_agent(&record.id) {
            return to_agent_payload(&live);
        }
        let ids: std::collections::HashSet<String> =
            self.manager.registered_provider_ids().into_iter().collect();
        let mut payload = build_stored_agent_payload(record, &|id| ids.contains(id));
        if let Some(map) = payload.as_object_mut() {
            map.insert("status".into(), "closed".into());
        }
        payload
    }

    fn subscribe_owned(&self, daemon_id: String, listener: OwnedEventListener) -> Unsubscribe {
        let manager = self.manager.clone();
        // The synchronous TS `subscribe` registers before it returns; here the registration
        // is a bus command, so the pump task does it. `replay_state` covers anything that
        // happened in between.
        let task = tokio::spawn(async move {
            let opts = SubscribeOpts {
                replay_state: true,
                ..SubscribeOpts::default()
            };
            let Ok(mut subscription) = manager.subscribe(opts).await else {
                return;
            };
            while let Some(event) = subscription.next().await {
                if let Some(owned) = project_event(&manager, &daemon_id, &event) {
                    listener(owned);
                }
            }
        });
        Unsubscribe::new(move || task.abort())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paseo_agent_types::AgentStreamEvent;
    use paseo_core::ManagerOptions;

    fn stream_of(agent_id: &str) -> ManagerEvent {
        ManagerEvent::AgentStream {
            agent_id: agent_id.into(),
            event: AgentStreamEvent::TurnStarted {
                provider: "codex".into(),
                turn_id: Some("t".into()),
            },
            seq: None,
            epoch: None,
            timestamp: None,
            internal: false,
        }
    }

    /// TS `projectEvent` looks the agent up when the event is delivered: an event of an agent
    /// that has left the manager (closed, archived) yields nothing, like an unowned one.
    #[tokio::test]
    async fn a_stream_event_of_an_agent_that_left_the_manager_projects_to_nothing() {
        let manager = AgentManager::new(ManagerOptions::default());
        assert_eq!(
            project_event(
                &manager,
                "daemon-1",
                &stream_of("00000000-0000-4000-8000-000000000001")
            ),
            None
        );
    }

    #[test]
    fn ownership_needs_the_same_daemon() {
        let owner = |daemon: &str| AgentOwner {
            kind: paseo_store::agent_record::OwnerKind::Daemon,
            daemon_id: daemon.into(),
            execution_id: "e".into(),
            extra: Default::default(),
        };
        assert_eq!(
            owned_execution_id(Some(&owner("daemon-1")), "daemon-1"),
            Some("e".into())
        );
        assert_eq!(
            owned_execution_id(Some(&owner("daemon-2")), "daemon-1"),
            None
        );
        assert_eq!(owned_execution_id(None, "daemon-1"), None);
    }
}
