//! Shared payload types: messages, models, session state, UI exchange.

use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};

/// Fields this crate does not model, preserved verbatim.
pub type Extra = serde_json::Map<String, Value>;

/// A payload with no modeled fields.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Bare {
    #[serde(flatten)]
    pub extra: Extra,
}

// ---- thinking ---------------------------------------------------------------------------

/// `off | minimal | low | medium | high | xhigh | max` (OMP adds `auto`). Kept as a string so a
/// future level does not break parsing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThinkingLevel(pub String);

impl ThinkingLevel {
    pub const KNOWN: [&'static str; 8] = [
        "off", "minimal", "low", "medium", "high", "xhigh", "max", "auto",
    ];

    /// Case-insensitive match against the known levels, like `normalizePiThinkingOption`.
    pub fn normalize(input: &str) -> Option<Self> {
        let lowered = input.trim().to_ascii_lowercase();
        if Self::KNOWN.contains(&lowered.as_str()) {
            Some(ThinkingLevel(lowered))
        } else {
            None
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ---- content ----------------------------------------------------------------------------

fn image_type() -> String {
    "image".to_string()
}

/// A base64 image attached to a prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageContent {
    #[serde(rename = "type", default = "image_type")]
    pub kind: String,
    pub data: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    #[serde(flatten)]
    pub extra: Extra,
}

impl ImageContent {
    pub fn new(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self {
            kind: image_type(),
            data: data.into(),
            mime_type: mime_type.into(),
            extra: Extra::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextContent {
    pub text: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThinkingContent {
    pub thinking: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallContent {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(flatten)]
    pub extra: Extra,
}

tagged_enum! {
    /// A block inside a user / custom message.
    pub enum ContentBlock ["type"] {
        Text(TextContent) = "text",
        Image(ImageContent) = "image",
    }
}

tagged_enum! {
    /// A block inside an assistant message.
    pub enum AssistantContent ["type"] {
        Text(TextContent) = "text",
        Thinking(ThinkingContent) = "thinking",
        ToolCall(ToolCallContent) = "toolCall",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

// ---- messages ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContentMessage {
    pub content: MessageContent,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CustomMessage {
    pub content: MessageContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<AssistantContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl AssistantMessage {
    /// Concatenated text blocks, without thinking or tool calls.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub content: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BashExecutionMessage {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<bool>,
    pub timestamp: Number,
    #[serde(flatten)]
    pub extra: Extra,
}

tagged_enum! {
    /// A conversation message. Roles Paseo does not render (`pythonExecution`, `hookMessage`,
    /// `branchSummary`, ...) land in `Other`.
    pub enum Message ["role"] {
        User(ContentMessage) = "user",
        /// OMP injects hidden developer messages such as rule-violation reminders.
        Developer(ContentMessage) = "developer",
        Custom(CustomMessage) = "custom",
        Assistant(AssistantMessage) = "assistant",
        ToolResult(ToolResultMessage) = "toolResult",
        BashExecution(BashExecutionMessage) = "bashExecution",
        /// Pi 1.x emits the system prompt as a message at the start of a run.
        System(Bare) = "system",
    }
}

impl Message {
    pub fn role(&self) -> Option<&str> {
        self.tag()
    }

    pub fn as_assistant(&self) -> Option<&AssistantMessage> {
        match self {
            Message::Assistant(m) => Some(m),
            _ => None,
        }
    }
}

// ---- models, state, stats ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub provider: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    /// OMP: `{mode, efforts, defaultLevel, effortMap}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Value>,
    /// Pi: thinking level -> provider value (or null when unsupported).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Model {
    /// `provider/id`, the form Paseo uses as a model id.
    pub fn qualified_id(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Number>,
    #[serde(
        default,
        rename = "contextWindow",
        skip_serializing_if = "Option::is_none"
    )]
    pub context_window: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percent: Option<Number>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Model>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
    pub is_streaming: bool,
    pub is_compacting: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compaction_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub message_count: u64,
    /// Pi.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_message_count: Option<u64>,
    /// OMP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_message_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<ContextUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_phases: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<Number>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStats {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<TokenStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<ContextUsage>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Pi: `extension | prompt | skill`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_info: Option<Value>,
    /// `{hint}`; OMP may send `null`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl SlashCommand {
    pub fn input_hint(&self) -> Option<&str> {
        self.input.as_ref()?.get("hint")?.as_str()
    }
}

// ---- responses and acks -----------------------------------------------------------------

/// A `type:"response"` frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseFrame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// What the provider said when it accepted a `prompt`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PromptAck {
    pub request_id: Option<String>,
    /// `Some(false)`: the prompt was handled without invoking the model (e.g. a slash command).
    pub agent_invoked: Option<bool>,
}

// ---- extension UI ------------------------------------------------------------------------

/// `extension_ui_request`: a dialog (`select`, `input`, `editor`, `confirm`) or a fire-and-forget
/// notification (`notify`, `setWidget`, `setStatus`, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiRequest {
    pub id: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl UiRequest {
    /// Dialogs wait for an `extension_ui_response`; everything else does not.
    pub fn is_dialog(&self) -> bool {
        matches!(
            self.method.as_str(),
            "select" | "input" | "editor" | "confirm"
        )
    }
}

/// Body of an `extension_ui_response` (the `type` and `id` are added on send).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UiResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<bool>,
}

impl UiResponse {
    pub fn value(value: impl Into<String>) -> Self {
        Self {
            value: Some(value.into()),
            ..Self::default()
        }
    }

    pub fn confirmed(confirmed: bool) -> Self {
        Self {
            confirmed: Some(confirmed),
            ..Self::default()
        }
    }

    pub fn cancelled() -> Self {
        Self {
            cancelled: Some(true),
            ..Self::default()
        }
    }
}
