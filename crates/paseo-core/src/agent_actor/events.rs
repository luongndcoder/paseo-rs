//! The provider event pipeline (`dispatchSessionEvent`, `handleStreamEvent`).
//!
//! Fixed order of `handle_stream_event` (spec section 3):
//! `limit_item_content` (timeline only) -> turn identity -> a terminal whose turn id was
//! already finalized is dropped (no dispatch, no waiter notify) -> live only: `touch_updated_at`
//! and the coalescer (buffered events return here; their waiters are notified from the flush
//! with the recorded event), otherwise flush this agent's buffer -> terminal: active turn
//! terminal -> per-type handler (may clear the dispatch/notify flags) -> live terminal: settle
//! the run, finalize the foreground turn when the event belongs to it -> live and still
//! dispatching: `dispatch_stream` with a timestamp only.
//!
//! Foreground waiters are notified after all of this with the RAW provider event (pre-limit,
//! pre-identity), matched on the raw turn id.

use paseo_agent_types::{AgentStreamEvent, ProviderSubagentInputEvent};
use paseo_timeline::coalescer::{is_coalescable, Flush};

use super::emit::{limit_stream_event, StreamMeta};
use super::turn_identity::{
    apply_active_turn_terminal, attach_turn_identity, is_foreground_event, TerminalDisposition,
};
use super::AgentActor;
use crate::events::ManagerEvent;

/// Whether the handler still wants the event dispatched / its waiters notified.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StreamFlags {
    pub dispatch: bool,
    pub notify_waiters: bool,
}

impl Default for StreamFlags {
    fn default() -> Self {
        Self {
            dispatch: true,
            notify_waiters: true,
        }
    }
}

pub(crate) fn is_terminal(event: &AgentStreamEvent) -> bool {
    matches!(
        event,
        AgentStreamEvent::TurnCompleted { .. }
            | AgentStreamEvent::TurnFailed { .. }
            | AgentStreamEvent::TurnCanceled { .. }
    )
}

impl AgentActor {
    /// `dispatchSessionEvent`. Also the entry for a forced event that bypasses the gate
    /// (cancel's synthesized `turn_canceled`).
    pub(crate) fn process_event(&mut self, event: AgentStreamEvent) {
        if let AgentStreamEvent::ProviderSubagent {
            provider,
            event: input,
        } = &event
        {
            self.apply_provider_subagent(provider, input);
            return;
        }
        let matching = self.waiters.matching(event.turn_id());
        let notify = self.handle_stream_event(event.clone(), false);
        if !notify {
            return;
        }
        // PARITY-QUIRK(waiter-raw-event): waiters get the raw provider event, matched on the raw
        // turn id; the recorded (limited, identified) event would be the better thing to deliver.
        self.waiters.notify(&matching, &event, is_terminal(&event));
    }

    fn apply_provider_subagent(&mut self, provider: &str, input: &ProviderSubagentInputEvent) {
        let now = self.now_iso();
        let update = self
            .ctx
            .subagents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .apply(&self.view.id, provider, input, &now);
        self.ctx.bus.emit(ManagerEvent::ProviderSubagent {
            event: update,
            parent_internal: self.view.internal,
        });
    }

    /// `handleStreamEvent`. Returns whether waiters should be notified.
    pub(crate) fn handle_stream_event(
        &mut self,
        event: AgentStreamEvent,
        from_history: bool,
    ) -> bool {
        let event = limit_stream_event(event);
        let identified = attach_turn_identity(&self.view, event, from_history);
        let mut event = identified.event;
        let event_turn = identified.turn_id;
        let is_foreground = is_foreground_event(&self.view, event_turn.as_deref());
        // PARITY-QUIRK(falsy-id-dropped): `eventTurnId && ...` is JS truthiness, so an empty turn id
        // counts as absent.
        let turn = event_turn.as_deref().filter(|t| !t.is_empty());
        if let Some(turn) = turn {
            if is_terminal(&event) && self.finalized.contains(turn) {
                return false;
            }
        }

        if !from_history {
            self.touch_updated_at();
            if is_coalescable(&event) {
                let now = tokio::time::Instant::now().into_std();
                let flushes = self.coalescer.handle(&self.view.id, &event, now);
                self.apply_flushes(flushes);
                return false;
            }
            self.flush_coalescer();
        }

        let disposition = if is_terminal(&event) {
            apply_active_turn_terminal(&mut self.view, event_turn.as_deref(), from_history)
        } else {
            TerminalDisposition::Untracked
        };

        let mut flags = StreamFlags::default();
        self.handle_by_type(
            &mut event,
            from_history,
            is_foreground,
            event_turn.as_deref(),
            disposition,
            &mut flags,
        );

        if !from_history {
            if is_terminal(&event) {
                self.run.settle_terminal_run(event_turn.as_deref());
                if is_foreground {
                    self.finalize_foreground_turn(event_turn.as_deref());
                }
            }
            if flags.dispatch {
                // PARITY-QUIRK(stale-terminal-dispatch): a stale terminal event is dispatched
                // although its handler ignored it.
                let timestamp = self.now_iso();
                self.dispatch_stream(event, StreamMeta::timestamp_only(timestamp));
            }
        }
        flags.notify_waiters
    }

    /// `agentStreamCoalescer.flushFor(agent)`.
    pub(crate) fn flush_coalescer(&mut self) {
        let now = tokio::time::Instant::now().into_std();
        let flushes = self.coalescer.flush_for(&self.view.id, now);
        self.apply_flushes(flushes);
    }

    /// The coalescer's `onFlush`: record and dispatch the row, then notify foreground waiters
    /// of the event as recorded.
    pub(crate) fn apply_flushes(&mut self, flushes: Vec<Flush>) {
        for flush in flushes {
            let event = self.record_and_dispatch_timeline_item(
                flush.item,
                &flush.provider,
                flush.turn_id,
                None,
            );
            let matching = self.waiters.matching(event.turn_id());
            self.waiters.notify(&matching, &event, false);
        }
    }
}
