//! Stream-event and timeline-item builders shared by the live mapper, the history mapper and the
//! extension host.

use paseo_agent_types::{AgentStreamEvent, Map, ToolCallDetail};
use paseo_tool_detail::detail::pi::PiTrackedToolCall;
use serde_json::{json, Value};

pub(super) fn decode_event(v: &Value) -> Option<AgentStreamEvent> {
    AgentStreamEvent::from_value(v).ok()
}

pub(super) fn tracked_tool_name(call: &PiTrackedToolCall) -> String {
    match call {
        PiTrackedToolCall::Bash { .. } => "bash".into(),
        PiTrackedToolCall::Read { .. } => "read".into(),
        PiTrackedToolCall::Edit { .. } => "edit".into(),
        PiTrackedToolCall::Write { .. } => "write".into(),
        PiTrackedToolCall::Find { .. } => "find".into(),
        PiTrackedToolCall::Grep { .. } => "grep".into(),
        PiTrackedToolCall::Ls { .. } => "ls".into(),
        PiTrackedToolCall::Unknown { tool_name, .. } => tool_name.clone(),
    }
}

/// `toolCall.args` as handed to the extension adapters. Known tools keep the fields the TS
/// parser retains; only `Unknown` carries the raw args the adapters read.
pub(super) fn tracked_args(call: &PiTrackedToolCall) -> Value {
    let mut m = Map::new();
    let mut put = |k: &str, v: Value| {
        m.insert(k.into(), v);
    };
    match call {
        PiTrackedToolCall::Bash { command, timeout } => {
            put("command", json!(command));
            if let Some(t) = timeout {
                put("timeout", Value::Number(t.clone()));
            }
        }
        PiTrackedToolCall::Read {
            path,
            offset,
            limit,
        } => {
            put("path", json!(path));
            if let Some(o) = offset {
                put("offset", Value::Number(o.clone()));
            }
            if let Some(l) = limit {
                put("limit", Value::Number(l.clone()));
            }
        }
        PiTrackedToolCall::Edit { path, edits } => {
            put("path", json!(path));
            put(
                "edits",
                Value::Array(
                    edits
                        .iter()
                        .map(|e| json!({"oldText": e.old_text, "newText": e.new_text}))
                        .collect(),
                ),
            );
        }
        PiTrackedToolCall::Write { path, content } => {
            put("path", json!(path));
            put("content", json!(content));
        }
        PiTrackedToolCall::Find {
            pattern,
            path,
            limit,
        } => {
            put("pattern", json!(pattern));
            if let Some(p) = path {
                put("path", json!(p));
            }
            if let Some(l) = limit {
                put("limit", Value::Number(l.clone()));
            }
        }
        PiTrackedToolCall::Grep { pattern, path } => {
            put("pattern", json!(pattern));
            if let Some(p) = path {
                put("path", json!(p));
            }
        }
        PiTrackedToolCall::Ls { path } => {
            if let Some(p) = path {
                put("path", json!(p));
            }
        }
        PiTrackedToolCall::Unknown { args, .. } => return args.clone(),
    }
    Value::Object(m)
}

/// A `tool_call` timeline item. `error: None` omits the key (JS `undefined`).
pub(super) fn tool_call_item_value(
    call_id: &str,
    name: &str,
    status: &str,
    detail: &ToolCallDetail,
    error: Option<Value>,
) -> Value {
    let mut item = Map::new();
    item.insert("type".into(), json!("tool_call"));
    item.insert("callId".into(), json!(call_id));
    item.insert("name".into(), json!(name));
    item.insert("status".into(), json!(status));
    item.insert("detail".into(), detail.to_value());
    if let Some(e) = error {
        item.insert("error".into(), e);
    }
    Value::Object(item)
}

/// `mapCustomMessageToToolCall` (`custom-message.ts`): extension context as an expandable tool row.
pub(super) fn custom_message_item(message: &Value, text: &str, call_id: &str) -> Value {
    let custom_type = match message.get("customType").and_then(Value::as_str) {
        Some(t) if !paseo_tool_detail::js::js_trim(t).is_empty() => t.to_string(),
        _ => "custom-message".to_string(),
    };
    let mut metadata = Map::new();
    metadata.insert("synthetic".into(), json!(true));
    metadata.insert("customType".into(), json!(custom_type));
    if let Some(d) = message.get("details") {
        metadata.insert("details".into(), d.clone());
    }
    json!({
        "type": "tool_call",
        "callId": call_id,
        "name": custom_type,
        "status": "completed",
        "detail": {"type": "plain_text", "text": text},
        "metadata": Value::Object(metadata),
        "error": Value::Null,
    })
}
