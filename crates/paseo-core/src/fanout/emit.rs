//! `emit` and `emitSubscribedEvent`: routing of a message that is not a plain reply.

use std::collections::HashSet;

use serde_json::{json, Value};

use crate::delivery::{Delivery, ReqCtx, SourceId};
use crate::session::caps::session_event_category;
use crate::session::state::{EventSub, SessionState};

pub struct Emitter<'a> {
    state: &'a SessionState,
    delivery: &'a Delivery,
}

impl<'a> Emitter<'a> {
    pub fn new(state: &'a SessionState, delivery: &'a Delivery) -> Self {
        Self { state, delivery }
    }

    /// `emit(msg)`: authorize, reply to the running request if it is one, deliver an event
    /// category to its subscribers, else broadcast (modern sources drop an unowned frame).
    pub fn emit(&self, ctx: Option<&ReqCtx>, message: Value) {
        if !self.delivery.is_allowed(&message) {
            return;
        }
        if let Some(ctx) = ctx {
            if ctx.reply(message.clone()) {
                return;
            }
        }
        if self.emit_subscribed_event(message.clone(), None) {
            return;
        }
        self.broadcast(message);
    }

    /// Every attached socket (`onMessage`).
    pub fn broadcast(&self, message: Value) {
        for source in self.delivery.sources() {
            self.delivery.emit_unowned(source, message.clone());
        }
    }

    fn emit_event_to_owner(
        &self,
        sub: &EventSub,
        message: &Value,
        notified: &mut HashSet<SourceId>,
    ) {
        let ty = message
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if ty != "agent_attention_required" && ty != "terminal_attention_required" {
            sub.owner.emit(message.clone());
            return;
        }
        let should_notify = message
            .get("payload")
            .and_then(|p| p.get("shouldNotify"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && sub.notifications
            && !notified.contains(&sub.owner.source());
        let mut message = message.clone();
        if let Some(payload) = message.get_mut("payload").and_then(Value::as_object_mut) {
            payload.insert("shouldNotify".into(), json!(should_notify));
        }
        sub.owner.emit(message);
        if should_notify {
            notified.insert(sub.owner.source());
        }
    }

    /// `emitSubscribedEvent`. Returns whether the message belongs to an event category at all
    /// (the caller stops routing it even when nobody wanted it).
    pub fn emit_subscribed_event(&self, message: Value, only_source: Option<SourceId>) -> bool {
        let Some(event) = session_event_category(&message) else {
            return false;
        };
        if !self.delivery.is_allowed(&message) {
            return true;
        }
        let mut delivered = HashSet::new();
        let mut notified = HashSet::new();
        for sub in self.state.event_subs.values() {
            if !sub.events.contains(event) || only_source.is_some_and(|s| sub.owner.source() != s) {
                continue;
            }
            self.emit_event_to_owner(sub, &message, &mut notified);
            delivered.insert(sub.owner.source());
        }
        // COMPAT(ownedSubscriptions): added in v0.8.0, remove implicit event delivery after 2027-03-09.
        if !self.state.clients.is_empty() {
            for source in self.state.clients.keys() {
                if only_source.is_none_or(|s| s == *source)
                    && !delivered.contains(source)
                    && !self.delivery.is_modern(*source)
                    && self.state.wants_event(self.delivery, event, Some(*source))
                {
                    self.delivery.emit_unowned(*source, message.clone());
                }
            }
        } else if delivered.is_empty() && self.state.wants_event(self.delivery, event, None) {
            self.broadcast(message);
        }
        true
    }
}
