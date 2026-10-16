//! Typed wire shapes. Field names and tags match the TS `agent-sdk-types.ts` JSON exactly
//! (camelCase fields, snake_case discriminators). Open-ended parts stay `serde_json::Value`.

use paseo_agent_types::{
    AgentMode, AgentRuntimeInfo, AgentTimelineItem, ProviderSubagentInputEvent,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

pub type JsonObject = Map<String, Value>;

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPersistenceHandle {
    pub provider: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonObject>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_used_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    Tool,
    Plan,
    Question,
    Mode,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionBehavior {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionAction {
    pub id: String,
    pub label: String,
    pub behavior: PermissionBehavior,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub id: String,
    pub provider: String,
    pub name: String,
    pub kind: PermissionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<JsonObject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestions: Option<Vec<JsonObject>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<Vec<PermissionAction>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonObject>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "behavior",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PermissionResponse {
    Allow {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected_action_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_input: Option<JsonObject>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_permissions: Option<Vec<JsonObject>>,
    },
    Deny {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selected_action_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interrupt: Option<bool>,
    },
}

impl PermissionResponse {
    pub fn allow() -> Self {
        Self::Allow {
            selected_action_id: None,
            updated_input: None,
            updated_permissions: None,
        }
    }

    pub fn deny(message: impl Into<String>) -> Self {
        Self::Deny {
            selected_action_id: None,
            message: Some(message.into()),
            interrupt: None,
        }
    }
}

/// `AgentStreamEvent`. Unmodelled event types decode as [`AgentStreamEvent::Unknown`]; the
/// untouched JSON is always available as [`crate::SessionEvent::raw`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AgentStreamEvent {
    ThreadStarted {
        session_id: String,
        provider: String,
    },
    TurnStarted {
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    TurnCompleted {
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<AgentUsage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    TurnFailed {
        provider: String,
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostic: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    TurnCanceled {
        provider: String,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    UsageUpdated {
        provider: String,
        usage: AgentUsage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    Timeline {
        item: TimelineItem,
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timestamp: Option<String>,
    },
    PermissionRequested {
        provider: String,
        request: Box<PermissionRequest>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    PermissionResolved {
        provider: String,
        request_id: String,
        resolution: PermissionResponse,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
    },
    ModeChanged {
        provider: String,
        #[serde(default)]
        current_mode_id: Option<String>,
        #[serde(default)]
        available_modes: Vec<AgentMode>,
    },
    ModelChanged {
        provider: String,
        runtime_info: AgentRuntimeInfo,
    },
    ThinkingOptionChanged {
        provider: String,
        #[serde(default)]
        thinking_option_id: Option<String>,
    },
    AttentionRequired {
        provider: String,
        /// `finished`, `error` or `permission`.
        reason: String,
        timestamp: String,
    },
    ProviderSubagent {
        provider: String,
        event: Box<ProviderSubagentInputEvent>,
    },
    #[serde(other)]
    Unknown,
}

/// `Timeline.item`: decoded into [`AgentTimelineItem`] when it validates, otherwise the
/// untouched JSON (an item shape this crate does not know must not break the stream).
#[derive(Debug, Clone, PartialEq)]
pub enum TimelineItem {
    Typed(Box<AgentTimelineItem>),
    Raw(Value),
}

impl TimelineItem {
    pub fn typed(&self) -> Option<&AgentTimelineItem> {
        match self {
            Self::Typed(item) => Some(item),
            Self::Raw(_) => None,
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Self::Typed(item) => item.to_value(),
            Self::Raw(value) => value.clone(),
        }
    }
}

impl Serialize for TimelineItem {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TimelineItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Ok(match AgentTimelineItem::from_value(&value) {
            Ok(item) => Self::Typed(Box::new(item)),
            Err(_) => Self::Raw(value),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRunResult {
    pub session_id: String,
    pub final_text: String,
    #[serde(default)]
    pub usage: Option<AgentUsage>,
    #[serde(default)]
    pub timeline: Vec<Value>,
    #[serde(default)]
    pub canceled: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStarted {
    pub turn_id: String,
}

/// Result of `provider.createSession`, `provider.resumeSession` and the session part of
/// `provider.importSession`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub handle: String,
    pub provider: String,
    pub session_id: Option<String>,
    #[serde(default)]
    pub capabilities: JsonObject,
    #[serde(default)]
    pub features: Vec<Value>,
    #[serde(default)]
    pub initial_timeline: Vec<Value>,
    #[serde(default)]
    pub persistence: Option<AgentPersistenceHandle>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub id: String,
    pub label: String,
    pub description: String,
    pub enabled: bool,
    #[serde(default)]
    pub default_mode_id: Option<String>,
    #[serde(default)]
    pub modes: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCatalog {
    #[serde(default)]
    pub models: Vec<Value>,
    #[serde(default)]
    pub modes: Vec<Value>,
    #[serde(default)]
    pub default_mode_id: Option<String>,
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

/// Prompt input: plain text or a content block array (`AgentPromptContentBlock[]`).
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
