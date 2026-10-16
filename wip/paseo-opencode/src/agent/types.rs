//! Provider-agnostic agent types: the Rust equivalent of `agent-sdk-types.ts`.
//!
//! Timeline / `ToolCallDetail` mapping is intentionally absent (that is P4): provider events are
//! forwarded raw in `AgentEvent::Provider`.

use crate::protocol::{GlobalEvent, JsonMap, PermissionRule};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// One part of a prompt (`AgentPromptInput`).
#[derive(Debug, Clone, PartialEq)]
pub enum PromptPart {
    Text(String),
    /// Base64 data (a full `data:` URL is also accepted).
    Image {
        mime_type: String,
        data: String,
    },
}

pub type AgentPromptInput = Vec<PromptPart>;

pub fn text_prompt(text: impl Into<String>) -> AgentPromptInput {
    vec![PromptPart::Text(text.into())]
}

#[derive(Debug, Clone, Default)]
pub struct AgentRunOptions {
    /// Opaque id echoed back by the caller's own bookkeeping; the provider does not interpret it.
    pub client_message_id: Option<String>,
    /// JSON schema for structured output (`format: {type: "json_schema"}`).
    pub output_schema: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Option<HashMap<String, String>>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: Option<HashMap<String, String>>,
    },
}

/// Tool grant that skips the permission prompt (`ToolPolicy.preapproved`).
#[derive(Debug, Clone, PartialEq)]
pub struct PreapprovedTool {
    pub server: String,
    pub tool: String,
}

#[derive(Debug, Clone, Default)]
pub struct AgentSessionConfig {
    pub cwd: String,
    pub system_prompt: Option<String>,
    /// Daemon-level text appended after `system_prompt`.
    pub daemon_append_system_prompt: Option<String>,
    pub mode_id: Option<String>,
    /// `provider/model`.
    pub model: Option<String>,
    pub thinking_option_id: Option<String>,
    pub title: Option<String>,
    /// OpenCode `permission` rules applied to the session (`providerOptions.permission` in TS,
    /// already flattened to rules).
    pub permission_rules: Vec<PermissionRule>,
    pub preapproved_tools: Vec<PreapprovedTool>,
    pub mcp_servers: HashMap<String, McpServerConfig>,
    pub internal: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AgentLaunchContext {
    pub agent_id: Option<String>,
    /// Environment for a dedicated server (forces `Acquire::Dedicated`).
    pub env: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Default)]
pub struct AgentCreateSessionOptions {
    /// Keep the native session after `close()`. Default `true`; `Some(false)` deletes it.
    pub persist_session: Option<bool>,
}

/// Handle to a durable native session (`AgentPersistenceHandle`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentPersistenceHandle {
    pub provider: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(
        rename = "nativeHandle",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub native_handle: Option<String>,
    #[serde(default)]
    pub metadata: JsonMap,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentCapabilities {
    pub supports_streaming: bool,
    pub supports_session_persistence: bool,
    pub supports_session_listing: bool,
    pub supports_dynamic_modes: bool,
    pub supports_mcp_servers: bool,
    pub supports_reasoning_stream: bool,
    pub supports_tool_invocations: bool,
    pub supports_rewind_conversation: bool,
    pub supports_rewind_files: bool,
    pub supports_rewind_both: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentMode {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub color: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThinkingOption {
    pub id: String,
    pub label: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentModelDefinition {
    /// `provider/model`.
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    pub thinking_options: Vec<ThinkingOption>,
    pub default_thinking_option_id: Option<String>,
    pub context_window_max_tokens: Option<u64>,
    /// The provider's own model record.
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderCatalog {
    pub models: Vec<AgentModelDefinition>,
    pub modes: Vec<AgentMode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentSlashCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
    /// `command` or `skill`.
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FetchCatalogScope {
    /// Server home directory.
    Global,
    Workspace {
        cwd: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct FetchCatalogOptions {
    pub scope: FetchCatalogScope,
    /// Start a fresh server instead of sharing the current one.
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionKind {
    Tool,
    Question,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionBehavior {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentPermissionAction {
    pub id: String,
    pub label: String,
    pub behavior: PermissionBehavior,
}

/// A pending permission or question. `raw` is the provider's request unchanged.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentPermissionRequest {
    pub id: String,
    pub provider: String,
    /// OpenCode permission name (`bash`, `edit`, ...) or `question`.
    pub name: String,
    pub kind: PermissionKind,
    pub input: Value,
    pub actions: Vec<AgentPermissionAction>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentPermissionResponse {
    pub behavior: PermissionBehavior,
    pub selected_action_id: Option<String>,
    pub message: Option<String>,
    /// For questions: `{"answers": {"<header>": "a, b"}}`.
    pub updated_input: Option<Value>,
}

impl AgentPermissionResponse {
    pub fn allow() -> Self {
        Self {
            behavior: PermissionBehavior::Allow,
            selected_action_id: None,
            message: None,
            updated_input: None,
        }
    }

    pub fn allow_always() -> Self {
        Self {
            selected_action_id: Some("allow_always".into()),
            ..Self::allow()
        }
    }

    pub fn deny(message: impl Into<String>) -> Self {
        Self {
            behavior: PermissionBehavior::Deny,
            selected_action_id: None,
            message: Some(message.into()),
            updated_input: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentUsage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_cost_usd: Option<f64>,
    pub context_window_max_tokens: Option<u64>,
    pub context_window_used_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentRuntimeInfo {
    pub provider: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub mode_id: Option<String>,
    pub thinking_option_id: Option<String>,
}

/// What a session emits. Everything the provider streams for the session is also forwarded as
/// `Provider`, so no information is lost by the lifecycle events layered on top.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    TurnStarted {
        turn_id: String,
    },
    /// Raw provider event belonging to this session.
    Provider {
        turn_id: Option<String>,
        event: Arc<GlobalEvent>,
    },
    PermissionRequested {
        turn_id: Option<String>,
        request: AgentPermissionRequest,
    },
    PermissionResolved {
        request_id: String,
        reply: Option<String>,
    },
    UsageUpdated {
        turn_id: Option<String>,
        usage: AgentUsage,
    },
    ModelChanged {
        model: Option<String>,
        thinking_option_id: Option<String>,
    },
    TurnCompleted {
        turn_id: String,
        usage: AgentUsage,
    },
    TurnFailed {
        turn_id: String,
        error: String,
    },
    TurnCanceled {
        turn_id: String,
        reason: String,
    },
}

impl AgentEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            AgentEvent::TurnCompleted { .. }
                | AgentEvent::TurnFailed { .. }
                | AgentEvent::TurnCanceled { .. }
        )
    }
}

#[derive(Debug, Clone)]
pub struct AgentRunResult {
    pub session_id: String,
    pub turn_id: String,
    /// Concatenated assistant text of the turn (from raw part events).
    pub final_text: String,
    pub canceled: bool,
    pub error: Option<String>,
    pub usage: AgentUsage,
    pub events: Vec<AgentEvent>,
}
