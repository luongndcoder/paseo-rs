//! Port of activity/terminal-activity-tracker.ts. Returns transitions instead of firing listeners.

use crate::protocol::ActivityState;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Finished,
    NeedsInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub state: Option<ActivityState>,
    pub reason: Option<Reason>,
    pub changed_at: u64,
}

impl Snapshot {
    /// `TerminalActivity | null`.
    pub fn to_json(self) -> Value {
        let Some(state) = self.state else {
            return Value::Null;
        };
        let mut v = json!({
            "state": match state {
                ActivityState::Idle => "idle",
                ActivityState::Working => "working",
                ActivityState::Attention => "attention",
            },
            "changedAt": self.changed_at,
        });
        if let Some(reason) = self.reason {
            v["attentionReason"] = json!(match reason {
                Reason::Finished => "finished",
                Reason::NeedsInput => "needs_input",
            });
        }
        v
    }
}

pub type Transition = (Snapshot, Snapshot);

pub struct ActivityTracker {
    state: Option<ActivityState>,
    reason: Option<Reason>,
    changed_at: u64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

impl Default for ActivityTracker {
    fn default() -> Self {
        Self {
            state: None,
            reason: None,
            changed_at: now_ms(),
        }
    }
}

impl ActivityTracker {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            state: self.state,
            reason: self.reason,
            changed_at: self.changed_at,
        }
    }

    pub fn set(&mut self, state: ActivityState) -> Option<Transition> {
        if state == ActivityState::Idle && self.state == Some(ActivityState::Working) {
            return self.set_state(Some(ActivityState::Idle), Some(Reason::Finished));
        }
        if state == ActivityState::Idle && self.reason == Some(Reason::Finished) {
            return None;
        }
        if state == ActivityState::Attention {
            self.set_state(Some(ActivityState::Idle), Some(Reason::NeedsInput))
        } else {
            self.set_state(Some(state), None)
        }
    }

    /// Silent: `clear()` on dispose never notifies (the terminal is already killed).
    pub fn clear(&mut self) {
        self.state = None;
        self.reason = None;
    }

    pub fn clear_attention(&mut self) -> Option<Transition> {
        self.reason?;
        self.set_state(Some(ActivityState::Idle), None)
    }

    pub fn interrupt(&mut self) -> Option<Transition> {
        if self.state != Some(ActivityState::Working) {
            return None;
        }
        self.set_state(None, None)
    }

    fn set_state(
        &mut self,
        state: Option<ActivityState>,
        reason: Option<Reason>,
    ) -> Option<Transition> {
        if state == self.state && reason == self.reason {
            return None;
        }
        let previous = self.snapshot();
        self.state = state;
        self.reason = reason;
        self.changed_at = now_ms();
        Some((self.snapshot(), previous))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_then_idle_finishes_with_attention() {
        let mut t = ActivityTracker::default();
        assert!(t.set(ActivityState::Working).is_some());
        let (now, prev) = t.set(ActivityState::Idle).unwrap();
        assert_eq!(now.reason, Some(Reason::Finished));
        assert_eq!(prev.state, Some(ActivityState::Working));
        assert!(t.set(ActivityState::Idle).is_none());
        assert!(t.clear_attention().is_some());
        assert!(t.clear_attention().is_none());
    }

    #[test]
    fn attention_maps_to_idle_needs_input_and_interrupt_clears_working() {
        let mut t = ActivityTracker::default();
        let (now, _) = t.set(ActivityState::Attention).unwrap();
        assert_eq!(now.to_json()["attentionReason"], "needs_input");
        assert_eq!(now.to_json()["state"], "idle");
        t.set(ActivityState::Working);
        t.interrupt().unwrap();
        assert_eq!(t.snapshot().to_json(), Value::Null);
    }
}
