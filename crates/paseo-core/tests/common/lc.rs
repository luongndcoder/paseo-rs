//! Lifecycle harness: a manager over a real [`AgentStore`] in a temp dir and a [`MockClient`],
//! the Rust form of the TS `new AgentManager({clients, registry, logger})` tests.
#![allow(dead_code)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use paseo_core::mock::{boxed, Gate, MockClient, MockSession, ScriptedBehavior};
use paseo_core::{
    AgentManager, CreateAgentOptions, ManagedAgentView, ManagerOptions, ProviderDefinition,
    ProviderRegistryUpdate, StorePersister,
};
use paseo_provider_api::{AgentClient, AgentSessionConfig, JsonObject};
use paseo_store::{AgentStore, StoredAgentRecord};
use serde_json::{json, Value};

use super::{collect, EventLog, HookLog, RecordingHooks};
use paseo_core::SubscribeOpts;

pub struct Lc {
    pub dir: tempfile::TempDir,
    pub store: Arc<AgentStore>,
    pub manager: Arc<AgentManager>,
    pub events: EventLog,
    pub hooks: Arc<HookLog>,
    pub client: Arc<MockClient>,
}

pub struct LcOptions {
    pub provider: String,
    pub hooks: Option<RecordingHooks>,
    /// Register provider definitions (`enabled: true`) next to the client.
    pub definitions: bool,
    pub store: bool,
    pub tweak: Box<dyn FnOnce(&mut ManagerOptions)>,
}

impl Default for LcOptions {
    fn default() -> Self {
        Self {
            provider: "codex".into(),
            hooks: None,
            definitions: false,
            store: true,
            tweak: Box::new(|_| {}),
        }
    }
}

/// Deterministic ids: `00000000-0000-4000-8000-0000000000NN`.
pub fn id_n(n: u32) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}

impl Lc {
    pub async fn new() -> Self {
        Self::with(MockClient::new("codex"), LcOptions::default()).await
    }

    pub async fn with_client(client: MockClient) -> Self {
        Self::with(client, LcOptions::default()).await
    }

    pub async fn with(client: MockClient, options: LcOptions) -> Self {
        let client = Arc::new(client);
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(AgentStore::new(dir.path().join("agents")));
        let (hooks, log) = match options.hooks {
            Some(h) => {
                let log = h.log.clone();
                (h, log)
            }
            None => RecordingHooks::new(),
        };
        let mut update = ProviderRegistryUpdate::default();
        update.clients.insert(
            options.provider.clone(),
            client.clone() as Arc<dyn AgentClient>,
        );
        if options.definitions {
            update
                .definitions
                .insert(options.provider.clone(), ProviderDefinition::enabled());
        }
        let counter = Arc::new(AtomicU32::new(0));
        let mut manager_options = ManagerOptions {
            hooks: Arc::new(hooks),
            store: options.store.then(|| store.clone()),
            providers: update,
            id_factory: Arc::new(move || id_n(counter.fetch_add(1, Ordering::SeqCst) + 1)),
            ..ManagerOptions::default()
        };
        (options.tweak)(&mut manager_options);
        let manager = AgentManager::new(manager_options);
        let events = collect(&manager, SubscribeOpts::default().without_replay()).await;
        Self {
            dir,
            store,
            manager,
            events,
            hooks: log,
            client,
        }
    }

    pub fn cwd(&self) -> String {
        self.dir.path().to_string_lossy().into_owned()
    }

    pub fn config(&self) -> AgentSessionConfig {
        AgentSessionConfig::new("codex", self.cwd())
    }

    pub async fn create(&self) -> Arc<ManagedAgentView> {
        self.create_with(self.config(), CreateAgentOptions::default())
            .await
    }

    pub async fn create_titled(&self, title: &str) -> Arc<ManagedAgentView> {
        let mut config = self.config();
        config.title = Some(title.into());
        self.create_with(config, CreateAgentOptions::default())
            .await
    }

    pub async fn create_with(
        &self,
        config: AgentSessionConfig,
        options: CreateAgentOptions,
    ) -> Arc<ManagedAgentView> {
        self.manager
            .create_agent(config, None, options)
            .await
            .expect("create_agent")
    }

    pub async fn record(&self, id: &str) -> Option<StoredAgentRecord> {
        self.store.get(id).await.expect("store get")
    }

    /// Start a foreground run and keep draining it in the background.
    pub async fn start_background_run(
        &self,
        id: &str,
        prompt: &str,
    ) -> tokio::task::JoinHandle<()> {
        let mut stream = self
            .manager
            .stream_agent(
                id,
                paseo_provider_api::PromptInput::from(prompt),
                Default::default(),
            )
            .await
            .expect("stream_agent");
        let task = tokio::spawn(async move { while let Ok(Some(_)) = stream.next().await {} });
        super::settle().await;
        self.manager
            .wait_for_agent_run_start(id, None)
            .await
            .expect("run starts");
        task
    }

    /// Let every event emitted so far reach the collected logs.
    pub async fn sync(&self) {
        self.manager.bus_barrier().await;
        super::settle().await;
    }

    /// The projected rows of a live agent.
    pub async fn rows(&self, id: &str) -> Vec<paseo_timeline::ProjectedEntry> {
        self.manager.get_timeline_rows(id).await.expect("rows")
    }

    /// The projected timeline items of a live agent.
    pub async fn items(&self, id: &str) -> Vec<paseo_agent_types::AgentTimelineItem> {
        self.manager
            .get_timeline_rows(id)
            .await
            .expect("rows")
            .into_iter()
            .map(|row| row.item)
            .collect()
    }

    /// `manager.flush()` then `storage.flush()`.
    pub async fn flush(&self) {
        self.manager.flush().await;
        self.store.flush().await.expect("store flush");
    }

    /// The `StorePersister` over this store.
    pub fn persister(&self) -> StorePersister {
        StorePersister::new(self.store.clone())
    }

    pub async fn load(&self, id: &str) -> Result<Arc<ManagedAgentView>, paseo_core::CoreError> {
        self.manager
            .ensure_agent_loaded(id, paseo_core::LoadOptions::default())
            .await
    }
}

/// A session whose `close` waits on `allowed` and signals `started` (TS `HeldReloadCloseClient`).
pub fn held_close_session(
    started: &Gate,
    allowed: &Gate,
    config: &AgentSessionConfig,
) -> MockSession {
    let (started, allowed) = (started.clone(), allowed.clone());
    MockSession::builder("codex")
        .config(config)
        .auto_completing()
        .behavior(ScriptedBehavior::new().on_close(move |session| {
            let (started, allowed) = (started.clone(), allowed.clone());
            boxed(async move {
                started.open();
                allowed.wait().await;
                session.with_state(|s| s.closed = true);
                Ok(())
            })
        }))
        .build()
}

/// A session whose `close` records itself and fails with `message`.
pub fn failing_close_session(message: &'static str, config: &AgentSessionConfig) -> MockSession {
    MockSession::builder("codex")
        .config(config)
        .auto_completing()
        .behavior(ScriptedBehavior::new().on_close(move |_| {
            boxed(async move { Err(paseo_provider_api::ProviderError::provider(message)) })
        }))
        .build()
}

pub fn obj(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        other => panic!("not an object: {other}"),
    }
}

pub fn handle(provider: &str, session_id: &str, cwd: &str) -> paseo_agent_types::PersistenceHandle {
    paseo_agent_types::PersistenceHandle {
        provider: provider.into(),
        session_id: session_id.into(),
        native_handle: None,
        metadata: Some(obj(json!({"cwd": cwd}))),
    }
}
