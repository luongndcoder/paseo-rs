//! `EventGate`: where a provider event goes at enqueue time.
//!
//! Highest priority first (`enqueueSessionEvent`, `agent-manager.ts:3908-3919`):
//! 1. a steer barrier is installed: hold the event on the barrier;
//! 2. a foreground run is `Pending`: stage it on the run;
//! 3. otherwise it goes to the event lane (the actor processes it).
//!
//! Barrier release and the accept block re-route every held event through [`EventGate::route`]
//! again, so a released event can be staged again by a replacement run.
//!
//! PARITY-QUIRK(rewind-staged-events-dropped): staged events are dropped when the run settles
//! without reaching `Started`; rewind parks a pending run as a lock and relies on this.

use paseo_agent_types::AgentStreamEvent;

use super::run_state::RunSlot;
use crate::model::RunStart;

#[derive(Debug)]
struct Barrier {
    id: u64,
    events: Vec<AgentStreamEvent>,
}

#[derive(Debug, Default)]
pub(crate) struct EventGate {
    barrier: Option<Barrier>,
    next_barrier_id: u64,
}

impl EventGate {
    /// `Some(event)` when the event goes to the event lane; `None` when it was held on a barrier or
    /// staged on a pending run.
    pub fn route(
        &mut self,
        run: &mut RunSlot,
        event: AgentStreamEvent,
    ) -> Option<AgentStreamEvent> {
        if let Some(barrier) = &mut self.barrier {
            barrier.events.push(event);
            return None;
        }
        if let Some(pending) = run.pending_mut() {
            if pending.start == RunStart::Pending {
                pending.staged.push(event);
                return None;
            }
        }
        Some(event)
    }

    #[cfg(test)]
    pub fn has_barrier(&self) -> bool {
        self.barrier.is_some()
    }

    /// Install a steer barrier and return its id.
    pub fn install_barrier(&mut self) -> u64 {
        self.next_barrier_id += 1;
        self.barrier = Some(Barrier {
            id: self.next_barrier_id,
            events: Vec::new(),
        });
        self.next_barrier_id
    }

    /// Remove barrier `id` (if it is still current) and hand back what it held.
    pub fn release_barrier(&mut self, id: u64) -> Vec<AgentStreamEvent> {
        match self.barrier.take() {
            Some(barrier) if barrier.id == id => barrier.events,
            other => {
                self.barrier = other;
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(text: &str) -> AgentStreamEvent {
        AgentStreamEvent::ThreadStarted {
            session_id: text.into(),
            provider: "codex".into(),
        }
    }

    fn id_of(event: &AgentStreamEvent) -> String {
        match event {
            AgentStreamEvent::ThreadStarted { session_id, .. } => session_id.clone(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn barrier_beats_pending_run_beats_lane() {
        let mut gate = EventGate::default();
        let mut run = RunSlot::default();
        assert!(gate.route(&mut run, ev("lane")).is_some());

        run.create_pending_run();
        assert!(gate.route(&mut run, ev("staged")).is_none());
        assert_eq!(run.pending().unwrap().staged.len(), 1);

        let id = gate.install_barrier();
        assert!(gate.route(&mut run, ev("barrier")).is_none());
        assert_eq!(
            run.pending().unwrap().staged.len(),
            1,
            "barrier wins over staging"
        );
        let released = gate.release_barrier(id);
        assert_eq!(released.iter().map(id_of).collect::<Vec<_>>(), ["barrier"]);
        assert!(!gate.has_barrier());
    }

    #[test]
    fn a_started_run_does_not_stage() {
        let mut gate = EventGate::default();
        let mut run = RunSlot::default();
        run.create_pending_run();
        run.pending_mut().unwrap().start = RunStart::Started {
            turn_id: "t".into(),
        };
        assert!(gate.route(&mut run, ev("x")).is_some());
    }

    #[test]
    fn releasing_a_stale_barrier_id_is_a_no_op() {
        let mut gate = EventGate::default();
        let mut run = RunSlot::default();
        let first = gate.install_barrier();
        let second = gate.install_barrier();
        gate.route(&mut run, ev("held"));
        assert!(gate.release_barrier(first).is_empty());
        assert!(gate.has_barrier());
        assert_eq!(gate.release_barrier(second).len(), 1);
    }
}
