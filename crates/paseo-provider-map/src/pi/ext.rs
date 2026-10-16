//! Pi extension host: per-extension adapters that claim tool calls, dialogs, custom messages and
//! runtime notifications (`pi/extensions/*.ts`). Adapters are pure state machines over JSON.
//!
//! DEVIATION(pi-extension-io): the TS host also follows child-session files and polls the
//! pi-subagents `status.json` on a 250 ms timer. Those are file reads, so they live in the session
//! layer: the host only records the child sessions a mapping reports
//! ([`PiExtensionHost::take_child_sessions`]) and the pi-subagents `poll()` is not ported.

use paseo_agent_types::{Map, ToolCallDetail};
use paseo_tool_detail::detail::pi::{extract_text_from_tool_result, PiToolResult};
use paseo_tool_detail::js::js_trim;
use serde_json::{json, Value};

mod ask;
mod subagents;
mod tools;

use ask::{PiAskUser, RpivAskUserQuestion};
use subagents::{Gotgenes, PiSubagents, Tintinweb};
use tools::{PiExampleTodo, PiGoalX, PiMcpAdapter, RpivTodo};

/// `PiExtensionToolCall`.
#[derive(Debug, Clone)]
pub struct ExtToolCall {
    pub call_id: String,
    pub tool_name: String,
    pub args: Value,
    pub status: &'static str,
    pub result: PiToolResult,
}

impl ExtToolCall {
    fn details(&self) -> Option<&Value> {
        match &self.result {
            PiToolResult::Object(m) => m.get("details"),
            _ => None,
        }
    }
}

/// A Pi child session file a mapping reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSession {
    pub id: String,
    pub file: String,
}

/// `PiExtensionToolMapping` (also used for custom mappings, which carry no name/detail).
#[derive(Debug, Clone, Default)]
pub struct ExtMapping {
    pub name: Option<String>,
    pub detail: Option<ToolCallDetail>,
    pub timeline: Vec<Value>,
    pub subagents: Vec<Value>,
    pub child_sessions: Vec<ChildSession>,
}

impl ExtMapping {
    fn subagents(subagents: Vec<Value>, child_sessions: Vec<ChildSession>) -> Self {
        Self {
            subagents,
            child_sessions,
            ..Self::default()
        }
    }
    fn detail(detail: ToolCallDetail) -> Self {
        Self {
            detail: Some(detail),
            ..Self::default()
        }
    }
}

/// `PiExtensionDialogMapping`.
#[derive(Debug, Clone)]
pub enum DialogMapping {
    Permission(Value),
    Response(Value),
    Deferred,
}

/// `PiExtensionUiReply`: `(dialog id, response)` pairs.
pub type UiReply = Vec<(String, Value)>;

/// `PiExtensionSession` minus `poll`.
pub trait ExtSession {
    fn map_tool_call(&mut self, _call: &ExtToolCall) -> Option<ExtMapping> {
        None
    }
    fn map_runtime_notification(&mut self, _message: &str) -> Option<ExtMapping> {
        None
    }
    fn map_custom_message(&mut self, _message: &Value) -> Option<ExtMapping> {
        None
    }
    fn on_tool_start(&mut self, _call: &ExtToolCall, _provider: &str) -> Option<Value> {
        None
    }
    fn on_tool_end(&mut self, _call: &ExtToolCall) {}
    fn map_dialog(&mut self, _dialog: &Value, _provider: &str) -> Option<DialogMapping> {
        None
    }
    fn respond_to_permission(&mut self, _request: &Value, _response: &Value) -> Option<UiReply> {
        None
    }
}

/// A mapping prepared by the host: events ready to emit, child sessions to hydrate.
#[derive(Debug, Clone, Default)]
pub struct ExtOutput {
    pub name: Option<String>,
    pub detail: Option<ToolCallDetail>,
    /// `timeline` and `provider_subagent` stream events (no `turnId` yet).
    pub events: Vec<Value>,
    pub child_sessions: Vec<ChildSession>,
}

pub struct PiExtensionHost {
    sessions: Vec<Box<dyn ExtSession>>,
    child_sessions: Vec<ChildSession>,
}

impl Default for PiExtensionHost {
    fn default() -> Self {
        Self::new()
    }
}

impl PiExtensionHost {
    /// All registered extensions in `registry.ts` order.
    pub fn new() -> Self {
        let sessions: Vec<Box<dyn ExtSession>> = vec![
            Box::<PiAskUser>::default(),
            Box::<PiSubagents>::default(),
            Box::<Tintinweb>::default(),
            Box::<Gotgenes>::default(),
            Box::new(PiMcpAdapter),
            Box::new(RpivTodo),
            Box::new(PiExampleTodo),
            Box::new(PiGoalX),
            Box::<RpivAskUserQuestion>::default(),
        ];
        Self {
            sessions,
            child_sessions: Vec::new(),
        }
    }

    /// Child session files reported since the last call (the TS hydration/follow requests).
    pub fn take_child_sessions(&mut self) -> Vec<ChildSession> {
        std::mem::take(&mut self.child_sessions)
    }

    fn prepare(&mut self, mapping: ExtMapping) -> ExtOutput {
        let mut events: Vec<Value> = mapping
            .timeline
            .iter()
            .map(|item| json!({"type": "timeline", "provider": "pi", "item": item}))
            .collect();
        events.extend(
            mapping
                .subagents
                .iter()
                .map(|e| json!({"type": "provider_subagent", "provider": "pi", "event": e})),
        );
        self.child_sessions
            .extend(mapping.child_sessions.iter().cloned());
        ExtOutput {
            name: mapping.name,
            detail: mapping.detail,
            events,
            child_sessions: mapping.child_sessions,
        }
    }

    pub fn map_tool_call(&mut self, call: &ExtToolCall) -> Option<ExtOutput> {
        let mapping = self
            .sessions
            .iter_mut()
            .find_map(|s| s.map_tool_call(call))?;
        Some(self.prepare(mapping))
    }

    pub fn map_custom_message(&mut self, message: &Value) -> Option<ExtOutput> {
        let mapping = self
            .sessions
            .iter_mut()
            .find_map(|s| s.map_custom_message(message))?;
        Some(self.prepare(mapping))
    }

    pub fn map_runtime_notification(&mut self, message: &str) -> Option<ExtOutput> {
        let mapping = self
            .sessions
            .iter_mut()
            .find_map(|s| s.map_runtime_notification(message))?;
        Some(self.prepare(mapping))
    }

    /// Every adapter sees the call; the first request wins (`request ??= candidate`).
    pub fn on_tool_start(&mut self, call: &ExtToolCall, provider: &str) -> Option<Value> {
        let mut request = None;
        for s in &mut self.sessions {
            let candidate = s.on_tool_start(call, provider);
            if request.is_none() {
                request = candidate;
            }
        }
        request
    }

    pub fn on_tool_end(&mut self, call: &ExtToolCall) {
        for s in &mut self.sessions {
            s.on_tool_end(call);
        }
    }

    pub fn map_dialog(&mut self, dialog: &Value, provider: &str) -> Option<DialogMapping> {
        self.sessions
            .iter_mut()
            .find_map(|s| s.map_dialog(dialog, provider))
    }

    pub fn respond_to_permission(&mut self, request: &Value, response: &Value) -> Option<UiReply> {
        self.sessions
            .iter_mut()
            .find_map(|s| s.respond_to_permission(request, response))
    }
}

// ---- shared helpers -----------------------------------------------------------------------

pub(super) fn upsert(id: &str, fields: &[(&str, Value)]) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), json!("upsert"));
    m.insert("id".into(), json!(id));
    for (k, v) in fields {
        m.insert((*k).into(), v.clone());
    }
    Value::Object(m)
}

pub(super) fn log_text(result: &PiToolResult) -> String {
    extract_text_from_tool_result(result)
        .map(|t| js_trim(&t).to_string())
        .unwrap_or_default()
}

pub(super) fn answer_for(input: Option<&Value>, header: &str) -> Option<String> {
    let answers = input?.as_object()?.get("answers")?.as_object()?;
    answers.get(header)?.as_str().map(str::to_string)
}

pub(super) fn sub_agent_detail(
    sub_agent_type: Option<&str>,
    description: Option<&str>,
    log: String,
) -> ToolCallDetail {
    ToolCallDetail::SubAgent {
        sub_agent_type: sub_agent_type.map(str::to_string),
        description: description.map(str::to_string),
        child_session_id: None,
        log,
        actions: None,
    }
}

/// The subagent status mapping shared by the tintinweb and gotgenes forks.
pub(super) fn fork_status(value: &str, steered_completes: bool) -> &'static str {
    match value {
        "completed" => "completed",
        "steered" if steered_completes => "completed",
        "error" => "failed",
        "aborted" | "stopped" => "canceled",
        _ => "running",
    }
}
