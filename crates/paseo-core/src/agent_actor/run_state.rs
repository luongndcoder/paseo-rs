//! `RunSlot`: the single run slot per agent (`agent-run-state.ts`).
//!
//! `None | Foreground | Autonomous`. `settled` is a `watch<bool>` so any number of waiters can
//! observe it. `create_pending_run` overwrites without settling the old run (TS never resolves
//! its `settledPromise`); the old run's sender is parked so its waiters keep waiting instead of
//! seeing a closed channel.

use paseo_agent_types::AgentStreamEvent;
use tokio::sync::watch;
use uuid::Uuid;

use crate::model::RunStart;

/// Maximum finalized turn ids remembered per agent (`rememberFinalizedTurn`).
pub(crate) const FINALIZED_TURN_LIMIT: usize = 50;

#[derive(Debug)]
struct Settle {
    tx: watch::Sender<bool>,
}

impl Settle {
    fn new() -> (Self, watch::Receiver<bool>) {
        let (tx, rx) = watch::channel(false);
        (Self { tx }, rx)
    }

    fn settle(&self) {
        let _ = self.tx.send(true);
    }
}

#[derive(Debug)]
pub(crate) struct ForegroundRun {
    pub token: String,
    pub start: RunStart,
    pub staged: Vec<AgentStreamEvent>,
    settle: Settle,
}

#[derive(Debug)]
pub(crate) struct AutonomousRun {
    pub token: String,
    pub turn: Option<String>,
    settle: Settle,
}

#[derive(Debug)]
enum Run {
    Foreground(ForegroundRun),
    Autonomous(AutonomousRun),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunKind {
    Foreground,
    Autonomous,
}

/// What a caller needs to wait on or settle a run later.
#[derive(Debug, Clone)]
pub(crate) struct RunTicket {
    pub kind: RunKind,
    pub token: String,
    pub settled: watch::Receiver<bool>,
}

#[derive(Debug, Default)]
pub(crate) struct RunSlot {
    run: Option<Run>,
    orphaned: Vec<Settle>,
}

fn token() -> String {
    Uuid::new_v4().to_string()
}

impl RunSlot {
    pub fn has_run(&self) -> bool {
        self.run.is_some()
    }

    pub fn pending(&self) -> Option<&ForegroundRun> {
        match &self.run {
            Some(Run::Foreground(run)) => Some(run),
            _ => None,
        }
    }

    pub fn pending_mut(&mut self) -> Option<&mut ForegroundRun> {
        match &mut self.run {
            Some(Run::Foreground(run)) => Some(run),
            _ => None,
        }
    }

    /// `createPendingRun`.
    pub fn create_pending_run(&mut self) -> RunTicket {
        let (settle, settled) = Settle::new();
        let run = ForegroundRun {
            token: token(),
            start: RunStart::Pending,
            staged: Vec::new(),
            settle,
        };
        let ticket = RunTicket {
            kind: RunKind::Foreground,
            token: run.token.clone(),
            settled,
        };
        if let Some(old) = self.run.replace(Run::Foreground(run)) {
            self.orphaned.push(match old {
                Run::Foreground(r) => r.settle,
                Run::Autonomous(r) => r.settle,
            });
        }
        ticket
    }

    /// `getTurnId`.
    pub fn turn_id(&self) -> Option<String> {
        match self.run.as_ref()? {
            Run::Autonomous(run) => run.turn.clone(),
            Run::Foreground(run) => match &run.start {
                RunStart::Started { turn_id } => Some(turn_id.clone()),
                _ => None,
            },
        }
    }

    pub fn ticket(&self) -> Option<RunTicket> {
        match self.run.as_ref()? {
            Run::Foreground(run) => Some(RunTicket {
                kind: RunKind::Foreground,
                token: run.token.clone(),
                settled: run.settle.tx.subscribe(),
            }),
            Run::Autonomous(run) => Some(RunTicket {
                kind: RunKind::Autonomous,
                token: run.token.clone(),
                settled: run.settle.tx.subscribe(),
            }),
        }
    }

    /// `trackAutonomousRun`: an existing run wins and is returned unchanged.
    pub fn track_autonomous_run(&mut self, turn: Option<String>) -> RunTicket {
        if let Some(ticket) = self.ticket() {
            return ticket;
        }
        let (settle, settled) = Settle::new();
        let run = AutonomousRun {
            token: token(),
            turn,
            settle,
        };
        let ticket = RunTicket {
            kind: RunKind::Autonomous,
            token: run.token.clone(),
            settled,
        };
        self.run = Some(Run::Autonomous(run));
        ticket
    }

    /// `settleTerminalRun`. A foreground run settles only when `Started` with the same turn id;
    /// an autonomous run is ignored only when both ids are known and differ.
    pub fn settle_terminal_run(&mut self, turn_id: Option<&str>) {
        let ignore = match &self.run {
            None => return,
            Some(Run::Foreground(run)) => match &run.start {
                RunStart::Started { turn_id: started } => Some(started.as_str()) != turn_id,
                _ => true,
            },
            Some(Run::Autonomous(run)) => match (&run.turn, turn_id) {
                (Some(known), Some(event)) => known != event,
                _ => false,
            },
        };
        if !ignore {
            self.clear();
        }
    }

    /// `settleForegroundRun`: a token mismatch is ignored, so an old cancel cannot settle a
    /// newer run.
    pub fn settle_foreground_run(&mut self, token: &str) {
        if matches!(&self.run, Some(Run::Foreground(run)) if run.token == token) {
            self.clear();
        }
    }

    /// `clearAgentRun`.
    pub fn clear(&mut self) {
        if let Some(run) = self.run.take() {
            match run {
                Run::Foreground(r) => r.settle.settle(),
                Run::Autonomous(r) => r.settle.settle(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(slot: &mut RunSlot, turn: &str) {
        slot.create_pending_run();
        slot.pending_mut().unwrap().start = RunStart::Started {
            turn_id: turn.into(),
        };
    }

    #[test]
    fn foreground_settles_only_for_its_started_turn() {
        let mut slot = RunSlot::default();
        slot.create_pending_run();
        slot.settle_terminal_run(Some("t1"));
        assert!(slot.has_run(), "pending run ignores terminals");
        started(&mut slot, "t1");
        slot.settle_terminal_run(Some("t2"));
        assert!(slot.has_run(), "different turn is ignored");
        slot.settle_terminal_run(None);
        assert!(
            slot.has_run(),
            "id-less terminal never settles a foreground run"
        );
        slot.settle_terminal_run(Some("t1"));
        assert!(!slot.has_run());
    }

    #[test]
    fn autonomous_ignored_only_when_both_ids_differ() {
        let mut slot = RunSlot::default();
        slot.track_autonomous_run(Some("a".into()));
        slot.settle_terminal_run(Some("b"));
        assert!(slot.has_run());
        slot.settle_terminal_run(None);
        assert!(!slot.has_run());
        slot.track_autonomous_run(None);
        slot.settle_terminal_run(Some("anything"));
        assert!(!slot.has_run());
    }

    #[test]
    fn old_token_cannot_settle_a_newer_run() {
        let mut slot = RunSlot::default();
        let old = slot.create_pending_run();
        let new = slot.create_pending_run();
        slot.settle_foreground_run(&old.token);
        assert!(slot.has_run());
        slot.settle_foreground_run(&new.token);
        assert!(!slot.has_run());
        assert!(*new.settled.borrow());
        assert!(
            !*old.settled.borrow(),
            "an overwritten run never settles (TS parity)"
        );
    }

    #[test]
    fn track_autonomous_keeps_an_existing_run() {
        let mut slot = RunSlot::default();
        let fg = slot.create_pending_run();
        let tracked = slot.track_autonomous_run(Some("x".into()));
        assert_eq!(tracked.token, fg.token);
        assert_eq!(tracked.kind, RunKind::Foreground);
    }
}
