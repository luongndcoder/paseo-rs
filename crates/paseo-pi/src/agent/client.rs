//! `AgentClient` for Pi and OMP.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use super::catalog::map_model;
use super::session::{commands_for, parse_model_reference, RpcAgentSession};
use super::{
    AgentClient, AgentSession, AgentSlashCommand, Capabilities, Catalog, LaunchContext,
    PersistenceHandle, ResumeOverrides, SessionConfig,
};
use crate::error::{Error, Result};
use crate::flavor::Flavor;
use crate::launch::Options;
use crate::protocol::ThinkingLevel;
use crate::rpc::Timeout;
use crate::session::Session;

/// How a provider process is started. Production spawns the binary; tests inject a replay.
pub trait Launcher: Send + Sync {
    fn start(&self, options: &Options) -> Result<Session>;
}

pub struct ProcessLauncher;

impl Launcher for ProcessLauncher {
    fn start(&self, options: &Options) -> Result<Session> {
        Session::start(options)
    }
}

pub struct RpcAgentClient {
    flavor: Flavor,
    command: Option<Vec<String>>,
    request_timeout: std::time::Duration,
    launcher: Arc<dyn Launcher>,
}

impl RpcAgentClient {
    pub fn new(flavor: Flavor) -> Self {
        Self {
            flavor,
            command: None,
            request_timeout: std::time::Duration::from_secs(60),
            launcher: Arc::new(ProcessLauncher),
        }
    }

    pub fn pi() -> Self {
        Self::new(Flavor::Pi)
    }

    pub fn omp() -> Self {
        Self::new(Flavor::Omp)
    }

    /// Override the binary (and leading args) for every session this client starts.
    pub fn with_command<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.command = Some(argv.into_iter().map(Into::into).collect());
        self
    }

    pub fn with_request_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    pub fn with_launcher(mut self, launcher: Arc<dyn Launcher>) -> Self {
        self.launcher = launcher;
        self
    }

    pub fn flavor(&self) -> Flavor {
        self.flavor
    }

    fn base_options(&self, cwd: &Path, context: &LaunchContext) -> Options {
        let mut options = Options::new(self.flavor)
            .cwd(cwd)
            .request_timeout(self.request_timeout);
        if let Some(command) = &self.command {
            options = options.command(command.clone());
        }
        options.env.extend(context.env.clone());
        options
    }

    fn session_options(&self, config: &SessionConfig, context: &LaunchContext) -> Options {
        let mut options = self.base_options(&config.cwd, context);
        options.model = config.model.clone();
        options.thinking = config
            .thinking_option_id
            .as_deref()
            .and_then(ThinkingLevel::normalize)
            .map(|l| l.0);
        options.no_session = config.internal;
        options.mcp_config_path = config.mcp_config_path.clone();
        options.extension_paths = config.extension_paths.clone();
        options.system_prompt = config.system_prompt.clone();
        options.extra_args = config.extra_args.clone();
        options
    }

    fn wrap(
        &self,
        session: Session,
        mut config: SessionConfig,
        state: crate::protocol::SessionState,
    ) -> Box<dyn AgentSession> {
        if config.thinking_option_id.is_none() {
            config.thinking_option_id = state.thinking_level.as_ref().map(|l| l.0.clone());
        }
        Box::new(RpcAgentSession::new(
            session,
            config,
            state,
            self.capabilities(),
        ))
    }

    fn initial_state(session: &Session) -> Result<crate::protocol::SessionState> {
        session.get_state().inspect_err(|_| session.close())
    }

    /// Pi resumes a session on the model it recorded, or on its default when that model is
    /// gone. Switching afterwards keeps a removed model from blocking the resume.
    fn apply_resume_model(
        session: &Session,
        requested: Option<&str>,
    ) -> Result<crate::protocol::SessionState> {
        let state = session.get_state()?;
        let Some((provider, id)) = requested.and_then(parse_model_reference) else {
            return Ok(state);
        };
        let is_requested = |m: &crate::protocol::Model| m.provider == provider && m.id == id;
        if state.model.as_ref().is_some_and(is_requested) {
            return Ok(state);
        }
        let available = session.get_available_models(Timeout::Default)?;
        if !available.iter().any(is_requested) {
            return Ok(state);
        }
        session.set_model(&provider, &id)?;
        session.get_state()
    }
}

impl AgentClient for RpcAgentClient {
    fn provider(&self) -> &str {
        self.flavor.provider_id()
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            supports_streaming: true,
            supports_session_persistence: true,
            supports_session_listing: false,
            supports_dynamic_modes: true,
            supports_mcp_servers: false,
            supports_reasoning_stream: true,
            supports_tool_invocations: true,
            supports_rewind_conversation: false,
        }
    }

    fn create_session(
        &self,
        config: &SessionConfig,
        context: &LaunchContext,
    ) -> Result<Box<dyn AgentSession>> {
        let session = self
            .launcher
            .start(&self.session_options(config, context))?;
        let state = Self::initial_state(&session)?;
        Ok(self.wrap(session, config.clone(), state))
    }

    fn resume_session(
        &self,
        handle: &PersistenceHandle,
        overrides: &ResumeOverrides,
        context: &LaunchContext,
    ) -> Result<Box<dyn AgentSession>> {
        let session_file = handle.native_handle.clone().ok_or_else(|| {
            Error::Protocol(format!(
                "{} resume requires a native session file handle",
                self.flavor.label()
            ))
        })?;
        let meta = |key: &str| {
            handle
                .metadata
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let config = SessionConfig {
            cwd: overrides
                .cwd
                .clone()
                .or_else(|| meta("cwd").map(PathBuf::from))
                .unwrap_or_default(),
            model: overrides.model.clone().or_else(|| meta("model")),
            thinking_option_id: overrides
                .thinking_option_id
                .clone()
                .or_else(|| meta("thinkingOptionId")),
            mode_id: overrides.mode_id.clone().or_else(|| meta("modeId")),
            system_prompt: overrides.system_prompt.clone(),
            extension_paths: overrides.extension_paths.clone(),
            mcp_config_path: overrides.mcp_config_path.clone(),
            extra_args: overrides.extra_args.clone(),
            ..SessionConfig::default()
        };
        let mut options = self.session_options(&config, context);
        // The model is applied after launch (pi refuses to start on a model it no longer knows).
        options.model = None;
        options.session = Some(session_file);
        let session = self.launcher.start(&options)?;
        let state = Self::apply_resume_model(&session, config.model.as_deref())
            .inspect_err(|_| session.close())?;
        let mut config = config;
        if let Some(model) = &state.model {
            config.model = Some(model.qualified_id());
        }
        Ok(self.wrap(session, config, state))
    }

    fn fetch_catalog(&self, cwd: Option<&Path>) -> Result<Catalog> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let cwd = cwd.map(Path::to_path_buf).unwrap_or(home);
        let session = self
            .launcher
            .start(&self.base_options(&cwd, &LaunchContext::default()))?;
        let result = (|| {
            let models = session.get_available_models(Timeout::Never)?;
            // A fresh session starts on the model the provider resolves from its own settings.
            let configured = session.get_state()?.model;
            let models = models
                .iter()
                .map(|model| {
                    let mut mapped = map_model(model);
                    mapped.is_default = configured
                        .as_ref()
                        .is_some_and(|c| c.provider == model.provider && c.id == model.id);
                    mapped
                })
                .collect();
            Ok(Catalog {
                models,
                modes: Vec::new(),
                default_mode_id: None,
            })
        })();
        session.close();
        result
    }

    fn list_commands(&self, config: &SessionConfig) -> Result<Vec<AgentSlashCommand>> {
        let session = self
            .launcher
            .start(&self.base_options(&config.cwd, &LaunchContext::default()))?;
        let result = commands_for(self.flavor, &session);
        session.close();
        result
    }

    fn is_available(&self) -> bool {
        let command = self
            .command
            .clone()
            .unwrap_or_else(|| self.flavor.default_command());
        let Some(program) = command.first() else {
            return false;
        };
        let path = Path::new(program);
        if path.components().count() > 1 {
            return path.is_file();
        }
        std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(program).is_file())
        })
    }
}
