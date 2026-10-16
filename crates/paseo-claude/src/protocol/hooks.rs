//! Hook registration (`initialize.hooks`), `hook_callback` input and hook output.

use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};
use std::collections::BTreeMap;

use super::lossless::{double_option, Extra};
use super::permission::{string_enum, PermissionUpdate};

string_enum!(HookEvent {
    PreToolUse = "PreToolUse",
    PostToolUse = "PostToolUse",
    PostToolUseFailure = "PostToolUseFailure",
    PostToolBatch = "PostToolBatch",
    Notification = "Notification",
    UserPromptSubmit = "UserPromptSubmit",
    UserPromptExpansion = "UserPromptExpansion",
    SessionStart = "SessionStart",
    SessionEnd = "SessionEnd",
    Stop = "Stop",
    StopFailure = "StopFailure",
    SubagentStart = "SubagentStart",
    SubagentStop = "SubagentStop",
    PreCompact = "PreCompact",
    PostCompact = "PostCompact",
    PermissionRequest = "PermissionRequest",
    PermissionDenied = "PermissionDenied",
    Setup = "Setup",
    TeammateIdle = "TeammateIdle",
    TaskCreated = "TaskCreated",
    TaskCompleted = "TaskCompleted",
    Elicitation = "Elicitation",
    ElicitationResult = "ElicitationResult",
    ConfigChange = "ConfigChange",
    WorktreeCreate = "WorktreeCreate",
    WorktreeRemove = "WorktreeRemove",
    InstructionsLoaded = "InstructionsLoaded",
    CwdChanged = "CwdChanged",
    FileChanged = "FileChanged",
    DirectoryAdded = "DirectoryAdded",
    MessageDisplay = "MessageDisplay",
});

/// One matcher in `initialize.hooks[event]`. The CLI echoes `callback_id` back in
/// `hook_callback` requests; ids are chosen by the host (`hook_0`, `hook_1`, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookCallbackMatcher {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,
    #[serde(rename = "hookCallbackIds")]
    pub hook_callback_ids: Vec<String>,
    /// Seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<Number>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `initialize.hooks`, keyed by [`HookEvent::as_str`].
pub type HooksRegistration = BTreeMap<String, Vec<HookCallbackMatcher>>;

/// Fields shared by every hook input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookInputBase {
    pub session_id: String,
    pub transcript_path: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// `{"level": "..."}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Value>,
}

/// `hook_callback.input`, tagged by `hook_event_name`. Events not listed here arrive as
/// `Unknown(Value)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "hook_event_name")]
pub enum HookInput {
    PreToolUse {
        #[serde(flatten)]
        base: HookInputBase,
        tool_name: String,
        tool_input: Value,
        tool_use_id: String,
        #[serde(flatten)]
        extra: Extra,
    },
    PostToolUse {
        #[serde(flatten)]
        base: HookInputBase,
        tool_name: String,
        tool_input: Value,
        tool_response: Value,
        tool_use_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<Number>,
        #[serde(flatten)]
        extra: Extra,
    },
    PostToolUseFailure {
        #[serde(flatten)]
        base: HookInputBase,
        tool_name: String,
        tool_input: Value,
        tool_use_id: String,
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_interrupt: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<Number>,
        #[serde(flatten)]
        extra: Extra,
    },
    Notification {
        #[serde(flatten)]
        base: HookInputBase,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        notification_type: String,
        #[serde(flatten)]
        extra: Extra,
    },
    UserPromptSubmit {
        #[serde(flatten)]
        base: HookInputBase,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_title: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    SessionStart {
        #[serde(flatten)]
        base: HookInputBase,
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_title: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    SessionEnd {
        #[serde(flatten)]
        base: HookInputBase,
        reason: String,
        #[serde(flatten)]
        extra: Extra,
    },
    Stop {
        #[serde(flatten)]
        base: HookInputBase,
        stop_hook_active: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_assistant_message: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    SubagentStart {
        #[serde(flatten)]
        base: HookInputBase,
        #[serde(flatten)]
        extra: Extra,
    },
    SubagentStop {
        #[serde(flatten)]
        base: HookInputBase,
        stop_hook_active: bool,
        agent_transcript_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_assistant_message: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    PreCompact {
        #[serde(flatten)]
        base: HookInputBase,
        trigger: String,
        #[serde(
            default,
            with = "double_option",
            skip_serializing_if = "Option::is_none"
        )]
        custom_instructions: Option<Option<String>>,
        #[serde(flatten)]
        extra: Extra,
    },
    PermissionRequest {
        #[serde(flatten)]
        base: HookInputBase,
        tool_name: String,
        tool_input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission_suggestions: Option<Vec<PermissionUpdate>>,
        #[serde(flatten)]
        extra: Extra,
    },
    #[serde(untagged)]
    Unknown(Value),
}

impl HookInput {
    pub fn base(&self) -> Option<&HookInputBase> {
        match self {
            Self::PreToolUse { base, .. }
            | Self::PostToolUse { base, .. }
            | Self::PostToolUseFailure { base, .. }
            | Self::Notification { base, .. }
            | Self::UserPromptSubmit { base, .. }
            | Self::SessionStart { base, .. }
            | Self::SessionEnd { base, .. }
            | Self::Stop { base, .. }
            | Self::SubagentStart { base, .. }
            | Self::SubagentStop { base, .. }
            | Self::PreCompact { base, .. }
            | Self::PermissionRequest { base, .. } => Some(base),
            Self::Unknown(_) => None,
        }
    }
}

/// Hook callback return value (`SyncHookJSONOutput` / `AsyncHookJSONOutput`). `{}` is a valid
/// no-op answer; the TS provider returns `{}` from its observation hooks.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HookJsonOutput {
    #[serde(rename = "continue", default, skip_serializing_if = "Option::is_none")]
    pub continue_: Option<bool>,
    #[serde(
        rename = "suppressOutput",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub suppress_output: Option<bool>,
    #[serde(
        rename = "stopReason",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub stop_reason: Option<String>,
    /// `"approve"` or `"block"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(
        rename = "systemMessage",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub system_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Event-specific object with a `hookEventName` key (`PreToolUse`: `permissionDecision`,
    /// `updatedInput`, ...).
    #[serde(
        rename = "hookSpecificOutput",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub hook_specific_output: Option<Value>,
    /// Async variant: `{"async": true, "asyncTimeout": n}`.
    #[serde(rename = "async", default, skip_serializing_if = "Option::is_none")]
    pub async_: Option<bool>,
    #[serde(
        rename = "asyncTimeout",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub async_timeout: Option<Number>,
    #[serde(flatten)]
    pub extra: Extra,
}
