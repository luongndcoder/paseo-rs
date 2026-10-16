//! `DaemonExecutions` (TS `daemon-executions.ts`): the Hub's idempotent, authority-fenced view
//! of the agents a daemon runs for it. One execution is one agent, keyed by
//! `daemonId \0 executionId` and found again after a restart through the agent store.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::future::BoxFuture;
use paseo_store::{AgentOwner, StoredAgentRecord};
use serde_json::{Map, Value};

use crate::backend::{
    CreateProgress, ExecError, ExecutionBackend, ExecutionCreateRequest, ExecutionWorktree,
};
use crate::pending::{self, Pending};

pub const AUTHORITY_INACTIVE: &str = "Hub relationship authority is no longer active";

/// `HubExecutionAgentCreateInput`. Opaque provider config stays JSON so it reaches the
/// backend byte for byte.
#[derive(Debug, Clone, Default)]
pub struct HubExecutionAgentCreateInput {
    pub execution_id: String,
    pub provider: String,
    pub cwd: String,
    pub prompt: String,
    pub model: Option<String>,
    pub mode_id: Option<String>,
    pub thinking_option_id: Option<String>,
    pub feature_values: Option<Value>,
    pub provider_options: Option<Value>,
    /// `{ preapproved: [{ kind: "mcp", server, tool }] }`.
    pub tool_policy: Option<Value>,
    pub env: Option<Value>,
    pub mcp_servers: Option<Map<String, Value>>,
    pub worktree: Option<ExecutionWorktree>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlAction {
    Interrupt,
    Archive,
}

#[derive(Debug, Clone)]
pub struct HubExecutionControlInput {
    pub request_id: String,
    pub execution_id: String,
    pub action: ControlAction,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OwnedAgentSnapshot {
    pub execution_id: String,
    pub agent: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OwnedAgentEvent {
    Update {
        execution_id: String,
        agent: Value,
    },
    Stream {
        execution_id: String,
        agent_id: String,
        event: Value,
    },
}

/// Cancels an observation when called or dropped.
pub struct Unsubscribe(Option<Box<dyn FnOnce() + Send>>);

impl Unsubscribe {
    pub fn new(cancel: impl FnOnce() + Send + 'static) -> Self {
        Self(Some(Box::new(cancel)))
    }

    pub fn call(mut self) {
        if let Some(cancel) = self.0.take() {
            cancel();
        }
    }
}

impl Drop for Unsubscribe {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            cancel();
        }
    }
}

pub type OwnedEventListener = Arc<dyn Fn(OwnedAgentEvent) + Send + Sync>;

/// `HubExecutionAgents`.
#[async_trait::async_trait]
pub trait HubExecutionAgents: Send + Sync + 'static {
    async fn create(
        &self,
        input: HubExecutionAgentCreateInput,
    ) -> Result<OwnedAgentSnapshot, ExecError>;

    async fn control(&self, input: HubExecutionControlInput) -> Result<(), ExecError>;

    fn subscribe(&self, listener: OwnedEventListener) -> Unsubscribe;

    /// Fences every create/control that started under the current authority. The fence is
    /// raised synchronously when this is called (the relationship actor drops the returned
    /// future when it does not care about the drain); awaiting the future waits for the
    /// in-flight operations to settle.
    fn invalidate_authority(&self) -> BoxFuture<'static, ()>;
}

type CreateResult = Result<OwnedAgentSnapshot, ExecError>;
type ControlResult = Result<(), ExecError>;

#[derive(Default)]
struct State {
    next_id: u64,
    pending_creates: HashMap<String, (u64, Pending<CreateResult>)>,
    pending_control_actions: HashMap<String, (u64, Pending<ControlResult>)>,
    control_tails: HashMap<String, (u64, Pending<ControlResult>)>,
    authority_generation: u64,
    authority_inactive: bool,
}

struct Inner {
    daemon_id: String,
    backend: Arc<dyn ExecutionBackend>,
    state: Mutex<State>,
}

#[derive(Clone)]
pub struct DaemonExecutions {
    inner: Arc<Inner>,
}

impl DaemonExecutions {
    pub fn new(daemon_id: impl Into<String>, backend: Arc<dyn ExecutionBackend>) -> Self {
        Self {
            inner: Arc::new(Inner {
                daemon_id: daemon_id.into(),
                backend,
                state: Mutex::new(State::default()),
            }),
        }
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

#[async_trait::async_trait]
impl HubExecutionAgents for DaemonExecutions {
    async fn create(&self, input: HubExecutionAgentCreateInput) -> CreateResult {
        let inner = &self.inner;
        let owner = inner.owner(&input.execution_id);
        let key = owner.execution_key();
        let pending = {
            let mut state = lock(&inner.state);
            if state.authority_inactive {
                return Err(ExecError::other(AUTHORITY_INACTIVE));
            }
            if let Some((_, existing)) = state.pending_creates.get(&key) {
                existing.clone()
            } else {
                let generation = state.authority_generation;
                let id = state.next_id;
                state.next_id += 1;
                let (tx, pending) = Pending::channel();
                state
                    .pending_creates
                    .insert(key.clone(), (id, pending.clone()));
                let work_inner = inner.clone();
                let cleanup_inner = inner.clone();
                pending::run(
                    tx,
                    async move { work_inner.create_or_resolve(owner, input, generation).await },
                    move || {
                        let mut state = lock(&cleanup_inner.state);
                        if state
                            .pending_creates
                            .get(&key)
                            .is_some_and(|(i, _)| *i == id)
                        {
                            state.pending_creates.remove(&key);
                        }
                    },
                );
                pending
            }
        };
        pending
            .wait()
            .await
            .unwrap_or_else(|| Err(ExecError::other("Hub execution create was dropped")))
    }

    async fn control(&self, input: HubExecutionControlInput) -> ControlResult {
        let inner = &self.inner;
        let owner = inner.owner(&input.execution_id);
        let execution_key = owner.execution_key();
        let action_key = format!(
            "{execution_key}\0{}",
            match input.action {
                ControlAction::Interrupt => "interrupt",
                ControlAction::Archive => "archive",
            }
        );
        let pending = {
            let mut state = lock(&inner.state);
            if state.authority_inactive {
                return Err(ExecError::other(AUTHORITY_INACTIVE));
            }
            if let Some((_, existing)) = state.pending_control_actions.get(&action_key) {
                existing.clone()
            } else {
                // Order after the previous control of this execution, else after its create.
                let previous_control = state
                    .control_tails
                    .get(&execution_key)
                    .map(|(_, p)| p.clone());
                let previous_create = if previous_control.is_none() {
                    state
                        .pending_creates
                        .get(&execution_key)
                        .map(|(_, p)| p.clone())
                } else {
                    None
                };
                let generation = state.authority_generation;
                let id = state.next_id;
                state.next_id += 1;
                let (tx, pending) = Pending::channel();
                state
                    .pending_control_actions
                    .insert(action_key.clone(), (id, pending.clone()));
                state
                    .control_tails
                    .insert(execution_key.clone(), (id, pending.clone()));
                let work_inner = inner.clone();
                let cleanup_inner = inner.clone();
                pending::run(
                    tx,
                    async move {
                        // A failed predecessor never blocks the next control.
                        if let Some(previous) = previous_control {
                            let _ = previous.wait().await;
                        } else if let Some(previous) = previous_create {
                            let _ = previous.wait().await;
                        }
                        work_inner.control_owned(owner, input, generation).await
                    },
                    move || {
                        let mut state = lock(&cleanup_inner.state);
                        if state
                            .pending_control_actions
                            .get(&action_key)
                            .is_some_and(|(i, _)| *i == id)
                        {
                            state.pending_control_actions.remove(&action_key);
                        }
                        if state
                            .control_tails
                            .get(&execution_key)
                            .is_some_and(|(i, _)| *i == id)
                        {
                            state.control_tails.remove(&execution_key);
                        }
                    },
                );
                pending
            }
        };
        pending
            .wait()
            .await
            .unwrap_or_else(|| Err(ExecError::other("Hub execution control was dropped")))
    }

    fn subscribe(&self, listener: OwnedEventListener) -> Unsubscribe {
        self.inner
            .backend
            .subscribe_owned(self.inner.daemon_id.clone(), listener)
    }

    fn invalidate_authority(&self) -> BoxFuture<'static, ()> {
        let (waits, controls): (Vec<Pending<CreateResult>>, Vec<Pending<ControlResult>>) = {
            let mut state = lock(&self.inner.state);
            state.authority_inactive = true;
            state.authority_generation += 1;
            (
                state
                    .pending_creates
                    .values()
                    .map(|(_, p)| p.clone())
                    .collect(),
                state
                    .pending_control_actions
                    .values()
                    .map(|(_, p)| p.clone())
                    .collect(),
            )
        };
        Box::pin(async move {
            for pending in waits {
                let _ = pending.wait().await;
            }
            for pending in controls {
                let _ = pending.wait().await;
            }
        })
    }
}

impl Inner {
    fn owner(&self, execution_id: &str) -> AgentOwner {
        AgentOwner {
            kind: paseo_store::agent_record::OwnerKind::Daemon,
            daemon_id: self.daemon_id.clone(),
            execution_id: execution_id.to_owned(),
            extra: Default::default(),
        }
    }

    fn require_authority(&self, generation: u64, operation: &str) -> Result<(), ExecError> {
        let state = lock(&self.state);
        if state.authority_inactive || generation != state.authority_generation {
            return Err(ExecError::other(format!(
                "Hub relationship authority ended during {operation}"
            )));
        }
        Ok(())
    }

    async fn create_or_resolve(
        &self,
        owner: AgentOwner,
        input: HubExecutionAgentCreateInput,
        generation: u64,
    ) -> CreateResult {
        const OP: &str = "agent creation";
        if let Some(existing) = self.backend.find_by_daemon_execution(&owner).await? {
            require_execution_workspace_id(&existing.id, existing.workspace_id.as_deref())?;
            self.require_authority(generation, OP)?;
            return self.project_record(&existing);
        }
        self.require_authority(generation, OP)?;
        require_hub_mcp_namespace(input.mcp_servers.as_ref())?;
        require_tool_policy_servers(input.tool_policy.as_ref(), input.mcp_servers.as_ref())?;

        let progress = CreateProgress::default();
        let outcome = self
            .create_checked(&owner, input, generation, progress.clone())
            .await;
        match outcome {
            Ok(snapshot) => Ok(snapshot),
            Err(error) => {
                // PARITY-QUIRK(cleanup-error-masks-create-error): a cleanup step that fails
                // replaces the create error, as an exception thrown inside TS `finally` does.
                self.cleanup_failed_create(&progress).await?;
                Err(error)
            }
        }
    }

    async fn create_checked(
        &self,
        owner: &AgentOwner,
        input: HubExecutionAgentCreateInput,
        generation: u64,
        progress: CreateProgress,
    ) -> CreateResult {
        let request = ExecutionCreateRequest {
            provider: match &input.model {
                Some(model) if !model.is_empty() => format!("{}/{model}", input.provider),
                _ => input.provider.clone(),
            },
            title: input.prompt.clone(),
            initial_prompt: input.prompt,
            cwd: input.cwd,
            mode: input.mode_id,
            thinking: input.thinking_option_id,
            features: input.feature_values,
            env: input.env,
            mcp_servers: input.mcp_servers,
            provider_options: input.provider_options,
            tool_policy: input.tool_policy,
            worktree: input.worktree,
            owner: owner.clone(),
        };
        let created = self.backend.create_agent(request, progress).await?;
        self.require_authority(generation, "agent creation")?;
        require_execution_workspace_id(&created.agent_id, created.workspace_id.as_deref())?;
        Ok(OwnedAgentSnapshot {
            execution_id: owner.execution_id.clone(),
            agent: created.snapshot,
        })
    }

    /// The `catch` block of TS `createOrResolve`: close and delete the live agent, remove a
    /// worktree the create made, then drop the stored record. Every step runs.
    async fn cleanup_failed_create(&self, progress: &CreateProgress) -> Result<(), ExecError> {
        let (worktree, agent_id) = progress.snapshot();
        let agent_step = async {
            match &agent_id {
                Some(id) if self.backend.is_live(id) => {
                    let closed = self.backend.close_agent(id).await;
                    try_finally(closed, self.backend.delete_agent_state(id).await)
                }
                _ => Ok(()),
            }
        }
        .await;
        let cleanup = self
            .backend
            .cleanup_failed_create(worktree.filter(|w| w.created).map(|w| w.value))
            .await;
        let remove = match &agent_id {
            Some(id) => self.backend.remove_agent_record(id).await,
            None => Ok(()),
        };
        // try { agent_step } finally { try { cleanup } finally { remove } }
        let inner2 = try_finally(cleanup, remove);
        try_finally(agent_step, inner2)
    }

    async fn control_owned(
        &self,
        owner: AgentOwner,
        input: HubExecutionControlInput,
        generation: u64,
    ) -> ControlResult {
        const OP: &str = "execution control";
        self.require_authority(generation, OP)?;
        let record = self.backend.find_by_daemon_execution(&owner).await?;
        self.require_authority(generation, OP)?;
        let Some(record) = record else {
            return Ok(());
        };
        self.require_owner(&record)?;
        if input.action == ControlAction::Interrupt {
            let archived = record.archived_at.value().is_some_and(|a| !a.is_empty());
            if !archived && self.backend.is_live(&record.id) {
                self.backend.interrupt_agent(&record.id).await?;
            }
            return Ok(());
        }
        let workspace_id =
            require_execution_workspace_id(&record.id, record.workspace_id.as_deref())?;
        self.require_authority(generation, OP)?;
        self.backend
            .archive_workspace(&workspace_id, &input.request_id)
            .await
    }

    fn project_record(&self, record: &StoredAgentRecord) -> CreateResult {
        let owner = self.require_owner(record)?;
        Ok(OwnedAgentSnapshot {
            execution_id: owner.execution_id.clone(),
            agent: self.backend.project_record(record),
        })
    }

    fn require_owner<'a>(
        &self,
        record: &'a StoredAgentRecord,
    ) -> Result<&'a AgentOwner, ExecError> {
        match &record.owner {
            Some(owner) if owner.daemon_id == self.daemon_id => Ok(owner),
            _ => Err(ExecError::other(format!(
                "Agent {} is not owned by daemon {}",
                record.id, self.daemon_id
            ))),
        }
    }
}

/// JS `try { a } finally { b }`: an error from `b` replaces the outcome of `a`.
fn try_finally(a: Result<(), ExecError>, b: Result<(), ExecError>) -> Result<(), ExecError> {
    match b {
        Err(error) => Err(error),
        Ok(()) => a,
    }
}

fn require_hub_mcp_namespace(servers: Option<&Map<String, Value>>) -> Result<(), ExecError> {
    if servers.is_some_and(|s| s.contains_key("paseo")) {
        return Err(ExecError::other(
            "Hub execution MCP server name \"paseo\" is reserved by the daemon",
        ));
    }
    Ok(())
}

fn require_tool_policy_servers(
    tool_policy: Option<&Value>,
    servers: Option<&Map<String, Value>>,
) -> Result<(), ExecError> {
    let Some(policy) = tool_policy else {
        return Ok(());
    };
    let grants = policy
        .get("preapproved")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for grant in grants {
        let server = grant
            .get("server")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !servers.is_some_and(|s| s.contains_key(server)) {
            let tool = grant
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or_default();
            return Err(ExecError::other(format!(
                "Hub tool preapproval '{server}.{tool}' requires MCP server '{server}' in the same create request"
            )));
        }
    }
    Ok(())
}

/// `requireExecutionWorkspaceId`: a missing or empty workspace id is an error.
fn require_execution_workspace_id(
    agent_id: &str,
    workspace_id: Option<&str>,
) -> Result<String, ExecError> {
    match workspace_id {
        Some(id) if !id.is_empty() => Ok(id.to_owned()),
        _ => Err(ExecError::other(format!(
            "Hub execution agent {agent_id} has no workspaceId"
        ))),
    }
}
