//! `SessionActor`: one task per `(principal, clientId)`.
//!
//! It owns delivery, agent updates, the subscription registries, the capability set of its
//! sockets and the 90 s reconnect grace. Everything that interleaves goes through one mpsc of
//! [`SessionCmd`]; handlers with long awaits run as sub-tasks that talk back through the same
//! mailbox. The actor itself only awaits its mailbox, short bus acknowledgements and, during
//! shutdown, nothing else, so a slow store read never stalls event delivery.
//!
//! Handler prologues (opening a subscription, registering with agent updates) run inside the
//! actor step, so no manager event can land between them and the snapshot the sub-task reads:
//! the producer is already subscribed and the update service is buffering.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use paseo_agent_types::to_wire;
use paseo_authz::{access_denied_error, lost_workspace_write, PermSet, SessionAuthz};
use paseo_timeline::FetchOptions;
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::agent_updates::{AgentUpdates, BeginSubscription, EmitFn, VisibleFn};
use crate::delivery::{
    Delivery, DeliveryError, DeliveryHooks, OpenRequest, OwnedSubscription, ReqCtx, RequestInfo,
    SourceGate, SourceId,
};
use crate::events::{ManagerEvent, SharedEvent, SubscribeOpts};
use crate::fanout::{self, Emitter, ReplayRow, StreamMeta};
use crate::time::iso8601_from_millis;

use super::caps::{cap, is_provider_visible, ClientCaps};
use super::env::{SessionDeps, SessionServices};
use super::fetch_agents::{
    list_fetch_agents_entries, read_agent_directory_sync, FetchAgentsRequest, SessionRequestError,
};
use super::sink::SessionSink;
use super::state::{ClientSource, EventSub, SessionState, TimelineSub};

pub const RECONNECT_GRACE: Duration = Duration::from_secs(90);
const MAILBOX: usize = 1024;

/// Identity of a logical session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub principal: String,
    pub client_id: String,
}

/// Facts of the hello a socket replayed.
#[derive(Debug, Clone, Default)]
pub struct ClientHello {
    pub capabilities: ClientCaps,
    pub app_version: Option<String>,
}

/// `PASEO_CORE` hook: the registry decides whether an idle session may go away.
pub type ExpireFn = Arc<dyn Fn(&SessionKey, u64) -> bool + Send + Sync>;

pub enum SessionCmd {
    Inbound {
        source: SourceId,
        message: Value,
    },
    SocketAttached {
        source: SourceId,
        hello: ClientHello,
        reply: oneshot::Sender<Result<Arc<SourceGate>, DeliveryError>>,
    },
    SocketDetached {
        source: SourceId,
    },
    AgentEvent(SharedEvent),
    /// A timeline replacement with the timeline read at replacement time.
    ReplacementReady {
        agent_id: String,
        provider: String,
        epoch: String,
        rows: Vec<ReplayRow>,
    },
    /// The manager closed our subscription because its queue overflowed; resubscribe.
    ProducerOverflow(u64),
    ProjectUpdate(paseo_directory::ProjectUpdate),
    /// A subscription's teardown ran: drop it from the registries.
    SubscriptionStopped {
        family: &'static str,
        id: String,
    },
    /// Producer demand may have changed.
    Refresh,
    /// A message for the broadcast path (`emit`).
    Emit(Value),
    GraceElapsed(u64),
    ShutdownComplete,
    Shutdown {
        done: oneshot::Sender<()>,
    },
}

/// Handle to a running session. Cheap to clone.
#[derive(Clone)]
pub struct SessionHandle {
    pub(crate) tx: mpsc::Sender<SessionCmd>,
    pub(crate) delivery: Delivery,
    pub(crate) authz: Arc<SessionAuthz>,
    pub(crate) key: SessionKey,
}

impl SessionHandle {
    pub fn key(&self) -> &SessionKey {
        &self.key
    }

    pub fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    /// Hands one inbound session message (the unwrapped `message` of the envelope) to the actor.
    pub async fn inbound(&self, source: SourceId, message: Value) -> bool {
        self.tx
            .send(SessionCmd::Inbound { source, message })
            .await
            .is_ok()
    }

    pub async fn project_update(&self, update: paseo_directory::ProjectUpdate) -> bool {
        self.tx
            .send(SessionCmd::ProjectUpdate(update))
            .await
            .is_ok()
    }

    pub async fn refresh(&self) {
        let _ = self.tx.send(SessionCmd::Refresh).await;
    }

    pub async fn shutdown(&self) {
        let (done, wait) = oneshot::channel();
        if self.tx.send(SessionCmd::Shutdown { done }).await.is_ok() {
            let _ = wait.await;
        }
    }

    /// `setPermissions`: replace the permission set atomically. Losing `workspace.write` also
    /// releases browser hosting.
    pub fn set_permissions(&self, set: PermSet) {
        let previous = self.authz.replace(set);
        if lost_workspace_write(previous, set) {
            let delivery = self.delivery.clone();
            tokio::spawn(async move {
                if let Err(e) = delivery.release_family("browser-host").await {
                    tracing::error!(error = %e, "Failed to release revoked browser hosting");
                }
            });
        }
    }

    pub fn permissions(&self) -> PermSet {
        self.authz.permissions()
    }
}

struct AgentProducer {
    task: JoinHandle<()>,
    generation: u64,
}

impl Drop for AgentProducer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Shared view of per-socket capabilities for closures that outlive one actor step.
pub(crate) type ClientViews = Arc<RwLock<HashMap<SourceId, ClientSource>>>;

pub struct SessionActor {
    key: SessionKey,
    authz: Arc<SessionAuthz>,
    generation: u64,
    services: SessionServices,
    delivery: Delivery,
    agent_updates: AgentUpdates,
    state: SessionState,
    views: ClientViews,
    tx: mpsc::Sender<SessionCmd>,
    rx: mpsc::Receiver<SessionCmd>,
    producer: Option<AgentProducer>,
    next_producer_generation: u64,
    grace: Duration,
    grace_epoch: u64,
    grace_task: Option<JoinHandle<()>>,
    expire: ExpireFn,
    shutting_down: bool,
    shutdown_waiters: Vec<oneshot::Sender<()>>,
}

pub(crate) struct SpawnedSession {
    pub handle: SessionHandle,
    pub task: JoinHandle<()>,
}

impl SessionActor {
    pub(crate) fn spawn(
        key: SessionKey,
        generation: u64,
        services: SessionServices,
        sink: Arc<SessionSink>,
        grace: Duration,
        expire: ExpireFn,
    ) -> SpawnedSession {
        let (tx, rx) = mpsc::channel(MAILBOX);
        let authz = Arc::new(SessionAuthz::owner());
        let delivery = {
            let authz = authz.clone();
            Delivery::with_hooks(
                sink,
                DeliveryHooks {
                    project: None,
                    unexpected_reply: Some(Arc::new(|request: &RequestInfo, message: &Value| {
                        let message_type = message
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        tracing::warn!(
                            request_type = %request.ty,
                            message_type,
                            "Unrecognized reply contract; application delivery denied"
                        );
                    })),
                    authorize: Some(Arc::new(move |message| {
                        paseo_authz::OutboundRef::from_value(message)
                            .is_some_and(|m| authz.allows_outbound(&m))
                    })),
                },
            )
        };
        let agent_updates = AgentUpdates::new(Arc::new(SessionDeps {
            services: services.clone(),
        }));
        let handle = SessionHandle {
            tx: tx.clone(),
            delivery: delivery.clone(),
            authz,
            key: key.clone(),
        };
        let actor = SessionActor {
            key,
            authz: handle.authz.clone(),
            generation,
            services,
            delivery,
            agent_updates,
            state: SessionState::default(),
            views: Arc::default(),
            tx,
            rx,
            producer: None,
            next_producer_generation: 0,
            grace,
            grace_epoch: 0,
            grace_task: None,
            expire,
            shutting_down: false,
            shutdown_waiters: Vec::new(),
        };
        let task = tokio::spawn(actor.run());
        SpawnedSession { handle, task }
    }

    async fn run(mut self) {
        while let Some(cmd) = self.rx.recv().await {
            if self.handle(cmd).await {
                break;
            }
        }
        if let Some(task) = self.grace_task.take() {
            task.abort();
        }
        for waiter in self.shutdown_waiters.drain(..) {
            let _ = waiter.send(());
        }
    }

    fn now_iso(&self) -> String {
        iso8601_from_millis(self.services.clock.now_ms())
    }

    /// Returns true when the actor is done.
    async fn handle(&mut self, cmd: SessionCmd) -> bool {
        match cmd {
            SessionCmd::Inbound { source, message } => {
                if !self.shutting_down {
                    self.on_inbound(source, message).await;
                }
            }
            SessionCmd::SocketAttached {
                source,
                hello,
                reply,
            } => {
                let result = self.on_socket_attached(source, hello).await;
                let _ = reply.send(result);
            }
            SessionCmd::SocketDetached { source } => self.on_socket_detached(source).await,
            SessionCmd::AgentEvent(event) => {
                if !self.shutting_down {
                    self.on_agent_event(&event);
                }
            }
            SessionCmd::ReplacementReady {
                agent_id,
                provider,
                epoch,
                rows,
            } => fanout::deliver_timeline_replacement(
                &self.state,
                &self.delivery,
                &agent_id,
                &provider,
                &epoch,
                &rows,
                None,
            ),
            SessionCmd::ProducerOverflow(generation) => {
                if self
                    .producer
                    .as_ref()
                    .is_some_and(|p| p.generation == generation)
                {
                    tracing::warn!("agent event subscription overflowed; resubscribing");
                    self.producer = None;
                    self.refresh_observation_producers().await;
                }
            }
            SessionCmd::ProjectUpdate(update) => self.on_project_update(update).await,
            SessionCmd::SubscriptionStopped { family, id } => {
                match family {
                    "timelines" => {
                        self.state.timeline_subs.shift_remove(&id);
                    }
                    "events" => {
                        self.state.event_subs.shift_remove(&id);
                    }
                    _ => {}
                }
                self.refresh_observation_producers().await;
            }
            SessionCmd::Refresh => self.refresh_observation_producers().await,
            SessionCmd::Emit(message) => {
                Emitter::new(&self.state, &self.delivery).emit(None, message);
            }
            SessionCmd::GraceElapsed(epoch) => {
                if epoch == self.grace_epoch
                    && self.state.clients.is_empty()
                    && (self.expire)(&self.key, self.generation)
                {
                    self.begin_shutdown();
                }
            }
            SessionCmd::Shutdown { done } => {
                self.shutdown_waiters.push(done);
                self.begin_shutdown();
            }
            SessionCmd::ShutdownComplete => return true,
        }
        false
    }

    /// `cleanup()`: close delivery (stops every producer), drop agent updates and the manager
    /// subscription. The close runs as a sub-task because the stops report back through the
    /// mailbox; `ShutdownComplete` ends the loop.
    fn begin_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.producer = None;
        self.agent_updates.dispose();
        let closing = self.delivery.close();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Err(error) = closing.await {
                tracing::error!(%error, "session delivery teardown failed");
            }
            let _ = tx.send(SessionCmd::ShutdownComplete).await;
        });
    }

    async fn on_socket_attached(
        &mut self,
        source: SourceId,
        hello: ClientHello,
    ) -> Result<Arc<SourceGate>, DeliveryError> {
        let modern = hello.capabilities.has(cap::OWNED_SUBSCRIPTIONS);
        let gate = self.delivery.attach(source, modern)?;
        self.grace_epoch += 1;
        if let Some(task) = self.grace_task.take() {
            task.abort();
        }
        self.state.client_caps = hello.capabilities.clone();
        if hello.app_version.is_some() && hello.app_version != self.state.app_version {
            self.state.app_version = hello.app_version.clone();
        }
        let client = ClientSource {
            capabilities: hello.capabilities,
            app_version: hello.app_version.or_else(|| self.state.app_version.clone()),
        };
        self.views
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(source, client.clone());
        self.state.clients.insert(source, client);
        self.refresh_observation_producers().await;
        Ok(gate)
    }

    async fn on_socket_detached(&mut self, source: SourceId) {
        // The synchronous half (source inactive, teardowns started) runs now, before producer
        // demand is re-evaluated below.
        let detaching = self.delivery.detach(source);
        tokio::spawn(async move {
            if let Err(error) = detaching.await {
                tracing::error!(%error, "Failed to release source subscriptions");
            }
        });
        self.state.clients.shift_remove(&source);
        self.views
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&source);
        self.refresh_observation_producers().await;
        if self.state.clients.is_empty() && !self.shutting_down {
            self.grace_epoch += 1;
            let epoch = self.grace_epoch;
            let tx = self.tx.clone();
            let grace = self.grace;
            if let Some(task) = self.grace_task.take() {
                task.abort();
            }
            self.grace_task = Some(tokio::spawn(async move {
                tokio::time::sleep(grace).await;
                let _ = tx.send(SessionCmd::GraceElapsed(epoch)).await;
            }));
        }
    }

    // ---- producers ----

    /// `refreshObservationProducers` (agent half): the manager subscription exists exactly while
    /// something wants agent events.
    async fn refresh_observation_producers(&mut self) {
        if self.shutting_down {
            return;
        }
        let state = &self.state;
        let d = &self.delivery;
        let legacy = d.has_legacy_sources();
        let workspaces = legacy
            || d.has_demand("workspaces")
            || state.wants_event(d, "checkout_status_update", None);
        let agents = workspaces
            || d.has_demand("agents")
            || d.has_demand("timelines")
            || state.wants_event(d, "agent_attention_required", None)
            || state.wants_event(d, "agent_permission_request", None)
            || state.wants_event(d, "agent_permission_resolved", None)
            || state.wants_event(d, "agent.provider_subagents.update", None);
        if agents && self.producer.is_none() {
            let opts = SubscribeOpts {
                replay_state: false,
                include_internal: true,
                ..SubscribeOpts::default()
            };
            match self.services.manager.subscribe(opts).await {
                Ok(mut subscription) => {
                    self.next_producer_generation += 1;
                    let generation = self.next_producer_generation;
                    let tx = self.tx.clone();
                    let manager = self.services.manager.clone();
                    let task = tokio::spawn(async move {
                        loop {
                            let Some(event) = subscription.next().await else {
                                if subscription.overflowed() {
                                    let _ = tx.send(SessionCmd::ProducerOverflow(generation)).await;
                                }
                                return;
                            };
                            let cmd = match &*event {
                                ManagerEvent::TimelineReplacement { agent_id, .. } => {
                                    let Some(agent) = manager.get_agent(agent_id) else {
                                        continue;
                                    };
                                    let options = FetchOptions {
                                        limit: Some(0),
                                        ..FetchOptions::default()
                                    };
                                    let Ok(fetched) =
                                        manager.fetch_timeline(agent_id, options).await
                                    else {
                                        continue;
                                    };
                                    SessionCmd::ReplacementReady {
                                        agent_id: agent_id.clone(),
                                        provider: agent.provider.clone(),
                                        epoch: fetched.epoch,
                                        rows: fetched
                                            .rows
                                            .iter()
                                            .map(|r| ReplayRow {
                                                item: r.item.clone(),
                                                turn_id: r.turn_id.clone(),
                                                timestamp: r.timestamp.clone(),
                                                seq: r.seq(),
                                            })
                                            .collect(),
                                    }
                                }
                                _ => SessionCmd::AgentEvent(event),
                            };
                            if tx.send(cmd).await.is_err() {
                                return;
                            }
                        }
                    });
                    self.producer = Some(AgentProducer { task, generation });
                }
                Err(error) => tracing::error!(%error, "agent event subscription failed"),
            }
        }
        if !agents {
            self.producer = None;
        }
    }

    // ---- agent events ----

    fn on_agent_event(&mut self, event: &ManagerEvent) {
        match event {
            ManagerEvent::AgentState { agent } => {
                drop(self.agent_updates.forward_live_agent(agent));
            }
            ManagerEvent::AgentStream {
                agent_id,
                event,
                seq,
                epoch,
                timestamp,
                internal,
            } => {
                let Some(wire) = to_wire(event) else {
                    tracing::warn!(agent_id, "agent stream event has no wire form; dropped");
                    return;
                };
                let meta = StreamMeta {
                    agent_id,
                    seq: *seq,
                    epoch: epoch.as_deref(),
                    timestamp: timestamp.as_deref(),
                };
                fanout::forward_agent_stream(
                    &self.state,
                    &self.delivery,
                    &meta,
                    wire.as_value(),
                    &self.now_iso(),
                );
                let emitter = Emitter::new(&self.state, &self.delivery);
                match event {
                    paseo_agent_types::AgentStreamEvent::PermissionRequested {
                        request, ..
                    } if !internal => {
                        emitter.emit(
                            None,
                            json!({
                                "type": "agent_permission_request",
                                "payload": { "agentId": agent_id, "request": request.to_value() },
                            }),
                        );
                    }
                    paseo_agent_types::AgentStreamEvent::PermissionResolved {
                        request_id,
                        resolution,
                        ..
                    } => {
                        // A provider event is not the outcome of whichever request is running.
                        emitter.emit_subscribed_event(
                            json!({
                                "type": "agent_permission_resolved",
                                "payload": {
                                    "agentId": agent_id,
                                    "requestId": request_id,
                                    "resolution": resolution.to_value(),
                                },
                            }),
                            None,
                        );
                    }
                    _ => {}
                }
            }
            // The provider-subagent track needs its descriptor wire form (WP8 handlers); until
            // those handlers move, TS keeps emitting `agent.provider_subagents.update`.
            ManagerEvent::ProviderSubagent { .. } => {}
            // Handled in the producer task (it reads the timeline).
            ManagerEvent::TimelineReplacement { .. } => {}
        }
    }

    async fn on_project_update(&mut self, update: paseo_directory::ProjectUpdate) {
        let wants = |s: &Self| s.state.wants_event(&s.delivery, "project.update", None);
        if !wants(self) {
            return;
        }
        let update = match update {
            paseo_directory::ProjectUpdate::Upsert { project } => {
                match self.services.hooks.project_descriptor(project).await {
                    Ok(project) => paseo_directory::ProjectUpdate::Upsert { project },
                    Err(error) => {
                        tracing::error!(%error, "project descriptor failed");
                        return;
                    }
                }
            }
            remove => remove,
        };
        if !wants(self) {
            return;
        }
        let payload = self
            .services
            .directory
            .sequence_project_update(&update, true);
        Emitter::new(&self.state, &self.delivery).emit(
            None,
            json!({ "type": "project.update", "payload": payload }),
        );
    }

    // ---- inbound ----

    fn provider_visibility(&self, source: SourceId) -> VisibleFn {
        let views = self.views.clone();
        let fallback_caps = self.state.client_caps.clone();
        let fallback_version = self.state.app_version.clone();
        Arc::new(move |provider: &str| {
            let guard = views.read().unwrap_or_else(|e| e.into_inner());
            match guard.get(&source) {
                Some(client) => is_provider_visible(
                    &client.capabilities,
                    client.app_version.as_deref(),
                    provider,
                ),
                None => is_provider_visible(&fallback_caps, fallback_version.as_deref(), provider),
            }
        })
    }

    async fn on_inbound(&mut self, source: SourceId, message: Value) {
        let info = RequestInfo::from_message(&message);
        let Ok(open) = self.delivery.open_request(Some(source), info.clone()) else {
            return;
        };
        let ctx = open.ctx.clone();
        if !self.authz.allows_inbound(&info.ty) {
            if let Some(request_id) = &info.request_id {
                Emitter::new(&self.state, &self.delivery).emit(
                    Some(&ctx),
                    rpc_error(
                        request_id,
                        &info.ty,
                        &access_denied_error(&info.ty),
                        "access_denied",
                    ),
                );
            }
            return;
        }
        let result = match info.ty.as_str() {
            "fetch_agents_request" => {
                self.start_fetch_agents(open, &message).await;
                Ok(())
            }
            "subscription.release.request" => {
                self.start_release(open, &message);
                Ok(())
            }
            "session.events.set_subscription.request" => {
                self.set_event_subscription(&ctx, &message).await
            }
            "agent.timeline.set_subscription.request" => {
                self.set_timeline_subscription(&ctx, &message).await
            }
            "ping" => {
                self.ping(&ctx, &message);
                Ok(())
            }
            other => {
                tracing::warn!(
                    message_type = other,
                    "message routed to core has no handler"
                );
                Ok(())
            }
        };
        if let Err(error) = result {
            self.handler_failed(&ctx, &info, &error);
        }
    }
}

fn rpc_error(request_id: &str, request_type: &str, error: &str, code: &str) -> Value {
    json!({
        "type": "rpc_error",
        "payload": {
            "requestId": request_id,
            "requestType": request_type,
            "error": error,
            "code": code,
        },
    })
}

impl SessionActor {
    /// `handleRequest`'s catch: a failed handler answers `handler_error` and logs an
    /// `activity_log` entry.
    fn handler_failed(&self, ctx: &ReqCtx, info: &RequestInfo, error: &str) {
        tracing::error!(request_type = %info.ty, %error, "Error handling message");
        handle_failure(&self.tx, ctx, info, error, &self.now_iso());
    }

    fn ping(&self, ctx: &ReqCtx, message: &Value) {
        let now = self.services.clock.now_ms();
        Emitter::new(&self.state, &self.delivery).emit(
            Some(ctx),
            json!({
                "type": "pong",
                "payload": {
                    "requestId": message.get("requestId").cloned().unwrap_or(Value::Null),
                    "clientSentAt": message.get("clientSentAt").cloned().unwrap_or(Value::Null),
                    "serverReceivedAt": now,
                    "serverSentAt": now,
                },
            }),
        );
    }

    /// Stop of a subscription whose registry entry lives in the actor.
    fn registry_stop(
        &self,
        family: &'static str,
    ) -> impl FnOnce(
        String,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>
           + Send
           + 'static {
        let tx = self.tx.clone();
        move |id| {
            Box::pin(async move {
                let _ = tx
                    .send(SessionCmd::SubscriptionStopped { family, id })
                    .await;
                Ok(())
            })
        }
    }

    async fn set_event_subscription(
        &mut self,
        ctx: &ReqCtx,
        message: &Value,
    ) -> Result<(), String> {
        let events: HashSet<String> = message
            .get("events")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let owner = ctx
            .begin("events", None, None, self.registry_stop("events"))
            .map_err(|e| e.to_string())?;
        self.state.event_subs.insert(
            owner.id().to_owned(),
            EventSub {
                owner: owner.clone(),
                events,
                notifications: message.get("notifications").and_then(Value::as_bool) == Some(true),
            },
        );
        // PARITY-QUIRK(set-subscription-response-keeps-legacy-subscription-id): TS answers through
        // `emitForSource`, which owns the reply but skips `reply()`'s legacy projection, so a legacy
        // source still receives `subscriptionId` here although `legacyMessage` would strip it.
        self.delivery.emit_to_source(
            ctx.source(),
            Some(ctx),
            json!({
                "type": "session.events.set_subscription.response",
                "payload": {
                    "requestId": message.get("requestId").cloned().unwrap_or(Value::Null),
                    "subscriptionId": owner.response_id(),
                },
            }),
        );
        self.refresh_observation_producers().await;
        Ok(())
    }

    async fn set_timeline_subscription(
        &mut self,
        ctx: &ReqCtx,
        message: &Value,
    ) -> Result<(), String> {
        let agent_ids: BTreeSet<String> = message
            .get("agentIds")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let owner = ctx
            .begin("timelines", None, None, self.registry_stop("timelines"))
            .map_err(|e| e.to_string())?;
        self.state.timeline_subs.insert(
            owner.id().to_owned(),
            TimelineSub {
                owner: owner.clone(),
                agent_ids: agent_ids.clone(),
            },
        );
        self.refresh_observation_producers().await;
        self.delivery.emit_to_source(
            ctx.source(),
            Some(ctx),
            json!({
                "type": "agent.timeline.set_subscription.response",
                "payload": {
                    "agentIds": agent_ids.iter().collect::<Vec<_>>(),
                    "requestId": message.get("requestId").cloned().unwrap_or(Value::Null),
                    "subscriptionId": owner.response_id(),
                },
            }),
        );
        Ok(())
    }

    /// `subscription.release.request`: release by id, then answer.
    fn start_release(&self, open: OpenRequest, message: &Value) {
        let info = RequestInfo::from_message(message);
        let subscription_id = message
            .get("subscriptionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let tx = self.tx.clone();
        let now = self.now_iso();
        tokio::spawn(async move {
            // Keep the whole guard alive: `async move` would capture only `open.ctx`.
            let _open = open;
            let ctx = _open.ctx.clone();
            match ctx.release(&subscription_id).await {
                Ok(()) => {
                    ctx.reply(json!({
                        "type": "subscription.release.response",
                        "payload": {
                            "requestId": info.request_id,
                            "subscriptionId": subscription_id,
                        },
                    }));
                }
                Err(error) => handle_failure(&tx, &ctx, &info, &error.to_string(), &now),
            }
        });
    }

    /// `handleFetchAgents`: the prologue runs here (subscription registered before the snapshot is
    /// read), the snapshot and the response run as a sub-task.
    async fn start_fetch_agents(&mut self, open: OpenRequest, message: &Value) {
        let ctx = open.ctx.clone();
        let info = RequestInfo::from_message(message);
        let request = FetchAgentsRequest::from_message(message);
        let visible = self.provider_visibility(ctx.source());
        let mut owner: Option<OwnedSubscription> = None;
        if let Some(subscribe) = &request.subscribe {
            let tx = self.tx.clone();
            let updates = self.agent_updates.clone();
            let begun = ctx.begin(
                "agents",
                subscribe.subscription_id.as_deref(),
                None,
                move |id| {
                    Box::pin(async move {
                        updates.clear_subscription(&id);
                        let _ = tx.send(SessionCmd::Refresh).await;
                        Ok(())
                    })
                        as std::pin::Pin<
                            Box<dyn std::future::Future<Output = Result<(), String>> + Send>,
                        >
                },
            );
            match begun {
                Ok(o) => {
                    let sink = o.clone();
                    let emit: EmitFn = Arc::new(move |message: Value| {
                        if message.get("type").and_then(Value::as_str) == Some("agent_update") {
                            sink.emit(message);
                        }
                    });
                    let filter = request
                        .filter
                        .clone()
                        .unwrap_or_default()
                        .with_include_background(request.include_background);
                    self.agent_updates.begin_subscription(
                        BeginSubscription::new(o.id())
                            .filter(filter)
                            .sync(request.sync.is_some())
                            .provider_visible(visible.clone())
                            .emit(emit),
                    );
                    owner = Some(o);
                    self.refresh_observation_producers().await;
                }
                Err(error) => {
                    self.handler_failed(&ctx, &info, &error.to_string());
                    return;
                }
            }
        }
        let services = self.services.clone();
        let updates = self.agent_updates.clone();
        tokio::spawn(async move {
            let _open = open;
            finish_fetch_agents(services, updates, ctx, info, request, owner, visible).await;
        });
    }
}

fn handle_failure(
    tx: &mpsc::Sender<SessionCmd>,
    ctx: &ReqCtx,
    info: &RequestInfo,
    error: &str,
    now_iso: &str,
) {
    if let Some(request_id) = &info.request_id {
        ctx.reply(rpc_error(
            request_id,
            &info.ty,
            &format!("Request failed: {error}"),
            "handler_error",
        ));
    }
    let mut entry = Map::new();
    entry.insert("id".into(), uuid::Uuid::new_v4().to_string().into());
    entry.insert("timestamp".into(), now_iso.into());
    entry.insert("type".into(), "error".into());
    entry.insert("content".into(), format!("Error: {error}").into());
    let _ = tx.try_send(SessionCmd::Emit(json!({
        "type": "activity_log",
        "payload": Value::Object(entry),
    })));
}

async fn finish_fetch_agents(
    services: SessionServices,
    updates: AgentUpdates,
    ctx: ReqCtx,
    info: RequestInfo,
    request: FetchAgentsRequest,
    owner: Option<OwnedSubscription>,
    visible: VisibleFn,
) {
    let listed: Result<Map<String, Value>, SessionRequestError> = if request.sync.is_some() {
        read_agent_directory_sync(&services, &request, visible.as_ref()).await
    } else {
        list_fetch_agents_entries(
            &services,
            &request,
            visible.as_ref(),
            request.include_background,
            request.page_limit.unwrap_or(200),
        )
        .await
        .map(|page| {
            let mut payload = Map::new();
            payload.insert("entries".into(), Value::Array(page.entries));
            payload.insert("pageInfo".into(), page.page_info);
            payload
        })
    };
    match listed {
        Ok(payload) => {
            let mut snapshot_updated_at: HashMap<String, i64> = HashMap::new();
            for entry in payload
                .get("entries")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let agent = entry.get("agent");
                let parsed = agent
                    .and_then(|a| a.get("updatedAt"))
                    .and_then(Value::as_str)
                    .and_then(paseo_store::time::parse_js_date);
                if let (Some(id), Some(parsed)) = (
                    agent.and_then(|a| a.get("id")).and_then(Value::as_str),
                    parsed,
                ) {
                    snapshot_updated_at.insert(id.to_owned(), parsed);
                }
            }
            let mut body = Map::new();
            body.insert(
                "requestId".into(),
                info.request_id.clone().map_or(Value::Null, Value::String),
            );
            if let Some(owner) = &owner {
                body.insert("subscriptionId".into(), owner.response_id().into());
            }
            body.extend(payload);
            ctx.reply(json!({ "type": "fetch_agents_response", "payload": Value::Object(body) }));
            if let Some(owner) = &owner {
                updates.flush_bootstrapped(owner.id(), Some(&snapshot_updated_at));
            }
        }
        Err(error) => {
            if let Some(owner) = &owner {
                if let Err(release) = owner.release().await {
                    tracing::error!(error = %release, "failed to release a failed fetch_agents subscription");
                }
            }
            tracing::error!(error = %error.message, "Failed to handle fetch_agents_request");
            if let Some(request_id) = &info.request_id {
                ctx.reply(rpc_error(request_id, &info.ty, &error.message, error.code));
            }
        }
    }
}
