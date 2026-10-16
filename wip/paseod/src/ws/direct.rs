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

use super::{ClientInfo, Message, PhysicalSocket, Transport, WsProxy};
use crate::error::{EdgeError, Result};

/// Request headers the backend needs for its own Host/Origin/auth decisions.
const FORWARDED: [&str; 5] = [
    "host",
    "origin",
    "user-agent",
    "authorization",
    "sec-websocket-protocol",
];

/// `GET /ws` handler body. Connect-then-accept: the client upgrade is only accepted after the
/// internal handshake succeeded, so a backend 403/503 reaches the client unchanged.
pub async fn handle_upgrade(proxy: Arc<WsProxy>, req: Request) -> Response {
    let (mut parts, _body) = req.into_parts();
    let remote = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let forwarded_headers = FORWARDED
        .iter()
        .flat_map(|name| {
            parts
                .headers
                .get_all(*name)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .map(|v| ((*name).to_owned(), v.to_owned()))
                .collect::<Vec<_>>()
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

/// axum WebSocket as a [`PhysicalSocket`]. WS-level ping/pong is handled by axum and never surfaces.
#[derive(Debug)]
pub struct DirectSocket {
    inner: WebSocket,
    peer_close: Option<(u16, String)>,
}

impl DirectSocket {
    pub fn new(inner: WebSocket) -> Self {
        Self {
            inner,
            peer_close: None,
        }
    }
}

fn io_err(e: axum::Error) -> EdgeError {
    EdgeError::Io(std::io::Error::other(e.to_string()))
}

impl PhysicalSocket for DirectSocket {
    async fn recv(&mut self) -> Option<Result<Message>> {
        loop {
            match self.inner.recv().await? {
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
        let out = match msg {
            Message::Text(t) => AxMessage::Text(t.into()),
            Message::Binary(b) => AxMessage::Binary(b),
        };
        self.inner.send(out).await.map_err(io_err)
    }

    async fn close(&mut self, code: u16, reason: String) {
        let frame = CloseFrame {
            code,
            reason: reason.into(),
        };
        if self
            .inner
            .send(AxMessage::Close(Some(frame)))
            .await
            .is_err()
        {
            return;
        }
        // Let the peer's close reply arrive so the TCP stream ends cleanly.
        let _ = tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(Ok(_)) = self.inner.recv().await {}
        })
        .await;
    }

    fn peer_close(&self) -> Option<(u16, String)> {
        self.peer_close.clone()
    }
}
