//! Control requests and responses (both directions share the same envelope).
//!
//! Host → CLI requests: `initialize`, `interrupt`, `set_*`, `rewind_files`, `mcp_*` ...
//! CLI → host requests: `can_use_tool`, `hook_callback`, `mcp_message`, `elicitation`, ...
//! `request_id` correlates a request with its `control_response`; ids are chosen by the sender
//! and only need to be unique among its in-flight requests.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::collections::BTreeMap;

use super::hooks::{HookInput, HooksRegistration};
use super::lossless::{double_option, Extra};
use super::mcp::{JsonRpcMessage, McpServerConfig};
use super::permission::{PermissionMode, PermissionUpdate};

/// Envelope of `{"type":"control_request","request_id":...,"request":{...}}`
/// (the `type` key lives on [`super::Line`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlRequest {
    pub request_id: String,
    pub request: ControlRequestBody,
}

impl ControlRequest {
    pub fn new(request_id: impl Into<String>, request: ControlRequestBody) -> Self {
        Self {
            request_id: request_id.into(),
            request,
        }
    }
}

/// `{"type":"control_cancel_request","request_id":...}`: abort an in-flight request (either
/// direction; the CLI sends it for `can_use_tool` it no longer needs answered).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlCancelRequest {
    pub request_id: String,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlResponse {
    pub response: ControlResponseBody,
}

impl ControlResponse {
    pub fn success(request_id: impl Into<String>, response: Option<Value>) -> Self {
        Self {
            response: ControlResponseBody::Success(SuccessResponse {
                request_id: request_id.into(),
                response,
                pending_permission_requests: None,
                pending_user_dialog_requests: None,
                extra: Extra::new(),
            }),
        }
    }

    pub fn error(request_id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            response: ControlResponseBody::Error(ErrorResponse {
                request_id: request_id.into(),
                error: error.into(),
                pending_permission_requests: None,
                pending_user_dialog_requests: None,
                extra: Extra::new(),
            }),
        }
    }

    pub fn request_id(&self) -> Option<&str> {
        match &self.response {
            ControlResponseBody::Success(s) => Some(&s.request_id),
            ControlResponseBody::Error(e) => Some(&e.request_id),
            ControlResponseBody::Unknown(v) => v.get("request_id").and_then(Value::as_str),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "subtype")]
pub enum ControlResponseBody {
    #[serde(rename = "success")]
    Success(SuccessResponse),
    #[serde(rename = "error")]
    Error(ErrorResponse),
    #[serde(untagged)]
    Unknown(Value),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuccessResponse {
    pub request_id: String,
    /// Absent for requests with no result (`set_model`). Payload shapes: [`super::responses`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
    /// Redelivered `can_use_tool` requests the CLI still awaits (envelopes of
    /// [`ControlRequest`] with `type`); sent on `initialize` when re-attaching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_permission_requests: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_user_dialog_requests: Option<Vec<Value>>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub request_id: String,
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_permission_requests: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_user_dialog_requests: Option<Vec<Value>>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `agents` entries of `initialize` (custom subagent definitions).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub description: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(
        rename = "disallowedTools",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub disallowed_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Server names (string) or `{name: config}` maps.
    #[serde(
        rename = "mcpServers",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub mcp_servers: Option<Vec<Value>>,
    #[serde(
        rename = "criticalSystemReminder_EXPERIMENTAL",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub critical_system_reminder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(
        rename = "initialPrompt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub initial_prompt: Option<String>,
    #[serde(rename = "maxTurns", default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<Number>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    /// Level string or number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Value>,
    #[serde(
        rename = "permissionMode",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_mode: Option<PermissionMode>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `initialize` request. Keys the SDK computes from its options:
/// `systemPrompt` is `[text]` (a plain string option is wrapped; an unset option is `[""]`),
/// `appendSystemPrompt` carries the preset `append`, `sdkMcpServers` the in-process server
/// names, `hooks` the callback registrations.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct InitializeRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HooksRegistration>,
    #[serde(
        rename = "sdkMcpServers",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub sdk_mcp_servers: Option<Vec<String>>,
    #[serde(
        rename = "jsonSchema",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub json_schema: Option<Value>,
    #[serde(
        rename = "systemPrompt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub system_prompt: Option<Vec<String>>,
    #[serde(
        rename = "appendSystemPrompt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub append_system_prompt: Option<String>,
    #[serde(
        rename = "planModeInstructions",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub plan_mode_instructions: Option<String>,
    #[serde(
        rename = "appendSubagentSystemPrompt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub append_subagent_system_prompt: Option<String>,
    #[serde(
        rename = "toolAliases",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_aliases: Option<BTreeMap<String, String>>,
    #[serde(
        rename = "excludeDynamicSections",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub exclude_dynamic_sections: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<BTreeMap<String, AgentDefinition>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(
        rename = "webSearchIsolationExemptMcpServers",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub web_search_isolation_exempt_mcp_servers: Option<Vec<String>>,
    #[serde(
        rename = "promptSuggestions",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub prompt_suggestions: Option<bool>,
    #[serde(
        rename = "agentProgressSummaries",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub agent_progress_summaries: Option<bool>,
    #[serde(
        rename = "forwardSubagentText",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub forward_subagent_text: Option<bool>,
    #[serde(
        rename = "supportedDialogKinds",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub supported_dialog_kinds: Option<Vec<String>>,
    #[serde(
        rename = "perTaskStopAffordance",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub per_task_stop_affordance: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// CLI → host: ask permission for a tool call. Answer with a
/// [`super::PermissionResult`] as the success `response`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanUseToolRequest {
    pub tool_name: String,
    pub input: Value,
    pub tool_use_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_suggestions: Option<Vec<PermissionUpdate>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier_approvable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppress_always_allow_rule: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_to_no: Option<bool>,
    /// `{source, tool_name, rule_content?}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_ask_rule: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_user_interaction: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// CLI → host: an MCP server asked the user for input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElicitationRequest {
    pub mcp_server_name: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicitation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// The `request` object, tagged by `subtype`. Subtypes not listed here (e.g. `get_usage`
/// variants, `mcp_authenticate`, `claude_oauth_*`) travel as `Unknown(Value)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "subtype", rename_all = "snake_case")]
pub enum ControlRequestBody {
    // ---- host -> CLI ----
    Initialize(InitializeRequest),
    Interrupt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cancel_queued: Option<bool>,
        #[serde(flatten)]
        extra: Extra,
    },
    SetPermissionMode {
        mode: PermissionMode,
        #[serde(flatten)]
        extra: Extra,
    },
    /// `model` omitted or `null` resets to the default model.
    SetModel {
        #[serde(
            default,
            with = "double_option",
            skip_serializing_if = "Option::is_none"
        )]
        model: Option<Option<String>>,
        #[serde(flatten)]
        extra: Extra,
    },
    SetMaxThinkingTokens {
        #[serde(
            default,
            with = "double_option",
            skip_serializing_if = "Option::is_none"
        )]
        max_thinking_tokens: Option<Option<Number>>,
        #[serde(
            default,
            with = "double_option",
            skip_serializing_if = "Option::is_none"
        )]
        thinking_display: Option<Option<String>>,
        #[serde(flatten)]
        extra: Extra,
    },
    /// `user_message_id` is the `uuid` of a user message sent with a client-supplied uuid.
    RewindFiles {
        user_message_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dry_run: Option<bool>,
        #[serde(flatten)]
        extra: Extra,
    },
    McpStatus {},
    McpSetServers {
        servers: BTreeMap<String, McpServerConfig>,
        #[serde(flatten)]
        extra: Extra,
    },
    McpReconnect {
        #[serde(rename = "serverName")]
        server_name: String,
        #[serde(flatten)]
        extra: Extra,
    },
    McpToggle {
        #[serde(rename = "serverName")]
        server_name: String,
        enabled: bool,
        #[serde(flatten)]
        extra: Extra,
    },
    GetContextUsage {},
    GetSettings {},
    GetUsage {},
    GetSessionCost {},
    ListModels {},
    /// Merge settings into the flag layer, e.g. `{"fastMode": true}`.
    ApplyFlagSettings {
        settings: Map<String, Value>,
        #[serde(flatten)]
        extra: Extra,
    },
    StopTask {
        task_id: String,
        #[serde(flatten)]
        extra: Extra,
    },
    BackgroundTasks {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    CancelAsyncMessage {
        message_uuid: String,
        #[serde(flatten)]
        extra: Extra,
    },
    ReadFile {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_bytes: Option<Number>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encoding: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    SeedReadState {
        path: String,
        mtime: Number,
        #[serde(flatten)]
        extra: Extra,
    },
    ReloadPlugins {},
    ReloadSkills {},
    RenameSession {
        title: String,
        #[serde(flatten)]
        extra: Extra,
    },
    SetColor {
        color: String,
        #[serde(flatten)]
        extra: Extra,
    },
    /// Present in sdk.mjs, absent from sdk.d.ts.
    SetCwd {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trust_accepted: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        trusted_directory: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },

    // ---- CLI -> host ----
    CanUseTool(CanUseToolRequest),
    HookCallback {
        callback_id: String,
        input: HookInput,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    /// JSON-RPC message for the in-process MCP server `server_name`. Requests (with `id`) are
    /// answered with `{"mcp_response": <jsonrpc response>}`.
    McpMessage {
        server_name: String,
        message: JsonRpcMessage,
        #[serde(flatten)]
        extra: Extra,
    },
    Elicitation(ElicitationRequest),
    RequestUserDialog {
        dialog_kind: String,
        payload: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        #[serde(flatten)]
        extra: Extra,
    },
    OauthTokenRefresh {
        #[serde(flatten)]
        extra: Extra,
    },
    HostAuthTokenRefresh {
        #[serde(flatten)]
        extra: Extra,
    },

    #[serde(untagged)]
    Unknown(Value),
}

impl ControlRequestBody {
    /// Wire `subtype` string.
    pub fn subtype(&self) -> Option<&str> {
        let name = match self {
            Self::Initialize(_) => "initialize",
            Self::Interrupt { .. } => "interrupt",
            Self::SetPermissionMode { .. } => "set_permission_mode",
            Self::SetModel { .. } => "set_model",
            Self::SetMaxThinkingTokens { .. } => "set_max_thinking_tokens",
            Self::RewindFiles { .. } => "rewind_files",
            Self::McpStatus {} => "mcp_status",
            Self::McpSetServers { .. } => "mcp_set_servers",
            Self::McpReconnect { .. } => "mcp_reconnect",
            Self::McpToggle { .. } => "mcp_toggle",
            Self::GetContextUsage {} => "get_context_usage",
            Self::GetSettings {} => "get_settings",
            Self::GetUsage {} => "get_usage",
            Self::GetSessionCost {} => "get_session_cost",
            Self::ListModels {} => "list_models",
            Self::ApplyFlagSettings { .. } => "apply_flag_settings",
            Self::StopTask { .. } => "stop_task",
            Self::BackgroundTasks { .. } => "background_tasks",
            Self::CancelAsyncMessage { .. } => "cancel_async_message",
            Self::ReadFile { .. } => "read_file",
            Self::SeedReadState { .. } => "seed_read_state",
            Self::ReloadPlugins {} => "reload_plugins",
            Self::ReloadSkills {} => "reload_skills",
            Self::RenameSession { .. } => "rename_session",
            Self::SetColor { .. } => "set_color",
            Self::SetCwd { .. } => "set_cwd",
            Self::CanUseTool(_) => "can_use_tool",
            Self::HookCallback { .. } => "hook_callback",
            Self::McpMessage { .. } => "mcp_message",
            Self::Elicitation(_) => "elicitation",
            Self::RequestUserDialog { .. } => "request_user_dialog",
            Self::OauthTokenRefresh { .. } => "oauth_token_refresh",
            Self::HostAuthTokenRefresh { .. } => "host_auth_token_refresh",
            Self::Unknown(v) => return v.get("subtype").and_then(Value::as_str),
        };
        Some(name)
    }
}
