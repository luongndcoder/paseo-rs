//! `StoredAgentRecord` (`STORED_AGENT_SCHEMA` in `agent/agent-storage.ts`).
//!
//! Field order is the schema order so a load -> write round trip keeps key order. `undefined`
//! is omitted and `null` is kept ([`Nullable`]). Every object carries `#[serde(flatten)] extra`
//! so unknown keys survive a rewrite (decision Q3): TS strips them, both runtimes read the
//! same files during the strangler, so Rust must not destroy fields a newer writer added.

use crate::nullable::{required_nullable, strict_optional, Nullable};
use indexmap::IndexMap;
use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

pub type JsonMap = Map<String, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentLifecycle {
    Initializing,
    Idle,
    Running,
    Error,
    #[default]
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttentionReason {
    Finished,
    Error,
    Permission,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OwnerKind {
    Daemon,
}

/// `AgentOwnerSchema`: a discriminated union with the single `daemon` member.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentOwner {
    pub kind: OwnerKind,
    pub daemon_id: String,
    pub execution_id: String,
    #[serde(flatten)]
    pub extra: JsonMap,
}

impl AgentOwner {
    /// `daemonExecutionKey`.
    pub fn execution_key(&self) -> String {
        format!("{}\0{}", self.daemon_id, self.execution_id)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolGrant {
    pub kind: McpKind,
    pub server: String,
    pub tool: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpKind {
    Mcp,
}

/// `toolPolicy` is a strict object in the schema, so unknown keys are a validation error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicy {
    pub preapproved: Vec<ToolGrant>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SerializableConfig {
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub mode_id: Nullable<String>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub model: Nullable<String>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub thinking_option_id: Nullable<String>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub feature_values: Nullable<JsonMap>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub provider_options: Nullable<JsonMap>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub tool_policy: Nullable<ToolPolicy>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub system_prompt: Nullable<String>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub mcp_servers: Nullable<JsonMap>,
    #[serde(flatten)]
    pub extra: JsonMap,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeInfo {
    pub provider: String,
    /// Required key, may be null.
    #[serde(deserialize_with = "required_nullable")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub model: Nullable<String>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub thinking_option_id: Nullable<String>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub mode_id: Nullable<String>,
    #[serde(
        default,
        rename = "extra",
        deserialize_with = "strict_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub extra_info: Option<JsonMap>,
    #[serde(flatten)]
    pub extra: JsonMap,
}

/// `PERSISTENCE_HANDLE_SCHEMA` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistenceHandle {
    pub provider: String,
    pub session_id: String,
    /// `z.any().optional()`: any JSON value, including null.
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub native_handle: Nullable<Value>,
    #[serde(
        default,
        deserialize_with = "strict_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub metadata: Option<JsonMap>,
    #[serde(flatten)]
    pub extra: JsonMap,
}

/// Validates the `AgentFeatureSchema` discriminated union; the value itself is kept verbatim.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct AgentFeature(pub Value);

impl<'de> Deserialize<'de> for AgentFeature {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        validate_feature(&value).map_err(de::Error::custom)?;
        Ok(AgentFeature(value))
    }
}

fn validate_feature(value: &Value) -> Result<(), String> {
    let obj = value.as_object().ok_or("feature: expected object")?;
    let str_field = |key: &str| -> Result<(), String> {
        match obj.get(key) {
            Some(Value::String(_)) => Ok(()),
            _ => Err(format!("feature.{key}: expected string")),
        }
    };
    let opt_str = |key: &str| -> Result<(), String> {
        match obj.get(key) {
            None | Some(Value::String(_)) => Ok(()),
            _ => Err(format!("feature.{key}: expected string")),
        }
    };
    str_field("id")?;
    str_field("label")?;
    opt_str("description")?;
    opt_str("tooltip")?;
    opt_str("icon")?;
    match obj.get("type").and_then(Value::as_str) {
        Some("toggle") => match obj.get("value") {
            Some(Value::Bool(_)) => Ok(()),
            _ => Err("feature.value: expected boolean".into()),
        },
        Some("select") => {
            match obj.get("desktopTrigger") {
                None => {}
                Some(Value::String(s)) if s == "icon" || s == "label" => {}
                _ => return Err("feature.desktopTrigger: invalid".into()),
            }
            match obj.get("value") {
                Some(Value::String(_) | Value::Null) => {}
                _ => return Err("feature.value: expected string | null".into()),
            }
            let options = obj
                .get("options")
                .and_then(Value::as_array)
                .ok_or("feature.options: expected array")?;
            for option in options {
                let o = option.as_object().ok_or("option: expected object")?;
                for key in ["id", "label"] {
                    if !matches!(o.get(key), Some(Value::String(_))) {
                        return Err(format!("option.{key}: expected string"));
                    }
                }
                if !matches!(o.get("description"), None | Some(Value::String(_))) {
                    return Err("option.description: expected string".into());
                }
                if !matches!(o.get("isDefault"), None | Some(Value::Bool(_))) {
                    return Err("option.isDefault: expected boolean".into());
                }
                if !matches!(o.get("metadata"), None | Some(Value::Object(_))) {
                    return Err("option.metadata: expected object".into());
                }
            }
            Ok(())
        }
        _ => Err("feature.type: invalid discriminator".into()),
    }
}

/// `z.record(z.string(), z.string()).default({})`.
pub type Labels = IndexMap<String, String>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredAgentRecord {
    pub id: String,
    pub provider: String,
    pub cwd: String,
    #[serde(
        default,
        deserialize_with = "strict_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub workspace_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(
        default,
        deserialize_with = "strict_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_activity_at: Option<String>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub last_user_message_at: Nullable<String>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub title: Nullable<String>,
    #[serde(default)]
    pub labels: Labels,
    #[serde(default)]
    pub last_status: AgentLifecycle,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub last_mode_id: Nullable<String>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub config: Nullable<SerializableConfig>,
    #[serde(
        default,
        deserialize_with = "strict_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub runtime_info: Option<RuntimeInfo>,
    #[serde(
        default,
        deserialize_with = "strict_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub features: Option<Vec<AgentFeature>>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub persistence: Nullable<PersistenceHandle>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub last_error: Nullable<String>,
    #[serde(
        default,
        deserialize_with = "strict_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub requires_attention: Option<bool>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub attention_reason: Nullable<AttentionReason>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub attention_timestamp: Nullable<String>,
    #[serde(
        default,
        deserialize_with = "strict_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub internal: Option<bool>,
    #[serde(default, skip_serializing_if = "Nullable::is_missing")]
    pub archived_at: Nullable<String>,
    #[serde(
        default,
        deserialize_with = "strict_optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub owner: Option<AgentOwner>,
    #[serde(flatten)]
    pub extra: JsonMap,
}

impl StoredAgentRecord {
    /// Minimal valid record, the starting point for tests and callers that build records.
    pub fn new(id: &str, provider: &str, cwd: &str, now: &str) -> Self {
        StoredAgentRecord {
            id: id.into(),
            provider: provider.into(),
            cwd: cwd.into(),
            workspace_id: None,
            created_at: now.into(),
            updated_at: now.into(),
            last_activity_at: None,
            last_user_message_at: Nullable::Missing,
            title: Nullable::Missing,
            labels: Labels::new(),
            last_status: AgentLifecycle::Closed,
            last_mode_id: Nullable::Missing,
            config: Nullable::Missing,
            runtime_info: None,
            features: None,
            persistence: Nullable::Missing,
            last_error: Nullable::Missing,
            requires_attention: None,
            attention_reason: Nullable::Missing,
            attention_timestamp: Nullable::Missing,
            internal: None,
            archived_at: Nullable::Missing,
            owner: None,
            extra: JsonMap::new(),
        }
    }

    pub fn is_archived(&self) -> bool {
        matches!(self.archived_at, Nullable::Value(_))
    }
}

/// `parseStoredAgentRecord`.
pub fn parse_stored_agent_record(value: Value) -> Result<StoredAgentRecord, serde_json::Error> {
    serde_json::from_value(value)
}
