//! `ManagerBus`: one task that owns the subscriber list and a mirror of the latest
//! `agent_state` per live agent.
//!
//! Actors send events over one unbounded mpsc (per-agent order is preserved because one actor
//! is one sender). `subscribe` is a bus command, so replay plus registration is atomic with
//! respect to events, like the synchronous `subscribe` in TS (`agent-manager.ts:980`).
//!
//! Each subscriber has a bounded queue. A full queue closes that subscriber and flags it
//! (`Subscription::overflowed`); events are never silently dropped.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use indexmap::IndexMap;
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::error::{CoreError, Result};
use crate::events::{ManagerEvent, SharedEvent, SubscribeOpts, Subscription};
use crate::ids::validate_agent_id;
use crate::model::{Lifecycle, ManagedAgentView};

struct Subscriber {
    id: u64,
    agent_id: Option<String>,
    include_internal: bool,
    tx: mpsc::Sender<SharedEvent>,
    overflowed: Arc<AtomicBool>,
}

enum BusMsg {
    Event(SharedEvent),
    Subscribe {
        subscriber: Subscriber,
        replay_state: bool,
        registered: oneshot::Sender<()>,
    },
    Unsubscribe(u64),
    /// Replies once every message sent before it has been handled.
    Barrier(oneshot::Sender<()>),
}

/// Cheap clonable sender side of the bus.
#[derive(Debug, Clone)]
pub struct BusTx {
    tx: mpsc::UnboundedSender<BusMsg>,
    next_id: Arc<AtomicU64>,
    count: Arc<AtomicUsize>,
}

impl std::fmt::Debug for BusMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BusMsg")
    }
}

impl BusTx {
    /// Start the bus task on the current tokio runtime.
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let count = Arc::new(AtomicUsize::new(0));
        tokio::spawn(run(rx, count.clone()));
        Self {
            tx,
            next_id: Arc::new(AtomicU64::new(1)),
            count,
        }
    }

    pub fn emit(&self, event: ManagerEvent) {
        let _ = self.tx.send(BusMsg::Event(Arc::new(event)));
    }

    pub async fn subscribe(&self, opts: SubscribeOpts) -> Result<Subscription> {
        let (subscription, registered) = self.register(opts)?;
        registered
            .await
            .map_err(|_| CoreError::msg("manager bus is stopped"))?;
        Ok(subscription)
    }

    /// Queue a registration without waiting for the bus to process it. Registration is ordered
    /// with every event sent before this call on the same sender, so an actor calling this inside
    /// one of its atomic blocks gets "snapshot, then every later event" with nothing in between.
    pub(crate) fn register(
        &self,
        opts: SubscribeOpts,
    ) -> Result<(Subscription, oneshot::Receiver<()>)> {
        let agent_id = opts
            .agent_id
            .as_deref()
            .map(|id| validate_agent_id(id, "subscribe"))
            .transpose()?;
        let (tx, rx) = mpsc::channel(opts.capacity.max(1));
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let overflowed = Arc::new(AtomicBool::new(false));
        let (registered, registered_rx) = oneshot::channel();
        self.tx
            .send(BusMsg::Subscribe {
                subscriber: Subscriber {
                    id,
                    agent_id,
                    include_internal: opts.include_internal,
                    tx,
                    overflowed: overflowed.clone(),
                },
                replay_state: opts.replay_state,
                registered,
            })
            .map_err(|_| CoreError::msg("manager bus is stopped"))?;
        Ok((
            Subscription {
                rx,
                id,
                overflowed,
                bus: self.clone(),
            },
            registered_rx,
        ))
    }

    pub(crate) fn unsubscribe(&self, id: u64) {
        let _ = self.tx.send(BusMsg::Unsubscribe(id));
    }

    /// Wait until every event emitted before this call has been delivered to subscriber queues.
    pub async fn barrier(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(BusMsg::Barrier(tx)).is_ok() {
            let _ = rx.await;
        }
    }

    pub fn subscription_count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

fn matches(subscriber: &Subscriber, event: &ManagerEvent) -> bool {
    if let Some(target) = &subscriber.agent_id {
        // PARITY-QUIRK(timeline-replacement-unfiltered): TS `dispatch` has no agent filter for
        // `timeline_replacement`, so agent-scoped subscribers see every agent's replacement.
        if !matches!(event, ManagerEvent::TimelineReplacement { .. }) && event.agent_id() != target
        {
            return false;
        }
        return true;
    }
    subscriber.include_internal || !event.belongs_to_internal_agent()
}

async fn run(mut rx: mpsc::UnboundedReceiver<BusMsg>, count: Arc<AtomicUsize>) {
    let mut subscribers: Vec<Subscriber> = Vec::new();
    // Latest emitted `agent_state` of every live agent, in first-emit order.
    let mut mirror: IndexMap<String, Arc<ManagedAgentView>> = IndexMap::new();
    while let Some(msg) = rx.recv().await {
        match msg {
            BusMsg::Event(event) => {
                if let ManagerEvent::AgentState { agent } = &*event {
                    if agent.lifecycle == Lifecycle::Closed {
                        mirror.shift_remove(&agent.id);
                    } else {
                        mirror.insert(agent.id.clone(), agent.clone());
                    }
                }
                deliver(&mut subscribers, &event);
            }
            BusMsg::Subscribe {
                subscriber,
                replay_state,
                registered,
            } => {
                let mut keep = true;
                if replay_state {
                    for agent in mirror.values() {
                        let event = Arc::new(ManagerEvent::AgentState {
                            agent: agent.clone(),
                        });
                        if matches(&subscriber, &event) && !offer(&subscriber, &event) {
                            keep = false;
                            break;
                        }
                    }
                }
                if keep {
                    subscribers.push(subscriber);
                }
                let _ = registered.send(());
            }
            BusMsg::Unsubscribe(id) => subscribers.retain(|s| s.id != id),
            BusMsg::Barrier(reply) => {
                let _ = reply.send(());
            }
        }
        count.store(subscribers.len(), Ordering::SeqCst);
    }
}

fn deliver(subscribers: &mut Vec<Subscriber>, event: &SharedEvent) {
    subscribers.retain(|s| !matches(s, event) || offer(s, event));
}

/// `false` when the subscriber must be dropped (queue full or receiver gone).
fn offer(subscriber: &Subscriber, event: &SharedEvent) -> bool {
    match subscriber.tx.try_send(event.clone()) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            warn!(
                subscriber = subscriber.id,
                "manager subscriber queue full; closing it"
            );
            subscriber.overflowed.store(true, Ordering::SeqCst);
            false
        }
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ManagedAgentView;
    use paseo_agent_types::AgentStreamEvent;

    const A: &str = "00000000-0000-4000-8000-00000000000a";
    const B: &str = "00000000-0000-4000-8000-00000000000b";

    fn state(id: &str, lifecycle: Lifecycle, internal: bool) -> ManagerEvent {
        let mut view = ManagedAgentView::for_test(id);
        view.lifecycle = lifecycle;
        view.internal = internal;
        ManagerEvent::AgentState {
            agent: Arc::new(view),
        }
    }

    fn stream(id: &str, text: &str, internal: bool) -> ManagerEvent {
        ManagerEvent::AgentStream {
            agent_id: id.to_owned(),
            event: AgentStreamEvent::ThreadStarted {
                session_id: text.to_owned(),
                provider: "codex".into(),
            },
            seq: None,
            epoch: None,
            timestamp: None,
            internal,
        }
    }

    async fn drain(sub: &mut Subscription) -> Vec<SharedEvent> {
        let mut out = Vec::new();
        while let Some(event) = sub.try_next() {
            out.push(event);
        }
        out
    }

    fn states_of(events: &[SharedEvent]) -> Vec<(String, Lifecycle)> {
        events
            .iter()
            .filter_map(|e| match &**e {
                ManagerEvent::AgentState { agent } => Some((agent.id.clone(), agent.lifecycle)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn subscribe_replays_the_latest_state_then_every_later_event_with_no_gap() {
        let bus = BusTx::spawn();
        bus.emit(state(A, Lifecycle::Idle, false));
        bus.emit(state(A, Lifecycle::Running, false));
        bus.emit(state(B, Lifecycle::Idle, false));
        // No barrier: the subscribe message is ordered after the emits on the same channel.
        let mut sub = bus.subscribe(SubscribeOpts::default()).await.unwrap();
        bus.emit(stream(A, "live-1", false));
        bus.emit(state(A, Lifecycle::Idle, false));
        bus.barrier().await;

        let events = drain(&mut sub).await;
        assert_eq!(
            states_of(&events),
            vec![
                (A.to_owned(), Lifecycle::Running),
                (B.to_owned(), Lifecycle::Idle),
                (A.to_owned(), Lifecycle::Idle),
            ],
            "replay holds the latest state per agent, then live events in order"
        );
        assert!(matches!(&*events[2], ManagerEvent::AgentStream { .. }));
    }

    #[tokio::test]
    async fn replay_can_be_skipped_and_filters_by_agent() {
        let bus = BusTx::spawn();
        bus.emit(state(A, Lifecycle::Idle, false));
        bus.emit(state(B, Lifecycle::Idle, false));
        let mut scoped = bus.subscribe(SubscribeOpts::for_agent(B)).await.unwrap();
        let mut quiet = bus
            .subscribe(SubscribeOpts::default().without_replay())
            .await
            .unwrap();
        bus.emit(stream(A, "a", false));
        bus.emit(stream(B, "b", false));
        bus.barrier().await;

        let scoped_events = drain(&mut scoped).await;
        assert_eq!(
            scoped_events.len(),
            2,
            "replayed B state plus B's stream event"
        );
        assert!(scoped_events.iter().all(|e| e.agent_id() == B));
        assert_eq!(
            drain(&mut quiet).await.len(),
            2,
            "global sees both streams, no replay"
        );
    }

    #[tokio::test]
    async fn global_subscribers_skip_internal_agents_unless_they_opt_in() {
        let bus = BusTx::spawn();
        bus.emit(state(A, Lifecycle::Idle, true));
        let mut hidden = bus.subscribe(SubscribeOpts::default()).await.unwrap();
        let mut opted = bus
            .subscribe(SubscribeOpts::default().including_internal())
            .await
            .unwrap();
        let mut scoped = bus.subscribe(SubscribeOpts::for_agent(A)).await.unwrap();
        bus.emit(stream(A, "x", true));
        bus.emit(state(A, Lifecycle::Closed, true));
        bus.barrier().await;

        assert!(drain(&mut hidden).await.is_empty());
        assert_eq!(drain(&mut opted).await.len(), 3);
        assert_eq!(
            drain(&mut scoped).await.len(),
            3,
            "scoped subscribers see internal agents"
        );
    }

    #[tokio::test]
    async fn closed_agents_leave_the_replay_mirror() {
        let bus = BusTx::spawn();
        bus.emit(state(A, Lifecycle::Idle, false));
        bus.emit(state(A, Lifecycle::Closed, false));
        let mut sub = bus.subscribe(SubscribeOpts::default()).await.unwrap();
        bus.barrier().await;
        assert!(drain(&mut sub).await.is_empty());
    }

    #[tokio::test]
    async fn timeline_replacement_reaches_every_agent_scoped_subscriber() {
        // PARITY-QUIRK(timeline-replacement-unfiltered)
        let bus = BusTx::spawn();
        let mut scoped = bus
            .subscribe(SubscribeOpts::for_agent(A).without_replay())
            .await
            .unwrap();
        bus.emit(ManagerEvent::TimelineReplacement {
            agent_id: B.to_owned(),
            epoch: "e".into(),
        });
        bus.barrier().await;
        assert_eq!(drain(&mut scoped).await.len(), 1);
    }

    #[tokio::test]
    async fn a_full_subscriber_is_closed_and_flagged_without_affecting_others() {
        let bus = BusTx::spawn();
        let mut slow = bus
            .subscribe(SubscribeOpts {
                capacity: 2,
                replay_state: false,
                ..SubscribeOpts::default()
            })
            .await
            .unwrap();
        let mut healthy = bus
            .subscribe(SubscribeOpts::default().without_replay())
            .await
            .unwrap();
        for i in 0..5 {
            bus.emit(stream(A, &format!("e{i}"), false));
        }
        bus.barrier().await;

        assert!(slow.overflowed(), "overflow is reported, never silent");
        assert_eq!(
            drain(&mut slow).await.len(),
            2,
            "what fit is still delivered"
        );
        assert!(
            slow.next().await.is_none(),
            "then the subscription is closed"
        );
        assert!(!healthy.overflowed());
        assert_eq!(drain(&mut healthy).await.len(), 5);
        assert_eq!(bus.subscription_count(), 1);
    }

    #[tokio::test]
    async fn dropping_a_subscription_unsubscribes_it() {
        let bus = BusTx::spawn();
        let sub = bus.subscribe(SubscribeOpts::default()).await.unwrap();
        assert_eq!(bus.subscription_count(), 1);
        drop(sub);
        bus.barrier().await;
        assert_eq!(bus.subscription_count(), 0);
    }

    #[tokio::test]
    async fn subscribe_rejects_a_non_uuid_agent_filter() {
        let bus = BusTx::spawn();
        let error = bus
            .subscribe(SubscribeOpts::for_agent("invalid-agent-id"))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "subscribe: agentId must be a UUID");
    }
}
