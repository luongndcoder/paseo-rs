//! Frames the provider emits that are not responses to a request.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use super::types::ResponseFrame;
use super::types::{Bare, Extra, Message, SlashCommand, ThinkingLevel, UiRequest};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Delta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta: Option<String>,
    #[serde(
        default,
        rename = "contentIndex",
        skip_serializing_if = "Option::is_none"
    )]
    pub content_index: Option<u64>,
    #[serde(flatten)]
    pub extra: Extra,
}

tagged_enum! {
    /// The streaming sub-event inside `message_update`.
    pub enum AssistantMessageEvent ["type"] {
        Start(Bare) = "start",
        TextStart(Bare) = "text_start",
        TextDelta(Delta) = "text_delta",
        TextEnd(Bare) = "text_end",
        ThinkingStart(Bare) = "thinking_start",
        ThinkingDelta(Delta) = "thinking_delta",
        ThinkingEnd(Bare) = "thinking_end",
        ToolcallStart(Bare) = "toolcall_start",
        ToolcallDelta(Delta) = "toolcall_delta",
        ToolcallEnd(Bare) = "toolcall_end",
        Done(Bare) = "done",
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThinkingLevelChanged {
    pub level: ThinkingLevel,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `message_start` / `message_end` / `turn_end`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageEvent {
    pub message: Message,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageUpdate {
    /// COMPAT: pi <= 0.83 repeats the cumulative assistant message on every update; newer
    /// versions omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    pub assistant_message_event: AssistantMessageEvent,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionStart {
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub args: Value,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionUpdate {
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    #[serde(default)]
    pub partial_result: Value,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecutionEnd {
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionStart {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEnd {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aborted: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEnd {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<Message>>,
    /// Absent on pi versions that predate `agent_settled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub will_retry: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSettled {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aborted: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoRetryStart {
    pub attempt: u64,
    pub max_attempts: u64,
    pub delay_ms: u64,
    pub error_message: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoRetryEnd {
    pub success: bool,
    pub attempt: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_error: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandOutput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// OMP: the late result of a `prompt` the provider acknowledged earlier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_invoked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl PromptResult {
    pub fn error_message(&self) -> Option<&str> {
        self.error.as_ref()?.get("message")?.as_str()
    }
}

/// OMP `notice`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    pub level: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AvailableCommandsUpdate {
    pub commands: Vec<SlashCommand>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostToolCall {
    pub id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostToolCancel {
    pub id: String,
    pub target_id: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostToolUpdate {
    pub id: String,
    #[serde(default)]
    pub partial_result: Value,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Pi 1.x: messages queued behind the running turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueUpdate {
    #[serde(default)]
    pub steering: Vec<Value>,
    #[serde(default)]
    pub follow_up: Vec<Value>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Synthetic: the reader saw EOF. Never appears on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessExit {
    pub error: String,
}

tagged_enum! {
    /// Everything the provider sends besides the response to a pending request. A response nothing
    /// is waiting for is delivered here too (OMP 18.3 rejects a prompt after acknowledging it).
    pub enum Event ["type"] {
        ThinkingLevelChanged(ThinkingLevelChanged) = "thinking_level_changed",
        AgentStart(Bare) = "agent_start",
        TurnStart(Bare) = "turn_start",
        TurnEnd(MessageEvent) = "turn_end",
        MessageStart(MessageEvent) = "message_start",
        MessageUpdate(MessageUpdate) = "message_update",
        MessageEnd(MessageEvent) = "message_end",
        ToolExecutionStart(ToolExecutionStart) = "tool_execution_start",
        ToolExecutionUpdate(ToolExecutionUpdate) = "tool_execution_update",
        ToolExecutionEnd(ToolExecutionEnd) = "tool_execution_end",
        CompactionStart(CompactionStart) = "compaction_start",
        CompactionEnd(CompactionEnd) = "compaction_end",
        AgentEnd(AgentEnd) = "agent_end",
        AgentSettled(AgentSettled) = "agent_settled",
        AutoRetryStart(AutoRetryStart) = "auto_retry_start",
        AutoRetryEnd(AutoRetryEnd) = "auto_retry_end",
        ExtensionUiRequest(UiRequest) = "extension_ui_request",
        CommandOutput(CommandOutput) = "command_output",
        QueueUpdate(QueueUpdate) = "queue_update",
        Response(ResponseFrame) = "response",
        /// OMP announces readiness (and protocol versions) first.
        Ready(Bare) = "ready",
        // ---- OMP only ----
        PromptResult(PromptResult) = "prompt_result",
        Notice(Notice) = "notice",
        AvailableCommandsUpdate(AvailableCommandsUpdate) = "available_commands_update",
        HostToolCall(HostToolCall) = "host_tool_call",
        HostToolCancel(HostToolCancel) = "host_tool_cancel",
        HostToolUpdate(HostToolUpdate) = "host_tool_update",
        SubagentLifecycle(Bare) = "subagent_lifecycle",
        SubagentProgress(Bare) = "subagent_progress",
        SubagentEvent(Bare) = "subagent_event",
        TodoReminder(Bare) = "todo_reminder",
        GoalUpdated(Bare) = "goal_updated",
        RetryFallbackApplied(Bare) = "retry_fallback_applied",
        RetryFallbackSucceeded(Bare) = "retry_fallback_succeeded",
        AutoCompactionStart(Bare) = "auto_compaction_start",
        AutoCompactionEnd(Bare) = "auto_compaction_end",
        ModelChanged(Bare) = "model_changed",
        /// Synthetic, see [`ProcessExit`].
        ProcessExit(ProcessExit) = "process_exit",
    }
}
