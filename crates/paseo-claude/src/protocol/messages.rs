//! Conversation messages emitted by the CLI (`assistant`, `user`, `stream_event`, `result`,
//! `system`, ...). Shapes follow `SDKMessage` in sdk.d.ts, checked against CLI 2.1.301 traffic.

use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};

use super::content::{ApiMessage, ContentBlock, MessageParam, UserContent};
use super::lossless::{double_option, opt_ref, Extra};
use super::permission::PermissionMode;

/// `{"type":"assistant"}`. With `--include-partial-messages` each content block also
/// arrives as `stream_event`s before this complete message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub message: ApiMessage,
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_tool_use_id: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// `authentication_failed`, `rate_limit`, `overloaded`, `max_output_tokens`, ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_message_uuid: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl AssistantMessage {
    pub fn parent_tool_use_id(&self) -> Option<&str> {
        opt_ref(&self.parent_tool_use_id).map(String::as_str)
    }
}

/// `{"type":"user"}`: input on stdin, and echo/tool results on stdout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserMessage {
    pub message: MessageParam,
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_tool_use_id: Option<Option<String>>,
    /// The SDK sends `""` for new input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Client-chosen id; `rewind_files.user_message_id` refers to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(
        rename = "isSynthetic",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub is_synthetic: Option<bool>,
    #[serde(rename = "isReplay", default, skip_serializing_if = "Option::is_none")]
    pub is_replay: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_result: Option<Value>,
    /// `now` | `next` | `later`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Value>,
    #[serde(
        rename = "shouldQuery",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub should_query: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl UserMessage {
    /// Input message as the TS SDK writes it: `session_id: ""`, `parent_tool_use_id: null`.
    pub fn input(content: UserContent) -> Self {
        Self {
            message: MessageParam::user(content),
            parent_tool_use_id: Some(None),
            session_id: Some(String::new()),
            uuid: None,
            timestamp: None,
            is_synthetic: None,
            is_replay: None,
            tool_use_result: None,
            priority: None,
            origin: None,
            should_query: None,
            extra: Extra::new(),
        }
    }

    pub fn text(text: impl Into<String>) -> Self {
        Self::input(UserContent::Blocks(vec![ContentBlock::text(text)]))
    }

    pub fn with_uuid(mut self, uuid: impl Into<String>) -> Self {
        self.uuid = Some(uuid.into());
        self
    }
}

/// `{"type":"stream_event"}`: one raw Messages API stream event; see
/// [`StreamEvent::kind`](super::stream).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamEvent {
    pub event: Value,
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_tool_use_id: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_message_uuid: Option<String>,
    /// `msg_...` id of the assistant message this event belongs to (observed, not in d.ts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_message_id: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl StreamEvent {
    pub fn parent_tool_use_id(&self) -> Option<&str> {
        opt_ref(&self.parent_tool_use_id).map(String::as_str)
    }
}

/// `{"type":"result"}`: exactly one per turn. `subtype` is `success`,
/// `error_during_execution`, `error_max_turns`, `error_max_budget_usd`,
/// `error_max_structured_output_retries`. Most fields are optional because crash results
/// may be sparse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultMessage {
    pub subtype: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_api_ms: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_turns: Option<Number>,
    /// Final text (success), or the error text when `is_error`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Error subtypes only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<String>>,
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub stop_reason: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    #[serde(
        rename = "modelUsage",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub model_usage: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_denials: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_tool_use: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub api_error_status: Option<Option<Number>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_turn_count: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_message_uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `system` messages, tagged by `subtype`. Subtypes not listed (`notification`,
/// `informational`, `local_command_output`, `files_persisted`, `memory_recall`,
/// `plugin_install`, `model_refusal_*`, `worker_shutting_down`, ...) are `Unknown(Value)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum SystemMessage {
    /// First message of a session; emitted after the first user message is received (not
    /// right after `initialize`).
    Init(InitMessage),
    Status {
        /// `compacting`, `requesting` or `null`.
        status: Option<String>,
        #[serde(
            rename = "permissionMode",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        permission_mode: Option<PermissionMode>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compact_result: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compact_error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    CompactBoundary {
        compact_metadata: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    ApiRetry {
        attempt: Number,
        max_retries: Number,
        retry_delay_ms: Number,
        #[serde(
            default,
            with = "double_option",
            skip_serializing_if = "Option::is_none"
        )]
        error_status: Option<Option<Number>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    /// Only emitted when the CLI runs with `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1`.
    /// `idle` closes an autonomous turn.
    SessionStateChanged {
        /// `idle` | `running` | `requires_action`.
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    CommandsChanged {
        commands: Vec<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    TaskStarted {
        task_id: String,
        description: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    TaskUpdated {
        task_id: String,
        patch: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    TaskProgress {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_tool_name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    TaskNotification {
        task_id: String,
        /// `completed` | `failed` | `stopped`.
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_file: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    HookStarted {
        hook_id: String,
        hook_name: String,
        hook_event: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    HookResponse {
        hook_id: String,
        hook_name: String,
        hook_event: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outcome: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<Number>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    PermissionDenied {
        tool_name: String,
        tool_use_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    ThinkingTokens {
        estimated_tokens: Number,
        estimated_tokens_delta: Number,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        uuid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(untagged)]
    Unknown(Value),
}

/// `system`/`init`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitMessage {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    pub cwd: String,
    pub model: String,
    #[serde(rename = "permissionMode")]
    pub permission_mode: PermissionMode,
    #[serde(default)]
    pub tools: Vec<String>,
    /// `{name, status, source?}`.
    #[serde(default)]
    pub mcp_servers: Vec<Value>,
    #[serde(default)]
    pub slash_commands: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<Vec<Value>>,
    #[serde(
        rename = "apiKeySource",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub api_key_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_style: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type":"command_lifecycle"}` (observed in CLI 2.1.301; not in sdk.d.ts): queue state of a
/// user message identified by its client `uuid` (`queued` → `started` → `completed`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandLifecycle {
    pub command_uuid: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type":"tool_progress"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolProgress {
    pub tool_use_id: String,
    pub tool_name: String,
    #[serde(
        default,
        with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent_tool_use_id: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_time_seconds: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `{"type":"rate_limit_event"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitEvent {
    pub rate_limit_info: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}
