//! [`MockClient`]: an `AgentClient` that hands out [`MockSession`]s, the Rust analog of the TS
//! `TestAgentClient` and its subclasses.
//!
//! By default `create_session` and `resume_session` return an auto-completing session built from
//! the config it was given. A test replaces either with a hook (an async closure, so creation
//! can be held on a [`Gate`](super::Gate)), flips availability, edits the catalog, and reads back
//! every call the manager made.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use paseo_agent_types::PersistenceHandle;
use paseo_provider_api::{
    AgentClient, AgentSession, AgentSessionConfig, CreateSessionOptions, FetchCatalogOptions,
    ImportedSession, JsonObject, LaunchContext, ProviderCatalog, ProviderError, Result,
};
use serde_json::{json, Value};

use super::behavior::BoxFut;
use super::MockSession;

/// What `create_session` was called with.
#[derive(Debug, Clone)]
pub struct CreateRequest {
    pub config: AgentSessionConfig,
    pub launch_context: Option<LaunchContext>,
    pub options: Option<CreateSessionOptions>,
}

/// What `resume_session` was called with.
#[derive(Debug, Clone)]
pub struct ResumeRequest {
    pub handle: PersistenceHandle,
    pub overrides: Option<JsonObject>,
    pub launch_context: Option<LaunchContext>,
    pub options: Option<CreateSessionOptions>,
}

type CreateHook = Arc<dyn Fn(CreateRequest) -> BoxFut<Result<MockSession>> + Send + Sync>;
type ResumeHook = Arc<dyn Fn(ResumeRequest) -> BoxFut<Result<MockSession>> + Send + Sync>;
type ListHook = Arc<dyn Fn() -> BoxFut<Result<Vec<Value>>> + Send + Sync>;
type ImportHook = Arc<dyn Fn(Value, Value) -> BoxFut<Result<ImportedSession>> + Send + Sync>;
type NativeArchiveHook = Arc<dyn Fn() -> BoxFut<Result<()>> + Send + Sync>;

struct ClientState {
    capabilities: JsonObject,
    available: std::result::Result<bool, String>,
    availability_calls: u32,
    catalog: std::result::Result<ProviderCatalog, String>,
    catalog_calls: Vec<FetchCatalogOptions>,
    creates: Vec<CreateRequest>,
    resumes: Vec<ResumeRequest>,
    sessions: Vec<MockSession>,
    create_hook: Option<CreateHook>,
    resume_hook: Option<ResumeHook>,
    archived: Vec<PersistenceHandle>,
    unarchived: Vec<PersistenceHandle>,
    unarchive_failure: Option<String>,
    on_archive: Option<NativeArchiveHook>,
    on_unarchive: Option<NativeArchiveHook>,
    commands: Vec<Value>,
    command_configs: Vec<AgentSessionConfig>,
    features: Vec<Value>,
    feature_configs: Vec<AgentSessionConfig>,
    list_importable: Option<ListHook>,
    list_calls: u32,
    import_hook: Option<ImportHook>,
}

/// The catalog of the TS `TestAgentClient`.
fn default_catalog(provider: &str) -> ProviderCatalog {
    let model = |id: &str, label: &str, default: bool| {
        let mut m = json!({"provider": provider, "id": id, "label": label});
        if default {
            m["isDefault"] = json!(true);
        }
        m
    };
    ProviderCatalog {
        models: vec![
            model("gpt-5.4", "GPT-5.4", true),
            model("gpt-5.4-mini", "GPT-5.4 Mini", false),
            model("gpt-5.2-codex", "GPT-5.2 Codex", false),
        ],
        modes: Vec::new(),
        default_mode_id: None,
    }
}

/// An `AgentClient` that hands out [`MockSession`]s.
pub struct MockClient {
    provider: String,
    state: Mutex<ClientState>,
}

impl MockClient {
    pub fn new(provider: &str) -> Self {
        let mut capabilities = JsonObject::new();
        capabilities.insert("supportsSessionListing".into(), Value::Bool(true));
        Self {
            provider: provider.to_owned(),
            state: Mutex::new(ClientState {
                capabilities,
                available: Ok(true),
                availability_calls: 0,
                catalog: Ok(default_catalog(provider)),
                catalog_calls: Vec::new(),
                creates: Vec::new(),
                resumes: Vec::new(),
                sessions: Vec::new(),
                create_hook: None,
                resume_hook: None,
                archived: Vec::new(),
                unarchived: Vec::new(),
                unarchive_failure: None,
                on_archive: None,
                on_unarchive: None,
                commands: Vec::new(),
                command_configs: Vec::new(),
                features: Vec::new(),
                feature_configs: Vec::new(),
                list_importable: None,
                list_calls: 0,
                import_hook: None,
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ClientState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- configuration ----

    pub fn with_capability(self, key: &str, value: bool) -> Self {
        self.state()
            .capabilities
            .insert(key.to_owned(), Value::Bool(value));
        self
    }

    pub fn set_available(&self, available: bool) {
        self.state().available = Ok(available);
    }

    pub fn set_availability_error(&self, message: &str) {
        self.state().available = Err(message.to_owned());
    }

    pub fn set_catalog(&self, catalog: ProviderCatalog) {
        self.state().catalog = Ok(catalog);
    }

    pub fn set_catalog_error(&self, message: &str) {
        self.state().catalog = Err(message.to_owned());
    }

    /// Replace session creation.
    pub fn on_create(
        &self,
        hook: impl Fn(CreateRequest) -> BoxFut<Result<MockSession>> + Send + Sync + 'static,
    ) {
        self.state().create_hook = Some(Arc::new(hook));
    }

    /// Replace session resume.
    pub fn on_resume(
        &self,
        hook: impl Fn(ResumeRequest) -> BoxFut<Result<MockSession>> + Send + Sync + 'static,
    ) {
        self.state().resume_hook = Some(Arc::new(hook));
    }

    pub fn on_list_importable(
        &self,
        hook: impl Fn() -> BoxFut<Result<Vec<Value>>> + Send + Sync + 'static,
    ) {
        self.state().list_importable = Some(Arc::new(hook));
    }

    pub fn on_import(
        &self,
        hook: impl Fn(Value, Value) -> BoxFut<Result<ImportedSession>> + Send + Sync + 'static,
    ) {
        self.state().import_hook = Some(Arc::new(hook));
    }

    /// Runs inside `archive_native_session`; an `Err` fails the call (the handle is recorded
    /// first).
    pub fn on_archive(&self, hook: impl Fn() -> BoxFut<Result<()>> + Send + Sync + 'static) {
        self.state().on_archive = Some(Arc::new(hook));
    }

    /// Runs inside `unarchive_native_session`, before the optional failure; an `Err` fails the
    /// call (the handle is recorded first).
    pub fn on_unarchive(&self, hook: impl Fn() -> BoxFut<Result<()>> + Send + Sync + 'static) {
        self.state().on_unarchive = Some(Arc::new(hook));
    }

    pub fn fail_unarchive(&self, message: &str) {
        self.state().unarchive_failure = Some(message.to_owned());
    }

    pub fn set_commands(&self, commands: Vec<Value>) {
        self.state().commands = commands;
    }

    pub fn set_features(&self, features: Vec<Value>) {
        self.state().features = features;
    }

    // ---- observations ----

    pub fn creates(&self) -> Vec<CreateRequest> {
        self.state().creates.clone()
    }

    pub fn resumes(&self) -> Vec<ResumeRequest> {
        self.state().resumes.clone()
    }

    /// Every session handed out (created or resumed), oldest first.
    pub fn created_sessions(&self) -> Vec<MockSession> {
        self.state().sessions.clone()
    }

    pub fn availability_calls(&self) -> u32 {
        self.state().availability_calls
    }

    pub fn catalog_calls(&self) -> Vec<FetchCatalogOptions> {
        self.state().catalog_calls.clone()
    }

    pub fn archived_handles(&self) -> Vec<PersistenceHandle> {
        self.state().archived.clone()
    }

    pub fn unarchived_handles(&self) -> Vec<PersistenceHandle> {
        self.state().unarchived.clone()
    }

    pub fn list_calls(&self) -> u32 {
        self.state().list_calls
    }

    pub fn command_configs(&self) -> Vec<AgentSessionConfig> {
        self.state().command_configs.clone()
    }

    pub fn feature_configs(&self) -> Vec<AgentSessionConfig> {
        self.state().feature_configs.clone()
    }

    /// The default session: auto-completing, with the client's capabilities (a client that
    /// declares `supportsMcpServers` hands out sessions that honor MCP servers).
    fn default_session(&self, config: &AgentSessionConfig) -> MockSession {
        let mut builder = MockSession::builder(&self.provider)
            .config(config)
            .auto_completing();
        for (key, value) in &self.state().capabilities {
            if let Value::Bool(flag) = value {
                builder = builder.capability(key, *flag);
            }
        }
        builder.build()
    }

    fn remember(&self, session: MockSession) -> Box<dyn AgentSession> {
        self.state().sessions.push(session.clone());
        Box::new(session)
    }
}

#[async_trait]
impl AgentClient for MockClient {
    fn provider(&self) -> &str {
        &self.provider
    }

    async fn capabilities(&self) -> Result<JsonObject> {
        Ok(self.state().capabilities.clone())
    }

    async fn create_session(
        &self,
        config: &AgentSessionConfig,
        launch_context: Option<&LaunchContext>,
        options: Option<&CreateSessionOptions>,
    ) -> Result<Box<dyn AgentSession>> {
        let request = CreateRequest {
            config: config.clone(),
            launch_context: launch_context.cloned(),
            options: options.cloned(),
        };
        let hook = {
            let mut state = self.state();
            state.creates.push(request.clone());
            state.create_hook.clone()
        };
        let session = match hook {
            Some(hook) => hook(request).await?,
            None => self.default_session(config),
        };
        Ok(self.remember(session))
    }

    async fn resume_session(
        &self,
        handle: &PersistenceHandle,
        overrides: Option<&JsonObject>,
        launch_context: Option<&LaunchContext>,
        options: Option<&CreateSessionOptions>,
    ) -> Result<Box<dyn AgentSession>> {
        let request = ResumeRequest {
            handle: handle.clone(),
            overrides: overrides.cloned(),
            launch_context: launch_context.cloned(),
            options: options.cloned(),
        };
        let hook = {
            let mut state = self.state();
            state.resumes.push(request.clone());
            state.resume_hook.clone()
        };
        let session = match hook {
            Some(hook) => hook(request).await?,
            None => {
                let mut config = AgentSessionConfig::new(
                    handle.provider.clone(),
                    overrides
                        .and_then(|o| o.get("cwd"))
                        .and_then(Value::as_str)
                        .unwrap_or("/tmp"),
                );
                config.model = overrides
                    .and_then(|o| o.get("model"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                config.mode_id = overrides
                    .and_then(|o| o.get("modeId"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let session = self.default_session(&config);
                session.set_persistence(handle.clone());
                session
            }
        };
        Ok(self.remember(session))
    }

    async fn fetch_catalog(&self, options: &FetchCatalogOptions) -> Result<ProviderCatalog> {
        let mut state = self.state();
        state.catalog_calls.push(options.clone());
        state.catalog.clone().map_err(ProviderError::provider)
    }

    async fn is_available(&self, _options: Option<&FetchCatalogOptions>) -> Result<bool> {
        let mut state = self.state();
        state.availability_calls += 1;
        state.available.clone().map_err(ProviderError::provider)
    }

    async fn list_commands(&self, config: &AgentSessionConfig) -> Result<Vec<Value>> {
        let mut state = self.state();
        state.command_configs.push(config.clone());
        Ok(state.commands.clone())
    }

    async fn list_features(&self, config: &AgentSessionConfig) -> Result<Vec<Value>> {
        let mut state = self.state();
        state.feature_configs.push(config.clone());
        Ok(state.features.clone())
    }

    async fn list_importable_sessions(&self, _options: Option<&JsonObject>) -> Result<Vec<Value>> {
        let hook = {
            let mut state = self.state();
            state.list_calls += 1;
            state.list_importable.clone()
        };
        match hook {
            Some(hook) => hook().await,
            None => Ok(Vec::new()),
        }
    }

    async fn import_session(&self, input: &Value, context: &Value) -> Result<ImportedSession> {
        let hook = self.state().import_hook.clone();
        match hook {
            Some(hook) => hook(input.clone(), context.clone()).await,
            None => Err(ProviderError::Unsupported("import".into())),
        }
    }

    async fn archive_native_session(&self, persistence: &PersistenceHandle) -> Result<()> {
        let hook = {
            let mut state = self.state();
            state.archived.push(persistence.clone());
            state.on_archive.clone()
        };
        match hook {
            Some(hook) => hook().await,
            None => Ok(()),
        }
    }

    async fn unarchive_native_session(&self, persistence: &PersistenceHandle) -> Result<()> {
        let (hook, failure) = {
            let mut state = self.state();
            state.unarchived.push(persistence.clone());
            (state.on_unarchive.clone(), state.unarchive_failure.clone())
        };
        if let Some(hook) = hook {
            hook().await?;
        }
        match failure {
            Some(message) => Err(ProviderError::provider(message)),
            None => Ok(()),
        }
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
