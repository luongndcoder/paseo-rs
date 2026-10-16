//! Messages delivered to the consumer and the input builder.

use serde_json::Value;

use crate::protocol::{
    AssistantMessage, CommandLifecycle, ContentBlock, Line, RateLimitEvent, ResultMessage,
    StreamEvent, SystemMessage, ToolProgress, UserContent, UserMessage,
};

/// A conversation message from the CLI (control traffic is handled internally and never
/// surfaces here).
// Messages are moved once through a channel; boxing would only burden consumers' matches.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum SdkMessage {
    Assistant(AssistantMessage),
    User(UserMessage),
    StreamEvent(StreamEvent),
    Result(ResultMessage),
    System(SystemMessage),
    ToolProgress(ToolProgress),
    RateLimitEvent(RateLimitEvent),
    CommandLifecycle(CommandLifecycle),
    /// Any other message type (`auth_status`, `prompt_suggestion`, `tool_use_summary`, ...),
    /// or a known type whose typed form would lose information. Raw JSON.
    Other(Value),
}

impl SdkMessage {
    /// `None` for control traffic, which the session consumes itself.
    pub(crate) fn from_line(line: Line) -> Option<Self> {
        Some(match line {
            Line::Assistant(m) => Self::Assistant(m),
            Line::User(m) => Self::User(m),
            Line::StreamEvent(m) => Self::StreamEvent(m),
            Line::Result(m) => Self::Result(m),
            Line::System(m) => Self::System(m),
            Line::ToolProgress(m) => Self::ToolProgress(m),
            Line::RateLimitEvent(m) => Self::RateLimitEvent(m),
            Line::CommandLifecycle(m) => Self::CommandLifecycle(m),
            Line::Unknown(v) => Self::Other(v),
            Line::ControlRequest(_)
            | Line::ControlResponse(_)
            | Line::ControlCancelRequest(_)
            | Line::KeepAlive { .. }
            | Line::TranscriptMirror { .. } => return None,
        })
    }

    pub fn is_result(&self) -> bool {
        matches!(self, Self::Result(_))
    }

    pub fn as_result(&self) -> Option<&ResultMessage> {
        match self {
            Self::Result(r) => Some(r),
            _ => None,
        }
    }
}

/// A user turn to send: text and image blocks plus optional metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct UserInput {
    blocks: Vec<ContentBlock>,
    uuid: Option<String>,
    priority: Option<String>,
    should_query: Option<bool>,
}

impl UserInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self::from_blocks(vec![ContentBlock::text(text)])
    }

    pub fn from_blocks(blocks: Vec<ContentBlock>) -> Self {
        Self {
            blocks,
            uuid: None,
            priority: None,
            should_query: None,
        }
    }

    pub fn push_text(mut self, text: impl Into<String>) -> Self {
        self.blocks.push(ContentBlock::text(text));
        self
    }

    /// Base64 image (`media_type` such as `image/png`, `data` without a data-URL prefix).
    pub fn push_image_base64(
        mut self,
        media_type: impl Into<String>,
        data: impl Into<String>,
    ) -> Self {
        self.blocks
            .push(ContentBlock::image_base64(media_type, data));
        self
    }

    /// Client-chosen uuid. [`crate::SessionHandle::send_user_message`] generates one when
    /// unset and returns it (needed later for `rewind_files`).
    pub fn uuid(mut self, uuid: impl Into<String>) -> Self {
        self.uuid = Some(uuid.into());
        self
    }

    /// `now` | `next` | `later`.
    pub fn priority(mut self, p: impl Into<String>) -> Self {
        self.priority = Some(p.into());
        self
    }

    /// `false`: append to the transcript without starting a turn.
    pub fn should_query(mut self, yes: bool) -> Self {
        self.should_query = Some(yes);
        self
    }

    pub(crate) fn into_message(
        mut self,
        default_uuid: impl FnOnce() -> String,
    ) -> (UserMessage, String) {
        let uuid = self.uuid.take().unwrap_or_else(default_uuid);
        let mut m = UserMessage::input(UserContent::Blocks(self.blocks));
        m.uuid = Some(uuid.clone());
        m.priority = self.priority;
        m.should_query = self.should_query;
        (m, uuid)
    }
}

impl From<&str> for UserInput {
    fn from(s: &str) -> Self {
        Self::text(s)
    }
}

impl From<String> for UserInput {
    fn from(s: String) -> Self {
        Self::text(s)
    }
}
