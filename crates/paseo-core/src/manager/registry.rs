//! Live-agent registry shared by the manager and its actors.

use std::collections::HashMap;
use std::sync::Mutex;

use indexmap::IndexMap;
use paseo_timeline::TimelineState;

use crate::agent_actor::ActorHandle;

/// `agents` plus the timelines TS retains across close (`timelineStore` survives
/// `prepareAgentForClosure`; only archive/delete discard it), so a resumed agent continues the
/// same timeline epoch.
///
/// Both maps hold tiny pure state; locks are never held across an `.await`.
#[derive(Default)]
pub(crate) struct Registry {
    pub agents: Mutex<IndexMap<String, ActorHandle>>,
    pub retained_timelines: Mutex<HashMap<String, TimelineState>>,
}

impl Registry {
    pub fn get(&self, id: &str) -> Option<ActorHandle> {
        self.agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    pub fn all(&self) -> Vec<ActorHandle> {
        self.agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// Remove `id` only if it still maps to `generation`.
    pub fn remove_generation(&self, id: &str, generation: u64) -> bool {
        let mut agents = self.agents.lock().unwrap_or_else(|e| e.into_inner());
        if agents.get(id).is_some_and(|h| h.generation == generation) {
            agents.shift_remove(id);
            true
        } else {
            false
        }
    }

    pub fn retain_timeline(&self, id: &str, timeline: TimelineState) {
        self.retained_timelines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_owned(), timeline);
    }

    pub fn take_retained_timeline(&self, id: &str) -> Option<TimelineState> {
        self.retained_timelines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
    }

    pub fn has_retained_timeline(&self, id: &str) -> bool {
        self.retained_timelines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(id)
    }
}
