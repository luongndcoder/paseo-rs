//! Actor-side pieces of `cancelAgentRun` (`agent-manager.ts:3149-3212`). The waits (interrupt
//! ack, settle timeouts) live in `manager::runs`; each step here is one atomic block.

use paseo_agent_types::AgentStreamEvent;

use super::run_state::RunTicket;
use super::AgentActor;
use crate::model::Lifecycle;

impl AgentActor {
    /// The run to cancel: the tracked run, or an autonomous run created on the spot when the
    /// lifecycle says `running`. `None` means `not_running`.
    pub(crate) fn cancel_begin(&mut self) -> Option<RunTicket> {
        self.run.ticket().or_else(|| {
            (self.view.lifecycle == Lifecycle::Running).then(|| self.run.track_autonomous_run(None))
        })
    }

    pub(crate) fn run_turn_id(&self) -> Option<String> {
        self.run.turn_id()
    }

    /// Force-feed an event past the gate (`dispatchSessionEvent` called directly).
    pub(crate) fn process_forced_event(&mut self, event: AgentStreamEvent) {
        self.process_event(event);
    }

    /// Acknowledged cancel whose pending foreground run never got a turn: clear it and drop to
    /// `idle` unless a replacement is coming.
    pub(crate) fn clear_pending_foreground_run(&mut self, token: &str) {
        self.run.settle_foreground_run(token);
        if !self.view.pending_replacement {
            self.view.lifecycle = Lifecycle::Idle;
            self.touch_updated_at();
            self.emit_state(true);
        }
    }

    /// After a cancel: deny what is still pending as `Interrupted`.
    pub(crate) fn resolve_interrupted_permissions(&mut self) {
        if self.view.pending_permissions.is_empty() {
            return;
        }
        let provider = self.view.provider.clone();
        self.resolve_pending_permissions(&provider, false, "Interrupted");
        self.touch_updated_at();
        self.emit_state(true);
    }
}
