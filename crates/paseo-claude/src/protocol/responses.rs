//! Payloads of successful `control_response`s (`SuccessResponse.response`). Parse on demand
//! with [`parse_response`].

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value};

use super::control::SuccessResponse;
use super::lossless::Extra;
use super::permission::PermissionMode;

/// Decode `success.response` as `T`. `None` when absent or when it does not fit `T`.
pub fn parse_response<T: DeserializeOwned>(success: &SuccessResponse) -> Option<T> {
    serde_json::from_value(success.response.clone()?).ok()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlashCommand {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(
        rename = "argumentHint",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub argument_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub value: String,
    #[serde(
        rename = "resolvedModel",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub resolved_model: Option<String>,
    #[serde(
        rename = "displayName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(
        rename = "supportsEffort",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_effort: Option<bool>,
    #[serde(
        rename = "supportedEffortLevels",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub supported_effort_levels: Option<Vec<String>>,
    #[serde(
        rename = "supportsAdaptiveThinking",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_adaptive_thinking: Option<bool>,
    #[serde(
        rename = "supportsFastMode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_fast_mode: Option<bool>,
    #[serde(
        rename = "supportsAutoMode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub supports_auto_mode: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `initialize` response. The SDK exposes `commands` via `supportedCommands()`, `models` via
/// `supportedModels()`, `agents` via `supportedAgents()`, `account` via `accountInfo()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InitializeResponse {
    #[serde(default)]
    pub commands: Vec<SlashCommand>,
    #[serde(default)]
    pub agents: Vec<AgentInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_style: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_output_styles: Option<Vec<String>>,
    #[serde(default)]
    pub models: Vec<ModelInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_permission_mode: Option<PermissionMode>,
    /// `false` when a repeated initialize's hooks were ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks_applied: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `interrupt` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterruptResponse {
    #[serde(default)]
    pub still_queued: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `set_permission_mode` response (`{"mode": "..."}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetPermissionModeResponse {
    pub mode: PermissionMode,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `rewind_files` response. `canRewind: false` carries `error` (e.g. "No file checkpoint
/// found for this message.").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RewindFilesResult {
    #[serde(rename = "canRewind")]
    pub can_rewind: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(
        rename = "filesChanged",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub files_changed: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insertions: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletions: Option<Number>,
    #[serde(
        rename = "skippedLinks",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub skipped_links: Option<Number>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `read_file` response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadFileResponse {
    pub contents: String,
    #[serde(rename = "absPath")]
    pub abs_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `background_tasks` response: `backgrounded` defaults to `true` when absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackgroundTasksResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backgrounded: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}
