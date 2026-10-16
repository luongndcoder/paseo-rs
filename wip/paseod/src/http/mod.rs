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
//!   byte streams to the backend (no frame parsing).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use tokio::net::TcpListener;

use crate::backend::Backend;
use crate::config::{EdgeConfig, ListenAddr};
use crate::error::{EdgeError, Result};
use crate::ws::WsProxy;

pub mod proxy;
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
}

impl AppState {
    pub fn new(config: Arc<EdgeConfig>, backend: Arc<Backend>, ws: Arc<WsProxy>) -> Self {
        Self {
            config,
            backend,
            ws,
            proxy: proxy::new_client(),
        }
    }
}

/// Builds the router: `/ws` -> [`ws_upgrade`], fallback -> static web UI or reverse proxy.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/ws", any(ws_upgrade))
        .fallback(fallback)
        .with_state(state)
}

/// `/ws` hook: frame-aware proxy to the backend (lease, high-water, routing hook).
async fn ws_upgrade(State(state): State<AppState>, req: Request) -> Response {
    crate::ws::handle_upgrade(state.ws.clone(), req).await
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
