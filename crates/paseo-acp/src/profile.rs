//! Per-agent behavior. The generic core is [`crate::client::AcpClient`]; an [`AgentProfile`]
//! holds what the TS `ACPAgentClientOptions` carry (default command, modes, capability meta,
//! feature options, command wait) and a [`ProviderHooks`] object for logic hooks.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::agent::{
    AgentMode, CapabilityFlags, ClientCapabilitiesOverride, ConfigFeatureOption, ModelDefinition,
    SlashCommand,
};
use crate::connection::Connection;
use crate::error::Result;
use crate::protocol::{ClientCapabilities, Extra, SessionConfigOption, SessionState};
use crate::util::BoxFuture;

pub struct ModeWriterContext<'a> {
    pub connection: &'a Connection,
    pub session_id: &'a str,
    pub requested_mode_id: &'a str,
    pub current_mode_id: Option<&'a str>,
    pub config_options: &'a [SessionConfigOption],
}

#[derive(Debug, Clone, Default)]
pub struct ModeWriteResult {
    pub handled: bool,
    pub current_mode_id: Option<String>,
    pub config_options: Option<Vec<SessionConfigOption>>,
}

#[derive(Debug, Clone, Default)]
pub struct BeforeModeWriteResult {
    pub config_options: Option<Vec<SessionConfigOption>>,
}

/// Handed to [`ProviderHooks::resolve_catalog_models`] with the live probe session.
pub struct CatalogContext<'a> {
    pub connection: &'a Connection,
    pub session_id: &'a str,
    pub models: Vec<ModelDefinition>,
    pub config_options: Option<&'a [SessionConfigOption]>,
    pub provider: &'a str,
    pub hooks: &'a dyn ProviderHooks,
}

/// All hooks default to the generic ACP behavior.
pub trait ProviderHooks: Send + Sync {
    fn transform_session_response(&self, response: SessionState) -> SessionState {
        response
    }

    fn transform_config_options(
        &self,
        options: Vec<SessionConfigOption>,
    ) -> Vec<SessionConfigOption> {
        options
    }

    /// `None` drops the update (`modeIdTransformer` returning null).
    fn transform_mode_id(&self, mode_id: &str) -> Option<String> {
        Some(mode_id.to_string())
    }

    /// Return `Some(commands)` for an extension notification this provider owns.
    fn extension_commands(&self, _method: &str, _params: &Value) -> Option<Vec<SlashCommand>> {
        None
    }

    fn provider_mode_write<'a>(
        &'a self,
        _ctx: &'a ModeWriterContext<'a>,
    ) -> BoxFuture<'a, Result<ModeWriteResult>> {
        Box::pin(async { Ok(ModeWriteResult::default()) })
    }

    fn before_mode_write<'a>(
        &'a self,
        _ctx: &'a ModeWriterContext<'a>,
    ) -> BoxFuture<'a, Result<BeforeModeWriteResult>> {
        Box::pin(async { Ok(BeforeModeWriteResult::default()) })
    }

    /// `Ok(true)` when the provider wrote the thinking option itself.
    fn write_thinking_option<'a>(
        &'a self,
        _connection: &'a Connection,
        _session_id: &'a str,
        _thinking_option_id: &'a str,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }

    /// Provider-owned model discovery. `None` keeps the models derived from `session/new`.
    fn resolve_catalog_models<'a>(
        &'a self,
        _ctx: CatalogContext<'a>,
    ) -> BoxFuture<'a, Result<Option<Vec<ModelDefinition>>>> {
        Box::pin(async { Ok(None) })
    }

    fn transform_models(&self, models: Vec<ModelDefinition>) -> Vec<ModelDefinition> {
        models
    }
}

/// Hooks that change nothing.
pub struct NoHooks;
impl ProviderHooks for NoHooks {}

#[derive(Clone)]
pub struct AgentProfile {
    pub provider: String,
    /// Configured provider id for the generic `acp` provider (diagnostics only).
    pub provider_id: Option<String>,
    /// Display name for diagnostics (`label` in the TS generic client).
    pub label: Option<String>,
    /// Launcher plus default args, e.g. `["copilot", "--acp"]`.
    pub default_command: Vec<String>,
    pub default_modes: Vec<AgentMode>,
    pub capabilities: CapabilityFlags,
    /// Goes into `clientCapabilities._meta` on `initialize`.
    pub client_capability_meta: Option<Extra>,
    pub client_capabilities: Option<ClientCapabilitiesOverride>,
    pub config_feature_options: Vec<ConfigFeatureOption>,
    pub wait_for_initial_commands: bool,
    pub initial_commands_wait_timeout: Duration,
    pub hooks: Arc<dyn ProviderHooks>,
}

pub const DEFAULT_INITIAL_COMMANDS_WAIT: Duration = Duration::from_millis(1500);

impl AgentProfile {
    pub fn new(provider: impl Into<String>, default_command: Vec<String>) -> Self {
        AgentProfile {
            provider: provider.into(),
            provider_id: None,
            label: None,
            default_command,
            default_modes: Vec::new(),
            capabilities: CapabilityFlags::default(),
            client_capability_meta: None,
            client_capabilities: None,
            config_feature_options: Vec::new(),
            wait_for_initial_commands: false,
            initial_commands_wait_timeout: DEFAULT_INITIAL_COMMANDS_WAIT,
            hooks: Arc::new(NoHooks),
        }
    }

    pub fn with_hooks(mut self, hooks: impl ProviderHooks + 'static) -> Self {
        self.hooks = Arc::new(hooks);
        self
    }
}

/// `buildACPClientCapabilities`: base set, partial override, optional `_meta`.
pub fn build_client_capabilities(
    meta: Option<&Extra>,
    over: Option<&ClientCapabilitiesOverride>,
) -> ClientCapabilities {
    let mut caps = ClientCapabilities::default();
    if let Some(o) = over {
        if let Some(v) = o.read_text_file {
            caps.fs.read_text_file = v;
        }
        if let Some(v) = o.write_text_file {
            caps.fs.write_text_file = v;
        }
        if let Some(v) = o.terminal {
            caps.terminal = v;
        }
    }
    if let Some(m) = meta.filter(|m| !m.is_empty()) {
        caps.meta = Some(m.clone());
    }
    caps
}
