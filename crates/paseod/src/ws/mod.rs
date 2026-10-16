//! WebSocket edge: physical client sockets and the per-socket proxy to the TS backend.
//!
//! One physical client socket (direct axum upgrade, or relay E2EE channel) = exactly one
//! internal WS to the backend. The same pump serves both via [`PhysicalSocket`].
//!
//! Direct upgrade flow ("connect-then-accept", EDGE-SPEC.md "WS upgrade"):
//! 1. `GET /ws` arrives. Wait for backend `Ready` (else 503 "Server not ready").
//! 2. Dial `ws://<internal>/ws` with the client's `Host`, `Origin`, `Authorization`,
//!    `Sec-WebSocket-Protocol` and every other non-hop-by-hop header copied verbatim, `x-paseo-edge-*` set by the edge.
//!    TS's `verifyWsUpgrade` stays the only Origin/Host/DNS-rebinding judge.
//! 3. Internal handshake rejected (HTTP 403/503) -> answer the client with the same status and body.
//! 4. Internal accepted -> accept the client upgrade echoing the subprotocol TS selected, then pump.
//!
//! Pump rules:
//! - Text and binary frames forwarded byte-for-byte, both directions, order preserved.
//!   Never re-serialize JSON; [`crate::routing::peek_route`] only borrows.
//! - Edge owns the top-level application lease: once the backend's `server_info` has been
//!   seen outbound, an inbound `{"type":"ping"}` is answered `{"type":"pong"}` by the edge and not
//!   forwarded; any inbound frame renews; 45 s without a frame closes both sockets.
//!   Before `server_info`, ping is forwarded (TS decides).
//! - Inbound backpressure: bytes of client frames accepted but not yet written to the backend
//!   are capped at `Limits::inbound_high_water_bytes`; past it the edge stops reading that
//!   client socket. Backend writes run in their own task so a slow backend never stalls the
//!   controller (lease, close, high-water).
//! - Outbound high-water: bytes queued toward one client socket may not exceed
//!   `Limits::outbound_high_water_bytes`; crossing it terminates only that client socket (no close
//!   frame, the peer is not reading) and closes the internal socket 1000. Bytes are counted in an
//!   unbounded queue guarded by [`OutboundBudget`], since tungstenite has no `bufferedAmount`.
//! - Close codes/reasons from the backend are mirrored to the client (4001, 4002, 4003, 4401, 1001...),
//!   with 1005/1006/1015 mapped to 1011 since they cannot be sent on the wire.
//! - Internal drop -> close client 1011; client drop -> close internal 1000. TS retains the
//!   reconnectable session exactly as for a direct disconnect.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::backend::Backend;
use crate::config::Limits;
use crate::error::Result;
use crate::routing::OwnedTypes;

mod dial;
mod direct;
#[cfg(test)]
mod proxy_tests;
mod pump;

pub use dial::BackendStream;
pub use direct::{handle_upgrade, ws_router, DirectSocket};
pub use pump::{Dispatch, Dispatcher, ForwardAll, Replier, SocketCtx, SocketWriter};

/// One physical client socket, numbered by the edge in the order they are dialed. The id is sent
/// to the backend in [`crate::internal::HEADER_SOCKET_ID`] and names the socket in
/// `internal.delivery.*`. Never reused within a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SocketId(pub u64);

/// An owned WebSocket data frame. Control frames never reach this layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Bytes),
}

impl Message {
    pub fn byte_len(&self) -> usize {
        match self {
            Message::Text(t) => t.len(),
            Message::Binary(b) => b.len(),
        }
    }
}

/// How the physical client reached the edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Direct { remote: Option<SocketAddr> },
    Relay { connection_id: String },
}

/// Request facts forwarded to the backend on the internal upgrade.
#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub transport: Transport,
    /// Client request headers copied verbatim (Host, Origin, Cookie, Authorization,
    /// Sec-WebSocket-Protocol, ...); see `dial::is_blocked_header` for what is dropped.
    /// Empty for relay (relay peers have no HTTP request).
    pub forwarded_headers: Vec<(String, String)>,
}

/// Callback that returns outbound budget; see [`PhysicalSocket::send_tracked`].
pub type Release = Box<dyn FnOnce() + Send>;

/// One physical client connection, direct or relay. Implemented by [`DirectSocket`] (axum)
/// and `relay` (E2EE channel). Generic, not `dyn`, to keep the hot path allocation-free.
///
/// The pump owns the socket in one task and polls `recv` and `send_tracked` from a `select!`, so
/// `recv` MUST be cancel-safe (dropping the future loses no frame).
pub trait PhysicalSocket: Send + 'static {
    /// Next data frame; `None` on clean close. The pump renews the lease itself.
    fn recv(&mut self) -> impl Future<Output = Option<Result<Message>>> + Send;
    fn send(&mut self, msg: Message) -> impl Future<Output = Result<()>> + Send;
    /// Like [`PhysicalSocket::send`], but `release` runs once the frame left the edge's queue
    /// (written, or the socket died). The pump uses it to return outbound budget.
    ///
    /// The default awaits the write, so a slow client blocks the caller. Sockets whose writes can
    /// stall (a TCP peer that does not read) override this to enqueue to a writer task and
    /// return at once, so the caller keeps reading inbound frames and renewing the lease.
    fn send_tracked(
        &mut self,
        msg: Message,
        release: Release,
    ) -> impl Future<Output = Result<()>> + Send {
        async move {
            let result = self.send(msg).await;
            release();
            result
        }
    }
    /// Sends a close frame with `code` and `reason` and shuts the socket.
    fn close(&mut self, code: u16, reason: String) -> impl Future<Output = ()> + Send;
    /// Code and reason of the close frame the peer sent, once `recv` returned `None`.
    /// Forwarded to the backend. Default: unknown.
    fn peer_close(&self) -> Option<(u16, String)> {
        None
    }
}

/// Why a proxied socket ended; for logs and metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyOutcome {
    ClientClosed,
    BackendClosed { code: Option<u16> },
    LeaseExpired,
    HighWater { buffered: usize },
    Error(String),
}

/// Where internal sockets are dialed.
#[derive(Debug, Clone)]
pub(crate) enum Upstream {
    Backend(Arc<Backend>),
    /// Fixed target; used by tests and by tools that front a backend without supervising it.
    Fixed {
        ws_url: String,
        token: String,
    },
}

impl Upstream {
    pub(crate) fn target(&self) -> Option<(String, String)> {
        match self {
            Upstream::Backend(b) => b
                .internal_ws_url()
                .map(|url| (url, b.edge_token().to_owned())),
            Upstream::Fixed { ws_url, token } => Some((ws_url.clone(), token.clone())),
        }
    }
}

/// Shared state for all sockets.
#[derive(Debug, Clone)]
pub struct WsProxy {
    pub(crate) upstream: Upstream,
    pub owned: Arc<OwnedTypes>,
    pub limits: Limits,
    pub dispatcher: Arc<dyn Dispatcher>,
    next_socket: Arc<std::sync::atomic::AtomicU64>,
}

impl WsProxy {
    pub fn new(backend: Arc<Backend>, owned: Arc<OwnedTypes>, limits: Limits) -> Self {
        Self {
            upstream: Upstream::Backend(backend),
            owned,
            limits,
            dispatcher: Arc::new(ForwardAll),
            next_socket: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    /// Proxy to a fixed internal URL (`ws://host:port/ws`) with a given edge token.
    pub fn new_fixed(
        ws_url: impl Into<String>,
        token: impl Into<String>,
        owned: Arc<OwnedTypes>,
        limits: Limits,
    ) -> Self {
        Self {
            upstream: Upstream::Fixed {
                ws_url: ws_url.into(),
                token: token.into(),
            },
            owned,
            limits,
            dispatcher: Arc::new(ForwardAll),
            next_socket: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    pub(crate) fn next_socket_id(&self) -> SocketId {
        SocketId(
            self.next_socket
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        )
    }

    /// Installs the handler that receives frames whose route is [`crate::routing::Route::Owned`].
    pub fn with_dispatcher(mut self, dispatcher: Arc<dyn Dispatcher>) -> Self {
        self.dispatcher = dispatcher;
        self
    }

    /// Dials the internal WS for `info` and pumps until either side ends.
    ///
    /// Returns after both sockets are closed. `client` is already accepted. Used by relay
    /// (which has no HTTP upgrade to hold open); the direct path calls
    /// [`WsProxy::dial_backend`] first (steps 2-4 above) and then [`WsProxy::pump`].
    pub async fn run<S: PhysicalSocket>(&self, mut client: S, info: ClientInfo) -> ProxyOutcome {
        match self.dial_backend(&info).await {
            Ok(link) => self.pump(client, link).await,
            Err(rej) => {
                client
                    .close(1011, format!("backend unavailable ({})", rej.status))
                    .await;
                ProxyOutcome::Error(format!("backend rejected upgrade: {}", rej.status))
            }
        }
    }

    /// Steps 2-3 of the direct flow. `Err(UpgradeRejection)` carries the backend's HTTP status
    /// and body for the client response.
    pub async fn dial_backend(
        &self,
        info: &ClientInfo,
    ) -> std::result::Result<BackendLink, UpgradeRejection> {
        dial::dial(self, info).await
    }

    /// Pumps frames between an accepted client socket and an established internal link.
    pub async fn pump<S: PhysicalSocket>(&self, client: S, link: BackendLink) -> ProxyOutcome {
        pump::pump(self, client, link).await
    }
}

/// An established internal WS.
#[derive(Debug)]
pub struct BackendLink {
    /// Subprotocol TS selected (`paseo.bearer.<token>` when a password is set); echo to the client.
    pub selected_protocol: Option<String>,
    pub(crate) stream: BackendStream,
    /// The id this socket carries in `x-paseo-edge-socket-id`.
    pub socket_id: SocketId,
}

#[derive(Debug, Clone)]
pub struct UpgradeRejection {
    pub status: u16,
    pub body: String,
}

/// Application lease, ported from `ApplicationSocketLease` (`physical-socket.ts:12`).
/// Pure and clock-injectable so it can be unit tested without timers.
#[derive(Debug)]
pub struct ApplicationLease {
    ttl: Duration,
    deadline: Option<Instant>,
}

impl ApplicationLease {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            deadline: None,
        }
    }

    /// First ping after admission starts the lease.
    pub fn claim(&mut self, now: Instant) {
        self.deadline = Some(now + self.ttl);
    }

    /// Any inbound frame renews, but only once claimed.
    pub fn renew(&mut self, now: Instant) {
        if self.deadline.is_some() {
            self.claim(now);
        }
    }

    pub fn is_expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|d| d <= now)
    }
}

/// Byte budget for the queue toward one client socket (see "Outbound high-water" above).
#[derive(Debug)]
pub struct OutboundBudget {
    max: usize,
    queued: usize,
}

impl OutboundBudget {
    pub fn new(max: usize) -> Self {
        Self { max, queued: 0 }
    }

    /// Mirrors `physicalSocketHasCapacity`: `queued + frame <= max`. On success reserves the bytes.
    pub fn try_reserve(&mut self, frame_bytes: usize) -> bool {
        if self.queued + frame_bytes > self.max {
            return false;
        }
        self.queued += frame_bytes;
        true
    }

    pub fn release(&mut self, frame_bytes: usize) {
        self.queued = self.queued.saturating_sub(frame_bytes);
    }

    pub fn queued(&self) -> usize {
        self.queued
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_only_runs_after_claim() {
        let t0 = Instant::now();
        let mut l = ApplicationLease::new(Duration::from_secs(45));
        l.renew(t0);
        assert!(!l.is_expired(t0 + Duration::from_secs(100)));
        l.claim(t0);
        assert!(!l.is_expired(t0 + Duration::from_secs(44)));
        assert!(l.is_expired(t0 + Duration::from_secs(45)));
        l.renew(t0 + Duration::from_secs(40));
        assert!(!l.is_expired(t0 + Duration::from_secs(80)));
    }

    #[test]
    fn budget_rejects_over_max() {
        let mut b = OutboundBudget::new(10);
        assert!(b.try_reserve(10));
        assert!(!b.try_reserve(1));
        b.release(10);
        assert!(b.try_reserve(1));
    }
}
