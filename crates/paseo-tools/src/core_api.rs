//! The seam between the tools and the rest of the daemon (`PaseoToolHostDependencies` in TS).
//!
//! Tools never reach into the agent manager, registries or terminals directly. They call this
//! trait, which a host implements over `AgentManager`, the stores and, for the TS-resident
//! domains (terminals, schedules, voice, browser, git/checkout), over `internal.<domain>.<op>`
//! RPC (docs/p4-core-spec.md section 13).
//!
//! The trait is the union of what the TS tool handlers call, grouped by dependency. Wire shaped
//! data (agent snapshot payloads, schedules, providers) travels as `serde_json::Value` because the
//! tools only pass it through or read a few fields; the host owns its projection (`toAgentPayload`
//! and friends live with the session layer).
//!
//! Optional dependencies of the TS catalog (`terminalManager?`, `scheduleService?`, ...) are
//! [`Capability`] flags: a tool checks the flag at the point where the TS handler checks the
//! option and throws the same message, so the order of "is not configured" errors is preserved.

use async_trait::async_trait;
use paseo_agent_types::AgentTimelineItem;
use paseo_core::PaseoToolPolicy;
use paseo_store::WorkspaceRecord;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;

/// An optional TS dependency (`options.<name>`); a host reports which it provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// `terminalManager`
    Terminals,
    /// `scheduleService`
    Schedules,
    /// `workspaceRegistry`
    WorkspaceRegistry,
    /// `workspaceScripts`
    WorkspaceScripts,
    /// `createDirectoryWorkspace`
    DirectoryWorkspaces,
    /// `listActiveWorkspaces`
    ActiveWorkspaces,
    /// `projectRegistry`
    ProjectRegistry,
    /// `emitWorkspaceUpdatesForWorkspaceIds`
    WorkspaceUpdates,
    /// `ensureWorkspaceForCreate`
    WorkspaceMinting,
    /// `browserToolsEnabled && browserToolsBroker`
    BrowserBroker,
}

/// The slice of `ManagedAgent` the tools read.
#[derive(Debug, Clone, Default)]
pub struct AgentInfo {
    pub id: String,
    pub provider: String,
    pub cwd: String,
    pub workspace_id: Option<String>,
    /// `AgentLifecycle`: initializing, idle, running, error, closed.
    pub lifecycle: String,
    pub current_mode_id: Option<String>,
    pub available_modes: Vec<Value>,
    pub config: AgentConfigInfo,
}

/// `ManagedAgent.config` fields the schedule and create tools inherit.
#[derive(Debug, Clone, Default)]
pub struct AgentConfigInfo {
    pub model: Option<String>,
    pub thinking_option_id: Option<String>,
    pub provider_options: Option<Value>,
    pub feature_values: Option<Value>,
    pub system_prompt: Option<String>,
    pub mcp_servers: Option<Value>,
}

/// `VoiceCallerContext` (`voice-types.ts`).
#[derive(Debug, Clone, Default)]
pub struct CallerContext {
    pub child_agent_default_labels: Option<Map<String, Value>>,
    pub locked_cwd: Option<String>,
    pub allow_custom_cwd: Option<bool>,
    pub enable_voice_tools: bool,
}

/// `AgentPermissionRequest`s of one live agent, already projected to payloads.
#[derive(Debug, Clone)]
pub struct AgentPermissions {
    pub agent_id: String,
    /// The payload status (`toAgentPayload(agent).status`).
    pub status: String,
    pub requests: Vec<Value>,
}

/// Result of `get_agent_status` lookups: live snapshot or stored record, projected.
#[derive(Debug, Clone)]
pub struct AgentStatusSnapshot {
    pub status: String,
    pub snapshot: Value,
}

/// A provider as `ProviderSnapshotManager` lists it (`toProviderSummary` input).
#[derive(Debug, Clone, Default)]
pub struct ProviderEntry {
    pub provider: String,
    pub label: Option<String>,
    pub description: Option<String>,
    pub enabled: bool,
    pub modes: Option<Vec<Value>>,
    /// `ready`, `unavailable`, ...
    pub status: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ActiveWorkspaceRef {
    pub workspace_id: String,
    pub cwd: String,
}

#[derive(Debug, Clone, Default)]
pub struct DirectoryWorkspaceContext {
    pub background: Option<bool>,
    pub caller_workspace_id: Option<String>,
}

/// `createPaseoWorktreeCommand` input (the fields `create_workspace` sets).
#[derive(Debug, Clone, Default)]
pub struct CreateWorktreeRequest {
    pub cwd: String,
    pub project_id: Option<String>,
    pub worktree_slug: Option<String>,
    /// `branch-off` or `checkout`.
    pub action: String,
    pub branch_name: Option<String>,
    pub ref_name: Option<String>,
    /// `{kind: "change_request", forge?, number}`.
    pub checkout_source: Option<Value>,
    pub title: Option<String>,
    pub background: Option<bool>,
    pub caller_workspace_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ArchiveWorkspaceResult {
    pub archived_agent_ids: Vec<String>,
    pub removed_directory: bool,
}

#[derive(Debug, Clone)]
pub struct TerminalSummary {
    pub id: String,
    pub name: String,
    pub cwd: String,
}

#[derive(Debug, Clone, Default)]
pub struct CaptureOptions {
    pub start: Option<f64>,
    pub end: Option<f64>,
    pub strip_ansi: bool,
}

#[derive(Debug, Clone)]
pub struct TerminalCapture {
    pub lines: Vec<String>,
    pub total_lines: u64,
}

/// `CreateAgentFromMcpInput` (`create-agent/create.ts`), the fields `create_agent` sets.
#[derive(Debug, Clone, Default)]
pub struct CreateAgentRequest {
    pub provider: String,
    pub title: String,
    pub initial_prompt: String,
    /// `config`: only `providerOptions`, inherited from a same-provider caller.
    pub provider_options: Option<Value>,
    pub cwd: Option<String>,
    pub workspace_id: Option<String>,
    pub thinking: Option<String>,
    pub features: Option<Map<String, Value>>,
    pub labels: Option<Map<String, Value>>,
    pub mode: Option<String>,
    pub notify_on_finish: bool,
    pub detached: bool,
    pub caller_agent_id: Option<String>,
    pub caller_context: Option<CallerContext>,
    pub worktree: Option<WorktreeSpec>,
}

#[derive(Debug, Clone, Default)]
pub struct WorktreeSpec {
    pub action: Option<String>,
    pub worktree_name: Option<String>,
    pub branch_name: Option<String>,
    pub base_branch: Option<String>,
    pub ref_name: Option<String>,
    pub github_pr_number: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct CreateAgentResult {
    pub snapshot: AgentInfo,
    pub initial_prompt_started: bool,
}

/// `PromptDispatchDisposition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDisposition {
    OutOfBand,
    Steered,
    TurnStarted,
}

impl PromptDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OutOfBand => "out_of_band",
            Self::Steered => "steered",
            Self::TurnStarted => "turn_started",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SendPromptRequest {
    pub agent_id: String,
    pub prompt: String,
    /// `{kind: "agent-message", agentId}` when an agent is the sender.
    pub source_agent_id: Option<String>,
    pub session_mode: Option<String>,
}

/// Called by the host once the dispatch decision is known and before the call returns
/// (`onDispatch`): the tool arms the finish notification here.
pub type OnDispatch<'a> = Box<dyn FnOnce(PromptDisposition) + Send + 'a>;

/// Cancels a finish notification armed by [`CoreApi::setup_finish_notification`].
pub type StopNotification = Box<dyn FnOnce() + Send>;

#[derive(Debug, Clone)]
pub struct FinishNotificationRequest {
    pub child_agent_id: String,
    pub caller_agent_id: String,
    pub wait_for_turn_start: bool,
}

fn not_configured(what: &str) -> ToolError {
    ToolError::new(format!("{what} is not configured"))
}

/// What the tools need from the daemon. Methods with a default body belong to an optional
/// capability and return a "not configured" error until a host overrides them.
#[async_trait]
pub trait CoreApi: Send + Sync + 'static {
    /// Optional dependencies this host provides. The default provides none.
    fn has(&self, _capability: Capability) -> bool {
        false
    }

    /// `Date.now()`.
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64)
    }

    /// `process.cwd()`.
    fn process_cwd(&self) -> String {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "/".to_owned())
    }

    // ---- policy and caller context ----------------------------------------------------

    /// `agentManager.getPaseoToolPolicy(callerAgentId)`.
    fn paseo_tool_policy(&self, _caller_agent_id: &str) -> Option<PaseoToolPolicy> {
        None
    }

    /// `resolveCallerContext` (`wsServer.resolveVoiceCallerContext`).
    fn caller_context(&self, _caller_agent_id: &str) -> Option<CallerContext> {
        None
    }

    // ---- agents -----------------------------------------------------------------------

    /// `agentManager.getAgent`.
    fn get_agent(&self, agent_id: &str) -> Option<AgentInfo>;

    /// `agentManager.listAgents()` projected for `list_pending_permissions`.
    fn list_agent_permissions(&self) -> Vec<AgentPermissions>;

    /// Live snapshot (`serializeSnapshotWithMetadata`) else a stored, non-internal record
    /// (`buildStoredAgentPayload`); `None` when the agent is unknown or internal.
    async fn agent_status_snapshot(
        &self,
        agent_id: &str,
    ) -> Result<Option<AgentStatusSnapshot>, ToolError>;

    /// `AgentDirectory.list`: public agent snapshot payloads, live then stored.
    async fn list_agent_snapshots(
        &self,
        include_background: bool,
        include_archived: bool,
    ) -> Result<Vec<Value>, ToolError>;

    /// `ensureAgentLoaded`.
    async fn ensure_agent_loaded(&self, agent_id: &str) -> Result<(), ToolError>;

    /// `agentManager.getTimeline`.
    fn agent_timeline(&self, agent_id: &str) -> Vec<AgentTimelineItem>;

    /// `createAgentCommand` for an MCP caller.
    async fn create_agent(
        &self,
        request: CreateAgentRequest,
    ) -> Result<CreateAgentResult, ToolError>;

    /// `sendPromptToAgent`.
    async fn send_prompt_to_agent(
        &self,
        request: SendPromptRequest,
        on_dispatch: OnDispatch<'_>,
    ) -> Result<PromptDisposition, ToolError>;

    /// `waitForAgentRunStartWithTimeout`.
    async fn wait_for_agent_run_start(&self, agent_id: &str) -> Result<(), ToolError>;

    /// `setupFinishNotification`.
    fn setup_finish_notification(&self, request: FinishNotificationRequest) -> StopNotification;

    /// `cancelAgentRunCommand`: whether a run was cancelled.
    async fn cancel_agent_run(&self, agent_id: &str) -> Result<bool, ToolError>;

    /// `archiveAgentCommand`.
    async fn archive_agent(&self, agent_id: &str) -> Result<(), ToolError>;

    /// `closeAgentCommand`.
    async fn close_agent(&self, agent_id: &str) -> Result<(), ToolError>;

    /// `agentManager.updateAgentMetadata`.
    async fn update_agent_metadata(
        &self,
        agent_id: &str,
        title: Option<String>,
        labels: Option<Map<String, Value>>,
    ) -> Result<(), ToolError>;

    /// `agentManager.setAgentMode`.
    async fn set_agent_mode(&self, agent_id: &str, mode_id: &str) -> Result<(), ToolError>;

    /// `agentManager.setAgentModel` (`None` clears).
    async fn set_agent_model(&self, agent_id: &str, model: Option<&str>) -> Result<(), ToolError>;

    /// `agentManager.setAgentThinkingOption` (`None` clears).
    async fn set_agent_thinking_option(
        &self,
        agent_id: &str,
        option: Option<&str>,
    ) -> Result<(), ToolError>;

    /// `agentManager.setAgentFeature`.
    async fn set_agent_feature(
        &self,
        agent_id: &str,
        feature_id: &str,
        value: &Value,
    ) -> Result<(), ToolError>;

    /// `respondToAgentPermission` (forwards the response, starts a follow-up run if any).
    async fn respond_to_permission(
        &self,
        agent_id: &str,
        request_id: &str,
        response: &Value,
    ) -> Result<(), ToolError>;

    // ---- providers --------------------------------------------------------------------

    /// `providerSnapshotManager.listProviders({wait: true})`.
    async fn list_providers(&self) -> Result<Vec<ProviderEntry>, ToolError>;

    /// `providerSnapshotManager.getProvider({cwd, provider, wait: true})`.
    async fn get_provider(&self, cwd: &str, provider: &str) -> Result<ProviderEntry, ToolError>;

    /// `providerSnapshotManager.listModels({provider, wait: true})`.
    async fn list_models(&self, provider: &str) -> Result<Vec<Value>, ToolError>;

    /// `agentManager.listDraftFeatures`.
    async fn list_draft_features(&self, request: Value) -> Result<Vec<Value>, ToolError>;

    /// `daemonConfigStore.get().agentProfiles ?? []`.
    fn agent_profiles(&self) -> Vec<Value> {
        Vec::new()
    }

    // ---- workspaces -------------------------------------------------------------------

    /// `workspaceRegistry.list` (needs [`Capability::WorkspaceRegistry`]).
    async fn list_workspaces(&self) -> Result<Vec<WorkspaceRecord>, ToolError> {
        Err(not_configured("Workspace registry"))
    }

    /// `workspaceRegistry.get`.
    async fn get_workspace(
        &self,
        _workspace_id: &str,
    ) -> Result<Option<WorkspaceRecord>, ToolError> {
        Err(not_configured("Workspace registry"))
    }

    /// `workspaceRegistry.upsert`.
    async fn upsert_workspace(&self, _record: WorkspaceRecord) -> Result<(), ToolError> {
        Err(not_configured("Workspace registry"))
    }

    /// `emitWorkspaceUpdatesForWorkspaceIds`.
    async fn emit_workspace_updates(&self, _workspace_ids: &[String]) -> Result<(), ToolError> {
        Err(not_configured("Workspace update emitter"))
    }

    /// `listActiveWorkspaces`.
    async fn list_active_workspaces(&self) -> Result<Vec<ActiveWorkspaceRef>, ToolError> {
        Err(not_configured("Workspace lookup"))
    }

    /// `createDirectoryWorkspace`.
    async fn create_directory_workspace(
        &self,
        _cwd: &str,
        _title: Option<&str>,
        _project_id: Option<&str>,
        _context: DirectoryWorkspaceContext,
    ) -> Result<WorkspaceRecord, ToolError> {
        Err(not_configured("Workspace provisioning"))
    }

    /// `resolveWorktreeSourceCwd({projectId}, projectRegistry)` (needs
    /// [`Capability::ProjectRegistry`]).
    async fn resolve_worktree_source_cwd(&self, _project_id: &str) -> Result<String, ToolError> {
        Err(not_configured("Project registry"))
    }

    /// `createPaseoWorktreeCommand`: the created worktree's workspace record.
    async fn create_paseo_worktree(
        &self,
        _request: CreateWorktreeRequest,
    ) -> Result<WorkspaceRecord, ToolError> {
        Err(not_configured("Worktree creation"))
    }

    /// `archiveByScope({kind: "workspace"})` including its dependency checks.
    async fn archive_workspace(
        &self,
        _workspace_id: &str,
    ) -> Result<ArchiveWorkspaceResult, ToolError> {
        Err(not_configured("Workspace archiving"))
    }

    /// `ensureWorkspaceForCreate`: mints a directory workspace for a cwd.
    async fn ensure_workspace_for_create(
        &self,
        _cwd: &str,
        _first_agent_context: Option<Value>,
        _caller_workspace_id: Option<&str>,
    ) -> Result<String, ToolError> {
        Err(not_configured("Workspace creation"))
    }

    // ---- workspace scripts ------------------------------------------------------------

    async fn workspace_scripts_list(&self, _workspace_id: &str) -> Result<Vec<Value>, ToolError> {
        Err(not_configured("Workspace script management"))
    }

    async fn workspace_script_launch(
        &self,
        _workspace_id: &str,
        _script_name: &str,
    ) -> Result<Value, ToolError> {
        Err(not_configured("Workspace script management"))
    }

    async fn workspace_script_stop(
        &self,
        _workspace_id: &str,
        _script_name: &str,
    ) -> Result<Value, ToolError> {
        Err(not_configured("Workspace script management"))
    }

    // ---- terminals --------------------------------------------------------------------

    fn terminal_directories(&self) -> Vec<String> {
        Vec::new()
    }

    async fn get_terminals(&self, _cwd: &str) -> Result<Vec<TerminalSummary>, ToolError> {
        Err(not_configured("Terminal manager"))
    }

    fn get_terminal(&self, _terminal_id: &str) -> Option<TerminalSummary> {
        None
    }

    async fn create_terminal(
        &self,
        _cwd: &str,
        _workspace_id: &str,
        _name: Option<&str>,
    ) -> Result<TerminalSummary, ToolError> {
        Err(not_configured("Terminal manager"))
    }

    async fn capture_terminal(
        &self,
        _terminal_id: &str,
        _options: CaptureOptions,
    ) -> Result<TerminalCapture, ToolError> {
        Err(not_configured("Terminal manager"))
    }

    fn kill_terminal(&self, _terminal_id: &str) {}

    /// `terminal.send({type: "input", data})`.
    fn send_terminal_input(&self, _terminal_id: &str, _data: &str) {}

    // ---- schedules (stored schedules travel as JSON) ------------------------------------

    async fn schedule_list(&self) -> Result<Vec<Value>, ToolError> {
        Err(not_configured("Schedule service"))
    }

    /// `scheduleService.inspect`: throws when the schedule does not exist.
    async fn schedule_inspect(&self, _id: &str) -> Result<Value, ToolError> {
        Err(not_configured("Schedule service"))
    }

    async fn schedule_create_or_replace(&self, _input: Value) -> Result<Value, ToolError> {
        Err(not_configured("Schedule service"))
    }

    async fn schedule_pause(&self, _id: &str) -> Result<(), ToolError> {
        Err(not_configured("Schedule service"))
    }

    async fn schedule_resume(&self, _id: &str) -> Result<(), ToolError> {
        Err(not_configured("Schedule service"))
    }

    async fn schedule_delete(&self, _id: &str) -> Result<(), ToolError> {
        Err(not_configured("Schedule service"))
    }

    async fn schedule_run_once(&self, _id: &str) -> Result<Value, ToolError> {
        Err(not_configured("Schedule service"))
    }

    async fn schedule_update(&self, _input: Value) -> Result<Value, ToolError> {
        Err(not_configured("Schedule service"))
    }

    async fn schedule_logs(&self, _id: &str) -> Result<Vec<Value>, ToolError> {
        Err(not_configured("Schedule service"))
    }

    // ---- voice ------------------------------------------------------------------------

    /// `resolveSpeakHandler(callerAgentId) !== null`.
    fn has_speak_handler(&self, _caller_agent_id: &str) -> bool {
        false
    }

    /// Runs the speak handler; resolves when playback completes. Cancelling `cancel` aborts it.
    async fn speak(
        &self,
        _caller_agent_id: &str,
        _text: &str,
        _cancel: CancellationToken,
    ) -> Result<(), ToolError> {
        Err(ToolError::new("No speak handler registered"))
    }

    // ---- browser ----------------------------------------------------------------------

    /// `browserToolsBroker.execute`: the `BrowserAutomationExecuteResponse["payload"]`.
    async fn browser_execute(&self, _request: Value) -> Result<Value, ToolError> {
        Err(not_configured("Browser tools broker"))
    }
}
