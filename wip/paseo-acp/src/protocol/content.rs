//! Content blocks, tool calls and the `session/update` union.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::lenient::Lenient;

pub type ContentBlock = Lenient<KnownContentBlock>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum KnownContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Value>,
    },
    Image {
        data: String,
        mime_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uri: Option<String>,
    },
    Audio {
        data: String,
        mime_type: String,
    },
    ResourceLink {
        uri: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
    },
    Resource {
        resource: Value,
    },
}

impl KnownContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        KnownContentBlock::Text {
            text: text.into(),
            annotations: None,
        }
    }

    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        KnownContentBlock::Image {
            data: data.into(),
            mime_type: mime_type.into(),
            uri: None,
        }
    }
}

/// Text carried by a block, same rules as `contentBlockToText` in the TS adapter.
pub fn content_block_text(block: &ContentBlock) -> String {
    match block {
        Lenient::Known(KnownContentBlock::Text { text, .. }) => text.clone(),
        Lenient::Known(KnownContentBlock::ResourceLink { title, uri, .. }) => {
            title.clone().unwrap_or_else(|| uri.clone())
        }
        Lenient::Known(KnownContentBlock::Resource { resource }) => match resource.get("text") {
            Some(Value::String(t)) => t.clone(),
            _ => {
                let mime = resource
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("binary");
                format!("[resource:{mime}]")
            }
        },
        Lenient::Known(KnownContentBlock::Image { .. }) => "[image]".into(),
        Lenient::Known(KnownContentBlock::Audio { .. }) => "[audio]".into(),
        Lenient::Unknown(_) => String::new(),
    }
}

/// Fields shared by `tool_call` and `tool_call_update`. Everything but the id is optional
/// because updates carry only the changed fields.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallFields {
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl AvailableCommand {
    pub fn input_hint(&self) -> String {
        self.input
            .as_ref()
            .and_then(|i| i.get("hint"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }
}

pub type SessionUpdate = Lenient<KnownSessionUpdate>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "sessionUpdate",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum KnownSessionUpdate {
    UserMessageChunk {
        content: ContentBlock,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    AgentMessageChunk {
        content: ContentBlock,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    AgentThoughtChunk {
        content: ContentBlock,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
    },
    ToolCall(ToolCallFields),
    ToolCallUpdate(ToolCallFields),
    Plan {
        entries: Vec<Value>,
    },
    AvailableCommandsUpdate {
        available_commands: Vec<AvailableCommand>,
    },
    CurrentModeUpdate {
        current_mode_id: String,
    },
    ConfigOptionUpdate {
        config_options: Vec<super::session::SessionConfigOption>,
    },
    SessionInfoUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        updated_at: Option<String>,
    },
    UsageUpdate {
        used: serde_json::Number,
        size: serde_json::Number,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost: Option<Value>,
    },
}

/// `session/update` notification params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionNotification {
    pub session_id: String,
    pub update: SessionUpdate,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// Unknown-field bag used by response structs so nothing from the agent is dropped.
pub type Extra = Map<String, Value>;
