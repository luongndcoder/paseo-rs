//! JSON-RPC multiplexer over a [`TransportHandle`]: correlates responses with requests and
//! hands agent-initiated traffic to the caller. Typed wrappers cover the ACP agent methods.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::error::{Error, Result};
use crate::protocol::{
    agent_methods as m, classify, id_key, notification_frame, request_frame, response_err_frame,
    response_ok_frame, Frame, InitializeParams, InitializeResponse, ListSessionsParams,
    ListSessionsResponse, LoadSessionParams, NewSessionParams, PromptParams, PromptResponse,
    RpcError, SessionState, SetConfigOptionParams, SetConfigOptionResponse, SetModeParams,
    SetModelParams,
};
use crate::transport::{ProcessControl, TransportHandle, WireEvent};

/// Acknowledgement half of an ordering barrier; compares equal always (test ergonomics).
#[derive(Debug, Clone)]
pub struct BarrierAck(Arc<Mutex<Option<oneshot::Sender<()>>>>);

impl PartialEq for BarrierAck {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl BarrierAck {
    fn fire(&self) {
        if let Some(tx) = self.0.lock().ok().and_then(|mut g| g.take()) {
            let _ = tx.send(());
        }
    }
}

/// Agent-initiated traffic.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// Non-JSON stdout line (ignored by the adapter, surfaced for logging).
    Malformed(String),
    /// Ordering marker queued at the position of an ordered response; fires once every earlier
    /// item has been handled.
    #[doc(hidden)]
    Barrier(BarrierAck),
    /// The inbound stream ended; every pending request has already failed with `Closed`.
    Closed,
}

type Reply = (
    std::result::Result<Value, RpcError>,
    Option<oneshot::Receiver<()>>,
);
struct Waiter {
    tx: oneshot::Sender<Reply>,
    /// Deliver the reply only after the consumer handled everything read before it.
    ordered: bool,
}
type Pending = Mutex<HashMap<String, Waiter>>;

struct Inner {
    outgoing: mpsc::UnboundedSender<Value>,
    pending: Pending,
    next_id: AtomicI64,
    closed: AtomicBool,
    process: Arc<dyn ProcessControl>,
}

#[derive(Clone)]
pub struct Connection {
    inner: Arc<Inner>,
}

impl Connection {
    /// Start the reader task. Returns the connection and the stream of agent-initiated traffic.
    pub fn start(handle: TransportHandle) -> (Connection, mpsc::UnboundedReceiver<Incoming>) {
        let TransportHandle {
            outgoing,
            mut incoming,
            process,
        } = handle;
        let (tx, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            outgoing,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(0),
            closed: AtomicBool::new(false),
            process,
        });
        let reader = inner.clone();
        tokio::spawn(async move {
            while let Some(event) = incoming.recv().await {
                match event {
                    WireEvent::Frame(value) => match classify(value) {
                        Ok(Frame::Response { id, result }) => {
                            let waiter = reader
                                .pending
                                .lock()
                                .ok()
                                .and_then(|mut p| p.remove(&id_key(&id)));
                            if let Some(w) = waiter {
                                let ack = if w.ordered {
                                    let (a_tx, a_rx) = oneshot::channel();
                                    let ack = BarrierAck(Arc::new(Mutex::new(Some(a_tx))));
                                    tx.send(Incoming::Barrier(ack)).ok().map(|_| a_rx)
                                } else {
                                    None
                                };
                                let _ = w.tx.send((result, ack));
                            }
                        }
                        Ok(Frame::Request { id, method, params }) => {
                            let _ = tx.send(Incoming::Request { id, method, params });
                        }
                        Ok(Frame::Notification { method, params }) => {
                            let _ = tx.send(Incoming::Notification { method, params });
                        }
                        Err(raw) => {
                            let _ = tx.send(Incoming::Malformed(raw.to_string()));
                        }
                    },
                    WireEvent::Malformed(line) => {
                        let _ = tx.send(Incoming::Malformed(line));
                    }
                    WireEvent::Closed => break,
                }
            }
            reader.closed.store(true, Ordering::SeqCst);
            // Dropping the senders fails every waiter with `Closed`.
            if let Ok(mut p) = reader.pending.lock() {
                p.clear();
            }
            let _ = tx.send(Incoming::Closed);
        });
        (Connection { inner }, rx)
    }

    pub fn process(&self) -> &Arc<dyn ProcessControl> {
        &self.inner.process
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    pub async fn request_raw(&self, method: &str, params: Value) -> Result<Value> {
        self.request_raw_inner(method, params, false).await
    }

    async fn request_raw_inner(&self, method: &str, params: Value, ordered: bool) -> Result<Value> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .map_err(|_| Error::Closed)?
            .insert(id_key(&json!(id)), Waiter { tx, ordered });
        // The reader may have closed between the check above and the insert.
        if self.is_closed() {
            if let Ok(mut p) = self.inner.pending.lock() {
                p.remove(&id_key(&json!(id)));
            }
            return Err(Error::Closed);
        }
        if self
            .inner
            .outgoing
            .send(request_frame(id, method, params))
            .is_err()
        {
            return Err(Error::Closed);
        }
        match rx.await {
            Ok((result, ack)) => {
                if let Some(ack) = ack {
                    // Dropped when the consumer is gone; nothing left to order against.
                    let _ = ack.await;
                }
                result.map_err(Error::Rpc)
            }
            Err(_) => Err(Error::Closed),
        }
    }

    pub async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R> {
        let raw = self
            .request_raw(method, serde_json::to_value(params)?)
            .await?;
        // A handler that returns nothing answers `null`; treat it as an empty object.
        let raw = if raw.is_null() { json!({}) } else { raw };
        serde_json::from_value(raw).map_err(|e| Error::Protocol(format!("{method} response: {e}")))
    }

    pub async fn request_with_timeout<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
        timeout: Duration,
    ) -> Result<R> {
        crate::util::with_timeout(
            timeout,
            format!("ACP {method} timed out after {}ms", timeout.as_millis()),
            self.request(method, params),
        )
        .await
    }

    pub fn notify_raw(&self, method: &str, params: Value) -> Result<()> {
        self.inner
            .outgoing
            .send(notification_frame(method, params))
            .map_err(|_| Error::Closed)
    }

    pub fn respond(&self, id: Value, result: std::result::Result<Value, RpcError>) -> Result<()> {
        let frame = match result {
            Ok(v) => response_ok_frame(id, v),
            Err(e) => response_err_frame(id, &e),
        };
        self.inner.outgoing.send(frame).map_err(|_| Error::Closed)
    }

    // ---- typed agent methods ----

    pub async fn initialize(&self, params: &InitializeParams) -> Result<InitializeResponse> {
        self.request(m::INITIALIZE, params).await
    }

    pub async fn authenticate(&self, method_id: &str) -> Result<Value> {
        self.request_raw(m::AUTHENTICATE, json!({ "methodId": method_id }))
            .await
    }

    pub async fn new_session(&self, params: &NewSessionParams) -> Result<SessionState> {
        self.request(m::SESSION_NEW, params).await
    }

    pub async fn load_session(&self, params: &LoadSessionParams) -> Result<SessionState> {
        self.request(m::SESSION_LOAD, params).await
    }

    pub async fn resume_session(&self, params: &LoadSessionParams) -> Result<SessionState> {
        self.request(m::SESSION_RESUME, params).await
    }

    pub async fn list_sessions(&self, params: &ListSessionsParams) -> Result<ListSessionsResponse> {
        self.request(m::SESSION_LIST, params).await
    }

    pub async fn close_session(&self, session_id: &str) -> Result<Value> {
        self.request_raw(m::SESSION_CLOSE, json!({ "sessionId": session_id }))
            .await
    }

    /// The response is delivered on a separate path from `session/update` notifications, so
    /// it resolves only after the consumer drained everything the agent sent before it:
    /// callers see strict wire order (chunks, then the turn result).
    pub async fn prompt(&self, params: &PromptParams) -> Result<PromptResponse> {
        let raw = self
            .request_raw_inner(m::SESSION_PROMPT, serde_json::to_value(params)?, true)
            .await?;
        let raw = if raw.is_null() { json!({}) } else { raw };
        serde_json::from_value(raw)
            .map_err(|e| Error::Protocol(format!("{} response: {e}", m::SESSION_PROMPT)))
    }

    pub fn cancel(&self, session_id: &str) -> Result<()> {
        self.notify_raw(m::SESSION_CANCEL, json!({ "sessionId": session_id }))
    }

    pub async fn set_mode(&self, params: &SetModeParams) -> Result<Value> {
        self.request_raw(m::SESSION_SET_MODE, serde_json::to_value(params)?)
            .await
    }

    pub async fn set_model(&self, params: &SetModelParams) -> Result<Value> {
        self.request_raw(m::SESSION_SET_MODEL, serde_json::to_value(params)?)
            .await
    }

    pub async fn set_config_option(
        &self,
        params: &SetConfigOptionParams,
    ) -> Result<SetConfigOptionResponse> {
        self.request(m::SESSION_SET_CONFIG_OPTION, params).await
    }

    pub async fn ext_method(&self, method: &str, params: Value) -> Result<Value> {
        self.request_raw(method, params).await
    }

    pub fn ext_notification(&self, method: &str, params: Value) -> Result<()> {
        self.notify_raw(method, params)
    }
}

/// Answers one agent-initiated request. The handler owns the response so it can sequence
/// follow-up frames after it (e.g. `session/cancel` right after a denied permission).
pub struct Responder {
    connection: Connection,
    id: Value,
}

impl Responder {
    pub fn send(self, result: std::result::Result<Value, RpcError>) {
        let _ = self.connection.respond(self.id, result);
    }
}

/// Receiver of agent-initiated traffic. Notifications are handled inline, in arrival order;
/// each request runs in its own task so a slow handler (a permission prompt, a terminal
/// wait) never blocks the stream.
pub trait IncomingHandler: Send + Sync + 'static {
    fn on_notification(&self, method: &str, params: Value);
    fn on_request(
        self: Arc<Self>,
        method: String,
        params: Value,
        responder: Responder,
    ) -> crate::util::BoxFuture<'static, ()>;
    fn on_malformed(&self, _line: &str) {}
    fn on_closed(self: Arc<Self>) -> crate::util::BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

/// Pump `incoming` into `handler` until the stream closes.
pub fn drive<H: IncomingHandler>(
    connection: Connection,
    mut incoming: mpsc::UnboundedReceiver<Incoming>,
    handler: Arc<H>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(item) = incoming.recv().await {
            match item {
                Incoming::Notification { method, params } => {
                    handler.on_notification(&method, params)
                }
                Incoming::Request { id, method, params } => {
                    let responder = Responder {
                        connection: connection.clone(),
                        id,
                    };
                    tokio::spawn(handler.clone().on_request(method, params, responder));
                }
                Incoming::Malformed(line) => handler.on_malformed(&line),
                Incoming::Barrier(ack) => ack.fire(),
                Incoming::Closed => {
                    handler.clone().on_closed().await;
                    break;
                }
            }
        }
    })
}
