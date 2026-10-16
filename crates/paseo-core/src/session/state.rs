//! Mutable state of one session, owned by its actor: the sockets it serves, their capabilities
//! and the subscription registries layered on top of [`Delivery`](crate::delivery::Delivery).

use std::collections::{BTreeSet, HashSet};

use indexmap::IndexMap;

use crate::delivery::{Delivery, OwnedSubscription, SourceId};

use super::caps::{cap, legacy_wants_event, ClientCaps};

/// One physical socket of the session (`clientSources`).
#[derive(Debug, Clone)]
pub struct ClientSource {
    pub capabilities: ClientCaps,
    pub app_version: Option<String>,
}

/// `agent.timeline.set_subscription`: the agents whose streams one source follows.
#[derive(Debug, Clone)]
pub struct TimelineSub {
    pub owner: OwnedSubscription,
    pub agent_ids: BTreeSet<String>,
}

/// `session.events.set_subscription`: connection-wide event categories one source follows.
#[derive(Debug, Clone)]
pub struct EventSub {
    pub owner: OwnedSubscription,
    pub events: HashSet<String>,
    pub notifications: bool,
}

#[derive(Debug, Default)]
pub struct SessionState {
    pub clients: IndexMap<SourceId, ClientSource>,
    /// Capabilities of the most recent hello; what a source-less operation falls back to.
    pub client_caps: ClientCaps,
    pub app_version: Option<String>,
    pub timeline_subs: IndexMap<String, TimelineSub>,
    pub event_subs: IndexMap<String, EventSub>,
}

impl SessionState {
    /// `supportsForSource`.
    pub fn supports_for_source(&self, capability: &str, source: SourceId) -> bool {
        self.clients.get(&source).map_or_else(
            || self.client_caps.has(capability),
            |c| c.capabilities.has(capability),
        )
    }

    /// `supports` outside a request: the session-level capability set.
    pub fn supports(&self, capability: &str) -> bool {
        self.client_caps.has(capability)
    }

    /// `wantsEvent(event, source?)`.
    /// COMPAT(explicitEventSubscriptions): added in v0.8.0, remove legacy broadcasts after 2027-03-08.
    pub fn wants_event(&self, delivery: &Delivery, event: &str, source: Option<SourceId>) -> bool {
        for sub in self.event_subs.values() {
            if source.is_none_or(|s| sub.owner.source() == s) && sub.events.contains(event) {
                return true;
            }
        }
        if source.is_none() && !self.clients.is_empty() {
            return self
                .clients
                .keys()
                .any(|candidate| self.wants_event(delivery, event, Some(*candidate)));
        }
        if let Some(source) = source {
            if delivery.is_modern(source) {
                return false;
            }
        } else if !delivery.has_legacy_sources() {
            return false;
        }
        let caps = match source {
            Some(source) => self
                .clients
                .get(&source)
                .map_or(&self.client_caps, |c| &c.capabilities),
            None => &self.client_caps,
        };
        legacy_wants_event(event, caps)
    }

    /// `wantsSourceNotification`.
    pub fn wants_source_notification(
        &self,
        delivery: &Delivery,
        source: SourceId,
        event: &str,
    ) -> bool {
        // COMPAT(ownedSubscriptions): added in v0.8.0, remove legacy notification selection after 2027-03-09.
        if !delivery.is_modern(source) {
            return true;
        }
        self.event_subs
            .values()
            .any(|s| s.owner.source() == source && s.notifications && s.events.contains(event))
    }

    /// Whether `source` receives the stream of `agent_id` through an explicit subscription.
    pub fn follows_agent(&self, source: SourceId, agent_id: &str) -> bool {
        self.timeline_subs
            .values()
            .any(|s| s.owner.source() == source && s.agent_ids.contains(agent_id))
    }

    /// `selective_agent_timeline` clients receive streams only for their viewed set.
    pub fn is_selective(&self, source: SourceId) -> bool {
        self.supports_for_source(cap::SELECTIVE_AGENT_TIMELINE, source)
    }
}
