//! Generic ACP agent: any command that speaks ACP over stdio (`GenericACPAgentClient`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::agent::ConfigFeatureOption;
use crate::client::AcpClient;
use crate::profile::{AgentProfile, ProviderHooks, DEFAULT_INITIAL_COMMANDS_WAIT};
use crate::protocol::Extra;

/// Provider name used by every generic ACP agent; the concrete agent is `provider_id`.
pub const GENERIC_PROVIDER: &str = "acp";

pub struct GenericOptions {
    /// Full launch argv, e.g. `["npx", "-y", "some-agent", "--acp"]`.
    pub command: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub provider_id: Option<String>,
    pub label: Option<String>,
    /// ACP agents advertise slash commands with `available_commands_update` after `session/new`,
    /// so the first `list_commands` waits for that batch.
    pub wait_for_initial_commands: bool,
    pub initial_commands_wait_timeout: Duration,
    pub client_capability_meta: Option<Extra>,
    pub config_feature_options: Vec<ConfigFeatureOption>,
    pub hooks: Option<Arc<dyn ProviderHooks>>,
}

impl GenericOptions {
    pub fn new(command: Vec<String>) -> Self {
        GenericOptions {
            command,
            env: BTreeMap::new(),
            provider_id: None,
            label: None,
            wait_for_initial_commands: true,
            initial_commands_wait_timeout: DEFAULT_INITIAL_COMMANDS_WAIT,
            client_capability_meta: None,
            config_feature_options: Vec::new(),
            hooks: None,
        }
    }
}

pub fn generic_profile(options: &GenericOptions) -> AgentProfile {
    let mut profile = AgentProfile::new(GENERIC_PROVIDER, options.command.clone());
    profile.provider_id = options.provider_id.clone();
    profile.label = options.label.clone();
    profile.wait_for_initial_commands = options.wait_for_initial_commands;
    profile.initial_commands_wait_timeout = options.initial_commands_wait_timeout;
    profile.client_capability_meta = options.client_capability_meta.clone();
    profile.config_feature_options = options.config_feature_options.clone();
    if let Some(h) = &options.hooks {
        profile.hooks = h.clone();
    }
    profile
}

pub fn generic_client(options: GenericOptions) -> AcpClient {
    let env = options.env.clone();
    AcpClient::new(generic_profile(&options)).with_env(env)
}
