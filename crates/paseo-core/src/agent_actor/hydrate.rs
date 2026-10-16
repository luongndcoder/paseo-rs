//! Actor side of timeline hydration from provider history
//! (`primeTimelineFromLegacyProviderHistory`, `forceHydrateTimelineFromLegacyProviderHistory`,
//! `agent-manager.ts:4142-4277`).
//!
//! The history is collected completely by the caller first (a stream that fails halfway leaves
//! the timeline untouched), then one actor step applies it: nothing interleaves with it.

use paseo_agent_types::{AgentStreamEvent, ProviderSubagentInputEvent};
use paseo_timeline::project_agent_message;
use paseo_timeline::store::SeedOptions;
use paseo_timeline::TimelineState;

use super::emit::limit_stream_event;
use super::timeline_record::RecordOptions;
use super::AgentActor;
use crate::events::ManagerEvent;
use crate::model::Broadcast;
use crate::provider_subagents::SubagentStoreEvent;

/// A timeline event of the history with its item already projected.
pub(crate) struct HistoryRow {
    pub event: AgentStreamEvent,
    pub timestamp: Option<String>,
}

/// The `timeline` and `provider_subagent` events of a history replay, in order. Every other
/// event type is ignored; a timeline item that projects to nothing is dropped.
#[derive(Default)]
pub(crate) struct CollectedHistory {
    pub rows: Vec<HistoryRow>,
    pub subagents: Vec<(String, ProviderSubagentInputEvent)>,
}

pub(crate) fn collect_history(raw: Vec<AgentStreamEvent>) -> CollectedHistory {
    let mut collected = CollectedHistory::default();
    for event in raw {
        match limit_stream_event(event) {
            AgentStreamEvent::ProviderSubagent { provider, event } => {
                collected.subagents.push((provider, *event));
            }
            AgentStreamEvent::Timeline {
                item,
                provider,
                turn_id,
                timestamp,
            } => {
                let Some(projected) = project_agent_message(&item) else {
                    continue;
                };
                collected.rows.push(HistoryRow {
                    timestamp: timestamp.clone(),
                    event: AgentStreamEvent::Timeline {
                        item: Box::new(projected),
                        provider,
                        turn_id,
                        timestamp,
                    },
                });
            }
            _ => {}
        }
    }
    collected
}

impl AgentActor {
    fn apply_subagent_input(
        &self,
        provider: &str,
        input: &ProviderSubagentInputEvent,
    ) -> ManagerEvent {
        let now = self.now_iso();
        let update = self
            .ctx
            .subagents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .apply(&self.view.id, provider, input, &now);
        self.subagent_event(update)
    }

    pub(crate) fn subagent_event(&self, update: SubagentStoreEvent) -> ManagerEvent {
        ManagerEvent::ProviderSubagent {
            event: update,
            parent_internal: self.view.internal,
        }
    }

    fn record_history_row(&mut self, row: &HistoryRow) -> paseo_timeline::Row {
        let AgentStreamEvent::Timeline { item, .. } = &row.event else {
            unreachable!("history rows are timeline events");
        };
        self.record_timeline(
            (**item).clone(),
            RecordOptions {
                timestamp: row.timestamp.clone(),
                ..RecordOptions::default()
            },
        )
    }

    /// Non-force hydration: the replay is APPENDED to the current timeline (same epoch, seq
    /// continues), provider children apply, `historyPrimed` becomes true. A deferred broadcast
    /// is read at the end, so a second loader can upgrade it while the history was streaming.
    pub(crate) fn apply_prime_history(&mut self, history: CollectedHistory, broadcast: &Broadcast) {
        let deferred = broadcast.is_deferred();
        let mut held_subagents = Vec::new();
        let mut held_rows = Vec::new();
        for (provider, input) in &history.subagents {
            let event = self.apply_subagent_input(provider, input);
            if deferred {
                held_subagents.push(event);
            } else if broadcast.now() {
                self.ctx.bus.emit(event);
            }
        }
        for row in &history.rows {
            let recorded = self.record_history_row(row);
            if deferred {
                held_rows.push((row.event.clone(), recorded));
            } else if broadcast.now() {
                self.dispatch_recorded_row(row.event.clone(), &recorded);
            }
        }
        self.view.history_primed = true;
        if !deferred || !broadcast.now() {
            return;
        }
        for event in held_subagents {
            self.ctx.bus.emit(event);
        }
        for (event, recorded) in held_rows {
            self.dispatch_recorded_row(event, &recorded);
        }
    }

    /// Force hydration: flush and discard the coalescer, mint a new timeline epoch, replace the
    /// provider children, record the replay, touch and emit state.
    pub(crate) fn apply_force_history(
        &mut self,
        history: CollectedHistory,
        broadcast: bool,
        broadcast_timeline: bool,
    ) {
        let flushes = self
            .coalescer
            .flush_and_discard(&self.view.id, tokio::time::Instant::now().into_std());
        self.apply_flushes(flushes);
        self.timeline = TimelineState::initialize(SeedOptions {
            timestamp: Some(self.now_iso()),
            ..SeedOptions::default()
        });
        self.view.history_primed = true;
        let removed = self
            .ctx
            .subagents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .delete_parent(&self.view.id);
        for update in removed {
            if broadcast {
                let event = self.subagent_event(update);
                self.ctx.bus.emit(event);
            }
        }
        for (provider, input) in &history.subagents {
            let event = self.apply_subagent_input(provider, input);
            if broadcast {
                self.ctx.bus.emit(event);
            }
        }
        for row in &history.rows {
            let recorded = self.record_history_row(row);
            if broadcast_timeline {
                self.dispatch_recorded_row(row.event.clone(), &recorded);
            }
        }
        self.touch_updated_at();
        self.emit_state(true);
    }
}
