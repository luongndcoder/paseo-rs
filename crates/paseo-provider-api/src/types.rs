//! Provider-facing request/response types. Open-ended TS shapes stay `serde_json` values;
//! everything the core branches on is typed.

use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem, PersistenceHandle};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type JsonObject = Map<String, Value>;

/// `AgentSessionConfig`. Unknown keys round trip through `extra`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionConfig {
    pub provider: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_append_system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_option_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_values: Option<JsonObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_options: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_policy: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub internal: Option<bool>,
    #[serde(flatten)]
    pub extra: JsonObject,
}

impl AgentSessionConfig {
    pub fn new(provider: impl Into<String>, cwd: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            cwd: cwd.into(),
            ..Self::default()
        }
    }
}

/// One internal Paseo tool offered to a provider session. Calls come back through
/// [`crate::ToolCallHandler`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// JSON Schema of the tool input (`{"type":"object",...}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
}

/// `AgentLaunchContext`: runtime-only, never persisted.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<std::collections::BTreeMap<String, String>>,
    /// Internal Paseo tools; absent when the session gets none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paseo_tools: Option<Vec<ToolSpec>>,
}

/// `AgentCreateSessionOptions`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persist_session: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_model_ids: Option<Vec<String>>,
}

/// `FetchCatalogOptions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "scope",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum FetchCatalogOptions {
    Global { force: bool },
    Workspace { cwd: String, force: bool },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCatalog {
    #[serde(default)]
    pub models: Vec<Value>,
    #[serde(default)]
    pub modes: Vec<Value>,
    #[serde(default)]
    pub default_mode_id: Option<String>,
}

/// `AgentPromptInput`: plain text or content blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PromptInput {
    Text(String),
    Blocks(Vec<Value>),
}

impl From<&str> for PromptInput {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<String> for PromptInput {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

/// `AgentRunOptions` (`outputSchema`, `resumeFrom`, `maxThinkingTokens`, `clientMessageId`).
pub type RunOptions = JsonObject;

/// `SteerActiveTurnOptions`: run options plus the turn the steer targets.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SteerOptions {
    pub expected_turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_pending_permissions: Option<bool>,
    #[serde(flatten)]
    pub run: RunOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteerResult {
    Accepted,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnHandle {
    pub turn_id: String,
}

/// `AgentPermissionResult`: what answering a permission may ask the manager to do next.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up_prompt: Option<PromptInput>,
}

/// Outcome of `run_provider_turn` (`AgentRunResult`).
#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub session_id: String,
    pub final_text: String,
    pub usage: Option<paseo_agent_types::AgentUsage>,
    pub timeline: Vec<AgentTimelineItem>,
}

/// An imported timeline entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedTimelineEntry {
    pub item: AgentTimelineItem,
    pub timestamp: Option<String>,
}

/// What a session reports when it opens.
#[derive(Debug, Clone, Default)]
pub struct SessionOpenInfo {
    pub capabilities: JsonObject,
    pub features: Vec<Value>,
    pub initial_timeline: Vec<AgentTimelineItem>,
}

/// Result of `AgentClient::import_session`.
pub struct ImportedSession {
    pub session: Box<dyn crate::AgentSession>,
    pub config: AgentSessionConfig,
    pub persistence: PersistenceHandle,
    pub timeline: Vec<ImportedTimelineEntry>,
    /// Always `provider_subagent` events.
    pub provider_subagent_events: Vec<AgentStreamEvent>,
}

/// A reverse tool call from a provider (OpenCode bridge, host MCP) to the core.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
}

/// MCP `CallToolResult` shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallResult {
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}
