//! Foreground turn waiters (`ForegroundTurnWaiter`): one per `stream_agent` consumer.
//!
//! A waiter is an unbounded channel of events for one turn id. Settling closes the channel
//! after the queue drains, which ends the consumer's stream.

use paseo_agent_types::AgentStreamEvent;
use tokio::sync::mpsc;

#[derive(Debug)]
struct Waiter {
    id: u64,
    turn_id: String,
    tx: Option<mpsc::UnboundedSender<AgentStreamEvent>>,
    settled: bool,
}

impl Waiter {
    fn settle(&mut self) {
        self.settled = true;
        self.tx = None;
    }
}

#[derive(Debug, Default)]
pub(crate) struct Waiters {
    list: Vec<Waiter>,
    next_id: u64,
}

impl Waiters {
    pub fn add(&mut self, turn_id: &str) -> (u64, mpsc::UnboundedReceiver<AgentStreamEvent>) {
        self.next_id += 1;
        let (tx, rx) = mpsc::unbounded_channel();
        self.list.push(Waiter {
            id: self.next_id,
            turn_id: turn_id.to_owned(),
            tx: Some(tx),
            settled: false,
        });
        (self.next_id, rx)
    }

    /// `deleteWaiter`: remove and settle.
    pub fn delete(&mut self, id: u64) {
        self.list.retain(|w| w.id != id);
    }

    /// `getMatchingWaiters`: unsettled waiters of `turn_id`. An absent id matches nothing.
    pub fn matching(&self, turn_id: Option<&str>) -> Vec<u64> {
        let Some(turn_id) = turn_id else {
            return Vec::new();
        };
        self.list
            .iter()
            .filter(|w| w.turn_id == turn_id && !w.settled)
            .map(|w| w.id)
            .collect()
    }

    /// `notifyWaiters`: deliver, and settle on a terminal event.
    pub fn notify(&mut self, ids: &[u64], event: &AgentStreamEvent, terminal: bool) {
        for waiter in self.list.iter_mut().filter(|w| ids.contains(&w.id)) {
            if let Some(tx) = &waiter.tx {
                let _ = tx.send(event.clone());
            }
            if terminal {
                waiter.settle();
            }
        }
    }

    /// `cancelWaiters`: every waiter gets a synthesized event, then settles.
    pub fn cancel_all(&mut self, make_event: impl Fn(&str) -> AgentStreamEvent) {
        for waiter in &mut self.list {
            if let Some(tx) = &waiter.tx {
                let _ = tx.send(make_event(&waiter.turn_id));
            }
            waiter.settle();
        }
        self.list.clear();
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.list.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed(turn: &str) -> AgentStreamEvent {
        AgentStreamEvent::TurnCompleted {
            provider: "codex".into(),
            usage: None,
            turn_id: Some(turn.into()),
        }
    }

    #[test]
    fn terminal_notify_settles_and_closes_the_channel_after_the_event() {
        let mut waiters = Waiters::default();
        let (id, mut rx) = waiters.add("t1");
        assert_eq!(waiters.matching(Some("t1")), vec![id]);
        waiters.notify(&[id], &completed("t1"), true);
        assert!(
            waiters.matching(Some("t1")).is_empty(),
            "settled waiters no longer match"
        );
        assert_eq!(rx.try_recv().unwrap(), completed("t1"));
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn absent_turn_id_matches_nothing() {
        let mut waiters = Waiters::default();
        waiters.add("t1");
        assert!(waiters.matching(None).is_empty());
    }

    #[test]
    fn cancel_all_delivers_a_synthesized_event_to_each_waiter() {
        let mut waiters = Waiters::default();
        let (_, mut a) = waiters.add("a");
        let (_, mut b) = waiters.add("b");
        waiters.cancel_all(|turn| AgentStreamEvent::TurnCanceled {
            provider: "codex".into(),
            reason: "agent closed".into(),
            turn_id: Some(turn.into()),
        });
        assert_eq!(waiters.len(), 0);
        for (rx, turn) in [(&mut a, "a"), (&mut b, "b")] {
            match rx.try_recv().unwrap() {
                AgentStreamEvent::TurnCanceled {
                    turn_id, reason, ..
                } => {
                    assert_eq!(turn_id.as_deref(), Some(turn));
                    assert_eq!(reason, "agent closed");
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }
}
