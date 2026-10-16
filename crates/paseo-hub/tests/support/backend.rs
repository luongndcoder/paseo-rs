use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use indexmap::IndexMap;
use paseo_hub::{
    CreateProgress, CreatedExecution, CreatedWorktree, ExecError, ExecutionBackend,
    ExecutionCreateRequest, OwnedAgentEvent, OwnedEventListener, Unsubscribe,
};
use paseo_store::{AgentOwner, StoredAgentRecord};
use serde_json::{json, Value};
use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq)]
pub enum Operation {
    CloseAgent(String),
    DeleteAgentState(String),
    CleanupFailedCreate(Option<Value>),
    RemoveRecord(String),
    Interrupt(String),
    ArchiveWorkspace(String, String),
}

#[derive(Default)]
struct State {
    records: IndexMap<String, StoredAgentRecord>,
    live: HashSet<String>,
    operations: Vec<Operation>,
    requests: Vec<ExecutionCreateRequest>,
    listeners: Vec<(u64, OwnedEventListener)>,
    next_listener: u64,
    next_agent: u64,
    /// Report a created worktree for requests that carry one.
    created_worktree: bool,
    fail_create: Option<ExecError>,
    fail_create_early: Option<ExecError>,
    fail_close: bool,
    fail_cleanup: bool,
    fail_remove: bool,
    reuse_worktree: bool,
    no_workspace: bool,
    fail_interrupt: bool,
}

/// In-memory agent store + manager + create command behind [`ExecutionBackend`].
pub struct FakeBackend {
    state: Arc<Mutex<State>>,
    pub creation_attempts: AtomicUsize,
    gate: watch::Sender<bool>,
    interrupt_gate: watch::Sender<bool>,
}

impl FakeBackend {
    pub fn new() -> Arc<Self> {
        let (gate, _) = watch::channel(true);
        let (interrupt_gate, _) = watch::channel(true);
        Arc::new(Self {
            state: Arc::new(Mutex::new(State {
                created_worktree: true,
                ..State::default()
            })),
            creation_attempts: AtomicUsize::new(0),
            gate,
            interrupt_gate,
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn hold_creation(&self) {
        self.gate.send_replace(false);
    }

    pub fn finish_creation(&self) {
        self.gate.send_replace(true);
    }

    pub fn creation_attempts(&self) -> usize {
        self.creation_attempts.load(Ordering::SeqCst)
    }

    pub fn fail_create_with(&self, error: ExecError) {
        self.state().fail_create = Some(error);
    }

    /// The create fails before it makes a worktree or an agent.
    pub fn fail_create_early_with(&self, error: ExecError) {
        self.state().fail_create_early = Some(error);
    }

    pub fn fail_close(&self) {
        self.state().fail_close = true;
    }

    pub fn fail_cleanup(&self) {
        self.state().fail_cleanup = true;
    }

    pub fn fail_remove(&self) {
        self.state().fail_remove = true;
    }

    pub fn without_workspace(&self) {
        self.state().no_workspace = true;
    }

    pub fn hold_interrupt(&self) {
        self.interrupt_gate.send_replace(false);
    }

    pub fn release_interrupt(&self) {
        self.interrupt_gate.send_replace(true);
    }

    pub fn fail_interrupt(&self) {
        self.state().fail_interrupt = true;
    }

    /// A worktree the create reuses: reported with `created: false`.
    pub fn reuse_worktree(&self) {
        self.state().reuse_worktree = true;
    }

    pub fn operations(&self) -> Vec<Operation> {
        self.state().operations.clone()
    }

    pub fn requests(&self) -> Vec<ExecutionCreateRequest> {
        self.state().requests.clone()
    }

    pub fn durable_agent_ids(&self) -> Vec<String> {
        self.state().records.keys().cloned().collect()
    }

    pub fn live_agent_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.state().live.iter().cloned().collect();
        ids.sort();
        ids
    }

    pub fn record(&self, agent_id: &str) -> Option<StoredAgentRecord> {
        self.state().records.get(agent_id).cloned()
    }

    /// Insert a record directly (a foreign execution, or one left by a previous daemon run).
    pub fn insert_record(&self, record: StoredAgentRecord) {
        self.state().records.insert(record.id.clone(), record);
    }

    /// A daemon restart: live agents are gone, records stay.
    pub fn restart(&self) {
        self.state().live.clear();
    }

    pub fn listener_count(&self) -> usize {
        self.state().listeners.len()
    }

    pub fn emit(&self, event: OwnedAgentEvent) {
        let listeners: Vec<OwnedEventListener> = self
            .state()
            .listeners
            .iter()
            .map(|(_, l)| l.clone())
            .collect();
        for listener in listeners {
            listener(event.clone());
        }
    }
}

fn owned_by(record: &StoredAgentRecord, owner: &AgentOwner) -> bool {
    record
        .owner
        .as_ref()
        .is_some_and(|o| o.execution_key() == owner.execution_key())
}

#[async_trait]
impl ExecutionBackend for FakeBackend {
    async fn find_by_daemon_execution(
        &self,
        owner: &AgentOwner,
    ) -> Result<Option<StoredAgentRecord>, ExecError> {
        // Yield like a file read would, so interleavings are observable.
        tokio::task::yield_now().await;
        Ok(self
            .state()
            .records
            .values()
            .find(|r| owned_by(r, owner))
            .cloned())
    }

    async fn create_agent(
        &self,
        request: ExecutionCreateRequest,
        progress: CreateProgress,
    ) -> Result<CreatedExecution, ExecError> {
        self.creation_attempts.fetch_add(1, Ordering::SeqCst);
        let (id, workspace_id) = {
            let mut state = self.state();
            state.requests.push(request.clone());
            if let Some(error) = state.fail_create_early.take() {
                return Err(error);
            }
            state.next_agent += 1;
            let id = format!("agent-{}", state.next_agent);
            let workspace_id = format!("workspace-{}", state.next_agent);
            if request.worktree.is_some() {
                let created = !state.reuse_worktree && state.created_worktree;
                progress.worktree_created(CreatedWorktree {
                    created,
                    value: json!({ "path": format!("/worktrees/{id}"), "created": created }),
                });
            }
            let mut record = StoredAgentRecord::new(
                &id,
                &request.provider,
                &request.cwd,
                "2026-07-13T00:00:00.000Z",
            );
            record.workspace_id = (!state.no_workspace).then(|| workspace_id.clone());
            record.owner = Some(request.owner.clone());
            state.records.insert(id.clone(), record);
            state.live.insert(id.clone());
            progress.agent_created(id.clone());
            (id, workspace_id)
        };
        let mut gate = self.gate.subscribe();
        let _ = gate.wait_for(|open| *open).await;
        if let Some(error) = self.state().fail_create.take() {
            return Err(error);
        }
        Ok(CreatedExecution {
            agent_id: id.clone(),
            workspace_id: (!self.state().no_workspace).then(|| workspace_id.clone()),
            snapshot: json!({ "id": id, "status": "running", "workspaceId": workspace_id, "provider": request.provider }),
        })
    }

    fn is_live(&self, agent_id: &str) -> bool {
        self.state().live.contains(agent_id)
    }

    async fn close_agent(&self, agent_id: &str) -> Result<(), ExecError> {
        let mut state = self.state();
        state
            .operations
            .push(Operation::CloseAgent(agent_id.into()));
        if state.fail_close {
            return Err(ExecError::other("provider close rejected"));
        }
        state.live.remove(agent_id);
        Ok(())
    }

    async fn delete_agent_state(&self, agent_id: &str) -> Result<(), ExecError> {
        let mut state = self.state();
        state
            .operations
            .push(Operation::DeleteAgentState(agent_id.into()));
        state.live.remove(agent_id);
        Ok(())
    }

    async fn cleanup_failed_create(
        &self,
        created_worktree: Option<Value>,
    ) -> Result<(), ExecError> {
        let mut state = self.state();
        state
            .operations
            .push(Operation::CleanupFailedCreate(created_worktree));
        if state.fail_cleanup {
            return Err(ExecError::other("worktree cleanup failed"));
        }
        Ok(())
    }

    async fn remove_agent_record(&self, agent_id: &str) -> Result<(), ExecError> {
        let mut state = self.state();
        state
            .operations
            .push(Operation::RemoveRecord(agent_id.into()));
        if state.fail_remove {
            return Err(ExecError::other("record removal failed"));
        }
        state.records.shift_remove(agent_id);
        Ok(())
    }

    async fn interrupt_agent(&self, agent_id: &str) -> Result<(), ExecError> {
        let mut gate = self.interrupt_gate.subscribe();
        let _ = gate.wait_for(|open| *open).await;
        let mut state = self.state();
        state.operations.push(Operation::Interrupt(agent_id.into()));
        if state.fail_interrupt {
            state.fail_interrupt = false;
            return Err(ExecError::other("interrupt failed"));
        }
        Ok(())
    }

    async fn archive_workspace(
        &self,
        workspace_id: &str,
        request_id: &str,
    ) -> Result<(), ExecError> {
        tokio::task::yield_now().await;
        let mut state = self.state();
        state.operations.push(Operation::ArchiveWorkspace(
            workspace_id.into(),
            request_id.into(),
        ));
        let ids: Vec<String> = state
            .records
            .values()
            .filter(|r| r.workspace_id.as_deref() == Some(workspace_id))
            .map(|r| r.id.clone())
            .collect();
        for id in ids {
            state.live.remove(&id);
            if let Some(record) = state.records.get_mut(&id) {
                record.archived_at =
                    paseo_store::Nullable::Value("2026-07-13T00:00:01.000Z".into());
            }
        }
        Ok(())
    }

    fn project_record(&self, record: &StoredAgentRecord) -> Value {
        let status = if self.state().live.contains(&record.id) {
            "idle"
        } else {
            "closed"
        };
        json!({ "id": record.id, "status": status, "workspaceId": record.workspace_id })
    }

    fn subscribe_owned(&self, _daemon_id: String, listener: OwnedEventListener) -> Unsubscribe {
        let id = {
            let mut state = self.state();
            state.next_listener += 1;
            let id = state.next_listener;
            state.listeners.push((id, listener));
            id
        };
        let shared = self.state.clone();
        Unsubscribe::new(move || {
            shared
                .lock()
                .unwrap()
                .listeners
                .retain(|(listener_id, _)| *listener_id != id);
        })
    }
}
