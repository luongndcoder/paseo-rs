//! Pi history conversion (`pi/history-mapper.ts`): `get_messages` output -> stream events, and the
//! incremental child-session transcript mapper (`pi/extensions/child-session.ts`).
//!
//! DEVIATION(pi-history-hooks): the TS `PiHistoryMapperHooks` (`mapCustomMessage`,
//! `resolveToolCallId`, `mapToolDetail`) exist for the OMP wrapper, which is host-only in P4, so
//! they are not ported. The default behavior (no hooks) is what the Pi provider uses.

use std::collections::HashMap;

use paseo_agent_types::{AgentStreamEvent, Map};
use paseo_tool_detail::detail::pi::{
    extract_text_from_tool_result, map_tool_detail, parse_tool_args, parse_tool_result,
    PiToolResult, PiTrackedToolCall,
};
use paseo_tool_detail::js::js_number_to_string;
use serde_json::{json, Value};

use super::ext::{ChildSession, ExtOutput, ExtToolCall, PiExtensionHost};
use super::{
    custom_message_item, decode_event, tool_call_item_value, tracked_args, tracked_tool_name,
};

/// `PiCapturedUserMessageEntry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiCapturedUserMessageEntry {
    pub id: String,
    pub text: String,
}

/// `getUserMessageText`: text blocks joined with a blank line (a string is returned as is).
pub fn user_message_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| {
                let m = b.as_object()?;
                (m.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| m.get("text").and_then(Value::as_str))
                    .flatten()
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

pub struct PiHistoryMapper {
    provider: String,
    user_entries: Vec<PiCapturedUserMessageEntry>,
    host: PiExtensionHost,
    pending_tool_calls: HashMap<String, PiTrackedToolCall>,
    child_sessions: Vec<ChildSession>,
    user_index: usize,
    assistant_index: usize,
    custom_index: usize,
}

impl PiHistoryMapper {
    pub fn new(provider: &str, user_entries: Vec<PiCapturedUserMessageEntry>) -> Self {
        Self {
            provider: provider.to_string(),
            user_entries,
            host: PiExtensionHost::new(),
            pending_tool_calls: HashMap::new(),
            child_sessions: Vec::new(),
            user_index: 0,
            assistant_index: 0,
            custom_index: 0,
        }
    }

    /// Child session files the extensions reported while mapping. The TS `hydrate()` reads each
    /// file (see [`PiChildSessionMapper`]) and appends the resulting `provider_subagent` events
    /// after all messages.
    pub fn child_sessions(&self) -> &[ChildSession] {
        &self.child_sessions
    }

    pub fn map_messages(&mut self, messages: &[Value]) -> Vec<AgentStreamEvent> {
        let mut events = Vec::new();
        for message in messages {
            let role = message.get("role").and_then(Value::as_str);
            let mapped: Vec<Value> = match role {
                Some("user") => self.map_user_message(message),
                Some("custom") => self.map_custom_message(message),
                Some("assistant") => self.map_assistant_message(message),
                Some("toolResult") => self.map_tool_result_message(message),
                Some("bashExecution") => vec![self.map_bash_execution_message(message)],
                _ => Vec::new(),
            };
            events.extend(mapped.iter().filter_map(decode_event));
        }
        events
    }

    fn timeline(&self, item: Value) -> Value {
        json!({"type": "timeline", "provider": self.provider, "item": item})
    }

    fn extension_events(&mut self, output: Option<&ExtOutput>) -> Vec<Value> {
        let Some(output) = output else {
            return Vec::new();
        };
        self.child_sessions
            .extend(output.child_sessions.iter().cloned());
        output.events.clone()
    }

    fn map_user_message(&mut self, message: &Value) -> Vec<Value> {
        let text = user_message_text(message.get("content"));
        self.user_index += 1;
        if text.is_empty() {
            return Vec::new();
        }
        let mut item = json!({"type": "user_message", "text": text});
        if let Some(entry) = self.user_entries.get(self.user_index - 1) {
            item["messageId"] = json!(entry.id);
        }
        vec![self.timeline(item)]
    }

    fn map_custom_message(&mut self, message: &Value) -> Vec<Value> {
        let mapping = self.host.map_custom_message(message);
        let mut events = self.extension_events(mapping.as_ref());
        if message.get("display") == Some(&Value::Bool(false)) {
            return events;
        }
        let text = user_message_text(message.get("content"));
        if !text.is_empty() {
            // PARITY-QUIRK(pi-custom-call-id-live-vs-history): see the live mapper.
            self.custom_index += 1;
            let call_id = format!("{}-custom-{}", self.provider, self.custom_index);
            events.push(self.timeline(custom_message_item(message, &text, &call_id)));
        }
        events
    }

    fn map_assistant_message(&mut self, message: &Value) -> Vec<Value> {
        let mut events = Vec::new();
        self.assistant_index += 1;
        let message_id = match message.get("responseId").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => format!(
                "{}-history-assistant-{}",
                self.provider, self.assistant_index
            ),
        };
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            return events;
        };
        for block in content {
            let Some(m) = block.as_object() else {
                continue;
            };
            match m.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = m
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty())
                    {
                        events.push(self.timeline(json!({
                            "type": "assistant_message", "text": text, "messageId": message_id,
                        })));
                    }
                }
                Some("thinking") => {
                    if let Some(text) = m
                        .get("thinking")
                        .and_then(Value::as_str)
                        .filter(|t| !t.is_empty())
                    {
                        events.push(self.timeline(json!({"type": "reasoning", "text": text})));
                    }
                }
                Some("toolCall") => events.extend(self.map_tool_call_block(m)),
                _ => {}
            }
        }
        events
    }

    fn map_tool_call_block(&mut self, m: &Map) -> Vec<Value> {
        let id = m
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let name = m.get("name").and_then(Value::as_str).unwrap_or("");
        let tracked = parse_tool_args(name, m.get("arguments"));
        self.pending_tool_calls.insert(id.clone(), tracked.clone());
        let tool_name = tracked_tool_name(&tracked);
        let mapping = self.host.map_tool_call(&ExtToolCall {
            call_id: id.clone(),
            tool_name: tool_name.clone(),
            args: tracked_args(&tracked),
            status: "running",
            result: PiToolResult::Null,
        });
        let detail = mapping
            .as_ref()
            .and_then(|o| o.detail.clone())
            .unwrap_or_else(|| map_tool_detail(&tracked, None));
        let item_name = mapping
            .as_ref()
            .and_then(|o| o.name.clone())
            .unwrap_or(tool_name);
        let mut events = vec![self.timeline(tool_call_item_value(
            &id,
            &item_name,
            "running",
            &detail,
            Some(Value::Null),
        ))];
        events.extend(self.extension_events(mapping.as_ref()));
        events
    }

    fn map_tool_result_message(&mut self, message: &Value) -> Vec<Value> {
        let call_id = message
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let tracked = self.pending_tool_calls.remove(&call_id).unwrap_or_else(|| {
            parse_tool_args(
                message
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                None,
            )
        });
        let mut raw = Map::new();
        if let Some(c) = message.get("content") {
            raw.insert("content".into(), c.clone());
        }
        if let Some(d) = message.get("details") {
            raw.insert("details".into(), d.clone());
        }
        let result = parse_tool_result(Some(&Value::Object(raw)));
        let is_error = js_truthy(message.get("isError"));
        let tool_name = tracked_tool_name(&tracked);
        let mapping = self.host.map_tool_call(&ExtToolCall {
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
            args: tracked_args(&tracked),
            status: if is_error { "failed" } else { "completed" },
            result: result.clone(),
        });
        let detail = mapping
            .as_ref()
            .and_then(|o| o.detail.clone())
            .unwrap_or_else(|| map_tool_detail(&tracked, Some(&result)));
        let name = mapping
            .as_ref()
            .and_then(|o| o.name.clone())
            .unwrap_or(tool_name);
        let item = if is_error {
            let text = extract_text_from_tool_result(&result)
                .unwrap_or_else(|| "Tool call failed".to_string());
            tool_call_item_value(&call_id, &name, "failed", &detail, Some(json!(text)))
        } else {
            tool_call_item_value(&call_id, &name, "completed", &detail, Some(Value::Null))
        };
        let mut events = vec![self.timeline(item)];
        events.extend(self.extension_events(mapping.as_ref()));
        events
    }

    fn map_bash_execution_message(&self, message: &Value) -> Value {
        let mut detail = Map::new();
        detail.insert("type".into(), json!("shell"));
        detail.insert(
            "command".into(),
            message.get("command").cloned().unwrap_or(Value::Null),
        );
        if let Some(output) = message.get("output") {
            detail.insert("output".into(), output.clone());
        }
        detail.insert(
            "exitCode".into(),
            match message.get("exitCode") {
                None | Some(Value::Null) => Value::Null,
                Some(v) => v.clone(),
            },
        );
        let timestamp = match message.get("timestamp") {
            Some(Value::Number(n)) => js_number_to_string(n.as_f64().unwrap_or(0.0)),
            Some(other) => paseo_tool_detail::js::js_to_string(other),
            None => "undefined".into(),
        };
        let status = if js_truthy(message.get("cancelled")) {
            "canceled"
        } else {
            "completed"
        };
        self.timeline(json!({
            "type": "tool_call",
            "callId": format!("pi-bash-{timestamp}"),
            "name": "bash",
            "status": status,
            "detail": Value::Object(detail),
            "error": Value::Null,
        }))
    }
}

fn js_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(_) => true,
    }
}

/// `streamPiHistory` without the child-session hydration: all message events, in order. Read the
/// reported [`PiHistoryMapper::child_sessions`] afterwards for the `provider_subagent` tail.
pub fn map_pi_history(
    provider: &str,
    messages: &[Value],
    user_entries: Vec<PiCapturedUserMessageEntry>,
) -> (Vec<AgentStreamEvent>, Vec<ChildSession>) {
    let mut mapper = PiHistoryMapper::new(provider, user_entries);
    let events = mapper.map_messages(messages);
    (events, mapper.child_sessions)
}

const CHILD_MAX_ITEMS: usize = 200;

/// Incremental mapper over one append-only child transcript (`PiChildSessionFollower` minus the
/// file reads): feed complete JSONL rows, get `provider_subagent` timeline events back. The
/// 2 MiB byte cap and the partial-row buffering stay with the file reader.
pub struct PiChildSessionMapper {
    id: String,
    mapper: PiHistoryMapper,
    items: usize,
}

impl PiChildSessionMapper {
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            mapper: PiHistoryMapper::new("pi", Vec::new()),
            items: 0,
        }
    }

    pub fn map_line(&mut self, line: &str) -> Vec<AgentStreamEvent> {
        if self.items >= CHILD_MAX_ITEMS {
            return Vec::new();
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let Some(message) = entry
            .get("message")
            .filter(|m| m.as_object().is_some_and(|o| o.contains_key("role")))
        else {
            return Vec::new();
        };
        let timestamp = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty());
        let mut out = Vec::new();
        for event in self.mapper.map_messages(std::slice::from_ref(message)) {
            let AgentStreamEvent::Timeline { item, .. } = event else {
                continue;
            };
            if self.items >= CHILD_MAX_ITEMS {
                break;
            }
            let item = paseo_tool_detail::limit_item_content(*item);
            let mut sub = json!({"type": "timeline", "id": self.id, "item": item.to_value()});
            if let Some(t) = timestamp {
                sub["timestamp"] = json!(t);
            }
            let ev = json!({"type": "provider_subagent", "provider": "pi", "event": sub});
            if let Some(e) = decode_event(&ev) {
                self.items += 1;
                out.push(e);
            }
        }
        out
    }
}
