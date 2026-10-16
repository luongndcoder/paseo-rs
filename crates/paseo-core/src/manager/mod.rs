//! `AgentManager`: owns the live-agent registry, the lifecycle and foreground lanes, the bus
//! and the shutdown barrier. Per-agent state lives in the actors; this type orchestrates.

pub mod bus;
pub(crate) mod cancel;
pub(crate) mod config_prep;
pub(crate) mod importable;
pub mod lanes;
pub(crate) mod permission;
pub(crate) mod prompt;
pub(crate) mod providers;
pub(crate) mod reads;
pub(crate) mod registry;
pub(crate) mod retired;
pub(crate) mod runs;
pub(crate) mod steer;
pub(crate) mod waits;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use paseo_store::AgentStore;
use tokio::sync::watch;

use crate::agent_actor::{ActorCtx, ActorHandle};
use crate::background::TaskTracker;
use crate::error::{CoreError, Result};
use crate::events::{SubscribeOpts, Subscription};
use crate::hooks::{ManagerHooks, NoopHooks, SnapshotPersister};
use crate::ids::validate_agent_id;
use crate::lifecycle::loader::PendingLoad;
use crate::persistence_sub::StorePersister;
use crate::provider_subagents::ProviderSubagentStore;
use crate::time::{Clock, SystemClock};
use bus::BusTx;
use config_prep::{PaseoToolPolicy, PaseoToolsOptions};
use lanes::LaneMap;
use providers::{ProviderRegistryUpdate, ProviderState};
use registry::Registry;
use retired::RetiredInternal;

/// `INTERRUPT_SESSION_TIMEOUT_MS`.
pub const INTERRUPT_SESSION_TIMEOUT: Duration = Duration::from_millis(2_000);
/// `RELOAD_SESSION_CLOSE_TIMEOUT_MS`.
pub const RELOAD_SESSION_CLOSE_TIMEOUT: Duration = Duration::from_millis(10_000);
/// `AGENT_STREAM_COALESCE_DEFAULT_WINDOW_MS`.
pub const DEFAULT_COALESCE_WINDOW: Duration = Duration::from_millis(60);

type IdFactory = Arc<dyn Fn() -> String + Send + Sync>;

/// `AgentManagerOptions`.
#[derive(Clone)]
pub struct ManagerOptions {
    pub hooks: Arc<dyn ManagerHooks>,
    /// Writes snapshots. Defaults to a [`StorePersister`] over `store` when only a store is set.
    pub persister: Option<Arc<dyn SnapshotPersister>>,
    /// The agent record store (`registry`): archive, detach, labels, loading and unarchive
    /// need it; without one those calls fail like TS without a registry.
    pub store: Option<Arc<AgentStore>>,
    /// Provider clients and definitions (`clients`, `providerDefinitions`).
    pub providers: ProviderRegistryUpdate,
    pub paseo_tools: PaseoToolsOptions,
    pub clock: Arc<dyn Clock>,
    pub id_factory: IdFactory,
    pub coalesce_window: Duration,
    /// `rescueTimeouts.interruptSessionMs`.
    pub interrupt_session_timeout: Duration,
    /// `rescueTimeouts.reloadSessionCloseMs`.
    pub reload_session_close_timeout: Duration,
}

impl Default for ManagerOptions {
    fn default() -> Self {
        Self {
            hooks: Arc::new(NoopHooks),
            persister: None,
            store: None,
            providers: ProviderRegistryUpdate::default(),
            paseo_tools: PaseoToolsOptions::default(),
            clock: Arc::new(SystemClock),
            id_factory: Arc::new(|| uuid::Uuid::new_v4().to_string()),
            coalesce_window: DEFAULT_COALESCE_WINDOW,
            interrupt_session_timeout: INTERRUPT_SESSION_TIMEOUT,
            reload_session_close_timeout: RELOAD_SESSION_CLOSE_TIMEOUT,
        }
    }
}

pub(crate) type CloseResult = Option<std::result::Result<(), String>>;

pub(crate) struct Inner {
    pub registry: Arc<Registry>,
    pub ctx: Arc<ActorCtx>,
    pub options: ManagerOptions,
    pub lifecycle_lanes: LaneMap<String>,
    pub foreground_lanes: LaneMap<String>,
    /// Agent registrations that crossed the shutdown barrier (`agentRegistrationTasks`).
    pub registrations: TaskTracker,
    pub accepting_registrations: AtomicBool,
    pub next_generation: AtomicU64,
    /// Sessions whose close a reload started, by actor generation (`reloadedSessionCloses`):
    /// the memoized close (a retry waits for the same one) and the stale-session marker.
    pub reloaded_sessions: Mutex<HashMap<u64, watch::Receiver<CloseResult>>>,
    pub in_flight_closes: Mutex<HashMap<String, watch::Receiver<CloseResult>>>,
    pub store: Option<Arc<AgentStore>>,
    pub providers: Mutex<ProviderState>,
    pub paseo: Mutex<PaseoToolsOptions>,
    /// `paseoToolPolicies`: the policy each agent was last opened with.
    pub tool_policies: Mutex<HashMap<String, Option<PaseoToolPolicy>>>,
    pub retired: Mutex<RetiredInternal>,
    /// Module-level `pendingAgentInitializations` of `agent-loading.ts`.
    pub pending_loads: Mutex<HashMap<String, PendingLoad>>,
}

/// The manager. Cheap to clone (one `Arc`).
#[derive(Clone)]
pub struct AgentManager {
    pub(crate) inner: Arc<Inner>,
}

impl AgentManager {
    /// Build a manager and start its bus task on the current tokio runtime.
    pub fn new(options: ManagerOptions) -> Arc<Self> {
        let registry = Arc::new(Registry::default());
        let bus = BusTx::spawn();
        let persister = options.persister.clone().or_else(|| {
            options
                .store
                .clone()
                .map(|store| Arc::new(StorePersister::new(store)) as Arc<dyn SnapshotPersister>)
        });
        let ctx = Arc::new(ActorCtx {
            bus,
            hooks: options.hooks.clone(),
            persister,
            clock: options.clock.clone(),
            background: TaskTracker::new(),
            persist_chain: Default::default(),
            subagents: Arc::new(Mutex::new(ProviderSubagentStore::default())),
            registry: registry.clone(),
        });
        let manager = Arc::new(Self {
            inner: Arc::new(Inner {
                registry,
                ctx,
                lifecycle_lanes: LaneMap::new(),
                foreground_lanes: LaneMap::new(),
                registrations: TaskTracker::new(),
                accepting_registrations: AtomicBool::new(true),
                next_generation: AtomicU64::new(1),
                reloaded_sessions: Mutex::new(HashMap::new()),
                in_flight_closes: Mutex::new(HashMap::new()),
                store: options.store.clone(),
                providers: Mutex::new(ProviderState::default()),
                paseo: Mutex::new(options.paseo_tools.clone()),
                tool_policies: Mutex::new(HashMap::new()),
                retired: Mutex::new(RetiredInternal::default()),
                pending_loads: Mutex::new(HashMap::new()),
                options: options.clone(),
            }),
        });
        manager.install_providers(&options.providers);
        manager
    }

    pub(crate) fn paseo_runtime(&self) -> PaseoToolsOptions {
        self.inner
            .paseo
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn edit_paseo(&self, edit: impl FnOnce(&mut PaseoToolsOptions)) {
        edit(&mut self.inner.paseo.lock().unwrap_or_else(|e| e.into_inner()));
    }

    /// `setMcpBaseUrl`.
    pub fn set_mcp_base_url(&self, url: Option<String>) {
        self.edit_paseo(|p| p.mcp_base_url = url);
    }

    /// `setPaseoToolsEnabled`.
    pub fn set_paseo_tools_enabled(&self, enabled: bool) {
        self.edit_paseo(|p| p.enabled = enabled);
    }

    /// `setAppendSystemPrompt`.
    pub fn set_append_system_prompt(&self, prompt: Option<&str>) {
        self.edit_paseo(|p| p.append_system_prompt = prompt.unwrap_or_default().to_owned());
    }

    /// `getPaseoToolPolicy`: the policy the agent was last opened with.
    pub fn paseo_tool_policy(&self, agent_id: &str) -> Option<PaseoToolPolicy> {
        self.inner
            .tool_policies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(agent_id)
            .cloned()
            .flatten()
    }

    pub(crate) fn set_tool_policy(&self, agent_id: &str, policy: Option<PaseoToolPolicy>) {
        self.inner
            .tool_policies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(agent_id.to_owned(), policy);
    }

    pub(crate) fn tool_policy_entry(&self, agent_id: &str) -> Option<Option<PaseoToolPolicy>> {
        self.inner
            .tool_policies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(agent_id)
            .cloned()
    }

    pub(crate) fn restore_tool_policy(
        &self,
        agent_id: &str,
        previous: Option<Option<PaseoToolPolicy>>,
    ) {
        let mut policies = self
            .inner
            .tool_policies
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match previous {
            Some(policy) => {
                policies.insert(agent_id.to_owned(), policy);
            }
            None => {
                policies.remove(agent_id);
            }
        }
    }

    pub(crate) fn store(&self) -> Option<&Arc<AgentStore>> {
        self.inner.store.as_ref()
    }

    /// `requireRegistry`.
    pub(crate) fn require_store(&self) -> Result<&Arc<AgentStore>> {
        self.store()
            .ok_or_else(|| CoreError::msg("Agent storage unavailable"))
    }

    pub(crate) fn bus(&self) -> &BusTx {
        &self.inner.ctx.bus
    }

    pub(crate) fn clock(&self) -> &Arc<dyn Clock> {
        &self.inner.ctx.clock
    }

    pub(crate) fn hooks(&self) -> &Arc<dyn ManagerHooks> {
        &self.inner.ctx.hooks
    }

    /// `subscribe`: replay plus registration is atomic on the bus task.
    pub async fn subscribe(&self, opts: SubscribeOpts) -> Result<Subscription> {
        self.bus().subscribe(opts).await
    }

    pub fn subscription_count(&self) -> usize {
        self.bus().subscription_count()
    }

    /// Wait until every manager event emitted so far has reached subscriber queues.
    pub async fn bus_barrier(&self) {
        self.bus().barrier().await;
    }

    /// `requireAgent`: validates the id, then looks the live actor up.
    pub(crate) fn require_agent(&self, id: &str) -> Result<ActorHandle> {
        let id = validate_agent_id(id, "requireAgent")?;
        self.inner
            .registry
            .get(&id)
            .ok_or_else(|| CoreError::unknown_agent(&id))
    }

    /// `prepareForShutdown`: from now on registrations fail with `ShuttingDown`.
    pub fn prepare_for_shutdown(&self) {
        self.inner
            .accepting_registrations
            .store(false, Ordering::SeqCst);
    }

    pub(crate) fn assert_accepting_registrations(&self) -> Result<()> {
        if self.inner.accepting_registrations.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(CoreError::ShuttingDown)
        }
    }

    /// `assertAgentRegistrationActive`: still accepting, and this exact actor still registered.
    pub(crate) fn assert_registration_active(&self, handle: &ActorHandle) -> Result<()> {
        let current = self.inner.registry.get(&handle.id);
        if self.inner.accepting_registrations.load(Ordering::SeqCst)
            && current.is_some_and(|c| c.generation == handle.generation)
        {
            Ok(())
        } else {
            Err(CoreError::ShuttingDown)
        }
    }

    /// `flush`: drain coalescer buffers, then background persistence.
    pub async fn flush(&self) {
        self.flush_tasks(false).await;
    }

    /// `flushForShutdown`: also joins registrations that crossed the shutdown barrier.
    pub async fn flush_for_shutdown(&self) {
        self.flush_tasks(true).await;
    }

    async fn flush_tasks(&self, include_registrations: bool) {
        for handle in self.inner.registry.all() {
            let _ = handle
                .ask(|actor| {
                    actor.drain_events();
                    actor.flush_all_coalescer();
                })
                .await;
        }
        loop {
            self.inner.ctx.background.wait_idle().await;
            if include_registrations {
                self.inner.registrations.wait_idle().await;
            }
            let busy = !self.inner.ctx.background.is_empty()
                || (include_registrations && !self.inner.registrations.is_empty());
            if !busy {
                return;
            }
        }
    }
}
