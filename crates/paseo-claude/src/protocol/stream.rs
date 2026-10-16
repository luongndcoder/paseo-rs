//! Typed view over `stream_event.event` (Anthropic streaming Messages API events).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::lossless::Extra;
use super::messages::StreamEvent;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum StreamEventKind {
    #[serde(rename = "message_start")]
    MessageStart {
        message: Value,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: u32,
        content_block: Value,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta {
        index: u32,
        delta: Delta,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop {
        index: u32,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Value>,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(rename = "message_stop")]
    MessageStop {
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(rename = "ping")]
    Ping {
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(untagged)]
    Unknown(Value),
}

/// `content_block_delta.delta`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Delta {
    #[serde(rename = "text_delta")]
    Text {
        text: String,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(rename = "thinking_delta")]
    Thinking {
        thinking: String,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(rename = "input_json_delta")]
    InputJson {
        partial_json: String,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(rename = "signature_delta")]
    Signature {
        signature: String,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(untagged)]
    Unknown(Value),
}

impl StreamEvent {
    /// Decode `event`; never fails (`Unknown` on anything unexpected).
    pub fn kind(&self) -> StreamEventKind {
        serde_json::from_value(self.event.clone())
            .unwrap_or_else(|_| StreamEventKind::Unknown(self.event.clone()))
    }
}
