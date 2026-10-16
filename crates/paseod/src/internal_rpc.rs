//! `internal.delivery.*`: how TS-resident controllers (checkout, files, voice, terminals) share
//! the Rust Source registry (spec sections 9 and 13).
//!
//! One long-lived WebSocket from the TS worker to `GET /__paseo_edge/rpc` (token required, the
//! same `x-paseo-edge-token` the internal `/ws` dials carry). Frames follow
//! `docs/rpc-namespacing.md`: requests keep their parameters at the top level, responses put
//! results under `payload`, both carry `requestId`.
//!
//! TS -> core:
//! - `internal.delivery.begin.request {sourceId, family, requestedId?, legacySlot?, request:{type,requestId?}}`
//!   -> `.response {requestId, subscriptionId, responseId}` or `{requestId, error}`. `sourceId` is
//!   the value of `x-paseo-edge-socket-id` of the TS socket serving the client request.
//! - `internal.delivery.emit {subscriptionId, message}` (notification)
//! - `internal.delivery.emit_binary {subscriptionId, base64}` (notification)
//! - `internal.delivery.release.request {subscriptionId}` -> `.response {requestId, error}`
//!
//! Core -> TS:
//! - `internal.delivery.stop.request {subscriptionId}` -> `.response {requestId, error}`. Sent when
//!   the owner is released (client `subscription.release.request`, socket detach, session close).
//!   The core forgets the subscription only when TS answers without error.
//!
//! The core never makes a call it cannot time out: [`RPC_TIMEOUT`] bounds every request.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use paseo_core::delivery::{RequestInfo, SourceId};
use paseo_core::session::registry::{RemoteBegin, TsRpc};
use paseo_core::session::SessionRegistry;
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot};

pub const BEGIN_REQUEST: &str = "internal.delivery.begin.request";
pub const EMIT: &str = "internal.delivery.emit";
pub const EMIT_BINARY: &str = "internal.delivery.emit_binary";
pub const RELEASE_REQUEST: &str = "internal.delivery.release.request";
pub const STOP_REQUEST: &str = "internal.delivery.stop.request";

/// Route the TS worker connects to.
pub const RPC_ROUTE: &str = "/__paseo_edge/rpc";

pub const RPC_TIMEOUT: Duration = Duration::from_secs(30);

type Pending = HashMap<String, oneshot::Sender<Result<Value, String>>>;

/// One connected TS worker.
pub struct Link {
    id: u64,
    out: mpsc::UnboundedSender<String>,
    pending: Mutex<Pending>,
    next_request: AtomicU64,
}

impl Link {
    fn pending(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn send(&self, frame: &Value) {
        let _ = self.out.send(frame.to_string());
    }

    fn fail_all(&self, reason: &str) {
        for (_, waiter) in self.pending().drain() {
            let _ = waiter.send(Err(reason.to_owned()));
        }
    }

    async fn call(&self, request_type: &str, payload: Value) -> Result<Value, String> {
        let request_id = format!("core-{}", self.next_request.fetch_add(1, Ordering::SeqCst));
        let (tx, rx) = oneshot::channel();
        self.pending().insert(request_id.clone(), tx);
        let mut frame = match payload {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        frame.insert("type".into(), request_type.into());
        frame.insert("requestId".into(), request_id.clone().into());
        self.send(&Value::Object(frame));
        match tokio::time::timeout(RPC_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("internal rpc link closed".to_owned()),
            Err(_) => {
                self.pending().remove(&request_id);
                Err(format!("{request_type} timed out"))
            }
        }
    }
}

/// The edge's side of the core <-> TS channel.
pub struct InternalRpc {
    registry: SessionRegistry,
    link: Mutex<Option<Arc<Link>>>,
    next_link: AtomicU64,
}

impl InternalRpc {
    pub fn new(registry: SessionRegistry) -> Arc<Self> {
        Arc::new(Self {
            registry,
            link: Mutex::new(None),
            next_link: AtomicU64::new(1),
        })
    }

    fn current(&self) -> Option<Arc<Link>> {
        self.link.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn is_connected(&self) -> bool {
        self.current().is_some()
    }

    /// A TS worker connected. Its frames come in through [`InternalRpc::handle_text`]; the
    /// returned receiver yields the frames to write back. A newer link replaces an older one.
    pub fn attach(&self) -> (Arc<Link>, mpsc::UnboundedReceiver<String>) {
        let (out, rx) = mpsc::unbounded_channel();
        let link = Arc::new(Link {
            id: self.next_link.fetch_add(1, Ordering::SeqCst),
            out,
            pending: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
        });
        let replaced = self
            .link
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(link.clone());
        if let Some(old) = replaced {
            old.fail_all("internal rpc link replaced");
        }
        (link, rx)
    }

    /// The link ended: outstanding calls fail, and it stops being the current one.
    pub fn detach(&self, link: &Arc<Link>) {
        link.fail_all("internal rpc link closed");
        let mut current = self.link.lock().unwrap_or_else(|e| e.into_inner());
        if current.as_ref().is_some_and(|c| c.id == link.id) {
            *current = None;
        }
    }

    /// Handles one text frame from TS. Long operations run as tasks, so the reader never stalls.
    pub fn handle_text(self: &Arc<Self>, link: &Arc<Link>, text: &str) {
        let Ok(frame) = serde_json::from_str::<Value>(text) else {
            tracing::warn!("internal rpc: unparsable frame");
            return;
        };
        let Some(ty) = frame.get("type").and_then(Value::as_str) else {
            return;
        };
        if ty.ends_with(".response") {
            Self::resolve(link, &frame);
            return;
        }
        let this = self.clone();
        let link = link.clone();
        match ty {
            EMIT => {
                let id = frame.get("subscriptionId").and_then(Value::as_str);
                let message = frame.get("message");
                if let (Some(id), Some(message)) = (id, message) {
                    if !this.registry.remote_emit(id, message.clone()) {
                        tracing::debug!(subscription_id = id, "emit for an unknown subscription");
                    }
                }
            }
            EMIT_BINARY => {
                let id = frame.get("subscriptionId").and_then(Value::as_str);
                let bytes = frame
                    .get("base64")
                    .and_then(Value::as_str)
                    .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok());
                if let (Some(id), Some(bytes)) = (id, bytes) {
                    this.registry.remote_emit_binary(id, bytes);
                }
            }
            BEGIN_REQUEST => {
                tokio::spawn(async move { this.begin(&link, &frame).await });
            }
            RELEASE_REQUEST => {
                tokio::spawn(async move { this.release(&link, &frame).await });
            }
            other if other.ends_with(".request") => {
                let request_id = frame
                    .get("requestId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                link.send(&respond(
                    &other.replace(".request", ".response"),
                    request_id,
                    Some("unknown internal rpc"),
                    Map::new(),
                ));
            }
            _ => {}
        }
    }

    fn resolve(link: &Link, frame: &Value) {
        let payload = frame.get("payload").unwrap_or(&Value::Null);
        let Some(request_id) = payload.get("requestId").and_then(Value::as_str) else {
            return;
        };
        let Some(waiter) = link.pending().remove(request_id) else {
            return;
        };
        let result = match payload.get("error").filter(|e| !e.is_null()) {
            Some(error) => Err(error
                .as_str()
                .map_or_else(|| error.to_string(), str::to_owned)),
            None => Ok(payload.clone()),
        };
        let _ = waiter.send(result);
    }

    async fn begin(self: Arc<Self>, link: &Arc<Link>, frame: &Value) {
        let request_id = frame
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let parsed = (|| {
            let source = frame.get("sourceId")?.as_u64()?;
            let family = frame.get("family")?.as_str()?.to_owned();
            let request = frame.get("request")?;
            Some(RemoteBegin {
                source: SourceId(source),
                family,
                requested_id: frame
                    .get("requestedId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                legacy_slot: frame
                    .get("legacySlot")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                request: RequestInfo::new(
                    request.get("type")?.as_str()?,
                    request.get("requestId").and_then(Value::as_str),
                ),
            })
        })();
        let response = "internal.delivery.begin.response";
        let Some(params) = parsed else {
            link.send(&respond(
                response,
                &request_id,
                Some("invalid begin request"),
                Map::new(),
            ));
            return;
        };
        let rpc: Arc<dyn TsRpc> = self.clone();
        match self.registry.remote_begin(rpc, params).await {
            Ok(begun) => {
                let mut extra = Map::new();
                extra.insert("subscriptionId".into(), begun.subscription_id.into());
                extra.insert("responseId".into(), begun.response_id.into());
                link.send(&respond(response, &request_id, None, extra));
            }
            Err(error) => link.send(&respond(response, &request_id, Some(&error), Map::new())),
        }
    }

    async fn release(self: Arc<Self>, link: &Arc<Link>, frame: &Value) {
        let request_id = frame
            .get("requestId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let id = frame
            .get("subscriptionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let result = self.registry.remote_release(id).await;
        link.send(&respond(
            "internal.delivery.release.response",
            &request_id,
            result.err().as_deref(),
            Map::new(),
        ));
    }
}

fn respond(
    response_type: &str,
    request_id: &str,
    error: Option<&str>,
    extra: Map<String, Value>,
) -> Value {
    let mut payload = Map::new();
    payload.insert("requestId".into(), request_id.into());
    payload.extend(extra);
    payload.insert(
        "error".into(),
        error.map_or(Value::Null, |e| Value::String(e.to_owned())),
    );
    json!({ "type": response_type, "payload": Value::Object(payload) })
}

#[async_trait]
impl TsRpc for InternalRpc {
    async fn call(&self, request_type: &str, payload: Value) -> Result<Value, String> {
        let link = self
            .current()
            .ok_or_else(|| "internal rpc link is not connected".to_owned())?;
        link.call(request_type, payload).await
    }
}
