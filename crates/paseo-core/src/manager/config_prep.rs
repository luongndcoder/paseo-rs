//! Config preparation (`agent-manager.ts:5236-5420`): normalization, the stored and launch
//! forms of a config, launch context and the plugin `before` hooks.

use std::collections::BTreeMap;
use std::sync::Arc;

use paseo_provider_api::{AgentClient, AgentSessionConfig, FetchCatalogOptions, LaunchContext};
use paseo_store::paths::resolve_path;
use serde_json::{json, Value};

use super::AgentManager;
use crate::config::{strip_internal_paseo_mcp_server, with_runtime_paseo_mcp_server};
use crate::error::{CoreError, Result};
use crate::hooks::PLUGIN_BEFORE_HOOK_TIMEOUT;

/// `ProviderPaseoToolsPolicy`: `enabled !== false` means on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaseoToolPolicy {
    pub enabled: Option<bool>,
    pub disabled_tools: Vec<String>,
}

impl PaseoToolPolicy {
    pub fn disabled() -> Self {
        Self {
            enabled: Some(false),
            disabled_tools: Vec::new(),
        }
    }
}

/// `isPaseoToolPolicyEnabled`.
pub fn is_paseo_tool_policy_enabled(policy: Option<&PaseoToolPolicy>) -> bool {
    policy.is_none_or(|p| p.enabled != Some(false))
}

type ResolvePolicy = Arc<dyn Fn(&str) -> Option<PaseoToolPolicy> + Send + Sync>;

/// Paseo-tools wiring of the manager (`paseoToolsEnabled`, `mcpBaseUrl`, ...). The setters
/// below change it at runtime like the TS ones.
#[derive(Clone)]
pub struct PaseoToolsOptions {
    pub enabled: bool,
    pub mcp_base_url: Option<String>,
    pub mcp_auth_token: Option<String>,
    pub append_system_prompt: String,
    pub resolve_policy: Option<ResolvePolicy>,
}

impl Default for PaseoToolsOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            mcp_base_url: None,
            mcp_auth_token: None,
            append_system_prompt: String::new(),
            resolve_policy: None,
        }
    }
}

/// `AgentResumePurpose`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResumePurpose {
    #[default]
    Interactive,
    History,
}

impl ResumePurpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::History => "history",
        }
    }
}

/// `NormalizeConfigOptions`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NormalizeOpts {
    pub resolve_default_model: bool,
    pub purpose: ResumePurpose,
}

impl Default for NormalizeOpts {
    fn default() -> Self {
        Self {
            resolve_default_model: true,
            purpose: ResumePurpose::Interactive,
        }
    }
}

pub(crate) struct PreparedConfig {
    pub stored: AgentSessionConfig,
    pub launch: AgentSessionConfig,
    pub policy: Option<PaseoToolPolicy>,
}

/// `PluginSessionOpenRequest.reason`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum OpenReason {
    Create,
    Resume,
    Refresh,
    Import,
}

impl OpenReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Resume => "resume",
            Self::Refresh => "refresh",
            Self::Import => "import",
        }
    }
}

pub(crate) struct Opening {
    pub reason: OpenReason,
    pub purpose: ResumePurpose,
    pub workspace_id: Option<String>,
}

/// A session that will run in a directory needs that directory to be there.
async fn assert_usable_working_directory(cwd: &str) -> Result<()> {
    match tokio::fs::metadata(cwd).await {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(CoreError::msg(format!(
            "Working directory is not a directory: {cwd}"
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(CoreError::msg(format!(
            "Working directory does not exist: {cwd}"
        ))),
        Err(error) => Err(CoreError::msg(error.to_string())),
    }
}

impl AgentManager {
    /// `normalizeConfig`.
    pub(crate) async fn normalize_config(
        &self,
        config: AgentSessionConfig,
        opts: NormalizeOpts,
    ) -> Result<AgentSessionConfig> {
        let mut normalized = config;
        if !normalized.cwd.is_empty() {
            normalized.cwd = resolve_path(&normalized.cwd);
            // Reading an archived agent's history runs nothing, and must survive the worktree
            // it ran in being removed with its workspace.
            if opts.purpose != ResumePurpose::History {
                assert_usable_working_directory(&normalized.cwd).await?;
            }
        }
        if let Some(model) = &normalized.model {
            let trimmed = model.trim();
            normalized.model =
                (!trimmed.is_empty() && trimmed != "default").then(|| trimmed.to_owned());
        }
        if opts.resolve_default_model && normalized.model.is_none() {
            if let Some(default) = self.resolve_default_model_id(&normalized).await {
                normalized.model = Some(default);
            }
        }
        self.apply_provider_configuration(normalized)
    }

    /// `resolveDefaultModelId`: the catalog's default model, else its first; errors ignored.
    async fn resolve_default_model_id(&self, config: &AgentSessionConfig) -> Option<String> {
        let client = self.client_for(&config.provider)?;
        let catalog = client
            .fetch_catalog(&FetchCatalogOptions::Workspace {
                cwd: config.cwd.clone(),
                force: false,
            })
            .await
            .ok()?;
        let chosen = catalog
            .models
            .iter()
            .find(|m| m.get("isDefault").and_then(Value::as_bool) == Some(true))
            .or_else(|| catalog.models.first())?;
        chosen.get("id").and_then(Value::as_str).map(str::to_owned)
    }

    /// `applyProviderConfiguration`.
    fn apply_provider_configuration(
        &self,
        config: AgentSessionConfig,
    ) -> Result<AgentSessionConfig> {
        let definition = self.definition_for(&config.provider);
        validate_tool_policy_servers(&config)?;
        let apply = definition.and_then(|d| d.apply_tool_policy);
        if config.tool_policy.is_some() && apply.is_none() {
            return Err(CoreError::msg(format!(
                "Provider '{}' cannot preapprove exact MCP tools for unattended execution",
                config.provider
            )));
        }
        Ok(match apply {
            Some(apply) => apply(&config, config.tool_policy.as_ref()),
            None => config,
        })
    }

    /// `prepareSessionConfig`.
    pub(crate) async fn prepare_session_config(
        &self,
        config: AgentSessionConfig,
        agent_id: &str,
        purpose: ResumePurpose,
    ) -> Result<PreparedConfig> {
        let stored = self
            .normalize_config(
                strip_internal_paseo_mcp_server(config),
                NormalizeOpts {
                    resolve_default_model: true,
                    purpose,
                },
            )
            .await?;
        let runtime = self.paseo_runtime();
        let policy = if runtime.enabled {
            runtime
                .resolve_policy
                .as_ref()
                .and_then(|f| f(&stored.provider))
        } else {
            Some(PaseoToolPolicy::disabled())
        };
        let base_url = (runtime.enabled && is_paseo_tool_policy_enabled(policy.as_ref()))
            .then(|| runtime.mcp_base_url.clone())
            .flatten();
        let mut launch = with_runtime_paseo_mcp_server(
            stored.clone(),
            agent_id,
            base_url.as_deref(),
            runtime.mcp_auth_token.as_deref(),
        );
        launch.daemon_append_system_prompt = None;
        let append = runtime.append_system_prompt.trim();
        if !append.is_empty() {
            launch.daemon_append_system_prompt = Some(append.to_owned());
        }
        Ok(PreparedConfig {
            stored,
            launch,
            policy,
        })
    }

    /// `buildLaunchContext`: plugin `agent.session_open` hook, env, native Paseo tools.
    pub(crate) async fn build_launch_context(
        &self,
        agent_id: &str,
        client: &Arc<dyn AgentClient>,
        cwd: &str,
        policy: Option<&PaseoToolPolicy>,
        env: Option<BTreeMap<String, String>>,
        opening: Opening,
    ) -> Result<LaunchContext> {
        let mut env = env.unwrap_or_default();
        if self.hooks().has_plugin_lifecycle() {
            let request = json!({
                "agentId": agent_id,
                "provider": client.provider(),
                "cwd": cwd,
                "workspaceId": opening.workspace_id,
                "reason": opening.reason.as_str(),
                "purpose": opening.purpose.as_str(),
                "env": env,
            });
            let transformed = self.run_plugin_before_session_open(request).await?;
            env = env_from_value(transformed.get("env"));
        }
        env.insert("PASEO_AGENT_ID".into(), agent_id.to_owned());
        env.insert("PASEO_AGENT_CWD".into(), cwd.to_owned());
        let mut context = LaunchContext {
            agent_id: Some(agent_id.to_owned()),
            env: Some(env),
            paseo_tools: None,
        };
        let enabled = self.paseo_runtime().enabled;
        if enabled && is_paseo_tool_policy_enabled(policy) {
            let native = client
                .capabilities()
                .await
                .map(|c| c.get("supportsNativePaseoTools") == Some(&Value::Bool(true)))
                .unwrap_or(false);
            if native {
                context.paseo_tools = self.hooks().paseo_tool_catalog(agent_id).await;
            }
        }
        Ok(context)
    }

    /// `resolveProviderLaunchConfig`: native tools replace the injected MCP entry.
    pub(crate) fn resolve_provider_launch_config(
        launch_config: AgentSessionConfig,
        context: &LaunchContext,
    ) -> AgentSessionConfig {
        if context.paseo_tools.is_some() {
            strip_internal_paseo_mcp_server(launch_config)
        } else {
            launch_config
        }
    }

    /// The plugin `agent.create` hook: `{config, env}` in, `{config, env}` out. `Err` aborts.
    pub(crate) async fn run_plugin_before_create(&self, input: Value) -> Result<Value> {
        self.run_before("agent.create", self.hooks().plugin_before_create(input))
            .await
    }

    async fn run_plugin_before_session_open(&self, input: Value) -> Result<Value> {
        self.run_before(
            "agent.session_open",
            self.hooks().plugin_before_session_open(input),
        )
        .await
    }

    /// Q16: one request/response per hook with the 30 s timeout; any failure aborts the caller.
    async fn run_before(
        &self,
        name: &str,
        call: impl std::future::Future<Output = std::result::Result<Value, String>>,
    ) -> Result<Value> {
        match tokio::time::timeout(PLUGIN_BEFORE_HOOK_TIMEOUT, call).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(CoreError::Message(message)),
            Err(_) => Err(CoreError::msg(format!(
                "Plugin before {name} hook timed out after {}ms",
                PLUGIN_BEFORE_HOOK_TIMEOUT.as_millis()
            ))),
        }
    }
}

/// `validateToolPolicyServers`: a preapproval must reference a server of the same request.
fn validate_tool_policy_servers(config: &AgentSessionConfig) -> Result<()> {
    let Some(policy) = &config.tool_policy else {
        return Ok(());
    };
    let servers = match &config.mcp_servers {
        Some(Value::Object(servers)) => Some(servers),
        _ => None,
    };
    for grant in policy
        .get("preapproved")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let server = grant.get("server").and_then(Value::as_str).unwrap_or("");
        if !servers.is_some_and(|s| s.contains_key(server)) {
            let tool = grant.get("tool").and_then(Value::as_str).unwrap_or("");
            return Err(CoreError::msg(format!(
                "toolPolicy preapproval '{server}.{tool}' requires MCP server '{server}' in the same agent request"
            )));
        }
    }
    Ok(())
}

pub(crate) fn env_from_value(value: Option<&Value>) -> BTreeMap<String, String> {
    value
        .and_then(Value::as_object)
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn env_to_value(env: &Option<BTreeMap<String, String>>) -> Value {
    match env {
        Some(env) => json!(env),
        None => Value::Null,
    }
}
