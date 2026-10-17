//! The per-socket pump: client <-> internal backend socket.
//!
//! Tasks per physical socket:
//! - client actor: owns the client socket, drains the outbound queue into it and reports inbound
//!   frames. It stops reading the client while too many inbound bytes wait for the backend.
//! - backend reader: reads the internal socket, detects `server_info`, charges the outbound budget
//! - backend writer: owns the internal sink; the only place that awaits a backend write
//! - controller (the caller's task): lease, ping/pong, routing, shutdown. It never awaits I/O on
//!   either socket, so lease ticks, close and high-water stay responsive under backpressure.
//!
//! Frames are moved as `String`/`Bytes` without re-serialization.

use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use super::{
    ApplicationLease, BackendLink, BackendStream, Message, OutboundBudget, PhysicalSocket,
    ProxyOutcome, Release, SocketId, WsProxy,
};
use crate::routing::{is_server_info, is_top_level_ping, peek_route, Domain, FrameRef, Route};

const PONG: &str = r#"{"type":"pong"}"#;
const CLOSE_WAIT: Duration = Duration::from_secs(5);
const MAX_CLOSE_REASON_BYTES: usize = 123;

/// What a domain handler did with a frame it owns.
#[derive(Debug)]
pub enum Dispatch {
    /// Fully handled in Rust; nothing goes to the backend.
    Handled,
    /// Not handled after all: forward the original frame.
    Forward(Message),
}

/// Sends frames to the client of one socket (bypasses the high-water check; replies are small and
/// solicited).
#[derive(Debug, Clone)]
pub struct Replier {
    tx: UnboundedSender<OutItem>,
}

impl Replier {
    pub fn send(&self, msg: Message) {
        let _ = self.tx.send(OutItem::Frame { msg, reserved: 0 });
    }
}

/// The one writer of a physical socket's outbound queue. Unlike [`Replier`] it is budgeted: a
/// frame that would cross the outbound high-water mark ends the socket (the same rule as proxied
/// frames), so a domain that writes straight to a client cannot grow the queue without bound.
/// Clones share the queue; keep none after [`Dispatcher::socket_closed`].
#[derive(Clone)]
pub struct SocketWriter {
    tx: UnboundedSender<OutItem>,
    budget: Budget,
    ev_tx: UnboundedSender<Event>,
}

impl Debug for SocketWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocketWriter").finish_non_exhaustive()
    }
}

impl SocketWriter {
    /// Queues `msg`. `false` when the socket is gone or the frame crossed the high-water mark
    /// (the socket is then being closed).
    pub fn write(&self, msg: Message) -> bool {
        let len = msg.byte_len();
        {
            let mut budget = lock(&self.budget);
            if !budget.try_reserve(len) {
                let buffered = budget.queued() + len;
                drop(budget);
                let _ = self.ev_tx.send(Event::HighWater { buffered });
                return false;
            }
        }
        if self.tx.send(OutItem::Frame { msg, reserved: len }).is_err() {
            lock(&self.budget).release(len);
            return false;
        }
        true
    }

    /// Ends the socket with `code`.
    pub fn close(&self, code: u16, reason: impl Into<String>) {
        let _ = self.tx.send(OutItem::Close {
            code,
            reason: reason.into(),
        });
    }
}

/// What a dispatcher knows about the socket a frame came from.
#[derive(Debug, Clone)]
pub struct SocketCtx {
    pub id: SocketId,
    pub writer: SocketWriter,
}

/// Handler for frames whose route is [`Route::Owned`].
pub trait Dispatcher: Debug + Send + Sync + 'static {
    fn dispatch(&self, domain: Domain, msg: Message, reply: &Replier) -> Dispatch;

    /// A client socket was accepted and its pump started.
    fn socket_opened(&self, _socket: &SocketCtx) {}

    /// Like [`Dispatcher::dispatch`] but knows the socket. Defaults to the socket-less form.
    fn dispatch_from(
        &self,
        _socket: &SocketCtx,
        domain: Domain,
        msg: Message,
        reply: &Replier,
    ) -> Dispatch {
        self.dispatch(domain, msg, reply)
    }

    /// The socket ended (any reason). Drop every clone of its [`SocketWriter`].
    fn socket_closed(&self, _socket: SocketId) {}
}

/// P2 dispatcher: owns nothing, forwards everything.
#[derive(Debug, Default, Clone, Copy)]
pub struct ForwardAll;

impl Dispatcher for ForwardAll {
    fn dispatch(&self, _domain: Domain, msg: Message, _reply: &Replier) -> Dispatch {
        Dispatch::Forward(msg)
    }
}

enum OutItem {
    Frame { msg: Message, reserved: usize },
    Close { code: u16, reason: String },
}

enum BackendOut {
    Frame(WsMsg, usize),
    Close(CloseFrame),
}

enum Event {
    Client(Message),
    BackendWriteFailed(String),
    ClientEnded { close: Option<(u16, String)> },
    BackendEnded { code: Option<u16>, reason: String },
    BackendError(String),
    HighWater { buffered: usize },
}

type Budget = Arc<Mutex<OutboundBudget>>;

/// Client frames accepted from the socket whose bytes have not yet been written to the backend
/// (or fully handled in Rust). The actor stops reading the client while this is at the cap.
struct Inflight {
    pending: AtomicUsize,
    cap: usize,
    freed: Notify,
}

impl Inflight {
    fn new(cap: usize) -> Self {
        Self {
            pending: AtomicUsize::new(0),
            cap,
            freed: Notify::new(),
        }
    }

    fn has_room(&self) -> bool {
        self.pending.load(Ordering::Relaxed) < self.cap
    }

    fn add(&self, bytes: usize) {
        self.pending.fetch_add(bytes, Ordering::Relaxed);
    }

    // `try_update` (the 1.99 name) is not available on the 1.92 MSRV.
    #[allow(deprecated)]
    fn release(&self, bytes: usize) {
        let _ = self
            .pending
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |p| {
                Some(p.saturating_sub(bytes))
            });
        self.freed.notify_one();
    }
}

fn lock(b: &Budget) -> std::sync::MutexGuard<'_, OutboundBudget> {
    b.lock().unwrap_or_else(|p| p.into_inner())
}

/// Code to put on the wire when mirroring a close. 1005/1006/1015 and other reserved or
/// missing codes cannot be sent and become 1011.
pub(super) fn mirror_close_code(code: Option<u16>) -> u16 {
    code.filter(|c| is_sendable_close_code(*c)).unwrap_or(1011)
}

fn is_sendable_close_code(c: u16) -> bool {
    matches!(c, 1000..=1003 | 1007..=1014 | 3000..=4999)
}

fn clip_reason(reason: &str) -> String {
    if reason.len() <= MAX_CLOSE_REASON_BYTES {
        return reason.to_owned();
    }
    let mut end = MAX_CLOSE_REASON_BYTES;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_owned()
}

async fn client_actor<S: PhysicalSocket>(
    mut sock: S,
    mut out_rx: UnboundedReceiver<OutItem>,
    ev_tx: UnboundedSender<Event>,
    budget: Budget,
    inflight: Arc<Inflight>,
) {
    loop {
        let can_read = inflight.has_room();
        tokio::select! {
            item = out_rx.recv() => match item {
                Some(OutItem::Frame { msg, reserved }) => {
                    let budget = budget.clone();
                    let release: Release = Box::new(move || lock(&budget).release(reserved));
                    if sock.send_tracked(msg, release).await.is_err() {
                        let _ = ev_tx.send(Event::ClientEnded { close: None });
                        return;
                    }
                }
                Some(OutItem::Close { code, reason }) => {
                    sock.close(code, reason).await;
                    return;
                }
                None => return,
            },
            // Not polled while the backend is behind: the client's TCP window fills instead
            // of edge memory. Outbound frames keep flowing through the arm above.
            msg = sock.recv(), if can_read => match msg {
                Some(Ok(m)) => {
                    inflight.add(m.byte_len());
                    if ev_tx.send(Event::Client(m)).is_err() {
                        return;
                    }
                }
                Some(Err(_)) => {
                    let _ = ev_tx.send(Event::ClientEnded { close: None });
                    return;
                }
                None => {
                    let _ = ev_tx.send(Event::ClientEnded { close: sock.peer_close() });
                    return;
                }
            },
            _ = inflight.freed.notified(), if !can_read => {}
        }
    }
}

async fn backend_reader(
    mut stream: SplitStream<BackendStream>,
    out_tx: UnboundedSender<OutItem>,
    ev_tx: UnboundedSender<Event>,
    budget: Budget,
    server_info_seen: Arc<AtomicBool>,
) {
    let enqueue = |msg: Message| -> bool {
        let len = msg.byte_len();
        let mut b = lock(&budget);
        if !b.try_reserve(len) {
            let buffered = b.queued() + len;
            drop(b);
            let _ = ev_tx.send(Event::HighWater { buffered });
            return false;
        }
        drop(b);
        out_tx.send(OutItem::Frame { msg, reserved: len }).is_ok()
    };
    while let Some(item) = stream.next().await {
        let msg = match item {
            Ok(WsMsg::Text(t)) => {
                let text = t.as_str();
                if !server_info_seen.load(Ordering::Relaxed) && is_server_info(text) {
                    server_info_seen.store(true, Ordering::Relaxed);
                }
                Message::Text(text.to_owned())
            }
            Ok(WsMsg::Binary(b)) => Message::Binary(b),
            Ok(WsMsg::Close(frame)) => {
                let (code, reason) = frame
                    .map(|f| (Some(u16::from(f.code)), f.reason.as_str().to_owned()))
                    .unwrap_or((None, String::new()));
                let _ = ev_tx.send(Event::BackendEnded { code, reason });
                return;
            }
            Ok(_) => continue,
            Err(e) => {
                let _ = ev_tx.send(Event::BackendError(e.to_string()));
                return;
            }
        };
        if !enqueue(msg) {
            return;
        }
    }
    let _ = ev_tx.send(Event::BackendEnded {
        code: None,
        reason: String::new(),
    });
}

type Sink = futures_util::stream::SplitSink<BackendStream, WsMsg>;

async fn backend_writer(
    mut sink: Sink,
    mut rx: UnboundedReceiver<BackendOut>,
    ev_tx: UnboundedSender<Event>,
    inflight: Arc<Inflight>,
) {
    while let Some(item) = rx.recv().await {
        match item {
            BackendOut::Frame(msg, bytes) => {
                let sent = sink.send(msg).await;
                inflight.release(bytes);
                if let Err(e) = sent {
                    let _ = ev_tx.send(Event::BackendWriteFailed(e.to_string()));
                    return;
                }
            }
            BackendOut::Close(frame) => {
                let _ = sink.send(WsMsg::Close(Some(frame))).await;
                break;
            }
        }
    }
    let _ = sink.close().await;
}

fn backend_close(code: u16, reason: &str) -> BackendOut {
    BackendOut::Close(CloseFrame {
        code: CloseCode::from(code),
        reason: clip_reason(reason).into(),
    })
}

fn to_ws(msg: Message) -> WsMsg {
    match msg {
        Message::Text(t) => WsMsg::Text(t.into()),
        Message::Binary(b) => WsMsg::Binary(b),
    }
}

pub(super) async fn pump<S: PhysicalSocket>(
    proxy: &WsProxy,
    client: S,
    link: BackendLink,
) -> ProxyOutcome {
    let socket_id = link.socket_id;
    let (sink, stream) = link.stream.split();
    let budget: Budget = Arc::new(Mutex::new(OutboundBudget::new(
        proxy.limits.outbound_high_water_bytes,
    )));
    let inflight = Arc::new(Inflight::new(proxy.limits.inbound_high_water_bytes));
    let server_info_seen = Arc::new(AtomicBool::new(false));
    let (out_tx, out_rx) = unbounded_channel::<OutItem>();
    let (bw_tx, bw_rx) = unbounded_channel::<BackendOut>();
    let (ev_tx, mut ev_rx) = unbounded_channel::<Event>();

    let actor = tokio::spawn(client_actor(
        client,
        out_rx,
        ev_tx.clone(),
        budget.clone(),
        inflight.clone(),
    ));
    let writer = tokio::spawn(backend_writer(sink, bw_rx, ev_tx.clone(), inflight.clone()));
    let reader = tokio::spawn(backend_reader(
        stream,
        out_tx.clone(),
        ev_tx.clone(),
        budget.clone(),
        server_info_seen.clone(),
    ));
    let ctx = SocketCtx {
        id: socket_id,
        writer: SocketWriter {
            tx: out_tx.clone(),
            budget: budget.clone(),
            ev_tx: ev_tx.clone(),
        },
    };
    drop(ev_tx);
    let replier = Replier { tx: out_tx.clone() };
    proxy.dispatcher.socket_opened(&ctx);

    let mut lease = ApplicationLease::new(proxy.limits.lease);
    let mut tick = tokio::time::interval(proxy.limits.lease_check_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;

    let outcome = loop {
        tokio::select! {
            ev = ev_rx.recv() => {
                let Some(ev) = ev else {
                    break ProxyOutcome::Error("pump tasks ended unexpectedly".into());
                };
                match ev {
                    Event::Client(msg) => {
                        let now = Instant::now();
                        let bytes = msg.byte_len();
                        lease.renew(now);
                        if let Message::Text(t) = &msg {
                            if server_info_seen.load(Ordering::Relaxed) && is_top_level_ping(t) {
                                lease.claim(now);
                                replier.send(Message::Text(PONG.to_owned()));
                                inflight.release(bytes);
                                continue;
                            }
                        }
                        let route = peek_route(&proxy.owned, match &msg {
                            Message::Text(t) => FrameRef::Text(t),
                            Message::Binary(b) => FrameRef::Binary(b),
                        });
                        let forward = match route {
                            Route::Forward => Some(msg),
                            Route::Owned(domain) => match proxy.dispatcher.dispatch_from(&ctx, domain, msg, &replier) {
                                Dispatch::Handled => {
                                    inflight.release(bytes);
                                    None
                                }
                                Dispatch::Forward(m) => Some(m),
                            },
                        };
                        if let Some(m) = forward {
                            if bw_tx.send(BackendOut::Frame(to_ws(m), bytes)).is_err() {
                                let _ = out_tx.send(OutItem::Close { code: 1011, reason: "backend write failed".into() });
                                break ProxyOutcome::Error("backend writer ended".into());
                            }
                        }
                    }
                    Event::BackendWriteFailed(e) => {
                        let _ = out_tx.send(OutItem::Close { code: 1011, reason: "backend write failed".into() });
                        break ProxyOutcome::Error(e);
                    }
                    Event::ClientEnded { close } => {
                        let code = match close {
                            Some((c, _)) if is_sendable_close_code(c) => c,
                            _ => 1000,
                        };
                        let reason = close.map(|(_, r)| r).unwrap_or_default();
                        let _ = bw_tx.send(backend_close(code, &reason));
                        break ProxyOutcome::ClientClosed;
                    }
                    Event::BackendEnded { code, reason } => {
                        let _ = out_tx.send(OutItem::Close {
                            code: mirror_close_code(code),
                            reason: clip_reason(&reason),
                        });
                        break ProxyOutcome::BackendClosed { code };
                    }
                    Event::BackendError(e) => {
                        let _ = out_tx.send(OutItem::Close { code: 1011, reason: "backend error".into() });
                        break ProxyOutcome::Error(e);
                    }
                    Event::HighWater { buffered } => {
                        actor.abort();
                        let _ = bw_tx.send(backend_close(1000, "client outbound high-water"));
                        break ProxyOutcome::HighWater { buffered };
                    }
                }
            }
            _ = tick.tick() => {
                if lease.is_expired(Instant::now()) {
                    actor.abort();
                    let _ = bw_tx.send(backend_close(1000, "client lease expired"));
                    break ProxyOutcome::LeaseExpired;
                }
            }
        }
    };

    reader.abort();
    proxy.dispatcher.socket_closed(socket_id);
    drop(ctx);
    drop(out_tx);
    drop(replier);
    // Closing the queue ends the backend writer after it flushes (and sends any close frame).
    drop(bw_tx);
    tokio::join!(await_task(actor), await_task(writer));
    outcome
}

/// Lets queued frames and the close frame flush, bounded.
async fn await_task(mut task: JoinHandle<()>) {
    if tokio::time::timeout(CLOSE_WAIT, &mut task).await.is_err() {
        task.abort();
    }
}
