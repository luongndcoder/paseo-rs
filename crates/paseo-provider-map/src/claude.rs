//! Claude mapper: Claude Code stream-json frames -> `AgentStreamEvent`s.
//!
//! Port of the translation seam of `ClaudeAgentSession` (`claude/agent.ts`): the query pump
//! (`routeSdkMessageFromPump`), `TimelineAssembler`, tool-use cache and partial tool input,
//! context usage, task state, background task notifications, the foreground/autonomous turn
//! bookkeeping, the permission request flow and history conversion.
//!
//! Input is one capture line per `feed` call, the format of `crates/paseo-claude/fixtures`:
//! `{argv, env_keys}` header and `{exit}` trailer (ignored) or `{dir, line}` where `in` lines are
//! what the daemon wrote to the CLI (user prompt -> `startTurn`, `interrupt` control request,
//! permission `control_response` -> `respondToPermission`) and `out` lines are what the CLI wrote
//! (SDK messages are pumped, `can_use_tool` requests call the permission handler). `finish`
//! closes the session the way the golden driver did.
//!
//! PARITY-QUIRK(claude-session-key-uuid): `ClaudeAgentSession` draws one uuid for its usage
//! session key at construction, so the first uuid a replay hands out is burned on the first
//! `feed` and the user message id is the second.
//!
//! PARITY-QUIRK(claude-tool-input-shallow-equality): streamed tool input is only skipped as
//! unchanged when every value is a primitive (`left[key] === right[key]`), so nested objects
//! re-emit the running tool call on every delta.
//!
//! PARITY-QUIRK(claude-answer-first-pending-permission): a permission `control_response` in a
//! capture answers the first pending request, whatever its `request_id` (the golden driver did).
//!
//! DEVIATION(claude-subagent-replay-unmapped): live subagent frames, the task protocol and the
//! sidechain tracker are ported (`subagents.rs`, `sidechain.rs`). On-disk subagent replay
//! (`subagents/replay-source.ts`) and hook-fed effort are not.
//!
//! DEVIATION(claude-turn-id-on-untyped-events): `AgentStreamEvent::ThreadStarted` and `ModeChanged`
//! carry no `turn_id`, so the runtime turn tag TS puts on them is not represented.
//!
//! DEVIATION(claude-tool-result-image-files): tool-result images are written to a private temp
//! dir by the mapper (as TS does) and rendered as `file://` markdown; Windows path forms of the
//! markdown source are not ported.
//!
//! DEVIATION(claude-rewind-command-unmapped): `/rewind` goes through the Agent SDK, which a mapper
//! cannot call; it is mapped like any other prompt.

mod assembler;
mod blocks;
mod history;
mod images;
mod models;
mod partial_json;
mod permission_flow;
mod permissions;
mod sidechain;
mod subagents;
mod task_notification;
mod task_state;
mod text;
mod usage;

use std::collections::{HashMap, HashSet};

use paseo_agent_types::AgentStreamEvent;
use serde_json::{json, Map, Value};

use crate::{MapEnv, ProviderMapper};
use assembler::TimelineAssembler;
use permissions::{default_modes_value, is_permission_mode};
use sidechain::{SidechainCtx, SidechainTracker};
use subagents::{fold_observations, Observation, TaskProtocolSource};
use task_state::TaskState;
use text::{first_trimmed, js_truthy, obj_get, str_of};
use usage::ContextUsageState;

pub use history::HistoryEntry;

/// SDK message types the golden driver pushed into the replay query.
const SDK_MESSAGE_TYPES: [&str; 9] = [
    "system",
    "assistant",
    "user",
    "stream_event",
    "result",
    "rate_limit_event",
    "command_lifecycle",
    "tool_progress",
    "auth_status",
];

/// Cached `tool_use` block (`ToolUseCacheEntry`).
#[derive(Debug, Clone)]
struct ToolEntry {
    id: String,
    name: String,
    server: String,
    started: bool,
    files: Vec<(String, String)>,
    input: Option<Value>,
}

#[derive(Debug, Clone)]
struct PendingPermission {
    request: Value,
}

#[derive(Debug)]
pub struct ClaudeMapper {
    cwd: Option<String>,
    out: Vec<Value>,
    session_key_burned: bool,

    claude_session_id: Option<String>,
    current_mode: String,
    plan_resume_mode: Option<String>,
    last_runtime_model: Option<String>,

    next_turn_ordinal: u64,
    active_fg: Option<String>,
    autonomous: Option<String>,
    /// `cancelCurrentTurn`: the SDK user message id of the foreground turn it belongs to.
    cancel_current_turn: Option<Option<String>>,
    main_turn_in_flight: bool,
    pending_interrupt_abort: bool,
    unstarted_message_uuids: HashSet<String>,
    foreground_has_visible_activity: bool,
    active_turn_has_assistant_text: bool,
    compaction_marker_open: bool,
    compacting: bool,
    closed: bool,

    emitted_user_message_ids: HashSet<String>,
    tool_use_cache: Vec<ToolEntry>,
    tool_use_index_to_id: HashMap<i64, String>,
    tool_use_input_buffers: HashMap<String, String>,
    subagent_handback_ids: HashSet<String>,

    assembler: TimelineAssembler,
    context_usage: ContextUsageState,
    task_state: TaskState,
    task_protocol: TaskProtocolSource,
    sidechain: SidechainTracker,

    pending_permissions: Vec<PendingPermission>,
    pending_tool_requests: HashSet<String>,
}

impl Default for ClaudeMapper {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeMapper {
    pub fn new() -> Self {
        Self {
            cwd: None,
            out: Vec::new(),
            session_key_burned: false,
            claude_session_id: None,
            current_mode: "default".to_owned(),
            plan_resume_mode: Some("default".to_owned()),
            last_runtime_model: None,
            next_turn_ordinal: 1,
            active_fg: None,
            autonomous: None,
            cancel_current_turn: None,
            main_turn_in_flight: false,
            pending_interrupt_abort: false,
            unstarted_message_uuids: HashSet::new(),
            foreground_has_visible_activity: false,
            active_turn_has_assistant_text: false,
            compaction_marker_open: false,
            compacting: false,
            closed: false,
            emitted_user_message_ids: HashSet::new(),
            tool_use_cache: Vec::new(),
            tool_use_index_to_id: HashMap::new(),
            tool_use_input_buffers: HashMap::new(),
            subagent_handback_ids: HashSet::new(),
            assembler: TimelineAssembler::default(),
            context_usage: ContextUsageState::default(),
            task_state: TaskState::default(),
            task_protocol: TaskProtocolSource::default(),
            sidechain: SidechainTracker::default(),
            pending_permissions: Vec::new(),
            pending_tool_requests: HashSet::new(),
        }
    }

    /// The session's working directory (`config.cwd`), used to relativize file paths in tool
    /// output. Without it the `cwd` of the first `system/init` message is used.
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// The configured permission mode (`config.modeId`, default `default`).
    pub fn with_mode_id(mut self, mode_id: &str) -> Self {
        self.current_mode = if is_permission_mode(mode_id) {
            mode_id.to_owned()
        } else {
            "default".to_owned()
        };
        self.plan_resume_mode = (self.current_mode != "plan").then(|| self.current_mode.clone());
        self
    }

    /// Ids of the permission requests still waiting for an answer, oldest first.
    pub fn pending_permission_ids(&self) -> Vec<String> {
        self.pending_permissions
            .iter()
            .filter_map(|p| {
                p.request
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect()
    }

    // ---- output ---------------------------------------------------------------------------------

    /// `notifySubscribers`: tags the event with the active turn id.
    fn notify(&mut self, mut event: Value) {
        let turn = self.active_fg.clone().or_else(|| self.autonomous.clone());
        if let (Some(turn), Some(o)) = (turn, event.as_object_mut()) {
            o.insert("turnId".into(), Value::String(turn));
        }
        self.out.push(event);
    }

    fn notify_timeline(&mut self, item: Value) {
        self.notify(json!({"type": "timeline", "item": item, "provider": "claude"}));
    }

    fn take_events(&mut self) -> Vec<AgentStreamEvent> {
        std::mem::take(&mut self.out)
            .iter()
            .filter_map(|e| AgentStreamEvent::from_value(e).ok())
            .collect()
    }

    // ---- capture rows (the golden driver) --------------------------------------------------------

    fn handle_row(&mut self, raw: &Value, env: &mut dyn MapEnv) {
        let Some(line) = raw.get("line") else {
            return;
        };
        if raw.get("dir").and_then(Value::as_str) == Some("in") {
            self.handle_in_line(line, env);
        } else {
            self.handle_out_line(line, env);
        }
    }

    fn handle_in_line(&mut self, line: &Value, env: &mut dyn MapEnv) {
        match str_of(line.get("type")) {
            Some("user") => {
                let prompt = user_text(line);
                self.start_turn(&prompt, env);
            }
            Some("control_request") => {
                if obj_get(line, "request").and_then(|r| str_of(r.get("subtype")))
                    == Some("interrupt")
                {
                    self.interrupt();
                }
            }
            Some("control_response") => {
                let Some(outer) = line.get("response") else {
                    return;
                };
                let response = outer.get("response").filter(|r| r.is_object());
                let request_id = outer
                    .get("request_id")
                    .map(paseo_tool_detail::js::js_to_string)
                    .unwrap_or_else(|| "undefined".to_owned());
                let behavior = response
                    .and_then(|r| r.get("behavior"))
                    .filter(|b| js_truthy(b))
                    .and_then(Value::as_str);
                if let (Some(behavior), true) =
                    (behavior, self.pending_tool_requests.contains(&request_id))
                {
                    let response = response.expect("behavior implies response");
                    if behavior == "deny" {
                        let message = match response.get("message") {
                            None | Some(Value::Null) => "denied in fixture".to_owned(),
                            Some(m) => paseo_tool_detail::js::js_to_string(m),
                        };
                        self.respond_to_first_permission(PermissionAnswer::Deny { message }, env);
                    } else {
                        self.respond_to_first_permission(PermissionAnswer::Allow, env);
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_out_line(&mut self, line: &Value, env: &mut dyn MapEnv) {
        let ty = str_of(line.get("type"));
        if ty == Some("control_request") {
            let Some(request) = line.get("request") else {
                return;
            };
            if str_of(request.get("subtype")) == Some("can_use_tool") {
                let request_id = line
                    .get("request_id")
                    .map(paseo_tool_detail::js::js_to_string)
                    .unwrap_or_else(|| "undefined".to_owned());
                self.pending_tool_requests.insert(request_id);
                let tool_name = request
                    .get("tool_name")
                    .map(paseo_tool_detail::js::js_to_string)
                    .unwrap_or_else(|| "undefined".to_owned());
                let input = request.get("input").cloned().unwrap_or_else(|| json!({}));
                let tool_use_id = read_nonempty_string(request.get("tool_use_id"));
                self.handle_permission_request(&tool_name, &input, tool_use_id, env);
            }
            return;
        }
        if ty.is_some_and(|t| SDK_MESSAGE_TYPES.contains(&t)) {
            self.route_sdk_message(line);
        }
    }

    // ---- turns -----------------------------------------------------------------------------------

    fn create_turn_id(&mut self, owner: &str) -> String {
        let id = format!("{owner}-turn-{}", self.next_turn_ordinal);
        self.next_turn_ordinal += 1;
        id
    }

    /// `startTurn`. A prompt while a foreground turn is active throws in TS and emits nothing.
    pub fn start_turn(&mut self, prompt: &str, env: &mut dyn MapEnv) {
        if self.closed || self.active_fg.is_some() {
            return;
        }
        if self.autonomous.is_some() {
            self.complete_autonomous_turn();
        }
        let message_id = env.uuid();
        let sdk_message = json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": prompt}]},
            "parent_tool_use_id": null,
            "uuid": message_id,
            "session_id": self.claude_session_id.clone().unwrap_or_default(),
        });
        let turn_id = self.create_turn_id("foreground");
        self.active_fg = Some(turn_id);
        self.foreground_has_visible_activity = false;
        self.active_turn_has_assistant_text = false;
        self.context_usage.begin_turn();
        self.unstarted_message_uuids.insert(message_id.clone());
        self.cancel_current_turn = Some(Some(message_id));
        self.notify(json!({"type": "turn_started", "provider": "claude"}));

        // The TS timer (setTimeout 0) emits the submitted user message right after the push.
        let mut events = Vec::new();
        self.append_user_message_events(&sdk_message, &mut events);
        if !events.is_empty() {
            self.foreground_has_visible_activity = true;
        }
        for e in events {
            self.notify(e);
        }
    }

    /// `interrupt`.
    pub fn interrupt(&mut self) {
        if self.cancel_current_turn.is_some() {
            self.request_cancel();
            return;
        }
        if self.autonomous.is_some() {
            self.flush_pending_tool_calls();
            self.complete_autonomous_turn();
        }
        self.interrupt_active_turn(self.main_turn_in_flight);
    }

    /// `requestCancel` of the active foreground turn.
    fn request_cancel(&mut self) {
        let Some(sdk_user_message_id) = self.cancel_current_turn.take() else {
            return;
        };
        let claude_started_turn = self.main_turn_in_flight
            || sdk_user_message_id
                .as_ref()
                .is_some_and(|id| !self.unstarted_message_uuids.contains(id));
        self.reject_all_pending_permissions("Permission request canceled");
        self.finish_foreground_turn(json!({
            "type": "turn_canceled",
            "provider": "claude",
            "reason": "Interrupted",
        }));
        if !self.closed {
            self.interrupt_active_turn(claude_started_turn);
        }
    }

    /// `interruptActiveTurn` against a query that has no `cancelAsyncMessage`.
    fn interrupt_active_turn(&mut self, claude_started_turn: bool) {
        if claude_started_turn {
            self.pending_interrupt_abort = true;
            self.unstarted_message_uuids.clear();
        } else {
            let had_unstarted = !self.unstarted_message_uuids.is_empty();
            self.unstarted_message_uuids.clear();
            if !had_unstarted {
                return;
            }
            self.pending_interrupt_abort = true;
        }
    }

    fn start_autonomous_turn(&mut self) {
        if self.autonomous.is_some() {
            return;
        }
        let id = self.create_turn_id("autonomous");
        self.autonomous = Some(id);
        self.active_turn_has_assistant_text = false;
        self.context_usage.begin_turn();
        self.notify(json!({"type": "turn_started", "provider": "claude"}));
    }

    fn complete_autonomous_turn(&mut self) {
        if self.autonomous.is_none() {
            return;
        }
        self.notify(json!({"type": "turn_completed", "provider": "claude"}));
        self.autonomous = None;
        self.active_turn_has_assistant_text = false;
        self.compaction_marker_open = false;
    }

    fn finish_foreground_turn(&mut self, event: Value) {
        let ty = str_of(event.get("type"));
        if matches!(ty, Some("turn_failed" | "turn_canceled")) {
            self.flush_pending_tool_calls();
        }
        self.notify(event);
        self.active_fg = None;
        self.cancel_current_turn = None;
        self.active_turn_has_assistant_text = false;
        self.compaction_marker_open = false;
    }

    fn dispatch_events(&mut self, events: Vec<Value>) {
        let mut terminal_seen = false;
        for event in events {
            terminal_seen |= matches!(
                str_of(event.get("type")),
                Some("turn_completed" | "turn_failed" | "turn_canceled")
            );
            self.notify(event);
        }
        if terminal_seen {
            self.compaction_marker_open = false;
            if self.active_fg.is_some() {
                self.active_fg = None;
                self.cancel_current_turn = None;
                self.active_turn_has_assistant_text = false;
            } else if self.autonomous.is_some() {
                self.autonomous = None;
                self.active_turn_has_assistant_text = false;
            }
        }
    }

    /// `failActiveTurns`.
    #[allow(dead_code)]
    fn fail_active_turns(&mut self, error_message: &str) {
        let failure = self.build_turn_failed_event(error_message);
        self.flush_pending_tool_calls();
        if self.active_fg.is_some() {
            self.finish_foreground_turn(failure);
        } else if self.autonomous.is_some() {
            self.dispatch_events(vec![failure]);
        }
    }

    /// `buildTurnFailedEvent` (no stderr diagnostic: the mapper does not see stderr).
    fn build_turn_failed_event(&self, error_message: &str) -> Value {
        let normalized = paseo_tool_detail::js::js_trim(error_message);
        let normalized = if normalized.is_empty() {
            "Claude run failed"
        } else {
            normalized
        };
        let mut event = Map::new();
        event.insert("type".into(), json!("turn_failed"));
        event.insert("provider".into(), json!("claude"));
        event.insert("error".into(), json!(normalized));
        if let Some(code) = exit_code_in(normalized) {
            event.insert("code".into(), json!(code));
        }
        Value::Object(event)
    }

    fn flush_pending_tool_calls(&mut self) {
        let entries = std::mem::take(&mut self.tool_use_cache);
        for entry in entries.iter().filter(|e| e.started) {
            let item = blocks::canceled_tool_call(&entry.name, &entry.id, entry.input.as_ref());
            if let Some(item) = item {
                self.notify_timeline(item);
            }
        }
        self.sidechain.clear();
        // The task protocol's routing table is session-scoped: the turn ended, the session did
        // not, so a backgrounded child that settles later still finds its descriptor.
        let observations = self.task_protocol.cancel_running_foreground_tasks();
        for event in fold_observations(&observations) {
            self.notify(event);
        }
    }

    /// `failRunningRuntimeTasks`: a lost Claude process terminates every task it owned.
    #[allow(dead_code)]
    fn fail_running_runtime_tasks(&mut self) {
        let observations = self.task_protocol.fail_running_tasks();
        self.dispatch_events(fold_observations(&observations));
    }

    // ---- pump ------------------------------------------------------------------------------------

    fn route_sdk_message(&mut self, message: &Value) {
        if self.consume_session_state_change(message) {
            return;
        }
        self.observe_main_turn_bracket(message);
        if self.should_suppress_stale_result(message) {
            return;
        }
        let is_foreground = self.active_fg.is_some();
        let ty = str_of(message.get("type"));
        if is_turn_init(message) && self.active_fg.is_none() {
            self.start_autonomous_turn();
        }
        if !is_foreground && self.autonomous.is_none() && ty == Some("result") {
            return;
        }
        let turn_id = self.active_fg.clone().or_else(|| self.autonomous.clone());
        let message_id = read_message_id(message);
        let events =
            self.build_pumped_message_events(message, message_id.as_deref(), turn_id.as_deref());
        if events.is_empty() {
            return;
        }
        let event_type = |e: &Value| str_of(e.get("type")).map(str::to_owned);
        if self.pending_interrupt_abort
            && ty == Some("result")
            && events.iter().any(|e| {
                matches!(
                    event_type(e).as_deref(),
                    Some("turn_completed" | "turn_failed")
                )
            })
            && (self.active_fg.is_none() || !self.foreground_has_visible_activity)
        {
            self.pending_interrupt_abort = false;
            return;
        }
        if events.iter().any(|e| {
            event_type(e).as_deref() == Some("timeline")
                && obj_get(e, "item").and_then(|i| str_of(i.get("type")))
                    == Some("assistant_message")
        }) {
            self.active_turn_has_assistant_text = true;
        }
        if self.active_fg.is_some()
            && events.iter().any(|e| {
                matches!(
                    event_type(e).as_deref(),
                    Some("timeline" | "permission_requested" | "permission_resolved")
                )
            })
        {
            self.foreground_has_visible_activity = true;
        }
        self.dispatch_events(events);
    }

    fn consume_session_state_change(&mut self, message: &Value) -> bool {
        if str_of(message.get("type")) != Some("system")
            || str_of(message.get("subtype")) != Some("session_state_changed")
        {
            return false;
        }
        if str_of(message.get("state")) == Some("idle") {
            self.main_turn_in_flight = false;
            self.complete_autonomous_turn();
        }
        true
    }

    fn observe_main_turn_bracket(&mut self, message: &Value) {
        if str_of(message.get("type")) == Some("result") {
            self.main_turn_in_flight = false;
            return;
        }
        if is_turn_init(message) {
            self.main_turn_in_flight = true;
            self.pending_interrupt_abort = false;
            return;
        }
        if command_lifecycle(message).is_some_and(|(_, state)| state == "started") {
            self.main_turn_in_flight = true;
        }
    }

    fn should_suppress_stale_result(&mut self, message: &Value) -> bool {
        if str_of(message.get("type")) != Some("result") {
            return false;
        }
        let success = str_of(message.get("subtype")) == Some("success");
        if self.pending_interrupt_abort {
            self.pending_interrupt_abort = false;
            if !success {
                return true;
            }
        }
        !success && is_abort_error(message)
    }

    fn build_pumped_message_events(
        &mut self,
        message: &Value,
        message_id_hint: Option<&str>,
        turn_id: Option<&str>,
    ) -> Vec<Value> {
        let mut events = self.translate_message_to_events(message);
        if parent_tool_use_id(message).is_none() {
            for item in self.assembler.consume(message, turn_id, message_id_hint) {
                events.push(json!({"type": "timeline", "item": item, "provider": "claude"}));
            }
        }
        events
    }

    /// `translateMessageToEvents` with assistant text and reasoning suppressed (the assembler owns
    /// them on the pump path).
    fn translate_message_to_events(&mut self, message: &Value) -> Vec<Value> {
        if let Some(parent) = parent_tool_use_id(message) {
            return self.translate_sidechain_frame_to_events(message, parent);
        }
        let mut events = Vec::new();
        if let Some(item) = self.task_state.observe(message) {
            events.push(json!({"type": "timeline", "provider": "claude", "item": item}));
        }
        // Subagent identity and lifecycle are announced by the task protocol, so they are read
        // rather than inferred from sidechain frames.
        let observations = {
            let cache = &self.tool_use_cache;
            let tool_input = |id: &str| {
                cache
                    .iter()
                    .find(|e| e.id == id)
                    .and_then(|e| e.input.clone())
            };
            self.task_protocol.observe(message, &tool_input)
        };
        events.extend(fold_observations(&observations));
        for observation in &observations {
            let Observation::Declared {
                id,
                title,
                description,
                parent_subagent_id,
                ..
            } = observation
            else {
                continue;
            };
            if !self.task_protocol.needs_synthetic_parent_tool_card(id) {
                continue;
            }
            if let Some(card) = subagent_tool_call_card(id, title, description, parent_subagent_id)
            {
                events.push(json!({"type": "timeline", "provider": "claude", "item": card}));
            }
        }
        let ty = str_of(message.get("type"));
        if ty != Some("system") {
            let (thread_started, notice) = self.capture_session_id_from_message(message);
            if let Some(notice) = notice {
                events.push(json!({"type": "timeline", "provider": "claude", "item": notice}));
            }
            if let Some(id) = thread_started {
                events
                    .push(json!({"type": "thread_started", "provider": "claude", "sessionId": id}));
            }
        }
        self.forget_started_message(message);
        match ty {
            Some("system") => self.append_system_message_events(message, &mut events),
            Some("user") => {
                self.append_user_message_events(message, &mut events);
                self.append_sidechain_result_events(message, &mut events);
            }
            Some("assistant") => {
                let content = obj_get(message, "message").and_then(|m| m.get("content"));
                if let Some(content) = content {
                    for item in self
                        .map_blocks_to_timeline(content, blocks::BlockOpts::assistant(true, true))
                    {
                        events
                            .push(json!({"type": "timeline", "item": item, "provider": "claude"}));
                    }
                }
                self.append_sidechain_result_events(message, &mut events);
            }
            Some("stream_event") => self.append_stream_event_events(message, &mut events),
            Some("result") => self.append_result_events(message, &mut events),
            _ => {}
        }
        events
    }

    /// `translateSidechainFrameToEvents`: a frame from inside a subagent, routed to that child
    /// rather than the parent's transcript.
    fn translate_sidechain_frame_to_events(&mut self, message: &Value, parent: &str) -> Vec<Value> {
        let canonical = self.task_protocol.resolve_subagent_id(parent);
        // Once a CLI announces its tasks it announces all of them, so a frame for one that was
        // never declared is work the declaration filter already rejected.
        if self.task_protocol.announces_tasks() && canonical.is_none() {
            return Vec::new();
        }
        let routed = canonical.unwrap_or_else(|| parent.to_owned());
        let mut events =
            fold_observations(&self.task_protocol.observe_sidechain_frame(message, &routed));
        let task_input = self.tool_input(&routed);
        let ctx = SidechainCtx {
            tool_input: task_input.as_ref(),
            descriptor_owned_elsewhere: self.task_protocol.is_active(),
            // The parent's tool call owns its card once its result arrives; a background child
            // streams its frames after Claude has already answered the call.
            needs_parent_card: self.tool_use_cache.iter().any(|e| e.id == routed)
                && self.task_protocol.needs_synthetic_parent_tool_card(&routed),
        };
        events.extend(self.sidechain.handle_message(message, &routed, &ctx));
        events
    }

    fn tool_input(&self, id: &str) -> Option<Value> {
        self.tool_use_cache
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.input.clone())
    }

    /// `appendSidechainResultEvents`: a tool result in the parent transcript closes its child.
    fn append_sidechain_result_events(&mut self, message: &Value, events: &mut Vec<Value>) {
        let Some(content) = obj_get(message, "message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            return;
        };
        for block in content {
            if !block.is_object() || str_of(block.get("type")) != Some("tool_result") {
                continue;
            }
            let Some(id) = str_of(block.get("tool_use_id")) else {
                continue;
            };
            let status = if block.get("is_error").is_some_and(js_truthy) {
                "failed"
            } else {
                "completed"
            };
            let owned = self.task_protocol.is_active();
            events.extend(self.sidechain.finish(id, status, owned));
        }
    }

    fn forget_started_message(&mut self, message: &Value) {
        if let Some((uuid, state)) = command_lifecycle(message) {
            if state != "queued" {
                self.unstarted_message_uuids.remove(&uuid);
            }
        }
    }

    fn capture_session_id_from_message(
        &mut self,
        message: &Value,
    ) -> (Option<String>, Option<Value>) {
        let session_id = extract_session_id(message);
        if session_id.is_empty() {
            return (None, None);
        }
        match self.claude_session_id.clone() {
            None => {
                self.claude_session_id = Some(session_id.clone());
                (Some(session_id), None)
            }
            Some(existing) if existing == session_id => (None, None),
            Some(existing) => {
                self.claude_session_id = Some(session_id.clone());
                (
                    Some(session_id.clone()),
                    Some(session_changed_notice(&existing, &session_id)),
                )
            }
        }
    }

    // ---- per message type ------------------------------------------------------------------------

    fn append_system_message_events(&mut self, message: &Value, events: &mut Vec<Value>) {
        match str_of(message.get("subtype")) {
            Some("init") => self.append_init_events(message, events),
            Some("status") => {
                if let Some(mode) =
                    str_of(message.get("permissionMode")).filter(|m| is_permission_mode(m))
                {
                    if self.observe_permission_mode(mode) {
                        events.push(json!({
                            "type": "mode_changed",
                            "provider": "claude",
                            "currentModeId": self.current_mode,
                            "availableModes": default_modes_value(),
                        }));
                    }
                }
                if str_of(message.get("status")) == Some("compacting") {
                    self.compacting = true;
                    if !self.compaction_marker_open {
                        self.compaction_marker_open = true;
                        events.push(json!({
                            "type": "timeline",
                            "item": {"type": "compaction", "status": "loading"},
                            "provider": "claude",
                        }));
                    }
                }
            }
            Some("compact_boundary") => {
                self.compaction_marker_open = false;
                let meta = compaction_metadata(message);
                let mut item = Map::new();
                item.insert("type".into(), json!("compaction"));
                item.insert("status".into(), json!("completed"));
                item.insert(
                    "trigger".into(),
                    json!(if meta.trigger.as_deref() == Some("manual") {
                        "manual"
                    } else {
                        "auto"
                    }),
                );
                if let Some(pre) = meta.pre_tokens {
                    item.insert(
                        "preTokens".into(),
                        Value::Number(paseo_tool_detail::num_from_f64(pre)),
                    );
                }
                events.push(
                    json!({"type": "timeline", "item": Value::Object(item), "provider": "claude"}),
                );
                events.push(
                    self.context_usage
                        .build_compaction_usage_event(meta.post_tokens),
                );
            }
            Some("task_notification") => {
                let cached = message
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .and_then(|id| self.tool_use_cache.iter().find(|e| e.id == id));
                let task_id = str_of(message.get("task_id")).unwrap_or("undefined");
                // The task protocol owns provider-subagent identity; the cache is only a fallback
                // for older Claude streams which do not announce tasks.
                if self.task_protocol.is_declared_task(task_id)
                    || cached
                        .is_some_and(|e| matches!(e.name.as_str(), "Task" | "Agent" | "Workflow"))
                {
                    return;
                }
                if let Some(item) = task_notification::map_system_record(message) {
                    let owner = self
                        .task_protocol
                        .resolve_task_owner(task_id, str_of(message.get("tool_use_id")));
                    match owner {
                        Some(owner) => events.push(subagents::provider_subagent_event(
                            json!({"type": "timeline", "id": owner, "item": item}),
                        )),
                        None => events
                            .push(json!({"type": "timeline", "item": item, "provider": "claude"})),
                    }
                }
            }
            _ => {}
        }
    }

    fn append_init_events(&mut self, message: &Value, events: &mut Vec<Value>) {
        let new_session_id = extract_session_id(message);
        if new_session_id.is_empty() {
            return;
        }
        match self.claude_session_id.clone() {
            None => {
                self.claude_session_id = Some(new_session_id.clone());
                events.push(json!({"type": "thread_started", "provider": "claude", "sessionId": new_session_id}));
            }
            Some(existing) if existing == new_session_id => {}
            Some(existing) => {
                self.claude_session_id = Some(new_session_id.clone());
                events.push(json!({
                    "type": "timeline",
                    "provider": "claude",
                    "item": session_changed_notice(&existing, &new_session_id),
                }));
                events.push(json!({"type": "thread_started", "provider": "claude", "sessionId": new_session_id}));
            }
        }
        if let Some(mode) = str_of(message.get("permissionMode")) {
            self.observe_permission_mode(mode);
        }
        if self.cwd.is_none() {
            self.cwd = read_nonempty_string(message.get("cwd"));
        }
        if let Some(model) = message
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
        {
            let window = models::context_window_max_tokens(Some(model));
            if self.last_runtime_model.as_deref() != Some(model) && window.is_some() {
                self.context_usage.set_initial_context_window_max(window);
            }
            self.last_runtime_model = Some(model.to_owned());
        }
    }

    fn observe_permission_mode(&mut self, mode: &str) -> bool {
        let changed = self.current_mode != mode;
        self.current_mode = mode.to_owned();
        if mode != "plan" {
            self.plan_resume_mode = Some(mode.to_owned());
        }
        changed
    }

    fn append_stream_event_events(&mut self, message: &Value, events: &mut Vec<Value>) {
        let Some(event) = message.get("event") else {
            return;
        };
        if let Some(usage_event) = self.context_usage.build_stream_usage_event(event) {
            events.push(usage_event);
        }
        for item in self.map_partial_event(event, blocks::BlockOpts::assistant(true, true)) {
            events.push(json!({"type": "timeline", "item": item, "provider": "claude"}));
        }
    }

    fn append_result_events(&mut self, message: &Value, events: &mut Vec<Value>) {
        let model = self.last_runtime_model.clone();
        let usage = self
            .context_usage
            .build_result_usage(message, model.as_deref());
        if str_of(message.get("subtype")) == Some("success") {
            let owned = self.task_protocol.is_active();
            events.extend(self.sidechain.finish_all("completed", owned));
            let result_text = message
                .get("result")
                .and_then(Value::as_str)
                .map(|s| paseo_tool_detail::js::js_trim(s).to_owned())
                .unwrap_or_default();
            let output_tokens = obj_get(message, "usage").and_then(|u| u.get("output_tokens"));
            if !result_text.is_empty()
                && output_tokens.and_then(Value::as_f64) == Some(0.0)
                && !self.active_turn_has_assistant_text
            {
                let mut item = Map::new();
                item.insert("type".into(), json!("assistant_message"));
                item.insert("text".into(), json!(result_text));
                if let Some(uuid) = message.get("uuid").filter(|u| u.is_string()) {
                    item.insert("messageId".into(), uuid.clone());
                }
                events.push(
                    json!({"type": "timeline", "provider": "claude", "item": Value::Object(item)}),
                );
            }
            let mut event = json!({"type": "turn_completed", "provider": "claude"});
            if let Some(usage) = usage {
                event["usage"] = usage;
            }
            events.push(event);
            return;
        }
        let errors = message
            .get("errors")
            .and_then(Value::as_array)
            .filter(|a| !a.is_empty())
            .map(|a| {
                a.iter()
                    .map(paseo_tool_detail::js::js_to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|| "Claude run failed".to_owned());
        let owned = self.task_protocol.is_active();
        events.extend(self.sidechain.finish_all("failed", owned));
        events.push(self.build_turn_failed_event(&errors));
    }

    /// `appendUserMessageEvents`.
    pub(crate) fn append_user_message_events(&mut self, message: &Value, events: &mut Vec<Value>) {
        if is_synthetic_user_entry(message) {
            return;
        }
        if self.compacting {
            self.compacting = false;
            return;
        }
        let message_id = read_nonempty_string(message.get("uuid"));
        if let Some(id) = &message_id {
            if self.emitted_user_message_ids.contains(id) {
                return;
            }
            self.emitted_user_message_ids.insert(id.clone());
        }
        let Some(content) = obj_get(message, "message").and_then(|m| m.get("content")) else {
            return;
        };
        if let Some(item) = task_notification::map_user_content(content, message_id.as_deref()) {
            let metadata = item.get("metadata");
            let task_id = str_of(metadata.and_then(|m| m.get("taskId")));
            let tool_use_id = str_of(metadata.and_then(|m| m.get("toolUseId")));
            if task_id.is_some_and(|t| self.task_protocol.is_declared_task(t)) {
                return;
            }
            let owner = task_id.and_then(|t| self.task_protocol.resolve_task_owner(t, tool_use_id));
            match owner {
                Some(owner) => events.push(subagents::provider_subagent_event(
                    json!({"type": "timeline", "id": owner, "item": item}),
                )),
                None => {
                    events.push(json!({"type": "timeline", "item": item, "provider": "claude"}))
                }
            }
            return;
        }
        match content {
            Value::String(s) if !s.is_empty() => {
                if !text::is_transcript_noise_str(s) {
                    let mut item = Map::new();
                    item.insert("type".into(), json!("user_message"));
                    item.insert("text".into(), json!(s));
                    if let Some(id) = &message_id {
                        item.insert("messageId".into(), json!(id));
                    }
                    events.push(json!({"type": "timeline", "item": Value::Object(item), "provider": "claude"}));
                }
            }
            Value::Array(_) => {
                for mut item in self.map_blocks_to_timeline(content, blocks::BlockOpts::user()) {
                    if str_of(item.get("type")) == Some("user_message")
                        && item.get("messageId").is_none()
                    {
                        if let (Some(id), Some(o)) = (&message_id, item.as_object_mut()) {
                            o.insert("messageId".into(), json!(id));
                        }
                    }
                    events.push(json!({"type": "timeline", "item": item, "provider": "claude"}));
                }
            }
            _ => {}
        }
    }

    // ---- session close ----------------------------------------------------------------------------

    /// `close()`: pending permissions are rejected, an active foreground turn is canceled.
    fn close_session(&mut self) {
        self.closed = true;
        self.reject_all_pending_permissions("Claude session closed");
        self.sidechain.clear();
        self.task_protocol.reset();
        if self.cancel_current_turn.is_some() {
            self.request_cancel();
        }
        self.active_fg = None;
        self.autonomous = None;
        self.cancel_current_turn = None;
    }
}

impl ProviderMapper for ClaudeMapper {
    fn provider(&self) -> &'static str {
        "claude"
    }

    fn feed(&mut self, raw: &Value, env: &mut dyn MapEnv) -> Vec<AgentStreamEvent> {
        if !self.session_key_burned {
            // PARITY-QUIRK(claude-session-key-uuid)
            env.uuid();
            self.session_key_burned = true;
        }
        self.handle_row(raw, env);
        self.take_events()
    }

    fn finish(&mut self, _env: &mut dyn MapEnv) -> Vec<AgentStreamEvent> {
        if self.closed {
            return Vec::new();
        }
        self.close_session();
        self.take_events()
    }
}

pub(crate) enum PermissionAnswer {
    Allow,
    Deny { message: String },
}

// ---- free helpers ------------------------------------------------------------------------------

/// `buildSubagentToolCallCard`: the parent's Task card, built from the declaration. A direct
/// child only; a launching sidechain already owns the card of the grandchild it spawned.
fn subagent_tool_call_card(
    id: &str,
    title: &Option<String>,
    description: &Option<String>,
    parent_subagent_id: &Option<String>,
) -> Option<Value> {
    if parent_subagent_id.is_some() {
        return None;
    }
    let card = paseo_tool_detail::detail::claude::map_claude_running_tool_call(
        &paseo_tool_detail::detail::claude::ClaudeToolCallParams {
            call_id: Some(id),
            name: "Task",
            input: None,
            output: None,
            metadata: None,
        },
    )?;
    let mut detail = Map::new();
    detail.insert("type".into(), json!("sub_agent"));
    if let Some(t) = title {
        detail.insert("subAgentType".into(), json!(t));
    }
    if let Some(d) = description {
        detail.insert("description".into(), json!(d));
    }
    detail.insert("log".into(), json!(""));
    detail.insert("actions".into(), json!([]));
    let mut value = card.to_value();
    if let Some(o) = value.as_object_mut() {
        o.insert("detail".into(), Value::Object(detail));
    }
    Some(value)
}

fn read_nonempty_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// `userText` of the golden driver.
fn user_text(line: &Value) -> String {
    match obj_get(line, "message").and_then(|m| m.get("content")) {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|b| b.get("text").and_then(Value::as_str).unwrap_or(""))
            .collect(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => paseo_tool_detail::js::js_to_string(other),
        None => String::new(),
    }
}

fn is_turn_init(message: &Value) -> bool {
    str_of(message.get("type")) == Some("system") && str_of(message.get("subtype")) == Some("init")
}

/// `readClaudeParentToolUseId`.
fn parent_tool_use_id(message: &Value) -> Option<&str> {
    let o = message.as_object()?;
    if str_of(o.get("type")) == Some("tool_progress")
        && o.get("heartbeat") == Some(&Value::Bool(true))
    {
        return None;
    }
    o.get("parent_tool_use_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// `readClaudeCommandLifecycle`.
fn command_lifecycle(message: &Value) -> Option<(String, String)> {
    if str_of(message.get("type")) != Some("command_lifecycle") {
        return None;
    }
    let uuid = message.get("command_uuid")?.as_str()?;
    let state = message
        .get("state")
        .map(paseo_tool_detail::js::js_to_string)?;
    matches!(
        state.as_str(),
        "queued" | "started" | "completed" | "cancelled"
    )
    .then(|| (uuid.to_owned(), state))
}

fn extract_session_id(message: &Value) -> String {
    // `extractSessionIdRaw`: the first field that is a string wins, even when empty.
    let raw = [
        message.get("session_id"),
        message.get("sessionId"),
        obj_get(message, "session").and_then(|s| s.get("id")),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
    .unwrap_or("");
    paseo_tool_detail::js::js_trim(raw).to_owned()
}

fn session_changed_notice(old: &str, new: &str) -> Value {
    json!({
        "type": "assistant_message",
        "text": format!("Claude switched to a new session: {old} -> {new}"),
    })
}

/// `readEventIdentifiers(message).messageId`.
fn read_message_id(message: &Value) -> Option<String> {
    let stream_event = message.get("event").filter(|e| e.is_object());
    let stream_message = stream_event.and_then(|e| e.get("message"));
    let container = message.get("message");
    let ty = str_of(message.get("type"));
    let from_uuid = matches!(ty, Some("user" | "assistant" | "system"))
        .then(|| message.get("uuid"))
        .flatten();
    first_trimmed(&[
        message.get("message_id"),
        stream_event.and_then(|e| e.get("message_id")),
        stream_message.and_then(|m| m.get("id")),
        stream_message.and_then(|m| m.get("message_id")),
        container.and_then(|m| m.get("id")),
        container.and_then(|m| m.get("message_id")),
        from_uuid,
    ])
}

/// `isSyntheticUserEntry`.
fn is_synthetic_user_entry(entry: &Value) -> bool {
    let Some(o) = entry.as_object() else {
        return false;
    };
    o.get("isSynthetic") == Some(&Value::Bool(true))
        || o.get("isMeta") == Some(&Value::Bool(true))
        || o.get("toolUseResult").is_some_and(js_truthy)
}

/// `isAbortError`: `/\baborted\b/i` over the `errors` strings.
fn is_abort_error(message: &Value) -> bool {
    let Some(errors) = message.get("errors").and_then(Value::as_array) else {
        return false;
    };
    errors.iter().any(|e| {
        contains_word(
            &paseo_tool_detail::js::js_to_string(e).to_lowercase(),
            "aborted",
        )
    })
}

/// Whole-word match (`\b` on ASCII word characters).
fn contains_word(haystack: &str, word: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(word) {
        let start = from + pos;
        let end = start + word.len();
        let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let after_ok = end == bytes.len() || !is_word_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

struct CompactionMetadata {
    trigger: Option<String>,
    pre_tokens: Option<f64>,
    post_tokens: Option<f64>,
}

/// `readCompactionMetadata`.
fn compaction_metadata(source: &Value) -> CompactionMetadata {
    for key in ["compact_metadata", "compactMetadata", "compactionMetadata"] {
        let Some(meta) = source.get(key).filter(|m| m.is_object()) else {
            continue;
        };
        let num = |a: &str, b: &str| {
            meta.get(a)
                .or_else(|| meta.get(b))
                .filter(|v| !v.is_null())
                .and_then(Value::as_f64)
        };
        return CompactionMetadata {
            trigger: meta
                .get("trigger")
                .and_then(Value::as_str)
                .map(str::to_owned),
            pre_tokens: num("preTokens", "pre_tokens"),
            post_tokens: num("postTokens", "post_tokens"),
        };
    }
    CompactionMetadata {
        trigger: None,
        pre_tokens: None,
        post_tokens: None,
    }
}

/// `/\bcode\s+(\d+)\b/i` capture.
fn exit_code_in(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(pos) = lower[from..].find("code") {
        let start = from + pos;
        let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let mut i = start + 4;
        let ws_start = i;
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t' || bytes[i] == b'\n') {
            i += 1;
        }
        let digits_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let after_ok = i == bytes.len() || !is_word_byte(bytes[i]);
        if before_ok && i > ws_start && i > digits_start && digits_start > ws_start && after_ok {
            return Some(lower[digits_start..i].to_owned());
        }
        from = start + 4;
    }
    None
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_regex_semantics() {
        assert_eq!(
            exit_code_in("process exited with code 137").as_deref(),
            Some("137")
        );
        assert_eq!(exit_code_in("Code 2 failed").as_deref(), Some("2"));
        assert_eq!(exit_code_in("decode 5"), None);
        assert_eq!(exit_code_in("code 12abc"), None);
        assert_eq!(exit_code_in("code"), None);
    }
}
