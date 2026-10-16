//! Direct (non-relay) client sockets: the axum `/ws` upgrade handler and its [`PhysicalSocket`].

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{CloseFrame, Message as AxMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, FromRequestParts, Request};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{ClientInfo, Message, PhysicalSocket, Release, Transport, WsProxy};
use crate::error::{EdgeError, Result};

const CLOSE_FLUSH: Duration = Duration::from_secs(5);

/// `GET /ws` handler body. Connect-then-accept: the client upgrade is only accepted after the
/// internal handshake succeeded, so a backend 403/503 reaches the client unchanged.
pub async fn handle_upgrade(proxy: Arc<WsProxy>, req: Request) -> Response {
    let (mut parts, _body) = req.into_parts();
    let remote = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    // Everything the client sent; the dial drops handshake, hop-by-hop, forwarding-chain and
    // edge-identity headers. Cookie and custom headers must reach TS like a direct upgrade.
    let forwarded_headers = parts
        .headers
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(u) => u,
        Err(rejection) => return rejection.into_response(),
    };
    let info = ClientInfo {
        transport: Transport::Direct { remote },
        forwarded_headers,
    };
    let link = match proxy.dial_backend(&info).await {
        Ok(link) => link,
        Err(rej) => {
            let status = axum::http::StatusCode::from_u16(rej.status)
                .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
            return (status, Body::from(rej.body)).into_response();
        }
    };
    let max = proxy.limits.max_message_bytes;
    let mut upgrade = upgrade.max_message_size(max).max_frame_size(max);
    if let Some(p) = &link.selected_protocol {
        upgrade = upgrade.protocols([p.clone()]);
    }
    upgrade.on_upgrade(move |socket| async move {
        let outcome = proxy.pump(DirectSocket::new(socket), link).await;
        tracing::debug!(?outcome, "client ws ended");
    })
}

/// Router with only `GET /ws`; the http module mounts [`handle_upgrade`] on its own state.
pub fn ws_router(proxy: Arc<WsProxy>) -> Router {
    Router::new().route(
        "/ws",
        get(move |req: Request| handle_upgrade(proxy.clone(), req)),
    )
}

/// What the writer task puts on the wire.
enum Outgoing {
    Frame(AxMessage, Option<Release>),
    Close(CloseFrame),
}

/// axum WebSocket as a [`PhysicalSocket`]. WS-level ping/pong is handled by axum and never surfaces.
///
/// Reads and writes are independent: a writer task owns the sink, so a client that stops
/// draining its receive buffer never keeps the pump from reading that client's pings (which
/// renew the lease). `send_tracked` only enqueues; the pump's outbound budget bounds the queue.
#[derive(Debug)]
pub struct DirectSocket {
    stream: SplitStream<WebSocket>,
    tx: mpsc::UnboundedSender<Outgoing>,
    writer: JoinHandle<()>,
    peer_close: Option<(u16, String)>,
}

impl DirectSocket {
    pub fn new(inner: WebSocket) -> Self {
        let (sink, stream) = inner.split();
        let (tx, rx) = mpsc::unbounded_channel();
        let writer = tokio::spawn(write_loop(sink, rx));
        Self {
            stream,
            tx,
            writer,
            peer_close: None,
        }
    }

    fn enqueue(&self, msg: Message, release: Option<Release>) -> Result<()> {
        let out = match msg {
            Message::Text(t) => AxMessage::Text(t.into()),
            Message::Binary(b) => AxMessage::Binary(b),
        };
        self.tx
            .send(Outgoing::Frame(out, release))
            .map_err(|_| EdgeError::Io(std::io::Error::other("client socket writer ended")))
    }
}

impl Drop for DirectSocket {
    /// Dropping the socket drops the connection: queued frames for a client that is gone (or
    /// was cut off for high-water) must not keep a writer stuck on a full TCP buffer.
    fn drop(&mut self) {
        self.writer.abort();
    }
}

async fn write_loop(
    mut sink: SplitSink<WebSocket, AxMessage>,
    mut rx: mpsc::UnboundedReceiver<Outgoing>,
) {
    while let Some(item) = rx.recv().await {
        match item {
            Outgoing::Frame(msg, release) => {
                let sent = sink.send(msg).await;
                if let Some(release) = release {
                    release();
                }
                if sent.is_err() {
                    return;
                }
            }
            Outgoing::Close(frame) => {
                let _ = sink.send(AxMessage::Close(Some(frame))).await;
                return;
            }
        }
    }
}

fn io_err(e: axum::Error) -> EdgeError {
    EdgeError::Io(std::io::Error::other(e.to_string()))
}

impl PhysicalSocket for DirectSocket {
    async fn recv(&mut self) -> Option<Result<Message>> {
        loop {
            match self.stream.next().await? {
                Ok(AxMessage::Text(t)) => return Some(Ok(Message::Text(t.as_str().to_owned()))),
                Ok(AxMessage::Binary(b)) => return Some(Ok(Message::Binary(b))),
                Ok(AxMessage::Ping(_) | AxMessage::Pong(_)) => {}
                Ok(AxMessage::Close(frame)) => {
                    self.peer_close = frame.map(|f| (f.code, f.reason.as_str().to_owned()));
                    return None;
                }
                Err(e) => return Some(Err(io_err(e))),
            }
        }
    }

    async fn send(&mut self, msg: Message) -> Result<()> {
        self.enqueue(msg, None)
    }

    async fn send_tracked(&mut self, msg: Message, release: Release) -> Result<()> {
        self.enqueue(msg, Some(release))
    }

    async fn close(&mut self, code: u16, reason: String) {
        let frame = CloseFrame {
            code,
            reason: reason.into(),
        };
        if self.tx.send(Outgoing::Close(frame)).is_err() {
            return;
        }
        // Queued frames flush first, then the close frame; bounded so a stuck peer cannot hold us.
        if tokio::time::timeout(CLOSE_FLUSH, &mut self.writer)
            .await
            .is_err()
        {
            return;
        }
        // Let the peer's close reply arrive so the TCP stream ends cleanly.
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(Ok(_)) = self.stream.next().await {}
        })
        .await;
    }

    fn peer_close(&self) -> Option<(u16, String)> {
        self.peer_close.clone()
    }
}
