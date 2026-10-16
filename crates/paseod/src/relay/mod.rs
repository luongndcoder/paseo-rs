//! Relay transport owned by the edge (EDGE-SPEC.md "Relay").
//!
//! Port of `relay-transport.ts` + `websocket/encrypted-relay-socket.ts` + `packages/relay`
//! (`encrypted-channel.ts`, `crypto.ts`). Wire parity is the whole job:
//!
//! - [`control`]: control socket `wss://<endpoint>/ws?serverId=<id>&role=server&v=2`; JSON
//!   control messages `sync`/`connected`/`disconnected`/`ping`/`pong`; WS ping every 10 s,
//!   reconnect when stale > 30 s, ready timeout 8 s, backoff `min(30 s, 1 s * attempt)`.
//! - [`data`]: one data socket per `connectionId` (same URL plus `&connectionId=<id>`), 15 s open
//!   timeout, 15 s E2EE handshake: plaintext `e2ee_hello` -> `e2ee_ready`.
//! - [`crypto`] / [`channel`]: NaCl box (X25519 + XSalsa20-Poly1305), bundle
//!   `[nonce(24)][ciphertext]`, text vs binary ciphertext per `binaryCiphertext`, re-hello with
//!   the same key re-sends `e2ee_ready`, a different key closes 1008. Cross-checked against the
//!   real TS packages with `tests/relay-vectors/*.json`.
//! - [`socket`]: each decrypted data socket becomes a [`RelaySocket`] ([`crate::ws::PhysicalSocket`])
//!   and is handed to the same per-socket proxy as direct clients, with
//!   `Transport::Relay { connection_id }` so the backend sees `x-paseo-edge-transport: relay`.
//! - [`keypair`]: `$PASEO_HOME/daemon-keypair.json` is read-only here (TS creates it).
//!
//! Deliberate differences from TS:
//! - The 200-message `pendingSends` queue is not ported: the daemon channel is open as soon as
//!   `e2ee_ready` is queued, so TS never uses it in the daemon role.
//! - `bufferedAmount` is the writer-task queue (bytes accepted, not yet handed to the socket);
//!   kernel buffers are not visible to tokio-tungstenite. The cap is `Limits::outbound_high_water_bytes`.

pub mod b64;
pub mod channel;
pub mod control;
pub mod crypto;
mod data;
mod dial;
pub mod keypair;
pub mod socket;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{watch, Mutex as AsyncMutex};
use tokio::task::JoinHandle;

use crate::backend::Backend;
use crate::config::RelayConfig;
use crate::error::{EdgeError, Result};
use crate::ws::{ClientInfo, WsProxy};

pub use keypair::{load_daemon_keypair, DaemonKeyPair};
pub use socket::{RelaySocket, Shutdown};

/// Receives each relay client after the E2EE handshake. `Arc<WsProxy>` is the production
/// implementation; tests plug in their own.
pub trait SocketHandler: Send + Sync + 'static {
    fn handle(
        &self,
        socket: RelaySocket,
        info: ClientInfo,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

impl SocketHandler for Arc<WsProxy> {
    fn handle(
        &self,
        socket: RelaySocket,
        info: ClientInfo,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let proxy = self.clone();
        Box::pin(async move {
            let outcome = proxy.run(socket, info).await;
            tracing::debug!(?outcome, "relay_client_proxy_ended");
        })
    }
}

struct DataEntry {
    id: u64,
    shutdown: watch::Sender<Shutdown>,
}

/// State shared by the control loop and every data socket task.
pub(crate) struct Inner {
    pub(crate) cfg: RelayConfig,
    pub(crate) keys: DaemonKeyPair,
    pub(crate) server_id: String,
    pub(crate) handler: Arc<dyn SocketHandler>,
    pub(crate) max_buffered: usize,
    stopped: AtomicBool,
    stop_tx: watch::Sender<bool>,
    next_id: AtomicU64,
    data: Mutex<HashMap<String, DataEntry>>,
}

impl Inner {
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    pub(crate) fn stop_rx(&self) -> watch::Receiver<bool> {
        self.stop_tx.subscribe()
    }

    /// `ensureClientDataSocket`: no-op when stopped, empty, or already tracked.
    pub(crate) fn ensure_client_data_socket(self: &Arc<Self>, connection_id: &str) {
        if self.is_stopped() || connection_id.is_empty() {
            return;
        }
        let (tx, rx) = watch::channel(Shutdown::Running);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut map = self.data.lock().unwrap_or_else(|p| p.into_inner());
            if map.contains_key(connection_id) {
                return;
            }
            map.insert(connection_id.to_owned(), DataEntry { id, shutdown: tx });
        }
        if self.is_stopped() {
            // Lost a race with `stop`: its drain may have run before the insert.
            self.close_all();
            return;
        }
        tokio::spawn(data::run_data_socket(
            self.clone(),
            connection_id.to_owned(),
            id,
            rx,
        ));
    }

    /// Control `disconnected`: close with 1001 and forget.
    pub(crate) fn disconnect_client(&self, connection_id: &str) {
        let entry = self
            .data
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(connection_id);
        if let Some(entry) = entry {
            let _ = entry
                .shutdown
                .send(Shutdown::Coded(1001, "Client disconnected".to_owned()));
        }
    }

    pub(crate) fn forget_data_socket(&self, connection_id: &str, entry_id: u64) {
        let mut map = self.data.lock().unwrap_or_else(|p| p.into_inner());
        if map.get(connection_id).is_some_and(|e| e.id == entry_id) {
            map.remove(connection_id);
        }
    }

    fn close_all(&self) {
        let entries: Vec<DataEntry> = self
            .data
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain()
            .map(|(_, v)| v)
            .collect();
        for entry in entries {
            let _ = entry.shutdown.send(Shutdown::Plain);
        }
    }
}

/// Running relay transport. `stop` (or drop) tears down control and data sockets.
pub struct RelayHandle {
    inner: Arc<Inner>,
    task: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for RelayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayHandle")
            .field("server_id", &self.inner.server_id)
            .finish_non_exhaustive()
    }
}

impl RelayHandle {
    /// Starts the control loop. Returns once the task is spawned (connection is async).
    pub async fn start(
        cfg: RelayConfig,
        keypair: DaemonKeyPair,
        server_id: String,
        proxy: Arc<WsProxy>,
    ) -> Result<Self> {
        let max_buffered = proxy.limits.outbound_high_water_bytes;
        Self::start_with_handler(cfg, keypair, server_id, Arc::new(proxy), max_buffered).await
    }

    /// Same as [`RelayHandle::start`] with an explicit client handler.
    pub async fn start_with_handler(
        cfg: RelayConfig,
        keypair: DaemonKeyPair,
        server_id: String,
        handler: Arc<dyn SocketHandler>,
        max_buffered: usize,
    ) -> Result<Self> {
        // Fail fast on a bad endpoint instead of retrying forever in the background.
        control::build_relay_url(&cfg.endpoint, cfg.use_tls, &server_id, None)?;
        let (stop_tx, _) = watch::channel(false);
        let inner = Arc::new(Inner {
            cfg,
            keys: keypair,
            server_id,
            handler,
            max_buffered,
            stopped: AtomicBool::new(false),
            stop_tx,
            next_id: AtomicU64::new(1),
            data: Mutex::new(HashMap::new()),
        });
        let task = tokio::spawn(control::control_loop(inner.clone()));
        Ok(Self {
            inner,
            task: Some(task),
        })
    }

    fn signal_stop(&self) {
        self.inner.stopped.store(true, Ordering::SeqCst);
        let _ = self.inner.stop_tx.send(true);
        self.inner.close_all();
    }

    pub async fn stop(mut self) -> Result<()> {
        self.signal_stop();
        if let Some(task) = self.task.take() {
            task.await
                .map_err(|e| EdgeError::Relay(format!("relay control task failed: {e}")))?;
        }
        Ok(())
    }
}

impl Drop for RelayHandle {
    fn drop(&mut self) {
        if self.task.is_some() {
            self.signal_stop();
        }
    }
}

/// Runtime toggle, replacing TS `RelayRuntime::setEnabled` (`relay-runtime.ts:44`). TS tells the
/// edge when `relay.enabled` changes (`POST /internal/edge/relay` on the edge control listener).
///
/// Call [`RelaySwitch::configure`] once at startup, then [`RelaySwitch::set_enabled`].
#[derive(Default)]
pub struct RelaySwitch {
    setup: Mutex<Option<SwitchSetup>>,
    handle: AsyncMutex<Option<RelayHandle>>,
}

impl std::fmt::Debug for RelaySwitch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelaySwitch").finish_non_exhaustive()
    }
}

struct SwitchSetup {
    cfg: RelayConfig,
    paseo_home: PathBuf,
    handler: Arc<dyn SocketHandler>,
    max_buffered: usize,
}

impl RelaySwitch {
    pub fn new(cfg: RelayConfig, paseo_home: PathBuf, proxy: Arc<WsProxy>) -> Self {
        let switch = Self::default();
        switch.configure(cfg, paseo_home, proxy);
        switch
    }

    /// Supplies endpoint, key location and the proxy. Replaces earlier configuration.
    pub fn configure(&self, cfg: RelayConfig, paseo_home: PathBuf, proxy: Arc<WsProxy>) {
        let max_buffered = proxy.limits.outbound_high_water_bytes;
        self.configure_with_handler(cfg, paseo_home, Arc::new(proxy), max_buffered);
    }

    pub fn configure_with_handler(
        &self,
        cfg: RelayConfig,
        paseo_home: PathBuf,
        handler: Arc<dyn SocketHandler>,
        max_buffered: usize,
    ) {
        *self.setup.lock().unwrap_or_else(|p| p.into_inner()) = Some(SwitchSetup {
            cfg,
            paseo_home,
            handler,
            max_buffered,
        });
    }

    /// Idempotent. `true` waits for the backend `Ready` (needs `serverId`), loads the keypair
    /// and starts the transport; `false` stops it.
    pub async fn set_enabled(&self, enabled: bool, backend: &Backend) -> Result<()> {
        let mut slot = self.handle.lock().await;
        if !enabled {
            if let Some(handle) = slot.take() {
                handle.stop().await?;
            }
            return Ok(());
        }
        if slot.is_some() {
            return Ok(());
        }
        let (cfg, paseo_home, handler, max_buffered) = {
            let guard = self.setup.lock().unwrap_or_else(|p| p.into_inner());
            let setup = guard
                .as_ref()
                .ok_or_else(|| EdgeError::Relay("RelaySwitch is not configured".into()))?;
            (
                setup.cfg.clone(),
                setup.paseo_home.clone(),
                setup.handler.clone(),
                setup.max_buffered,
            )
        };
        let ready = backend.wait_ready().await?;
        let keypair = load_daemon_keypair(&paseo_home)?;
        let handle =
            RelayHandle::start_with_handler(cfg, keypair, ready.server_id, handler, max_buffered)
                .await?;
        *slot = Some(handle);
        Ok(())
    }
}
