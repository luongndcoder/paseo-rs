//! Provider-agnostic surface: the Rust equivalent of the types `AgentClient` / `AgentSession`
//! in `agent-sdk-types.ts` use. Timeline and `ToolCallDetail` mapping is P4 and not here;
//! session events carry the raw ACP payloads instead.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::RequestPermissionRequest;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentMode {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectorOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub is_default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelDefinition {
    pub provider: String,
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub is_default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking_options: Vec<SelectorOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_thinking_option_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlashCommandKind {
    Command,
    Skill,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    pub argument_hint: String,
    pub kind: SlashCommandKind,
}

/// A select feature derived from an ACP config option, or the local `auto_accept` toggle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Feature {
    Toggle {
        id: String,
        label: String,
        description: Option<String>,
        tooltip: Option<String>,
        icon: Option<String>,
        value: bool,
    },
    Select {
        id: String,
        label: String,
        description: Option<String>,
        tooltip: Option<String>,
        icon: Option<String>,
        value: Option<String>,
        options: Vec<SelectorOption>,
    },
}

/// Maps an ACP select config option to a Paseo feature (`ACPConfigFeatureOption`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ConfigFeatureOption {
    pub id: String,
    pub config_id: String,
    pub category: Option<String>,
    pub label: String,
    pub description: Option<String>,
    pub tooltip: Option<String>,
    pub icon: Option<String>,
    pub empty_option_label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityFlags {
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

impl Default for CapabilityFlags {
    /// `DEFAULT_ACP_CAPABILITIES`.
    fn default() -> Self {
        CapabilityFlags {
            supports_streaming: true,
            supports_session_persistence: true,
            supports_session_listing: true,
            supports_dynamic_modes: true,
            supports_mcp_servers: true,
            supports_reasoning_stream: true,
            supports_tool_invocations: true,
            supports_rewind_conversation: false,
            supports_rewind_files: false,
            supports_rewind_both: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum McpServerConfig {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
    Sse {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

/// Per-session overrides carried in `providerOptions` (`ACPProviderOptionsSchema`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderOptions {
    pub supports_mcp_servers: Option<bool>,
    pub client_capabilities: Option<ClientCapabilitiesOverride>,
}

/// Partial client capabilities merged over the base set; unset fields keep the base value.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClientCapabilitiesOverride {
    pub read_text_file: Option<bool>,
    pub write_text_file: Option<bool>,
    pub terminal: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionConfig {
    pub cwd: String,
    pub model: Option<String>,
    pub mode_id: Option<String>,
    pub thinking_option_id: Option<String>,
    pub feature_values: BTreeMap<String, Value>,
    pub title: Option<String>,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    pub provider_options: ProviderOptions,
}

/// Persisted handle used to resume a session (`AgentPersistenceHandle`). `metadata` holds the
/// stored session config; only `cwd`, `model`, `modeId`, `thinkingOptionId`, `featureValues`,
/// `title` are read back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistenceHandle {
    pub provider: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResumePurpose {
    #[default]
    Interactive,
    /// Replay persisted events, then release the process.
    History,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResumeOverrides {
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub mode_id: Option<String>,
    pub thinking_option_id: Option<String>,
    pub feature_values: Option<BTreeMap<String, Value>>,
    pub provider_options: Option<ProviderOptions>,
    pub mcp_servers: Option<BTreeMap<String, McpServerConfig>>,
}

#[derive(Debug, Clone, Default)]
pub struct CreateSessionOptions {
    /// Model ids from provider config that must be accepted even when the agent does not advertise them.
    pub configured_model_ids: Vec<String>,
    pub resume_purpose: ResumePurpose,
    pub agent_id: Option<String>,
    /// Extra env for the agent process (`AgentLaunchContext.env`).
    pub launch_env: BTreeMap<String, String>,
}

/// Prompt content. Attachments other than images are rendered to text by the caller
/// (`renderPromptAttachmentAsText` is daemon-side).
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
    fn from(s: &str) -> Self {
        PromptInput::Text(s.to_string())
    }
}

impl From<String> for PromptInput {
    fn from(s: String) -> Self {
        PromptInput::Text(s)
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub client_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PermissionResponse {
    Allow {
        selected_action_id: Option<String>,
    },
    Deny {
        selected_action_id: Option<String>,
        interrupt: bool,
    },
}

impl PermissionResponse {
    pub fn allow() -> Self {
        PermissionResponse::Allow {
            selected_action_id: None,
        }
    }
    pub fn deny() -> Self {
        PermissionResponse::Deny {
            selected_action_id: None,
            interrupt: false,
        }
    }
}

/// A permission request waiting for a decision. `raw` is the untouched ACP request.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingPermission {
    pub id: String,
    pub provider: String,
    pub raw: RequestPermissionRequest,
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TurnUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub context_window_max_tokens: Option<u64>,
    pub context_window_used_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Catalog {
    pub models: Vec<ModelDefinition>,
    pub modes: Vec<AgentMode>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeInfo {
    pub provider: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub thinking_option_id: Option<String>,
    pub mode_id: Option<String>,
    pub title: Option<String>,
}
