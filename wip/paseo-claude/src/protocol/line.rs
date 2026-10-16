//! One line of stdin or stdout: `Line` is the union of every top-level message type in both
//! directions, tagged by `type`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::control::{ControlCancelRequest, ControlRequest, ControlRequestBody, ControlResponse};
use super::lossless::{parse_lossless, Extra, OpenEnum};
use super::messages::{
    AssistantMessage, CommandLifecycle, RateLimitEvent, ResultMessage, StreamEvent, SystemMessage,
    ToolProgress, UserMessage,
};

/// Framing: UTF-8 JSON objects separated by `\n` (one object per line, no embedded raw
/// newlines). Empty lines are skipped; a stdout line that is not JSON is logged and ignored
/// by the TS SDK.
///
/// Stdin carries `User`, `ControlRequest`, `ControlResponse`, `ControlCancelRequest`.
/// Stdout carries everything else plus the CLI's own `ControlRequest` / `ControlResponse`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Line {
    ControlRequest(ControlRequest),
    ControlResponse(ControlResponse),
    ControlCancelRequest(ControlCancelRequest),
    /// Heartbeat; ignore.
    KeepAlive {
        #[serde(flatten)]
        extra: Extra,
    },
    /// Session-store mirroring (`--session-mirror`); `filePath` + transcript `entries`.
    TranscriptMirror {
        #[serde(rename = "filePath")]
        file_path: String,
        entries: Vec<Value>,
        #[serde(flatten)]
        extra: Extra,
    },
    User(UserMessage),
    Assistant(AssistantMessage),
    StreamEvent(StreamEvent),
    Result(ResultMessage),
    System(SystemMessage),
    ToolProgress(ToolProgress),
    RateLimitEvent(RateLimitEvent),
    CommandLifecycle(CommandLifecycle),
    /// `auth_status`, `prompt_suggestion`, `tool_use_summary`, `active_goal`,
    /// `autocompact_state`, `conversation_reset`, and any future type; also lines with no
    /// `type`, and known types whose typed form would lose information.
    #[serde(untagged)]
    Unknown(Value),
}

impl OpenEnum for Line {
    fn unknown(value: Value) -> Self {
        Self::Unknown(value)
    }
}

impl Line {
    /// Parse one line (without the trailing newline). Errors only on invalid JSON; the result
    /// always reserializes to the same JSON value (see [`parse_lossless`]).
    pub fn parse(line: &str) -> Result<Self, serde_json::Error> {
        Ok(Self::from_value(serde_json::from_str(line)?))
    }

    pub fn from_value(value: Value) -> Self {
        parse_lossless(value)
    }

    /// Compact JSON followed by `\n`, ready to write to stdin.
    pub fn to_json_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("Line serializes");
        s.push('\n');
        s
    }

    pub fn control_request(request_id: impl Into<String>, body: ControlRequestBody) -> Self {
        Self::ControlRequest(ControlRequest::new(request_id, body))
    }

    pub fn control_success(request_id: impl Into<String>, response: Option<Value>) -> Self {
        Self::ControlResponse(ControlResponse::success(request_id, response))
    }

    pub fn control_error(request_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self::ControlResponse(ControlResponse::error(request_id, error))
    }

    pub fn cancel_request(request_id: impl Into<String>) -> Self {
        Self::ControlCancelRequest(ControlCancelRequest {
            request_id: request_id.into(),
            extra: Extra::new(),
        })
    }

    /// `true` for the per-turn terminator.
    pub fn is_result(&self) -> bool {
        matches!(self, Self::Result(_))
    }
}
