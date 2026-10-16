//! HTTP surface of the edge (axum). EDGE-SPEC.md "HTTP routes" is the per-route table.
//!
//! P2 policy: the edge owns `GET /ws` only. Every other request, including the web UI, is
//! reverse proxied to the backend with `Host` preserved and `x-paseo-edge-*` set, because the TS
//! middleware chain (service-proxy host classification, Host allowlist, CORS, bearer auth) is
//! order-sensitive and its decisions must stay in one place.
//!
//! Reverse proxy contract:
//! - Streaming bodies both ways (file downloads, MCP streamable HTTP/SSE), no buffering.
//! - Hop-by-hop headers dropped; every other header, status and body byte unchanged.
//! - Inbound `x-paseo-edge-*` stripped ([`crate::internal::STRIPPED_CLIENT_HEADERS`]) before the
//!   edge adds its own.
//! - Backend not ready -> `503 {"error":"Server not ready"}`.
//! - Static web UI: served by the edge only when `EdgeConfig::web_ui` is set (see [`static_ui`]);
//!   otherwise it is reverse proxied like everything else.
//! - Non-`/ws` Upgrade requests (service-proxy script WebSockets) are tunneled as raw upgraded
//!   byte streams to the backend (no frame parsing). `/ws` on a registered service Host is too
//!   ([`crate::internal::HOST_CLASS_PATH`] decides).

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, Request, State};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use http_body_util::BodyExt;
use tokio::net::TcpListener;

use crate::backend::Backend;
use crate::config::{EdgeConfig, ListenAddr};
use crate::error::{EdgeError, Result};
use crate::ws::WsProxy;

pub mod proxy;
pub mod rpc;
pub mod static_ui;

#[cfg(test)]
mod tests;

/// Shared axum state.
#[derive(Debug, Clone)]
pub struct AppState {
    pub config: Arc<EdgeConfig>,
    pub backend: Arc<Backend>,
    pub ws: Arc<WsProxy>,
    pub proxy: proxy::ProxyClient,
    /// The in-process `/mcp/agents` endpoint (`paseo_mcp::mcp_router`), set under
    /// `PASEO_CORE=rust` once the core exists. `None`: the route is reverse proxied to TS.
    pub agent_mcp: Option<Router>,
    /// `GET /__paseo_edge/rpc`: the core <-> TS channel (`internal.delivery.*`), built by
    /// [`rpc::router`]. `None` unless the core is on.
    pub internal_rpc: Option<Router>,
}

impl AppState {
    pub fn new(config: Arc<EdgeConfig>, backend: Arc<Backend>, ws: Arc<WsProxy>) -> Self {
        Self {
            config,
            backend,
            ws,
            proxy: proxy::new_client(),
            agent_mcp: None,
            internal_rpc: None,
        }
    }

    /// Mounts the internal RPC route ([`rpc::router`]).
    pub fn with_internal_rpc(mut self, router: Router) -> Self {
        self.internal_rpc = Some(router);
        self
    }

    /// Serves `/mcp/agents` in process. The endpoint does its own auth (capability token or the
    /// daemon password), so it bypasses the TS middleware chain like the TS route does for the
    /// bearer check; the Host allowlist and CORS that TS applies first are not applied here.
    pub fn with_agent_mcp(mut self, router: Router) -> Self {
        self.agent_mcp = Some(router);
        self
    }
}

/// Route the in-process agent MCP endpoint is mounted on.
pub const AGENT_MCP_ROUTE: &str = "/mcp/agents";

/// Builds the router: `/ws` -> [`ws_upgrade`], `/mcp/agents` -> the in-process agent MCP
/// endpoint when configured, fallback -> static web UI or reverse proxy.
pub fn router(state: AppState) -> Router {
    let mut router = Router::new().route("/ws", any(ws_upgrade));
    if let Some(mcp) = state.agent_mcp.clone() {
        router = router.route_service(AGENT_MCP_ROUTE, mcp);
    }
    if let Some(rpc) = state.internal_rpc.clone() {
        router = router.route_service(crate::internal_rpc::RPC_ROUTE, rpc);
    }
    router.fallback(fallback).with_state(state)
}

/// `/ws` hook: frame-aware proxy to the backend (lease, high-water, routing hook), unless the
/// Host is a script service that serves its own `/ws` (TS tunnels those by Host before the daemon
/// socket sees them), which is tunneled raw like any other upgrade.
async fn ws_upgrade(State(state): State<AppState>, req: Request) -> Response {
    if is_service_host(&state, req.headers()).await {
        return proxy::reverse_proxy(State(state), req).await;
    }
    crate::ws::handle_upgrade(state.ws.clone(), req).await
}

const HOST_CLASS_TIMEOUT: Duration = Duration::from_secs(2);

/// Authority without its port; `None` for hosts that can never be a service host (IP literals,
/// bare `localhost`, empty), which skips the backend round trip for the common case.
fn candidate_service_host(host: &str) -> Option<&str> {
    if host.starts_with('[') {
        return None;
    }
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    if name.is_empty() || name.eq_ignore_ascii_case("localhost") || name.parse::<IpAddr>().is_ok() {
        return None;
    }
    Some(host)
}

/// Asks the backend whether `Host` is a registered service host. Any failure means "daemon",
/// so a backend hiccup never turns the daemon socket into a raw tunnel.
async fn is_service_host(state: &AppState, headers: &http::HeaderMap) -> bool {
    let Some(host) = headers
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .and_then(candidate_service_host)
    else {
        return false;
    };
    let Some(addr) = state.backend.internal_addr() else {
        return false;
    };
    let query: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("host", host)
        .finish();
    let Ok(token) = http::HeaderValue::from_str(state.backend.edge_token()) else {
        return false;
    };
    let Ok(request) = http::Request::builder()
        .uri(format!(
            "http://{addr}{}?{query}",
            crate::internal::HOST_CLASS_PATH
        ))
        .header(crate::internal::HEADER_TOKEN, token)
        .body(axum::body::Body::empty())
    else {
        return false;
    };
    let ask = async {
        let response = state.proxy.request(request).await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body = response.into_body().collect().await.ok()?.to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).ok()?;
        value.get("service").and_then(serde_json::Value::as_bool)
    };
    matches!(
        tokio::time::timeout(HOST_CLASS_TIMEOUT, ask).await,
        Ok(Some(true))
    )
}

async fn fallback(State(state): State<AppState>, req: Request) -> Response {
    if let Some(ui) = &state.config.web_ui {
        if static_ui::handles(req.method(), req.uri().path()) {
            let peer = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0);
            return static_ui::serve(ui, req.method(), req.uri().path(), req.headers(), peer).await;
        }
    }
    proxy::reverse_proxy(State(state), req).await
}

/// A bound public listener.
#[derive(Debug)]
pub enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix {
        listener: tokio::net::UnixListener,
        path: PathBuf,
    },
}

impl Listener {
    /// The address clients connect to, in `PASEO_LISTEN` grammar (resolves port 0).
    pub fn public_address(&self) -> String {
        match self {
            Listener::Tcp(l) => l
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| "unknown".into()),
            #[cfg(unix)]
            Listener::Unix { path, .. } => path.display().to_string(),
        }
    }
}

/// Binds `addr`. Unix sockets: unlink a stale file first (as `bootstrap.ts:1862`).
pub async fn bind(addr: &ListenAddr) -> Result<Listener> {
    match addr {
        ListenAddr::Tcp(sock) => TcpListener::bind(sock)
            .await
            .map(Listener::Tcp)
            .map_err(|err| EdgeError::Config(format!("cannot listen on {sock}: {err}"))),
        #[cfg(unix)]
        ListenAddr::Unix(path) => {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(EdgeError::Config(format!(
                        "cannot remove stale socket {}: {err}",
                        path.display()
                    )))
                }
            }
            tokio::net::UnixListener::bind(path)
                .map(|listener| Listener::Unix {
                    listener,
                    path: path.clone(),
                })
                .map_err(|err| {
                    EdgeError::Config(format!("cannot listen on {}: {err}", path.display()))
                })
        }
        #[cfg(not(unix))]
        ListenAddr::Unix(path) => Err(EdgeError::Config(format!(
            "unix sockets are not supported on this platform: {}",
            path.display()
        ))),
    }
}

/// Serves `router` on `listener` until `shutdown` resolves. A Unix socket file is unlinked on exit.
pub async fn serve_on(
    state: AppState,
    listener: Listener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let app = router(state);
    match listener {
        Listener::Tcp(listener) => axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(EdgeError::Io),
        #[cfg(unix)]
        Listener::Unix { listener, path } => {
            let result = axum::serve(listener, app.into_make_service())
                .with_graceful_shutdown(shutdown)
                .await
                .map_err(EdgeError::Io);
            let _ = std::fs::remove_file(path);
            result
        }
    }
}

/// Binds `config.listen` and serves until `shutdown` resolves.
pub async fn serve(
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let listener = bind(&state.config.listen).await?;
    serve_on(state, listener, shutdown).await
}
