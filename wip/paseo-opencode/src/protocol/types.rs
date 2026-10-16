//! REST payload types. Field names follow OpenCode's wire casing (`sessionID`, `modelID`, ...).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

pub type JsonMap = Map<String, Value>;

/// Defines a struct that keeps unknown fields in `extra`.
macro_rules! wire {
    ($(#[$m:meta])* pub struct $name:ident { $( $(#[$fm:meta])* pub $f:ident : $t:ty ),* $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
        pub struct $name {
            $( $(#[$fm])* pub $f: $t, )*
            #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
            pub extra: JsonMap,
        }
    };
}
pub(crate) use wire;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Ask,
    Allow,
    Deny,
}

wire! {
    /// One entry of a session's `permission` ruleset (also used by `PATCH /session/{id}`).
    pub struct PermissionRule {
        pub permission: String,
        pub pattern: String,
        pub action: Option<PermissionAction>,
    }
}

wire! {
    pub struct Session {
        pub id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub slug: Option<String>,
        #[serde(rename = "projectID", default, skip_serializing_if = "Option::is_none")]
        pub project_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub directory: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub path: Option<String>,
        #[serde(rename = "parentID", default, skip_serializing_if = "Option::is_none")]
        pub parent_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub model: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub time: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub summary: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub share: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub revert: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub permission: Option<Vec<PermissionRule>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cost: Option<Number>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tokens: Option<Tokens>,
    }
}

wire! {
    pub struct Tokens {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub input: Option<Number>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub output: Option<Number>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reasoning: Option<Number>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub total: Option<Number>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cache: Option<CacheTokens>,
    }
}

wire! {
    pub struct CacheTokens {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub read: Option<Number>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub write: Option<Number>,
    }
}

wire! {
    /// `info` of `message.updated` and of `GET /session/{id}/message` entries (user or assistant).
    pub struct MessageInfo {
        pub id: String,
        #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
        pub session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub role: Option<String>,
        #[serde(rename = "parentID", default, skip_serializing_if = "Option::is_none")]
        pub parent_id: Option<String>,
        #[serde(rename = "modelID", default, skip_serializing_if = "Option::is_none")]
        pub model_id: Option<String>,
        #[serde(rename = "providerID", default, skip_serializing_if = "Option::is_none")]
        pub provider_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agent: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub mode: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub finish: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub error: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub time: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub model: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub path: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub summary: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cost: Option<Number>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tokens: Option<Tokens>,
    }
}

impl MessageInfo {
    pub fn is_assistant(&self) -> bool {
        self.role.as_deref() == Some("assistant")
    }
}

wire! {
    /// Text and reasoning parts.
    pub struct TextPart {
        pub id: String,
        #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
        pub session_id: Option<String>,
        #[serde(rename = "messageID", default, skip_serializing_if = "Option::is_none")]
        pub message_id: Option<String>,
        #[serde(default)]
        pub text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub synthetic: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ignored: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub time: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub metadata: Option<Value>,
    }
}

wire! {
    /// `state` of a tool part: `status` is `pending | running | completed | error`.
    pub struct ToolState {
        pub status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub input: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub output: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub error: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub metadata: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub time: Option<Value>,
    }
}

wire! {
    pub struct ToolPart {
        pub id: String,
        #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
        pub session_id: Option<String>,
        #[serde(rename = "messageID", default, skip_serializing_if = "Option::is_none")]
        pub message_id: Option<String>,
        #[serde(rename = "callID", default)]
        pub call_id: String,
        #[serde(default)]
        pub tool: String,
        pub state: Option<ToolState>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub metadata: Option<Value>,
    }
}

wire! {
    /// Parts without payload we interpret (`step-start`, `step-finish`, `file`, ...). The fields that
    /// matter are typed; everything else stays in `extra`.
    pub struct OtherPart {
        pub id: String,
        #[serde(rename = "sessionID", default, skip_serializing_if = "Option::is_none")]
        pub session_id: Option<String>,
        #[serde(rename = "messageID", default, skip_serializing_if = "Option::is_none")]
        pub message_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub cost: Option<Number>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tokens: Option<Tokens>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub mime: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub filename: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub url: Option<String>,
    }
}

/// A message part. Unknown `type` values (or shapes that do not parse) land in `Unknown`.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(Box<TextPart>),
    Reasoning(Box<TextPart>),
    Tool(Box<ToolPart>),
    StepStart(Box<OtherPart>),
    StepFinish(Box<OtherPart>),
    File(Box<OtherPart>),
    Unknown(Value),
}

impl Part {
    pub fn from_value(v: Value) -> Part {
        let ty = v
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let parsed = match ty.as_str() {
            "text" => serde_json::from_value(v.clone()).map(|p| Part::Text(Box::new(p))),
            "reasoning" => serde_json::from_value(v.clone()).map(|p| Part::Reasoning(Box::new(p))),
            "tool" => serde_json::from_value(v.clone()).map(|p| Part::Tool(Box::new(p))),
            "step-start" => serde_json::from_value(v.clone()).map(|p| Part::StepStart(Box::new(p))),
            "step-finish" => {
                serde_json::from_value(v.clone()).map(|p| Part::StepFinish(Box::new(p)))
            }
            "file" => serde_json::from_value(v.clone()).map(|p| Part::File(Box::new(p))),
            _ => return Part::Unknown(v),
        };
        parsed.unwrap_or(Part::Unknown(v))
    }

    pub fn to_value(&self) -> Value {
        fn tagged<T: Serialize>(ty: &str, p: &T) -> Value {
            let mut v = serde_json::to_value(p).unwrap_or(Value::Null);
            if let Value::Object(m) = &mut v {
                m.insert("type".into(), Value::String(ty.into()));
            }
            v
        }
        match self {
            Part::Text(p) => tagged("text", p),
            Part::Reasoning(p) => tagged("reasoning", p),
            Part::Tool(p) => tagged("tool", p),
            Part::StepStart(p) => tagged("step-start", p),
            Part::StepFinish(p) => tagged("step-finish", p),
            Part::File(p) => tagged("file", p),
            Part::Unknown(v) => v.clone(),
        }
    }

    pub fn id(&self) -> Option<&str> {
        match self {
            Part::Text(p) | Part::Reasoning(p) => Some(&p.id),
            Part::Tool(p) => Some(&p.id),
            Part::StepStart(p) | Part::StepFinish(p) | Part::File(p) => Some(&p.id),
            Part::Unknown(v) => v.get("id").and_then(Value::as_str),
        }
    }
}

impl Serialize for Part {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.to_value().serialize(s)
    }
}

impl<'de> Deserialize<'de> for Part {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Part::from_value(Value::deserialize(d)?))
    }
}

wire! {
    /// One entry of `GET /session/{id}/message`.
    pub struct MessageWithParts {
        pub info: MessageInfo,
        #[serde(default)]
        pub parts: Vec<Part>,
    }
}

wire! {
    /// Value of `GET /session/status` per session; idle sessions are absent from the map.
    pub struct SessionStatusInfo {
        #[serde(rename = "type")]
        pub kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub attempt: Option<Number>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub next: Option<Number>,
    }
}

wire! {
    pub struct ModelLimit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub context: Option<Number>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub output: Option<Number>,
    }
}

wire! {
    pub struct ModelInfo {
        pub id: String,
        #[serde(rename = "providerID", default, skip_serializing_if = "Option::is_none")]
        pub provider_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub family: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub limit: Option<ModelLimit>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub capabilities: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub variants: Option<JsonMap>,
    }
}

wire! {
    pub struct ProviderInfo {
        pub id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub source: Option<String>,
        #[serde(default)]
        pub models: Map<String, Value>,
    }
}

impl ProviderInfo {
    /// Models parsed leniently; entries that do not parse are skipped.
    pub fn parsed_models(&self) -> Vec<ModelInfo> {
        self.models
            .values()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect()
    }
}

wire! {
    /// `GET /provider`.
    pub struct ProviderList {
        #[serde(default)]
        pub all: Vec<ProviderInfo>,
        #[serde(default)]
        pub default: Map<String, Value>,
        #[serde(default)]
        pub connected: Vec<String>,
    }
}

wire! {
    /// `GET /agent` entry. `mode` is `primary | subagent | all`.
    pub struct AgentInfo {
        pub name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub mode: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub native: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub hidden: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub color: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub permission: Option<Vec<PermissionRule>>,
    }
}

wire! {
    /// `GET /command` entry.
    pub struct CommandInfo {
        pub name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub template: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub hints: Option<Vec<String>>,
    }
}

wire! {
    pub struct PermissionToolRef {
        #[serde(rename = "messageID", default, skip_serializing_if = "Option::is_none")]
        pub message_id: Option<String>,
        #[serde(rename = "callID", default, skip_serializing_if = "Option::is_none")]
        pub call_id: Option<String>,
    }
}

wire! {
    /// `permission.asked` properties and `GET /permission` entries.
    pub struct PermissionRequest {
        pub id: String,
        #[serde(rename = "sessionID")]
        pub session_id: String,
        #[serde(default)]
        pub permission: String,
        #[serde(default)]
        pub patterns: Vec<String>,
        #[serde(default)]
        pub metadata: JsonMap,
        #[serde(default)]
        pub always: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tool: Option<PermissionToolRef>,
    }
}

wire! {
    pub struct QuestionOption {
        #[serde(default)]
        pub label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub description: Option<String>,
    }
}

wire! {
    pub struct QuestionInfo {
        #[serde(default)]
        pub question: String,
        #[serde(default)]
        pub header: String,
        #[serde(default)]
        pub options: Vec<QuestionOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub multiple: Option<bool>,
    }
}

wire! {
    /// `question.asked` properties and `GET /question` entries.
    pub struct QuestionRequest {
        pub id: String,
        #[serde(rename = "sessionID")]
        pub session_id: String,
        #[serde(default)]
        pub questions: Vec<QuestionInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tool: Option<PermissionToolRef>,
    }
}
