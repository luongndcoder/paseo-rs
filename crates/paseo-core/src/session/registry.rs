//! `SessionRegistry`: what the edge talks to. Maps physical sockets to their logical session
//! (`(principal, clientId)`), creates session actors on demand, arbitrates the reconnect grace
//! and holds the owned subscriptions TS-resident producers registered over
//! `internal.delivery.*`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::delivery::{
    DeliveryError, OutboundSink, OwnedSubscription, RequestInfo, SourceGate, SourceId,
};

use super::actor::{
    ClientHello, ExpireFn, SessionActor, SessionCmd, SessionHandle, SessionKey, RECONNECT_GRACE,
};
use super::env::SessionServices;
use super::sink::SessionSink;

/// Core -> TS request/response channel (`internal.*.request` / `.response`).
#[async_trait]
pub trait TsRpc: Send + Sync + 'static {
    async fn call(&self, request_type: &str, payload: Value) -> Result<Value, String>;
}

pub struct AttachRequest {
    pub principal: String,
    pub client_id: String,
    pub source: SourceId,
    pub hello: ClientHello,
    /// The socket's writer. It must enforce [`SourceGate::permits`] (see [`Attached::gate`]).
    pub out: Arc<dyn OutboundSink>,
}

pub struct Attached {
    pub handle: SessionHandle,
    pub gate: Arc<SourceGate>,
}

struct Entry {
    handle: SessionHandle,
    sink: Arc<SessionSink>,
    count: usize,
    generation: u64,
    _task: JoinHandle<()>,
}

#[derive(Default)]
struct Sessions {
    by_key: HashMap<SessionKey, Entry>,
    by_source: HashMap<SourceId, SessionKey>,
}

struct Inner {
    services: SessionServices,
    grace: Duration,
    next_source: AtomicU64,
    next_generation: AtomicU64,
    sessions: Mutex<Sessions>,
    remote: Mutex<HashMap<String, OwnedSubscription>>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Sessions> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn remote(&self) -> MutexGuard<'_, HashMap<String, OwnedSubscription>> {
        self.remote.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Clone)]
pub struct SessionRegistry {
    inner: Arc<Inner>,
}

/// Parameters of `internal.delivery.begin.request`.
#[derive(Debug, Clone)]
pub struct RemoteBegin {
    pub source: SourceId,
    pub family: String,
    pub requested_id: Option<String>,
    pub legacy_slot: Option<String>,
    /// The client request the TS controller is serving.
    pub request: RequestInfo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteBegun {
    pub subscription_id: String,
    pub response_id: String,
}

impl SessionRegistry {
    pub fn new(services: SessionServices) -> Self {
        Self::with_grace(services, RECONNECT_GRACE)
    }

    pub fn with_grace(services: SessionServices, grace: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                services,
                grace,
                next_source: AtomicU64::new(1),
                next_generation: AtomicU64::new(1),
                sessions: Mutex::new(Sessions::default()),
                remote: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn services(&self) -> &SessionServices {
        &self.inner.services
    }

    /// A fresh id for a physical socket. Ids are never reused.
    pub fn next_source_id(&self) -> SourceId {
        SourceId(self.inner.next_source.fetch_add(1, Ordering::SeqCst))
    }

    pub fn session_count(&self) -> usize {
        self.inner.lock().by_key.len()
    }

    pub fn session_of(&self, source: SourceId) -> Option<SessionHandle> {
        let sessions = self.inner.lock();
        let key = sessions.by_source.get(&source)?;
        sessions.by_key.get(key).map(|e| e.handle.clone())
    }

    fn expire_fn(&self) -> ExpireFn {
        let weak = Arc::downgrade(&self.inner);
        Arc::new(move |key, generation| {
            let Some(inner) = weak.upgrade() else {
                return true;
            };
            let mut sessions = inner.lock();
            let expired = sessions
                .by_key
                .get(key)
                .is_some_and(|e| e.generation == generation && e.count == 0);
            if expired {
                sessions.by_key.remove(key);
            }
            expired
        })
    }

    /// Registers a physical socket with its logical session (created on first use, resumed within
    /// the grace window).
    pub async fn attach(&self, request: AttachRequest) -> Result<Attached, DeliveryError> {
        let key = SessionKey {
            principal: request.principal,
            client_id: request.client_id,
        };
        let (handle, sink) = {
            let mut sessions = self.inner.lock();
            if !sessions.by_key.contains_key(&key) {
                let generation = self.inner.next_generation.fetch_add(1, Ordering::SeqCst);
                let sink = Arc::new(SessionSink::default());
                let spawned = SessionActor::spawn(
                    key.clone(),
                    generation,
                    self.inner.services.clone(),
                    sink.clone(),
                    self.inner.grace,
                    self.expire_fn(),
                );
                sessions.by_key.insert(
                    key.clone(),
                    Entry {
                        handle: spawned.handle,
                        sink,
                        count: 0,
                        generation,
                        _task: spawned.task,
                    },
                );
            }
            let entry = sessions.by_key.get_mut(&key).expect("just inserted");
            entry.count += 1;
            entry.sink.add(request.source, request.out);
            let found = (entry.handle.clone(), entry.sink.clone());
            sessions.by_source.insert(request.source, key.clone());
            found
        };
        let (reply, wait) = oneshot::channel();
        let sent = handle
            .tx
            .send(SessionCmd::SocketAttached {
                source: request.source,
                hello: request.hello,
                reply,
            })
            .await;
        let gate = match (sent, wait.await) {
            (Ok(()), Ok(result)) => result,
            _ => Err(DeliveryError::Closed),
        };
        match gate {
            Ok(gate) => Ok(Attached { handle, gate }),
            Err(error) => {
                sink.remove(request.source);
                self.release_source(request.source);
                Err(error)
            }
        }
    }

    fn release_source(&self, source: SourceId) -> Option<SessionHandle> {
        let mut sessions = self.inner.lock();
        let key = sessions.by_source.remove(&source)?;
        let entry = sessions.by_key.get_mut(&key)?;
        entry.count = entry.count.saturating_sub(1);
        entry.sink.remove(source);
        Some(entry.handle.clone())
    }

    /// The socket closed. The session lingers for the reconnect grace.
    pub async fn detach(&self, source: SourceId) {
        if let Some(handle) = self.release_source(source) {
            let _ = handle.tx.send(SessionCmd::SocketDetached { source }).await;
        }
    }

    /// Hands an inbound session message to its session. `false` when the socket has none.
    pub async fn inbound(&self, source: SourceId, message: Value) -> bool {
        match self.session_of(source) {
            Some(handle) => handle.inbound(source, message).await,
            None => false,
        }
    }

    pub async fn shutdown_all(&self) {
        let handles: Vec<SessionHandle> = {
            let mut sessions = self.inner.lock();
            sessions.by_source.clear();
            sessions.by_key.drain().map(|(_, e)| e.handle).collect()
        };
        for handle in handles {
            handle.shutdown().await;
        }
    }

    // ---- internal.delivery.* (TS-resident producers) ----

    /// `internal.delivery.begin.request`: a TS controller serving `request` from `source` opens an
    /// owned subscription in the Rust registry. Releasing it (client release, detach, close)
    /// calls `internal.delivery.stop.request` on TS and forgets the subscription only when TS
    /// answers.
    pub async fn remote_begin(
        &self,
        rpc: Arc<dyn TsRpc>,
        params: RemoteBegin,
    ) -> Result<RemoteBegun, String> {
        let session = self
            .session_of(params.source)
            .ok_or_else(|| format!("unknown source {}", params.source))?;
        let open = session
            .delivery()
            .open_request(Some(params.source), params.request)
            .map_err(|e| e.to_string())?;
        let inner = self.inner.clone();
        let stop_rpc = rpc.clone();
        let subscription = open
            .ctx
            .begin(
                &params.family,
                params.requested_id.as_deref(),
                params.legacy_slot.as_deref(),
                move |id| async move {
                    stop_rpc
                        .call(
                            "internal.delivery.stop.request",
                            serde_json::json!({ "subscriptionId": id }),
                        )
                        .await?;
                    inner.remote().remove(&id);
                    Ok(())
                },
            )
            .map_err(|e| e.to_string())?;
        self.inner
            .remote()
            .insert(subscription.id().to_owned(), subscription.clone());
        drop(open);
        session.refresh().await;
        Ok(RemoteBegun {
            subscription_id: subscription.id().to_owned(),
            response_id: subscription.response_id().to_owned(),
        })
    }

    fn remote_subscription(&self, id: &str) -> Option<OwnedSubscription> {
        self.inner.remote().get(id).cloned()
    }

    /// `internal.delivery.emit`.
    pub fn remote_emit(&self, id: &str, message: Value) -> bool {
        match self.remote_subscription(id) {
            Some(sub) => {
                sub.emit(message);
                true
            }
            None => false,
        }
    }

    /// `internal.delivery.emit_binary`.
    pub fn remote_emit_binary(&self, id: &str, frame: Vec<u8>) -> bool {
        match self.remote_subscription(id) {
            Some(sub) => {
                sub.emit_binary(frame);
                true
            }
            None => false,
        }
    }

    /// `internal.delivery.release.request`: TS ended the producer itself.
    pub async fn remote_release(&self, id: &str) -> Result<(), String> {
        match self.remote_subscription(id) {
            Some(sub) => sub.release().await.map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }
}
