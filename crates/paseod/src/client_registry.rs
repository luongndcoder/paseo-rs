//! Sockets <-> core sessions (spec section 9).
//!
//! [`ClientRegistry::register`] takes a socket's identity (`clientId`, capabilities) and its
//! [`SocketWriter`] and returns the [`OutboundSink`] the core writes through. The writer is the
//! SAME per-socket queue the proxy pump uses for frames it forwards from TS, so one task ends up
//! writing every frame of a socket, and the delivery gate
//! ([`paseo_core::delivery::SourceGate::permits`]) is enforced exactly there and nowhere else.
//!
//! [`CoreDispatcher`] is the pump's hook. It observes `hello` (still forwarded to TS, which owns
//! admission) to register the socket, and hands claimed session messages to the socket's
//! session. Frames of one socket are processed strictly in order by one worker task, so a
//! session message can never overtake its `hello`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use paseo_core::delivery::{DeliveryError, Frame, Outbound, OutboundSink, SourceGate, SourceId};
use paseo_core::session::caps::ClientCaps;
use paseo_core::session::{
    AttachRequest, ClientHello, SessionHandle, SessionRegistry, HANDLED_INBOUND,
};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::routing::Domain;
use crate::ws::{Dispatch, Dispatcher, Message, Replier, SocketCtx, SocketId, SocketWriter};

/// The principal of every direct and relay client (spec section 8). Hub sessions are admitted by
/// TS and never reach a Rust session in P4b.
pub const OWNER_PRINCIPAL: &str = "owner";

/// Bytes of owned frames one socket may have queued for its session before the edge drops it.
/// The pump releases a frame's inbound budget when it hands the frame over, so without this a
/// client could flood a slow session unnoticed.
pub const SESSION_QUEUE_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// What the core needs to know about a connecting client.
#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub client_id: String,
    pub capabilities: ClientCaps,
    pub app_version: Option<String>,
}

impl ClientInfo {
    /// Reads a top-level `hello` message. `None` when `clientId` is missing or empty (TS rejects
    /// those with close 4002; the edge leaves that to TS).
    pub fn from_hello(hello: &Value) -> Option<Self> {
        let client_id = hello.get("clientId")?.as_str().filter(|s| !s.is_empty())?;
        Some(Self {
            client_id: client_id.to_owned(),
            capabilities: ClientCaps::from_hello(hello.get("capabilities")),
            app_version: hello
                .get("appVersion")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

/// The core's sink for one socket: the delivery gate in front of the socket's writer queue.
pub struct GatedWriter {
    writer: SocketWriter,
    gate: OnceLock<Arc<SourceGate>>,
    denied: AtomicUsize,
}

impl GatedWriter {
    fn new(writer: SocketWriter) -> Arc<Self> {
        Arc::new(Self {
            writer,
            gate: OnceLock::new(),
            denied: AtomicUsize::new(0),
        })
    }

    /// Frames the gate refused (ownership proof missing or expired).
    pub fn denied(&self) -> usize {
        self.denied.load(Ordering::Relaxed)
    }
}

impl OutboundSink for GatedWriter {
    fn send(&self, out: Outbound) {
        // No gate yet means the socket is still being attached: nothing may be written.
        let permitted = self.gate.get().is_some_and(|gate| gate.permits(&out));
        if !permitted {
            self.denied.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let message = match out.body {
            Frame::Text(text) => Message::Text(text.to_string()),
            Frame::Binary(bytes) => Message::Binary(bytes.to_vec().into()),
        };
        // `false` means the high-water mark was crossed (the socket is being closed) or the
        // socket is gone; either way the frame is not retried.
        let _ = self.writer.write(message);
    }
}

struct Registered {
    source: SourceId,
    handle: SessionHandle,
    writer: Arc<GatedWriter>,
}

/// Which socket belongs to which core session.
pub struct ClientRegistry {
    sessions: SessionRegistry,
    sockets: Mutex<HashMap<SocketId, Registered>>,
}

impl ClientRegistry {
    pub fn new(sessions: SessionRegistry) -> Self {
        Self {
            sessions,
            sockets: Mutex::new(HashMap::new()),
        }
    }

    pub fn sessions(&self) -> &SessionRegistry {
        &self.sessions
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SocketId, Registered>> {
        self.sockets.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The source id the core uses for `socket` (the same number: socket ids are unique per edge,
    /// and TS learns it in `x-paseo-edge-socket-id`).
    pub fn source_of(socket: SocketId) -> SourceId {
        SourceId(socket.0)
    }

    /// Registers `socket` with its logical session and returns the sink the core writes through.
    pub async fn register(
        &self,
        socket: SocketId,
        info: ClientInfo,
        writer: SocketWriter,
    ) -> Result<Arc<dyn OutboundSink>, DeliveryError> {
        let source = Self::source_of(socket);
        let sink = GatedWriter::new(writer);
        let attached = self
            .sessions
            .attach(AttachRequest {
                principal: OWNER_PRINCIPAL.to_owned(),
                client_id: info.client_id,
                source,
                hello: ClientHello {
                    capabilities: info.capabilities,
                    app_version: info.app_version,
                },
                out: sink.clone(),
            })
            .await?;
        sink.gate.set(attached.gate).ok();
        self.lock().insert(
            socket,
            Registered {
                source,
                handle: attached.handle,
                writer: sink.clone(),
            },
        );
        Ok(sink)
    }

    /// The socket ended. Its session lingers for the reconnect grace.
    pub async fn unregister(&self, socket: SocketId) {
        let registered = self.lock().remove(&socket);
        if registered.is_some() {
            self.sessions.detach(Self::source_of(socket)).await;
        }
    }

    /// Hands one unwrapped session message to the socket's session.
    pub async fn inbound(&self, socket: SocketId, message: Value) -> bool {
        let target = self
            .lock()
            .get(&socket)
            .map(|r| (r.source, r.handle.clone()));
        match target {
            Some((source, handle)) => handle.inbound(source, message).await,
            None => false,
        }
    }

    pub fn is_registered(&self, socket: SocketId) -> bool {
        self.lock().contains_key(&socket)
    }

    /// Frames the gate refused for `socket`.
    pub fn denied(&self, socket: SocketId) -> usize {
        self.lock().get(&socket).map_or(0, |r| r.writer.denied())
    }
}

enum Work {
    Hello(ClientInfo),
    Inbound(Value, usize),
}

struct SocketState {
    queue: mpsc::UnboundedSender<Work>,
    writer: SocketWriter,
    hello_seen: AtomicBool,
    queued_bytes: Arc<AtomicUsize>,
}

/// The pump's dispatcher for [`Domain::Core`].
pub struct CoreDispatcher {
    registry: Arc<ClientRegistry>,
    sockets: Mutex<HashMap<SocketId, Arc<SocketState>>>,
}

impl std::fmt::Debug for CoreDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreDispatcher").finish_non_exhaustive()
    }
}

impl CoreDispatcher {
    pub fn new(registry: Arc<ClientRegistry>) -> Arc<Self> {
        Arc::new(Self {
            registry,
            sockets: Mutex::new(HashMap::new()),
        })
    }

    pub fn registry(&self) -> &Arc<ClientRegistry> {
        &self.registry
    }

    fn state(&self, socket: SocketId) -> Option<Arc<SocketState>> {
        self.sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&socket)
            .cloned()
    }

    fn enqueue(state: &SocketState, work: Work, bytes: usize) {
        let queued = state.queued_bytes.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if queued > SESSION_QUEUE_LIMIT_BYTES {
            state
                .writer
                .close(1008, "session queue overflow".to_owned());
            return;
        }
        let _ = state.queue.send(work);
    }
}

async fn run_socket(
    registry: Arc<ClientRegistry>,
    socket: SocketId,
    writer: SocketWriter,
    queued_bytes: Arc<AtomicUsize>,
    mut queue: mpsc::UnboundedReceiver<Work>,
) {
    let mut registered = false;
    while let Some(work) = queue.recv().await {
        match work {
            Work::Hello(info) => {
                if registered {
                    continue;
                }
                match registry.register(socket, info, writer.clone()).await {
                    Ok(_) => registered = true,
                    Err(error) => {
                        tracing::warn!(%error, ?socket, "core session attach failed");
                        writer.close(1011, "core session unavailable".to_owned());
                        break;
                    }
                }
            }
            Work::Inbound(message, bytes) => {
                queued_bytes.fetch_sub(bytes, Ordering::Relaxed);
                if registered {
                    registry.inbound(socket, message).await;
                }
            }
        }
    }
    registry.unregister(socket).await;
}

impl Dispatcher for CoreDispatcher {
    fn dispatch(&self, _domain: Domain, msg: Message, _reply: &Replier) -> Dispatch {
        Dispatch::Forward(msg)
    }

    fn socket_opened(&self, socket: &SocketCtx) {
        let (queue, rx) = mpsc::unbounded_channel();
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let state = Arc::new(SocketState {
            queue,
            writer: socket.writer.clone(),
            hello_seen: AtomicBool::new(false),
            queued_bytes: queued_bytes.clone(),
        });
        self.sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(socket.id, state);
        tokio::spawn(run_socket(
            self.registry.clone(),
            socket.id,
            socket.writer.clone(),
            queued_bytes,
            rx,
        ));
    }

    fn dispatch_from(
        &self,
        socket: &SocketCtx,
        domain: Domain,
        msg: Message,
        _reply: &Replier,
    ) -> Dispatch {
        let Message::Text(text) = &msg else {
            return Dispatch::Forward(msg);
        };
        if domain != Domain::Core {
            return Dispatch::Forward(msg);
        }
        let Some(state) = self.state(socket.id) else {
            return Dispatch::Forward(msg);
        };
        let Ok(frame) = serde_json::from_str::<Value>(text) else {
            return Dispatch::Forward(msg);
        };
        match frame.get("type").and_then(Value::as_str) {
            Some("hello") => {
                // Observed only: TS still validates and answers it.
                if let Some(info) = ClientInfo::from_hello(&frame) {
                    if !state.hello_seen.swap(true, Ordering::SeqCst) {
                        Self::enqueue(&state, Work::Hello(info), 0);
                    }
                }
                Dispatch::Forward(msg)
            }
            Some("session") => {
                let Some(inner) = frame.get("message").cloned() else {
                    return Dispatch::Forward(msg);
                };
                let handled = inner
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| HANDLED_INBOUND.contains(&t));
                // Before `hello` TS must see the frame (it rejects it); unknown types are TS's.
                if !handled || !state.hello_seen.load(Ordering::SeqCst) {
                    return Dispatch::Forward(msg);
                }
                let bytes = text.len();
                Self::enqueue(&state, Work::Inbound(inner, bytes), bytes);
                Dispatch::Handled
            }
            _ => Dispatch::Forward(msg),
        }
    }

    fn socket_closed(&self, socket: SocketId) {
        // Dropping the state closes the queue; the worker drains what is left, then unregisters.
        self.sockets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&socket);
    }
}
