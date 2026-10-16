//! JSON-RPC multiplexer: matches responses to pending requests and forwards everything else
//! (notifications, server requests) to one event stream.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::error::{Error, Result};
use crate::protocol::{self, Incoming, Notification, RawServerRequest, RequestId};
use crate::transport::{ExitInfo, Transport, TransportControl, TransportEvent};

/// Everything the server sends that is not the answer to one of our requests.
#[derive(Debug, Clone, PartialEq)]
pub enum RpcEvent {
    Notification(Notification),
    ServerRequest(RawServerRequest),
    /// A line that is not a JSON-RPC frame. Reported, never fatal.
    InvalidLine {
        line: String,
        error: String,
    },
    /// The transport ended. Always the last event.
    Closed(ExitInfo),
}

type Pending = HashMap<i64, oneshot::Sender<Result<Value>>>;

struct Inner {
    outgoing: mpsc::UnboundedSender<String>,
    pending: Mutex<Pending>,
    next_id: AtomicI64,
    closed: AtomicBool,
    exit: Mutex<Option<ExitInfo>>,
    control: Arc<dyn TransportControl>,
}

#[derive(Clone)]
pub struct RpcClient {
    inner: Arc<Inner>,
}

impl RpcClient {
    pub fn start(
        transport: Box<dyn Transport>,
    ) -> Result<(RpcClient, mpsc::UnboundedReceiver<RpcEvent>)> {
        let io = transport.start()?;
        let inner = Arc::new(Inner {
            outgoing: io.outgoing,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            closed: AtomicBool::new(false),
            exit: Mutex::new(None),
            control: io.control,
        });
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let driver = inner.clone();
        let mut incoming = io.incoming;
        tokio::spawn(async move {
            let mut exit = ExitInfo::default();
            while let Some(event) = incoming.recv().await {
                match event {
                    TransportEvent::Line(line) => driver.dispatch(&line, &events_tx),
                    TransportEvent::Exited(info) => {
                        exit = info;
                        break;
                    }
                }
            }
            driver.fail_all(&exit);
            let _ = events_tx.send(RpcEvent::Closed(exit));
        });
        Ok((RpcClient { inner }, events_rx))
    }

    /// Same underlying connection.
    pub fn same(&self, other: &RpcClient) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Send a request and wait for its response. `timeout: None` waits for a response, process
    /// exit, or [`RpcClient::close`].
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        if self.is_closed() {
            return Err(self.closed_error());
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .map_err(|_| Error::Closed)?
            .insert(id, tx);
        if self.is_closed() {
            // lost a race with the driver's fail_all
            self.remove_pending(id);
            return Err(self.closed_error());
        }
        if self
            .inner
            .outgoing
            .send(protocol::request_line(id, method, &params))
            .is_err()
        {
            self.remove_pending(id);
            return Err(self.closed_error());
        }
        let waited = match timeout {
            Some(limit) => match tokio::time::timeout(limit, rx).await {
                Ok(r) => r,
                Err(_) => {
                    self.remove_pending(id);
                    return Err(Error::Timeout {
                        method: method.to_string(),
                    });
                }
            },
            None => rx.await,
        };
        waited.unwrap_or(Err(Error::Closed))
    }

    pub fn notify(&self, method: &str, params: Value) {
        if !self.is_closed() {
            let _ = self
                .inner
                .outgoing
                .send(protocol::notification_line(method, &params));
        }
    }

    /// Answer a server request.
    pub fn respond(&self, id: &RequestId, result: Value) {
        if !self.is_closed() {
            let _ = self
                .inner
                .outgoing
                .send(protocol::response_line(id, &result));
        }
    }

    pub fn respond_error(&self, id: &RequestId, message: &str) {
        if !self.is_closed() {
            let _ = self
                .inner
                .outgoing
                .send(protocol::error_response_line(id, message));
        }
    }

    /// Fail pending requests and terminate the server.
    pub async fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.fail_pending(None);
        self.inner.control.terminate().await;
    }

    fn remove_pending(&self, id: i64) {
        if let Ok(mut p) = self.inner.pending.lock() {
            p.remove(&id);
        }
    }

    fn closed_error(&self) -> Error {
        match self.inner.exit.lock().ok().and_then(|g| g.clone()) {
            Some(info) => Error::Exited(info.describe()),
            None => Error::Closed,
        }
    }
}

impl Inner {
    fn dispatch(&self, line: &str, events: &mpsc::UnboundedSender<RpcEvent>) {
        let incoming = match Incoming::parse_line(line) {
            Ok(i) => i,
            Err(e) => {
                let _ = events.send(RpcEvent::InvalidLine {
                    line: line.to_string(),
                    error: e.to_string(),
                });
                return;
            }
        };
        match incoming {
            Incoming::Response {
                id: RequestId::Int(id),
                result,
            } => {
                self.settle(id, Ok(result));
            }
            Incoming::Error {
                id: Some(RequestId::Int(id)),
                error,
            } => {
                self.settle(id, Err(Error::Rpc(Box::new(error))));
            }
            // A response nothing waits for (or with an id we never issued) carries no event.
            Incoming::Response { .. } | Incoming::Error { .. } => {}
            Incoming::Request(req) => {
                let _ = events.send(RpcEvent::ServerRequest(req));
            }
            Incoming::Notification(n) => {
                let _ = events.send(RpcEvent::Notification(n));
            }
        }
    }

    fn settle(&self, id: i64, result: Result<Value>) {
        let tx = self.pending.lock().ok().and_then(|mut p| p.remove(&id));
        if let Some(tx) = tx {
            let _ = tx.send(result);
        }
    }

    /// `exit`: why the server went away; `None` for a deliberate close.
    fn fail_pending(&self, exit: Option<&ExitInfo>) {
        let drained: Vec<_> = match self.pending.lock() {
            Ok(mut p) => p.drain().collect(),
            Err(_) => return,
        };
        for (_, tx) in drained {
            let _ = tx.send(Err(match exit {
                Some(info) => Error::Exited(info.describe()),
                None => Error::Closed,
            }));
        }
    }

    fn fail_all(&self, exit: &ExitInfo) {
        self.closed.store(true, Ordering::SeqCst);
        if let Ok(mut e) = self.exit.lock() {
            *e = Some(exit.clone());
        }
        self.fail_pending(Some(exit));
    }
}
