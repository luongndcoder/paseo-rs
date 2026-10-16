//! `to_wire`: the daemon-to-client allow-list for stream events.
//!
//! Mirrors `serializeAgentStreamEvent` + `AgentStreamEventPayloadSchema` in
//! `packages/protocol/src/messages.ts`. zod objects strip unknown keys and emit keys in schema
//! order, so output here is rebuilt field by field in schema order. An event that fails the
//! schema returns `None` (TS returns `null` and the daemon drops it silently; callers should
//! `tracing::warn!`).
//!
//! Rules worth knowing:
//! - Only 9 of the 14 event types have a wire form. `usage_updated`, `mode_changed`,
//!   `model_changed`, `thinking_option_changed`, `provider_subagent` return `None`.
//! - `tool_call.error` must be `null` for running/completed/canceled and a non-null value for
//!   failed. A missing key is invalid for every status.
//! - `unknown` detail always carries `input` and `output` (null when absent).
//! - `attention_required` gains `shouldNotify: false`; the edge sets the real value per client.
//! - `timeline.timestamp` is not part of the wire event and is dropped.

use crate::json::*;
use crate::permission::{PermissionRequest, PermissionResponse};
use crate::stream_event::AgentStreamEvent;
use crate::timeline_item::AgentTimelineItem;
use crate::tool_call::{ToolCallDetail, ToolCallItem, ToolCallStatus};
use crate::usage::AgentUsage;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct WireEvent(pub Value);

impl WireEvent {
    pub fn into_value(self) -> Value {
        self.0
    }

    pub fn as_value(&self) -> &Value {
        &self.0
    }

    /// `JSON.stringify` of the event, writing lone-surrogate carriers as `\udXXX` escapes.
    pub fn to_json_string(&self) -> String {
        crate::js_str::to_js_json_string(&self.0)
    }

    pub fn type_name(&self) -> &str {
        self.0.get("type").and_then(Value::as_str).unwrap_or("")
    }

    /// Wrap in a `session`/`agent_stream` frame and ask `paseo-protocol` whether a generated
    /// outbound type accepts it.
    pub fn validates_against_protocol(&self) -> bool {
        let frame = serde_json::json!({
            "type": "session",
            "message": {
                "type": "agent_stream",
                "payload": {
                    "agentId": "agent-wire-check",
                    "event": self.0,
                    "timestamp": "2026-01-01T00:00:00.000Z",
                },
            },
        });
        paseo_protocol::Frame::from_value(frame)
            .typed()
            .is_some_and(|t| t.direction == paseo_protocol::Direction::Outbound)
    }
}

fn is_safe_int(n: &Num) -> bool {
    if n.is_i64() || n.is_u64() {
        return n.as_i64().is_none_or(|i| i.unsigned_abs() < (1u64 << 53))
            && n.as_u64().is_none_or(|u| u < (1u64 << 53));
    }
    n.as_f64()
        .is_some_and(|f| f.fract() == 0.0 && f.abs() <= 9007199254740991.0)
}

fn is_positive_safe_int(n: &Num) -> bool {
    is_safe_int(n) && n.as_f64().is_some_and(|f| f > 0.0)
}

fn is_non_negative(n: &Num) -> bool {
    n.as_f64().is_some_and(|f| f >= 0.0)
}

pub fn wire_tool_call_detail(d: &ToolCallDetail) -> Option<Value> {
    use ToolCallDetail as D;
    match d {
        D::WorktreeSetup { commands, .. } => {
            for c in commands {
                if !is_positive_safe_int(&c.index) {
                    return None;
                }
                if c.duration_ms.as_ref().is_some_and(|n| !is_non_negative(n)) {
                    return None;
                }
            }
            Some(d.to_value())
        }
        D::SubAgent { actions, .. } => {
            if let Some(a) = actions {
                if a.iter().any(|x| !is_positive_safe_int(&x.index)) {
                    return None;
                }
            }
            // wire strips nothing here beyond unknown keys, which typed values do not have
            Some(d.to_value())
        }
        D::Search(s) => {
            // webResults hits are z.object({title,url}): extras stripped on the wire.
            let mut v = d.to_value();
            if let (Some(w), Some(obj)) = (&s.web_results, v.as_object_mut()) {
                let stripped = w
                    .iter()
                    .map(|r| {
                        let mut m = Map::new();
                        m.insert("title".into(), Value::String(r.title.clone()));
                        m.insert("url".into(), Value::String(r.url.clone()));
                        Value::Object(m)
                    })
                    .collect();
                obj.insert("webResults".into(), Value::Array(stripped));
            }
            Some(v)
        }
        _ => Some(d.to_value()),
    }
}

pub fn wire_tool_call(t: &ToolCallItem) -> Option<Value> {
    let detail = wire_tool_call_detail(&t.detail)?;
    // error rule
    let error = match (t.status, &t.error) {
        (ToolCallStatus::Failed, Some(e)) if !e.is_null() => e.clone(),
        (ToolCallStatus::Failed, _) => return None,
        (_, Some(Value::Null)) => Value::Null,
        (_, _) => return None,
    };
    if let Some(a) = &t.agent_message {
        if !a.is_wire_valid() {
            return None;
        }
    }
    let mut m = Map::new();
    if let Some(a) = &t.agent_message {
        m.insert("agentMessage".into(), a.to_value());
    }
    m.insert("type".into(), "tool_call".into());
    m.insert("callId".into(), t.call_id.clone().into());
    m.insert("name".into(), t.name.clone().into());
    m.insert("detail".into(), detail);
    if let Some(md) = &t.metadata {
        m.insert("metadata".into(), Value::Object(md.clone()));
    }
    m.insert("status".into(), t.status.as_str().into());
    m.insert("error".into(), error);
    Some(Value::Object(m))
}

fn plugin_id_valid(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn wire_timeline_item(item: &AgentTimelineItem) -> Option<Value> {
    match item {
        AgentTimelineItem::ToolCall(t) => wire_tool_call(t),
        AgentTimelineItem::Plugin { plugin_id, .. } => {
            if !plugin_id_valid(plugin_id) {
                return None;
            }
            Some(item.to_value())
        }
        // compaction extras are stripped by the wire schema
        AgentTimelineItem::Compaction {
            status,
            trigger,
            pre_tokens,
            ..
        } => Some(
            AgentTimelineItem::Compaction {
                status: *status,
                trigger: *trigger,
                pre_tokens: pre_tokens.clone(),
                extra: Map::new(),
            }
            .to_value(),
        ),
        _ => Some(item.to_value()),
    }
}

pub fn wire_permission_request(r: &PermissionRequest) -> Option<Value> {
    let mut v = r.to_value();
    if let Some(d) = &r.detail {
        v.as_object_mut()?
            .insert("detail".into(), wire_tool_call_detail(d)?);
    }
    Some(v)
}

fn wire_usage(u: &AgentUsage) -> Value {
    u.to_value()
}

fn wire_resolution(r: &PermissionResponse) -> Value {
    r.to_value()
}

pub fn to_wire(event: &AgentStreamEvent) -> Option<WireEvent> {
    let mut m = Map::new();
    m.insert("type".into(), event.type_name().into());
    match event {
        AgentStreamEvent::ThreadStarted {
            session_id,
            provider,
        } => {
            m.insert("sessionId".into(), session_id.clone().into());
            m.insert("provider".into(), provider.clone().into());
        }
        AgentStreamEvent::TurnStarted { provider, turn_id } => {
            m.insert("provider".into(), provider.clone().into());
            put_opt_str(&mut m, "turnId", turn_id);
        }
        AgentStreamEvent::TurnCompleted {
            provider,
            usage,
            turn_id,
        } => {
            m.insert("provider".into(), provider.clone().into());
            put_opt_str(&mut m, "turnId", turn_id);
            if let Some(u) = usage {
                m.insert("usage".into(), wire_usage(u));
            }
        }
        AgentStreamEvent::TurnFailed {
            provider,
            error,
            code,
            diagnostic,
            turn_id,
        } => {
            m.insert("provider".into(), provider.clone().into());
            put_opt_str(&mut m, "turnId", turn_id);
            m.insert("error".into(), error.clone().into());
            put_opt_str(&mut m, "code", code);
            put_opt_str(&mut m, "diagnostic", diagnostic);
        }
        AgentStreamEvent::TurnCanceled {
            provider,
            reason,
            turn_id,
        } => {
            m.insert("provider".into(), provider.clone().into());
            put_opt_str(&mut m, "turnId", turn_id);
            m.insert("reason".into(), reason.clone().into());
        }
        AgentStreamEvent::Timeline {
            item,
            provider,
            turn_id,
            ..
        } => {
            m.insert("provider".into(), provider.clone().into());
            m.insert("item".into(), wire_timeline_item(item)?);
            put_opt_str(&mut m, "turnId", turn_id);
        }
        AgentStreamEvent::PermissionRequested {
            provider, request, ..
        } => {
            m.insert("provider".into(), provider.clone().into());
            m.insert("request".into(), wire_permission_request(request)?);
        }
        AgentStreamEvent::PermissionResolved {
            provider,
            request_id,
            resolution,
            ..
        } => {
            m.insert("provider".into(), provider.clone().into());
            m.insert("requestId".into(), request_id.clone().into());
            m.insert("resolution".into(), wire_resolution(resolution));
        }
        AgentStreamEvent::AttentionRequired {
            provider,
            reason,
            timestamp,
        } => {
            m.insert("provider".into(), provider.clone().into());
            m.insert("reason".into(), reason.as_str().into());
            m.insert("timestamp".into(), timestamp.clone().into());
            m.insert("shouldNotify".into(), Value::Bool(false));
        }
        AgentStreamEvent::UsageUpdated { .. }
        | AgentStreamEvent::ModeChanged { .. }
        | AgentStreamEvent::ModelChanged { .. }
        | AgentStreamEvent::ThinkingOptionChanged { .. }
        | AgentStreamEvent::ProviderSubagent { .. } => return None,
    }
    Some(WireEvent(Value::Object(m)))
}
