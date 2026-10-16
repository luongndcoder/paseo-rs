//! Turn lifecycle derived from raw events. Pure state machine, no I/O.

use crate::flavor::Flavor;
use crate::protocol::{AssistantMessage, Event, Message};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed { final_text: String },
    Failed { error: String },
    Canceled { reason: String },
}

struct ActiveTurn {
    id: String,
    last_assistant: Option<AssistantMessage>,
}

/// Decides when a prompt's turn is over.
///
/// Pi: `agent_end` ends the turn unless it carries `willRetry`, in which case `agent_settled`
/// does. OMP: `agent_end` ends the turn once an assistant message was seen (it can end an
/// internal notice cycle first).
pub struct TurnTracker {
    flavor: Flavor,
    next_turn: u64,
    active: Option<ActiveTurn>,
    pending_settled: Option<Vec<Message>>,
    interrupting: bool,
}

impl TurnTracker {
    pub fn new(flavor: Flavor) -> Self {
        Self {
            flavor,
            next_turn: 1,
            active: None,
            pending_settled: None,
            interrupting: false,
        }
    }

    /// Starts a turn and returns its id.
    pub fn begin(&mut self) -> String {
        self.begin_superseding().0
    }

    /// Like `begin`, also returning the terminal outcome of a turn it replaced (if any).
    pub fn begin_superseding(&mut self) -> (String, Option<(String, TurnOutcome)>) {
        let superseded = self.active.take().map(|old| {
            (
                old.id,
                TurnOutcome::Canceled {
                    reason: "superseded".into(),
                },
            )
        });
        let id = format!("turn-{}", self.next_turn);
        self.next_turn += 1;
        self.active = Some(ActiveTurn {
            id: id.clone(),
            last_assistant: None,
        });
        self.pending_settled = None;
        self.interrupting = false;
        (id, superseded)
    }

    pub fn active_turn_id(&self) -> Option<&str> {
        self.active.as_ref().map(|a| a.id.as_str())
    }

    pub fn mark_interrupting(&mut self) {
        if self.active.is_some() {
            self.interrupting = true;
        }
    }

    /// Ends the active turn as canceled (after a successful abort).
    pub fn cancel_active(&mut self, reason: &str) -> Option<(String, TurnOutcome)> {
        self.finish_with(TurnOutcome::Canceled {
            reason: reason.to_string(),
        })
    }

    pub fn fail_active(&mut self, error: &str) -> Option<(String, TurnOutcome)> {
        self.finish_with(TurnOutcome::Failed {
            error: error.to_string(),
        })
    }

    /// The provider handled the prompt without invoking the model (`agentInvoked: false`).
    pub fn complete_without_agent(&mut self) -> Option<(String, TurnOutcome)> {
        self.finish_with(TurnOutcome::Completed {
            final_text: String::new(),
        })
    }

    fn finish_with(&mut self, outcome: TurnOutcome) -> Option<(String, TurnOutcome)> {
        let turn = self.active.take()?;
        self.pending_settled = None;
        self.interrupting = false;
        Some((turn.id, outcome))
    }

    pub fn on_event(&mut self, event: &Event) -> Option<(String, TurnOutcome)> {
        match event {
            Event::MessageEnd(m) => {
                if let (Some(turn), Message::Assistant(msg)) = (self.active.as_mut(), &m.message) {
                    turn.last_assistant = Some(msg.clone());
                }
                None
            }
            Event::AgentEnd(end) => {
                let turn = self.active.as_ref()?;
                let messages = end.messages.clone().unwrap_or_default();
                match self.flavor {
                    Flavor::Pi if end.will_retry.is_some() => {
                        self.pending_settled = Some(messages);
                        None
                    }
                    Flavor::Pi => Some(self.finish(messages)),
                    Flavor::Omp => {
                        let has_assistant = messages.iter().any(|m| m.as_assistant().is_some())
                            || turn.last_assistant.is_some();
                        has_assistant.then(|| self.finish(messages))
                    }
                }
            }
            Event::AgentSettled(settled) if self.flavor == Flavor::Pi => {
                self.active.as_ref()?;
                let messages = self.pending_settled.take().unwrap_or_default();
                let (id, outcome) = self.finish(messages);
                if settled.aborted == Some(true) && matches!(outcome, TurnOutcome::Completed { .. })
                {
                    return Some((
                        id,
                        TurnOutcome::Canceled {
                            reason: "aborted".into(),
                        },
                    ));
                }
                Some((id, outcome))
            }
            Event::ProcessExit(exit) => self.fail_active(&exit.error),
            _ => None,
        }
    }

    fn finish(&mut self, messages: Vec<Message>) -> (String, TurnOutcome) {
        let turn = self.active.take().expect("finish requires an active turn");
        let interrupting = std::mem::take(&mut self.interrupting);
        self.pending_settled = None;
        let terminal = messages
            .iter()
            .rev()
            .find_map(|m| m.as_assistant().cloned())
            .or(turn.last_assistant);
        let outcome = if interrupting {
            TurnOutcome::Canceled {
                reason: "interrupted".into(),
            }
        } else {
            match &terminal {
                Some(msg) if msg.stop_reason.as_deref() == Some("aborted") => {
                    TurnOutcome::Canceled {
                        reason: "aborted".into(),
                    }
                }
                Some(msg)
                    if msg.stop_reason.as_deref() == Some("error")
                        || msg.error_message.is_some() =>
                {
                    TurnOutcome::Failed {
                        error: msg
                            .error_message
                            .clone()
                            .unwrap_or_else(|| "The provider ended the turn with an error".into()),
                    }
                }
                Some(msg) => TurnOutcome::Completed {
                    final_text: msg.text(),
                },
                None => TurnOutcome::Completed {
                    final_text: String::new(),
                },
            }
        };
        (turn.id, outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(v: serde_json::Value) -> Event {
        Event::from_value(v)
    }

    fn assistant_end(text: &str, stop: &str) -> Event {
        ev(json!({"type":"message_end","message":{"role":"assistant",
            "content":[{"type":"text","text":text}],"stopReason":stop}}))
    }

    #[test]
    fn pi_agent_end_without_will_retry_completes_and_settled_is_then_ignored() {
        let mut t = TurnTracker::new(Flavor::Pi);
        let id = t.begin();
        assert!(t.on_event(&assistant_end("pong", "stop")).is_none());
        let done = t
            .on_event(&ev(json!({"type":"agent_end","messages":[]})))
            .unwrap();
        assert_eq!(
            done,
            (
                id,
                TurnOutcome::Completed {
                    final_text: "pong".into()
                }
            )
        );
        assert!(t
            .on_event(&ev(json!({"type":"agent_settled","aborted":false})))
            .is_none());
    }

    #[test]
    fn pi_will_retry_waits_for_settled() {
        let mut t = TurnTracker::new(Flavor::Pi);
        t.begin();
        assert!(t
            .on_event(&ev(
                json!({"type":"agent_end","willRetry":true,"messages":[]})
            ))
            .is_none());
        t.on_event(&assistant_end("ok", "stop"));
        let (_, outcome) = t.on_event(&ev(json!({"type":"agent_settled"}))).unwrap();
        assert_eq!(
            outcome,
            TurnOutcome::Completed {
                final_text: "ok".into()
            }
        );
    }

    #[test]
    fn error_stop_reason_fails_the_turn() {
        let mut t = TurnTracker::new(Flavor::Pi);
        t.begin();
        t.on_event(&ev(
            json!({"type":"message_end","message":{"role":"assistant","content":[],
            "stopReason":"error","errorMessage":"401 bad key"}}),
        ));
        let (_, outcome) = t.on_event(&ev(json!({"type":"agent_end"}))).unwrap();
        assert_eq!(
            outcome,
            TurnOutcome::Failed {
                error: "401 bad key".into()
            }
        );
    }

    #[test]
    fn interrupt_cancels_even_if_the_provider_reports_a_normal_end() {
        let mut t = TurnTracker::new(Flavor::Pi);
        t.begin();
        t.mark_interrupting();
        let (_, outcome) = t.on_event(&ev(json!({"type":"agent_end"}))).unwrap();
        assert_eq!(
            outcome,
            TurnOutcome::Canceled {
                reason: "interrupted".into()
            }
        );
    }

    #[test]
    fn omp_ignores_notice_only_cycles() {
        let mut t = TurnTracker::new(Flavor::Omp);
        t.begin();
        assert!(t
            .on_event(&ev(json!({"type":"agent_end","messages":[]})))
            .is_none());
        t.on_event(&assistant_end("hi", "stop"));
        assert!(t
            .on_event(&ev(json!({"type":"agent_end","messages":[]})))
            .is_some());
    }

    #[test]
    fn begin_reports_the_replaced_turn() {
        let mut t = TurnTracker::new(Flavor::Pi);
        let first = t.begin();
        let (second, old) = t.begin_superseding();
        assert_ne!(first, second);
        assert_eq!(
            old,
            Some((
                first,
                TurnOutcome::Canceled {
                    reason: "superseded".into()
                }
            ))
        );
    }

    #[test]
    fn cancel_active_ends_the_turn() {
        let mut t = TurnTracker::new(Flavor::Pi);
        let id = t.begin();
        t.mark_interrupting();
        let (got, outcome) = t.cancel_active("interrupted").unwrap();
        assert_eq!(got, id);
        assert!(matches!(outcome, TurnOutcome::Canceled { .. }));
        assert!(t.active_turn_id().is_none());
    }

    #[test]
    fn process_exit_fails_the_active_turn() {
        let mut t = TurnTracker::new(Flavor::Pi);
        t.begin();
        let (_, outcome) = t
            .on_event(&ev(json!({"type":"process_exit","error":"boom"})))
            .unwrap();
        assert_eq!(
            outcome,
            TurnOutcome::Failed {
                error: "boom".into()
            }
        );
    }
}
