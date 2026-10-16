//! Provider-agnostic agent surface, mirroring `AgentClient` / `AgentSession` from Paseo's
//! `agent-sdk-types.ts`. Another provider crate implements the same traits, so the daemon core
//! never names Codex.
//!
//! Events are *raw*: normalized turn lifecycle plus every provider notification verbatim. Mapping
//! them to Paseo's timeline / `ToolCallDetail` is a later phase (P4) and lives above this crate.

use std::collections::BTreeMap;
use std::future::Future;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::broadcast;

use crate::error::Result;
use crate::protocol::{Notification, ServerNotification};

// ---- prompt ----

#[derive(Debug, Clone, PartialEq)]
pub enum PromptBlock {
    Text(String),
    /// Base64-encoded image bytes.
    Image {
        data: String,
        mime_type: String,
    },
    Skill {
        name: String,
        path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PromptInput(pub Vec<PromptBlock>);

impl From<&str> for PromptInput {
    fn from(s: &str) -> Self {
        PromptInput(vec![PromptBlock::Text(s.to_string())])
    }
}

impl From<String> for PromptInput {
    fn from(s: String) -> Self {
        PromptInput(vec![PromptBlock::Text(s)])
    }
}

impl From<Vec<PromptBlock>> for PromptInput {
    fn from(v: Vec<PromptBlock>) -> Self {
        PromptInput(v)
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub output_schema: Option<Value>,
    pub client_message_id: Option<String>,
}

// ---- configuration ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerConfig {
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<BTreeMap<String, String>>,
    },
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers: Option<BTreeMap<String, String>>,
    },
    Sse {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers: Option<BTreeMap<String, String>>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolGrant {
    pub server: String,
    pub tool: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolPolicy {
    pub preapproved: Vec<ToolGrant>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    pub cwd: String,
    pub system_prompt: Option<String>,
    /// Daemon-level instructions appended at runtime; never persisted.
    pub daemon_append_system_prompt: Option<String>,
    pub mode_id: Option<String>,
    pub model: Option<String>,
    pub thinking_option_id: Option<String>,
    /// Feature toggles, e.g. `service_tier`.
    pub feature_values: Map<String, Value>,
    pub title: Option<String>,
    /// Provider-native options (for Codex: `approval_policy`, `sandbox_mode`, ...).
    pub provider_options: Option<Map<String, Value>>,
    pub tool_policy: Option<ToolPolicy>,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistenceHandle {
    pub provider: String,
    pub session_id: String,
    #[serde(default)]
    pub native_handle: Option<String>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

// ---- catalog ----

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AgentMode {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SelectOption {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelDefinition {
    pub provider: String,
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub is_default: bool,
    pub thinking_options: Vec<SelectOption>,
    pub default_thinking_option_id: Option<String>,
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderCatalog {
    pub models: Vec<ModelDefinition>,
    pub modes: Vec<AgentMode>,
    pub default_mode_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommandKind {
    Command,
    Skill,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
    pub kind: SlashCommandKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportableSession {
    pub session_id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub preview: Option<String>,
    /// Unix seconds.
    pub updated_at: Option<i64>,
}

// ---- runtime ----

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RuntimeInfo {
    pub provider: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub thinking_option_id: Option<String>,
    pub mode_id: Option<String>,
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub context_window_max_tokens: Option<i64>,
    pub context_window_used_tokens: Option<i64>,
}

/// Non-fatal notice returned by a setter that only takes effect on the next turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderNotice {
    pub message: String,
}

// ---- permissions ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionKind {
    Tool,
    Question,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub id: String,
    pub provider: String,
    /// `CodexBash`, `CodexFileChange`, `request_user_input`, `CodexMcpElicitation`.
    pub name: String,
    pub kind: PermissionKind,
    pub title: Option<String>,
    pub description: Option<String>,
    pub input: Value,
    pub metadata: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionResponse {
    Allow {
        /// `acceptForSession` widens a command / file-change approval to the session.
        selected_action_id: Option<String>,
        /// For questions: `{ "answers": { "<question id>": ["label", ...] } }`.
        updated_input: Option<Value>,
    },
    Deny {
        message: Option<String>,
        /// Also interrupt the turn.
        interrupt: bool,
    },
}

impl PermissionResponse {
    pub fn allow() -> Self {
        PermissionResponse::Allow {
            selected_action_id: None,
            updated_input: None,
        }
    }

    pub fn deny() -> Self {
        PermissionResponse::Deny {
            message: None,
            interrupt: false,
        }
    }
}

// ---- events ----

/// A provider notification, verbatim plus its typed view.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderEvent {
    pub notification: Notification,
    pub typed: ServerNotification,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    ThreadStarted {
        session_id: String,
    },
    TurnStarted {
        turn_id: Option<String>,
    },
    TurnCompleted {
        turn_id: Option<String>,
        usage: Option<Usage>,
    },
    TurnFailed {
        turn_id: Option<String>,
        error: String,
    },
    TurnCanceled {
        turn_id: Option<String>,
        reason: String,
    },
    UsageUpdated {
        turn_id: Option<String>,
        usage: Usage,
    },
    PermissionRequested {
        turn_id: Option<String>,
        request: PermissionRequest,
    },
    /// `resolution` is `None` when the provider dismissed the request itself.
    PermissionResolved {
        request_id: String,
        resolution: Option<PermissionResponse>,
    },
    ModeChanged {
        current_mode_id: Option<String>,
    },
    ModelChanged {
        model: Option<String>,
    },
    /// Every notification the app-server sends, in order, including the ones also surfaced above.
    Provider {
        turn_id: Option<String>,
        event: Box<ProviderEvent>,
    },
    /// The app-server process went away (a session reconnects lazily on next use unless closed).
    Closed {
        reason: String,
    },
}

impl AgentEvent {
    pub fn is_turn_terminal(&self) -> bool {
        matches!(
            self,
            AgentEvent::TurnCompleted { .. }
                | AgentEvent::TurnFailed { .. }
                | AgentEvent::TurnCanceled { .. }
                | AgentEvent::Closed { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnHandle {
    /// Synthetic `codex-turn-N` id, stable before Codex assigns its own turn id.
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub session_id: String,
    /// Text of the last completed assistant message.
    pub final_text: String,
    pub usage: Option<Usage>,
    pub canceled: bool,
    /// Raw events of the turn, in order.
    pub events: Vec<AgentEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerResult {
    Accepted,
    /// The input was not submitted; start a new turn instead.
    Unavailable,
}

// ---- traits ----

pub trait AgentClient: Send + Sync {
    type Session: AgentSession;

    fn provider(&self) -> &'static str;

    fn create_session(
        &self,
        config: SessionConfig,
    ) -> impl Future<Output = Result<Self::Session>> + Send;

    /// `config: None` rebuilds the config from the handle's metadata.
    fn resume_session(
        &self,
        handle: &PersistenceHandle,
        config: Option<SessionConfig>,
    ) -> impl Future<Output = Result<Self::Session>> + Send;

    /// Models and modes in one call; one upstream process is enough.
    fn fetch_catalog(&self) -> impl Future<Output = Result<ProviderCatalog>> + Send;

    fn list_commands(
        &self,
        config: &SessionConfig,
    ) -> impl Future<Output = Result<Vec<SlashCommand>>> + Send;

    fn list_importable_sessions(
        &self,
        cwd: Option<&str>,
        limit: u32,
    ) -> impl Future<Output = Result<Vec<ImportableSession>>> + Send;
}

pub trait AgentSession: Send + Sync {
    /// Provider session id (the Codex thread id) once the thread exists.
    fn id(&self) -> Option<String>;

    fn subscribe(&self) -> broadcast::Receiver<AgentEvent>;

    /// Start a foreground turn and return without waiting for it to finish.
    fn start_turn(
        &self,
        prompt: PromptInput,
        options: RunOptions,
    ) -> impl Future<Output = Result<TurnHandle>> + Send;

    /// Start a turn and wait for it to finish.
    fn run(
        &self,
        prompt: PromptInput,
        options: RunOptions,
    ) -> impl Future<Output = Result<RunResult>> + Send;

    /// Add input to the running turn.
    fn steer_active_turn(
        &self,
        prompt: PromptInput,
        expected_turn_id: &str,
        client_message_id: Option<String>,
    ) -> impl Future<Output = Result<SteerResult>> + Send;

    /// Stop the foreground turn, if any.
    fn interrupt(&self) -> impl Future<Output = Result<()>> + Send;

    fn pending_permissions(&self) -> Vec<PermissionRequest>;

    fn respond_to_permission(
        &self,
        request_id: &str,
        response: PermissionResponse,
    ) -> impl Future<Output = Result<()>> + Send;

    fn available_modes(&self) -> impl Future<Output = Result<Vec<AgentMode>>> + Send;
    fn current_mode(&self) -> impl Future<Output = Result<Option<String>>> + Send;
    fn set_mode(
        &self,
        mode_id: &str,
    ) -> impl Future<Output = Result<Option<ProviderNotice>>> + Send;
    fn set_model(&self, model_id: Option<String>) -> impl Future<Output = Result<()>> + Send;
    fn set_thinking_option(
        &self,
        thinking_option_id: Option<String>,
    ) -> impl Future<Output = Result<Option<ProviderNotice>>> + Send;
    fn runtime_info(&self) -> impl Future<Output = Result<RuntimeInfo>> + Send;
    fn list_commands(&self) -> impl Future<Output = Result<Vec<SlashCommand>>> + Send;

    fn describe_persistence(&self) -> Option<PersistenceHandle>;

    /// Release the app-server process without archiving the durable Codex thread.
    fn close(&self) -> impl Future<Output = Result<()>> + Send;
}
