//! `AgentActor`: one tokio task per live agent.
//!
//! Owns the provider session handle and its [`EventStream`], the [`TimelineState`], the
//! agent's coalescer slice, the managed fields, the [`RunSlot`], the [`EventGate`], foreground
//! waiters, pending permissions and `finalized_turn_ids`.
//!
//! Every TS block with no `await` is one actor command (a closure run on `&mut AgentActor`), so
//! nothing interleaves inside it. A multi-step TS operation is an async function outside the
//! actor that `ask`s one command per synchronous stretch and awaits provider calls in between;
//! the actor loop itself never awaits a provider.
//!
//! Loop order is `biased`: provider events, then commands, then the coalescer deadline. Events
//! already delivered by the provider are therefore applied before a later command, like the
//! microtask-driven event lane in TS.

pub(crate) mod cancel;
pub(crate) mod closure;
pub(crate) mod emit;
pub(crate) mod events;
pub(crate) mod gate;
pub(crate) mod handlers;
pub(crate) mod hydrate;
pub(crate) mod permissions;
pub(crate) mod run_state;
pub(crate) mod steer;
pub(crate) mod timeline_record;
pub(crate) mod turn;
pub(crate) mod turn_identity;
pub(crate) mod waiters;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use indexmap::IndexSet;
use paseo_agent_types::{AgentRuntimeInfo, AgentStreamEvent, PersistenceHandle};
use paseo_provider_api::EventStream;
use paseo_timeline::{Coalescer, TimelineState};
use tokio::sync::{mpsc, oneshot};

use crate::background::TaskTracker;
use crate::bridge::SessionRef;
use crate::error::{CoreError, Result};
use crate::hooks::{ManagerHooks, SnapshotPersister};
use crate::manager::bus::BusTx;
use crate::manager::registry::Registry;
use crate::model::{Lifecycle, ManagedAgentView};
use crate::provider_subagents::ProviderSubagentStore;
use crate::time::Clock;

use gate::EventGate;
use run_state::RunSlot;
use waiters::Waiters;

/// Mailbox bound (spec section 3).
const MAILBOX_CAPACITY: usize = 1024;

/// After this many provider events in a row the loop serves one waiting command, so a flooding
/// provider cannot starve lifecycle calls (the loop prefers events otherwise).
const EVENT_STREAK_BEFORE_COMMAND: u32 = 64;

/// One atomic block. It returns the reply delivery, which the loop runs only after the actor has
/// published its view, so a caller that resumes after `ask` always reads a view that includes
/// the block's effects (read-your-writes through `get_agent`).
pub(crate) type Cmd = Box<dyn FnOnce(&mut AgentActor) -> Deliver + Send>;
pub(crate) type Deliver = Box<dyn FnOnce() + Send>;

pub(crate) enum AgentMsg {
    /// One atomic block.
    Run(Cmd),
}

/// Manager-level handles an actor needs; shared by every actor of one manager.
pub(crate) struct ActorCtx {
    pub bus: BusTx,
    pub hooks: Arc<dyn ManagerHooks>,
    pub persister: Option<Arc<dyn SnapshotPersister>>,
    pub clock: Arc<dyn Clock>,
    pub background: TaskTracker,
    pub persist_chain: Arc<crate::persist_chain::PersistChain>,
    pub subagents: Arc<Mutex<ProviderSubagentStore>>,
    pub registry: Arc<Registry>,
}

pub(crate) struct AgentActor {
    pub(crate) view: ManagedAgentView,
    pub(crate) session: SessionRef,
    pub(crate) events: Option<EventStream>,
    pub(crate) timeline: TimelineState,
    pub(crate) coalescer: Coalescer,
    pub(crate) run: RunSlot,
    pub(crate) gate: EventGate,
    pub(crate) waiters: Waiters,
    /// Insertion ordered; the oldest is evicted past 50.
    pub(crate) finalized: IndexSet<String>,
    pub(crate) previous_status: Option<Lifecycle>,
    pub(crate) buffered_resolutions: HashMap<String, AgentStreamEvent>,
    pub(crate) in_flight_permissions: HashSet<String>,
    /// Events routed to the lane while the current message is being handled.
    pub(crate) lane_queue: VecDeque<AgentStreamEvent>,
    pub(crate) ctx: Arc<ActorCtx>,
    pub(crate) published: Arc<ArcSwap<ManagedAgentView>>,
    pub(crate) generation: u64,
    pub(crate) terminated: bool,
    pub(crate) self_tx: mpsc::WeakSender<AgentMsg>,
}

/// Cloneable handle to a live actor. Reads go through `published`, writes through `ask`.
#[derive(Clone)]
pub struct ActorHandle {
    pub(crate) id: String,
    pub(crate) tx: mpsc::Sender<AgentMsg>,
    pub(crate) published: Arc<ArcSwap<ManagedAgentView>>,
    pub(crate) session: SessionRef,
    /// Distinguishes two actors registered under one id (`agents.get(id) !== agent`).
    pub(crate) generation: u64,
}

impl ActorHandle {
    pub fn view(&self) -> Arc<ManagedAgentView> {
        self.published.load_full()
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Run one atomic block on the actor. `Err(Unknown agent)` once the actor has terminated.
    pub(crate) async fn ask<R: Send + 'static>(
        &self,
        block: impl FnOnce(&mut AgentActor) -> R + Send + 'static,
    ) -> Result<R> {
        let (reply, response) = oneshot::channel();
        let cmd: Cmd = Box::new(move |actor| {
            let result = block(actor);
            let deliver: Deliver = Box::new(move || {
                let _ = reply.send(result);
            });
            deliver
        });
        self.tx
            .send(AgentMsg::Run(cmd))
            .await
            .map_err(|_| CoreError::unknown_agent(&self.id))?;
        response
            .await
            .map_err(|_| CoreError::unknown_agent(&self.id))
    }

    /// Fire and forget (used from `Drop`, where nothing can be awaited).
    pub(crate) fn tell(&self, block: impl FnOnce(&mut AgentActor) + Send + 'static) {
        let cmd: Cmd = Box::new(move |actor| {
            block(actor);
            let deliver: Deliver = Box::new(|| {});
            deliver
        });
        if let Err(mpsc::error::TrySendError::Full(msg)) = self.tx.try_send(AgentMsg::Run(cmd)) {
            let tx = self.tx.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = tx.send(msg).await;
                });
            }
        }
    }
}

pub(crate) struct SpawnParams {
    pub view: ManagedAgentView,
    pub session: SessionRef,
    pub timeline: TimelineState,
    pub coalesce_window: Duration,
    pub ctx: Arc<ActorCtx>,
    pub generation: u64,
}

pub(crate) fn spawn_actor(params: SpawnParams) -> ActorHandle {
    let (tx, rx) = mpsc::channel(MAILBOX_CAPACITY);
    let published = Arc::new(ArcSwap::from_pointee(params.view.clone()));
    let handle = ActorHandle {
        id: params.view.id.clone(),
        tx: tx.clone(),
        published: published.clone(),
        session: params.session.clone(),
        generation: params.generation,
    };
    let initial_status = params.view.lifecycle;
    let actor = AgentActor {
        view: params.view,
        session: params.session,
        events: None,
        timeline: params.timeline,
        coalescer: Coalescer::new(params.coalesce_window),
        run: RunSlot::default(),
        gate: EventGate::default(),
        waiters: Waiters::default(),
        finalized: IndexSet::new(),
        previous_status: Some(initial_status),
        buffered_resolutions: HashMap::new(),
        in_flight_permissions: HashSet::new(),
        lane_queue: VecDeque::new(),
        ctx: params.ctx,
        published,
        generation: params.generation,
        terminated: false,
        self_tx: tx.downgrade(),
    };
    tokio::spawn(actor.run_loop(rx));
    handle
}

async fn next_event(events: &mut Option<EventStream>) -> Option<AgentStreamEvent> {
    match events {
        Some(stream) => stream.next().await,
        None => std::future::pending().await,
    }
}

enum Wake {
    Event(Option<AgentStreamEvent>),
    Msg(Option<AgentMsg>),
    Timer,
}

impl AgentActor {
    async fn run_loop(mut self, mut rx: mpsc::Receiver<AgentMsg>) {
        let mut event_streak = 0u32;
        loop {
            let deadline = self
                .coalescer
                .next_deadline()
                .map(tokio::time::Instant::from_std);
            let overdue_command = if event_streak >= EVENT_STREAK_BEFORE_COMMAND {
                rx.try_recv().ok()
            } else {
                None
            };
            let wake = match overdue_command {
                Some(msg) => Wake::Msg(Some(msg)),
                None => {
                    tokio::select! {
                        biased;
                        event = next_event(&mut self.events), if self.events.is_some() => Wake::Event(event),
                        msg = rx.recv() => Wake::Msg(msg),
                        () = async {
                            match deadline {
                                Some(at) => tokio::time::sleep_until(at).await,
                                None => std::future::pending().await,
                            }
                        }, if deadline.is_some() => Wake::Timer,
                    }
                }
            };
            event_streak = if matches!(wake, Wake::Event(Some(_))) {
                event_streak + 1
            } else {
                0
            };
            let mut deliver: Option<Deliver> = None;
            match wake {
                Wake::Event(Some(event)) => self.handle_session_event(event),
                // The provider closed its stream; keep serving commands.
                Wake::Event(None) => self.events = None,
                Wake::Msg(Some(AgentMsg::Run(cmd))) => deliver = Some(cmd(&mut self)),
                Wake::Msg(None) => break,
                Wake::Timer => self.on_timer(),
            }
            self.process_lane();
            self.publish();
            if let Some(deliver) = deliver {
                deliver();
            }
            if self.terminated {
                break;
            }
        }
    }

    /// Route one provider event (`enqueueSessionEvent`).
    pub(crate) fn handle_session_event(&mut self, event: AgentStreamEvent) {
        if let Some(event) = self.gate.route(&mut self.run, event) {
            self.lane_queue.push_back(event);
        }
    }

    /// Process every event routed to the lane, in order.
    pub(crate) fn process_lane(&mut self) {
        while let Some(event) = self.lane_queue.pop_front() {
            if self.terminated {
                self.lane_queue.clear();
                return;
            }
            self.process_event(event);
        }
    }

    /// `drainSessionEvents`: apply everything the provider has already delivered.
    pub(crate) fn drain_events(&mut self) {
        while let Some(event) = self.events.as_mut().and_then(EventStream::try_next) {
            self.handle_session_event(event);
        }
        self.process_lane();
    }

    /// `subscribeToSession`: take the session's event stream. Called at the end of
    /// registration, so events emitted before are lost (PARITY-QUIRK(pre-subscribe-events-lost)).
    pub(crate) fn subscribe_to_session(&mut self) {
        if self.events.is_none() {
            self.events = Some(self.session.subscribe());
        }
    }

    fn on_timer(&mut self) {
        let flushes = self.coalescer.tick(tokio::time::Instant::now().into_std());
        self.apply_flushes(flushes);
    }

    /// Re-route events a barrier or run held (`enqueueSessionEvent` again for each).
    pub(crate) fn requeue(&mut self, events: Vec<AgentStreamEvent>) {
        for event in events {
            self.handle_session_event(event);
        }
    }

    pub(crate) fn publish(&self) {
        self.published.store(Arc::new(self.snapshot()));
    }

    /// Provider-side runtime info fetched outside the actor (`refreshRuntimeInfo`).
    pub(crate) fn apply_runtime_info(&mut self, info: AgentRuntimeInfo, emit: bool) {
        let old = self.view.runtime_info.as_ref();
        let changed = old.map(|o| &o.model) != Some(&info.model)
            || old.map(|o| &o.thinking_option_id) != Some(&info.thinking_option_id)
            || old.map(|o| &o.session_id) != Some(&info.session_id)
            || old.map(|o| &o.mode_id) != Some(&info.mode_id);
        let session_id = info.session_id.clone();
        self.view.runtime_info = Some(info);
        if self.view.persistence.is_none() {
            if let Some(session_id) = session_id {
                self.view.persistence = attach_persistence_cwd(
                    Some(PersistenceHandle {
                        provider: self.view.provider.clone(),
                        session_id,
                        native_handle: None,
                        metadata: None,
                    }),
                    &self.view.cwd,
                );
            }
        }
        if changed && emit {
            self.emit_state(true);
        }
    }

    /// `void this.refreshRuntimeInfo(agent)`: fetch in a sub-task, apply as one command.
    pub(crate) fn spawn_refresh_runtime_info(&self) {
        let session = self.session.clone();
        let tx = self.self_tx.clone();
        tokio::spawn(async move {
            let Ok(info) = session.get_runtime_info().await else {
                return;
            };
            let Some(tx) = tx.upgrade() else {
                return;
            };
            let cmd: Cmd = Box::new(move |actor| {
                actor.apply_runtime_info(info, true);
                let deliver: Deliver = Box::new(|| {});
                deliver
            });
            let _ = tx.send(AgentMsg::Run(cmd)).await;
        });
    }
}

/// `attachPersistenceCwd`: stamp the working directory into the handle metadata.
pub(crate) fn attach_persistence_cwd(
    handle: Option<PersistenceHandle>,
    cwd: &str,
) -> Option<PersistenceHandle> {
    let mut handle = handle?;
    let mut metadata = handle.metadata.take().unwrap_or_default();
    metadata.insert("cwd".into(), serde_json::Value::String(cwd.to_owned()));
    handle.metadata = Some(metadata);
    Some(handle)
}
