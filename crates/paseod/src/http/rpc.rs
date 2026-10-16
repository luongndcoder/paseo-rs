//! `GET /__paseo_edge/rpc`: the WebSocket the TS worker opens for `internal.delivery.*`.
//!
//! Token-gated like every other `x-paseo-edge-*` surface: the header must carry the per-run edge
//! token (constant-time compare). One connection at a time is current; a new one replaces the old.

use std::sync::Arc;

use axum::extract::ws::{Message as AxMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use futures_util::{SinkExt, StreamExt};

use crate::internal::HEADER_TOKEN;
use crate::internal_rpc::InternalRpc;

/// Constant-time equality of two tokens.
fn tokens_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The route's service. `token` is the edge token the TS worker was started with.
pub fn router(rpc: Arc<InternalRpc>, token: String) -> Router {
    // The router is mounted with `route_service`, which forwards the full path.
    Router::new().route(
        crate::internal_rpc::RPC_ROUTE,
        any(move |req: Request| {
            let (rpc, token) = (rpc.clone(), token.clone());
            async move { upgrade(rpc, &token, req).await }
        }),
    )
}

async fn upgrade(rpc: Arc<InternalRpc>, token: &str, req: Request) -> Response {
    let presented = req
        .headers()
        .get(HEADER_TOKEN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !tokens_equal(presented.as_bytes(), token.as_bytes()) {
        return (StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    let (mut parts, _body) = req.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(u) => u,
        Err(rejection) => return rejection.into_response(),
    };
    upgrade.on_upgrade(move |socket| serve(rpc, socket))
}

async fn serve(rpc: Arc<InternalRpc>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (link, mut outgoing) = rpc.attach();
    let writer = tokio::spawn(async move {
        while let Some(frame) = outgoing.recv().await {
            if sink.send(AxMessage::Text(frame.into())).await.is_err() {
                return;
            }
        }
    });
    while let Some(Ok(message)) = stream.next().await {
        match message {
            AxMessage::Text(text) => rpc.handle_text(&link, text.as_str()),
            AxMessage::Close(_) => break,
            _ => {}
        }
    }
    rpc.detach(&link);
    writer.abort();
}
