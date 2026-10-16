//! Owned-subscription delivery: the Rust port of `SessionDelivery`
//! (`session/owned-subscriptions/index.ts`).
//!
//! The registry knows which physical sockets ([`SourceId`]) a logical session has, which
//! subscriptions and long-running operations each of them owns, and what each socket may be told.
//! It does not own snapshots, filters or buffering; domain producers do.
//!
//! Differences from TS, all forced by the language:
//! - `AsyncLocalStorage` request contexts become an explicit [`ReqCtx`] handed to the handler.
//! - The `WeakMap` of proofs becomes the `owner` field of each [`Outbound`]; the socket writer
//!   asks [`SourceGate::permits`] (egress gate lives only there).
//! - Releasing an owner starts its teardown at call time (a spawned task) and memoizes the
//!   result, so an un-awaited release still stops the producer, as the TS promise does.
//!
//! Teardown rules kept from TS: cancel first, then `stop().await`, memoized; the owner is
//! forgotten only when `stop` succeeds, so a failed teardown stays registered and
//! `detach`/`close` report it. `detach`/`close` wait for every teardown and return the first
//! error.

mod gate;
pub(crate) mod legacy;
mod replies;
mod types;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use indexmap::IndexMap;
use serde_json::Value;
use tokio::sync::{watch, OnceCell};

use crate::abort::AbortSignal;

pub use gate::SourceGate;
pub use replies::{conventional_response_type, is_reply};
use types::OwnerState;
pub use types::{DeliveryError, Frame, Outbound, OutboundSink, OwnerId, RequestInfo, SourceId};

pub type StopFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;
pub type StopFn = Box<dyn FnOnce() -> StopFuture + Send>;

/// A stop that has nothing to wait for.
pub fn stop_sync(f: impl FnOnce() + Send + 'static) -> StopFn {
    Box::new(move || {
        f();
        Box::pin(async { Ok(()) })
    })
}

/// A stop that awaits an async teardown.
pub fn stop_async<Fut>(f: impl FnOnce() -> Fut + Send + 'static) -> StopFn
where
    Fut: Future<Output = Result<(), String>> + Send + 'static,
{
    Box::new(move || Box::pin(f()))
}

/// Per-source message projection (provider visibility downgrades and the like). Runs on every
/// owned send, before the envelope is built.
pub type Projector = Arc<dyn Fn(SourceId, Value) -> Value + Send + Sync>;
/// Outbound authorization (`authorization.allowsOutbound`): a message it rejects is dropped
/// silently, evaluated per message at emit time.
pub type Authorizer = Arc<dyn Fn(&Value) -> bool + Send + Sync>;
/// Called when a handler emits a correlated message that is not a reply to its request
/// (`unexpectedReply`).
pub type UnexpectedReply = Arc<dyn Fn(&RequestInfo, &Value) + Send + Sync>;

type ReleaseRx = watch::Receiver<Option<Result<(), String>>>;

enum OwnerKind {
    Subscription {
        id: String,
        family: String,
        legacy_slot: String,
        response_id: String,
    },
    Operation {
        accepts: Box<dyn Fn(&Value) -> bool + Send + Sync>,
    },
}

struct Owner {
    proof: OwnerId,
    gate: Arc<SourceGate>,
    cancel: AbortSignal,
    stop: Mutex<Option<StopFn>>,
    release: Mutex<Option<ReleaseRx>>,
    delivery: Weak<Inner>,
    kind: OwnerKind,
}

impl Owner {
    fn is_active(&self) -> bool {
        self.proof.is_active()
    }

    fn sub_id(&self) -> Option<&str> {
        match &self.kind {
            OwnerKind::Subscription { id, .. } => Some(id),
            OwnerKind::Operation { .. } => None,
        }
    }

    fn family(&self) -> Option<&str> {
        match &self.kind {
            OwnerKind::Subscription { family, .. } => Some(family),
            OwnerKind::Operation { .. } => None,
        }
    }

    /// `forget`: drop the registration. Only called after a successful `stop`.
    fn forget(&self) {
        let Some(inner) = self.delivery.upgrade() else {
            return;
        };
        let mut state = inner.lock();
        if let Some(source) = state.sources.get_mut(&self.gate.id()) {
            match &self.kind {
                OwnerKind::Subscription { id, .. } => {
                    source.subs.shift_remove(id);
                }
                OwnerKind::Operation { .. } => {
                    source.ops.shift_remove(&self.proof.0.id);
                }
            }
        }
    }
}

/// `releaseOwner`: the synchronous part (inactive, cancelled, `stop` started) happens now; the
/// returned receiver resolves when the memoized teardown has finished.
fn start_release(owner: &Arc<Owner>) -> ReleaseRx {
    let mut slot = owner.release.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(rx) = slot.as_ref() {
        return rx.clone();
    }
    owner.proof.deactivate();
    owner.cancel.abort("released");
    let (tx, rx) = watch::channel(None);
    *slot = Some(rx.clone());
    drop(slot);
    let stop = owner.stop.lock().unwrap_or_else(|e| e.into_inner()).take();
    let owner = owner.clone();
    tokio::spawn(async move {
        let result = match stop {
            None => Ok(()),
            Some(stop) => {
                // A panicking stop is a failed teardown, like a throwing one in TS.
                match tokio::spawn(async move { stop().await }).await {
                    Ok(result) => result,
                    Err(join) => Err(format!("teardown panicked: {join}")),
                }
            }
        };
        if result.is_ok() {
            owner.forget();
        }
        let _ = tx.send(Some(result));
    });
    rx
}

async fn wait_release(mut rx: ReleaseRx) -> Result<(), DeliveryError> {
    let result = match rx.wait_for(Option::is_some).await {
        Ok(v) => v.clone().unwrap_or(Ok(())),
        // The teardown task vanished (runtime shutdown): treat as done.
        Err(_) => Ok(()),
    };
    result.map_err(DeliveryError::Teardown)
}

async fn release_owner(owner: &Arc<Owner>) -> Result<(), DeliveryError> {
    wait_release(start_release(owner)).await
}

/// Waits for every release, returns the first error (by completion order).
async fn join_releases(receivers: Vec<ReleaseRx>) -> Result<(), DeliveryError> {
    let mut set = tokio::task::JoinSet::new();
    for rx in receivers {
        set.spawn(wait_release(rx));
    }
    let mut first = None;
    while let Some(joined) = set.join_next().await {
        if let Ok(Err(e)) = joined {
            first.get_or_insert(e);
        }
    }
    first.map_or(Ok(()), Err)
}

struct SourceEntry {
    gate: Arc<SourceGate>,
    cancel: AbortSignal,
    subs: IndexMap<String, Arc<Owner>>,
    ops: IndexMap<u64, Arc<Owner>>,
}

#[derive(Default)]
struct State {
    sources: IndexMap<SourceId, SourceEntry>,
}

/// Optional behavior a session plugs into its delivery.
#[derive(Default, Clone)]
pub struct DeliveryHooks {
    pub project: Option<Projector>,
    pub unexpected_reply: Option<UnexpectedReply>,
    pub authorize: Option<Authorizer>,
}

struct Inner {
    sink: Arc<dyn OutboundSink>,
    hooks: DeliveryHooks,
    closing: AtomicBool,
    closed: OnceCell<Result<(), DeliveryError>>,
    next_owner: AtomicU64,
    state: Mutex<State>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Source provenance and lifetimes for one logical session. Cheap to clone.
#[derive(Clone)]
pub struct Delivery {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Delivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Delivery")
            .field("registrations", &self.registration_count())
            .finish()
    }
}

impl Delivery {
    pub fn new(sink: Arc<dyn OutboundSink>) -> Self {
        Self::with_hooks(sink, DeliveryHooks::default())
    }

    pub fn with_hooks(sink: Arc<dyn OutboundSink>, hooks: DeliveryHooks) -> Self {
        Self {
            inner: Arc::new(Inner {
                sink,
                hooks,
                closing: AtomicBool::new(false),
                closed: OnceCell::new(),
                next_owner: AtomicU64::new(1),
                state: Mutex::new(State::default()),
            }),
        }
    }

    fn is_closing(&self) -> bool {
        self.inner.closing.load(Ordering::SeqCst)
    }

    fn new_owner_id(&self, source: SourceId) -> OwnerId {
        OwnerId(Arc::new(OwnerState {
            id: self.inner.next_owner.fetch_add(1, Ordering::SeqCst),
            source,
            active: AtomicBool::new(true),
        }))
    }

    /// Registers a physical socket. Idempotent for the same ownership model; the gate is what
    /// the edge's writer consults.
    pub fn attach(&self, source: SourceId, modern: bool) -> Result<Arc<SourceGate>, DeliveryError> {
        if self.is_closing() {
            return Err(DeliveryError::Closed);
        }
        let mut state = self.inner.lock();
        if let Some(existing) = state.sources.get(&source) {
            if existing.gate.is_modern() != modern {
                return Err(DeliveryError::Renegotiate);
            }
            return Ok(existing.gate.clone());
        }
        let gate = Arc::new(SourceGate::new(source, modern));
        state.sources.insert(
            source,
            SourceEntry {
                gate: gate.clone(),
                cancel: AbortSignal::new(),
                subs: IndexMap::new(),
                ops: IndexMap::new(),
            },
        );
        Ok(gate)
    }

    /// Every registered source, in registration order.
    pub fn sources(&self) -> Vec<SourceId> {
        self.inner.lock().sources.keys().copied().collect()
    }

    /// Outbound authorization for a message the caller is about to route itself.
    pub fn is_allowed(&self, message: &Value) -> bool {
        self.allowed(message)
    }

    pub fn has_source(&self, source: SourceId) -> bool {
        self.inner.lock().sources.contains_key(&source)
    }

    pub fn has_legacy_sources(&self) -> bool {
        self.inner
            .lock()
            .sources
            .values()
            .any(|s| s.gate.is_active() && !s.gate.is_modern())
    }

    /// An active subscription of `family` exists on an active source.
    pub fn has_demand(&self, family: &str) -> bool {
        self.inner.lock().sources.values().any(|s| {
            s.gate.is_active()
                && s.subs
                    .values()
                    .any(|o| o.is_active() && o.family() == Some(family))
        })
    }

    pub fn subscription_ids(&self, source: SourceId, family: &str) -> Vec<String> {
        self.inner
            .lock()
            .sources
            .get(&source)
            .map(|s| {
                s.subs
                    .values()
                    .filter(|o| o.is_active() && o.family() == Some(family))
                    .filter_map(|o| o.sub_id().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn registration_count(&self) -> usize {
        self.inner
            .lock()
            .sources
            .values()
            .map(|s| s.subs.len() + s.ops.len())
            .sum()
    }

    pub fn source_registration_count(&self, source: SourceId) -> usize {
        self.inner
            .lock()
            .sources
            .get(&source)
            .map_or(0, |s| s.subs.len() + s.ops.len())
    }

    pub fn is_modern(&self, source: SourceId) -> bool {
        self.inner
            .lock()
            .sources
            .get(&source)
            .is_some_and(|s| s.gate.is_modern())
    }

    pub async fn release_family(&self, family: &str) -> Result<(), DeliveryError> {
        let owners: Vec<Arc<Owner>> = self
            .inner
            .lock()
            .sources
            .values()
            .flat_map(|s| s.subs.values().cloned())
            .filter(|o| o.family() == Some(family))
            .collect();
        join_releases(owners.iter().map(start_release).collect()).await
    }

    /// Source gone: cancel, release everything it owns, then forget it. The synchronous part
    /// (source inactive, cancelled, every teardown started) happens when this is called, like the
    /// first segment of the TS async function; the returned future waits for the teardowns. A
    /// failed teardown keeps the source registered so the error surfaces again from `close`.
    pub fn detach(
        &self,
        source: SourceId,
    ) -> impl Future<Output = Result<(), DeliveryError>> + Send + 'static {
        let receivers: Option<Vec<ReleaseRx>> = {
            let state = self.inner.lock();
            state.sources.get(&source).map(|entry| {
                entry.gate.deactivate();
                entry.cancel.abort("source detached");
                entry
                    .subs
                    .values()
                    .chain(entry.ops.values())
                    .map(start_release)
                    .collect()
            })
        };
        let this = self.clone();
        async move {
            let Some(receivers) = receivers else {
                return Ok(());
            };
            join_releases(receivers).await?;
            this.inner.lock().sources.shift_remove(&source);
            Ok(())
        }
    }

    /// Closes the registry (memoized). New work fails at once (the flag is set when this is
    /// called); the teardown error, if any, is reported to every caller.
    pub fn close(&self) -> impl Future<Output = Result<(), DeliveryError>> + Send + 'static {
        self.inner.closing.store(true, Ordering::SeqCst);
        let this = self.clone();
        async move {
            this.inner
                .closed
                .get_or_init(|| async { this.dispose().await })
                .await
                .clone()
        }
    }

    async fn dispose(&self) -> Result<(), DeliveryError> {
        let sources: Vec<SourceId> = self.inner.lock().sources.keys().copied().collect();
        let mut first = None;
        for source in sources {
            if let Err(e) = self.detach(source).await {
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// Opens a request context without running anything: the handler's synchronous prologue runs
    /// in the caller's step, and the guard keeps the request the owner of its replies until it
    /// is dropped. `None` is the default legacy source.
    pub fn open_request(
        &self,
        source: Option<SourceId>,
        request: RequestInfo,
    ) -> Result<OpenRequest, DeliveryError> {
        if self.is_closing() {
            return Err(DeliveryError::Closed);
        }
        let source = source.unwrap_or(SourceId::DEFAULT);
        let (gate, cancel) = {
            let mut state = self.inner.lock();
            if !state.sources.contains_key(&source) {
                state.sources.insert(
                    source,
                    SourceEntry {
                        gate: Arc::new(SourceGate::new(source, false)),
                        cancel: AbortSignal::new(),
                        subs: IndexMap::new(),
                        ops: IndexMap::new(),
                    },
                );
            }
            let entry = &state.sources[&source];
            (entry.gate.clone(), entry.cancel.clone())
        };
        let ctx = ReqCtx {
            delivery: self.clone(),
            request: Arc::new(request),
            proof: self.new_owner_id(source),
            gate,
            cancel,
        };
        let guard = ActiveGuard(ctx.proof.clone());
        Ok(OpenRequest { ctx, _guard: guard })
    }

    /// Runs `run` as the handler of `request` from `source` (`None` is the default legacy source).
    /// The request is the owner of its replies until `run` finishes.
    pub async fn request<F, Fut>(
        &self,
        source: Option<SourceId>,
        request: RequestInfo,
        run: F,
    ) -> Result<Fut::Output, DeliveryError>
    where
        F: FnOnce(ReqCtx) -> Fut,
        Fut: Future,
    {
        let open = self.open_request(source, request)?;
        Ok(run(open.ctx.clone()).await)
    }

    /// A malformed or unknown inbound message: answered to its source only, as a control reply.
    pub fn protocol_failure(&self, source: SourceId, failure: &ProtocolFailure) {
        let gate = {
            let state = self.inner.lock();
            match state.sources.get(&source) {
                Some(s) if s.gate.is_active() => s.gate.clone(),
                _ => return,
            }
        };
        let message = match &failure.request_id {
            Some(request_id) => {
                let mut payload = serde_json::Map::new();
                payload.insert("requestId".into(), request_id.clone().into());
                if let Some(ty) = &failure.request_type {
                    payload.insert("requestType".into(), ty.clone().into());
                }
                payload.insert("error".into(), failure.error.clone().into());
                payload.insert("code".into(), failure.code.as_str().into());
                serde_json::json!({ "type": "rpc_error", "payload": Value::Object(payload) })
            }
            None => serde_json::json!({
                "type": "status",
                "payload": { "status": "error", "message": failure.error }
            }),
        };
        let owner = self.new_owner_id(gate.id());
        self.send_owned(&gate, &owner, message);
        owner.deactivate();
    }

    /// A message with no owner: delivered to legacy sources only (the modern gate drops it).
    /// `emit_unowned` mirrors `onMessageToSource` without a request proof.
    pub fn emit_unowned(&self, source: SourceId, message: Value) {
        let Some(gate) = self.gate(source) else {
            return;
        };
        if !self.allowed(&message) {
            return;
        }
        let message = self.project(source, message);
        self.inner
            .sink
            .send(Outbound::session_message(gate.id(), None, &message));
    }

    /// `emitForSource` / `authorizeReply`: send to `source`; the message is owned by `ctx` only
    /// when it is a reply to that request from that same source.
    pub fn emit_to_source(&self, source: SourceId, ctx: Option<&ReqCtx>, message: Value) {
        let Some(gate) = self.gate(source) else {
            return;
        };
        if !self.allowed(&message) {
            return;
        }
        let message = self.project(source, message);
        let owner = ctx
            .filter(|c| {
                c.proof.is_active()
                    && c.gate.is_active()
                    && c.gate.id() == source
                    && is_reply(&c.request, &message)
            })
            .map(|c| c.proof.clone());
        self.inner
            .sink
            .send(Outbound::session_message(gate.id(), owner, &message));
    }

    fn gate(&self, source: SourceId) -> Option<Arc<SourceGate>> {
        self.inner
            .lock()
            .sources
            .get(&source)
            .map(|s| s.gate.clone())
    }

    fn allowed(&self, message: &Value) -> bool {
        self.inner
            .hooks
            .authorize
            .as_ref()
            .is_none_or(|f| f(message))
    }

    fn project(&self, source: SourceId, message: Value) -> Value {
        match &self.inner.hooks.project {
            Some(project) => project(source, message),
            None => message,
        }
    }

    fn send_owned(&self, gate: &SourceGate, owner: &OwnerId, message: Value) {
        if !self.allowed(&message) {
            return;
        }
        let projected = self.project(gate.id(), message);
        self.inner.sink.send(Outbound::session_message(
            gate.id(),
            Some(owner.clone()),
            &projected,
        ));
    }
}

/// A request context that stays the owner of its replies while this value lives.
pub struct OpenRequest {
    pub ctx: ReqCtx,
    _guard: ActiveGuard,
}

struct ActiveGuard(OwnerId);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.deactivate();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolFailureCode {
    InvalidMessage,
    UnknownSchema,
}

impl ProtocolFailureCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidMessage => "invalid_message",
            Self::UnknownSchema => "unknown_schema",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProtocolFailure {
    pub request_id: Option<String>,
    pub request_type: Option<String>,
    pub error: String,
    pub code: ProtocolFailureCode,
}

/// The request a handler runs for. Replaces the TS request `AsyncLocalStorage`: it carries the
/// requesting source, its cancellation and the proof that replies are owned.
#[derive(Clone)]
pub struct ReqCtx {
    delivery: Delivery,
    request: Arc<RequestInfo>,
    proof: OwnerId,
    gate: Arc<SourceGate>,
    cancel: AbortSignal,
}

impl std::fmt::Debug for ReqCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReqCtx")
            .field("source", &self.gate.id())
            .field("request", &self.request)
            .finish()
    }
}

impl ReqCtx {
    pub fn source(&self) -> SourceId {
        self.gate.id()
    }

    pub fn request(&self) -> &RequestInfo {
        &self.request
    }

    pub fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    /// Cancelled when the requesting source detaches (`requestSignal`).
    pub fn signal(&self) -> AbortSignal {
        self.cancel.clone()
    }

    /// `ReqCtx.active` semantics: the request is still running and its source still attached.
    pub fn is_active(&self) -> bool {
        self.proof.is_active() && self.gate.is_active()
    }

    pub fn is_modern(&self) -> bool {
        self.gate.is_modern()
    }

    /// The proof binary replies of a file explorer request carry (`authorizeFileReply`).
    pub fn file_reply_owner(&self) -> Option<OwnerId> {
        (self.proof.is_active()
            && self.gate.is_active()
            && self.request.ty == "file_explorer_request")
            .then(|| self.proof.clone())
    }

    /// Sends `message` as the reply to this request. Returns whether it is a reply at all (the
    /// caller then stops routing it); a reply from a request that already ended or whose source
    /// left is swallowed. A correlated non-reply is reported through the `unexpected_reply` hook.
    pub fn reply(&self, message: Value) -> bool {
        if !is_reply(&self.request, &message) {
            let correlated = self.request.request_id.is_some()
                && message
                    .get("payload")
                    .and_then(|p| p.get("requestId"))
                    .and_then(Value::as_str)
                    == self.request.request_id.as_deref();
            if correlated {
                if let Some(hook) = &self.delivery.inner.hooks.unexpected_reply {
                    hook(&self.request, &message);
                }
            }
            return false;
        }
        if self.is_active() {
            let message = if self.gate.is_modern() {
                message
            } else {
                legacy::legacy_message(message)
            };
            self.delivery.send_owned(&self.gate, &self.proof, message);
        }
        true
    }

    /// Opens a subscription owned by this request's source.
    ///
    /// Modern sources: the host assigns the id, `requestId` is required and a client-supplied id
    /// is rejected. Legacy sources: the client's id is echoed, and a new subscription replaces
    /// the previous one in the same `legacy_slot` (default: the family).
    pub fn begin<Fut>(
        &self,
        family: &str,
        requested_id: Option<&str>,
        legacy_slot: Option<&str>,
        stop: impl FnOnce(String) -> Fut + Send + 'static,
    ) -> Result<OwnedSubscription, DeliveryError>
    where
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        if !self.is_active() {
            return Err(DeliveryError::SubscriptionSourceClosed);
        }
        let modern = self.gate.is_modern();
        if modern && self.request.request_id.as_deref().is_none_or(str::is_empty) {
            return Err(DeliveryError::RequestIdRequired);
        }
        if modern && requested_id.is_some() {
            return Err(DeliveryError::HostAssignedId);
        }
        let legacy_slot = legacy_slot.unwrap_or(family).to_owned();
        let source = self.gate.id();
        let id = uuid::Uuid::new_v4().to_string();
        let response_id = if modern {
            id.clone()
        } else {
            requested_id
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map_or_else(|| id.clone(), str::to_owned)
        };
        let owner = {
            let mut state = self.delivery.inner.lock();
            let Some(entry) = state.sources.get_mut(&source) else {
                return Err(DeliveryError::SubscriptionSourceClosed);
            };
            // COMPAT(ownedSubscriptions): added in v0.8.0, remove after 2027-03-09 once client
            // floor >= v0.8.0. A failed prior teardown stays registered so close can report it.
            if !modern {
                for prior in entry.subs.values() {
                    if matches!(&prior.kind, OwnerKind::Subscription { legacy_slot: slot, .. } if *slot == legacy_slot)
                    {
                        let _ = start_release(prior);
                    }
                }
            }
            let sub_id = id.clone();
            let stop_id = id.clone();
            let owner = Arc::new(Owner {
                proof: self.delivery.new_owner_id(source),
                gate: self.gate.clone(),
                cancel: AbortSignal::new(),
                stop: Mutex::new(Some(Box::new(move || Box::pin(stop(stop_id))))),
                release: Mutex::new(None),
                delivery: Arc::downgrade(&self.delivery.inner),
                kind: OwnerKind::Subscription {
                    id: sub_id,
                    family: family.to_owned(),
                    legacy_slot,
                    response_id: response_id.clone(),
                },
            });
            entry.subs.insert(id.clone(), owner.clone());
            owner
        };
        Ok(OwnedSubscription {
            delivery: self.delivery.clone(),
            owner,
            id,
            response_id,
        })
    }

    /// A domain extends the originating request until its asynchronous operation ends. Messages
    /// the operation emits must satisfy `accepts`.
    pub fn operation(
        &self,
        accepts: impl Fn(&Value) -> bool + Send + Sync + 'static,
        stop: StopFn,
    ) -> Result<OwnedOperation, DeliveryError> {
        if !self.is_active() {
            return Err(DeliveryError::OperationSourceClosed);
        }
        let source = self.gate.id();
        let mut state = self.delivery.inner.lock();
        let Some(entry) = state.sources.get_mut(&source) else {
            return Err(DeliveryError::OperationSourceClosed);
        };
        let owner = Arc::new(Owner {
            proof: self.delivery.new_owner_id(source),
            gate: self.gate.clone(),
            cancel: AbortSignal::new(),
            stop: Mutex::new(Some(stop)),
            release: Mutex::new(None),
            delivery: Arc::downgrade(&self.delivery.inner),
            kind: OwnerKind::Operation {
                accepts: Box::new(accepts),
            },
        });
        entry.ops.insert(owner.proof.0.id, owner.clone());
        Ok(OwnedOperation {
            delivery: self.delivery.clone(),
            owner,
        })
    }

    /// `release(id)`: by server id, or by the client's id on a legacy source.
    pub async fn release(&self, id: &str) -> Result<(), DeliveryError> {
        let owner = {
            let state = self.delivery.inner.lock();
            let Some(entry) = state.sources.get(&self.gate.id()) else {
                return Ok(());
            };
            entry.subs.get(id).cloned().or_else(|| {
                (!self.gate.is_modern())
                    .then(|| {
                        entry
                            .subs
                            .values()
                            .find(|o| {
                                matches!(&o.kind, OwnerKind::Subscription { response_id, .. } if response_id == id)
                            })
                            .cloned()
                    })
                    .flatten()
            })
        };
        match owner {
            Some(owner) => release_owner(&owner).await,
            None => Ok(()),
        }
    }

    pub async fn release_legacy_slot(&self, slot: &str) -> Result<(), DeliveryError> {
        if self.gate.is_modern() {
            return Err(DeliveryError::ReleaseByServerId);
        }
        let owner = {
            let state = self.delivery.inner.lock();
            state.sources.get(&self.gate.id()).and_then(|entry| {
                entry
                    .subs
                    .values()
                    .find(|o| {
                        matches!(&o.kind, OwnerKind::Subscription { legacy_slot, .. } if legacy_slot == slot)
                    })
                    .cloned()
            })
        };
        match owner {
            Some(owner) => release_owner(&owner).await,
            None => Ok(()),
        }
    }
}

/// A subscription a source owns. Emissions are tagged for the source's ownership model and
/// dropped once the subscription or its source is gone.
#[derive(Clone)]
pub struct OwnedSubscription {
    delivery: Delivery,
    owner: Arc<Owner>,
    id: String,
    response_id: String,
}

impl std::fmt::Debug for OwnedSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedSubscription")
            .field("id", &self.id)
            .field("response_id", &self.response_id)
            .field("source", &self.owner.gate.id())
            .finish()
    }
}

impl OwnedSubscription {
    /// Internal id (the key producers use). Modern sources see this one on the wire.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The id clients see: the server id for modern sources, the client's own for legacy ones.
    pub fn response_id(&self) -> &str {
        &self.response_id
    }

    pub fn source(&self) -> SourceId {
        self.owner.gate.id()
    }

    pub fn is_modern(&self) -> bool {
        self.owner.gate.is_modern()
    }

    pub fn is_active(&self) -> bool {
        self.owner.is_active() && self.owner.gate.is_active()
    }

    pub fn signal(&self) -> AbortSignal {
        self.owner.cancel.clone()
    }

    pub fn emit(&self, message: Value) {
        if !self.is_active() {
            return;
        }
        let ty = message
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let tagged = if self.owner.gate.is_modern() || legacy::legacy_keeps_subscription_id(ty) {
            legacy::with_subscription_id(message, &self.response_id)
        } else {
            legacy::legacy_message(message)
        };
        self.delivery
            .send_owned(&self.owner.gate, &self.owner.proof, tagged);
    }

    pub fn emit_binary(&self, frame: impl Into<Arc<[u8]>>) {
        if !self.is_active() {
            return;
        }
        self.delivery.inner.sink.send(Outbound::binary(
            self.owner.gate.id(),
            Some(self.owner.proof.clone()),
            frame,
        ));
    }

    pub async fn release(&self) -> Result<(), DeliveryError> {
        release_owner(&self.owner).await
    }
}

/// A request-extending operation (an upload, a dictation stream).
#[derive(Clone)]
pub struct OwnedOperation {
    delivery: Delivery,
    owner: Arc<Owner>,
}

impl OwnedOperation {
    pub fn source(&self) -> SourceId {
        self.owner.gate.id()
    }

    pub fn signal(&self) -> AbortSignal {
        self.owner.cancel.clone()
    }

    pub fn emit(&self, message: Value) {
        if !self.owner.is_active() || !self.owner.gate.is_active() {
            return;
        }
        let OwnerKind::Operation { accepts } = &self.owner.kind else {
            return;
        };
        if !accepts(&message) {
            return;
        }
        self.delivery
            .send_owned(&self.owner.gate, &self.owner.proof, message);
    }

    pub async fn release(&self) -> Result<(), DeliveryError> {
        release_owner(&self.owner).await
    }
}

/// Counts registrations by source, for tests and diagnostics.
pub fn registrations_by_source(delivery: &Delivery) -> HashMap<SourceId, usize> {
    delivery
        .inner
        .lock()
        .sources
        .iter()
        .map(|(id, s)| (*id, s.subs.len() + s.ops.len()))
        .collect()
}
