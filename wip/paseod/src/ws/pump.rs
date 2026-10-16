//! The per-socket pump: client <-> internal backend socket.
//!
//! Tasks per physical socket:
//! - client actor: owns the client socket, drains the outbound queue into it and reports inbound frames
//! - backend reader: reads the internal socket, detects `server_info`, charges the outbound budget
//! - controller (the caller's task): lease, ping/pong, routing, client -> backend writes, shutdown
//!
//! Frames are moved as `String`/`Bytes` without re-serialization.

use std::fmt::Debug;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::stream::SplitStream;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use super::{
    ApplicationLease, BackendLink, BackendStream, Message, OutboundBudget, PhysicalSocket,
    ProxyOutcome, WsProxy,
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

/// Handler for frames whose route is [`Route::Owned`].
pub trait Dispatcher: Debug + Send + Sync + 'static {
    fn dispatch(&self, domain: Domain, msg: Message, reply: &Replier) -> Dispatch;
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

enum Event {
    Client(Message),
    ClientEnded { close: Option<(u16, String)> },
    BackendEnded { code: Option<u16>, reason: String },
    BackendError(String),
    HighWater { buffered: usize },
}

type Budget = Arc<Mutex<OutboundBudget>>;

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
) {
    loop {
        tokio::select! {
            item = out_rx.recv() => match item {
                Some(OutItem::Frame { msg, reserved }) => {
                    let sent = sock.send(msg).await;
                    lock(&budget).release(reserved);
                    if sent.is_err() {
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
            msg = sock.recv() => match msg {
                Some(Ok(m)) => {
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

async fn close_backend(sink: &mut Sink, code: u16, reason: &str) {
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: clip_reason(reason).into(),
    };
    let _ = tokio::time::timeout(CLOSE_WAIT, async {
        let _ = sink.send(WsMsg::Close(Some(frame))).await;
        let _ = sink.close().await;
    })
    .await;
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
    let (mut sink, stream) = link.stream.split();
    let budget: Budget = Arc::new(Mutex::new(OutboundBudget::new(
        proxy.limits.outbound_high_water_bytes,
    )));
    let server_info_seen = Arc::new(AtomicBool::new(false));
    let (out_tx, out_rx) = unbounded_channel::<OutItem>();
    let (ev_tx, mut ev_rx) = unbounded_channel::<Event>();

    let actor = tokio::spawn(client_actor(client, out_rx, ev_tx.clone(), budget.clone()));
    let reader = tokio::spawn(backend_reader(
        stream,
        out_tx.clone(),
        ev_tx.clone(),
        budget.clone(),
        server_info_seen.clone(),
    ));
    drop(ev_tx);
    let replier = Replier { tx: out_tx.clone() };

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
                        lease.renew(now);
                        if let Message::Text(t) = &msg {
                            if server_info_seen.load(Ordering::Relaxed) && is_top_level_ping(t) {
                                lease.claim(now);
                                replier.send(Message::Text(PONG.to_owned()));
                                continue;
                            }
                        }
                        let route = peek_route(&proxy.owned, match &msg {
                            Message::Text(t) => FrameRef::Text(t),
                            Message::Binary(b) => FrameRef::Binary(b),
                        });
                        let forward = match route {
                            Route::Forward => Some(msg),
                            Route::Owned(domain) => match proxy.dispatcher.dispatch(domain, msg, &replier) {
                                Dispatch::Handled => None,
                                Dispatch::Forward(m) => Some(m),
                            },
                        };
                        if let Some(m) = forward {
                            if let Err(e) = sink.send(to_ws(m)).await {
                                let _ = out_tx.send(OutItem::Close { code: 1011, reason: "backend write failed".into() });
                                break ProxyOutcome::Error(e.to_string());
                            }
                        }
                    }
                    Event::ClientEnded { close } => {
                        let code = match close {
                            Some((c, _)) if is_sendable_close_code(c) => c,
                            _ => 1000,
                        };
                        let reason = close.map(|(_, r)| r).unwrap_or_default();
                        close_backend(&mut sink, code, &reason).await;
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
                        close_backend(&mut sink, 1000, "client outbound high-water").await;
                        break ProxyOutcome::HighWater { buffered };
                    }
                }
            }
            _ = tick.tick() => {
                if lease.is_expired(Instant::now()) {
                    actor.abort();
                    close_backend(&mut sink, 1000, "client lease expired").await;
                    break ProxyOutcome::LeaseExpired;
                }
            }
        }
    };

    reader.abort();
    drop(out_tx);
    drop(replier);
    await_actor(actor).await;
    if !matches!(
        outcome,
        ProxyOutcome::ClientClosed | ProxyOutcome::BackendClosed { .. }
    ) {
        let _ = sink.close().await;
    }
    outcome
}

/// Lets queued frames and the close frame flush to the client, bounded.
async fn await_actor(mut actor: JoinHandle<()>) {
    if tokio::time::timeout(CLOSE_WAIT, &mut actor).await.is_err() {
        actor.abort();
    }
}
