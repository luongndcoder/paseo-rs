//! Pi mapper: raw Pi RPC events -> `AgentStreamEvent` (spec WP10, `pi/agent.ts` session logic,
//! `pi/history-mapper.ts`, `pi/tool-call-mapper.ts` via `paseo-tool-detail`, `pi/extensions/*`).
//!
//! [`PiMapper`] is the event-handling half of `PiRpcAgentSession`: it owns the turn state machine
//! and the extension host, takes Pi runtime events (and the client actions that change turn
//! state) and returns the stream events. Process I/O, the RPC calls and the usage poller stay in
//! the session layer.
//!
//! `feed` replays a recorded transcript the way the WP0 capture driver does: every line is
//! `{"dir": "in"|"out", "line": {...}}` (or a bare Pi `out` line); `out` lines other than
//! `response` are runtime events, `in` lines `prompt` / `abort` / `extension_ui_response` call
//! [`PiMapper::start_turn`] / [`PiMapper::interrupt`] / [`PiMapper::respond_to_permission`]. The
//! first `get_state` response supplies the session id. A fresh session consumes one uuid before
//! anything else (TS `usageSessionKey = randomUUID()`), which `feed` reproduces on its first call.
//!
//! DEVIATION(pi-usage-poll): `usage_updated` comes from polling `get_session_stats`; that is an
//! RPC concern and not emitted here.
//! DEVIATION(pi-no-turn-probe): the TS session probes `get_state` after a slash-command prompt
//! whose ack carries no `agentInvoked`; the mapper only handles an explicit ack
//! ([`PiMapper::prompt_acked`]) or `prompt_result` event. `completeNoTurnPrompt` runs inline
//! (TS defers it one macrotask).
//! DEVIATION(pi-out-of-band-compaction): `/compact` out-of-band emission is a session-layer
//! concern and is not ported.

mod dialogs;
mod errors;
mod ext;
mod history;
mod items;
mod util;

use paseo_agent_types::AgentStreamEvent;
use paseo_tool_detail::detail::pi::{
    map_tool_detail, parse_tool_args, parse_tool_result, PiToolResult, PiTrackedToolCall,
};
use serde_json::{json, Value};

pub use dialogs::PiCapturedEntry;
pub use ext::{
    ChildSession, DialogMapping, ExtMapping, ExtOutput, ExtSession, ExtToolCall, PiExtensionHost,
};
pub use history::{
    map_pi_history, user_message_text, PiCapturedUserMessageEntry, PiChildSessionMapper,
    PiHistoryMapper,
};

use dialogs::{
    build_ui_response, map_ui_request_to_permission, parse_captured_entries, parse_marker_payload,
};
use errors::{is_aborted_terminal_response, latest_error_message};
use items::{
    custom_message_item, decode_event, tool_call_item_value, tracked_args, tracked_tool_name,
};
use util::*;

use crate::{MapEnv, ProviderMapper};

const PI_PROVIDER: &str = "pi";
const ENTRY_CAPTURE_MARKER: &str = "PASEO_ENTRY_CAPTURE";
const SUBMITTED_USER_ENTRY_MARKER: &str = "PASEO_SUBMITTED_USER_ENTRY";
const COMMAND_RESULT_MARKER: &str = "PASEO_COMMAND_RESULT";

fn is_thinking_level(v: &str) -> bool {
    matches!(
        v,
        "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
    )
}

// ---- mapper -------------------------------------------------------------------------------

struct Interrupting {
    seq: u64,
    turn_id: Option<String>,
    error: Option<String>,
}

/// Returned by [`PiMapper::begin_interrupt`].
#[derive(Debug, Clone)]
pub struct InterruptToken {
    seq: u64,
    turn_id: Option<String>,
    interrupting: bool,
}

/// Result of an extension command or entry capture marker, for the session layer's waiters.
#[derive(Debug, Clone, PartialEq)]
pub enum ExtensionResult {
    Resolved { request_id: String, result: Value },
    Rejected { request_id: String, error: String },
}

/// Pi runtime events -> stream events. See the module docs.
#[derive(Default)]
pub struct PiMapper {
    initialized: bool,
    state_seen: bool,
    session_id: String,
    out: Vec<AgentStreamEvent>,
    host: PiExtensionHostCell,
    active_tool_calls: Vec<(String, PiTrackedToolCall)>,
    pending_ui_requests: Vec<(String, Value)>,
    ui_responses: Vec<(String, Value)>,
    extension_results: Vec<ExtensionResult>,
    active_turn_id: Option<String>,
    active_client_message_id: Option<String>,
    active_assistant_message_id: Option<String>,
    active_turn_started: bool,
    active_turn_started_emitted: bool,
    pending_settled_messages: Option<Vec<Value>>,
    active_no_turn_prompt_text: Option<String>,
    pending_no_turn_outputs: Vec<(String, String)>,
    active_prompt_request_id: Option<String>,
    pending_prompt_results: Vec<(String, bool)>,
    pending_steer_submissions: Vec<(String, Option<String>)>,
    interrupting: Option<Interrupting>,
    interrupt_seq: u64,
    context_user_entries: Vec<PiCapturedEntry>,
    tree_user_entries: Vec<PiCapturedEntry>,
}

/// `Default` wrapper so `PiMapper` can derive it.
#[derive(Default)]
struct PiExtensionHostCell(PiExtensionHost);

impl PiMapper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_session_id(&mut self, id: &str) {
        self.session_id = id.to_string();
        self.state_seen = true;
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Pending permission requests (`getPendingPermissions`), in request order.
    pub fn pending_permissions(&self) -> Vec<&Value> {
        self.pending_ui_requests.iter().map(|(_, r)| r).collect()
    }

    /// `extension_ui_response` payloads to send to Pi (`respondToExtensionUiRequest`).
    pub fn take_ui_responses(&mut self) -> Vec<(String, Value)> {
        std::mem::take(&mut self.ui_responses)
    }

    /// Child session files reported by extension mappings (see `ext` module docs).
    pub fn take_child_sessions(&mut self) -> Vec<ChildSession> {
        self.host.0.take_child_sessions()
    }

    /// Extension command results and entry captures since the last call.
    pub fn take_extension_results(&mut self) -> Vec<ExtensionResult> {
        std::mem::take(&mut self.extension_results)
    }

    /// User entries of the current branch, as captured by `PASEO_ENTRY_CAPTURE`.
    pub fn context_user_entries(&self) -> &[PiCapturedEntry] {
        &self.context_user_entries
    }

    pub fn tree_user_entries(&self) -> &[PiCapturedEntry] {
        &self.tree_user_entries
    }

    fn ensure_init(&mut self, env: &mut dyn MapEnv) {
        if !self.initialized {
            self.initialized = true;
            // PARITY-QUIRK(pi-session-key-uuid): `PiRpcAgentSession` draws a uuid for its usage
            // session key at construction, so replay ids start at the second uuid.
            env.uuid();
        }
    }

    fn take_events(&mut self) -> Vec<AgentStreamEvent> {
        std::mem::take(&mut self.out)
    }

    fn emit(&mut self, event: Value) {
        if let Some(e) = decode_event(&event) {
            self.out.push(e);
        }
    }

    fn emit_timeline(&mut self, turn_id: Option<&str>, item: Value) {
        let mut ev = json!({"type": "timeline", "provider": PI_PROVIDER, "item": item});
        if let Some(t) = turn_id {
            ev["turnId"] = json!(t);
        }
        self.emit(ev);
    }

    fn with_turn(mut ev: Value, turn_id: Option<&str>) -> Value {
        if let Some(t) = turn_id {
            ev["turnId"] = json!(t);
        }
        ev
    }

    fn turn_id(&self) -> Option<String> {
        self.active_turn_id.clone()
    }

    /// The block repeated in `startTurn`'s catch, `interrupt`, `completeTurn`.
    fn reset_turn(&mut self) {
        self.active_turn_id = None;
        self.active_client_message_id = None;
        self.active_turn_started = false;
        self.active_turn_started_emitted = false;
        self.pending_settled_messages = None;
        self.active_assistant_message_id = None;
        self.pending_steer_submissions.clear();
        self.clear_no_turn_buffers();
    }

    fn clear_no_turn_buffers(&mut self) {
        self.active_no_turn_prompt_text = None;
        self.active_prompt_request_id = None;
        self.pending_no_turn_outputs.clear();
    }

    fn set_pending_request(&mut self, request: Value) {
        let id = request
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if let Some(slot) = self.pending_ui_requests.iter_mut().find(|(k, _)| *k == id) {
            slot.1 = request;
        } else {
            self.pending_ui_requests.push((id, request));
        }
    }

    // ---- client actions ----

    /// `startTurn`. Fails when a turn is already active. Returns the new turn id.
    pub fn start_turn(
        &mut self,
        text: &str,
        client_message_id: Option<&str>,
        env: &mut dyn MapEnv,
    ) -> Result<String, String> {
        self.ensure_init(env);
        if self.active_turn_id.is_some() {
            return Err("A Pi turn is already active".into());
        }
        let turn_id = env.uuid();
        self.active_turn_id = Some(turn_id.clone());
        self.active_client_message_id = client_message_id.map(str::to_string);
        self.active_assistant_message_id = None;
        self.active_turn_started = false;
        self.active_turn_started_emitted = false;
        self.pending_settled_messages = None;
        self.active_prompt_request_id = None;
        self.pending_steer_submissions.clear();
        self.clear_no_turn_buffers();
        self.active_no_turn_prompt_text = Some(text.to_string());
        Ok(turn_id)
    }

    /// The `prompt` RPC ack (`PiPromptAck`) for the active turn.
    pub fn prompt_acked(&mut self, request_id: Option<&str>, agent_invoked: Option<bool>) {
        let Some(turn_id) = self.turn_id() else {
            return;
        };
        self.active_prompt_request_id = request_id.map(str::to_string);
        let correlated = request_id.and_then(|id| {
            let pos = self
                .pending_prompt_results
                .iter()
                .position(|(k, _)| k == id)?;
            Some(self.pending_prompt_results.remove(pos).1)
        });
        if correlated.or(agent_invoked) == Some(false) {
            self.complete_no_turn_prompt(&turn_id);
        }
    }

    /// The prompt RPC failed (`startTurn`'s catch). `abort` is an abort-style error.
    pub fn prompt_failed(&mut self, error: &str, abort: bool) {
        let Some(turn_id) = self.turn_id() else {
            return;
        };
        self.reset_turn();
        if abort {
            self.emit(json!({
                "type": "turn_canceled", "provider": PI_PROVIDER,
                "turnId": turn_id, "reason": error,
            }));
        } else {
            self.emit(json!({
                "type": "turn_failed", "provider": PI_PROVIDER,
                "turnId": turn_id, "error": error,
            }));
        }
    }

    /// A steer RPC was accepted by Pi (`pendingSteerSubmissions`).
    pub fn steer_accepted(&mut self, text: &str, client_message_id: Option<&str>) {
        self.pending_steer_submissions
            .push((text.to_string(), client_message_id.map(str::to_string)));
    }

    pub fn begin_interrupt(&mut self) -> InterruptToken {
        let turn_id = self.turn_id();
        let active = self.active_turn_id.is_some() || self.active_turn_started;
        self.interrupt_seq += 1;
        let seq = self.interrupt_seq;
        self.interrupting = active.then(|| Interrupting {
            seq,
            turn_id: turn_id.clone(),
            error: None,
        });
        InterruptToken {
            seq,
            turn_id,
            interrupting: active,
        }
    }

    fn is_current(&self, token: &InterruptToken) -> bool {
        token.interrupting
            && self
                .interrupting
                .as_ref()
                .is_some_and(|i| i.seq == token.seq)
    }

    /// Settle an interrupt after `clear_queue` + `abort`. `Err` is the RPC failure message.
    pub fn finish_interrupt(&mut self, token: InterruptToken, rpc: Result<(), String>) {
        if let Err(_message) = rpc {
            let terminal_error = if self.is_current(&token) {
                self.interrupting.as_mut().and_then(|i| i.error.take())
            } else {
                None
            };
            if self.is_current(&token) {
                self.interrupting = None;
            }
            if let Some(error) = terminal_error {
                self.reset_turn();
                let mut ev =
                    json!({"type": "turn_failed", "provider": PI_PROVIDER, "error": error});
                if let Some(t) = &token.turn_id {
                    ev["turnId"] = json!(t);
                }
                self.emit(ev);
            }
            return;
        }
        if self.is_current(&token)
            && (self.active_turn_id.is_some() || self.active_turn_started)
            && self.active_turn_id == token.turn_id
        {
            self.reset_turn();
            let mut ev = json!({
                "type": "turn_canceled", "provider": PI_PROVIDER, "reason": "interrupted",
            });
            if let Some(t) = &token.turn_id {
                ev["turnId"] = json!(t);
            }
            self.emit(ev);
        }
        if self.is_current(&token) {
            self.interrupting = None;
        }
    }

    /// `interrupt()` when the RPCs succeed.
    pub fn interrupt(&mut self) {
        let token = self.begin_interrupt();
        self.finish_interrupt(token, Ok(()));
    }

    /// `respondToPermission`. `response` is `{behavior: "allow" | "deny", ...}`.
    pub fn respond_to_permission(
        &mut self,
        request_id: &str,
        response: &Value,
    ) -> Result<(), String> {
        let Some(pos) = self
            .pending_ui_requests
            .iter()
            .position(|(k, _)| k == request_id)
        else {
            return Err(format!(
                "No pending permission request with id '{request_id}'"
            ));
        };
        let (_, request) = self.pending_ui_requests.remove(pos);
        let replies = self
            .host
            .0
            .respond_to_permission(&request, response)
            .unwrap_or_else(|| {
                vec![(
                    request_id.to_string(),
                    build_ui_response(&request, response),
                )]
            });
        self.ui_responses.extend(replies);
        let turn_id = self.turn_id();
        self.emit(Self::with_turn(
            json!({
                "type": "permission_resolved", "provider": PI_PROVIDER,
                "requestId": request_id, "resolution": response,
            }),
            turn_id.as_deref(),
        ));
        Ok(())
    }

    // ---- no-turn prompts ----

    fn complete_no_turn_prompt(&mut self, turn_id: &str) {
        if self.active_turn_id.as_deref() != Some(turn_id) || self.active_turn_started {
            return;
        }
        self.emit_buffered_no_turn_outputs(turn_id);
        self.complete_turn(Some(turn_id.to_string()), &[]);
    }

    fn emit_buffered_no_turn_outputs(&mut self, turn_id: &str) {
        let prompt_text = self.active_no_turn_prompt_text.take();
        let outputs: Vec<String> = self
            .pending_no_turn_outputs
            .iter()
            .filter(|(t, _)| t == turn_id)
            .map(|(_, m)| m.clone())
            .collect();
        self.clear_no_turn_buffers();
        if let Some(text) = prompt_text.filter(|t| !t.is_empty()) {
            let mut item = json!({"type": "user_message", "text": text});
            if let Some(c) = self.active_client_message_id.clone() {
                item["clientMessageId"] = json!(c);
            }
            self.emit_timeline(Some(turn_id), item);
        }
        for message in outputs {
            self.emit_timeline(
                Some(turn_id),
                json!({"type": "assistant_message", "text": message}),
            );
        }
    }

    // ---- runtime events ----

    /// `handleRuntimeEvent` for one Pi runtime event (an `out` line other than a response).
    pub fn handle_runtime_event(&mut self, event: &Value, env: &mut dyn MapEnv) {
        self.ensure_init(env);
        let ty = event.get("type").and_then(Value::as_str);
        if ty == Some("extension_ui_request") && event.get("id").is_some_and(Value::is_string) {
            self.handle_extension_ui_request(event);
            return;
        }
        if ty == Some("process_exit") && event.get("error").is_some_and(Value::is_string) {
            let error = event["error"].as_str().unwrap_or("").to_string();
            self.handle_process_exit(&error);
            return;
        }
        if ty == Some("command_output") {
            self.handle_command_output(event.get("text"));
            return;
        }
        if ty == Some("prompt_result") {
            self.handle_prompt_result(event);
            return;
        }
        if is_session_event(event) {
            self.handle_session_event(event, env);
        }
    }

    fn handle_prompt_result(&mut self, event: &Value) {
        let request_id = optional_string(event.get("id")).filter(|s| !s.is_empty());
        let agent_invoked = event.get("agentInvoked").and_then(Value::as_bool);
        let (Some(request_id), Some(agent_invoked)) = (request_id, agent_invoked) else {
            return;
        };
        if self.active_prompt_request_id.as_deref() == Some(request_id)
            && !agent_invoked
            && self.active_turn_id.is_some()
        {
            if let Some(turn_id) = self.turn_id() {
                self.complete_no_turn_prompt(&turn_id);
            }
        } else if self.active_prompt_request_id.is_none() {
            self.pending_prompt_results
                .push((request_id.to_string(), agent_invoked));
        }
    }

    fn handle_process_exit(&mut self, error: &str) {
        self.interrupting = None;
        if self.active_turn_id.is_none() && !self.active_turn_started {
            return;
        }
        let turn_id = self.turn_id();
        let assistant = self.active_assistant_message_id.clone();
        self.reset_turn();
        // TS leaves `activeAssistantMessageId` alone on process exit.
        self.active_assistant_message_id = assistant;
        self.emit(Self::with_turn(
            json!({"type": "turn_failed", "provider": PI_PROVIDER, "error": error}),
            turn_id.as_deref(),
        ));
    }

    fn handle_command_output(&mut self, text: Option<&Value>) {
        let Some(turn_id) = self.turn_id() else {
            return;
        };
        let stripped = strip_ansi(optional_string(text).unwrap_or(""));
        let text = paseo_tool_detail::js::js_trim(&stripped).to_string();
        if text.is_empty() {
            return;
        }
        if !self.active_turn_started {
            self.pending_no_turn_outputs.push((turn_id, text));
            return;
        }
        self.emit_timeline(
            Some(&turn_id),
            json!({"type": "assistant_message", "text": text}),
        );
    }

    fn handle_extension_ui_request(&mut self, event: &Value) {
        let message = optional_string(event.get("message")).filter(|m| !m.is_empty());
        let method = event.get("method").and_then(Value::as_str);
        if method == Some("notify") {
            if let Some(message) = message {
                let turn_id = self.turn_id();
                if let Some(output) = self.host.0.map_runtime_notification(message) {
                    self.emit_extension_output(&output, turn_id.as_deref());
                    return;
                }
                if self.handle_submitted_user_entry_marker(message)
                    || self.handle_entry_capture_marker(message)
                    || self.handle_command_result_marker(message)
                {
                    return;
                }
                let level = event
                    .get("notifyType")
                    .and_then(Value::as_str)
                    .filter(|l| matches!(*l, "info" | "warning" | "error"))
                    .unwrap_or("info");
                self.emit_timeline(
                    turn_id.as_deref(),
                    json!({"type": "notification", "level": level, "message": message}),
                );
                return;
            }
        }
        let request = match self.host.0.map_dialog(event, PI_PROVIDER) {
            Some(DialogMapping::Response(response)) => {
                let id = event
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.ui_responses.push((id, response));
                return;
            }
            Some(DialogMapping::Deferred) => return,
            Some(DialogMapping::Permission(request)) => request,
            None => match map_ui_request_to_permission(event, PI_PROVIDER, "Pi") {
                Some(r) => r,
                None => return,
            },
        };
        self.set_pending_request(request.clone());
        let turn_id = self.turn_id();
        self.emit(Self::with_turn(
            json!({"type": "permission_requested", "provider": PI_PROVIDER, "request": request}),
            turn_id.as_deref(),
        ));
    }

    fn handle_submitted_user_entry_marker(&mut self, message: &str) -> bool {
        let Some(payload) = parse_marker_payload(message, SUBMITTED_USER_ENTRY_MARKER) else {
            return false;
        };
        let entries = parse_captured_entries(Some(&Value::Array(
            payload.get("entry").cloned().into_iter().collect(),
        )));
        let Some(entry) = entries.into_iter().next() else {
            return true;
        };
        let pos = self
            .pending_steer_submissions
            .iter()
            .position(|(t, _)| *t == entry.text);
        let client_message_id = match pos {
            Some(p) => self.pending_steer_submissions.remove(p).1,
            None => self.active_client_message_id.clone(),
        };
        let mut item = json!({"type": "user_message", "text": entry.text, "messageId": entry.id});
        if let Some(c) = client_message_id.filter(|c| !c.is_empty()) {
            item["clientMessageId"] = json!(c);
        }
        let turn_id = self.turn_id();
        self.emit_timeline(turn_id.as_deref(), item);
        true
    }

    fn handle_entry_capture_marker(&mut self, message: &str) -> bool {
        let Some(payload) = parse_marker_payload(message, ENTRY_CAPTURE_MARKER) else {
            return false;
        };
        self.context_user_entries = parse_captured_entries(payload.get("contextEntries"));
        self.tree_user_entries = parse_captured_entries(payload.get("treeEntries"));
        if let Some(Value::String(id)) = payload.get("requestId") {
            self.extension_results.push(ExtensionResult::Resolved {
                request_id: id.clone(),
                result: Value::Null,
            });
        }
        true
    }

    fn handle_command_result_marker(&mut self, message: &str) -> bool {
        let Some(payload) = parse_marker_payload(message, COMMAND_RESULT_MARKER) else {
            return false;
        };
        let Some(Value::String(request_id)) = payload.get("requestId") else {
            return true;
        };
        if payload.get("ok") == Some(&Value::Bool(true)) {
            self.extension_results.push(ExtensionResult::Resolved {
                request_id: request_id.clone(),
                result: payload.get("result").cloned().unwrap_or(Value::Null),
            });
            return true;
        }
        let error = payload
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Pi extension command failed");
        self.extension_results.push(ExtensionResult::Rejected {
            request_id: request_id.clone(),
            error: error.to_string(),
        });
        true
    }

    fn emit_extension_output(&mut self, output: &ExtOutput, turn_id: Option<&str>) {
        for event in &output.events {
            let is_timeline = event.get("type").and_then(Value::as_str) == Some("timeline");
            let ev = if is_timeline {
                Self::with_turn(event.clone(), turn_id)
            } else {
                event.clone()
            };
            self.emit(ev);
        }
    }

    fn handle_session_event(&mut self, event: &Value, env: &mut dyn MapEnv) {
        let turn_id = self.turn_id();
        let ty = event.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "agent_end" | "agent_settled" => self.handle_turn_boundary(event, ty),
            "thinking_level_changed" => {
                self.emit(json!({
                    "type": "thinking_option_changed", "provider": PI_PROVIDER,
                    "thinkingOptionId": event.get("level").cloned().unwrap_or(Value::Null),
                }));
            }
            "agent_start" => {
                self.active_turn_started = true;
                self.clear_no_turn_buffers();
                self.emit(json!({
                    "type": "thread_started", "provider": PI_PROVIDER,
                    "sessionId": self.session_id,
                }));
            }
            "turn_start" => {
                self.active_turn_started = true;
                self.clear_no_turn_buffers();
                if self.active_turn_started_emitted {
                    return;
                }
                self.active_turn_started_emitted = true;
                self.emit(Self::with_turn(
                    json!({"type": "turn_started", "provider": PI_PROVIDER}),
                    turn_id.as_deref(),
                ));
            }
            "message_start" => {
                if event
                    .get("message")
                    .and_then(|m| m.get("role"))
                    .and_then(Value::as_str)
                    == Some("assistant")
                {
                    self.active_assistant_message_id =
                        truthy_str(event.get("message").and_then(|m| m.get("responseId")))
                            .map(str::to_string);
                }
            }
            "message_end" => self.handle_message_end(event, turn_id.as_deref(), env),
            "message_update" => self.handle_message_update(event, turn_id.as_deref(), env),
            "tool_execution_start" => self.handle_tool_execution_start(event),
            "tool_execution_update" => {
                let id = event
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let Some(tool_call) = self.find_tool_call(id).cloned() else {
                    return;
                };
                let partial = parse_tool_result(event.get("partialResult"));
                self.emit_tool_call_event(id, &tool_call, "running", &partial, Some(Value::Null));
            }
            "tool_execution_end" => self.handle_tool_execution_end(event),
            "compaction_start" | "compaction_end" => {
                let status = if ty == "compaction_start" {
                    "loading"
                } else {
                    "completed"
                };
                let trigger = if event.get("reason").and_then(Value::as_str) == Some("manual") {
                    "manual"
                } else {
                    "auto"
                };
                self.emit_timeline(
                    turn_id.as_deref(),
                    json!({"type": "compaction", "status": status, "trigger": trigger}),
                );
            }
            "auto_retry_start" => {
                let message = format!(
                    "Provider retry (attempt {}): {}",
                    js_interp(event.get("attempt")),
                    js_interp(event.get("errorMessage")),
                );
                self.emit_timeline(
                    turn_id.as_deref(),
                    json!({"type": "error", "message": message}),
                );
            }
            _ => {}
        }
    }

    fn find_tool_call(&self, id: &str) -> Option<&PiTrackedToolCall> {
        self.active_tool_calls
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, v)| v)
    }

    fn handle_turn_boundary(&mut self, event: &Value, ty: &str) {
        if self.active_turn_id.is_none() && !self.active_turn_started {
            return;
        }
        let turn_id = self.turn_id();
        let messages = || -> Vec<Value> {
            event
                .get("messages")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        if ty == "agent_end" {
            // COMPAT(piAgentSettled): TS removes this after 2027-02-21 once the Pi floor emits
            // agent_settled and willRetry.
            if event.get("willRetry").is_none() {
                self.complete_turn(turn_id, &messages());
                return;
            }
            self.pending_settled_messages = Some(messages());
            return;
        }
        let settled = self.pending_settled_messages.clone().unwrap_or_default();
        self.complete_turn(turn_id, &settled);
    }

    fn handle_tool_execution_start(&mut self, event: &Value) {
        let id = event
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = event.get("toolName").and_then(Value::as_str).unwrap_or("");
        let tool_call = parse_tool_args(name, event.get("args"));
        match self.active_tool_calls.iter_mut().find(|(k, _)| *k == id) {
            Some(slot) => slot.1 = tool_call.clone(),
            None => self.active_tool_calls.push((id.clone(), tool_call.clone())),
        }
        let request = self.host.0.on_tool_start(
            &ExtToolCall {
                call_id: id.clone(),
                tool_name: event
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                args: tracked_args(&tool_call),
                status: "running",
                result: PiToolResult::Null,
            },
            PI_PROVIDER,
        );
        if let Some(request) = request {
            self.set_pending_request(request.clone());
            let turn_id = self.turn_id();
            self.emit(Self::with_turn(
                json!({"type": "permission_requested", "provider": PI_PROVIDER, "request": request}),
                turn_id.as_deref(),
            ));
        }
        self.emit_tool_call_event(
            &id,
            &tool_call,
            "running",
            &PiToolResult::Null,
            Some(Value::Null),
        );
    }

    fn handle_tool_execution_end(&mut self, event: &Value) {
        let id = event
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = event.get("toolName").and_then(Value::as_str).unwrap_or("");
        let tool_call = match self.active_tool_calls.iter().position(|(k, _)| *k == id) {
            Some(pos) => self.active_tool_calls.remove(pos).1,
            None => parse_tool_args(name, None),
        };
        let result = parse_tool_result(event.get("result"));
        let is_error = event.get("isError").is_some_and(|v| match v {
            Value::Bool(b) => *b,
            Value::Null => false,
            Value::String(s) => !s.is_empty(),
            Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
            _ => true,
        });
        let status = if is_error { "failed" } else { "completed" };
        let error = if is_error {
            event.get("result").cloned()
        } else {
            Some(Value::Null)
        };
        self.host.0.on_tool_end(&ExtToolCall {
            call_id: id.clone(),
            tool_name: tracked_tool_name(&tool_call),
            args: tracked_args(&tool_call),
            status,
            result: result.clone(),
        });
        self.emit_tool_call_event(&id, &tool_call, status, &result, error);
    }

    fn emit_tool_call_event(
        &mut self,
        call_id: &str,
        tool_call: &PiTrackedToolCall,
        status: &'static str,
        result: &PiToolResult,
        error: Option<Value>,
    ) {
        let turn_id = self.turn_id();
        let tool_name = tracked_tool_name(tool_call);
        let mapping = self.host.0.map_tool_call(&ExtToolCall {
            call_id: call_id.to_string(),
            tool_name: tool_name.clone(),
            args: tracked_args(tool_call),
            status,
            result: result.clone(),
        });
        let detail = mapping
            .as_ref()
            .and_then(|m| m.detail.clone())
            .unwrap_or_else(|| map_tool_detail(tool_call, Some(result)));
        let name = mapping
            .as_ref()
            .and_then(|m| m.name.clone())
            .unwrap_or(tool_name);
        let item = tool_call_item_value(call_id, &name, status, &detail, error);
        self.emit_timeline(turn_id.as_deref(), item);
        if let Some(mapping) = &mapping {
            self.emit_extension_output(mapping, turn_id.as_deref());
        }
    }

    fn handle_message_update(
        &mut self,
        event: &Value,
        turn_id: Option<&str>,
        env: &mut dyn MapEnv,
    ) {
        let message = event.get("message").filter(|m| !m.is_null());
        if let Some(m) = message {
            if m.get("role").and_then(Value::as_str) != Some("assistant") {
                return;
            }
        }
        let inner = event.get("assistantMessageEvent");
        let ty = inner.and_then(|i| i.get("type")).and_then(Value::as_str);
        let delta = inner
            .and_then(|i| i.get("delta"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match ty {
            Some("text_delta") => {
                // Pi-compatible runtimes may emit updates without a preceding message_start.
                if self.active_assistant_message_id.is_none() {
                    let id = truthy_str(message.and_then(|m| m.get("responseId")))
                        .map(str::to_string)
                        .unwrap_or_else(|| env.uuid());
                    self.active_assistant_message_id = Some(id);
                }
                let message_id = self.active_assistant_message_id.clone().unwrap_or_default();
                self.emit_timeline(
                    turn_id,
                    json!({"type": "assistant_message", "text": delta, "messageId": message_id}),
                );
            }
            Some("thinking_delta") => {
                self.emit_timeline(turn_id, json!({"type": "reasoning", "text": delta}));
            }
            _ => {}
        }
    }

    fn handle_message_end(&mut self, event: &Value, turn_id: Option<&str>, env: &mut dyn MapEnv) {
        let Some(message) = event.get("message") else {
            return;
        };
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => self.active_assistant_message_id = None,
            Some("custom") => {
                if let Some(output) = self.host.0.map_custom_message(message) {
                    self.emit_extension_output(&output, turn_id);
                }
                let text = user_message_text(message.get("content"));
                if message.get("display") != Some(&Value::Bool(false)) && !text.is_empty() {
                    // PARITY-QUIRK(pi-custom-call-id-live-vs-history): the live path names the
                    // row `pi-custom-<uuid>`, history replay `pi-custom-<n>`.
                    let call_id = format!("{PI_PROVIDER}-custom-{}", env.uuid());
                    self.emit_timeline(turn_id, custom_message_item(message, &text, &call_id));
                }
                if !self.active_turn_started {
                    self.complete_turn(turn_id.map(str::to_string), &[]);
                }
            }
            _ => {}
        }
    }

    fn complete_turn(&mut self, turn_id: Option<String>, messages: &[Value]) {
        let error_message = latest_error_message(messages);
        if let Some(i) = &mut self.interrupting {
            if i.turn_id == turn_id
                && (error_message.is_some() || is_aborted_terminal_response(messages))
            {
                i.error = Some(error_message.unwrap_or_else(|| "Pi turn failed".into()));
                return;
            }
        }
        self.interrupting = None;
        self.reset_turn();
        if let Some(error) = error_message.filter(|e| !e.is_empty()) {
            self.emit(Self::with_turn(
                json!({"type": "turn_failed", "provider": PI_PROVIDER, "error": error}),
                turn_id.as_deref(),
            ));
            return;
        }
        self.emit(Self::with_turn(
            json!({"type": "turn_completed", "provider": PI_PROVIDER}),
            turn_id.as_deref(),
        ));
    }

    // ---- transcript replay ----

    fn handle_in_line(&mut self, line: &Value, env: &mut dyn MapEnv) {
        match line.get("type").and_then(Value::as_str) {
            Some("prompt") => {
                let message = match line.get("message") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => paseo_tool_detail::js::js_to_string(other),
                    None => "undefined".into(),
                };
                // A failed start (turn already active) is the TS driver's thrown error.
                let _ = self.start_turn(&message, None, env);
            }
            Some("abort") => self.interrupt(),
            Some("extension_ui_response") => {
                // PARITY-QUIRK(pi-answer-first-pending-permission): an `extension_ui_response`
                // in a capture answers the first pending request when its id matches none (what
                // the golden driver did); hosts answer by id through the typed API.
                let target = {
                    let want = line.get("id").and_then(Value::as_str);
                    self.pending_ui_requests
                        .iter()
                        .find(|(k, _)| Some(k.as_str()) == want)
                        .or_else(|| self.pending_ui_requests.first())
                        .map(|(k, _)| k.clone())
                };
                if let Some(id) = target {
                    let deny = line.get("cancelled") == Some(&Value::Bool(true))
                        || line.get("confirmed") == Some(&Value::Bool(false));
                    let response = if deny {
                        json!({"behavior": "deny", "message": "denied in fixture"})
                    } else {
                        json!({"behavior": "allow"})
                    };
                    let _ = self.respond_to_permission(&id, &response);
                }
            }
            _ => {}
        }
    }

    fn handle_out_line(&mut self, line: &Value, env: &mut dyn MapEnv) {
        if line.get("type").and_then(Value::as_str) == Some("response") {
            if !self.state_seen && line.get("command").and_then(Value::as_str) == Some("get_state")
            {
                if let Some(id) = line
                    .get("data")
                    .and_then(|d| d.get("sessionId"))
                    .and_then(Value::as_str)
                {
                    self.set_session_id(id);
                }
            }
            return;
        }
        self.handle_runtime_event(line, env);
    }
}

fn is_session_event(event: &Value) -> bool {
    match event.get("type").and_then(Value::as_str) {
        Some("thinking_level_changed") => event
            .get("level")
            .and_then(Value::as_str)
            .is_some_and(is_thinking_level),
        Some(
            "agent_start"
            | "turn_start"
            | "message_start"
            | "message_end"
            | "message_update"
            | "tool_execution_start"
            | "tool_execution_update"
            | "tool_execution_end"
            | "compaction_start"
            | "compaction_end"
            | "agent_end"
            | "agent_settled"
            | "auto_retry_start",
        ) => true,
        _ => false,
    }
}

impl ProviderMapper for PiMapper {
    fn provider(&self) -> &'static str {
        PI_PROVIDER
    }

    fn feed(&mut self, raw: &Value, env: &mut dyn MapEnv) -> Vec<AgentStreamEvent> {
        self.ensure_init(env);
        let dir = raw.get("dir").and_then(Value::as_str);
        match (dir, raw.get("line")) {
            (Some("in"), Some(line)) => self.handle_in_line(line, env),
            (Some("out"), Some(line)) => self.handle_out_line(line, env),
            (None, _) if raw.get("type").is_some() => self.handle_out_line(raw, env),
            _ => {}
        }
        self.take_events()
    }
}
