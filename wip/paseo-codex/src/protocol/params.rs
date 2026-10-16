//! Typed client request parameters and the response shapes this crate reads.
//!
//! Every struct skips unset optional fields so the serialized request stays minimal and byte-wise
//! close to what the TS adapter sends.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::notifications::Thread;

pub const CLIENT_NAME: &str = "codex_app_server_daemon";

#[derive(Debug, Clone, Serialize)]
pub struct ClientInfo {
    pub name: String,
    pub title: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeCapabilities {
    pub experimental_api: bool,
    pub mcp_server_openai_form_elicitation: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub client_info: ClientInfo,
    pub capabilities: InitializeCapabilities,
}

impl Default for InitializeParams {
    /// Codex treats most client names as the model-request originator. This reserved name is
    /// non-originating, so requests keep Codex's default CLI identity in provider usage logs.
    fn default() -> Self {
        InitializeParams {
            client_info: ClientInfo {
                name: CLIENT_NAME.into(),
                title: "Codex App Server Daemon".into(),
                version: "0.0.0".into(),
            },
            capabilities: InitializeCapabilities {
                experimental_api: true,
                mcp_server_openai_form_elicitation: true,
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub codex_home: Option<String>,
    #[serde(default)]
    pub platform_family: Option<String>,
    #[serde(default)]
    pub platform_os: Option<String>,
}

/// One entry of `turn/start.input`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum UserInput {
    Text {
        text: String,
        #[serde(rename = "text_elements")]
        text_elements: Vec<Value>,
    },
    LocalImage {
        path: String,
    },
    Skill {
        name: String,
        path: String,
    },
    Mention {
        name: String,
        path: String,
    },
}

impl UserInput {
    pub fn text(text: impl Into<String>) -> Self {
        UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Always serialized (`null` when unset), as the TS adapter does.
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResumeParams {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub thread_id: String,
    pub input: Vec<UserInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approvals_reviewer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_mode: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_user_message_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnInterruptParams {
    pub thread_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSteerParams {
    pub thread_id: String,
    pub expected_turn_id: String,
    pub input: Vec<UserInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_user_message_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_providers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReadParams {
    pub thread_id: String,
    pub include_turns: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadIdParams {
    pub thread_id: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadForkParams {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// `thread/rollback` exists on Codex < 0.156 only.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRollbackParams {
    pub thread_id: String,
    pub num_turns: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillsListParams {
    pub cwds: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ConfigReadParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

pub fn to_value<T: Serialize>(params: &T) -> Value {
    serde_json::to_value(params).unwrap_or_else(|_| json!({}))
}

// ---- responses ----

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResponse {
    pub thread: Thread,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_provider: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub approval_policy: Option<Value>,
    #[serde(default)]
    pub approvals_reviewer: Option<String>,
    #[serde(default)]
    pub sandbox: Option<Value>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEffortEntry {
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceTier {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
    #[serde(default)]
    pub supported_reasoning_efforts: Vec<ReasoningEffortEntry>,
    #[serde(default)]
    pub service_tiers: Vec<ServiceTier>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelListResponse {
    #[serde(default)]
    pub data: Vec<ModelInfo>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListResponse {
    #[serde(default)]
    pub data: Vec<Thread>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CollaborationModeEntry {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub developer_instructions: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CollaborationModeListResponse {
    #[serde(default)]
    pub data: Vec<CollaborationModeEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Skill {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub path: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillsEntry {
    #[serde(default)]
    pub skills: Vec<Skill>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillsListResponse {
    #[serde(default)]
    pub data: Vec<SkillsEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_matches_ts_adapter() {
        assert_eq!(
            to_value(&InitializeParams::default()),
            json!({
                "clientInfo": {"name":"codex_app_server_daemon","title":"Codex App Server Daemon","version":"0.0.0"},
                "capabilities": {"experimentalApi": true, "mcpServerOpenaiFormElicitation": true}
            })
        );
    }

    #[test]
    fn text_input_has_empty_text_elements() {
        assert_eq!(
            serde_json::to_value(UserInput::text("hi")).unwrap(),
            json!({"type":"text","text":"hi","text_elements":[]})
        );
        assert_eq!(
            serde_json::to_value(UserInput::LocalImage {
                path: "/a.png".into()
            })
            .unwrap(),
            json!({"type":"localImage","path":"/a.png"})
        );
    }
}
