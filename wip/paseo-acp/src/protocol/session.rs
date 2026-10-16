//! Initialize, session lifecycle, modes, models and config options.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::content::{ContentBlock, Extra};
use super::lenient::Lenient;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Implementation {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsCapabilities {
    #[serde(default)]
    pub read_text_file: bool,
    #[serde(default)]
    pub write_text_file: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientCapabilities {
    #[serde(default)]
    pub fs: FsCapabilities,
    #[serde(default)]
    pub terminal: bool,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Extra>,
}

impl Default for ClientCapabilities {
    /// `BASE_ACP_CLIENT_CAPABILITIES` in the TS adapter.
    fn default() -> Self {
        ClientCapabilities {
            fs: FsCapabilities::default(),
            terminal: true,
            meta: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: u32,
    pub client_capabilities: ClientCapabilities,
    pub client_info: Implementation,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCapabilities {
    /// Presence is what matters (`{}` is truthy in the TS checks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_session: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_capabilities: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_capabilities: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_capabilities: Option<SessionCapabilities>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl AgentCapabilities {
    pub fn can_load_session(&self) -> bool {
        self.load_session == Some(true)
    }
    pub fn can_resume(&self) -> bool {
        self.session_capabilities
            .as_ref()
            .is_some_and(|c| c.resume.is_some())
    }
    pub fn can_close(&self) -> bool {
        self.session_capabilities
            .as_ref()
            .is_some_and(|c| c.close.is_some())
    }
    pub fn can_list(&self) -> bool {
        self.session_capabilities
            .as_ref()
            .is_some_and(|c| c.list.is_some())
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    #[serde(default)]
    pub protocol_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_capabilities: Option<AgentCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_info: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_methods: Option<Vec<Value>>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// MCP server entry as sent in `session/new|load|resume`. Stdio entries carry no `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpServer {
    Remote(RemoteMcpServer),
    Stdio(StdioMcpServer),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NameValue {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StdioMcpServer {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<NameValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteMcpServer {
    /// "http" or "sse".
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<NameValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewSessionParams {
    pub cwd: String,
    pub mcp_servers: Vec<McpServer>,
}

/// Params of `session/load` and `session/resume`. Agents such as Devin reject the call when
/// `cwd` or `mcpServers` is missing, even if empty; never drop them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadSessionParams {
    pub session_id: String,
    pub cwd: String,
    pub mcp_servers: Vec<McpServer>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMode {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModeState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_modes: Option<Vec<SessionMode>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_mode_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub model_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModelState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_models: Option<Vec<ModelInfo>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_model_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectChoice {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectGroup {
    pub group: String,
    pub name: String,
    pub options: Vec<SelectChoice>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SelectEntry {
    Choice(SelectChoice),
    Group(SelectGroup),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectConfigOption {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub current_value: String,
    pub options: Vec<SelectEntry>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KnownConfigOption {
    Select(SelectConfigOption),
}

pub type SessionConfigOption = Lenient<KnownConfigOption>;

pub fn as_select(option: &SessionConfigOption) -> Option<&SelectConfigOption> {
    match option {
        Lenient::Known(KnownConfigOption::Select(s)) => Some(s),
        Lenient::Unknown(_) => None,
    }
}

/// Union of the `session/new`, `session/load` and `session/resume` responses.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<SessionModelState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modes: Option<SessionModeState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<SessionConfigOption>>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub prompt: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_write_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResponse {
    pub stop_reason: Lenient<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl PromptResponse {
    pub fn is_cancelled(&self) -> bool {
        matches!(self.stop_reason, Lenient::Known(StopReason::Cancelled))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModeParams {
    pub session_id: String,
    pub mode_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModelParams {
    pub session_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionParams {
    pub session_id: String,
    pub config_id: String,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetConfigOptionResponse {
    #[serde(default)]
    pub config_options: Vec<SessionConfigOption>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsResponse {
    #[serde(default)]
    pub sessions: Vec<SessionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}
