//! Stream fan-out (`session.ts` `forwardAgentStream`, `deliverTimelineReplacement`,
//! `emit`/`emitSubscribedEvent`): who receives which manager event.
//!
//! The rules, all COMPAT-gated by what each socket advertised:
//! - `attention_required` is suppressed for modern and selective sockets, which get the separate
//!   `agent_attention_required` message through event subscriptions instead.
//! - `notification` and `plugin` timeline items only reach sockets that understand them
//!   (COMPAT(timelineItemCapabilities), remove after 2027-03-07).
//! - Timeline delivery targets are the agent ids in each socket's `agent.timeline.set_subscription`
//!   set. Sockets that are neither modern nor selective still get every agent's stream implicitly
//!   (COMPAT(ownedSubscriptions), remove after 2027-03-09).
//! - Agent updates are async through the per-agent tail and streams are immediate; callers keep
//!   that asymmetry.
//!
//! DEVIATION(outbound-authz-on-direct-sends): TS skips `allowsOutbound` for the implicit legacy
//! sends below (they go straight to the socket callback). Here every send is authorized; the only
//! sessions that can differ (Hub principals) never reach a Rust session in P4b.

mod emit;

use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem};
use serde_json::{json, Map, Value};

use crate::delivery::{Delivery, SourceId};
use crate::session::caps::cap;
use crate::session::state::SessionState;

pub use emit::Emitter;

/// What one manager `agent_stream` event carries besides the event itself.
#[derive(Debug, Clone)]
pub struct StreamMeta<'a> {
    pub agent_id: &'a str,
    pub seq: Option<i64>,
    pub epoch: Option<&'a str>,
    pub timestamp: Option<&'a str>,
}

/// `supportsTimelineItem`. COMPAT(timelineItemCapabilities): plugin items added in v0.8.0,
/// notifications in v0.7.2. Remove after 2027-03-07 once the supported client floor is >= v0.8.0.
pub fn supports_timeline_item(
    state: &SessionState,
    item_type: &str,
    source: Option<SourceId>,
) -> bool {
    let capability = match item_type {
        "notification" => cap::TIMELINE_NOTIFICATIONS,
        "plugin" => cap::PLUGIN_TIMELINE_ITEMS,
        _ => return true,
    };
    match source {
        Some(source) => state.supports_for_source(capability, source),
        None => state.supports(capability),
    }
}

/// `buildAgentStreamPayload`.
pub fn build_agent_stream_payload(
    meta: &StreamMeta<'_>,
    wire_event: &Value,
    now_iso: &str,
) -> Value {
    let mut payload = Map::new();
    payload.insert("agentId".into(), meta.agent_id.into());
    payload.insert("event".into(), wire_event.clone());
    payload.insert("timestamp".into(), meta.timestamp.unwrap_or(now_iso).into());
    if let Some(seq) = meta.seq {
        payload.insert("seq".into(), seq.into());
    }
    if let Some(epoch) = meta.epoch {
        payload.insert("epoch".into(), epoch.into());
    }
    Value::Object(payload)
}

fn timeline_item_type(wire_event: &Value) -> Option<&str> {
    if wire_event.get("type").and_then(Value::as_str) != Some("timeline") {
        return None;
    }
    wire_event.get("item")?.get("type")?.as_str()
}

/// `forwardAgentStream`. `wire_event` is `to_wire(event)`; `now_iso` stamps events that carry
/// no timestamp of their own.
pub fn forward_agent_stream(
    state: &SessionState,
    delivery: &Delivery,
    meta: &StreamMeta<'_>,
    wire_event: &Value,
    now_iso: &str,
) {
    let emitter = Emitter::new(state, delivery);
    let attention = wire_event.get("type").and_then(Value::as_str) == Some("attention_required");
    if attention {
        let mut payload = Map::new();
        payload.insert("agentId".into(), meta.agent_id.into());
        for key in ["reason", "timestamp", "shouldNotify"] {
            payload.insert(
                key.into(),
                wire_event.get(key).cloned().unwrap_or(Value::Null),
            );
        }
        if let Some(notification) = wire_event.get("notification").filter(|v| !v.is_null()) {
            payload.insert("notification".into(), notification.clone());
        }
        emitter.emit(
            None,
            json!({ "type": "agent_attention_required", "payload": Value::Object(payload) }),
        );
    }
    let message = json!({
        "type": "agent_stream",
        "payload": build_agent_stream_payload(meta, wire_event, now_iso),
    });
    let item_type = timeline_item_type(wire_event);
    for sub in state.timeline_subs.values() {
        let source = sub.owner.source();
        if !sub.agent_ids.contains(meta.agent_id) {
            continue;
        }
        if attention && (delivery.is_modern(source) || state.is_selective(source)) {
            continue;
        }
        if item_type.is_some_and(|t| !supports_timeline_item(state, t, Some(source))) {
            continue;
        }
        sub.owner.emit(message.clone());
    }
    // COMPAT(ownedSubscriptions): added in v0.8.0, remove implicit timeline delivery after 2027-03-09.
    for (source, client) in &state.clients {
        if delivery.is_modern(*source) || client.capabilities.has(cap::SELECTIVE_AGENT_TIMELINE) {
            continue;
        }
        if item_type.is_some_and(|t| !supports_timeline_item(state, t, Some(*source))) {
            continue;
        }
        if !state.follows_agent(*source, meta.agent_id) {
            delivery.emit_unowned(*source, message.clone());
        }
    }
    if state.clients.is_empty()
        && !state.supports(cap::SELECTIVE_AGENT_TIMELINE)
        && state.timeline_subs.is_empty()
    {
        emitter.emit(None, message);
    }
}

/// A timeline row the way a reconstructed legacy replay needs it.
#[derive(Debug, Clone)]
pub struct ReplayRow {
    pub item: AgentTimelineItem,
    pub turn_id: Option<String>,
    pub timestamp: String,
    pub seq: i64,
}

/// `deliverTimelineReplacement` after the timeline was read: invalidation for subscribers that
/// understand it, reconstructed rows for sockets that do not. `rows` and `epoch` are the
/// agent's timeline at replacement time.
///
/// COMPAT(timelineReplacementInvalidation): added in v0.5.0, remove reconstructed replay after
/// 2027-02-21.
#[allow(clippy::too_many_arguments)]
pub fn deliver_timeline_replacement(
    state: &SessionState,
    delivery: &Delivery,
    agent_id: &str,
    provider: &str,
    epoch: &str,
    rows: &[ReplayRow],
    initiating_source: Option<SourceId>,
) {
    let reconstruct = |source: Option<SourceId>| {
        for row in rows {
            let item_type = row.item.type_name();
            if !supports_timeline_item(state, item_type, source) {
                continue;
            }
            let event = AgentStreamEvent::Timeline {
                item: Box::new(row.item.clone()),
                provider: provider.to_owned(),
                turn_id: row.turn_id.clone().filter(|t| !t.is_empty()),
                timestamp: Some(row.timestamp.clone()),
            };
            let Some(wire) = paseo_agent_types::to_wire(&event) else {
                continue;
            };
            let message = json!({
                "type": "agent_stream",
                "payload": {
                    "agentId": agent_id,
                    "event": wire.into_value(),
                    "timestamp": row.timestamp,
                    "seq": row.seq,
                    "epoch": epoch,
                },
            });
            match source {
                Some(source) => delivery.emit_unowned(source, message),
                None => Emitter::new(state, delivery).emit(None, message),
            }
        }
    };
    if state.clients.is_empty() {
        if !state.supports(cap::TIMELINE_REPLACEMENT_INVALIDATION) {
            reconstruct(None);
        }
        return;
    }
    for sub in state.timeline_subs.values() {
        let source = sub.owner.source();
        if !sub.agent_ids.contains(agent_id) {
            continue;
        }
        let modern = delivery.is_modern(source);
        if modern
            || (Some(source) != initiating_source
                && state.supports_for_source(cap::TIMELINE_REPLACEMENT_INVALIDATION, source))
        {
            sub.owner.emit(json!({
                "type": "agent.timeline.replacement",
                "payload": { "agentId": agent_id, "epoch": epoch },
            }));
        }
    }
    for (source, client) in &state.clients {
        if !delivery.is_modern(*source)
            && !client
                .capabilities
                .has(cap::TIMELINE_REPLACEMENT_INVALIDATION)
        {
            reconstruct(Some(*source));
        }
    }
}
