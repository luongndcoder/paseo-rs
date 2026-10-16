//! Provider-agnostic surface, the Rust shape of `AgentClient` / `AgentSession` from
//! `agent-sdk-types.ts`. The crate implements it for Pi and OMP on top of [`crate::Session`].
//!
//! Provider events are exposed raw ([`AgentEvent::Provider`]); the Paseo timeline / tool-call
//! mapping is a later phase and deliberately absent.

mod catalog;
mod client;
mod permission;
mod session;
mod turn;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use serde_json::{Map, Value};

use crate::error::Result;

pub use catalog::{map_model, thinking_config};
pub use client::{Launcher, ProcessLauncher, RpcAgentClient};
pub use permission::{build_ui_response, ui_request_to_permission};
pub use session::RpcAgentSession;
pub use turn::{TurnOutcome, TurnTracker};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    pub supports_streaming: bool,
    pub supports_session_persistence: bool,
    pub supports_session_listing: bool,
    pub supports_dynamic_modes: bool,
    pub supports_mcp_servers: bool,
    pub supports_reasoning_stream: bool,
    pub supports_tool_invocations: bool,
    pub supports_rewind_conversation: bool,
}

/// Session config; the subset of `AgentSessionConfig` a native provider acts on.
#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    pub cwd: PathBuf,
    /// `provider/id`.
    pub model: Option<String>,
    pub thinking_option_id: Option<String>,
    pub mode_id: Option<String>,
    pub system_prompt: Option<String>,
    /// Internal sessions leave no durable native session behind (`--no-session`).
    pub internal: bool,
    /// Pi: extension files to load (`--extension`).
    pub extension_paths: Vec<PathBuf>,
    /// Pi: `--mcp-config` file.
    pub mcp_config_path: Option<PathBuf>,
    pub extra_args: Vec<String>,
}

/// Process-level context supplied by the daemon at launch (`AgentLaunchContext`).
#[derive(Debug, Clone, Default)]
pub struct LaunchContext {
    pub agent_id: Option<String>,
    pub env: BTreeMap<String, String>,
}

/// Per-resume overrides (`Partial<AgentSessionConfig>`).
#[derive(Debug, Clone, Default)]
pub struct ResumeOverrides {
    pub cwd: Option<PathBuf>,
    pub model: Option<String>,
    pub thinking_option_id: Option<String>,
    pub mode_id: Option<String>,
    pub system_prompt: Option<String>,
    /// Pi: extension files to load (`--extension`).
    pub extension_paths: Vec<PathBuf>,
    /// Pi: `--mcp-config` file.
    pub mcp_config_path: Option<PathBuf>,
    pub extra_args: Vec<String>,
}

/// `AgentPersistenceHandle`. For Pi/OMP `native_handle` is the session file path.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PersistenceHandle {
    pub provider: String,
    pub session_id: String,
    pub native_handle: Option<String>,
    pub metadata: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PromptBlock {
    Text(String),
    Image { data: String, mime_type: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PromptInput {
    Text(String),
    Blocks(Vec<PromptBlock>),
}

impl From<&str> for PromptInput {
    fn from(text: &str) -> Self {
        PromptInput::Text(text.to_string())
    }
}

impl From<String> for PromptInput {
    fn from(text: String) -> Self {
        PromptInput::Text(text)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnHandle {
    pub turn_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelDefinition {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub is_default: bool,
    pub thinking_options: Vec<SelectOption>,
    pub default_thinking_option_id: Option<String>,
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mode {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Catalog {
    pub models: Vec<ModelDefinition>,
    pub modes: Vec<Mode>,
    pub default_mode_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlashCommandKind {
    Command,
    Skill,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSlashCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
    pub kind: SlashCommandKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInfo {
    pub provider: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub thinking_option_id: Option<String>,
    pub mode_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionKind {
    /// A dialog the agent asks the user to answer (select / input / editor / confirm).
    Question,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub id: String,
    pub provider: String,
    pub name: String,
    pub kind: PermissionKind,
    pub title: String,
    /// `{ "questions": [...] }`
    pub input: Value,
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionResponse {
    Allow {
        /// For questions: `{ "answers": { "<header>": "<answer>" } }`.
        updated_input: Option<Value>,
    },
    Deny {
        message: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    TurnStarted {
        turn_id: String,
    },
    /// Every raw provider frame (including `extension_ui_request`), unmodified.
    Provider {
        turn_id: Option<String>,
        event_type: String,
        raw: Value,
    },
    PermissionRequested {
        turn_id: Option<String>,
        request: PermissionRequest,
    },
    PermissionResolved {
        turn_id: Option<String>,
        request_id: String,
        response: PermissionResponse,
    },
    TurnCompleted {
        turn_id: String,
        final_text: String,
    },
    TurnFailed {
        turn_id: String,
        error: String,
    },
    TurnCanceled {
        turn_id: String,
        reason: String,
    },
    /// The provider process went away.
    ProcessExited {
        error: String,
    },
}

/// Result of [`AgentSession::run`].
#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub turn_id: String,
    pub final_text: String,
    pub events: Vec<AgentEvent>,
}

pub trait AgentClient: Send + Sync {
    fn provider(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    fn create_session(
        &self,
        config: &SessionConfig,
        context: &LaunchContext,
    ) -> Result<Box<dyn AgentSession>>;
    fn resume_session(
        &self,
        handle: &PersistenceHandle,
        overrides: &ResumeOverrides,
        context: &LaunchContext,
    ) -> Result<Box<dyn AgentSession>>;
    /// Models and modes in one provider process.
    fn fetch_catalog(&self, cwd: Option<&std::path::Path>) -> Result<Catalog>;
    fn list_commands(&self, config: &SessionConfig) -> Result<Vec<AgentSlashCommand>>;
    /// Whether the provider binary can be launched (resolves on `PATH` or is an existing path).
    fn is_available(&self) -> bool;
}

pub trait AgentSession: Send + Sync {
    fn provider(&self) -> &str;
    fn capabilities(&self) -> Capabilities;
    /// Native session id once known.
    fn id(&self) -> Option<String>;
    /// New event stream; events before this call are not replayed.
    fn subscribe(&self) -> Receiver<AgentEvent>;
    /// Send a prompt (text and images). Returns once the provider acknowledged it.
    fn start_turn(&self, prompt: PromptInput) -> Result<TurnHandle>;
    /// `start_turn` then block until the turn reaches a terminal state.
    fn run(&self, prompt: PromptInput) -> Result<RunResult> {
        let events = self.subscribe();
        let handle = self.start_turn(prompt)?;
        let mut collected = Vec::new();
        loop {
            let Ok(event) = events.recv() else {
                return Err(crate::Error::Closed(
                    "event stream ended before the turn finished".into(),
                ));
            };
            let terminal = match &event {
                AgentEvent::TurnCompleted {
                    turn_id,
                    final_text,
                } if *turn_id == handle.turn_id => Some(Ok(final_text.clone())),
                AgentEvent::TurnFailed { turn_id, error } if *turn_id == handle.turn_id => {
                    Some(Err(crate::Error::Closed(error.clone())))
                }
                AgentEvent::TurnCanceled { turn_id, reason } if *turn_id == handle.turn_id => Some(
                    Err(crate::Error::Closed(format!("turn canceled: {reason}"))),
                ),
                AgentEvent::ProcessExited { error } => {
                    Some(Err(crate::Error::Closed(error.clone())))
                }
                _ => None,
            };
            collected.push(event);
            if let Some(outcome) = terminal {
                return outcome.map(|final_text| RunResult {
                    turn_id: handle.turn_id.clone(),
                    final_text,
                    events: collected,
                });
            }
        }
    }
    fn interrupt(&self) -> Result<()>;
    fn pending_permissions(&self) -> Vec<PermissionRequest>;
    fn respond_to_permission(&self, request_id: &str, response: PermissionResponse) -> Result<()>;
    fn list_commands(&self) -> Result<Vec<AgentSlashCommand>>;
    fn available_modes(&self) -> Result<Vec<Mode>>;
    fn current_mode(&self) -> Option<String>;
    fn set_mode(&self, mode_id: &str) -> Result<()>;
    /// `provider/id`; `None` is a no-op.
    fn set_model(&self, model_id: Option<&str>) -> Result<()>;
    fn set_thinking_option(&self, thinking_option_id: Option<&str>) -> Result<()>;
    fn runtime_info(&self) -> Result<RuntimeInfo>;
    fn describe_persistence(&self) -> Option<PersistenceHandle>;
    /// Release the process; the durable native session stays on disk.
    fn close(&self) -> Result<()>;
}
