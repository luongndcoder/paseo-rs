//! Backend: spawns and supervises the TS worker that sits behind the edge.
//!
//! Contract (EDGE-SPEC.md "Backend contract"):
//! - Edge picks a free loopback port, spawns the worker with `PASEO_LISTEN=127.0.0.1:<port>`,
//!   `PASEO_EDGE_TOKEN`, `PASEO_EDGE_PUBLIC_LISTEN`. Relay env is passed through untouched; the worker
//!   skips its own relay transport in edge mode but still reports the real relay config.
//! - Worker stdout lines prefixed `@@paseo ` are lifecycle events; everything else is
//!   forwarded to the edge's own stdout/stderr.
//! - Worker stdin is a pipe owned by the edge; worker exits when it closes.
//! - Readiness is `paseo:ready`, then `GET /api/health` on the internal port must return 200.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use base64::Engine;
use rand::RngCore;
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use crate::config::BackendConfig;
use crate::error::{EdgeError, Result};
use crate::internal::LifecycleEvent;

mod supervisor;

pub use supervisor::public_origins;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyInfo {
    /// Internal loopback address the worker listens on.
    pub internal_addr: SocketAddr,
    /// The worker's `serverId` (needed for the relay data path).
    pub server_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendState {
    Starting,
    Ready(ReadyInfo),
    /// Worker exited or asked for restart; edge respawns (crash backoff in the builder).
    Restarting {
        reason: String,
    },
    /// Gave up (ready timeout, repeated crashes). Edge answers 503 and exits non-zero.
    Failed {
        reason: String,
    },
}

/// Handle to the supervised TS worker. Cheap to clone via `Arc`.
#[derive(Debug)]
pub struct Backend {
    state: watch::Receiver<BackendState>,
    edge_token: String,
    lifecycle: broadcast::Sender<LifecycleEvent>,
    shutdown: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
    /// Only for `fixed` backends: keeps the watch channel open.
    _keepalive: Option<watch::Sender<BackendState>>,
}

/// 32 random bytes, base64url without padding.
fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

impl Backend {
    /// Spawns the worker and starts supervising it. Returns immediately in `Starting`.
    ///
    /// Contract: generates `edge_token` (32 random bytes, base64url); never passes it via argv.
    pub async fn spawn(cfg: &BackendConfig, public_listen: &str) -> Result<Arc<Self>> {
        if cfg.command.is_empty() {
            return Err(EdgeError::Backend("empty worker command".into()));
        }
        let edge_token = generate_token();
        let (state_tx, state_rx) = watch::channel(BackendState::Starting);
        let (life_tx, _) = broadcast::channel(16);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let ctx = supervisor::Ctx {
            cfg: cfg.clone(),
            public_listen: public_listen.to_string(),
            token: edge_token.clone(),
            state_tx,
            life_tx: life_tx.clone(),
            shutdown_rx,
        };
        let handle = tokio::spawn(supervisor::supervise(ctx));
        Ok(Arc::new(Self {
            state: state_rx,
            edge_token,
            lifecycle: life_tx,
            shutdown: shutdown_tx,
            task: Mutex::new(Some(handle)),
            _keepalive: None,
        }))
    }

    /// A backend that is permanently `Ready` at `addr`, with no worker process behind it.
    /// For tests (fake backends) and for running the edge against an already-running daemon.
    pub fn fixed(addr: SocketAddr, edge_token: impl Into<String>) -> Arc<Self> {
        Self::with_state(
            BackendState::Ready(ReadyInfo {
                internal_addr: addr,
                server_id: "fixed".into(),
            }),
            edge_token,
        )
    }

    /// A backend frozen in `state`, with no worker process. For tests.
    pub fn with_state(state: BackendState, edge_token: impl Into<String>) -> Arc<Self> {
        let (tx, rx) = watch::channel(state);
        let (life_tx, _) = broadcast::channel(1);
        let (shutdown_tx, _) = watch::channel(false);
        Arc::new(Self {
            state: rx,
            edge_token: edge_token.into(),
            lifecycle: life_tx,
            shutdown: shutdown_tx,
            task: Mutex::new(None),
            _keepalive: Some(tx),
        })
    }

    /// Watch channel of backend state; `ws`/`http` gate requests on `Ready`.
    pub fn state(&self) -> watch::Receiver<BackendState> {
        self.state.clone()
    }

    /// Resolves with [`ReadyInfo`] once ready, or an error on `Failed`/timeout.
    pub async fn wait_ready(&self) -> Result<ReadyInfo> {
        let mut rx = self.state.clone();
        loop {
            match &*rx.borrow_and_update() {
                BackendState::Ready(info) => return Ok(info.clone()),
                BackendState::Failed { reason } => return Err(EdgeError::Backend(reason.clone())),
                BackendState::Starting | BackendState::Restarting { .. } => {}
            }
            rx.changed()
                .await
                .map_err(|_| EdgeError::Backend("backend supervisor stopped".into()))?;
        }
    }

    /// Secret the edge sends as `x-paseo-edge-token` on every internal request.
    pub fn edge_token(&self) -> &str {
        &self.edge_token
    }

    /// Internal address when `Ready`, else `None`.
    pub fn internal_addr(&self) -> Option<SocketAddr> {
        match &*self.state.borrow() {
            BackendState::Ready(info) => Some(info.internal_addr),
            _ => None,
        }
    }

    /// `ws://127.0.0.1:<port>/ws` once ready, else `None`.
    pub fn internal_ws_url(&self) -> Option<String> {
        self.internal_addr().map(|addr| format!("ws://{addr}/ws"))
    }

    /// Graceful stop: SIGTERM (Unix) / stdin shutdown line (Windows), then kill after a grace period.
    /// Resolves once the worker process is gone. Safe to call more than once.
    pub async fn shutdown(&self) -> Result<()> {
        let _ = self.shutdown.send(true);
        let handle = self.task.lock().expect("backend task lock").take();
        if let Some(handle) = handle {
            handle
                .await
                .map_err(|err| EdgeError::Backend(format!("supervisor task failed: {err}")))?;
        }
        Ok(())
    }

    /// Lifecycle events the worker emitted that the edge must relay to its own
    /// supervisor (`paseo:shutdown`, `paseo:restart`).
    pub fn subscribe_lifecycle(&self) -> broadcast::Receiver<LifecycleEvent> {
        self.lifecycle.subscribe()
    }
}

#[cfg(test)]
mod tests;
