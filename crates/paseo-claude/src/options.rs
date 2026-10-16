//! [`ClaudeOptions`]: the Rust counterpart of the TS SDK's `Options`. It is split at
//! [`ClaudeOptions::prepare`] into spawn config (argv + env), the `initialize` request and the
//! callback registry. See `PROTOCOL.md` section 1 and 3.1.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::callbacks::{CanUseTool, ElicitationHandler, HookCallback, SdkMcpServer};
use crate::error::{Error, Result};
use crate::process::{SpawnConfig, StderrCallback};
use crate::protocol::argv::ToolsOption;
use crate::protocol::{
    build_argv, AgentDefinition, ArgvOptions, HookCallbackMatcher, HookEvent, InitializeRequest,
    McpServerConfig, PermissionMode, ThinkingConfig,
};

/// SDK identity sent to the CLI (`CLAUDE_AGENT_SDK_VERSION`); the protocol this crate
/// implements was derived from this SDK release.
pub const SDK_VERSION: &str = "0.3.246";
pub const DEFAULT_CONTROL_TIMEOUT: Duration = Duration::from_secs(60);

/// One entry of `mcpServers`.
#[derive(Clone)]
pub enum McpServerSpec {
    /// Passed through `--mcp-config` (stdio / sse / http).
    Config(McpServerConfig),
    /// In-process server: named in `initialize.sdkMcpServers`, served over `mcp_message`.
    Sdk(Arc<dyn SdkMcpServer>),
}

/// `systemPrompt`.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum SystemPrompt {
    /// SDK default: an empty prompt (`systemPrompt: [""]`), not Claude Code's own.
    #[default]
    Empty,
    Text(String),
    /// Several blocks (cache-boundary aware, see the SDK docs).
    Parts(Vec<String>),
    /// Claude Code's own prompt (`{type:'preset', preset:'claude_code'}`): no `systemPrompt`
    /// key is sent; combine with [`ClaudeOptions::append_system_prompt`].
    ClaudeCodePreset,
}

struct HookRegistration {
    event: HookEvent,
    matcher: Option<String>,
    timeout_secs: Option<u64>,
    callback: Arc<dyn HookCallback>,
}

/// Callbacks registered with a session.
#[derive(Default, Clone)]
pub struct Callbacks {
    pub can_use_tool: Option<Arc<dyn CanUseTool>>,
    /// Hook callback id (`hook_<n>`) to callback.
    pub hooks: HashMap<String, Arc<dyn HookCallback>>,
    pub mcp_servers: HashMap<String, Arc<dyn SdkMcpServer>>,
    pub elicitation: Option<Arc<dyn ElicitationHandler>>,
}

/// Result of [`ClaudeOptions::prepare`].
pub struct Prepared {
    pub spawn: SpawnConfig,
    pub init: InitializeRequest,
    pub callbacks: Callbacks,
    pub control_timeout: Duration,
}

pub struct ClaudeOptions {
    executable: String,
    command_prefix: Vec<String>,
    args_suffix: Vec<String>,
    cwd: Option<PathBuf>,
    env_clear: bool,
    env: BTreeMap<String, Option<String>>,
    stderr: Option<StderrCallback>,
    /// Raw CLI flag options; the builder methods below fill it.
    pub argv: ArgvOptions,
    /// Raw `initialize` fields; `hooks` and `sdk_mcp_servers` are computed in `prepare`.
    pub init: InitializeRequest,
    system_prompt: SystemPrompt,
    sdk_servers: BTreeMap<String, Arc<dyn SdkMcpServer>>,
    hooks: Vec<HookRegistration>,
    can_use_tool: Option<Arc<dyn CanUseTool>>,
    elicitation: Option<Arc<dyn ElicitationHandler>>,
    enable_file_checkpointing: bool,
    emit_session_state_events: bool,
    control_timeout: Duration,
}

impl Default for ClaudeOptions {
    fn default() -> Self {
        Self {
            executable: "claude".into(),
            command_prefix: Vec::new(),
            args_suffix: Vec::new(),
            cwd: None,
            env_clear: false,
            env: BTreeMap::new(),
            stderr: None,
            argv: ArgvOptions::default(),
            init: InitializeRequest::default(),
            system_prompt: SystemPrompt::default(),
            sdk_servers: BTreeMap::new(),
            hooks: Vec::new(),
            can_use_tool: None,
            elicitation: None,
            enable_file_checkpointing: false,
            emit_session_state_events: false,
            control_timeout: DEFAULT_CONTROL_TIMEOUT,
        }
    }
}

impl ClaudeOptions {
    pub fn new() -> Self {
        Self::default()
    }

    // ---- process ----

    /// `pathToClaudeCodeExecutable` (default: `claude` from `PATH`).
    pub fn executable(mut self, path: impl Into<String>) -> Self {
        self.executable = path.into();
        self
    }

    /// Replace the command: `argv[0]` is the program, the rest are placed before the CLI flags
    /// (Paseo's custom command mode).
    pub fn command_argv<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut it = argv.into_iter().map(Into::into);
        if let Some(first) = it.next() {
            self.executable = first;
            self.command_prefix = it.collect();
        }
        self
    }

    /// Extra arguments appended after the CLI flags (Paseo's append mode).
    pub fn append_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args_suffix.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Set one environment variable (overlaid on the inherited environment).
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), Some(value.into()));
        self
    }

    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        for (k, v) in vars {
            self.env.insert(k.into(), Some(v.into()));
        }
        self
    }

    pub fn env_remove(mut self, key: impl Into<String>) -> Self {
        self.env.insert(key.into(), None);
        self
    }

    /// Do not inherit the parent environment (TS `Options.env` semantics: replaces, not overlays).
    pub fn env_clear(mut self) -> Self {
        self.env_clear = true;
        self
    }

    pub fn on_stderr(mut self, f: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.stderr = Some(Arc::new(f));
        self
    }

    pub fn control_timeout(mut self, d: Duration) -> Self {
        self.control_timeout = d;
        self
    }

    // ---- query options ----

    pub fn model(mut self, m: impl Into<String>) -> Self {
        self.argv.model = Some(m.into());
        self
    }

    pub fn fallback_model(mut self, m: impl Into<String>) -> Self {
        self.argv.fallback_model = Some(m.into());
        self
    }

    pub fn resume(mut self, session_id: impl Into<String>) -> Self {
        self.argv.resume = Some(session_id.into());
        self
    }

    pub fn fork_session(mut self, yes: bool) -> Self {
        self.argv.fork_session = yes;
        self
    }

    pub fn resume_session_at(mut self, uuid: impl Into<String>) -> Self {
        self.argv.resume_session_at = Some(uuid.into());
        self
    }

    pub fn continue_conversation(mut self, yes: bool) -> Self {
        self.argv.continue_conversation = yes;
        self
    }

    /// `--session-id` for a fresh session with a known id.
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.argv.session_id = Some(id.into());
        self
    }

    pub fn persist_session(mut self, yes: bool) -> Self {
        self.argv.persist_session = Some(yes);
        self
    }

    pub fn permission_mode(mut self, mode: PermissionMode) -> Self {
        self.argv.permission_mode = Some(mode.as_str().to_string());
        self
    }

    pub fn allow_dangerously_skip_permissions(mut self, yes: bool) -> Self {
        self.argv.allow_dangerously_skip_permissions = yes;
        self
    }

    /// `settingSources`, e.g. `["user", "project", "local"]`. An empty list loads nothing.
    pub fn setting_sources<I, S>(mut self, sources: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.argv.setting_sources = Some(sources.into_iter().map(Into::into).collect());
        self
    }

    pub fn allowed_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.argv.allowed_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    pub fn disallowed_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.argv.disallowed_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    pub fn tools(mut self, tools: ToolsOption) -> Self {
        self.argv.tools = Some(tools);
        self
    }

    pub fn additional_directories<I, S>(mut self, dirs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.argv.additional_directories = dirs.into_iter().map(Into::into).collect();
        self
    }

    pub fn include_partial_messages(mut self, yes: bool) -> Self {
        self.argv.include_partial_messages = yes;
        self
    }

    pub fn include_hook_events(mut self, yes: bool) -> Self {
        self.argv.include_hook_events = yes;
        self
    }

    pub fn thinking(mut self, t: ThinkingConfig) -> Self {
        self.argv.thinking = Some(t);
        self
    }

    /// Legacy `maxThinkingTokens`: `0` disables thinking.
    pub fn max_thinking_tokens(mut self, n: u64) -> Self {
        self.argv.thinking = Some(if n == 0 {
            ThinkingConfig::Disabled
        } else {
            ThinkingConfig::Enabled {
                budget_tokens: Some(n),
                display: None,
            }
        });
        self
    }

    /// `low | medium | high | xhigh | max`.
    pub fn effort(mut self, e: impl Into<String>) -> Self {
        self.argv.effort = Some(e.into());
        self
    }

    pub fn max_turns(mut self, n: u64) -> Self {
        self.argv.max_turns = Some(n);
        self
    }

    pub fn max_budget_usd(mut self, n: f64) -> Self {
        self.argv.max_budget_usd = Some(n);
        self
    }

    pub fn strict_mcp_config(mut self, yes: bool) -> Self {
        self.argv.strict_mcp_config = yes;
        self
    }

    /// `--settings` (JSON string or file path).
    pub fn settings(mut self, s: impl Into<String>) -> Self {
        self.argv.settings = Some(s.into());
        self
    }

    /// Generic `--key [value]` (`extraArgs`).
    pub fn extra_arg(mut self, key: impl Into<String>, value: Option<String>) -> Self {
        self.argv.extra_args.insert(key.into(), value);
        self
    }

    pub fn plugin_dir(mut self, path: impl Into<String>, skip_mcp_discovery: bool) -> Self {
        self.argv
            .plugin_dirs
            .push((path.into(), skip_mcp_discovery));
        self
    }

    /// Required for `rewind_files` (`CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING`).
    pub fn enable_file_checkpointing(mut self, yes: bool) -> Self {
        self.enable_file_checkpointing = yes;
        self
    }

    /// `CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS=1`: emit `system/session_state_changed`.
    pub fn emit_session_state_events(mut self, yes: bool) -> Self {
        self.emit_session_state_events = yes;
        self
    }

    // ---- initialize ----

    pub fn system_prompt(mut self, p: SystemPrompt) -> Self {
        self.system_prompt = p;
        self
    }

    pub fn append_system_prompt(mut self, s: impl Into<String>) -> Self {
        self.init.append_system_prompt = Some(s.into());
        self
    }

    pub fn agent(mut self, name: impl Into<String>, def: AgentDefinition) -> Self {
        self.init
            .agents
            .get_or_insert_with(BTreeMap::new)
            .insert(name.into(), def);
        self
    }

    /// Name of a defined agent to run as the main thread (`--agent`).
    pub fn main_agent(mut self, name: impl Into<String>) -> Self {
        self.argv.agent = Some(name.into());
        self
    }

    pub fn forward_subagent_text(mut self, yes: bool) -> Self {
        self.init.forward_subagent_text = Some(yes);
        self
    }

    pub fn per_task_stop_affordance(mut self, yes: bool) -> Self {
        self.init.per_task_stop_affordance = Some(yes);
        self
    }

    pub fn json_schema(mut self, schema: Value) -> Self {
        self.argv.json_schema = Some(schema.clone());
        self.init.json_schema = Some(schema);
        self
    }

    // ---- MCP ----

    pub fn mcp_server(mut self, name: impl Into<String>, spec: McpServerSpec) -> Self {
        let name = name.into();
        match spec {
            McpServerSpec::Config(c) => {
                self.argv.mcp_servers.insert(name, c);
            }
            McpServerSpec::Sdk(s) => {
                self.sdk_servers.insert(name, s);
            }
        }
        self
    }

    pub fn sdk_mcp_server(self, name: impl Into<String>, server: Arc<dyn SdkMcpServer>) -> Self {
        self.mcp_server(name, McpServerSpec::Sdk(server))
    }

    // ---- callbacks ----

    pub fn can_use_tool(mut self, cb: Arc<dyn CanUseTool>) -> Self {
        self.can_use_tool = Some(cb);
        self
    }

    /// Register a hook. Each call is one matcher with one callback; ids are allocated in call
    /// order (`hook_0`, `hook_1`, ...).
    pub fn hook(
        mut self,
        event: HookEvent,
        matcher: Option<&str>,
        timeout_secs: Option<u64>,
        callback: Arc<dyn HookCallback>,
    ) -> Self {
        self.hooks.push(HookRegistration {
            event,
            matcher: matcher.map(str::to_string),
            timeout_secs,
            callback,
        });
        self
    }

    pub fn on_elicitation(mut self, h: Arc<dyn ElicitationHandler>) -> Self {
        self.elicitation = Some(h);
        self
    }

    /// Validate and split into spawn config, `initialize` request and callbacks.
    pub fn prepare(self) -> Result<Prepared> {
        let mut argv = self.argv;
        if self.can_use_tool.is_some() && argv.permission_prompt_tool_name.is_some() {
            return Err(Error::InvalidOptions(
                "can_use_tool and permission_prompt_tool_name are mutually exclusive".into(),
            ));
        }
        if let (Some(m), Some(f)) = (&argv.model, &argv.fallback_model) {
            if m == f {
                return Err(Error::InvalidOptions(
                    "fallback_model must differ from model".into(),
                ));
            }
        }
        argv.can_use_tool = self.can_use_tool.is_some();

        let mut init = self.init;
        match self.system_prompt {
            SystemPrompt::Empty => init.system_prompt = Some(vec![String::new()]),
            SystemPrompt::Text(t) => init.system_prompt = Some(vec![t]),
            SystemPrompt::Parts(p) => init.system_prompt = Some(p),
            SystemPrompt::ClaudeCodePreset => init.system_prompt = None,
        }
        if !self.sdk_servers.is_empty() {
            init.sdk_mcp_servers = Some(self.sdk_servers.keys().cloned().collect());
        }

        let mut callbacks = Callbacks {
            can_use_tool: self.can_use_tool,
            hooks: HashMap::new(),
            mcp_servers: self.sdk_servers.into_iter().collect(),
            elicitation: self.elicitation,
        };
        if !self.hooks.is_empty() {
            let mut registration: BTreeMap<String, Vec<HookCallbackMatcher>> = BTreeMap::new();
            for (n, h) in self.hooks.into_iter().enumerate() {
                let id = format!("hook_{n}");
                registration
                    .entry(h.event.as_str().to_string())
                    .or_default()
                    .push(HookCallbackMatcher {
                        matcher: h.matcher,
                        hook_callback_ids: vec![id.clone()],
                        timeout: h.timeout_secs.map(serde_json::Number::from),
                        extra: Default::default(),
                    });
                callbacks.hooks.insert(id, h.callback);
            }
            init.hooks = Some(registration);
        }

        let mut env = self.env;
        let user_set = |env: &BTreeMap<String, Option<String>>, k: &str| {
            env.contains_key(k) || (!self.env_clear && std::env::var_os(k).is_some())
        };
        let mut default = |k: &str, v: &str| {
            if !user_set(&env, k) {
                env.insert(k.to_string(), Some(v.to_string()));
            }
        };
        default("CLAUDE_CODE_ENTRYPOINT", "sdk-ts");
        default("CLAUDE_AGENT_SDK_VERSION", SDK_VERSION);
        if self.enable_file_checkpointing {
            default("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING", "true");
        }
        if self.emit_session_state_events {
            default("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1");
        }
        // The SDK always drops these so an inherited debugger / node flags cannot leak in.
        env.entry("NODE_OPTIONS".into()).or_insert(None);
        env.entry("DEBUG".into()).or_insert(None);

        Ok(Prepared {
            spawn: SpawnConfig {
                command: self.executable,
                args_prefix: self.command_prefix,
                args: build_argv(&argv),
                args_suffix: self.args_suffix,
                cwd: self.cwd,
                env_clear: self.env_clear,
                env,
                stderr: self.stderr,
            },
            init,
            callbacks,
            control_timeout: self.control_timeout,
        })
    }
}
