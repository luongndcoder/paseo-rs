//! ACP mapper: recorded ACP JSON-RPC frames -> `AgentStreamEvent`s.
//!
//! Port of the translation seam of `ACPAgentSession` (`acp-agent.ts`): `translateSessionUpdate`,
//! tool snapshot merge, `mapToolDetail`, the user-message coalescer, and history replay.
//! Input is one capture line per `feed` call: `{argv}` header, `{dir, line}` JSON-RPC frame, or
//! `{exit}`. The mapper tracks just enough of the controller state (active turn id, prompt
//! response) to stamp turn ids the way the TS driver did when the goldens were captured.
//!
//! PARITY-QUIRK(acp-no-turn-events): `turn_started` / `turn_completed` / permission events come
//! from the prompt and permission controller in TS, not from `translateSessionUpdate`, so the
//! goldens never contain them; a prompt response only clears the active turn. A `cancelled`
//! stop reason additionally emits the synthesized `canceled` tool calls (still no turn event).
//!
//! PARITY-QUIRK(acp-no-session-id): the capture driver never sets the session id, so
//! `model_changed.runtimeInfo.sessionId` is `null`. Hosts call [`AcpMapper::set_session_id`].
//!
//! PARITY-QUIRK(acp-session-state-not-applied): the driver never applied the `session/new|load`
//! response (`applySessionState`), so e.g. `thinkingOptionId` stays `null` in `model_changed`
//! even when the response carries a `thought_level` option. `feed` mirrors that; hosts call
//! [`AcpMapper::apply_session_state`] themselves.
//!
//! PARITY-QUIRK(acp-no-profile-transforms): the driver ran without a provider profile, so the
//! Copilot config/mode transformers do not apply (goldens keep Copilot's `autopilot` mode).
//!
//! PARITY-QUIRK(acp-history-events-emitted): while `session/load` replays history, TS stores the
//! timeline items instead of emitting them; the goldens contain them as events. The mapper
//! emits them and also records them for [`AcpMapper::take_history`].
//!
//! PARITY-QUIRK(acp-usage-before-pending-user): `usage_update` pushes `usage_updated`
//! immediately while a pending user message flushed by the same update is delivered after it.

use std::collections::HashMap;

use paseo_agent_types::AgentStreamEvent;
use paseo_tool_detail::js::{js_trim, json_stringify};
use serde_json::{json, Map, Value};

use crate::{MapEnv, ProviderMapper};

type Obj = Map<String, Value>;

/// Output of an ACP terminal the host spawned for a `terminal/create` request.
#[derive(Debug, Clone, Default)]
pub struct TerminalEntry {
    pub output: String,
    pub exit_code: Option<Value>,
}

#[derive(Debug, Clone)]
struct ToolSnapshot {
    tool_call_id: String,
    title: String,
    kind: Option<String>,
    status: Option<String>,
    /// `Some(Null)` is an explicit `null`; `None` is `undefined`.
    content: Value,
    locations: Value,
    raw_input: Option<Value>,
    raw_output: Option<Value>,
}

#[derive(Debug)]
struct PendingUser {
    text: String,
    message_id: Option<String>,
}

#[derive(Debug)]
pub struct AcpMapper {
    provider: String,
    session_id: Option<String>,
    default_modes: Vec<Value>,
    available_modes: Vec<Value>,
    current_mode: Option<String>,
    current_model: Option<String>,
    thinking_option_id: Option<String>,
    title: Value,
    updated_at: Value,
    current_turn_usage: Obj,
    turn_counter: u64,
    active_turn: Option<String>,
    submitted_user_message_turn: Option<String>,
    fallback_assistant_id: Option<String>,
    pending_user: Option<PendingUser>,
    tool_calls: Vec<(String, ToolSnapshot)>,
    terminals: HashMap<String, TerminalEntry>,
    commands: Vec<Value>,
    in_requests: HashMap<String, String>,
    replaying_history: bool,
    history: Vec<Value>,
}

impl Default for AcpMapper {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpMapper {
    pub fn new() -> Self {
        Self::with_provider("acp")
    }

    pub fn with_provider(provider: &str) -> Self {
        Self {
            provider: provider.to_owned(),
            session_id: None,
            default_modes: Vec::new(),
            available_modes: Vec::new(),
            current_mode: None,
            current_model: None,
            thinking_option_id: None,
            title: Value::Null,
            updated_at: Value::Null,
            current_turn_usage: Obj::new(),
            turn_counter: 0,
            active_turn: None,
            submitted_user_message_turn: None,
            fallback_assistant_id: None,
            pending_user: None,
            tool_calls: Vec::new(),
            terminals: HashMap::new(),
            commands: Vec::new(),
            in_requests: HashMap::new(),
            replaying_history: false,
            history: Vec::new(),
        }
    }

    /// Provider id stamped on every event.
    pub fn provider_id(&self) -> &str {
        &self.provider
    }

    pub fn set_session_id(&mut self, id: Option<String>) {
        self.session_id = id;
    }

    /// Modes used when neither the session response nor config options define any
    /// (`AgentProfile.defaultModes`); each entry is an `AgentMode` JSON object.
    pub fn set_default_modes(&mut self, modes: Vec<Value>) {
        self.default_modes = modes;
    }

    /// Marks the turn whose user message the host already emitted (`emitSubmittedUserMessage`):
    /// echoed `user_message_chunk`s for that turn are dropped.
    pub fn set_submitted_user_message_turn(&mut self, turn: Option<String>) {
        self.submitted_user_message_turn = turn;
    }

    /// Registers the output of a host-side terminal so `terminal` tool content resolves.
    pub fn set_terminal(&mut self, terminal_id: &str, entry: TerminalEntry) {
        self.terminals.insert(terminal_id.to_owned(), entry);
    }

    /// Cached `available_commands_update` payload (`{name, description, argumentHint, kind}`).
    pub fn commands(&self) -> &[Value] {
        &self.commands
    }

    /// Timeline items collected while `session/load` replayed history (`persistedHistory`).
    pub fn take_history(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.history)
    }

    pub fn active_turn_id(&self) -> Option<&str> {
        self.active_turn.as_deref()
    }

    // ---- frame routing -------------------------------------------------------------------

    fn feed_capture_line(&mut self, raw: &Value, env: &mut dyn MapEnv) -> Vec<Value> {
        if let Some(argv) = raw.get("argv").and_then(Value::as_array) {
            if let Some(first) = argv.first().and_then(Value::as_str) {
                self.provider = provider_from_argv0(first);
            }
            return Vec::new();
        }
        let Some(line) = raw.get("line") else {
            // Bare JSON-RPC frame (no capture wrapper): treat as agent output.
            return if raw.get("jsonrpc").is_some() {
                self.feed_frame("out", raw, env)
            } else {
                Vec::new()
            };
        };
        let dir = raw.get("dir").and_then(Value::as_str).unwrap_or("");
        self.feed_frame(dir, line, env)
    }

    fn feed_frame(&mut self, dir: &str, frame: &Value, env: &mut dyn MapEnv) -> Vec<Value> {
        let method = frame.get("method").and_then(Value::as_str);
        let id = frame.get("id").map(id_key);
        match (dir, method, id) {
            ("in", Some(method), Some(id)) => {
                self.on_client_request(method, id);
                Vec::new()
            }
            ("out", Some("session/update"), _) => {
                let params = frame.get("params").unwrap_or(&Value::Null);
                if let (Some(mine), Some(theirs)) = (
                    self.session_id.as_deref(),
                    params.get("sessionId").and_then(Value::as_str),
                ) {
                    if mine != theirs {
                        return Vec::new();
                    }
                }
                match params.get("update") {
                    Some(update) => self.translate_session_update(update, env),
                    None => Vec::new(),
                }
            }
            ("out", None, Some(id)) => match self.in_requests.remove(&id) {
                Some(method) => self.on_response(&method, frame),
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    fn on_client_request(&mut self, method: &str, id: String) {
        if method == "session/prompt" {
            self.turn_counter += 1;
            self.active_turn = Some(format!("turn-{}", self.turn_counter));
        }
        if method == "session/load" {
            self.replaying_history = true;
        }
        self.in_requests.insert(id, method.to_owned());
    }

    fn on_response(&mut self, method: &str, frame: &Value) -> Vec<Value> {
        let result = frame.get("result");
        match method {
            "session/load" => {
                let out = self.flush_pending_user();
                self.replaying_history = false;
                out
            }
            "session/prompt" => self.on_prompt_response(result),
            _ => Vec::new(),
        }
    }

    fn on_prompt_response(&mut self, result: Option<&Value>) -> Vec<Value> {
        let Some(result) = result else {
            return Vec::new();
        };
        if let Some(usage) = result.get("usage").and_then(Value::as_object) {
            self.current_turn_usage = map_usage(usage);
        }
        let mut out = Vec::new();
        if result.get("stopReason").and_then(Value::as_str) == Some("cancelled") {
            out.extend(self.synthesize_canceled_tool_calls());
        }
        out.extend(self.flush_pending_user());
        self.active_turn = None;
        self.fallback_assistant_id = None;
        out
    }

    // ---- session state ---------------------------------------------------------------------

    /// `applySessionState` for a `session/new|load|resume` result. Hosts call it explicitly; the
    /// frame router does not (see PARITY-QUIRK `acp-session-state-not-applied`).
    pub fn apply_session_state(&mut self, response: &Value) {
        let config_options = response.get("configOptions").and_then(Value::as_array);
        let (modes, current) = derive_modes(
            &self.default_modes,
            response.get("modes"),
            config_options.map(Vec::as_slice),
        );
        self.available_modes = modes;
        if let Some(current) = current {
            self.current_mode = Some(current);
        }
        let models = response.get("models");
        self.current_model = models
            .and_then(|m| m.get("currentModelId"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| derive_current_config_value(config_options.map(Vec::as_slice), "model"));
        if let Some(t) =
            derive_current_config_value(config_options.map(Vec::as_slice), "thought_level")
        {
            self.thinking_option_id = Some(t);
        }
    }

    fn runtime_info(&self) -> Value {
        json!({
            "provider": self.provider,
            "sessionId": opt_str(&self.session_id),
            "model": opt_str(&self.current_model),
            "thinkingOptionId": opt_str(&self.thinking_option_id),
            "modeId": opt_str(&self.current_mode),
            "extra": { "title": self.title, "updatedAt": self.updated_at },
        })
    }

    // ---- translateSessionUpdate --------------------------------------------------------------

    fn translate_session_update(&mut self, update: &Value, env: &mut dyn MapEnv) -> Vec<Value> {
        let kind = update.get("sessionUpdate").and_then(Value::as_str);
        if kind == Some("user_message_chunk") {
            return self.handle_user_message_chunk(update);
        }
        let mut events = self.flush_pending_user();
        match kind {
            Some("agent_message_chunk") => {
                if let Some(item) = self.message_item(true, update, env) {
                    events.push(self.wrap_timeline(item));
                }
            }
            Some("agent_thought_chunk") => {
                self.fallback_assistant_id = None;
                if let Some(item) = self.message_item(false, update, env) {
                    events.push(self.wrap_timeline(item));
                }
            }
            Some("tool_call") => {
                self.fallback_assistant_id = None;
                events.push(self.handle_tool_call_update(update, false));
            }
            Some("tool_call_update") => events.push(self.handle_tool_call_update(update, true)),
            Some("plan") => {
                self.fallback_assistant_id = None;
                events.push(self.wrap_timeline(map_plan(update)));
            }
            Some("current_mode_update") => {
                self.current_mode = update
                    .get("currentModeId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                events.push(json!({
                    "type": "mode_changed",
                    "provider": self.provider,
                    "currentModeId": opt_str(&self.current_mode),
                    "availableModes": self.available_modes,
                }));
            }
            Some("config_option_update") => events.extend(self.handle_config_option_update(update)),
            Some("session_info_update") => {
                if let Some(o) = update.as_object() {
                    if let Some(t) = o.get("title") {
                        self.title = t.clone();
                    }
                    if let Some(t) = o.get("updatedAt") {
                        self.updated_at = t.clone();
                    }
                }
            }
            Some("usage_update") => {
                if let Some(usage_event) = self.handle_usage_update(update) {
                    events.insert(0, usage_event);
                }
            }
            Some("available_commands_update") => {
                self.commands = update
                    .get("availableCommands")
                    .and_then(Value::as_array)
                    .map(|cmds| cmds.iter().map(map_command).collect())
                    .unwrap_or_default();
            }
            _ => {}
        }
        events
    }

    fn record_history(&mut self, events: &[Value]) {
        for e in events {
            if e.get("type").and_then(Value::as_str) == Some("timeline") {
                if let Some(item) = e.get("item") {
                    self.history.push(item.clone());
                }
            }
        }
    }

    fn wrap_timeline(&self, item: Value) -> Value {
        let mut m = Obj::new();
        m.insert("type".into(), json!("timeline"));
        m.insert("provider".into(), json!(self.provider));
        m.insert("item".into(), item);
        if let Some(turn) = &self.active_turn {
            m.insert("turnId".into(), json!(turn));
        }
        Value::Object(m)
    }

    fn message_item(
        &mut self,
        assistant: bool,
        update: &Value,
        env: &mut dyn MapEnv,
    ) -> Option<Value> {
        let text = content_block_to_text(update.get("content"));
        if text.is_empty() {
            return None;
        }
        if !assistant {
            return Some(json!({ "type": "reasoning", "text": text }));
        }
        let message_id = match update.get("messageId").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => {
                self.fallback_assistant_id = None;
                id.to_owned()
            }
            _ => {
                if self.fallback_assistant_id.is_none() {
                    self.fallback_assistant_id = Some(env.uuid());
                }
                self.fallback_assistant_id.clone().unwrap_or_default()
            }
        };
        Some(json!({ "type": "assistant_message", "text": text, "messageId": message_id }))
    }

    // ---- user messages -----------------------------------------------------------------------

    fn handle_user_message_chunk(&mut self, update: &Value) -> Vec<Value> {
        self.fallback_assistant_id = None;
        if self.active_turn.is_some() && self.submitted_user_message_turn == self.active_turn {
            return Vec::new();
        }
        let text = content_block_to_text(update.get("content"));
        if text.is_empty() {
            return Vec::new();
        }
        let message_id = update
            .get("messageId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let starts_new = match (&self.pending_user, &message_id) {
            (
                Some(PendingUser {
                    message_id: Some(p),
                    ..
                }),
                Some(m),
            ) => p != m,
            _ => false,
        };
        let events = if starts_new {
            self.flush_pending_user()
        } else {
            Vec::new()
        };
        let pending = self.pending_user.get_or_insert_with(|| PendingUser {
            text: String::new(),
            message_id: message_id.clone(),
        });
        if pending.message_id.is_none() {
            pending.message_id = message_id;
        }
        pending.text.push_str(&text);
        events
    }

    fn flush_pending_user(&mut self) -> Vec<Value> {
        let Some(p) = self.pending_user.take() else {
            return Vec::new();
        };
        let mut item = Obj::new();
        item.insert("type".into(), json!("user_message"));
        item.insert("text".into(), json!(p.text));
        if let Some(id) = p.message_id {
            item.insert("messageId".into(), json!(id));
        }
        vec![self.wrap_timeline(Value::Object(item))]
    }

    // ---- tools, config, usage ----------------------------------------------------------------

    fn handle_tool_call_update(&mut self, update: &Value, merge: bool) -> Value {
        let id = tool_call_id(update);
        let previous = if merge {
            self.tool_calls
                .iter()
                .find(|(k, _)| *k == id)
                .map(|(_, s)| s.clone())
        } else {
            None
        };
        let snapshot = merge_tool_snapshot(&id, update, previous.as_ref());
        match self.tool_calls.iter_mut().find(|(k, _)| *k == id) {
            Some(slot) => slot.1 = snapshot.clone(),
            None => self.tool_calls.push((id, snapshot.clone())),
        }
        self.wrap_timeline(map_snapshot_to_timeline(&snapshot, &self.terminals))
    }

    fn handle_config_option_update(&mut self, update: &Value) -> Vec<Value> {
        let options: Vec<Value> = update
            .get("configOptions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let (modes, next_mode) = derive_modes(&self.default_modes, None, Some(&options));
        let next_model = derive_current_config_value(Some(&options), "model");
        let next_thinking = derive_current_config_value(Some(&options), "thought_level");
        self.available_modes = modes;
        if next_mode.is_some() {
            self.current_mode = next_mode.clone();
        }
        if next_model.is_some() {
            self.current_model = next_model.clone();
        }
        if next_thinking.is_some() {
            self.thinking_option_id = next_thinking.clone();
        }
        let mut events = Vec::new();
        if next_mode.is_some() {
            events.push(json!({
                "type": "mode_changed",
                "provider": self.provider,
                "currentModeId": opt_str(&self.current_mode),
                "availableModes": self.available_modes,
            }));
        }
        if next_model.is_some() {
            events.push(json!({
                "type": "model_changed",
                "provider": self.provider,
                "runtimeInfo": self.runtime_info(),
            }));
        }
        if next_thinking.is_some() {
            events.push(json!({
                "type": "thinking_option_changed",
                "provider": self.provider,
                "thinkingOptionId": opt_str(&self.thinking_option_id),
            }));
        }
        events
    }

    fn handle_usage_update(&self, update: &Value) -> Option<Value> {
        let size = update.get("size").and_then(Value::as_f64)?;
        let used = update.get("used").and_then(Value::as_f64)?;
        if !(size.is_finite() && size > 0.0 && used.is_finite() && used >= 0.0) {
            return None;
        }
        let mut usage = self.current_turn_usage.clone();
        usage.insert("contextWindowMaxTokens".into(), update["size"].clone());
        usage.insert("contextWindowUsedTokens".into(), update["used"].clone());
        let mut event = json!({
            "type": "usage_updated",
            "provider": self.provider,
            "usage": usage,
        });
        if let Some(turn) = &self.active_turn {
            event["turnId"] = json!(turn);
        }
        Some(event)
    }

    fn synthesize_canceled_tool_calls(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for (_, snapshot) in &self.tool_calls {
            let mut item = map_snapshot_to_timeline(snapshot, &self.terminals);
            if item["status"] == "running" {
                item["status"] = json!("canceled");
                item["error"] = Value::Null;
                out.push(self.wrap_timeline(item));
            }
        }
        out
    }
}

impl ProviderMapper for AcpMapper {
    fn provider(&self) -> &'static str {
        "acp"
    }

    fn feed(&mut self, raw: &Value, env: &mut dyn MapEnv) -> Vec<AgentStreamEvent> {
        let replaying = self.replaying_history;
        let events = self.feed_capture_line(raw, env);
        if replaying {
            self.record_history(&events);
        }
        events
            .iter()
            .filter_map(|e| AgentStreamEvent::from_value(e).ok())
            .collect()
    }
}

// ---- helpers (ports of the free functions in acp-agent.ts) -----------------------------------

fn provider_from_argv0(argv0: &str) -> String {
    let base = argv0.rsplit(['/', '\\']).next().unwrap_or(argv0);
    let base = base.strip_suffix(".exe").unwrap_or(base);
    if base.is_empty() {
        "acp".to_owned()
    } else {
        base.to_owned()
    }
}

/// JSON-RPC ids may be numbers or strings; key them by their JSON text.
fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn opt_str(s: &Option<String>) -> Value {
    s.as_ref().map_or(Value::Null, |s| json!(s))
}

/// `value ?? undefined`: present and not `null`.
fn defined(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

fn map_usage(usage: &Obj) -> Obj {
    let mut out = Obj::new();
    for (src, dst) in [
        ("inputTokens", "inputTokens"),
        ("outputTokens", "outputTokens"),
        ("cachedReadTokens", "cachedInputTokens"),
    ] {
        if let Some(v) = defined(usage.get(src)) {
            out.insert(dst.into(), v.clone());
        }
    }
    out
}

fn map_command(c: &Value) -> Value {
    let hint = c
        .get("input")
        .and_then(|i| i.get("hint"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut m = Obj::new();
    m.insert("name".into(), c.get("name").cloned().unwrap_or(Value::Null));
    if let Some(d) = c.get("description") {
        m.insert("description".into(), d.clone());
    }
    m.insert("argumentHint".into(), json!(hint));
    m.insert("kind".into(), json!("command"));
    Value::Object(m)
}

fn flatten_select_options(options: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    for option in options.as_array().into_iter().flatten() {
        if option.get("value").is_some() {
            out.push(option.clone());
        } else if let Some(group) = option.get("options").and_then(Value::as_array) {
            out.extend(group.iter().cloned());
        }
    }
    out
}

fn find_select_option<'a>(options: Option<&'a [Value]>, category: &str) -> Option<&'a Value> {
    options?.iter().find(|o| {
        o.get("type").and_then(Value::as_str) == Some("select")
            && o.get("category").and_then(Value::as_str) == Some(category)
    })
}

fn derive_current_config_value(options: Option<&[Value]>, category: &str) -> Option<String> {
    find_select_option(options, category)
        .and_then(|o| o.get("currentValue"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn agent_mode(id: &Value, label: &Value, description: Option<&Value>) -> Value {
    let mut m = Obj::new();
    m.insert("id".into(), id.clone());
    m.insert("label".into(), label.clone());
    if let Some(d) = defined(description) {
        m.insert("description".into(), d.clone());
    }
    Value::Object(m)
}

/// `deriveModesFromACP`.
fn derive_modes(
    fallback: &[Value],
    mode_state: Option<&Value>,
    config_options: Option<&[Value]>,
) -> (Vec<Value>, Option<String>) {
    if let Some(available) = mode_state
        .and_then(|m| m.get("availableModes"))
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    {
        let modes = available
            .iter()
            .map(|m| {
                agent_mode(
                    m.get("id").unwrap_or(&Value::Null),
                    m.get("name").unwrap_or(&Value::Null),
                    m.get("description"),
                )
            })
            .collect();
        let current = mode_state
            .and_then(|m| m.get("currentModeId"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        return (modes, current);
    }
    if let Some(option) = find_select_option(config_options, "mode") {
        let modes = flatten_select_options(option.get("options").unwrap_or(&Value::Null))
            .iter()
            .map(|o| {
                agent_mode(
                    o.get("value").unwrap_or(&Value::Null),
                    o.get("name").unwrap_or(&Value::Null),
                    o.get("description"),
                )
            })
            .collect();
        let current = option
            .get("currentValue")
            .and_then(Value::as_str)
            .map(str::to_owned);
        return (modes, current);
    }
    (fallback.to_vec(), None)
}

fn tool_call_id(update: &Value) -> String {
    match update.get("toolCallId") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn merge_tool_snapshot(id: &str, update: &Value, previous: Option<&ToolSnapshot>) -> ToolSnapshot {
    let text = |key: &str, prev: Option<&String>| -> Option<String> {
        defined(update.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| prev.cloned())
    };
    // `coalesceDefined`: an explicit `null` from the agent replaces the previous value.
    let coalesce = |key: &str, prev: Option<&Value>| -> Value {
        match update.get(key) {
            Some(v) => v.clone(),
            None => prev.cloned().unwrap_or(Value::Null),
        }
    };
    ToolSnapshot {
        tool_call_id: id.to_owned(),
        title: text("title", previous.map(|p| &p.title)).unwrap_or_else(|| id.to_owned()),
        kind: text("kind", previous.and_then(|p| p.kind.as_ref())),
        status: text("status", previous.and_then(|p| p.status.as_ref())),
        content: coalesce("content", previous.map(|p| &p.content)),
        locations: coalesce("locations", previous.map(|p| &p.locations)),
        raw_input: update
            .get("rawInput")
            .cloned()
            .or_else(|| previous.and_then(|p| p.raw_input.clone())),
        raw_output: update
            .get("rawOutput")
            .cloned()
            .or_else(|| previous.and_then(|p| p.raw_output.clone())),
    }
}

fn map_plan(plan: &Value) -> Value {
    let items: Vec<Value> = plan
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|e| {
            json!({
                "text": e.get("content").cloned().unwrap_or(Value::Null),
                "completed": e.get("status").and_then(Value::as_str) == Some("completed"),
            })
        })
        .collect();
    json!({ "type": "todo", "items": items })
}

fn content_block_to_text(content: Option<&Value>) -> String {
    let Some(c) = content else {
        return String::new();
    };
    let s = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_owned);
    match c.get("type").and_then(Value::as_str) {
        Some("text") => s(c.get("text")).unwrap_or_default(),
        Some("resource_link") => s(defined(c.get("title")))
            .or_else(|| s(c.get("uri")))
            .unwrap_or_default(),
        Some("resource") => {
            let r = c.get("resource");
            match s(r.and_then(|r| r.get("text"))) {
                Some(t) => t,
                None => format!(
                    "[resource:{}]",
                    s(defined(r.and_then(|r| r.get("mimeType")))).unwrap_or("binary".into())
                ),
            }
        }
        Some("image") => "[image]".into(),
        Some("audio") => "[audio]".into(),
        _ => String::new(),
    }
}

fn map_snapshot_to_timeline(
    snapshot: &ToolSnapshot,
    terminals: &HashMap<String, TerminalEntry>,
) -> Value {
    let detail = map_tool_detail(snapshot, terminals);
    let mut metadata = Obj::new();
    if let Some(kind) = &snapshot.kind {
        metadata.insert("kind".into(), json!(kind));
    }
    metadata.insert("title".into(), json!(snapshot.title));
    let (status, error) = match snapshot.status.as_deref() {
        Some("failed") => (
            "failed",
            json!({ "message": read_error_message(snapshot.raw_output.as_ref()) }),
        ),
        Some("completed") => ("completed", Value::Null),
        _ => ("running", Value::Null),
    };
    json!({
        "type": "tool_call",
        "callId": snapshot.tool_call_id,
        "name": snapshot.kind.clone().unwrap_or_else(|| snapshot.title.clone()),
        "detail": detail,
        "metadata": metadata,
        "status": status,
        "error": error,
    })
}

struct DetailCtx<'a> {
    snapshot: &'a ToolSnapshot,
    first_location: Option<String>,
    text: Option<String>,
    diff: Option<(Option<String>, Value)>,
    terminal: Option<(String, Option<Value>)>,
    raw_input: Option<&'a Obj>,
    raw_output: Option<&'a Obj>,
}

fn map_tool_detail(snapshot: &ToolSnapshot, terminals: &HashMap<String, TerminalEntry>) -> Value {
    let content = snapshot.content.as_array();
    let ctx = DetailCtx {
        snapshot,
        first_location: snapshot
            .locations
            .as_array()
            .and_then(|l| l.first())
            .and_then(|l| l.get("path"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        text: extract_tool_text(content),
        diff: extract_diff(content),
        terminal: extract_terminal(content, terminals),
        raw_input: snapshot.raw_input.as_ref().and_then(Value::as_object),
        raw_output: snapshot.raw_output.as_ref().and_then(Value::as_object),
    };
    match snapshot.kind.as_deref() {
        Some("read") => read_detail(&ctx),
        Some("edit") | Some("delete") => edit_detail(&ctx),
        Some("search") => search_detail(&ctx),
        Some("execute") => shell_detail(&ctx),
        Some("fetch") => fetch_detail(&ctx),
        Some("think") => json!({
            "type": "plain_text",
            "label": snapshot.title,
            "icon": "brain",
            "text": ctx.text.clone().or_else(|| stringify_unknown(snapshot.raw_output.as_ref())),
        })
        .pipe(strip_nulls),
        Some("switch_mode") => json!({
            "type": "plain_text",
            "label": snapshot.title,
            "icon": "sparkles",
            "text": ctx.text.clone().or_else(|| stringify_unknown(snapshot.raw_input.as_ref())),
        })
        .pipe(strip_nulls),
        _ => default_detail(&ctx),
    }
}

trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl Pipe for Value {}

/// Drops top-level `null` members (our stand-in for JS `undefined`).
fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.into_iter().filter(|(_, v)| !v.is_null()).collect()),
        other => other,
    }
}

fn read_string(rec: Option<&Obj>, keys: &[&str]) -> Option<String> {
    let rec = rec?;
    keys.iter().find_map(|k| match rec.get(*k) {
        Some(Value::String(s)) if !js_trim(s).is_empty() => Some(s.clone()),
        _ => None,
    })
}

fn read_number(rec: Option<&Obj>, keys: &[&str]) -> Option<Value> {
    let rec = rec?;
    keys.iter().find_map(|k| match rec.get(*k) {
        Some(Value::Number(n)) if n.as_f64().is_some_and(f64::is_finite) => {
            Some(Value::Number(n.clone()))
        }
        _ => None,
    })
}

fn build_shell_command(rec: Option<&Obj>) -> Option<String> {
    let rec = rec?;
    let command = read_string(Some(rec), &["command"])?;
    let args: Vec<&str> = rec
        .get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    Some(if args.is_empty() {
        command
    } else {
        format!("{command} {}", args.join(" "))
    })
}

fn read_error_message(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(o)) => {
            read_string(Some(o), &["message", "error"]).unwrap_or_else(|| "Tool call failed".into())
        }
        _ => "Tool call failed".into(),
    }
}

fn stringify_unknown(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(json_stringify(other)),
    }
}

fn extract_tool_text(content: Option<&Vec<Value>>) -> Option<String> {
    let parts: Vec<String> = content?
        .iter()
        .filter(|i| i.get("type").and_then(Value::as_str) == Some("content"))
        .map(|i| content_block_to_text(i.get("content")))
        .filter(|t| !t.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn extract_diff(content: Option<&Vec<Value>>) -> Option<(Option<String>, Value)> {
    let diff = content?
        .iter()
        .find(|i| i.get("type").and_then(Value::as_str) == Some("diff"))?;
    let old = defined(diff.get("oldText"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some((old, diff.get("newText").cloned().unwrap_or(Value::Null)))
}

fn extract_terminal(
    content: Option<&Vec<Value>>,
    terminals: &HashMap<String, TerminalEntry>,
) -> Option<(String, Option<Value>)> {
    let terminal = content?
        .iter()
        .find(|i| i.get("type").and_then(Value::as_str) == Some("terminal"))?;
    let id = terminal.get("terminalId").and_then(Value::as_str)?;
    let entry = terminals.get(id)?;
    Some((
        entry.output.clone(),
        entry.exit_code.clone().filter(|v| !v.is_null()),
    ))
}

fn file_path(ctx: &DetailCtx) -> String {
    ctx.first_location
        .clone()
        .or_else(|| read_string(ctx.raw_input, &["path", "filePath", "file"]))
        .unwrap_or_else(|| ctx.snapshot.title.clone())
}

fn read_detail(ctx: &DetailCtx) -> Value {
    json!({
        "type": "read",
        "filePath": file_path(ctx),
        "content": ctx.text.clone().or_else(|| read_string(ctx.raw_output, &["content", "text"])),
        "offset": read_number(ctx.raw_input, &["offset", "line"]),
        "limit": read_number(ctx.raw_input, &["limit"]),
    })
    .pipe(strip_nulls)
}

fn edit_detail(ctx: &DetailCtx) -> Value {
    let old = ctx
        .diff
        .as_ref()
        .and_then(|(o, _)| o.clone())
        .or_else(|| read_string(ctx.raw_input, &["oldText", "oldString"]));
    let new = if ctx.snapshot.kind.as_deref() == Some("delete") {
        Some(String::new())
    } else {
        ctx.diff
            .as_ref()
            .and_then(|(_, n)| n.as_str().map(str::to_owned))
            .or_else(|| read_string(ctx.raw_input, &["newText", "newString"]))
    };
    json!({
        "type": "edit",
        "filePath": file_path(ctx),
        "oldString": old,
        "newString": new,
        "unifiedDiff": ctx.text,
    })
    .pipe(strip_nulls)
}

fn search_detail(ctx: &DetailCtx) -> Value {
    let file_paths = ctx.snapshot.locations.as_array().map(|l| {
        l.iter()
            .filter_map(|x| x.get("path").cloned())
            .collect::<Vec<_>>()
    });
    json!({
        "type": "search",
        "query": read_string(ctx.raw_input, &["query", "pattern"])
            .unwrap_or_else(|| ctx.snapshot.title.clone()),
        "toolName": "search",
        "content": ctx.text.clone().or_else(|| read_string(ctx.raw_output, &["content", "text"])),
        "filePaths": file_paths,
    })
    .pipe(strip_nulls)
}

fn shell_detail(ctx: &DetailCtx) -> Value {
    let command = build_shell_command(ctx.raw_input)
        .or_else(|| read_string(ctx.raw_input, &["command"]))
        .unwrap_or_else(|| ctx.snapshot.title.clone());
    let output = ctx
        .terminal
        .as_ref()
        .map(|(o, _)| o.clone())
        .or_else(|| ctx.text.clone())
        .or_else(|| read_string(ctx.raw_output, &["output", "text"]));
    let exit_code = ctx
        .terminal
        .as_ref()
        .and_then(|(_, e)| e.clone())
        .or_else(|| read_number(ctx.raw_output, &["exitCode"]));
    json!({
        "type": "shell",
        "command": command,
        "cwd": read_string(ctx.raw_input, &["cwd"]),
        "output": output,
        "exitCode": exit_code,
    })
    .pipe(strip_nulls)
}

fn fetch_detail(ctx: &DetailCtx) -> Value {
    json!({
        "type": "fetch",
        "url": read_string(ctx.raw_input, &["url"]).unwrap_or_else(|| ctx.snapshot.title.clone()),
        "prompt": read_string(ctx.raw_input, &["prompt"]),
        "result": ctx.text.clone().or_else(|| read_string(ctx.raw_output, &["result", "text", "content"])),
        "code": read_number(ctx.raw_output, &["status", "code"]),
    })
    .pipe(strip_nulls)
}

fn default_detail(ctx: &DetailCtx) -> Value {
    if let Some((output, exit)) = &ctx.terminal {
        return json!({
            "type": "shell",
            "command": ctx.snapshot.title,
            "output": output,
            "exitCode": exit,
        })
        .pipe(strip_nulls);
    }
    if let Some(text) = &ctx.text {
        return json!({
            "type": "plain_text",
            "label": ctx.snapshot.title,
            "text": text,
            "icon": "wrench",
        });
    }
    json!({
        "type": "unknown",
        "input": ctx.snapshot.raw_input.clone().unwrap_or(Value::Null),
        "output": ctx.snapshot.raw_output.clone().unwrap_or(Value::Null),
    })
}
