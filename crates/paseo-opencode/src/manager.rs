//! Server lifecycle: leases on `opencode serve` processes, shared by sessions.
//!
//! Same ownership rules as the TS `OpenCodeServerManager`:
//! - `Current` shares one server; it is stopped when its last lease is released.
//! - `New` retires the current server (stopped once its users release) and starts a fresh one.
//! - `Dedicated(env)` starts a private server with extra environment, stopped on release.

use crate::client::RawClient;
use crate::error::{Error, Result};
use crate::events::{EventHub, HubTiming};
use crate::options::OpenCodeOptions;
use crate::server::ServerProcess;
use crate::transport::Transport;
use async_trait::async_trait;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquire {
    Current,
    New,
    Dedicated(HashMap<String, String>),
}

/// A leased server: where to send requests and where events arrive. Call `release` when done.
pub struct ServerHandle {
    pub url: String,
    pub transport: Arc<dyn Transport>,
    pub events: Arc<EventHub>,
    release: StdMutex<Option<ReleaseHook>>,
}

pub type ReleaseHook = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

impl ServerHandle {
    pub fn new(url: String, transport: Arc<dyn Transport>, events: Arc<EventHub>) -> Self {
        Self {
            url,
            transport,
            events,
            release: StdMutex::new(None),
        }
    }

    /// Runs `hook` on the first `release()`.
    pub fn with_release(self, hook: ReleaseHook) -> Self {
        *self.release.lock().unwrap() = Some(hook);
        self
    }

    /// Releases the lease (idempotent).
    pub async fn release(&self) {
        let hook = self.release.lock().unwrap().take();
        if let Some(hook) = hook {
            hook().await;
        }
    }
}

#[async_trait]
pub trait ServerProvider: Send + Sync + 'static {
    async fn acquire(&self, kind: Acquire) -> Result<ServerHandle>;
    async fn shutdown(&self);
}

struct Generation {
    process: StdMutex<Option<ServerProcess>>,
    url: String,
    transport: Arc<dyn Transport>,
    events: Arc<EventHub>,
    users: AtomicUsize,
    retired: AtomicBool,
}

impl Generation {
    fn is_live(&self) -> bool {
        self.process
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(ServerProcess::is_running)
    }

    async fn stop(&self) {
        self.events.close();
        let process = self.process.lock().unwrap().take();
        if let Some(p) = process {
            p.shutdown().await;
        }
    }
}

#[derive(Default)]
struct State {
    current: Option<Arc<Generation>>,
    retired: Vec<Arc<Generation>>,
}

pub struct OpenCodeServerManager {
    opts: OpenCodeOptions,
    state: Mutex<State>,
}

impl OpenCodeServerManager {
    pub fn new(opts: OpenCodeOptions) -> Arc<Self> {
        Arc::new(Self {
            opts,
            state: Mutex::new(State::default()),
        })
    }

    pub fn options(&self) -> &OpenCodeOptions {
        &self.opts
    }

    async fn start(&self, env: &HashMap<String, String>) -> Result<Arc<Generation>> {
        let process = ServerProcess::spawn(&self.opts, env).await?;
        let transport: Arc<dyn Transport> = Arc::new(process.transport()?);
        let raw = RawClient::new(transport.clone(), None);
        let events = EventHub::spawn(raw, HubTiming::default(), Some(process.exited()));
        Ok(Arc::new(Generation {
            url: process.url.clone(),
            process: StdMutex::new(Some(process)),
            transport,
            events,
            users: AtomicUsize::new(0),
            retired: AtomicBool::new(false),
        }))
    }

    fn lease(self: &Arc<Self>, generation: Arc<Generation>) -> ServerHandle {
        generation.users.fetch_add(1, Ordering::SeqCst);
        let handle = ServerHandle::new(
            generation.url.clone(),
            generation.transport.clone(),
            generation.events.clone(),
        );
        let manager = self.clone();
        handle.with_release(Arc::new(move || {
            let manager = manager.clone();
            let generation = generation.clone();
            Box::pin(async move { manager.release(generation).await })
        }))
    }

    async fn release(&self, generation: Arc<Generation>) {
        // Decrement under the state lock so a concurrent `lease()` (which runs inside
        // `acquire_arc` with the same lock held) cannot interleave with the stop decision.
        {
            let mut state = self.state.lock().await;
            let remaining = generation
                .users
                .fetch_sub(1, Ordering::SeqCst)
                .saturating_sub(1);
            if remaining > 0 {
                return;
            }
            if state
                .current
                .as_ref()
                .is_some_and(|c| Arc::ptr_eq(c, &generation))
            {
                state.current = None;
                generation.retired.store(true, Ordering::SeqCst);
            }
            if !generation.retired.load(Ordering::SeqCst) {
                return;
            }
            state.retired.retain(|g| !Arc::ptr_eq(g, &generation));
        }
        generation.stop().await;
    }

    /// Acquire as an `Arc` receiver (the trait method delegates here).
    pub async fn acquire_arc(self: &Arc<Self>, kind: Acquire) -> Result<ServerHandle> {
        let mut state = self.state.lock().await;
        match kind {
            Acquire::Current => {
                if let Some(current) = state.current.clone().filter(|g| g.is_live()) {
                    return Ok(self.lease(current));
                }
                let generation = self.start(&HashMap::new()).await?;
                self.wait_ready(&generation).await?;
                state.current = Some(generation.clone());
                Ok(self.lease(generation))
            }
            Acquire::New => {
                if let Some(old) = state.current.take() {
                    old.retired.store(true, Ordering::SeqCst);
                    if old.users.load(Ordering::SeqCst) == 0 {
                        old.stop().await;
                    } else {
                        state.retired.push(old);
                    }
                }
                let generation = self.start(&HashMap::new()).await?;
                self.wait_ready(&generation).await?;
                state.current = Some(generation.clone());
                Ok(self.lease(generation))
            }
            Acquire::Dedicated(env) => {
                let generation = self.start(&env).await?;
                generation.retired.store(true, Ordering::SeqCst);
                self.wait_ready(&generation).await?;
                state.retired.push(generation.clone());
                Ok(self.lease(generation))
            }
        }
    }

    async fn wait_ready(&self, generation: &Arc<Generation>) -> Result<()> {
        match generation
            .events
            .ready(crate::options::EVENT_STREAM_READY_TIMEOUT)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                generation.stop().await;
                Err(match e {
                    Error::Timeout(m) => Error::Startup(m),
                    other => other,
                })
            }
        }
    }

    pub async fn shutdown_all(&self) {
        let generations: Vec<_> = {
            let mut state = self.state.lock().await;
            let mut all: Vec<_> = state.current.take().into_iter().collect();
            all.append(&mut state.retired);
            all
        };
        for g in generations {
            g.stop().await;
        }
    }
}

/// `ServerProvider` over an `Arc<OpenCodeServerManager>`.
pub struct ManagedServers(pub Arc<OpenCodeServerManager>);

#[async_trait]
impl ServerProvider for ManagedServers {
    async fn acquire(&self, kind: Acquire) -> Result<ServerHandle> {
        self.0.acquire_arc(kind).await
    }

    async fn shutdown(&self) {
        self.0.shutdown_all().await;
    }
}
