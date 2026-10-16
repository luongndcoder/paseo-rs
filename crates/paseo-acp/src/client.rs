//! `AcpClient`: the Rust equivalent of `ACPAgentClient` (create/resume sessions, catalog
//! probe, availability, diagnostics). One client per configured agent.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::{
    CapabilityFlags, Catalog, CreateSessionOptions, Feature, McpServerConfig, PersistenceHandle,
    ResumeOverrides, ResumePurpose, SessionConfig, SlashCommand,
};
use crate::connection::{drive, Connection, Incoming, IncomingHandler, Responder};
use crate::derive::{derive_features, derive_model_definitions, derive_modes};
use crate::error::{Error, Result};
use crate::launch::{
    build_version_probe_command, find_executable, probe_env, resolve_launch, CommandConfig,
    LaunchSpec,
};
use crate::profile::{build_client_capabilities, AgentProfile, CatalogContext};
use crate::protocol::{
    Implementation, InitializeParams, InitializeResponse, ListSessionsParams, LoadSessionParams,
    McpServer, NameValue, NewSessionParams, RemoteMcpServer, RpcError, SessionInfo, StdioMcpServer,
    PROTOCOL_VERSION,
};
use crate::session::{AcpSession, AUTO_ACCEPT_FEATURE_ID};
use crate::transport::{StdioTransportFactory, TransportFactory};
use crate::util::{home_dir, with_timeout, BoxFuture};

const PROBE_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const DIAGNOSTIC_PHASE_TIMEOUT: Duration = Duration::from_secs(20);

pub struct AcpClient {
    profile: Arc<AgentProfile>,
    command: CommandConfig,
    env: BTreeMap<String, String>,
    factory: Arc<dyn TransportFactory>,
}

/// Kills the agent process when dropped while armed. Covers futures cancelled mid-open or
/// mid-session-creation, which would otherwise leak the child (the reader task keeps the
/// connection alive, so dropping the connection alone never closes stdin).
#[derive(Default)]
pub(crate) struct KillSlot(std::sync::Mutex<Option<Arc<dyn crate::transport::ProcessControl>>>);

impl KillSlot {
    fn arm(&self, process: Arc<dyn crate::transport::ProcessControl>) {
        if let Ok(mut g) = self.0.lock() {
            *g = Some(process);
        }
    }

    pub(crate) fn disarm(&self) {
        if let Ok(mut g) = self.0.lock() {
            *g = None;
        }
    }
}

impl Drop for KillSlot {
    fn drop(&mut self) {
        if let Some(p) = self.0.get_mut().ok().and_then(|g| g.take()) {
            p.kill();
        }
    }
}

/// An initialized agent process that no session owns yet.
pub(crate) struct Opened {
    pub connection: Connection,
    pub incoming: mpsc::UnboundedReceiver<Incoming>,
    pub initialize: InitializeResponse,
}

impl AcpClient {
    pub fn new(profile: AgentProfile) -> Self {
        AcpClient {
            profile: Arc::new(profile),
            command: CommandConfig::Default,
            env: BTreeMap::new(),
            factory: Arc::new(StdioTransportFactory::default()),
        }
    }

    pub fn with_command(mut self, command: CommandConfig) -> Self {
        self.command = command;
        self
    }

    /// Env overlay applied to every agent process (`runtimeSettings.env`).
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub fn with_transport_factory(mut self, factory: Arc<dyn TransportFactory>) -> Self {
        self.factory = factory;
        self
    }

    pub fn provider(&self) -> &str {
        &self.profile.provider
    }

    pub fn profile(&self) -> &AgentProfile {
        &self.profile
    }

    pub fn capabilities(&self) -> &CapabilityFlags {
        &self.profile.capabilities
    }

    fn resolve_command(&self) -> Result<(String, Vec<String>)> {
        let launch =
            resolve_launch(&self.command, &self.profile.default_command).ok_or_else(|| {
                Error::Unsupported(format!("{} has no launch command", self.profile.provider))
            })?;
        let default_bin = self
            .profile
            .default_command
            .first()
            .cloned()
            .unwrap_or_default();
        if self.factory_needs_binary() && find_executable(&launch.0, &self.env).is_none() {
            return Err(Error::NotFound(format!(
                "{} command '{}' not found",
                self.profile.provider, default_bin
            )));
        }
        Ok(launch)
    }

    /// Replay/in-memory factories do not need a binary on PATH.
    fn factory_needs_binary(&self) -> bool {
        self.factory.requires_binary()
    }

    pub async fn is_available(&self) -> bool {
        self.resolve_command().is_ok()
    }

    pub(crate) async fn open(
        &self,
        profile: &AgentProfile,
        cwd: Option<&str>,
        launch_env: &BTreeMap<String, String>,
        initialize_timeout: Option<Duration>,
        slot: &KillSlot,
    ) -> Result<Opened> {
        let (command, args) = self.resolve_command()?;
        let mut spec = LaunchSpec::new(command, args)
            .with_env(&self.env)
            .with_env(launch_env);
        if let Some(cwd) = cwd {
            spec = spec.with_cwd(cwd);
        }
        let handle = self.factory.open(&spec)?.start()?;
        let (connection, incoming) = Connection::start(handle);
        slot.arm(connection.process().clone());
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            client_capabilities: build_client_capabilities(
                profile.client_capability_meta.as_ref(),
                profile.client_capabilities.as_ref(),
            ),
            client_info: Implementation {
                name: "Paseo".into(),
                version: "dev".into(),
            },
        };
        let init = async { connection.initialize(&params).await };
        let result = match initialize_timeout {
            Some(t) => {
                with_timeout(
                    t,
                    format!("ACP initialize timed out after {}ms", t.as_millis()),
                    init,
                )
                .await
            }
            None => init.await,
        };
        match result {
            Ok(initialize) => Ok(Opened {
                connection,
                incoming,
                initialize,
            }),
            Err(e) => {
                connection
                    .process()
                    .terminate(Duration::from_secs(2), Duration::from_secs(2))
                    .await;
                slot.disarm();
                Err(e)
            }
        }
    }

    fn effective_capabilities(&self, config: &SessionConfig) -> CapabilityFlags {
        let mut caps = self.profile.capabilities.clone();
        if let Some(v) = config.provider_options.supports_mcp_servers {
            caps.supports_mcp_servers = v;
        }
        caps
    }

    fn session_profile(&self, config: &SessionConfig) -> Arc<AgentProfile> {
        match &config.provider_options.client_capabilities {
            None => self.profile.clone(),
            Some(over) => {
                let mut p = (*self.profile).clone();
                p.client_capabilities = Some(over.clone());
                Arc::new(p)
            }
        }
    }

    fn mcp_servers(&self, caps: &CapabilityFlags, config: &SessionConfig) -> Vec<McpServer> {
        if caps.supports_mcp_servers {
            normalize_mcp_servers(&config.mcp_servers)
        } else {
            Vec::new()
        }
    }

    pub async fn create_session(
        &self,
        config: SessionConfig,
        options: CreateSessionOptions,
    ) -> Result<AcpSession> {
        let caps = self.effective_capabilities(&config);
        let profile = self.session_profile(&config);
        let session = AcpSession::new(
            profile,
            config.clone(),
            caps.clone(),
            options.configured_model_ids.clone(),
            options.agent_id.clone(),
            options.launch_env.clone(),
        );
        let slot = KillSlot::default();
        let result: Result<()> = async {
            let opened = self
                .open(
                    &session.inner.profile,
                    Some(&config.cwd),
                    &options.launch_env,
                    None,
                    &slot,
                )
                .await?;
            session.set_agent_capabilities(opened.initialize.agent_capabilities.clone());
            session.attach(opened.connection.clone(), opened.incoming);
            let response = opened
                .connection
                .new_session(&NewSessionParams {
                    cwd: config.cwd.clone(),
                    mcp_servers: self.mcp_servers(&caps, &config),
                })
                .await?;
            let id = response
                .session_id
                .clone()
                .ok_or_else(|| Error::Protocol("session/new response had no sessionId".into()))?;
            session.set_session_id(id);
            session.apply_session_state(response);
            session.flush_early_updates();
            session.apply_configured_overrides().await
        }
        .await;
        slot.disarm();
        match result {
            Ok(()) => Ok(session),
            Err(e) => {
                session.close().await;
                Err(e)
            }
        }
    }

    pub async fn resume_session(
        &self,
        handle: PersistenceHandle,
        overrides: ResumeOverrides,
        options: CreateSessionOptions,
    ) -> Result<AcpSession> {
        if handle.provider != self.profile.provider {
            return Err(Error::Unsupported(format!(
                "Cannot resume {} handle with {} provider",
                handle.provider, self.profile.provider
            )));
        }
        let stored = stored_config(handle.metadata.as_ref());
        let cwd = overrides
            .cwd
            .clone()
            .or(stored.cwd.clone())
            .ok_or_else(|| {
                Error::Unsupported(format!(
                    "{} resume requires the original working directory",
                    self.profile.provider
                ))
            })?;
        let config = SessionConfig {
            cwd: cwd.clone(),
            model: overrides.model.clone().or(stored.model),
            mode_id: overrides.mode_id.clone().or(stored.mode_id),
            thinking_option_id: overrides
                .thinking_option_id
                .clone()
                .or(stored.thinking_option_id),
            feature_values: overrides
                .feature_values
                .clone()
                .unwrap_or(stored.feature_values),
            title: stored.title,
            mcp_servers: overrides.mcp_servers.clone().unwrap_or_default(),
            provider_options: overrides.provider_options.clone().unwrap_or_default(),
        };
        let caps = self.effective_capabilities(&config);
        let profile = self.session_profile(&config);
        let session = AcpSession::new(
            profile,
            config.clone(),
            caps.clone(),
            options.configured_model_ids.clone(),
            options.agent_id.clone(),
            options.launch_env.clone(),
        );
        let slot = KillSlot::default();
        let result: Result<()> = async {
            // Loading an archived agent's history runs nothing in its working directory, which
            // may have been removed with its worktree; only the process starts from home.
            let process_cwd = if options.resume_purpose == ResumePurpose::History
                && !std::path::Path::new(&cwd).is_dir()
            {
                home_dir().to_string_lossy().into_owned()
            } else {
                cwd.clone()
            };
            let opened = self
                .open(
                    &session.inner.profile,
                    Some(&process_cwd),
                    &options.launch_env,
                    None,
                    &slot,
                )
                .await?;
            let agent_caps = opened
                .initialize
                .agent_capabilities
                .clone()
                .unwrap_or_default();
            session.set_agent_capabilities(Some(agent_caps.clone()));
            session.attach(opened.connection.clone(), opened.incoming);
            session.set_session_id(handle.session_id.clone());
            // Some agents (Devin) reject load/resume without all three params; never drop cwd or mcpServers.
            let params = LoadSessionParams {
                session_id: handle.session_id.clone(),
                cwd: cwd.clone(),
                mcp_servers: self.mcp_servers(&caps, &config),
            };
            let response = if agent_caps.can_load_session() {
                session.begin_history_replay();
                let r = opened.connection.load_session(&params).await;
                session.end_history_replay();
                r?
            } else if agent_caps.can_resume() {
                opened.connection.resume_session(&params).await?
            } else {
                return Err(Error::Unsupported(format!(
                    "{} does not support ACP session resume",
                    self.profile.provider
                )));
            };
            session.apply_session_state(response);
            session.apply_configured_overrides().await?;
            if options.resume_purpose == ResumePurpose::History {
                session.release_history_runtime().await;
            }
            Ok(())
        }
        .await;
        slot.disarm();
        match result {
            Ok(()) => Ok(session),
            Err(e) => {
                session.close().await;
                Err(e)
            }
        }
    }

    /// Discover models and modes through a throwaway probe session in `cwd`
    /// (`homedir()` for the global scope).
    pub async fn fetch_catalog(&self, cwd: Option<&str>) -> Result<Catalog> {
        let cwd = cwd
            .map(str::to_owned)
            .unwrap_or_else(|| home_dir().to_string_lossy().into_owned());
        let probe = Probe::start(self, &cwd).await?;
        let result = self.catalog_from_probe(&probe, &cwd).await;
        probe.close().await;
        result
    }

    async fn catalog_from_probe(&self, probe: &Probe, cwd: &str) -> Result<Catalog> {
        let response = probe
            .connection
            .new_session(&NewSessionParams {
                cwd: cwd.to_string(),
                mcp_servers: Vec::new(),
            })
            .await?;
        *probe.session_id.lock().expect("probe lock") = response.session_id.clone();
        let hooks = &self.profile.hooks;
        let transformed = hooks.transform_session_response(response);
        let options = transformed.config_options.clone().unwrap_or_default();
        let derived = derive_model_definitions(
            &self.profile.provider,
            transformed.models.as_ref(),
            &options,
        );
        let session_id = transformed.session_id.clone().unwrap_or_default();
        let resolved = hooks
            .resolve_catalog_models(CatalogContext {
                connection: &probe.connection,
                session_id: &session_id,
                models: derived.clone(),
                config_options: transformed.config_options.as_deref(),
                provider: &self.profile.provider,
                hooks: hooks.as_ref(),
            })
            .await?;
        let models = hooks.transform_models(resolved.unwrap_or(derived));
        let (modes, _) = derive_modes(
            &self.profile.default_modes,
            transformed.modes.as_ref(),
            &options,
        );
        Ok(Catalog { models, modes })
    }

    /// Features for a config without a session: the local `auto_accept` toggle plus any
    /// agent-backed ones read from a probe session.
    pub async fn list_features(&self, config: &SessionConfig) -> Result<Vec<Feature>> {
        let auto = Feature::Toggle {
            id: AUTO_ACCEPT_FEATURE_ID.into(),
            label: "Auto-accept".into(),
            description: Some("Automatically approve permission requests".into()),
            tooltip: None,
            icon: None,
            value: crate::session::is_auto_accept_enabled(config),
        };
        if self.profile.config_feature_options.is_empty() {
            return Ok(vec![auto]);
        }
        let probe = Probe::start(self, &config.cwd).await?;
        let result = async {
            let response = probe
                .connection
                .new_session(&NewSessionParams {
                    cwd: config.cwd.clone(),
                    mcp_servers: Vec::new(),
                })
                .await?;
            *probe.session_id.lock().expect("probe lock") = response.session_id.clone();
            let transformed = self.profile.hooks.transform_session_response(response);
            let options = transformed.config_options.unwrap_or_default();
            let mut out = vec![auto];
            out.extend(derive_features(
                &options,
                &self.profile.config_feature_options,
            ));
            Ok(out)
        }
        .await;
        probe.close().await;
        result
    }

    /// Page through `session/list` (agents without the capability yield nothing).
    pub async fn list_sessions(
        &self,
        cwd: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<SessionInfo>> {
        let probe = Probe::start(self, &home_dir().to_string_lossy()).await?;
        let result = async {
            if !probe
                .initialize
                .agent_capabilities
                .as_ref()
                .is_some_and(|c| c.can_list())
            {
                return Ok(Vec::new());
            }
            let limit = limit.unwrap_or(usize::MAX);
            let mut out = Vec::new();
            let mut cursor: Option<String> = None;
            loop {
                let page = probe
                    .connection
                    .list_sessions(&ListSessionsParams {
                        cursor: cursor.clone().filter(|c| !c.is_empty()),
                        cwd: cwd.map(str::to_owned).filter(|c| !c.is_empty()),
                    })
                    .await?;
                for s in page.sessions {
                    if out.len() >= limit {
                        break;
                    }
                    out.push(s);
                }
                cursor = page.next_cursor.filter(|c| !c.is_empty());
                if cursor.is_none() || out.len() >= limit {
                    break;
                }
            }
            Ok(out)
        }
        .await;
        probe.close().await;
        result
    }

    /// Slash commands advertised by a fresh probe session.
    pub async fn list_commands(&self, config: &SessionConfig) -> Result<Vec<SlashCommand>> {
        let session = self
            .create_session(config.clone(), CreateSessionOptions::default())
            .await?;
        let commands = session.list_commands().await;
        session.close().await;
        Ok(commands)
    }

    /// Human-readable diagnostic: launcher, version command, spawn/initialize/session probe.
    pub async fn diagnostic(&self) -> String {
        let name = match (&self.profile.label, &self.profile.provider_id) {
            (Some(l), _) => format!("{l} (ACP)"),
            (None, Some(id)) => format!("{id} (ACP)"),
            _ => "Custom ACP".to_string(),
        };
        let mut rows: Vec<(String, String)> = Vec::new();
        let provider_id = self
            .profile
            .provider_id
            .clone()
            .unwrap_or_else(|| self.profile.provider.clone());
        rows.push(("Provider ID".into(), provider_id));
        let launch = resolve_launch(&self.command, &self.profile.default_command);
        let configured: Vec<String> = launch
            .as_ref()
            .map(|(c, a)| {
                std::iter::once(c.clone())
                    .chain(a.iter().cloned())
                    .collect()
            })
            .unwrap_or_default();
        rows.push(("Configured command".into(), configured.join(" ")));
        if let Some(probe_cmd) = build_version_probe_command(&configured) {
            let found = find_executable(&probe_cmd.command, &self.env);
            rows.push((
                "Launcher binary".into(),
                found
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "not found".into()),
            ));
            rows.push((
                "Version command".into(),
                std::iter::once(probe_cmd.command.clone())
                    .chain(probe_cmd.args.clone())
                    .collect::<Vec<_>>()
                    .join(" "),
            ));
        }
        let cwd = home_dir().to_string_lossy().into_owned();
        let started = std::time::Instant::now();
        match with_timeout(
            DIAGNOSTIC_PHASE_TIMEOUT,
            "ACP probe timed out",
            Probe::start(self, &cwd),
        )
        .await
        {
            Err(e) => {
                // A spawn failure is an I/O or lookup error; anything else got past spawn.
                if matches!(e, Error::Io(_) | Error::NotFound(_)) {
                    rows.push(("ACP spawn".into(), format!("error: {e}")));
                } else {
                    rows.push((
                        "ACP spawn".into(),
                        format!("ok ({}ms)", started.elapsed().as_millis()),
                    ));
                    rows.push(("ACP initialize".into(), format!("error: {e}")));
                }
            }
            Ok(probe) => {
                rows.push((
                    "ACP spawn".into(),
                    format!("ok ({}ms)", started.elapsed().as_millis()),
                ));
                rows.push((
                    "ACP initialize".into(),
                    format!("ok ({}ms)", started.elapsed().as_millis()),
                ));
                let session_started = std::time::Instant::now();
                let r = probe
                    .connection
                    .new_session(&NewSessionParams {
                        cwd,
                        mcp_servers: Vec::new(),
                    })
                    .await;
                match r {
                    Ok(resp) => {
                        *probe.session_id.lock().expect("probe lock") = resp.session_id.clone();
                        rows.push((
                            "ACP session/new".into(),
                            format!("ok ({}ms)", session_started.elapsed().as_millis()),
                        ));
                    }
                    Err(e) => rows.push(("ACP session/new".into(), format!("error: {e}"))),
                }
                let stderr = probe.connection.process().stderr();
                if !stderr.trim().is_empty() {
                    rows.push((
                        "ACP stderr".into(),
                        stderr.trim().chars().take(2000).collect(),
                    ));
                }
                let cleanup_started = std::time::Instant::now();
                probe.close().await;
                rows.push((
                    "ACP cleanup".into(),
                    format!("ok ({}ms)", cleanup_started.elapsed().as_millis()),
                ));
            }
        }
        let mut out = name;
        for (k, v) in rows {
            out.push_str(&format!("\n  {k}: {v}"));
        }
        out
    }
}

/// A throwaway process used for catalog/feature/list calls. Permissions are cancelled,
/// fs is served, terminals are refused.
struct Probe {
    connection: Connection,
    initialize: InitializeResponse,
    session_id: std::sync::Mutex<Option<String>>,
    /// Stays armed: dropping a probe (including on caller cancellation) kills its process.
    _kill: KillSlot,
}

struct ProbeHandler;

impl IncomingHandler for ProbeHandler {
    fn on_notification(&self, _method: &str, _params: Value) {}

    fn on_request(
        self: Arc<Self>,
        method: String,
        params: Value,
        responder: Responder,
    ) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            use crate::protocol::client_methods as cm;
            let parse_err = |e: serde_json::Error| RpcError::invalid_params(e.to_string());
            let result: std::result::Result<Value, RpcError> = async {
                match method.as_str() {
                    cm::SESSION_REQUEST_PERMISSION => {
                        Ok(serde_json::json!({ "outcome": { "outcome": "cancelled" } }))
                    }
                    cm::FS_READ_TEXT_FILE => crate::host::read_text_file(
                        &serde_json::from_value(params).map_err(parse_err)?,
                    )
                    .await
                    .map_err(crate::host::host_error),
                    cm::FS_WRITE_TEXT_FILE => crate::host::write_text_file(
                        &serde_json::from_value(params).map_err(parse_err)?,
                    )
                    .await
                    .map_err(crate::host::host_error),
                    cm::TERMINAL_CREATE => Err(RpcError::internal(
                        "ACP model probe does not support terminal execution",
                    )),
                    _ => Err(RpcError::method_not_found(&method)),
                }
            }
            .await;
            responder.send(result);
        })
    }
}

impl Probe {
    async fn start(client: &AcpClient, cwd: &str) -> Result<Probe> {
        let kill = KillSlot::default();
        let opened = client
            .open(&client.profile, Some(cwd), &probe_env(), None, &kill)
            .await?;
        drive(
            opened.connection.clone(),
            opened.incoming,
            Arc::new(ProbeHandler),
        );
        Ok(Probe {
            connection: opened.connection,
            initialize: opened.initialize,
            session_id: Default::default(),
            _kill: kill,
        })
    }

    async fn close(self) {
        let can_close = self
            .initialize
            .agent_capabilities
            .as_ref()
            .is_some_and(|c| c.can_close());
        let id = self.session_id.lock().ok().and_then(|g| g.clone());
        if let (true, Some(id)) = (can_close, id) {
            let _ = with_timeout(
                PROBE_CLOSE_TIMEOUT,
                "ACP probe session/close timed out",
                async { self.connection.close_session(&id).await },
            )
            .await;
        }
        self.connection
            .process()
            .terminate(Duration::from_secs(2), Duration::from_secs(2))
            .await;
    }
}

#[derive(Default)]
struct StoredConfig {
    cwd: Option<String>,
    model: Option<String>,
    mode_id: Option<String>,
    thinking_option_id: Option<String>,
    feature_values: BTreeMap<String, Value>,
    title: Option<String>,
}

fn stored_config(metadata: Option<&Value>) -> StoredConfig {
    let Some(obj) = metadata.and_then(Value::as_object) else {
        return StoredConfig::default();
    };
    let text = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_owned);
    StoredConfig {
        cwd: text("cwd"),
        model: text("model"),
        mode_id: text("modeId"),
        thinking_option_id: text("thinkingOptionId"),
        feature_values: obj
            .get("featureValues")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default(),
        title: text("title"),
    }
}

fn pairs(map: &BTreeMap<String, String>) -> Vec<NameValue> {
    map.iter()
        .map(|(k, v)| NameValue {
            name: k.clone(),
            value: v.clone(),
        })
        .collect()
}

/// `normalizeMcpServers`: stdio entries carry no `type`; http/sse do.
pub fn normalize_mcp_servers(servers: &BTreeMap<String, McpServerConfig>) -> Vec<McpServer> {
    servers
        .iter()
        .map(|(name, cfg)| match cfg {
            McpServerConfig::Stdio { command, args, env } => McpServer::Stdio(StdioMcpServer {
                name: name.clone(),
                command: command.clone(),
                args: args.clone(),
                env: pairs(env),
            }),
            McpServerConfig::Http { url, headers } => McpServer::Remote(RemoteMcpServer {
                kind: "http".into(),
                name: name.clone(),
                url: url.clone(),
                headers: pairs(headers),
            }),
            McpServerConfig::Sse { url, headers } => McpServer::Remote(RemoteMcpServer {
                kind: "sse".into(),
                name: name.clone(),
                url: url.clone(),
                headers: pairs(headers),
            }),
        })
        .collect()
}
