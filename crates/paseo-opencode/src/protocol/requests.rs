//! Request bodies.

use super::types::{JsonMap, PermissionRule};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `{providerID, modelID}` as accepted by prompt and command calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}

impl ModelRef {
    /// Parses `provider/model` (the model id may itself contain `/`); a bare id means provider `opencode`,
    /// as the TS adapter does.
    pub fn parse(model: &str) -> ModelRef {
        match model.split_once('/') {
            Some((provider, rest)) => ModelRef {
                provider_id: provider.into(),
                model_id: rest.into(),
            },
            None => ModelRef {
                provider_id: "opencode".into(),
                model_id: model.into(),
            },
        }
    }

    pub fn to_model_string(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }
}

/// One element of a prompt's `parts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PromptPartInput {
    Text {
        text: String,
    },
    File {
        mime: String,
        filename: String,
        url: String,
    },
}

/// Body of `POST /session/{id}/prompt_async` and `POST /session/{id}/message`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PromptBody {
    #[serde(rename = "messageID", skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    pub parts: Vec<PromptPartInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// `{"type":"json_schema","schema":{..}}` structured output request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Value>,
}

/// Body of `POST /session/{id}/command` (slash command).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct CommandBody {
    pub command: String,
    pub arguments: String,
    #[serde(rename = "messageID", skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Command calls take the combined `provider/model` string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// Body of `POST /session` (all optional).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SessionCreateBody {
    #[serde(rename = "parentID", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<Vec<PermissionRule>>,
}

/// Body of `PATCH /session/{id}`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SessionUpdateBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<Vec<PermissionRule>>,
}

/// `reply` of `POST /permission/{id}/reply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionReply {
    Once,
    Always,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionReplyBody {
    pub reply: PermissionReply,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Body of `POST /question/{id}/reply`: one answer list per question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionReplyBody {
    pub answers: Vec<Vec<String>>,
}

/// Body of `POST /session/{id}/revert`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevertBody {
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(rename = "partID", skip_serializing_if = "Option::is_none")]
    pub part_id: Option<String>,
}

/// Body of `POST /mcp` (`{name, config}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpAddBody {
    pub name: String,
    pub config: JsonMap,
}
