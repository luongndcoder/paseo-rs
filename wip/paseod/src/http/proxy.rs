//! Reverse proxy to the TS backend: streaming bodies, SSE, raw upgrade tunneling.

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::response::Response;
use http::header::{HeaderName, HeaderValue, CONNECTION, UPGRADE};
use http::{HeaderMap, StatusCode};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};

use super::AppState;
use crate::internal::{
    HEADER_REMOTE_ADDR, HEADER_TOKEN, HEADER_TRANSPORT, STRIPPED_CLIENT_HEADERS,
};

pub type ProxyClient = Client<HttpConnector, Body>;

pub fn new_client() -> ProxyClient {
    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    Client::builder(TokioExecutor::new()).build(connector)
}

const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
];

/// Headers that describe the client's own proxy chain. TS trusts them from a loopback peer
/// (its default `trust proxy`), and the edge is always that peer, so a client must not be able
/// to set them: dropped, then re-added from facts the edge knows.
const FORWARDING_HEADERS: [&str; 6] = [
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-port",
    "x-forwarded-prefix",
    "forwarded",
];

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|s| s.split(','))
        .filter_map(|t| HeaderName::from_bytes(t.trim().as_bytes()).ok())
        .collect();
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
    headers.remove(UPGRADE);
    for name in named {
        headers.remove(name);
    }
}

pub(super) fn is_upgrade_request(headers: &HeaderMap) -> bool {
    headers.contains_key(UPGRADE)
        && headers
            .get_all(CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|s| s.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
}

fn is_loopback_peer(peer: Option<SocketAddr>) -> bool {
    peer.is_some_and(|p| p.ip().to_canonical().is_loopback())
}

/// Headers for the internal request: the client's, minus hop-by-hop and any forged edge
/// identity, plus the edge's own identity headers.
fn outbound_headers(
    inbound: &HeaderMap,
    peer: Option<SocketAddr>,
    token: &str,
    upgrade: bool,
) -> HeaderMap {
    let mut headers = inbound.clone();
    let upgrade_value = headers.get(UPGRADE).cloned();
    strip_hop_by_hop(&mut headers);
    for name in STRIPPED_CLIENT_HEADERS {
        headers.remove(name);
    }
    let keep_forwarded = is_loopback_peer(peer);
    for name in FORWARDING_HEADERS {
        if keep_forwarded && name != "x-forwarded-for" {
            continue;
        }
        headers.remove(name);
    }
    if upgrade {
        if let Some(value) = upgrade_value {
            headers.insert(UPGRADE, value);
            headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
        }
    }
    if let Ok(value) = HeaderValue::from_str(token) {
        headers.insert(HeaderName::from_static(HEADER_TOKEN), value);
    }
    headers.insert(
        HeaderName::from_static(HEADER_TRANSPORT),
        HeaderValue::from_static("direct"),
    );
    if let Some(peer) = peer {
        if let Ok(value) = HeaderValue::from_str(&peer.to_string()) {
            headers.insert(HeaderName::from_static(HEADER_REMOTE_ADDR), value);
        }
        if let Ok(value) = HeaderValue::from_str(&peer.ip().to_canonical().to_string()) {
            headers.insert(HeaderName::from_static("x-forwarded-for"), value);
        }
    }
    headers
}

pub(super) fn json_error(status: StatusCode, message: &str) -> Response {
    let body = format!("{{\"error\":\"{message}\"}}");
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

/// Fallback handler: forwards the request to the backend and streams the response back.
pub async fn reverse_proxy(State(state): State<AppState>, mut req: Request) -> Response {
    let Some(addr) = state.backend.internal_addr() else {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "Server not ready");
    };
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    let upgrade = is_upgrade_request(req.headers());
    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut req));

    let path_and_query = req
        .uri()
        .path_and_query()
        .map_or("/", |pq| pq.as_str())
        .to_string();
    let headers = outbound_headers(req.headers(), peer, state.backend.edge_token(), upgrade);
    let method = req.method().clone();
    let body = if upgrade {
        Body::empty()
    } else {
        std::mem::take(req.body_mut())
    };

    let mut outbound = match http::Request::builder()
        .method(method)
        .uri(format!("http://{addr}{path_and_query}"))
        .body(body)
    {
        Ok(outbound) => outbound,
        Err(err) => {
            tracing::debug!(%err, "rejecting request that cannot be forwarded");
            return json_error(StatusCode::BAD_REQUEST, "Bad request");
        }
    };
    *outbound.headers_mut() = headers;

    let mut response = match state.proxy.request(outbound).await {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!(%err, %addr, "backend request failed");
            return json_error(StatusCode::BAD_GATEWAY, "Bad gateway");
        }
    };

    if upgrade && response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let backend_upgrade = hyper::upgrade::on(&mut response);
        let mut reply = Response::new(Body::empty());
        *reply.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        *reply.headers_mut() = response.headers().clone();
        if let Some(client_upgrade) = client_upgrade {
            tokio::spawn(async move {
                let (client, backend) = match tokio::join!(client_upgrade, backend_upgrade) {
                    (Ok(client), Ok(backend)) => (client, backend),
                    (client, backend) => {
                        tracing::debug!(
                            client_err = ?client.err(),
                            backend_err = ?backend.err(),
                            "upgrade handoff failed"
                        );
                        return;
                    }
                };
                let mut client = TokioIo::new(client);
                let mut backend = TokioIo::new(backend);
                if let Err(err) = tokio::io::copy_bidirectional(&mut client, &mut backend).await {
                    tracing::debug!(%err, "upgrade tunnel ended with error");
                }
            });
        }
        return reply;
    }

    let (mut parts, incoming) = response.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    Response::from_parts(parts, Body::new(incoming))
}
