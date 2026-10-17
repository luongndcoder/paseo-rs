//! Codex mapper: `codex app-server` JSON-RPC traffic -> `AgentStreamEvent`s (spec WP10).
//!
//! Port of the notification and approval half of `CodexAppServerAgentSession`
//! (`codex-app-server-agent.ts`) plus `codex/tool-call-mapper.ts`.
//!
//! Input is the wire transcript the `paseo-codex` fixtures record: one value per line, either
//! `{"dir":"in"|"out","line":<JSON-RPC message>}` (`in` = client to server) or a meta line
//! (`{"argv":..}` starts a new app-server process and therefore a new session instance,
//! `{"exit":..}` is ignored). The mapper reads both directions because the TS adapter's state
//! also moves on what it sends (`turn/start` assigns the `codex-turn-N` id, an approval answer
//! resolves a permission).
//!
//! Turn ids: the adapter tags every emitted event with its foreground turn id, assigned right
//! before `turn/start` is sent, after the thread was created or resumed. Notifications that the
//! server sent earlier are therefore tagged too (the replay driver delivers them after
//! `turn/start`). The mapper reproduces that by holding notifications from the start of a turn
//! setup (`thread/start`, `thread/resume`, `thread/loaded/list`) until `turn/start`.
//!
//! DEVIATION(codex-turn-id-on-untyped-events): TS tags every event with the foreground turn id,
//! including `thread_started` and `provider_subagent`. The Rust `AgentStreamEvent` has no
//! `turn_id` on those variants, so they carry none. The manager's `attach_turn_identity` does
//! not use it for them and the wire serializer drops it for `thread_started`.
//!
//! DEVIATION(codex-deny-message-from-options): the deny message of a permission answer is client
//! side state that never reaches the wire (only `decision: "decline"` does). Use
//! [`CodexMapper::with_deny_message`] to supply it; without one the resolution has no message.

mod activity;
pub mod items;
mod notification;
pub mod questions;
pub mod tool_call;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use paseo_agent_types::{
    ActionIntent, ActionVariant, AgentStreamEvent, AgentTimelineItem, AgentUsage, Map,
    PermissionAction, PermissionBehavior, PermissionKind, PermissionRequest, PermissionResponse,
    ProviderSubagentInputEvent, ProviderSubagentStatus, ToolCallDetail, ToolCallIcon, ToolCallItem,
    ToolCallStatus,
};
use paseo_tool_detail::js::js_trim;
use paseo_tool_detail::shell_output::extract_codex_terminal_session_id;
use serde_json::{json, Value};

use crate::{MapEnv, ProviderMapper};
use activity::curate_labeled_activity;
use items::{
    item_type, map_codex_plan_update_to_todo, mcp_tool_result_images_to_timeline,
    normalize_plan_markdown, plan_steps_to_markdown, project_thread_history,
    read_sub_agent_activity, settle_historical_sub_agent_activity, thread_item_to_timeline,
    ImageMaterializer, PersistedSubAgentRoute, PersistedTimelineEntry, SubAgentKind,
    ThreadItemOptions, CONTEXT_COMPACTION_TYPE,
};
use notification::{parse_notification, Parsed, Source};
use questions::{
    format_question_prompts, map_question_request_to_tool_call, normalize_question_prompts,
    AsyncQuestions,
};
use tool_call::{
    map_codex_exec_notification_to_tool_call, map_codex_patch_notification_to_tool_call,
    map_codex_terminal_interaction_to_tool_call, map_codex_tool_call_from_thread_item,
    normalize_command_execution_command, ExecParams, PatchParams,
};

const PROVIDER: &str = "codex";
const ASSISTANT_MESSAGE_BOUNDARY_MARKDOWN: &str = "\n\n---\n\n";
const MAX_PENDING_SUB_AGENT_THREADS: usize = 32;
const MAX_PENDING_SUB_AGENT_NOTIFICATIONS_PER_THREAD: usize = 128;

/// Construction options (everything the TS session reads from its config).
#[derive(Clone, Default)]
pub struct CodexOptions {
    /// `config.cwd`. When `None` the mapper uses the `cwd` of the last `thread/start` or
    /// `turn/start` request.
    pub cwd: Option<String>,
    /// Message of the `deny` resolution built from a `decline` answer.
    pub deny_message: Option<String>,
    /// Writes `data` images to a file (see [`ImageMaterializer`]).
    pub materialize: Option<Arc<ImageMaterializer>>,
}

#[derive(Default)]
pub struct CodexMapper {
    opts: CodexOptions,
    st: State,
}

impl std::fmt::Debug for CodexMapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexMapper")
            .field("thread", &self.st.current_thread_id)
            .finish()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Command,
    File,
    Question,
    McpElicitation,
    Plan,
}

struct PendingPermission {
    request: PermissionRequest,
    kind: PendingKind,
    /// JSON-RPC id of the server request this answers.
    server_id: Option<String>,
    questions: Vec<Value>,
    plan: Option<(String, Option<String>)>,
}

struct SubAgentCall {
    call_id: String,
    tool_call: ToolCallItem,
    parent_call_id: Option<String>,
    parent_subagent_id: Option<String>,
    activity_item_ids: HashSet<String>,
    pending_command_output_deltas: HashMap<String, Vec<String>>,
    pending_file_change_output_deltas: HashMap<String, Vec<String>>,
    child_item_order: Vec<String>,
    child_items: HashMap<String, AgentTimelineItem>,
    child_thread_ids: Vec<String>,
}

struct LatestPlan {
    text: String,
}

#[derive(Default)]
struct State {
    cwd: Option<String>,
    current_thread_id: Option<String>,
    next_turn_ordinal: u64,
    active_foreground_turn_id: Option<String>,
    latest_usage: Option<AgentUsage>,
    plan_mode: bool,
    latest_plan_result: Option<LatestPlan>,

    // turn setup buffering
    pre_turn: bool,
    setup_started: bool,
    buffered: Vec<(String, Value)>,
    /// client request id -> (method, params.threadId)
    client_requests: HashMap<String, (String, Option<String>)>,

    // per-turn tracking (`resetTurnTrackingState`)
    emitted_item_started_ids: HashSet<String>,
    emitted_item_completed_ids: HashSet<String>,
    emitted_provider_subagent_user_message_keys: HashSet<String>,
    emitted_exec_started_call_ids: HashSet<String>,
    emitted_exec_completed_call_ids: HashSet<String>,
    pending_agent_messages: HashMap<String, String>,
    pending_reasoning: HashMap<String, Vec<String>>,
    pending_command_output_deltas: HashMap<String, Vec<String>>,
    pending_file_change_output_deltas: HashMap<String, Vec<String>>,
    pending_assistant_message_boundary: bool,
    pending_root_compaction_item_ids: Vec<String>,
    pending_anonymous_root_compactions: usize,
    unpaired_compaction_notification_completions: usize,
    unpaired_compaction_item_completions: usize,

    compaction_trigger_by_item_id: HashMap<String, &'static str>,

    // terminals
    terminal_command_by_process_id: HashMap<String, String>,
    pending_unlabeled_terminal_interactions: HashMap<String, Vec<(String, Option<String>)>>,
    emitted_terminal_interaction_keys: HashSet<String>,
    next_terminal_interaction_ordinal: u64,

    // permissions
    pending: Vec<PendingPermission>,
    mcp_elicitation_permission_ids: HashMap<String, String>,
    async_questions: AsyncQuestions,

    // user message index
    user_message_turn_indexes: HashMap<String, usize>,
    user_message_turn_ids: Vec<String>,
    user_message_provider_turn_ids: HashMap<String, String>,

    // sub-agents
    sub_agent_calls: HashMap<String, SubAgentCall>,
    sub_agent_call_id_by_child_thread_id: HashMap<String, String>,
    pending_sub_agent_notifications: Vec<(String, Vec<Parsed>)>,

    // history
    history_pending: bool,
    persisted_history: Vec<PersistedTimelineEntry>,
    persisted_provider_subagent_events: Vec<AgentStreamEvent>,
    loading_persisted_history: bool,
    history_child_threads: HashSet<String>,
}

fn is_terminal(status: ToolCallStatus) -> bool {
    matches!(
        status,
        ToolCallStatus::Completed | ToolCallStatus::Failed | ToolCallStatus::Canceled
    )
}

fn error_or(v: &Option<Value>, default: Value) -> Value {
    match v {
        Some(x) if !x.is_null() => x.clone(),
        _ => default,
    }
}

/// `decodeCodexOutputDeltaChunk`.
fn decode_output_delta_chunk(chunk: &str) -> String {
    use base64::Engine;
    let trimmed = js_trim(chunk);
    if trimmed.is_empty() {
        return chunk.to_string();
    }
    let b64 = trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='));
    if !b64 || !trimmed.len().is_multiple_of(4) {
        return chunk.to_string();
    }
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(trimmed) else {
        return chunk.to_string();
    };
    let decoded = String::from_utf8_lossy(&bytes).into_owned();
    if decoded.is_empty() {
        return chunk.to_string();
    }
    let round = base64::engine::general_purpose::STANDARD.encode(decoded.as_bytes());
    if round.trim_end_matches('=') == trimmed.trim_end_matches('=') {
        decoded
    } else {
        chunk.to_string()
    }
}

fn append_output_delta(
    store: &mut HashMap<String, Vec<String>>,
    id: Option<&str>,
    chunk: Option<&str>,
    decode_base64: bool,
) {
    let (Some(id), Some(chunk)) = (
        id.filter(|s| !s.is_empty()),
        chunk.filter(|s| !s.is_empty()),
    ) else {
        return;
    };
    let normalized = if decode_base64 {
        decode_output_delta_chunk(chunk)
    } else {
        chunk.to_string()
    };
    if normalized.is_empty() {
        return;
    }
    store.entry(id.to_string()).or_default().push(normalized);
}

fn consume_output_delta(
    store: &mut HashMap<String, Vec<String>>,
    id: Option<&str>,
) -> Option<String> {
    let id = id.filter(|s| !s.is_empty())?;
    let buffered = store.remove(id)?;
    if buffered.is_empty() {
        return None;
    }
    Some(buffered.join(""))
}

/// `toAgentUsage`.
pub fn to_agent_usage(token_usage: &Value) -> Option<AgentUsage> {
    let usage = token_usage.as_object()?;
    let last = usage.get("last").and_then(Value::as_object);
    let positive = |a: Option<&Value>, b: Option<&Value>| -> Option<serde_json::Number> {
        for v in [a, b].into_iter().flatten() {
            if let Value::Number(n) = v {
                if n.as_f64().is_some_and(|f| f.is_finite() && f > 0.0) {
                    return Some(n.clone());
                }
            }
        }
        None
    };
    let num = |k: &str| -> Option<serde_json::Number> {
        match last.and_then(|l| l.get(k)) {
            Some(Value::Number(n)) => Some(n.clone()),
            _ => None,
        }
    };
    Some(AgentUsage {
        input_tokens: num("inputTokens"),
        cached_input_tokens: num("cachedInputTokens"),
        output_tokens: num("outputTokens"),
        total_cost_usd: None,
        context_window_max_tokens: positive(
            usage.get("model_context_window"),
            usage.get("modelContextWindow"),
        ),
        context_window_used_tokens: positive(
            last.and_then(|l| l.get("total_tokens")),
            last.and_then(|l| l.get("totalTokens")),
        ),
    })
}

fn plan_permission_actions() -> Vec<PermissionAction> {
    vec![
        PermissionAction {
            id: "dismiss".into(),
            label: "Dismiss".into(),
            behavior: PermissionBehavior::Deny,
            variant: Some(ActionVariant::Danger),
            intent: Some(ActionIntent::Dismiss),
        },
        PermissionAction {
            id: "implement".into(),
            label: "Implement".into(),
            behavior: PermissionBehavior::Allow,
            variant: Some(ActionVariant::Primary),
            intent: Some(ActionIntent::Implement),
        },
    ]
}

fn json_id_key(v: &Value) -> String {
    match v {
        Value::String(s) => format!("s:{s}"),
        other => format!("n:{other}"),
    }
}

fn allow() -> PermissionResponse {
    PermissionResponse::Allow {
        selected_action_id: None,
        updated_input: None,
        updated_permissions: None,
    }
}

impl CodexMapper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_options(opts: CodexOptions) -> Self {
        CodexMapper {
            opts,
            st: State::default(),
        }
    }

    pub fn with_deny_message(mut self, message: impl Into<String>) -> Self {
        self.opts.deny_message = Some(message.into());
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.opts.cwd = Some(cwd.into());
        self
    }

    /// `streamHistory`: provider sub-agent events first, then the persisted timeline. Drains.
    pub fn stream_history(&mut self) -> Vec<AgentStreamEvent> {
        let st = &mut self.st;
        if (!st.history_pending || st.persisted_history.is_empty())
            && st.persisted_provider_subagent_events.is_empty()
        {
            return Vec::new();
        }
        let history = std::mem::take(&mut st.persisted_history);
        let mut out = std::mem::take(&mut st.persisted_provider_subagent_events);
        st.history_pending = false;
        for entry in history {
            out.push(AgentStreamEvent::Timeline {
                item: Box::new(entry.item),
                provider: PROVIDER.into(),
                turn_id: None,
                timestamp: entry.timestamp,
            });
        }
        out
    }

    fn cwd(&self) -> Option<String> {
        self.opts.cwd.clone().or_else(|| self.st.cwd.clone())
    }

    // -- emit -------------------------------------------------------------------------------

    /// `notifySubscribers`: tag with the foreground turn id unless the event has its own.
    fn emit(&mut self, out: &mut Vec<AgentStreamEvent>, mut ev: AgentStreamEvent) {
        let fg = self.st.active_foreground_turn_id.clone();
        match &mut ev {
            AgentStreamEvent::TurnStarted { turn_id, .. }
            | AgentStreamEvent::TurnCompleted { turn_id, .. }
            | AgentStreamEvent::UsageUpdated { turn_id, .. }
            | AgentStreamEvent::TurnFailed { turn_id, .. }
            | AgentStreamEvent::TurnCanceled { turn_id, .. }
            | AgentStreamEvent::Timeline { turn_id, .. }
            | AgentStreamEvent::PermissionRequested { turn_id, .. }
            | AgentStreamEvent::PermissionResolved { turn_id, .. }
                if turn_id.is_none() =>
            {
                *turn_id = fg;
            }
            _ => {}
        }
        out.push(ev);
    }

    /// `emitEvent`: provider sub-agent events are held while persisted history loads.
    fn emit_event(&mut self, out: &mut Vec<AgentStreamEvent>, ev: AgentStreamEvent) {
        if self.st.loading_persisted_history
            && matches!(ev, AgentStreamEvent::ProviderSubagent { .. })
        {
            self.st.persisted_provider_subagent_events.push(ev);
            return;
        }
        self.emit(out, ev);
    }

    fn timeline(&mut self, out: &mut Vec<AgentStreamEvent>, item: AgentTimelineItem) {
        self.timeline_with_turn(out, item, None);
    }

    fn timeline_with_turn(
        &mut self,
        out: &mut Vec<AgentStreamEvent>,
        item: AgentTimelineItem,
        turn_id: Option<String>,
    ) {
        self.emit_event(
            out,
            AgentStreamEvent::Timeline {
                item: Box::new(item),
                provider: PROVIDER.into(),
                turn_id,
                timestamp: None,
            },
        );
    }

    fn tool_timeline(&mut self, out: &mut Vec<AgentStreamEvent>, item: ToolCallItem) {
        self.timeline(out, AgentTimelineItem::tool_call(item));
    }

    // -- wire dispatch -----------------------------------------------------------------------

    fn feed_line(&mut self, raw: &Value, env: &mut dyn MapEnv, out: &mut Vec<AgentStreamEvent>) {
        let Some(obj) = raw.as_object() else { return };
        if obj.contains_key("argv") && !obj.contains_key("dir") {
            self.st = State::default();
            return;
        }
        let (Some(dir), Some(line)) = (obj.get("dir").and_then(Value::as_str), obj.get("line"))
        else {
            return;
        };
        match dir {
            "in" => self.on_client_message(line, env, out),
            "out" => self.on_server_message(line, env, out),
            _ => {}
        }
    }

    /// Begin of `startTurn` before the turn id exists: dismisses plan approvals, then holds
    /// server notifications until `turn/start` goes out.
    fn begin_turn_setup(&mut self, out: &mut Vec<AgentStreamEvent>) {
        if self.st.setup_started {
            return;
        }
        self.st.setup_started = true;
        self.dismiss_pending_plan_approvals("Dismissed by a new prompt", out);
        self.st.pre_turn = true;
    }

    fn on_client_message(
        &mut self,
        line: &Value,
        env: &mut dyn MapEnv,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Some(m) = line.as_object() else { return };
        let params = m.get("params").cloned().unwrap_or(Value::Null);
        let Some(method) = m.get("method").and_then(Value::as_str) else {
            // client answer to a server request
            if let Some(id) = m.get("id") {
                self.on_client_response(id, m, out);
            }
            return;
        };
        let thread_param = params
            .get("threadId")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(id) = m.get("id") {
            self.st
                .client_requests
                .insert(json_id_key(id), (method.to_string(), thread_param.clone()));
        }
        match method {
            "thread/start" => {
                if let Some(c) = params.get("cwd").and_then(Value::as_str) {
                    self.st.cwd = Some(c.to_string());
                }
                self.begin_turn_setup(out);
            }
            "thread/loaded/list" => self.begin_turn_setup(out),
            "thread/resume" => {
                if let Some(t) = thread_param {
                    self.st.current_thread_id = Some(t);
                    self.st.history_pending = true;
                }
                self.begin_turn_setup(out);
            }
            "turn/start" => self.on_turn_start(&params, env, out),
            _ => {}
        }
    }

    fn on_turn_start(
        &mut self,
        params: &Value,
        env: &mut dyn MapEnv,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        self.begin_turn_setup(out);
        if let Some(c) = params.get("cwd").and_then(Value::as_str) {
            self.st.cwd = Some(c.to_string());
        }
        self.st.plan_mode = params
            .get("collaborationMode")
            .and_then(|c| c.get("mode"))
            .and_then(Value::as_str)
            == Some("plan");
        let id = format!("codex-turn-{}", self.st.next_turn_ordinal);
        self.st.next_turn_ordinal += 1;
        self.st.active_foreground_turn_id = Some(id);
        self.st.pre_turn = false;
        self.st.setup_started = false;
        let buffered = std::mem::take(&mut self.st.buffered);
        for (method, params) in buffered {
            self.handle_notification(&method, &params, env, out);
        }
    }

    fn on_server_message(
        &mut self,
        line: &Value,
        env: &mut dyn MapEnv,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Some(m) = line.as_object() else { return };
        let params = m.get("params").cloned().unwrap_or(Value::Null);
        match (m.get("method").and_then(Value::as_str), m.get("id")) {
            (Some(method), Some(id)) => self.on_server_request(method, id, &params, env, out),
            (Some(method), None) => {
                if self.st.pre_turn {
                    self.st.buffered.push((method.to_string(), params));
                } else {
                    self.handle_notification(method, &params, env, out);
                }
            }
            (None, Some(id)) => {
                let key = json_id_key(id);
                let Some((method, thread)) = self.st.client_requests.remove(&key) else {
                    return;
                };
                let result = m.get("result").cloned().unwrap_or(Value::Null);
                match method.as_str() {
                    "thread/start" => {
                        if let Some(t) = result
                            .get("thread")
                            .and_then(|t| t.get("id"))
                            .and_then(Value::as_str)
                        {
                            self.st.current_thread_id = Some(t.to_string());
                        }
                    }
                    "thread/read" if m.contains_key("result") => {
                        self.on_thread_read_response(thread.as_deref(), &result, out);
                    }
                    _ => {}
                }
            }
            (None, None) => {}
        }
    }

    // -- server requests (approvals) ---------------------------------------------------------

    fn on_server_request(
        &mut self,
        method: &str,
        id: &Value,
        params: &Value,
        env: &mut dyn MapEnv,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        match method {
            "item/commandExecution/requestApproval" => {
                self.handle_command_approval(id, params, out)
            }
            "item/fileChange/requestApproval" => self.handle_file_change_approval(id, params, out),
            "item/tool/requestUserInput" | "tool/requestUserInput" => {
                self.handle_tool_approval(id, params, out)
            }
            "mcpServer/elicitation/request" => self.handle_mcp_elicitation(id, params, env, out),
            _ => {}
        }
    }

    fn handle_command_approval(
        &mut self,
        id: &Value,
        params: &Value,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Some(p) = params.as_object() else { return };
        let (Some(item_id), Some(thread_id), Some(turn_id)) = (
            p.get("itemId").and_then(Value::as_str),
            p.get("threadId").and_then(Value::as_str),
            p.get("turnId").and_then(Value::as_str),
        ) else {
            return;
        };
        let approval_id = p.get("approvalId").and_then(Value::as_str);
        let command = p.get("command").and_then(Value::as_str);
        let cwd = p.get("cwd").and_then(Value::as_str);
        let reason = p.get("reason").and_then(Value::as_str);
        let command_value = command.map(|c| Value::String(c.to_string()));
        let config_cwd = self.cwd();
        let preview = map_codex_exec_notification_to_tool_call(&ExecParams {
            call_id: Some(item_id),
            command: command_value.as_ref(),
            cwd: cwd.or(config_cwd.as_deref()),
            output: None,
            exit_code: None,
            success: None,
            stderr: None,
            running: true,
        });
        let request_id = format!("permission-{}", approval_id.unwrap_or(item_id));
        let title = match command {
            Some(c) if !c.is_empty() => format!("Run command: {c}"),
            _ => "Run command".to_string(),
        };
        let mut input = Map::new();
        if let Some(c) = command {
            input.insert("command".into(), Value::String(c.to_string()));
        }
        if let Some(c) = cwd {
            input.insert("cwd".into(), Value::String(c.to_string()));
        }
        let detail = preview
            .map(|t| t.detail)
            .unwrap_or_else(|| ToolCallDetail::Unknown {
                input: json!({
                    "command": command.map(Value::from).unwrap_or(Value::Null),
                    "cwd": cwd.map(Value::from).unwrap_or(Value::Null),
                }),
                output: Value::Null,
            });
        let mut metadata = Map::new();
        metadata.insert("itemId".into(), item_id.into());
        metadata.insert("threadId".into(), thread_id.into());
        metadata.insert("turnId".into(), turn_id.into());
        let request = PermissionRequest {
            id: request_id,
            provider: PROVIDER.into(),
            name: "CodexBash".into(),
            kind: PermissionKind::Tool,
            title: Some(title),
            description: reason.map(str::to_string),
            input: Some(input),
            detail: Some(detail),
            suggestions: None,
            actions: None,
            metadata: Some(metadata),
        };
        self.register_request(
            out,
            request,
            PendingKind::Command,
            Some(json_id_key(id)),
            Vec::new(),
        );
    }

    fn handle_file_change_approval(
        &mut self,
        id: &Value,
        params: &Value,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Some(p) = params.as_object() else { return };
        let (Some(item_id), Some(thread_id), Some(turn_id)) = (
            p.get("itemId").and_then(Value::as_str),
            p.get("threadId").and_then(Value::as_str),
            p.get("turnId").and_then(Value::as_str),
        ) else {
            return;
        };
        let reason = p.get("reason").and_then(Value::as_str);
        let mut metadata = Map::new();
        metadata.insert("itemId".into(), item_id.into());
        metadata.insert("threadId".into(), thread_id.into());
        metadata.insert("turnId".into(), turn_id.into());
        let request = PermissionRequest {
            id: format!("permission-{item_id}"),
            provider: PROVIDER.into(),
            name: "CodexFileChange".into(),
            kind: PermissionKind::Tool,
            title: Some("Apply file changes".into()),
            description: reason.map(str::to_string),
            input: None,
            detail: Some(ToolCallDetail::Unknown {
                input: json!({ "reason": reason.map(Value::from).unwrap_or(Value::Null) }),
                output: Value::Null,
            }),
            suggestions: None,
            actions: None,
            metadata: Some(metadata),
        };
        self.register_request(
            out,
            request,
            PendingKind::File,
            Some(json_id_key(id)),
            Vec::new(),
        );
    }

    fn handle_tool_approval(
        &mut self,
        id: &Value,
        params: &Value,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Some(p) = params.as_object() else { return };
        let (Some(item_id), Some(thread_id), Some(turn_id), Some(raw_questions)) = (
            p.get("itemId").and_then(Value::as_str),
            p.get("threadId").and_then(Value::as_str),
            p.get("turnId").and_then(Value::as_str),
            p.get("questions").filter(|q| q.is_array()),
        ) else {
            return;
        };
        let questions = normalize_question_prompts(raw_questions);
        let mut input = Map::new();
        input.insert("questions".into(), Value::Array(questions.clone()));
        let mut metadata = Map::new();
        metadata.insert("itemId".into(), item_id.into());
        metadata.insert("threadId".into(), thread_id.into());
        metadata.insert("turnId".into(), turn_id.into());
        metadata.insert("questions".into(), Value::Array(questions.clone()));
        let request = PermissionRequest {
            id: format!("permission-{item_id}"),
            provider: PROVIDER.into(),
            name: "request_user_input".into(),
            kind: PermissionKind::Question,
            title: Some("Question".into()),
            description: None,
            input: Some(input),
            detail: Some(ToolCallDetail::PlainText {
                label: None,
                text: Some(format_question_prompts(&questions)),
                icon: Some(ToolCallIcon::Brain),
            }),
            suggestions: None,
            actions: None,
            metadata: Some(metadata),
        };
        // The question row is emitted before the permission request.
        let row = map_question_request_to_tool_call(
            item_id,
            &questions,
            ToolCallStatus::Running,
            None,
            None,
        );
        self.tool_timeline(out, row);
        self.register_request(
            out,
            request,
            PendingKind::Question,
            Some(json_id_key(id)),
            questions,
        );
    }

    fn handle_mcp_elicitation(
        &mut self,
        id: &Value,
        params: &Value,
        env: &mut dyn MapEnv,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Some(p) = params.as_object() else { return };
        let (Some(thread_id), Some(server_name), Some(mode), Some(message)) = (
            p.get("threadId").and_then(Value::as_str),
            p.get("serverName").and_then(Value::as_str),
            p.get("mode").and_then(Value::as_str),
            p.get("message").and_then(Value::as_str),
        ) else {
            return;
        };
        if !matches!(mode, "form" | "openai/form" | "url") || mode == "url" {
            return;
        }
        let required = p
            .get("requestedSchema")
            .and_then(|s| s.get("required"))
            .and_then(Value::as_array);
        if required.is_some_and(|r| !r.is_empty()) {
            return;
        }
        let request_id = format!("permission-{}", env.uuid());
        let mut input = Map::new();
        input.insert("mode".into(), mode.into());
        input.insert(
            "requestedSchema".into(),
            p.get("requestedSchema").cloned().unwrap_or(Value::Null),
        );
        input.insert("url".into(), p.get("url").cloned().unwrap_or(Value::Null));
        let mut metadata = Map::new();
        metadata.insert("threadId".into(), thread_id.into());
        metadata.insert(
            "turnId".into(),
            p.get("turnId").cloned().unwrap_or(Value::Null),
        );
        metadata.insert("serverName".into(), server_name.into());
        metadata.insert(
            "elicitationId".into(),
            p.get("elicitationId").cloned().unwrap_or(Value::Null),
        );
        let request = PermissionRequest {
            id: request_id.clone(),
            provider: PROVIDER.into(),
            name: "CodexMcpElicitation".into(),
            kind: PermissionKind::Tool,
            title: Some(format!("MCP approval: {server_name}")),
            description: Some(message.to_string()),
            input: Some(input),
            detail: None,
            suggestions: None,
            actions: None,
            metadata: Some(metadata),
        };
        let key = json_id_key(id);
        self.st
            .mcp_elicitation_permission_ids
            .insert(key.clone(), request_id);
        self.register_request(
            out,
            request,
            PendingKind::McpElicitation,
            Some(key),
            Vec::new(),
        );
    }

    fn register_request(
        &mut self,
        out: &mut Vec<AgentStreamEvent>,
        request: PermissionRequest,
        kind: PendingKind,
        server_id: Option<String>,
        questions: Vec<Value>,
    ) {
        self.st.pending.push(PendingPermission {
            request: request.clone(),
            kind,
            server_id,
            questions,
            plan: None,
        });
        self.emit(
            out,
            AgentStreamEvent::PermissionRequested {
                provider: PROVIDER.into(),
                request: Box::new(request),
                turn_id: None,
            },
        );
    }

    /// The client's answer to a server request: the wire half of `respondToPermission`.
    fn on_client_response(&mut self, id: &Value, m: &Map, out: &mut Vec<AgentStreamEvent>) {
        let key = json_id_key(id);
        let Some(pos) = self
            .st
            .pending
            .iter()
            .position(|p| p.server_id.as_deref() == Some(key.as_str()))
        else {
            return;
        };
        let pending = self.st.pending.remove(pos);
        let result = m.get("result").cloned().unwrap_or(Value::Null);
        let decision = result
            .get("decision")
            .or_else(|| result.get("action"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let question_answers = result.get("answers").and_then(Value::as_object);
        let response = match pending.kind {
            PendingKind::Question => {
                if question_answers.is_some_and(|a| !a.is_empty()) {
                    allow()
                } else {
                    PermissionResponse::Deny {
                        selected_action_id: None,
                        message: self.opts.deny_message.clone(),
                        interrupt: None,
                    }
                }
            }
            _ => match decision {
                "accept" | "acceptForSession" => allow(),
                "cancel" => PermissionResponse::Deny {
                    selected_action_id: None,
                    message: self.opts.deny_message.clone(),
                    interrupt: Some(true),
                },
                _ => PermissionResponse::Deny {
                    selected_action_id: None,
                    message: self.opts.deny_message.clone(),
                    interrupt: None,
                },
            },
        };
        let request_id = pending.request.id.clone();
        if matches!(response, PermissionResponse::Deny { .. })
            && pending.request.kind == PermissionKind::Tool
        {
            self.emit_denied_tool_call(out, &pending.request, &response);
        }
        self.emit(
            out,
            AgentStreamEvent::PermissionResolved {
                provider: PROVIDER.into(),
                request_id,
                resolution: response.clone(),
                turn_id: None,
            },
        );
        if pending.kind != PendingKind::Question {
            return;
        }
        let item_id = pending
            .request
            .metadata
            .as_ref()
            .and_then(|m| m.get("itemId"))
            .and_then(Value::as_str)
            .unwrap_or(&pending.request.id)
            .to_string();
        let row = if matches!(response, PermissionResponse::Allow { .. }) {
            let answers: Vec<(String, Vec<String>)> = question_answers
                .map(|a| {
                    a.iter()
                        .map(|(qid, v)| {
                            let values = v
                                .get("answers")
                                .and_then(Value::as_array)
                                .map(|x| {
                                    x.iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_string)
                                        .collect()
                                })
                                .unwrap_or_default();
                            (qid.clone(), values)
                        })
                        .collect()
                })
                .unwrap_or_default();
            map_question_request_to_tool_call(
                &item_id,
                &pending.questions,
                ToolCallStatus::Completed,
                Some(&answers),
                None,
            )
        } else {
            let (status, message) = match &response {
                PermissionResponse::Deny {
                    interrupt, message, ..
                } => (
                    if *interrupt == Some(true) {
                        ToolCallStatus::Canceled
                    } else {
                        ToolCallStatus::Failed
                    },
                    message
                        .clone()
                        .unwrap_or_else(|| "Question dismissed".into()),
                ),
                _ => (ToolCallStatus::Failed, "Question dismissed".into()),
            };
            map_question_request_to_tool_call(
                &item_id,
                &pending.questions,
                status,
                None,
                Some(json!({ "message": message })),
            )
        };
        self.tool_timeline(out, row);
    }

    /// `emitDeniedToolCallTimelineEvent`.
    fn emit_denied_tool_call(
        &mut self,
        out: &mut Vec<AgentStreamEvent>,
        request: &PermissionRequest,
        response: &PermissionResponse,
    ) {
        let name = match request.name.as_str() {
            "CodexBash" => "shell".to_string(),
            "CodexFileChange" => "apply_patch".to_string(),
            other => other.to_string(),
        };
        let message = match response {
            PermissionResponse::Deny {
                message: Some(m), ..
            } if !m.is_empty() => m.clone(),
            _ => "Permission denied".to_string(),
        };
        let detail = request
            .detail
            .clone()
            .unwrap_or_else(|| ToolCallDetail::Unknown {
                input: request
                    .input
                    .clone()
                    .map(Value::Object)
                    .unwrap_or(Value::Null),
                output: Value::Null,
            });
        let mut item = ToolCallItem::failed(
            request.id.clone(),
            name,
            detail,
            json!({ "message": message }),
        );
        let mut metadata = Map::new();
        metadata.insert("permissionRequestId".into(), request.id.clone().into());
        metadata.insert("denied".into(), Value::Bool(true));
        item.metadata = Some(metadata);
        self.tool_timeline(out, item);
    }

    // -- plan approvals ----------------------------------------------------------------------

    fn dismiss_pending_plan_approvals(&mut self, message: &str, out: &mut Vec<AgentStreamEvent>) {
        let ids: Vec<String> = self
            .st
            .pending
            .iter()
            .filter(|p| p.kind == PendingKind::Plan)
            .map(|p| p.request.id.clone())
            .collect();
        for id in ids {
            self.resolve_plan_permission(
                &id,
                PermissionResponse::Deny {
                    selected_action_id: None,
                    message: Some(message.to_string()),
                    interrupt: None,
                },
                out,
            );
        }
    }

    fn resolve_plan_permission(
        &mut self,
        request_id: &str,
        resolution: PermissionResponse,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let approved = matches!(resolution, PermissionResponse::Allow { .. });
        self.record_plan_outcome(
            request_id,
            if approved { "approved" } else { "rejected" },
            out,
        );
        self.st.pending.retain(|p| p.request.id != request_id);
        self.emit(
            out,
            AgentStreamEvent::PermissionResolved {
                provider: PROVIDER.into(),
                request_id: request_id.to_string(),
                resolution,
                turn_id: None,
            },
        );
    }

    fn record_plan_outcome(
        &mut self,
        request_id: &str,
        outcome: &str,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Some((text, turn)) = self
            .st
            .pending
            .iter()
            .find(|p| p.request.id == request_id)
            .and_then(|p| p.plan.clone())
        else {
            return;
        };
        let mut item = ToolCallItem::new(
            request_id,
            "plan_approval",
            if outcome == "canceled" {
                ToolCallStatus::Canceled
            } else {
                ToolCallStatus::Completed
            },
            ToolCallDetail::Plan { text },
        );
        let mut m = Map::new();
        if outcome != "canceled" {
            m.insert("approved".into(), Value::Bool(outcome == "approved"));
        }
        item.metadata = Some(m);
        self.timeline_with_turn(out, AgentTimelineItem::tool_call(item), turn);
    }

    fn emit_synthetic_plan_approval_request(
        &mut self,
        plan_text: &str,
        env: &mut dyn MapEnv,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        self.dismiss_pending_plan_approvals("Superseded by a newer plan", out);
        let request_id = format!("permission-{}", env.uuid());
        let mut input = Map::new();
        input.insert("plan".into(), plan_text.into());
        let mut metadata = Map::new();
        metadata.insert("planText".into(), plan_text.into());
        metadata.insert("source".into(), "codex_plan_approval".into());
        let request = PermissionRequest {
            id: request_id.clone(),
            provider: PROVIDER.into(),
            name: "CodexPlanApproval".into(),
            kind: PermissionKind::Plan,
            title: Some("Plan".into()),
            description: Some("Review the proposed plan before implementation starts.".into()),
            input: Some(input),
            detail: None,
            suggestions: None,
            actions: Some(plan_permission_actions()),
            metadata: Some(metadata),
        };
        self.st.pending.push(PendingPermission {
            request: request.clone(),
            kind: PendingKind::Plan,
            server_id: None,
            questions: Vec::new(),
            plan: Some((
                plan_text.to_string(),
                self.st.active_foreground_turn_id.clone(),
            )),
        });
        let row = ToolCallItem::new(
            request_id,
            "plan_approval",
            ToolCallStatus::Running,
            ToolCallDetail::Plan {
                text: plan_text.to_string(),
            },
        );
        self.tool_timeline(out, row);
        self.emit(
            out,
            AgentStreamEvent::PermissionRequested {
                provider: PROVIDER.into(),
                request: Box::new(request),
                turn_id: None,
            },
        );
    }

    // -- notifications -----------------------------------------------------------------------

    fn handle_notification(
        &mut self,
        method: &str,
        params: &Value,
        env: &mut dyn MapEnv,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        if method == "serverRequest/resolved" {
            if let Some(Value::Number(n)) = params.get("requestId") {
                let key = format!("n:{n}");
                if let Some(request_id) = self.st.mcp_elicitation_permission_ids.get(&key).cloned()
                {
                    self.st.mcp_elicitation_permission_ids.remove(&key);
                    if !self.st.pending.iter().any(|p| p.request.id == request_id) {
                        return;
                    }
                    self.st.pending.retain(|p| p.request.id != request_id);
                    self.emit(
                        out,
                        AgentStreamEvent::PermissionResolved {
                            provider: PROVIDER.into(),
                            request_id,
                            resolution: PermissionResponse::Deny {
                                selected_action_id: None,
                                message: None,
                                interrupt: Some(true),
                            },
                            turn_id: None,
                        },
                    );
                }
                return;
            }
        }
        let parsed = parse_notification(method, params);
        let thread = parsed.thread_id().map(str::to_string);
        match self.resolve_route(thread.as_deref()) {
            Route::Root => self.dispatch_parsed(&parsed, env, out),
            Route::SubAgent(call_id) => self.dispatch_sub_agent(&parsed, &call_id, env, out),
            Route::Pending(thread_id) => self.buffer_pending_sub_agent(&thread_id, parsed),
        }
    }

    fn resolve_route(&self, thread_id: Option<&str>) -> Route {
        let Some(t) = thread_id else {
            return Route::Root;
        };
        match &self.st.current_thread_id {
            None => Route::Root,
            Some(cur) if cur == t => Route::Root,
            Some(_) => match self.st.sub_agent_call_id_by_child_thread_id.get(t) {
                Some(c) => Route::SubAgent(c.clone()),
                None => Route::Pending(t.to_string()),
            },
        }
    }

    fn buffer_pending_sub_agent(&mut self, thread_id: &str, parsed: Parsed) {
        let list = &mut self.st.pending_sub_agent_notifications;
        let idx = match list.iter().position(|(t, _)| t == thread_id) {
            Some(i) => i,
            None => {
                if list.len() >= MAX_PENDING_SUB_AGENT_THREADS {
                    list.remove(0);
                }
                list.push((thread_id.to_string(), Vec::new()));
                list.len() - 1
            }
        };
        let pending = &mut list[idx].1;
        if pending.len() >= MAX_PENDING_SUB_AGENT_NOTIFICATIONS_PER_THREAD {
            pending.remove(0);
        }
        pending.push(parsed);
    }

    fn replay_pending_sub_agent_notifications(
        &mut self,
        thread_ids: &[String],
        env: &mut dyn MapEnv,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        for t in thread_ids {
            let Some(pos) = self
                .st
                .pending_sub_agent_notifications
                .iter()
                .position(|(x, _)| x == t)
            else {
                continue;
            };
            let (_, pending) = self.st.pending_sub_agent_notifications.remove(pos);
            let Some(call_id) = self.st.sub_agent_call_id_by_child_thread_id.get(t).cloned() else {
                continue;
            };
            for parsed in pending {
                self.dispatch_sub_agent(&parsed, &call_id, env, out);
            }
        }
    }

    fn sub_agent_call_id_for_thread(&self, thread_id: Option<&str>) -> Option<String> {
        let t = thread_id.filter(|t| !t.is_empty())?;
        if Some(t) == self.st.current_thread_id.as_deref() {
            return None;
        }
        self.st.sub_agent_call_id_by_child_thread_id.get(t).cloned()
    }

    fn dispatch_sub_agent(
        &mut self,
        parsed: &Parsed,
        call_id: &str,
        env: &mut dyn MapEnv,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        match parsed {
            Parsed::ThreadStarted { .. } => self.emit_sub_agent_activity_update(
                call_id,
                Some(ToolCallStatus::Running),
                true,
                out,
            ),
            Parsed::TurnStarted { .. }
            | Parsed::TurnCompleted { .. }
            | Parsed::AgentMessageDelta { .. }
            | Parsed::ReasoningDelta { .. }
            | Parsed::ItemStarted { .. }
            | Parsed::ItemCompleted { .. } => self.dispatch_parsed(parsed, env, out),
            Parsed::ExecCommandOutputDelta { .. } | Parsed::FileChangeOutputDelta { .. } => {
                self.handle_delta(parsed, Some(call_id), out)
            }
            Parsed::ExecCommandStarted { .. } => {
                self.handle_exec_started(parsed, Some(call_id), out)
            }
            Parsed::ExecCommandCompleted { .. } => {
                self.handle_exec_completed(parsed, Some(call_id), out)
            }
            Parsed::PatchApplyStarted { .. } => {
                self.handle_patch_started(parsed, Some(call_id), out)
            }
            Parsed::PatchApplyCompleted { .. } => {
                self.handle_patch_completed(parsed, Some(call_id), out)
            }
            _ => {}
        }
    }

    fn dispatch_parsed(
        &mut self,
        parsed: &Parsed,
        env: &mut dyn MapEnv,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        match parsed {
            Parsed::AgentMessageDelta { .. }
            | Parsed::ReasoningDelta { .. }
            | Parsed::ExecCommandOutputDelta { .. }
            | Parsed::FileChangeOutputDelta { .. } => self.handle_delta(parsed, None, out),
            Parsed::ContextCompacted { .. } => self.handle_context_compacted(parsed, out),
            Parsed::ThreadRolledBack { num_turns, .. } => {
                self.truncate_user_message_turns(*num_turns)
            }
            Parsed::ThreadStarted { thread_id } => {
                self.st.current_thread_id = Some(thread_id.clone());
                self.emit_event(
                    out,
                    AgentStreamEvent::ThreadStarted {
                        session_id: thread_id.clone(),
                        provider: PROVIDER.into(),
                    },
                );
            }
            Parsed::TurnStarted { .. } => self.handle_turn_started(parsed, out),
            Parsed::TurnCompleted { .. } => self.handle_turn_completed(parsed, env, out),
            Parsed::PlanUpdated { .. } => self.handle_plan_updated(parsed, out),
            // `turn/diff/updated` is progress telemetry for the whole turn, not a tool call.
            Parsed::DiffUpdated { .. } => {}
            Parsed::TokenUsageUpdated { .. } => self.handle_token_usage(parsed, out),
            Parsed::ExecCommandStarted { .. } => self.handle_exec_started(parsed, None, out),
            Parsed::ExecCommandCompleted { .. } => self.handle_exec_completed(parsed, None, out),
            Parsed::TerminalInteraction { .. } => self.handle_terminal_interaction(parsed, out),
            Parsed::PatchApplyStarted { .. } => self.handle_patch_started(parsed, None, out),
            Parsed::PatchApplyCompleted { .. } => self.handle_patch_completed(parsed, None, out),
            Parsed::ItemCompleted { .. } => self.handle_item_completed(parsed, env, out),
            Parsed::ItemStarted { .. } => self.handle_item_started(parsed, env, out),
            Parsed::Ignored => {}
        }
    }

    // -- deltas ------------------------------------------------------------------------------

    fn handle_delta(
        &mut self,
        parsed: &Parsed,
        routed: Option<&str>,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        match parsed {
            Parsed::AgentMessageDelta {
                item_id,
                delta,
                thread_id,
            } => {
                let prev = self
                    .st
                    .pending_agent_messages
                    .get(item_id)
                    .cloned()
                    .unwrap_or_default();
                let text = format!("{prev}{delta}");
                self.st
                    .pending_agent_messages
                    .insert(item_id.clone(), text.clone());
                if let Some(call_id) = self.sub_agent_call_id_for_thread(thread_id.as_deref()) {
                    if let Some(t) = thread_id {
                        self.emit_provider_subagent_timeline(
                            t,
                            AgentTimelineItem::AssistantMessage {
                                text: delta.clone(),
                                message_id: Some(item_id.clone()),
                            },
                            None,
                            out,
                        );
                    }
                    self.upsert_sub_agent_child_item(
                        &call_id,
                        item_id,
                        AgentTimelineItem::AssistantMessage {
                            text,
                            message_id: Some(item_id.clone()),
                        },
                    );
                    self.emit_sub_agent_activity_update(
                        &call_id,
                        Some(ToolCallStatus::Running),
                        false,
                        out,
                    );
                    return;
                }
                let first = prev.is_empty();
                let text = if first && self.st.pending_assistant_message_boundary {
                    format!("{ASSISTANT_MESSAGE_BOUNDARY_MARKDOWN}{delta}")
                } else {
                    delta.clone()
                };
                self.timeline(
                    out,
                    AgentTimelineItem::AssistantMessage {
                        text,
                        message_id: Some(item_id.clone()),
                    },
                );
                if first {
                    self.st.pending_assistant_message_boundary = false;
                }
            }
            Parsed::ReasoningDelta {
                item_id,
                delta,
                thread_id,
            } => {
                let list = self
                    .st
                    .pending_reasoning
                    .entry(item_id.clone())
                    .or_default();
                list.push(delta.clone());
                let joined = list.join("");
                if let Some(call_id) = self.sub_agent_call_id_for_thread(thread_id.as_deref()) {
                    if let Some(t) = thread_id {
                        self.emit_provider_subagent_timeline(
                            t,
                            AgentTimelineItem::Reasoning {
                                text: delta.clone(),
                            },
                            None,
                            out,
                        );
                    }
                    self.upsert_sub_agent_child_item(
                        &call_id,
                        item_id,
                        AgentTimelineItem::Reasoning { text: joined },
                    );
                    self.emit_sub_agent_activity_update(
                        &call_id,
                        Some(ToolCallStatus::Running),
                        false,
                        out,
                    );
                    return;
                }
                self.timeline(
                    out,
                    AgentTimelineItem::Reasoning {
                        text: delta.clone(),
                    },
                );
            }
            Parsed::ExecCommandOutputDelta { call_id, chunk, .. } => {
                let store = match routed {
                    Some(c) => self
                        .st
                        .sub_agent_calls
                        .get_mut(c)
                        .map(|s| &mut s.pending_command_output_deltas),
                    None => Some(&mut self.st.pending_command_output_deltas),
                };
                if let Some(store) = store {
                    append_output_delta(store, call_id.as_deref(), chunk.as_deref(), true);
                }
            }
            Parsed::FileChangeOutputDelta { item_id, delta, .. } => {
                let store = match routed {
                    Some(c) => self
                        .st
                        .sub_agent_calls
                        .get_mut(c)
                        .map(|s| &mut s.pending_file_change_output_deltas),
                    None => Some(&mut self.st.pending_file_change_output_deltas),
                };
                if let Some(store) = store {
                    append_output_delta(store, Some(item_id), delta.as_deref(), false);
                }
            }
            _ => {}
        }
    }

    // -- turn lifecycle ----------------------------------------------------------------------

    fn handle_turn_started(&mut self, parsed: &Parsed, out: &mut Vec<AgentStreamEvent>) {
        let Parsed::TurnStarted { thread_id, .. } = parsed else {
            return;
        };
        if let Some(call_id) = self.sub_agent_call_id_for_thread(thread_id.as_deref()) {
            self.emit_sub_agent_activity_update(&call_id, Some(ToolCallStatus::Running), true, out);
            return;
        }
        self.reset_turn_tracking_state();
        self.emit(
            out,
            AgentStreamEvent::TurnStarted {
                provider: PROVIDER.into(),
                turn_id: None,
            },
        );
    }

    fn handle_turn_completed(
        &mut self,
        parsed: &Parsed,
        env: &mut dyn MapEnv,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Parsed::TurnCompleted {
            status,
            error_message,
            thread_id,
        } = parsed
        else {
            return;
        };
        if let Some(call_id) = self.sub_agent_call_id_for_thread(thread_id.as_deref()) {
            self.settle_completed_sub_agent_turn(&call_id, thread_id.as_deref(), status, out);
            return;
        }
        self.complete_pending_root_compactions(out);
        if status == "failed" {
            self.emit(
                out,
                AgentStreamEvent::TurnFailed {
                    provider: PROVIDER.into(),
                    error: error_message
                        .clone()
                        .unwrap_or_else(|| "Codex turn failed".into()),
                    code: None,
                    diagnostic: None,
                    turn_id: None,
                },
            );
        } else if status == "interrupted" {
            self.dismiss_interrupted_async_questions(out);
            self.emit(
                out,
                AgentStreamEvent::TurnCanceled {
                    provider: PROVIDER.into(),
                    reason: "interrupted".into(),
                    turn_id: None,
                },
            );
        } else {
            let plan = self
                .st
                .latest_plan_result
                .as_ref()
                .map(|p| p.text.clone())
                .filter(|t| !t.is_empty());
            if let (true, Some(text)) = (self.st.plan_mode, plan) {
                self.emit_synthetic_plan_approval_request(&text, env, out);
            }
            let usage = self.st.latest_usage.clone();
            self.emit(
                out,
                AgentStreamEvent::TurnCompleted {
                    provider: PROVIDER.into(),
                    usage,
                    turn_id: None,
                },
            );
        }
        self.st.active_foreground_turn_id = None;
        self.st.pending_sub_agent_notifications.clear();
        self.reset_turn_tracking_state();
    }

    fn reset_turn_tracking_state(&mut self) {
        let st = &mut self.st;
        st.latest_plan_result = None;
        st.emitted_item_started_ids.clear();
        st.emitted_item_completed_ids.clear();
        st.emitted_provider_subagent_user_message_keys.clear();
        st.emitted_exec_started_call_ids.clear();
        st.emitted_exec_completed_call_ids.clear();
        st.pending_agent_messages.clear();
        st.pending_reasoning.clear();
        st.pending_command_output_deltas.clear();
        st.pending_file_change_output_deltas.clear();
        st.pending_assistant_message_boundary = false;
        st.pending_root_compaction_item_ids.clear();
        st.pending_anonymous_root_compactions = 0;
        st.unpaired_compaction_notification_completions = 0;
        st.unpaired_compaction_item_completions = 0;
    }

    fn dismiss_interrupted_async_questions(&mut self, out: &mut Vec<AgentStreamEvent>) {
        let pending: Vec<String> = self
            .st
            .async_questions
            .pending()
            .into_iter()
            .map(|r| r.id)
            .collect();
        for id in pending {
            if let Some(row) = self.st.async_questions.dismiss(&id) {
                self.tool_timeline(out, row);
            }
            self.emit(
                out,
                AgentStreamEvent::PermissionResolved {
                    provider: PROVIDER.into(),
                    request_id: id,
                    resolution: AsyncQuestions::interrupted_resolution(),
                    turn_id: None,
                },
            );
        }
    }

    fn handle_plan_updated(&mut self, parsed: &Parsed, out: &mut Vec<AgentStreamEvent>) {
        let Parsed::PlanUpdated { plan, .. } = parsed else {
            return;
        };
        if !self.st.plan_mode {
            let item = map_codex_plan_update_to_todo(plan);
            self.timeline(out, item);
            return;
        }
        let steps: Vec<(String, String)> = plan
            .iter()
            .map(|(s, st)| {
                (
                    s.clone().unwrap_or_default(),
                    st.clone().unwrap_or_else(|| "pending".into()),
                )
            })
            .collect();
        let text = plan_steps_to_markdown(&steps);
        let normalized = normalize_plan_markdown(&text);
        if !normalized.is_empty() {
            // Older Codex builds report Plan-mode proposals here; only remember the result.
            self.st.latest_plan_result = Some(LatestPlan { text: normalized });
        }
    }

    fn handle_token_usage(&mut self, parsed: &Parsed, out: &mut Vec<AgentStreamEvent>) {
        let Parsed::TokenUsageUpdated { token_usage, .. } = parsed else {
            return;
        };
        self.st.latest_usage = to_agent_usage(token_usage);
        if let Some(usage) = self.st.latest_usage.clone() {
            self.emit(
                out,
                AgentStreamEvent::UsageUpdated {
                    provider: PROVIDER.into(),
                    usage,
                    turn_id: None,
                },
            );
        }
    }

    // -- compaction --------------------------------------------------------------------------

    fn resolve_context_compaction_trigger(
        &mut self,
        item_id: Option<&str>,
    ) -> Option<&'static str> {
        if let Some(t) = item_id.and_then(|i| self.st.compaction_trigger_by_item_id.get(i)) {
            return Some(t);
        }
        // `pendingManualCompactionStarts` is only raised by the `/compact` slash command, which
        // is not on the wire; compactions here are always automatic or unattributed.
        None
    }

    fn compaction_item(&mut self, loading: bool, item_id: Option<&str>) -> AgentTimelineItem {
        let trigger = self.resolve_context_compaction_trigger(item_id);
        if let (Some(id), Some(t)) = (item_id.filter(|s| !s.is_empty()), trigger) {
            if loading {
                self.st
                    .compaction_trigger_by_item_id
                    .insert(id.to_string(), t);
            } else {
                self.st.compaction_trigger_by_item_id.remove(id);
            }
        }
        AgentTimelineItem::Compaction {
            status: if loading {
                paseo_agent_types::CompactionStatus::Loading
            } else {
                paseo_agent_types::CompactionStatus::Completed
            },
            trigger: trigger.map(|t| {
                if t == "manual" {
                    paseo_agent_types::CompactionTrigger::Manual
                } else {
                    paseo_agent_types::CompactionTrigger::Auto
                }
            }),
            pre_tokens: None,
            extra: Map::new(),
        }
    }

    fn track_pending_root_compaction(&mut self, item_id: Option<&str>) {
        match item_id.filter(|s| !s.is_empty()) {
            Some(id) => {
                if !self
                    .st
                    .pending_root_compaction_item_ids
                    .iter()
                    .any(|x| x == id)
                {
                    self.st
                        .pending_root_compaction_item_ids
                        .push(id.to_string());
                }
            }
            None => self.st.pending_anonymous_root_compactions += 1,
        }
    }

    /// `consumePendingRootCompaction`: `Some(Some(id))`, `Some(None)` (anonymous) or `None`.
    fn consume_pending_root_compaction(&mut self, item_id: Option<&str>) -> Option<Option<String>> {
        let st = &mut self.st;
        if let Some(id) = item_id.filter(|s| !s.is_empty()) {
            if let Some(pos) = st
                .pending_root_compaction_item_ids
                .iter()
                .position(|x| x == id)
            {
                st.pending_root_compaction_item_ids.remove(pos);
                return Some(Some(id.to_string()));
            }
            if st.pending_root_compaction_item_ids.is_empty()
                && st.pending_anonymous_root_compactions > 0
            {
                st.pending_anonymous_root_compactions -= 1;
                return Some(None);
            }
            return None;
        }
        if !st.pending_root_compaction_item_ids.is_empty() {
            return Some(Some(st.pending_root_compaction_item_ids.remove(0)));
        }
        if st.pending_anonymous_root_compactions > 0 {
            st.pending_anonymous_root_compactions -= 1;
            return Some(None);
        }
        None
    }

    fn complete_pending_root_compactions(&mut self, out: &mut Vec<AgentStreamEvent>) {
        let ids = std::mem::take(&mut self.st.pending_root_compaction_item_ids);
        for id in &ids {
            let item = self.compaction_item(false, Some(id));
            self.timeline(out, item);
        }
        let anon = std::mem::take(&mut self.st.pending_anonymous_root_compactions);
        for _ in 0..anon {
            let item = self.compaction_item(false, None);
            self.timeline(out, item);
        }
    }

    fn handle_context_compacted(&mut self, parsed: &Parsed, out: &mut Vec<AgentStreamEvent>) {
        let Parsed::ContextCompacted { thread_id, turn_id } = parsed else {
            return;
        };
        if Some(thread_id.as_str()) != self.st.current_thread_id.as_deref() {
            return;
        }
        if self.st.unpaired_compaction_item_completions > 0 {
            self.st.unpaired_compaction_item_completions -= 1;
            return;
        }
        let pending_item_id = self.consume_pending_root_compaction(None).and_then(|x| x);
        self.st.unpaired_compaction_notification_completions += 1;
        let item = self.compaction_item(false, pending_item_id.as_deref());
        self.timeline_with_turn(out, item, turn_id.clone());
    }

    fn handle_completed_context_compaction_item(
        &mut self,
        item: &Value,
        out: &mut Vec<AgentStreamEvent>,
    ) -> bool {
        if item_type(item).as_deref() != Some(CONTEXT_COMPACTION_TYPE) {
            return false;
        }
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let consumed = self.consume_pending_root_compaction(id);
        let has_different_pending = !self.st.pending_root_compaction_item_ids.is_empty()
            || self.st.pending_anonymous_root_compactions > 0;
        if id.is_some() && consumed.is_none() && has_different_pending {
            return true;
        }
        if self.st.unpaired_compaction_notification_completions > 0 {
            self.st.unpaired_compaction_notification_completions -= 1;
            return true;
        }
        let compaction = self.compaction_item(false, id);
        self.timeline(out, compaction);
        self.st.unpaired_compaction_item_completions += 1;
        true
    }

    // -- exec / patch / terminal -------------------------------------------------------------

    fn handle_exec_started(
        &mut self,
        parsed: &Parsed,
        sub: Option<&str>,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Parsed::ExecCommandStarted {
            call_id,
            command,
            cwd,
            thread_id,
        } = parsed
        else {
            return;
        };
        let store_exists = match sub {
            Some(c) => self.st.sub_agent_calls.contains_key(c),
            None => true,
        };
        if !store_exists {
            return;
        }
        if let (Some(id), None) = (call_id, sub) {
            self.st.emitted_exec_started_call_ids.insert(id.clone());
        }
        if let Some(id) = call_id {
            match sub {
                Some(c) => {
                    if let Some(s) = self.st.sub_agent_calls.get_mut(c) {
                        s.pending_command_output_deltas.remove(id);
                    }
                }
                None => {
                    self.st.pending_command_output_deltas.remove(id);
                }
            }
        }
        let cfg = self.cwd();
        let item = map_codex_exec_notification_to_tool_call(&ExecParams {
            call_id: call_id.as_deref(),
            command: Some(command),
            cwd: cwd.as_deref().or(cfg.as_deref()),
            output: None,
            exit_code: None,
            success: None,
            stderr: None,
            running: true,
        });
        if let Some(item) = item {
            self.emit_tool_item(item, sub, thread_id.as_deref(), out);
        }
    }

    fn handle_exec_completed(
        &mut self,
        parsed: &Parsed,
        sub: Option<&str>,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Parsed::ExecCommandCompleted {
            call_id,
            command,
            cwd,
            output,
            exit_code,
            success,
            stderr,
            thread_id,
        } = parsed
        else {
            return;
        };
        let buffered = match sub {
            Some(c) => match self.st.sub_agent_calls.get_mut(c) {
                Some(s) => {
                    consume_output_delta(&mut s.pending_command_output_deltas, call_id.as_deref())
                }
                None => return,
            },
            None => consume_output_delta(
                &mut self.st.pending_command_output_deltas,
                call_id.as_deref(),
            ),
        };
        let resolved = output.clone().or(buffered);
        if sub.is_none() {
            self.remember_terminal_process_for_command(command, resolved.as_deref(), out);
        }
        let cfg = self.cwd();
        let item = map_codex_exec_notification_to_tool_call(&ExecParams {
            call_id: call_id.as_deref(),
            command: Some(command),
            cwd: cwd.as_deref().or(cfg.as_deref()),
            output: resolved.as_deref(),
            exit_code: exit_code.as_ref(),
            success: *success,
            stderr: stderr.as_deref(),
            running: false,
        });
        if let Some(item) = item {
            if sub.is_none() {
                self.st
                    .emitted_exec_completed_call_ids
                    .insert(item.call_id.clone());
            }
            self.emit_tool_item(item, sub, thread_id.as_deref(), out);
        }
    }

    fn handle_patch_started(
        &mut self,
        parsed: &Parsed,
        sub: Option<&str>,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Parsed::PatchApplyStarted {
            call_id,
            changes,
            thread_id,
        } = parsed
        else {
            return;
        };
        match sub {
            Some(c) => match self.st.sub_agent_calls.get_mut(c) {
                Some(s) => {
                    if let Some(id) = call_id {
                        s.pending_file_change_output_deltas.remove(id);
                    }
                }
                None => return,
            },
            None => {
                if let Some(id) = call_id {
                    self.st.pending_file_change_output_deltas.remove(id);
                }
            }
        }
        let cfg = self.cwd();
        let item = map_codex_patch_notification_to_tool_call(&PatchParams {
            call_id: call_id.as_deref(),
            changes,
            cwd: cfg.as_deref(),
            stdout: None,
            stderr: None,
            success: None,
            running: true,
        });
        if let Some(item) = item {
            self.emit_tool_item(item, sub, thread_id.as_deref(), out);
        }
    }

    fn handle_patch_completed(
        &mut self,
        parsed: &Parsed,
        sub: Option<&str>,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Parsed::PatchApplyCompleted {
            call_id,
            changes,
            stdout,
            stderr,
            success,
            thread_id,
        } = parsed
        else {
            return;
        };
        let buffered = match sub {
            Some(c) => match self.st.sub_agent_calls.get_mut(c) {
                Some(s) => consume_output_delta(
                    &mut s.pending_file_change_output_deltas,
                    call_id.as_deref(),
                ),
                None => return,
            },
            None => consume_output_delta(
                &mut self.st.pending_file_change_output_deltas,
                call_id.as_deref(),
            ),
        };
        let cfg = self.cwd();
        let stdout = stdout.clone().or(buffered);
        let item = map_codex_patch_notification_to_tool_call(&PatchParams {
            call_id: call_id.as_deref(),
            changes,
            cwd: cfg.as_deref(),
            stdout: stdout.as_deref(),
            stderr: stderr.as_deref(),
            success: *success,
            running: false,
        });
        if let Some(item) = item {
            self.emit_tool_item(item, sub, thread_id.as_deref(), out);
        }
    }

    /// `emitCodexToolTimelineItem`.
    fn emit_tool_item(
        &mut self,
        item: ToolCallItem,
        sub: Option<&str>,
        child_thread: Option<&str>,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Some(call_id) = sub else {
            self.tool_timeline(out, item);
            return;
        };
        let timeline_item = AgentTimelineItem::tool_call(item.clone());
        self.upsert_sub_agent_child_item(call_id, &item.call_id, timeline_item.clone());
        let targets: Vec<String> = match child_thread {
            Some(t) => vec![t.to_string()],
            None => self
                .st
                .sub_agent_calls
                .get(call_id)
                .map(|s| s.child_thread_ids.clone())
                .unwrap_or_default(),
        };
        if self.st.sub_agent_calls.contains_key(call_id) {
            for t in targets {
                self.emit_provider_subagent_timeline(&t, timeline_item.clone(), None, out);
            }
        }
        let status = (item.status == ToolCallStatus::Running).then_some(ToolCallStatus::Running);
        self.emit_sub_agent_activity_update(call_id, status, false, out);
    }

    fn handle_terminal_interaction(&mut self, parsed: &Parsed, out: &mut Vec<AgentStreamEvent>) {
        let Parsed::TerminalInteraction {
            call_id,
            process_id,
            stdin,
            ..
        } = parsed
        else {
            return;
        };
        let key = format!(
            "{}\u{0}{}",
            process_id.as_deref().unwrap_or(""),
            stdin.as_deref().unwrap_or("")
        );
        if !self.st.emitted_terminal_interaction_keys.insert(key) {
            return;
        }
        let command = process_id
            .as_deref()
            .and_then(|p| self.st.terminal_command_by_process_id.get(p).cloned());
        let call =
            self.create_terminal_interaction_call_id(process_id.as_deref(), call_id.as_deref());
        if let (None, Some(p)) = (&command, process_id) {
            self.st
                .pending_unlabeled_terminal_interactions
                .entry(p.clone())
                .or_default()
                .push((call.clone(), stdin.clone()));
        }
        let item = map_codex_terminal_interaction_to_tool_call(
            &call,
            process_id.as_deref(),
            command.as_deref(),
            stdin.as_deref(),
        );
        self.tool_timeline(out, item);
    }

    fn create_terminal_interaction_call_id(
        &mut self,
        process_id: Option<&str>,
        fallback: Option<&str>,
    ) -> String {
        let base = match process_id.filter(|s| !s.is_empty()) {
            Some(p) => format!("terminal-session-{p}"),
            None => fallback
                .filter(|s| !s.is_empty())
                .unwrap_or("terminal-interaction")
                .to_string(),
        };
        self.st.next_terminal_interaction_ordinal += 1;
        format!("{base}-{}", self.st.next_terminal_interaction_ordinal)
    }

    fn remember_terminal_process_for_command(
        &mut self,
        command: &Value,
        output: Option<&str>,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        if let Some(pid) = extract_codex_terminal_session_id(output) {
            self.remember_terminal_process(&pid, command, out);
        }
    }

    fn remember_terminal_process(
        &mut self,
        process_id: &str,
        command: &Value,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Some(display) = normalize_command_execution_command(Some(command)) else {
            return;
        };
        self.st
            .terminal_command_by_process_id
            .insert(process_id.to_string(), display.clone());
        let Some(pending) = self
            .st
            .pending_unlabeled_terminal_interactions
            .remove(process_id)
        else {
            return;
        };
        for (call, stdin) in pending {
            let item = map_codex_terminal_interaction_to_tool_call(
                &call,
                Some(process_id),
                Some(&display),
                stdin.as_deref(),
            );
            self.tool_timeline(out, item);
        }
    }

    // -- items -------------------------------------------------------------------------------

    fn remember_user_message_turn(
        &mut self,
        message_id: Option<&str>,
        provider_turn_id: Option<&str>,
    ) -> bool {
        let Some(id) = message_id.filter(|s| !s.is_empty()) else {
            return false;
        };
        let st = &mut self.st;
        if st.user_message_turn_indexes.contains_key(id) {
            if let Some(t) = provider_turn_id.filter(|s| !s.is_empty()) {
                st.user_message_provider_turn_ids
                    .insert(id.to_string(), t.to_string());
            }
            return false;
        }
        st.user_message_turn_indexes
            .insert(id.to_string(), st.user_message_turn_ids.len());
        st.user_message_turn_ids.push(id.to_string());
        if let Some(t) = provider_turn_id.filter(|s| !s.is_empty()) {
            st.user_message_provider_turn_ids
                .insert(id.to_string(), t.to_string());
        }
        true
    }

    fn truncate_user_message_turns(&mut self, num_turns: i64) {
        if num_turns <= 0 {
            return;
        }
        let st = &mut self.st;
        let retained = st
            .user_message_turn_ids
            .len()
            .saturating_sub(num_turns as usize);
        let removed: Vec<String> = st.user_message_turn_ids.split_off(retained);
        for id in removed {
            st.user_message_provider_turn_ids.remove(&id);
        }
        st.user_message_turn_indexes.clear();
        for (i, id) in st.user_message_turn_ids.iter().enumerate() {
            st.user_message_turn_indexes.insert(id.clone(), i);
        }
    }

    fn thread_item_opts_cwd(&self) -> Option<String> {
        self.cwd()
    }

    fn to_timeline(&self, item: &Value, include_user: bool) -> Option<AgentTimelineItem> {
        let cwd = self.thread_item_opts_cwd();
        thread_item_to_timeline(
            item,
            &ThreadItemOptions {
                include_user_message: include_user,
                cwd: cwd.as_deref(),
                materialize: self.opts.materialize.as_deref(),
            },
        )
    }

    fn handle_user_message_item(
        &mut self,
        thread_id: Option<&str>,
        turn_id: Option<&str>,
        item: &Value,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Some(AgentTimelineItem::UserMessage {
            text,
            message_id,
            client_message_id,
        }) = self.to_timeline(item, true)
        else {
            return;
        };
        let item_id = item.get("id").and_then(Value::as_str).map(str::to_string);
        if let Some(call_id) = self.sub_agent_call_id_for_thread(thread_id) {
            let Some(child_message_id) = item_id.clone().or_else(|| message_id.clone()) else {
                return;
            };
            let key = format!("{}:{}", thread_id.unwrap_or(&call_id), child_message_id);
            if !self
                .st
                .emitted_provider_subagent_user_message_keys
                .insert(key)
            {
                return;
            }
            let timeline_item = AgentTimelineItem::UserMessage {
                text,
                message_id,
                client_message_id,
            };
            if let Some(t) = thread_id {
                self.emit_provider_subagent_timeline(t, timeline_item.clone(), None, out);
            }
            if let Some(id) = item_id {
                self.upsert_sub_agent_child_item(&call_id, &id, timeline_item);
            }
            self.emit_sub_agent_activity_update(
                &call_id,
                Some(ToolCallStatus::Running),
                false,
                out,
            );
            return;
        }
        if !self.remember_user_message_turn(message_id.as_deref(), turn_id) {
            return;
        }
        self.timeline(
            out,
            AgentTimelineItem::UserMessage {
                text,
                message_id,
                client_message_id,
            },
        );
    }

    fn handle_item_started(
        &mut self,
        parsed: &Parsed,
        env: &mut dyn MapEnv,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Parsed::ItemStarted {
            source,
            thread_id,
            turn_id,
            item,
        } = parsed
        else {
            return;
        };
        if *source == Source::CodexEvent && read_sub_agent_activity(item).is_none() {
            return;
        }
        let kind = item_type(item);
        if kind.as_deref() == Some("userMessage") {
            self.handle_user_message_item(thread_id.as_deref(), turn_id.as_deref(), item, out);
            return;
        }
        let child = self.sub_agent_call_id_for_thread(thread_id.as_deref());
        if let Some(c) = &child {
            if self.handle_sub_agent_context_compaction_item(c, item, true, out) {
                return;
            }
        }
        if kind.as_deref() == Some(CONTEXT_COMPACTION_TYPE) {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            self.track_pending_root_compaction(id);
            let c = self.compaction_item(true, id);
            self.timeline(out, c);
            return;
        }
        if self.handle_registered_sub_agent_activity(item, out) {
            return;
        }
        let Some(AgentTimelineItem::ToolCall(call)) = self.to_timeline(item, false) else {
            return;
        };
        let call = *call;
        let registered = self.register_sub_agent_tool_call(
            &call,
            item,
            child.clone(),
            parent_subagent_id(child.as_deref(), thread_id.as_deref()),
            out,
        );
        if let Some(c) = &child {
            self.emit_started_provider_subagent_item(
                thread_id.as_deref(),
                AgentTimelineItem::tool_call(call.clone()),
                out,
            );
            if let Some(id) = item
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                self.upsert_sub_agent_child_item(c, id, AgentTimelineItem::tool_call(call.clone()));
            }
            self.emit_sub_agent_activity_update(c, Some(ToolCallStatus::Running), false, out);
            self.replay_pending_sub_agent_notifications(&registered, env, out);
            return;
        }
        let item_id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if kind.as_deref() == Some("commandExecution") {
            if let Some(pid) = item
                .get("processId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let cmd = item.get("command").cloned().unwrap_or(Value::Null);
                self.remember_terminal_process(pid, &cmd, out);
            }
            let call_id = if call.call_id.is_empty() {
                item_id.clone()
            } else {
                Some(call.call_id.clone())
            };
            if call_id.is_some_and(|c| self.st.emitted_exec_started_call_ids.contains(&c)) {
                return;
            }
        }
        if item_id
            .as_ref()
            .is_some_and(|i| self.st.emitted_item_started_ids.contains(i))
        {
            return;
        }
        self.tool_timeline(out, call);
        if let Some(id) = item_id {
            self.st.emitted_item_started_ids.insert(id.clone());
            self.st.pending_command_output_deltas.remove(&id);
            self.st.pending_file_change_output_deltas.remove(&id);
        }
        self.replay_pending_sub_agent_notifications(&registered, env, out);
    }

    fn handle_item_completed(
        &mut self,
        parsed: &Parsed,
        env: &mut dyn MapEnv,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Parsed::ItemCompleted {
            source,
            thread_id,
            turn_id,
            item,
        } = parsed
        else {
            return;
        };
        if *source == Source::CodexEvent && read_sub_agent_activity(item).is_none() {
            return;
        }
        self.receive_async_question(thread_id.as_deref(), item, out);
        let kind = item_type(item);
        if kind.as_deref() == Some("userMessage") {
            self.handle_user_message_item(thread_id.as_deref(), turn_id.as_deref(), item, out);
            return;
        }
        let child = self.sub_agent_call_id_for_thread(thread_id.as_deref());
        // handleCompletedSpecialItem
        if let Some(c) = &child {
            if self.handle_sub_agent_context_compaction_item(c, item, false, out) {
                return;
            }
        }
        if self.handle_completed_context_compaction_item(item, out)
            || self.handle_registered_sub_agent_activity(item, out)
        {
            return;
        }
        let Some(mut timeline_item) = self.to_timeline(item, false) else {
            return;
        };
        let registered = match &timeline_item {
            AgentTimelineItem::ToolCall(call) => self.register_sub_agent_tool_call(
                call,
                item,
                child.clone(),
                parent_subagent_id(child.as_deref(), thread_id.as_deref()),
                out,
            ),
            _ => Vec::new(),
        };
        self.settle_completed_non_child_sub_agent_activity(&mut timeline_item, item, &registered);
        let image_items =
            mcp_tool_result_images_to_timeline(item, self.opts.materialize.as_deref());
        let item_id = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if let Some(c) = &child {
            self.emit_completed_provider_subagent_item(
                thread_id.as_deref(),
                item_id.as_deref(),
                &timeline_item,
                out,
            );
            self.handle_sub_agent_child_item_completed(c, item_id.as_deref(), timeline_item, out);
            if let Some(t) = thread_id.as_deref() {
                for img in image_items {
                    self.emit_provider_subagent_timeline(t, img, None, out);
                }
            }
            self.replay_pending_sub_agent_notifications(&registered, env, out);
            return;
        }
        // shouldSkipCompletedThreadItem
        let skip = match (&timeline_item, kind.as_deref()) {
            (AgentTimelineItem::ToolCall(call), Some("commandExecution")) => {
                let id = if call.call_id.is_empty() {
                    item_id.clone()
                } else {
                    Some(call.call_id.clone())
                };
                id.is_some_and(|i| self.st.emitted_exec_completed_call_ids.contains(&i))
            }
            _ => item_id
                .as_ref()
                .is_some_and(|i| self.st.emitted_item_completed_ids.contains(i)),
        };
        if skip {
            self.replay_pending_sub_agent_notifications(&registered, env, out);
            return;
        }
        if self.consume_streamed_text_completion(&timeline_item, item_id.as_deref(), out) {
            if matches!(timeline_item, AgentTimelineItem::AssistantMessage { .. }) {
                self.st.pending_assistant_message_boundary = true;
            }
            if let Some(id) = &item_id {
                self.st.emitted_item_completed_ids.insert(id.clone());
                self.st.emitted_item_started_ids.remove(id);
            }
            self.replay_pending_sub_agent_notifications(&registered, env, out);
            return;
        }
        self.apply_buffered_delta_text(&mut timeline_item, item_id.as_deref());
        if let AgentTimelineItem::ToolCall(call) = &timeline_item {
            if let ToolCallDetail::Plan { text } = &call.detail {
                self.st.latest_plan_result = Some(LatestPlan { text: text.clone() });
                if self.st.plan_mode {
                    return;
                }
            }
        }
        let is_assistant = matches!(timeline_item, AgentTimelineItem::AssistantMessage { .. });
        self.timeline(out, timeline_item);
        if is_assistant {
            self.st.pending_assistant_message_boundary = true;
        }
        for img in image_items {
            self.timeline(out, img);
            self.st.pending_assistant_message_boundary = true;
        }
        if let Some(id) = item_id {
            self.st.emitted_item_completed_ids.insert(id.clone());
            self.st.emitted_item_started_ids.remove(&id);
            self.st.pending_command_output_deltas.remove(&id);
            self.st.pending_file_change_output_deltas.remove(&id);
        }
        self.replay_pending_sub_agent_notifications(&registered, env, out);
    }

    fn receive_async_question(
        &mut self,
        thread_id: Option<&str>,
        item: &Value,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        if thread_id != self.st.current_thread_id.as_deref() {
            return;
        }
        if let Some(request) = self.st.async_questions.receive(item) {
            self.emit(
                out,
                AgentStreamEvent::PermissionRequested {
                    provider: PROVIDER.into(),
                    request: Box::new(request),
                    turn_id: None,
                },
            );
        }
    }

    fn consume_streamed_text_completion(
        &mut self,
        item: &AgentTimelineItem,
        item_id: Option<&str>,
        out: &mut Vec<AgentStreamEvent>,
    ) -> bool {
        let Some(id) = item_id.filter(|s| !s.is_empty()) else {
            return false;
        };
        match item {
            AgentTimelineItem::AssistantMessage { .. }
                if self.st.pending_agent_messages.contains_key(id) =>
            {
                let streamed = self
                    .st
                    .pending_agent_messages
                    .remove(id)
                    .unwrap_or_default();
                if let Some(suffix) = missing_final_text_suffix(item, &streamed) {
                    self.timeline(out, suffix);
                }
                true
            }
            AgentTimelineItem::Reasoning { .. } if self.st.pending_reasoning.contains_key(id) => {
                let streamed = self
                    .st
                    .pending_reasoning
                    .remove(id)
                    .unwrap_or_default()
                    .join("");
                if let Some(suffix) = missing_final_text_suffix(item, &streamed) {
                    self.timeline(out, suffix);
                }
                true
            }
            _ => false,
        }
    }

    fn apply_buffered_delta_text(&mut self, item: &mut AgentTimelineItem, item_id: Option<&str>) {
        let Some(id) = item_id.filter(|s| !s.is_empty()) else {
            return;
        };
        match item {
            AgentTimelineItem::AssistantMessage { text, .. } => {
                if let Some(b) = self
                    .st
                    .pending_agent_messages
                    .get(id)
                    .filter(|b| !b.is_empty())
                {
                    if !text.starts_with(b.as_str()) {
                        *text = b.clone();
                    }
                }
            }
            AgentTimelineItem::Reasoning { text } => {
                if let Some(b) = self.st.pending_reasoning.get(id).filter(|b| !b.is_empty()) {
                    let streamed = b.join("");
                    if !text.starts_with(streamed.as_str()) {
                        *text = streamed;
                    }
                }
            }
            _ => {}
        }
    }

    // -- sub-agents --------------------------------------------------------------------------

    fn upsert_sub_agent_child_item(
        &mut self,
        call_id: &str,
        item_id: &str,
        item: AgentTimelineItem,
    ) {
        let Some(state) = self.st.sub_agent_calls.get_mut(call_id) else {
            return;
        };
        if !state.child_items.contains_key(item_id) {
            state.child_item_order.push(item_id.to_string());
        }
        state.child_items.insert(item_id.to_string(), item);
    }

    #[allow(clippy::too_many_arguments)]
    fn register_sub_agent_tool_call(
        &mut self,
        timeline_item: &ToolCallItem,
        raw_item: &Value,
        parent_call_id: Option<String>,
        parent_subagent_id: Option<String>,
        out: &mut Vec<AgentStreamEvent>,
    ) -> Vec<String> {
        let ToolCallDetail::SubAgent { log, .. } = &timeline_item.detail else {
            return Vec::new();
        };
        let call_id = timeline_item.call_id.clone();
        let existing_log =
            self.st
                .sub_agent_calls
                .get(&call_id)
                .and_then(|s| match &s.tool_call.detail {
                    ToolCallDetail::SubAgent { log, .. } => Some(log.clone()),
                    _ => None,
                });
        let mut merged = timeline_item.clone();
        if let ToolCallDetail::SubAgent { log: l, .. } = &mut merged.detail {
            *l = if !log.is_empty() {
                log.clone()
            } else {
                existing_log.unwrap_or_default()
            };
        }
        let state = self
            .st
            .sub_agent_calls
            .entry(call_id.clone())
            .or_insert_with(|| SubAgentCall {
                call_id: call_id.clone(),
                tool_call: timeline_item.clone(),
                parent_call_id: parent_call_id.clone(),
                parent_subagent_id: parent_subagent_id.clone(),
                activity_item_ids: HashSet::new(),
                pending_command_output_deltas: HashMap::new(),
                pending_file_change_output_deltas: HashMap::new(),
                child_item_order: Vec::new(),
                child_items: HashMap::new(),
                child_thread_ids: Vec::new(),
            });
        state.tool_call = merged;
        if state.parent_call_id.is_none() {
            state.parent_call_id = parent_call_id;
        }
        if state.parent_subagent_id.is_none() {
            state.parent_subagent_id = parent_subagent_id;
        }
        if let Some(a) = read_sub_agent_activity(raw_item) {
            if let Some(id) = a.id {
                state.activity_item_ids.insert(id);
            }
        }
        let receiver: Vec<String> = raw_item
            .get("receiverThreadIds")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let agent_thread = raw_item
            .get("agentThreadId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let mut child_ids: Vec<String> = Vec::new();
        for t in receiver.into_iter().chain(agent_thread) {
            if !child_ids.contains(&t) {
                child_ids.push(t);
            }
        }
        child_ids.retain(|t| Some(t.as_str()) != self.st.current_thread_id.as_deref());
        for t in &child_ids {
            if let Some(owner) = self.st.sub_agent_call_id_by_child_thread_id.get(t) {
                if *owner != call_id {
                    continue;
                }
            }
            self.st
                .sub_agent_call_id_by_child_thread_id
                .insert(t.clone(), call_id.clone());
            if let Some(state) = self.st.sub_agent_calls.get_mut(&call_id) {
                if !state.child_thread_ids.contains(t) {
                    state.child_thread_ids.push(t.clone());
                }
            }
            self.emit_provider_subagent_upsert(t, &call_id, timeline_item.status, out);
        }
        child_ids
    }

    fn handle_registered_sub_agent_activity(
        &mut self,
        raw_item: &Value,
        out: &mut Vec<AgentStreamEvent>,
    ) -> bool {
        let Some(activity) = read_sub_agent_activity(raw_item) else {
            return false;
        };
        let Some(call_id) = self
            .st
            .sub_agent_call_id_by_child_thread_id
            .get(&activity.agent_thread_id)
            .cloned()
        else {
            return false;
        };
        let cwd = self.cwd();
        let activity_tool_call = map_codex_tool_call_from_thread_item(raw_item, cwd.as_deref());
        let Some(state) = self.st.sub_agent_calls.get_mut(&call_id) else {
            return false;
        };
        if let Some(id) = &activity.id {
            if state.activity_item_ids.contains(id) {
                return true;
            }
            state.activity_item_ids.insert(id.clone());
        }
        if let (Some(act), ToolCallDetail::SubAgent { sub_agent_type, .. }) =
            (&activity_tool_call, &mut state.tool_call.detail)
        {
            if let ToolCallDetail::SubAgent {
                sub_agent_type: new_t,
                ..
            } = &act.detail
            {
                *sub_agent_type = new_t.clone();
            }
        }
        let current = state.tool_call.status;
        let next = match activity.kind {
            SubAgentKind::Interrupted => Some(ToolCallStatus::Canceled),
            SubAgentKind::Completed => Some(ToolCallStatus::Completed),
            _ if is_terminal(current) => None,
            _ => Some(ToolCallStatus::Running),
        };
        self.emit_sub_agent_activity_update(&call_id, next, false, out);
        true
    }

    fn settle_completed_non_child_sub_agent_activity(
        &mut self,
        timeline_item: &mut AgentTimelineItem,
        raw_item: &Value,
        registered: &[String],
    ) {
        let activity = read_sub_agent_activity(raw_item);
        let AgentTimelineItem::ToolCall(call) = timeline_item else {
            return;
        };
        let Some(activity) = activity else { return };
        let has_children = self
            .st
            .sub_agent_calls
            .get(&call.call_id)
            .is_some_and(|s| !s.child_thread_ids.is_empty());
        if !registered.is_empty() || has_children {
            return;
        }
        let settled = settle_historical_sub_agent_activity((**call).clone(), activity.kind);
        call.status = settled.status;
        call.error = settled.error;
        if let Some(state) = self.st.sub_agent_calls.get_mut(&call.call_id) {
            state.tool_call = (**call).clone();
        }
    }

    fn settle_completed_sub_agent_turn(
        &mut self,
        call_id: &str,
        thread_id: Option<&str>,
        parsed_status: &str,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let status = match parsed_status {
            "failed" => ToolCallStatus::Failed,
            "interrupted" => ToolCallStatus::Canceled,
            _ => ToolCallStatus::Completed,
        };
        let mut settled_events: Vec<(String, AgentTimelineItem)> = Vec::new();
        if let Some(state) = self.st.sub_agent_calls.get(call_id) {
            let orphans: Vec<(String, ToolCallItem)> = state
                .child_items
                .iter()
                .filter_map(|(id, item)| match item {
                    AgentTimelineItem::ToolCall(c)
                        if matches!(c.detail, ToolCallDetail::SubAgent { .. })
                            && c.status == ToolCallStatus::Running
                            && self
                                .st
                                .sub_agent_calls
                                .get(&c.call_id)
                                .map(|s| s.child_thread_ids.len())
                                .unwrap_or(0)
                                == 0 =>
                    {
                        Some((id.clone(), (**c).clone()))
                    }
                    _ => None,
                })
                .collect();
            for (id, child) in orphans {
                let mut settled = child.clone();
                if status == ToolCallStatus::Failed {
                    settled.status = ToolCallStatus::Failed;
                    settled.error = Some(error_or(
                        &child.error,
                        json!({"message": "Sub-agent failed"}),
                    ));
                } else {
                    settled.status = status;
                    settled.error = Some(Value::Null);
                }
                settled_events.push((id, AgentTimelineItem::tool_call(settled)));
            }
        }
        for (id, item) in settled_events {
            if let Some(state) = self.st.sub_agent_calls.get_mut(call_id) {
                state.child_items.insert(id, item.clone());
            }
            if let Some(t) = thread_id {
                self.emit_provider_subagent_timeline(t, item, None, out);
            }
        }
        self.emit_sub_agent_activity_update(call_id, Some(status), false, out);
    }

    fn emit_sub_agent_activity_update(
        &mut self,
        call_id: &str,
        status: Option<ToolCallStatus>,
        reopen: bool,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let (resolved, child_threads, parent_call_id, next_call) = {
            let Some(state) = self.st.sub_agent_calls.get(call_id) else {
                return;
            };
            if !matches!(state.tool_call.detail, ToolCallDetail::SubAgent { .. }) {
                return;
            }
            let child_timeline: Vec<AgentTimelineItem> = state
                .child_item_order
                .iter()
                .filter_map(|id| state.child_items.get(id).cloned())
                .collect();
            let log = if child_timeline.is_empty() {
                String::new()
            } else {
                curate_labeled_activity(&child_timeline)
            };
            let mut resolved = status.unwrap_or(state.tool_call.status);
            if status == Some(ToolCallStatus::Running)
                && !reopen
                && is_terminal(state.tool_call.status)
            {
                resolved = state.tool_call.status;
            }
            let mut next = state.tool_call.clone();
            if let ToolCallDetail::SubAgent { log: l, .. } = &mut next.detail {
                *l = log.clone();
            }
            if resolved == ToolCallStatus::Failed {
                next.status = ToolCallStatus::Failed;
                next.error = Some(error_or(
                    &state.tool_call.error,
                    json!({"message": "Sub-agent failed"}),
                ));
            } else {
                next.status = resolved;
                next.error = Some(Value::Null);
            }
            (
                resolved,
                state.child_thread_ids.clone(),
                state.parent_call_id.clone(),
                next,
            )
        };
        for t in &child_threads {
            self.emit_provider_subagent_upsert(t, call_id, resolved, out);
        }
        if let Some(state) = self.st.sub_agent_calls.get_mut(call_id) {
            state.tool_call = next_call.clone();
        }
        if let Some(parent) = parent_call_id.filter(|p| p != call_id) {
            self.upsert_sub_agent_child_item(
                &parent,
                call_id,
                AgentTimelineItem::tool_call(next_call),
            );
            self.emit_sub_agent_activity_update(&parent, None, false, out);
            return;
        }
        self.tool_timeline(out, next_call);
    }

    fn emit_provider_subagent_upsert(
        &mut self,
        child_thread_id: &str,
        call_id: &str,
        status: ToolCallStatus,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Some(state) = self.st.sub_agent_calls.get(call_id) else {
            return;
        };
        let ToolCallDetail::SubAgent {
            sub_agent_type,
            description,
            ..
        } = &state.tool_call.detail
        else {
            return;
        };
        let provider_status = match status {
            ToolCallStatus::Completed => ProviderSubagentStatus::Completed,
            ToolCallStatus::Failed => ProviderSubagentStatus::Failed,
            ToolCallStatus::Canceled => ProviderSubagentStatus::Canceled,
            ToolCallStatus::Running => ProviderSubagentStatus::Running,
        };
        let event = ProviderSubagentInputEvent::Upsert {
            id: child_thread_id.to_string(),
            title: Some(Some(
                sub_agent_type
                    .clone()
                    .unwrap_or_else(|| "Codex subagent".into()),
            )),
            description: Some(description.clone()),
            status: Some(provider_status),
            tool_call_id: Some(Some(state.call_id.clone())),
            cwd: None,
            subtitle: None,
            parent_subagent_id: Some(state.parent_subagent_id.clone()),
            timestamp: None,
        };
        self.emit_event(
            out,
            AgentStreamEvent::ProviderSubagent {
                provider: PROVIDER.into(),
                event: Box::new(event),
            },
        );
    }

    fn emit_provider_subagent_timeline(
        &mut self,
        child_thread_id: &str,
        item: AgentTimelineItem,
        timestamp: Option<String>,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        self.emit_event(
            out,
            AgentStreamEvent::ProviderSubagent {
                provider: PROVIDER.into(),
                event: Box::new(ProviderSubagentInputEvent::Timeline {
                    id: child_thread_id.to_string(),
                    item: Box::new(item),
                    timestamp,
                }),
            },
        );
    }

    fn emit_completed_provider_subagent_item(
        &mut self,
        thread_id: Option<&str>,
        item_id: Option<&str>,
        item: &AgentTimelineItem,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let Some(t) = thread_id else { return };
        if let (AgentTimelineItem::AssistantMessage { .. }, Some(id)) = (item, item_id) {
            if let Some(streamed) = self.st.pending_agent_messages.get(id).cloned() {
                if let Some(suffix) = missing_final_text_suffix(item, &streamed) {
                    self.emit_provider_subagent_timeline(t, suffix, None, out);
                }
                return;
            }
        }
        if let (AgentTimelineItem::Reasoning { .. }, Some(id)) = (item, item_id) {
            if let Some(streamed) = self.st.pending_reasoning.get(id).map(|v| v.join("")) {
                if let Some(suffix) = missing_final_text_suffix(item, &streamed) {
                    self.emit_provider_subagent_timeline(t, suffix, None, out);
                }
                return;
            }
        }
        self.emit_provider_subagent_timeline(t, item.clone(), None, out);
    }

    fn emit_started_provider_subagent_item(
        &mut self,
        thread_id: Option<&str>,
        item: AgentTimelineItem,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        if let Some(t) = thread_id {
            self.emit_provider_subagent_timeline(t, item, None, out);
        }
    }

    fn handle_sub_agent_child_item_completed(
        &mut self,
        call_id: &str,
        item_id: Option<&str>,
        mut item: AgentTimelineItem,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        self.apply_buffered_delta_text(&mut item, item_id);
        if let Some(id) = item_id.filter(|s| !s.is_empty()) {
            self.upsert_sub_agent_child_item(call_id, id, item);
            self.st.pending_agent_messages.remove(id);
            self.st.pending_reasoning.remove(id);
            self.st.pending_command_output_deltas.remove(id);
            self.st.pending_file_change_output_deltas.remove(id);
        }
        self.emit_sub_agent_activity_update(call_id, None, false, out);
    }

    fn handle_sub_agent_context_compaction_item(
        &mut self,
        call_id: &str,
        item: &Value,
        loading: bool,
        out: &mut Vec<AgentStreamEvent>,
    ) -> bool {
        if item_type(item).as_deref() != Some(CONTEXT_COMPACTION_TYPE) {
            return false;
        }
        if let Some(id) = item
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            self.upsert_sub_agent_child_item(
                call_id,
                id,
                AgentTimelineItem::Compaction {
                    status: if loading {
                        paseo_agent_types::CompactionStatus::Loading
                    } else {
                        paseo_agent_types::CompactionStatus::Completed
                    },
                    trigger: None,
                    pre_tokens: None,
                    extra: Map::new(),
                },
            );
        }
        self.emit_sub_agent_activity_update(call_id, None, false, out);
        true
    }

    // -- persisted history -------------------------------------------------------------------

    /// A `thread/read` answer (`loadPersistedHistory` and the child-thread loop of
    /// `loadPersistedSubAgentHistories`).
    fn on_thread_read_response(
        &mut self,
        thread: Option<&str>,
        result: &Value,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        let cwd = self.cwd();
        let Some(projection) =
            project_thread_history(result, cwd.as_deref(), self.opts.materialize.as_deref())
        else {
            return;
        };
        let is_root = thread.is_none() || thread == self.st.current_thread_id.as_deref();
        if is_root {
            self.st.sub_agent_calls.clear();
            self.st.sub_agent_call_id_by_child_thread_id.clear();
            self.st.pending_sub_agent_notifications.clear();
            self.st.persisted_provider_subagent_events.clear();
            self.st.history_child_threads.clear();
            self.st.loading_persisted_history = true;
            for route in &projection.sub_agent_routes {
                self.register_history_route(route, None, None, out);
            }
            self.st.loading_persisted_history = false;
            // resetCodexUserMessageTurns + remember the history's user messages
            self.st.user_message_turn_indexes.clear();
            self.st.user_message_turn_ids.clear();
            self.st.user_message_provider_turn_ids.clear();
            for entry in &projection.timeline {
                if let AgentTimelineItem::UserMessage { message_id, .. } = &entry.item {
                    self.remember_user_message_turn(
                        message_id.as_deref(),
                        entry.provider_turn_id.as_deref(),
                    );
                }
            }
            let mut timeline = projection.timeline;
            for entry in &mut timeline {
                if let AgentTimelineItem::ToolCall(c) = &entry.item {
                    if c.name == "request_user_input_async" {
                        if let Some(t) = self.async_timeline(&c.call_id) {
                            entry.item = AgentTimelineItem::tool_call(t);
                        }
                    }
                }
            }
            self.st.history_pending = !timeline.is_empty();
            self.st.persisted_history = timeline;
            return;
        }
        // child thread history
        let Some(child) = thread else { return };
        let Some(call_id) = self
            .st
            .sub_agent_call_id_by_child_thread_id
            .get(child)
            .cloned()
        else {
            return;
        };
        self.st.loading_persisted_history = true;
        for entry in &projection.timeline {
            self.emit_provider_subagent_timeline(
                child,
                entry.item.clone(),
                entry.timestamp.clone(),
                out,
            );
        }
        for route in &projection.sub_agent_routes {
            self.register_history_route(route, Some(call_id.clone()), Some(child.to_string()), out);
        }
        self.st.loading_persisted_history = false;
    }

    fn async_timeline(&self, _call_id: &str) -> Option<ToolCallItem> {
        // The async question record only exists for questions received live in this process.
        None
    }

    fn register_history_route(
        &mut self,
        route: &PersistedSubAgentRoute,
        parent_call_id: Option<String>,
        parent_subagent_id: Option<String>,
        out: &mut Vec<AgentStreamEvent>,
    ) {
        if !self
            .st
            .history_child_threads
            .insert(route.child_thread_id.clone())
        {
            return;
        }
        let raw = json!({ "agentThreadId": route.child_thread_id });
        self.register_sub_agent_tool_call(
            &route.tool_call,
            &raw,
            parent_call_id,
            parent_subagent_id,
            out,
        );
    }
}

enum Route {
    Root,
    SubAgent(String),
    Pending(String),
}

fn parent_subagent_id(
    parent_call_id: Option<&str>,
    emitting_thread_id: Option<&str>,
) -> Option<String> {
    parent_call_id.and_then(|_| emitting_thread_id.map(str::to_string))
}

/// `buildMissingFinalTextSuffix`: `None` when nothing is missing; the whole item when the final
/// text does not extend what was streamed.
fn missing_final_text_suffix(
    item: &AgentTimelineItem,
    streamed: &str,
) -> Option<AgentTimelineItem> {
    match item {
        AgentTimelineItem::AssistantMessage { text, message_id } => {
            if !text.starts_with(streamed) {
                return Some(item.clone());
            }
            let suffix = &text[streamed.len()..];
            if suffix.is_empty() {
                return None;
            }
            Some(AgentTimelineItem::AssistantMessage {
                text: suffix.to_string(),
                message_id: message_id.clone(),
            })
        }
        AgentTimelineItem::Reasoning { text } => {
            if !text.starts_with(streamed) {
                return Some(item.clone());
            }
            let suffix = &text[streamed.len()..];
            if suffix.is_empty() {
                return None;
            }
            Some(AgentTimelineItem::Reasoning {
                text: suffix.to_string(),
            })
        }
        _ => None,
    }
}

impl ProviderMapper for CodexMapper {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn feed(&mut self, raw: &Value, env: &mut dyn MapEnv) -> Vec<AgentStreamEvent> {
        let mut out = Vec::new();
        self.feed_line(raw, env, &mut out);
        out
    }

    /// End of input: notifications still held for a turn that never started are delivered
    /// untagged (the session had no foreground turn for them).
    fn finish(&mut self, env: &mut dyn MapEnv) -> Vec<AgentStreamEvent> {
        let mut out = Vec::new();
        self.st.pre_turn = false;
        let buffered = std::mem::take(&mut self.st.buffered);
        for (method, params) in buffered {
            self.handle_notification(&method, &params, env, &mut out);
        }
        out
    }
}
