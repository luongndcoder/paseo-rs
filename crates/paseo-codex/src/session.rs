//! One Codex thread driven over one `codex app-server` process.
//!
//! The session mirrors the TS adapter's behaviour that is observable on the wire: the handshake
//! order, when `thread/start` vs `thread/resume` is sent, which fields `turn/start` carries, how
//! mode / model / effort changes apply on the *next* turn, and how approvals map to replies. What it
//! does not do is turn notifications into timeline items; it emits them raw (see [`AgentEvent`]).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Map, Value};
use tokio::sync::{broadcast, oneshot};

use crate::agent::*;
use crate::catalog::{self, to_usage};
use crate::error::{Error, Result};
use crate::options::{self, WorkspaceWrite, PROVIDER};
use crate::protocol::notifications::{ServerNotification, TurnStatus};
use crate::protocol::params::{
    self, CollaborationModeEntry, CollaborationModeListResponse, InitializeParams, ModelInfo,
    Skill, SkillsListResponse, ThreadResponse, TurnInterruptParams, TurnStartParams,
    TurnSteerParams, UserInput,
};
use crate::protocol::requests::{
    approval_response, mcp_elicitation_response, tool_user_input_response, ApprovalDecision,
    ElicitationAction, ServerRequest,
};
use crate::protocol::{Notification, RawServerRequest, RequestId};
use crate::rpc::{RpcClient, RpcEvent};
use crate::transport::{ExitInfo, TransportFactory};

const TURN_START_TIMEOUT: Duration = Duration::from_secs(90);
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(2);
const IDENTIFY_TURN_TIMEOUT: Duration = Duration::from_secs(15);
const SETTLE_AFTER_INTERRUPT: Duration = Duration::from_secs(5);
const EVENT_CAPACITY: usize = 4096;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Process-level switches resolved from `codex --version` by the client.
#[derive(Debug, Clone, Copy, Default)]
pub struct LaunchFlags {
    /// Pass `--enable goals` (Codex >= 0.128).
    pub goals: bool,
    /// The `auto-review` mode exists (Codex >= 0.115).
    pub auto_review: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum TurnOutcome {
    Completed(Option<Usage>),
    Failed(String),
    Canceled(String),
    Closed(String),
}

struct ActiveTurn {
    foreground_id: String,
    ident_waiters: Vec<oneshot::Sender<Option<String>>>,
    outcome: Option<oneshot::Sender<TurnOutcome>>,
}

#[derive(Default)]
struct PendingStart {
    cancel_requested: bool,
    done: Vec<oneshot::Sender<()>>,
}

struct StartGuard {
    shared: Arc<Shared>,
    keep_active: bool,
}

impl Drop for StartGuard {
    fn drop(&mut self) {
        let (pending, active) = {
            let mut st = lock(&self.shared.state);
            let active = if self.keep_active {
                None
            } else {
                st.active_turn.take()
            };
            if active.is_some() {
                st.native_turn_id = None;
            }
            (st.pending_start.take(), active)
        };
        if let Some(mut a) = active {
            for w in a.ident_waiters.drain(..) {
                let _ = w.send(None);
            }
            if let Some(tx) = a.outcome.take() {
                let _ = tx.send(TurnOutcome::Canceled("turn start abandoned".into()));
            }
        }
        if let Some(p) = pending {
            for tx in p.done {
                let _ = tx.send(());
            }
        }
    }
}

enum PendingKind {
    Command,
    FileChange,
    Question {
        questions: Vec<params_questions::Question>,
    },
    McpElicitation,
}

mod params_questions {
    pub use crate::protocol::requests::UserInputQuestion as Question;
}

struct PendingPermission {
    request: PermissionRequest,
    kind: PendingKind,
    server_id: RequestId,
}

struct State {
    config: SessionConfig,
    thread_id: Option<String>,
    mode: String,
    has_mode_override: bool,
    service_tier: Option<String>,
    resolved_sandbox: Option<Value>,
    workspace_write: WorkspaceWrite,
    models: Vec<ModelInfo>,
    collaboration_modes: Vec<CollaborationModeEntry>,
    resolved_collaboration_mode: Option<(String, Value, String)>,
    skills: Option<Vec<Skill>>,
    plan_mode: bool,
    connected: bool,
    closed: bool,
    next_turn_ordinal: u64,
    active_turn: Option<ActiveTurn>,
    pending_start: Option<PendingStart>,
    native_turn_id: Option<String>,
    permissions: Vec<PendingPermission>,
    latest_usage: Option<Usage>,
    thread_rollback_available: bool,
}

struct Shared {
    factory: Arc<dyn TransportFactory>,
    flags: LaunchFlags,
    events: broadcast::Sender<AgentEvent>,
    connect_lock: tokio::sync::Mutex<()>,
    rpc: Mutex<Option<RpcClient>>,
    state: Mutex<State>,
}

/// A live Codex conversation. Cheap to clone; clones share the same thread and process.
#[derive(Clone)]
pub struct CodexSession {
    shared: Arc<Shared>,
}

fn read_service_tier(values: &Map<String, Value>) -> Option<String> {
    if let Some(Value::String(t)) = values.get("service_tier") {
        return Some(t.clone());
    }
    // COMPAT(codexFastPreference): saved Fast preferences predate `service_tier`.
    match values.get("fast_mode") {
        Some(v)
            if v.as_bool()
                .unwrap_or(!v.is_null() && v != &json!(0) && v != &json!("")) =>
        {
            Some("fast".into())
        }
        _ => None,
    }
}

impl CodexSession {
    /// Open the session and connect (spawn, handshake, and resume the thread when one is given).
    pub(crate) async fn open(
        factory: Arc<dyn TransportFactory>,
        config: SessionConfig,
        thread_id: Option<String>,
        flags: LaunchFlags,
    ) -> Result<CodexSession> {
        if let Some(mode) = config.mode_id.as_deref() {
            validate_mode(mode)?;
        }
        let mode = config
            .mode_id
            .clone()
            .unwrap_or_else(|| options::DEFAULT_MODE_ID.to_string());
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let state = State {
            service_tier: read_service_tier(&config.feature_values),
            plan_mode: config
                .feature_values
                .get("plan_mode")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            has_mode_override: config.mode_id.is_some(),
            mode,
            config,
            thread_id,
            resolved_sandbox: None,
            workspace_write: WorkspaceWrite::default(),
            models: Vec::new(),
            collaboration_modes: Vec::new(),
            resolved_collaboration_mode: None,
            skills: None,
            connected: false,
            closed: false,
            next_turn_ordinal: 0,
            active_turn: None,
            pending_start: None,
            native_turn_id: None,
            permissions: Vec::new(),
            latest_usage: None,
            thread_rollback_available: true,
        };
        let shared = Arc::new(Shared {
            factory,
            flags,
            events,
            connect_lock: tokio::sync::Mutex::new(()),
            rpc: Mutex::new(None),
            state: Mutex::new(state),
        });
        shared.ensure_connected().await?;
        Ok(CodexSession { shared })
    }

    /// Whether `thread/rollback` exists on the connected server (Codex < 0.156).
    pub fn thread_rollback_available(&self) -> bool {
        lock(&self.shared.state).thread_rollback_available
    }

    /// `thread/read` with turns: the persisted history of this thread.
    pub async fn read_thread(&self, include_turns: bool) -> Result<Value> {
        let rpc = self.shared.ensure_connected().await?;
        let thread_id = lock(&self.shared.state)
            .thread_id
            .clone()
            .ok_or_else(|| Error::Invalid("the session has no thread yet".into()))?;
        rpc.request(
            "thread/read",
            params::to_value(&params::ThreadReadParams {
                thread_id,
                include_turns,
            }),
            None,
        )
        .await
    }

    /// `service_tier` (a tier id or `default`) and `plan_mode` (bool) take effect on the next turn.
    pub async fn set_feature(&self, feature_id: &str, value: Value) -> Result<()> {
        let mut st = lock(&self.shared.state);
        match feature_id {
            "service_tier" => {
                st.service_tier = value.as_str().map(str::to_string);
                st.reconcile_service_tier();
            }
            "plan_mode" => {
                let enabled = value.as_bool().unwrap_or(false);
                st.plan_mode = enabled;
                st.config
                    .feature_values
                    .insert("plan_mode".into(), json!(enabled));
                st.refresh_collaboration_mode();
            }
            other => return Err(Error::Invalid(format!("Unknown Codex feature '{other}'"))),
        }
        Ok(())
    }
}

fn validate_mode(mode: &str) -> Result<()> {
    if options::mode_preset(mode).is_some() {
        return Ok(());
    }
    let valid: Vec<&str> = options::MODE_PRESETS.iter().map(|p| p.id).collect();
    Err(Error::Invalid(format!(
        "Invalid Codex mode \"{mode}\". Valid modes are: {}",
        valid.join(", ")
    )))
}

impl State {
    fn current_service_tiers(&self) -> Vec<params::ServiceTier> {
        let selected = match &self.config.model {
            Some(m) => self
                .models
                .iter()
                .find(|x| &x.id == m || x.model.as_deref() == Some(m.as_str())),
            None => self
                .models
                .iter()
                .find(|x| x.is_default)
                .or_else(|| self.models.first()),
        };
        selected
            .map(|m| m.service_tiers.clone())
            .unwrap_or_default()
    }

    fn reconcile_service_tier(&mut self) {
        let tiers = self.current_service_tiers();
        if self.service_tier.as_deref() == Some("fast") && !tiers.iter().any(|t| t.id == "fast") {
            self.service_tier = tiers
                .iter()
                .find(|t| t.name.eq_ignore_ascii_case("fast"))
                .map(|t| t.id.clone());
        }
        if self.service_tier.as_deref() != Some("default")
            && !self
                .service_tier
                .as_ref()
                .is_some_and(|s| tiers.iter().any(|t| &t.id == s))
        {
            self.service_tier = None;
        }
        self.config.feature_values.remove("fast_mode");
        self.config.feature_values.insert(
            "service_tier".into(),
            json!(self
                .service_tier
                .clone()
                .unwrap_or_else(|| "default".into())),
        );
    }

    /// `(mode, settings, name)` of the collaboration mode that applies now. Cached, and refreshed
    /// only on load / model / effort / plan-mode changes, as the TS adapter does.
    fn refresh_collaboration_mode(&mut self) {
        let Some(m) = options::find_collaboration_mode(&self.collaboration_modes, self.plan_mode)
        else {
            self.resolved_collaboration_mode = None;
            return;
        };
        let mut settings = Map::new();
        if let Some(model) = &m.model {
            settings.insert("model".into(), json!(model));
        }
        if let Some(effort) = &m.reasoning_effort {
            settings.insert("reasoning_effort".into(), json!(effort));
        }
        if let Some(instructions) = options::compose_system_prompt([
            m.developer_instructions.as_deref(),
            self.config.system_prompt.as_deref(),
            self.config.daemon_append_system_prompt.as_deref(),
        ]) {
            settings.insert("developer_instructions".into(), json!(instructions));
        }
        if let Some(model) = &self.config.model {
            settings.insert("model".into(), json!(model));
        }
        if let Some(effort) =
            options::normalize_thinking_option(self.config.thinking_option_id.as_deref())
        {
            settings.insert("reasoning_effort".into(), json!(effort));
        }
        self.resolved_collaboration_mode = Some((
            m.mode.clone().unwrap_or_else(|| "code".into()),
            Value::Object(settings),
            m.name.clone(),
        ));
    }

    fn remember_resolved_sandbox(&mut self, response: &Value) {
        let sandbox = response.get("sandbox").filter(|s| s.is_object()).cloned();
        if let Some(s) = &sandbox {
            if s.get("type").and_then(Value::as_str) == Some("workspaceWrite") {
                if let Some(ww) = WorkspaceWrite::from_value(s) {
                    self.workspace_write = ww;
                }
            }
        }
        self.resolved_sandbox = sandbox;
    }

    fn provider_option(&self, key: &str) -> Option<&Value> {
        self.config
            .provider_options
            .as_ref()
            .and_then(|o| o.get(key))
    }

    fn preset(&self) -> &'static options::ModePreset {
        options::mode_preset(&self.mode)
            .or_else(|| options::mode_preset(options::DEFAULT_MODE_ID))
            .expect("default preset exists")
    }

    fn developer_instructions(&self) -> Option<String> {
        options::compose_system_prompt([
            self.config.system_prompt.as_deref(),
            self.config.daemon_append_system_prompt.as_deref(),
        ])
    }

    fn foreground_tag(&self) -> Option<String> {
        self.active_turn.as_ref().map(|a| a.foreground_id.clone())
    }

    fn build_thread_start(&self, model: &str) -> params::ThreadStartParams {
        let preset = self.preset();
        let override_ = self.has_mode_override;
        params::ThreadStartParams {
            model: Some(model.to_string()),
            cwd: Some(self.config.cwd.clone()),
            approval_policy: (override_ && self.provider_option("approval_policy").is_none())
                .then(|| json!(preset.approval_policy)),
            sandbox: (override_ && self.provider_option("sandbox_mode").is_none())
                .then(|| preset.sandbox.to_string()),
            developer_instructions: self.developer_instructions(),
            config: options::build_inner_config(&self.config),
            ephemeral: None,
            approvals_reviewer: override_.then(|| preset.approvals_reviewer.to_string()),
        }
    }

    fn build_turn_start(&self, input: Vec<UserInput>, run: &RunOptions) -> TurnStartParams {
        let preset = self.preset();
        let override_ = self.has_mode_override;
        let thread_id = self.thread_id.clone().unwrap_or_default();
        let mut p = TurnStartParams {
            thread_id,
            input,
            ..Default::default()
        };
        if override_ && self.provider_option("approval_policy").is_none() {
            p.approval_policy = Some(json!(preset.approval_policy));
        }
        let sandbox_type = self
            .provider_option("sandbox_mode")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| override_.then(|| preset.sandbox.to_string()));
        if let Some(sandbox) = sandbox_type {
            let native = options::sandbox_policy_type(&sandbox);
            let provider_ww = self
                .provider_option("sandbox_workspace_write")
                .and_then(WorkspaceWrite::from_value)
                .unwrap_or_default();
            let ww = provider_ww.over(&self.workspace_write);
            p.sandbox_policy = Some(match &self.resolved_sandbox {
                Some(resolved) if resolved.get("type").and_then(Value::as_str) == Some(native) => {
                    resolved.clone()
                }
                _ => options::sandbox_policy(&sandbox, &ww),
            });
        }
        if override_ {
            p.approvals_reviewer = Some(preset.approvals_reviewer.to_string());
        }
        p.model = self.config.model.clone();
        p.effort = options::normalize_thinking_option(self.config.thinking_option_id.as_deref());
        p.service_tier = Some(
            self.service_tier
                .clone()
                .unwrap_or_else(|| "default".into()),
        );
        if let Some((mode, settings, _)) = &self.resolved_collaboration_mode {
            p.collaboration_mode = Some(json!({ "mode": mode, "settings": settings }));
        }
        p.cwd = Some(self.config.cwd.clone());
        p.output_schema = run.output_schema.clone();
        p.developer_instructions = self.developer_instructions();
        p.config = options::build_inner_config(&self.config);
        p.client_user_message_id = None;
        p
    }
}

impl Shared {
    fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(event);
    }

    fn live_rpc(&self) -> Option<RpcClient> {
        let rpc = lock(&self.rpc).clone()?;
        (!rpc.is_closed() && lock(&self.state).connected).then_some(rpc)
    }

    fn spawn_args(&self) -> Vec<String> {
        let mut args = vec!["app-server".to_string()];
        if self.flags.goals {
            args.push("--enable".into());
            args.push("goals".into());
        }
        args
    }

    async fn ensure_connected(self: &Arc<Self>) -> Result<RpcClient> {
        if let Some(rpc) = self.live_rpc() {
            return Ok(rpc);
        }
        let _guard = self.connect_lock.lock().await;
        if let Some(rpc) = self.live_rpc() {
            return Ok(rpc);
        }
        if lock(&self.state).closed {
            return Err(Error::Closed);
        }
        let transport = self.factory.open(&self.spawn_args())?;
        let (rpc, events) = RpcClient::start(transport)?;
        tokio::spawn(run_event_loop(self.clone(), rpc.clone(), events));
        match self.handshake(&rpc).await {
            Ok(()) => {
                if lock(&self.state).closed {
                    rpc.close().await;
                    return Err(Error::Closed);
                }
                *lock(&self.rpc) = Some(rpc.clone());
                lock(&self.state).connected = true;
                Ok(rpc)
            }
            Err(e) => {
                rpc.close().await;
                Err(e)
            }
        }
    }

    async fn handshake(self: &Arc<Self>, rpc: &RpcClient) -> Result<()> {
        let init = rpc
            .request(
                "initialize",
                params::to_value(&InitializeParams::default()),
                None,
            )
            .await?;
        let user_agent = serde_json::from_value::<params::InitializeResponse>(init)
            .ok()
            .and_then(|r| r.user_agent);
        // An agent we cannot parse is treated as an older server that still has rollback.
        let rollback = user_agent
            .as_deref()
            .map(|ua| !options::version_at_least(ua, options::THREAD_ROLLBACK_REMOVED_VERSION))
            .unwrap_or(true);
        rpc.notify("initialized", json!({}));
        lock(&self.state).thread_rollback_available = rollback;

        let models = catalog::list_models(rpc).await?;
        {
            let mut st = lock(&self.state);
            st.models = models;
            st.reconcile_service_tier();
        }

        let cwd = lock(&self.state).config.cwd.clone();
        if let Ok(v) = rpc
            .request("config/read", json!({ "cwd": cwd }), None)
            .await
        {
            if let Some(ww) = v
                .get("config")
                .and_then(|c| c.get("sandbox_workspace_write"))
                .and_then(WorkspaceWrite::from_value)
            {
                lock(&self.state).workspace_write = ww;
            }
        }
        match rpc.request("collaborationMode/list", json!({}), None).await {
            Ok(v) => {
                let list = serde_json::from_value::<CollaborationModeListResponse>(v)
                    .map(|r| r.data)
                    .unwrap_or_default();
                let mut st = lock(&self.state);
                st.collaboration_modes = list;
                st.refresh_collaboration_mode();
            }
            Err(_) => {
                let mut st = lock(&self.state);
                st.collaboration_modes.clear();
                st.refresh_collaboration_mode();
            }
        }
        self.load_skills(rpc).await;

        if lock(&self.state).thread_id.is_some() {
            self.ensure_thread_loaded(rpc).await?;
            self.apply_default_model_and_thinking(rpc).await?;
        }
        Ok(())
    }

    async fn load_skills(&self, rpc: &RpcClient) {
        let cwd = lock(&self.state).config.cwd.clone();
        let skills = rpc
            .request(
                "skills/list",
                params::to_value(&params::SkillsListParams { cwds: vec![cwd] }),
                None,
            )
            .await
            .ok()
            .and_then(|v| serde_json::from_value::<SkillsListResponse>(v).ok())
            .map(|r| {
                let mut by_name: Vec<Skill> = Vec::new();
                for skill in r.data.into_iter().flat_map(|e| e.skills) {
                    if skill.enabled
                        && !skill.name.is_empty()
                        && !by_name.iter().any(|s| s.name == skill.name)
                    {
                        by_name.push(skill);
                    }
                }
                by_name
            });
        lock(&self.state).skills = skills;
    }

    /// Resume the persisted thread unless the server already has it loaded.
    async fn ensure_thread_loaded(&self, rpc: &RpcClient) -> Result<()> {
        let (thread_id, instructions, config) = {
            let st = lock(&self.state);
            let Some(thread_id) = st.thread_id.clone() else {
                return Ok(());
            };
            (
                thread_id,
                st.developer_instructions(),
                options::build_inner_config(&st.config),
            )
        };
        let loaded = rpc.request("thread/loaded/list", json!({}), None).await?;
        let is_loaded = loaded
            .get("data")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(thread_id.as_str())));
        if is_loaded {
            return Ok(());
        }
        let request = params::to_value(&params::ThreadResumeParams {
            thread_id: thread_id.clone(),
            developer_instructions: instructions,
            config,
        });
        let response = match rpc.request("thread/resume", request.clone(), None).await {
            Ok(r) => r,
            Err(Error::Rpc(e)) if is_archived_error(&e.message, &thread_id) => {
                match rpc
                    .request(
                        "thread/unarchive",
                        params::to_value(&params::ThreadIdParams {
                            thread_id: thread_id.clone(),
                        }),
                        None,
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(Error::Rpc(e))
                        if e.message.contains(&format!(
                            "no archived rollout found for thread id {thread_id}"
                        )) => {}
                    Err(e) => return Err(e),
                }
                rpc.request("thread/resume", request, None).await?
            }
            Err(e) => {
                return Err(Error::Invalid(format!(
                    "Failed to resume Codex thread {thread_id}: {e}"
                )));
            }
        };
        lock(&self.state).remember_resolved_sandbox(&response);
        Ok(())
    }

    async fn resolve_model_and_thinking(
        &self,
        rpc: &RpcClient,
    ) -> Result<(String, Option<String>)> {
        let (mut model, mut thinking) = {
            let st = lock(&self.state);
            (
                st.config.model.clone(),
                options::normalize_thinking_option(st.config.thinking_option_id.as_deref()),
            )
        };
        if model.is_none() || thinking.is_none() {
            let defaults = catalog::read_configured_defaults(rpc).await;
            model = model.or(defaults.model);
            thinking = thinking.or(defaults.thinking_option_id);
        }
        if model.is_none() || thinking.is_none() {
            let models = catalog::list_models(rpc).await?;
            let default = models
                .iter()
                .find(|m| m.is_default)
                .or_else(|| models.first());
            let Some(default) = default else {
                return Err(Error::Invalid(
                    "No models available from Codex app-server".into(),
                ));
            };
            let selected = model
                .as_deref()
                .and_then(|m| models.iter().find(|c| c.id == m))
                .unwrap_or(default);
            if model.is_none() {
                model = Some(selected.id.clone());
            }
            if thinking.is_none() {
                thinking = options::normalize_thinking_option(
                    selected.default_reasoning_effort.as_deref(),
                );
            }
        }
        let model = model.ok_or_else(|| Error::Invalid("Unable to resolve Codex model".into()))?;
        Ok((model, thinking))
    }

    /// A session without a model or effort runs on Codex's configured defaults, whether its thread
    /// is new or resumed after a restart.
    async fn apply_default_model_and_thinking(&self, rpc: &RpcClient) -> Result<String> {
        let (model, thinking) = self.resolve_model_and_thinking(rpc).await?;
        let mut st = lock(&self.state);
        st.config.model = Some(model.clone());
        st.reconcile_service_tier();
        st.config.thinking_option_id = thinking;
        Ok(model)
    }

    async fn ensure_thread(&self, rpc: &RpcClient) -> Result<()> {
        if lock(&self.state).thread_id.is_some() {
            return Ok(());
        }
        let model = self.apply_default_model_and_thinking(rpc).await?;
        let request = params::to_value(&lock(&self.state).build_thread_start(&model));
        let raw = rpc.request("thread/start", request, None).await?;
        let response: ThreadResponse = serde_json::from_value(raw.clone())
            .map_err(|_| Error::Invalid("Codex app-server did not return thread id".into()))?;
        let mut st = lock(&self.state);
        st.remember_resolved_sandbox(&raw);
        let promote = response
            .approvals_reviewer
            .as_deref()
            .is_some_and(|r| r == "auto_review" || r == "guardian_subagent")
            && match &response.approval_policy {
                Some(Value::String(p)) => p == "on-request",
                None => false,
                _ => false,
            }
            && response
                .sandbox
                .as_ref()
                .and_then(|s| s.get("type"))
                .and_then(Value::as_str)
                == Some("workspaceWrite");
        if promote {
            st.mode = options::AUTO_REVIEW_MODE_ID.to_string();
        }
        st.thread_id = Some(response.thread.id);
        Ok(())
    }

    // ---- turns ----

    async fn start_turn_inner(
        self: &Arc<Self>,
        prompt: PromptInput,
        run: RunOptions,
    ) -> Result<(TurnHandle, oneshot::Receiver<TurnOutcome>)> {
        {
            let mut st = lock(&self.state);
            if st.closed {
                return Err(Error::Closed);
            }
            if st.active_turn.is_some() || st.pending_start.is_some() {
                return Err(Error::TurnActive);
            }
            st.pending_start = Some(PendingStart::default());
        }
        // Cancel-safe: if the caller drops this future mid-flight the guard still clears
        // `pending_start` (waking interrupt()) and any half-registered `active_turn`.
        let mut guard = StartGuard {
            shared: self.clone(),
            keep_active: false,
        };
        let result = self.start_turn_body(prompt, run).await;
        guard.keep_active = result.is_ok();
        result
    }

    async fn start_turn_body(
        self: &Arc<Self>,
        prompt: PromptInput,
        run: RunOptions,
    ) -> Result<(TurnHandle, oneshot::Receiver<TurnOutcome>)> {
        let rpc = self.ensure_connected().await?;
        let has_thread = lock(&self.state).thread_id.is_some();
        if has_thread {
            self.ensure_thread_loaded(&rpc).await?;
        } else {
            self.ensure_thread(&rpc).await?;
        }
        let input = build_user_input(&prompt);
        let (outcome_tx, outcome_rx) = oneshot::channel();
        let (handle, request) = {
            let mut st = lock(&self.state);
            let mut request = st.build_turn_start(input, &run);
            request.client_user_message_id = run.client_message_id.clone();
            let foreground_id = format!("codex-turn-{}", st.next_turn_ordinal);
            st.next_turn_ordinal += 1;
            st.native_turn_id = None;
            st.active_turn = Some(ActiveTurn {
                foreground_id: foreground_id.clone(),
                ident_waiters: Vec::new(),
                outcome: Some(outcome_tx),
            });
            if st
                .pending_start
                .as_ref()
                .is_some_and(|p| p.cancel_requested)
            {
                st.active_turn = None;
                return Err(Error::Invalid(
                    "Codex turn start was interrupted before reaching Codex".into(),
                ));
            }
            (
                TurnHandle {
                    turn_id: foreground_id,
                },
                params::to_value(&request),
            )
        };
        if let Err(e) = rpc
            .request("turn/start", request, Some(TURN_START_TIMEOUT))
            .await
        {
            let mut st = lock(&self.state);
            st.active_turn = None;
            return Err(e);
        }
        Ok((handle, outcome_rx))
    }

    // ---- notifications ----

    fn handle_notification(&self, n: Notification) {
        let typed = n.typed();
        let mut events = Vec::new();
        {
            let mut st = lock(&self.state);
            let root = match (typed.thread_id(), st.thread_id.as_deref()) {
                (Some(t), Some(cur)) => t == cur,
                _ => true,
            };
            let tag = if root { st.foreground_tag() } else { None };
            events.push(AgentEvent::Provider {
                turn_id: tag.clone(),
                event: Box::new(ProviderEvent {
                    notification: n,
                    typed: typed.clone(),
                }),
            });
            if let ServerNotification::ServerRequestResolved { request_id, .. } = &typed {
                if let Some(pos) = st
                    .permissions
                    .iter()
                    .position(|p| &p.server_id == request_id)
                {
                    let gone = st.permissions.remove(pos);
                    events.push(AgentEvent::PermissionResolved {
                        request_id: gone.request.id,
                        resolution: None,
                    });
                }
            }
            if root {
                match typed {
                    ServerNotification::ThreadStarted { thread } => {
                        events.push(AgentEvent::ThreadStarted {
                            session_id: thread.id,
                        });
                    }
                    ServerNotification::TurnStarted { turn, .. } => {
                        st.native_turn_id = Some(turn.id.clone());
                        if let Some(active) = st.active_turn.as_mut() {
                            for w in active.ident_waiters.drain(..) {
                                let _ = w.send(Some(turn.id.clone()));
                            }
                        }
                        events.push(AgentEvent::TurnStarted { turn_id: tag });
                    }
                    ServerNotification::ThreadTokenUsageUpdated { token_usage, .. } => {
                        let usage = to_usage(&token_usage);
                        st.latest_usage = Some(usage);
                        events.push(AgentEvent::UsageUpdated {
                            turn_id: tag,
                            usage,
                        });
                    }
                    ServerNotification::TurnCompleted { turn, .. } => {
                        let (outcome, event) = match turn.status {
                            TurnStatus::Failed => {
                                let error = turn
                                    .error
                                    .as_ref()
                                    .map(|e| e.message.clone())
                                    .filter(|m| !m.is_empty())
                                    .unwrap_or_else(|| "Codex turn failed".into());
                                (
                                    TurnOutcome::Failed(error.clone()),
                                    AgentEvent::TurnFailed {
                                        turn_id: tag,
                                        error,
                                    },
                                )
                            }
                            TurnStatus::Interrupted => (
                                TurnOutcome::Canceled("interrupted".into()),
                                AgentEvent::TurnCanceled {
                                    turn_id: tag,
                                    reason: "interrupted".into(),
                                },
                            ),
                            _ => {
                                let usage = st.latest_usage;
                                (
                                    TurnOutcome::Completed(usage),
                                    AgentEvent::TurnCompleted {
                                        turn_id: tag,
                                        usage,
                                    },
                                )
                            }
                        };
                        events.push(event);
                        if let Some(mut active) = st.active_turn.take() {
                            for w in active.ident_waiters.drain(..) {
                                let _ = w.send(None);
                            }
                            if let Some(tx) = active.outcome.take() {
                                let _ = tx.send(outcome);
                            }
                        }
                        st.native_turn_id = None;
                    }
                    _ => {}
                }
            }
        }
        for e in events {
            self.emit(e);
        }
    }

    fn handle_server_request(&self, rpc: &RpcClient, req: RawServerRequest) {
        let typed = req.typed();
        let server_id = req.id.clone();
        let mut st = lock(&self.state);
        let tag = st.foreground_tag();
        let pending = match typed {
            ServerRequest::CommandApproval(p) => {
                let id = format!(
                    "permission-{}",
                    p.approval_id.clone().unwrap_or_else(|| p.item_id.clone())
                );
                let mut metadata = Map::new();
                metadata.insert("itemId".into(), json!(p.item_id));
                metadata.insert("threadId".into(), json!(p.thread_id));
                metadata.insert("turnId".into(), json!(p.turn_id));
                metadata.insert("availableDecisions".into(), json!(p.available_decisions));
                metadata.insert("commandActions".into(), json!(p.command_actions));
                PendingPermission {
                    request: PermissionRequest {
                        id,
                        provider: PROVIDER.into(),
                        name: "CodexBash".into(),
                        kind: PermissionKind::Tool,
                        title: Some(match &p.command {
                            Some(c) => format!("Run command: {c}"),
                            None => "Run command".into(),
                        }),
                        description: p.reason.clone(),
                        input: json!({ "command": p.command, "cwd": p.cwd }),
                        metadata,
                    },
                    kind: PendingKind::Command,
                    server_id,
                }
            }
            ServerRequest::FileChangeApproval(p) => {
                let mut metadata = Map::new();
                metadata.insert("itemId".into(), json!(p.item_id));
                metadata.insert("threadId".into(), json!(p.thread_id));
                metadata.insert("turnId".into(), json!(p.turn_id));
                PendingPermission {
                    request: PermissionRequest {
                        id: format!("permission-{}", p.item_id),
                        provider: PROVIDER.into(),
                        name: "CodexFileChange".into(),
                        kind: PermissionKind::Tool,
                        title: Some("Apply file changes".into()),
                        description: p.reason.clone(),
                        input: json!({ "reason": p.reason }),
                        metadata,
                    },
                    kind: PendingKind::FileChange,
                    server_id,
                }
            }
            ServerRequest::ToolUserInput(p) => {
                let questions: Vec<Value> = p
                    .questions
                    .iter()
                    .map(|q| {
                        json!({
                            "id": q.id, "header": q.header, "question": q.question,
                            "isOther": q.is_other, "isSecret": q.is_secret,
                            "options": q.options.as_ref().map(|o| o.iter().map(|o| json!({"label": o.label, "description": o.description})).collect::<Vec<_>>()),
                        })
                    })
                    .collect();
                let mut metadata = Map::new();
                metadata.insert("itemId".into(), json!(p.item_id));
                metadata.insert("threadId".into(), json!(p.thread_id));
                metadata.insert("turnId".into(), json!(p.turn_id));
                metadata.insert("questions".into(), json!(questions));
                PendingPermission {
                    request: PermissionRequest {
                        id: format!("permission-{}", p.item_id),
                        provider: PROVIDER.into(),
                        name: "request_user_input".into(),
                        kind: PermissionKind::Question,
                        title: Some("Question".into()),
                        description: None,
                        input: json!({ "questions": questions }),
                        metadata,
                    },
                    kind: PendingKind::Question {
                        questions: p.questions,
                    },
                    server_id,
                }
            }
            ServerRequest::McpElicitation(p) => {
                let required = p
                    .requested_schema
                    .as_ref()
                    .and_then(|s| s.get("required"))
                    .and_then(Value::as_array)
                    .is_some_and(|r| !r.is_empty());
                if p.mode == "url" || required {
                    drop(st);
                    rpc.respond(
                        &req.id,
                        mcp_elicitation_response(ElicitationAction::Decline, None),
                    );
                    return;
                }
                let mut metadata = Map::new();
                metadata.insert("threadId".into(), json!(p.thread_id));
                metadata.insert("turnId".into(), json!(p.turn_id));
                metadata.insert("serverName".into(), json!(p.server_name));
                metadata.insert("elicitationId".into(), json!(p.elicitation_id));
                PendingPermission {
                    request: PermissionRequest {
                        id: format!("permission-mcp-{}", server_id),
                        provider: PROVIDER.into(),
                        name: "CodexMcpElicitation".into(),
                        kind: PermissionKind::Tool,
                        title: Some(format!("MCP approval: {}", p.server_name)),
                        description: Some(p.message.clone()),
                        input: json!({ "mode": p.mode, "requestedSchema": p.requested_schema, "url": p.url }),
                        metadata,
                    },
                    kind: PendingKind::McpElicitation,
                    server_id,
                }
            }
            ServerRequest::Unknown { .. } => {
                drop(st);
                rpc.respond(&req.id, json!({}));
                return;
            }
            ServerRequest::Invalid { error, .. } => {
                drop(st);
                rpc.respond_error(&req.id, &error);
                return;
            }
        };
        let request = pending.request.clone();
        st.permissions.push(pending);
        drop(st);
        self.emit(AgentEvent::PermissionRequested {
            turn_id: tag,
            request,
        });
    }

    fn handle_closed(&self, rpc: &RpcClient, exit: ExitInfo) {
        let reason = exit.describe();
        let (was_current, intentional, active, tag) = {
            let mut slot = lock(&self.rpc);
            let was_current = slot.as_ref().is_some_and(|c| c.same(rpc));
            if was_current {
                *slot = None;
            }
            let mut st = lock(&self.state);
            if was_current {
                st.connected = false;
            }
            let tag = st.foreground_tag();
            let active = st.active_turn.take();
            st.native_turn_id = None;
            st.permissions.clear();
            (was_current, st.closed, active, tag)
        };
        if !was_current && !intentional {
            // a connection that never finished its handshake: the caller sees the error
            if let Some(mut a) = active {
                for w in a.ident_waiters.drain(..) {
                    let _ = w.send(None);
                }
                if let Some(tx) = a.outcome.take() {
                    let _ = tx.send(TurnOutcome::Closed(reason));
                }
            }
            return;
        }
        if let Some(mut a) = active {
            for w in a.ident_waiters.drain(..) {
                let _ = w.send(None);
            }
            let error = format!("Codex app-server exited: {reason}");
            if let Some(tx) = a.outcome.take() {
                let _ = tx.send(if intentional {
                    TurnOutcome::Closed(reason.clone())
                } else {
                    TurnOutcome::Failed(error.clone())
                });
            }
            if !intentional {
                self.emit(AgentEvent::TurnFailed {
                    turn_id: tag,
                    error,
                });
            }
        }
        self.emit(AgentEvent::Closed { reason });
    }
}

async fn run_event_loop(
    shared: Arc<Shared>,
    rpc: RpcClient,
    mut events: tokio::sync::mpsc::UnboundedReceiver<RpcEvent>,
) {
    while let Some(event) = events.recv().await {
        match event {
            RpcEvent::Notification(n) => shared.handle_notification(n),
            RpcEvent::ServerRequest(r) => shared.handle_server_request(&rpc, r),
            RpcEvent::InvalidLine { .. } => {}
            RpcEvent::Closed(exit) => {
                shared.handle_closed(&rpc, exit);
                break;
            }
        }
    }
}

fn is_archived_error(message: &str, thread_id: &str) -> bool {
    message == format!("session {thread_id} is archived. Run `codex unarchive {thread_id}` to unarchive it first.")
}

// ---- prompt -> input ----

fn image_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

/// Codex reads images from disk, so an inline image is written to a content-addressed temp file.
fn materialize_image(data: &str, mime: &str) -> std::io::Result<std::path::PathBuf> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid base64: {e}"),
            )
        })?;
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    let dir = std::env::temp_dir().join("paseo-attachments");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "{:016x}-{}.{}",
        hasher.finish(),
        bytes.len(),
        image_extension(mime)
    ));
    if !path.exists() {
        std::fs::write(&path, &bytes)?;
    }
    Ok(path)
}

pub fn build_user_input(prompt: &PromptInput) -> Vec<UserInput> {
    prompt
        .0
        .iter()
        .map(|block| match block {
            PromptBlock::Text(t) => UserInput::text(t.clone()),
            PromptBlock::Skill { name, path } => UserInput::Skill {
                name: name.clone(),
                path: path.clone(),
            },
            PromptBlock::Image { data, mime_type } => match materialize_image(data, mime_type) {
                Ok(path) => UserInput::LocalImage {
                    path: path.to_string_lossy().into_owned(),
                },
                Err(e) => UserInput::text(format!(
                    "User attached image (failed to write temp file): {e}"
                )),
            },
        })
        .collect()
}

// ---- permission replies ----

fn decision_for(response: &PermissionResponse) -> ApprovalDecision {
    match response {
        PermissionResponse::Allow {
            selected_action_id, ..
        } => {
            if selected_action_id.as_deref() == Some("acceptForSession") {
                ApprovalDecision::AcceptForSession
            } else {
                ApprovalDecision::Accept
            }
        }
        PermissionResponse::Deny {
            interrupt: true, ..
        } => ApprovalDecision::Cancel,
        PermissionResponse::Deny { .. } => ApprovalDecision::Decline,
    }
}

fn question_answers(
    questions: &[params_questions::Question],
    response: &PermissionResponse,
) -> Value {
    let PermissionResponse::Allow { updated_input, .. } = response else {
        return json!({ "answers": {} });
    };
    let given = updated_input
        .as_ref()
        .and_then(|v| v.get("answers"))
        .and_then(Value::as_object);
    let mut answers = Map::new();
    for q in questions {
        if let Some(v) = given.and_then(|g| g.get(&q.id)) {
            answers.insert(q.id.clone(), v.clone());
        } else if let Some(first) = q
            .options
            .as_ref()
            .and_then(|o| o.first())
            .map(|o| o.label.trim())
            .filter(|l| !l.is_empty())
        {
            answers.insert(q.id.clone(), json!([first]));
        }
    }
    tool_user_input_response(&answers)
}

impl PendingPermission {
    fn reply(&self, response: &PermissionResponse) -> Value {
        match &self.kind {
            PendingKind::Command | PendingKind::FileChange => {
                approval_response(decision_for(response))
            }
            PendingKind::McpElicitation => match response {
                PermissionResponse::Allow { .. } => {
                    mcp_elicitation_response(ElicitationAction::Accept, Some(json!({})))
                }
                PermissionResponse::Deny {
                    interrupt: true, ..
                } => mcp_elicitation_response(ElicitationAction::Cancel, None),
                PermissionResponse::Deny { .. } => {
                    mcp_elicitation_response(ElicitationAction::Decline, None)
                }
            },
            PendingKind::Question { questions } => question_answers(questions, response),
        }
    }

    /// Reply used when the session closes with the request still open.
    fn cancel_reply(&self) -> Value {
        match &self.kind {
            PendingKind::Command | PendingKind::FileChange => {
                approval_response(ApprovalDecision::Cancel)
            }
            PendingKind::McpElicitation => {
                mcp_elicitation_response(ElicitationAction::Cancel, None)
            }
            PendingKind::Question { .. } => json!({ "answers": {} }),
        }
    }
}

// ---- trait impl ----

impl AgentSession for CodexSession {
    fn id(&self) -> Option<String> {
        lock(&self.shared.state).thread_id.clone()
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.shared.events.subscribe()
    }

    async fn start_turn(&self, prompt: PromptInput, options: RunOptions) -> Result<TurnHandle> {
        self.shared
            .start_turn_inner(prompt, options)
            .await
            .map(|(h, _)| h)
    }

    async fn run(&self, prompt: PromptInput, options: RunOptions) -> Result<RunResult> {
        let mut rx = self.shared.events.subscribe();
        let (handle, outcome) = self.shared.start_turn_inner(prompt, options).await?;
        let mut events = Vec::new();
        let mut final_text = String::new();
        let mut outcome = outcome;
        let mut resolved: Option<TurnOutcome> = None;
        let note = |event: &AgentEvent, final_text: &mut String| {
            if let AgentEvent::Provider { event: p, .. } = event {
                if let ServerNotification::ItemCompleted { item, .. } = &p.typed {
                    if let Some(crate::protocol::items::KnownItem::AgentMessage { text, .. }) =
                        &item.known
                    {
                        *final_text = text.clone();
                    }
                }
            }
        };
        loop {
            tokio::select! {
                got = rx.recv() => match got {
                    Ok(event) => {
                        note(&event, &mut final_text);
                        let terminal = event.is_turn_terminal();
                        events.push(event);
                        if terminal {
                            break;
                        }
                    }
                    // A lagged receiver may have lost the terminal event; the outcome
                    // channel below is the authority, so keep waiting on both.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                got = &mut outcome, if resolved.is_none() => {
                    resolved = Some(got.unwrap_or(TurnOutcome::Closed("session dropped".into())));
                    while let Ok(event) = rx.try_recv() {
                        note(&event, &mut final_text);
                        events.push(event);
                    }
                    break;
                }
            }
        }
        let outcome = match resolved {
            Some(o) => o,
            None => outcome
                .await
                .unwrap_or(TurnOutcome::Closed("session dropped".into())),
        };
        let session_id = self.id().unwrap_or_default();
        let _ = handle;
        match outcome {
            TurnOutcome::Completed(usage) => Ok(RunResult {
                session_id,
                final_text,
                usage,
                canceled: false,
                events,
            }),
            TurnOutcome::Canceled(_) => Ok(RunResult {
                session_id,
                final_text,
                usage: None,
                canceled: true,
                events,
            }),
            TurnOutcome::Failed(error) => Err(Error::TurnFailed(error)),
            TurnOutcome::Closed(reason) => Err(Error::Exited(reason)),
        }
    }

    async fn steer_active_turn(
        &self,
        prompt: PromptInput,
        expected_turn_id: &str,
        client_message_id: Option<String>,
    ) -> Result<SteerResult> {
        let Some(rpc) = self.shared.live_rpc() else {
            return Ok(SteerResult::Unavailable);
        };
        let (thread_id, native) = {
            let st = lock(&self.shared.state);
            let foreground_matches = st
                .active_turn
                .as_ref()
                .is_some_and(|a| a.foreground_id == expected_turn_id);
            match (
                st.thread_id.clone(),
                st.native_turn_id.clone(),
                foreground_matches,
            ) {
                (Some(t), Some(n), true) => (t, n),
                _ => return Ok(SteerResult::Unavailable),
            }
        };
        let request = TurnSteerParams {
            thread_id,
            expected_turn_id: native.clone(),
            input: build_user_input(&prompt),
            client_user_message_id: client_message_id,
        };
        match rpc
            .request(
                "turn/steer",
                params::to_value(&request),
                Some(TURN_START_TIMEOUT),
            )
            .await
        {
            Ok(v) => {
                let acked = v.get("turnId").and_then(Value::as_str).or_else(|| {
                    v.get("turn")
                        .and_then(|t| t.get("id"))
                        .and_then(Value::as_str)
                });
                if acked == Some(native.as_str()) {
                    Ok(SteerResult::Accepted)
                } else {
                    Err(Error::Protocol(
                        "Codex returned an invalid steer acknowledgement".into(),
                    ))
                }
            }
            Err(e) if e.is_definitive_steer_rejection() => Ok(SteerResult::Unavailable),
            Err(e) => Err(e),
        }
    }

    async fn interrupt(&self) -> Result<()> {
        let pending_done = {
            let mut st = lock(&self.shared.state);
            st.pending_start.as_mut().map(|p| {
                p.cancel_requested = true;
                let (tx, rx) = oneshot::channel();
                p.done.push(tx);
                rx
            })
        };
        if let Some(rx) = pending_done {
            let _ = rx.await;
        }
        let rpc = self.shared.live_rpc();
        let (thread_id, native, ident) = {
            let mut st = lock(&self.shared.state);
            let waiter = if st.native_turn_id.is_none() {
                st.active_turn.as_mut().map(|a| {
                    let (tx, rx) = oneshot::channel();
                    a.ident_waiters.push(tx);
                    rx
                })
            } else {
                None
            };
            (st.thread_id.clone(), st.native_turn_id.clone(), waiter)
        };
        let (Some(rpc), Some(thread_id)) = (rpc, thread_id) else {
            return if native.is_none() && ident.is_none() {
                Ok(())
            } else {
                Err(Error::Invalid(
                    "Cannot interrupt Codex before the active thread is initialized".into(),
                ))
            };
        };
        // turn/start is accepted before Codex publishes the native turn id; hold the interrupt
        // until that ordered notification arrives.
        let turn_id = match (native, ident) {
            (Some(n), _) => n,
            (None, Some(rx)) => match tokio::time::timeout(IDENTIFY_TURN_TIMEOUT, rx).await {
                Ok(Ok(Some(id))) => id,
                Ok(_) => return Ok(()), // the turn ended before it was identified
                Err(_) => {
                    return Err(Error::Invalid(
                        "Cannot interrupt Codex before turn/started identifies the active turn"
                            .into(),
                    ))
                }
            },
            (None, None) => return Ok(()),
        };
        let request = params::to_value(&TurnInterruptParams { thread_id, turn_id });
        match rpc
            .request("turn/interrupt", request, Some(INTERRUPT_TIMEOUT))
            .await
        {
            Ok(_) => {}
            Err(e) if e.is_already_idle_interrupt() => {
                let (active, tag) = {
                    let mut st = lock(&self.shared.state);
                    let tag = st.foreground_tag();
                    st.native_turn_id = None;
                    (st.active_turn.take(), tag)
                };
                if let Some(mut a) = active {
                    for w in a.ident_waiters.drain(..) {
                        let _ = w.send(None);
                    }
                    if let Some(tx) = a.outcome.take() {
                        let _ = tx.send(TurnOutcome::Canceled("interrupted".into()));
                    }
                    self.shared.emit(AgentEvent::TurnCanceled {
                        turn_id: tag,
                        reason: "interrupted".into(),
                    });
                }
                return Ok(());
            }
            Err(e) => return Err(e),
        }
        // best effort: let the interrupted turn settle so a following prompt does not race it
        let deadline = tokio::time::Instant::now() + SETTLE_AFTER_INTERRUPT;
        while lock(&self.shared.state).active_turn.is_some()
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok(())
    }

    fn pending_permissions(&self) -> Vec<PermissionRequest> {
        lock(&self.shared.state)
            .permissions
            .iter()
            .map(|p| p.request.clone())
            .collect()
    }

    async fn respond_to_permission(
        &self,
        request_id: &str,
        response: PermissionResponse,
    ) -> Result<()> {
        // `live_rpc` takes the state lock itself, so resolve it before holding the guard.
        let rpc = self.shared.live_rpc();
        let pending = {
            let mut st = lock(&self.shared.state);
            let pos = st
                .permissions
                .iter()
                .position(|p| p.request.id == request_id)
                .ok_or_else(|| Error::UnknownPermission(request_id.to_string()))?;
            st.permissions.remove(pos)
        };
        let rpc = rpc.ok_or(Error::Closed)?;
        rpc.respond(&pending.server_id, pending.reply(&response));
        self.shared.emit(AgentEvent::PermissionResolved {
            request_id: request_id.to_string(),
            resolution: Some(response),
        });
        Ok(())
    }

    async fn available_modes(&self) -> Result<Vec<AgentMode>> {
        Ok(options::catalog_modes(self.shared.flags.auto_review))
    }

    async fn current_mode(&self) -> Result<Option<String>> {
        Ok(Some(lock(&self.shared.state).mode.clone()))
    }

    async fn set_mode(&self, mode_id: &str) -> Result<Option<ProviderNotice>> {
        validate_mode(mode_id)?;
        let notice = {
            let mut st = lock(&self.shared.state);
            st.mode = mode_id.to_string();
            st.has_mode_override = true;
            st.config.mode_id = Some(mode_id.to_string());
            st.active_turn.is_some()
        };
        self.shared.emit(AgentEvent::ModeChanged {
            current_mode_id: Some(mode_id.to_string()),
        });
        Ok(notice.then(|| ProviderNotice {
            message: "Mode applies from the next turn.".into(),
        }))
    }

    async fn set_model(&self, model_id: Option<String>) -> Result<()> {
        {
            let mut st = lock(&self.shared.state);
            st.config.model = model_id.clone();
            st.reconcile_service_tier();
            st.refresh_collaboration_mode();
        }
        self.shared
            .emit(AgentEvent::ModelChanged { model: model_id });
        Ok(())
    }

    async fn set_thinking_option(
        &self,
        thinking_option_id: Option<String>,
    ) -> Result<Option<ProviderNotice>> {
        let mut st = lock(&self.shared.state);
        st.config.thinking_option_id =
            options::normalize_thinking_option(thinking_option_id.as_deref());
        st.refresh_collaboration_mode();
        Ok(st.active_turn.is_some().then(|| ProviderNotice {
            message: "Thinking option applies from the next turn.".into(),
        }))
    }

    async fn runtime_info(&self) -> Result<RuntimeInfo> {
        let rpc = self.shared.ensure_connected().await?;
        if lock(&self.shared.state).thread_id.is_none() {
            self.shared.ensure_thread(&rpc).await?;
        }
        let st = lock(&self.shared.state);
        let mut extra = Map::new();
        if let Some((_, _, name)) = &st.resolved_collaboration_mode {
            extra.insert("collaborationMode".into(), json!(name));
        }
        Ok(RuntimeInfo {
            provider: PROVIDER.into(),
            session_id: st.thread_id.clone(),
            model: st.config.model.clone(),
            thinking_option_id: options::normalize_thinking_option(
                st.config.thinking_option_id.as_deref(),
            ),
            mode_id: Some(st.mode.clone()),
            extra,
        })
    }

    async fn list_commands(&self) -> Result<Vec<SlashCommand>> {
        let rpc = self.shared.ensure_connected().await?;
        self.shared.load_skills(&rpc).await;
        let st = lock(&self.shared.state);
        let mut commands = vec![SlashCommand {
            name: "compact".into(),
            description: "Summarize conversation to prevent hitting the context limit".into(),
            argument_hint: String::new(),
            kind: SlashCommandKind::Command,
        }];
        for skill in st.skills.iter().flatten() {
            commands.push(SlashCommand {
                name: skill.name.clone(),
                description: if skill.description.is_empty() {
                    "Skill".into()
                } else {
                    skill.description.clone()
                },
                argument_hint: String::new(),
                kind: SlashCommandKind::Skill,
            });
        }
        commands.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(commands)
    }

    fn describe_persistence(&self) -> Option<PersistenceHandle> {
        let st = lock(&self.shared.state);
        let thread_id = st.thread_id.clone()?;
        let mut metadata = Map::new();
        metadata.insert("provider".into(), json!(PROVIDER));
        metadata.insert("cwd".into(), json!(st.config.cwd));
        metadata.insert("title".into(), json!(st.config.title));
        metadata.insert("threadId".into(), json!(thread_id));
        metadata.insert("modeId".into(), json!(st.config.mode_id));
        metadata.insert("model".into(), json!(st.config.model));
        metadata.insert(
            "thinkingOptionId".into(),
            json!(options::normalize_thinking_option(
                st.config.thinking_option_id.as_deref()
            )),
        );
        metadata.insert("toolPolicy".into(), json!(st.config.tool_policy));
        metadata.insert("systemPrompt".into(), json!(st.config.system_prompt));
        metadata.insert("mcpServers".into(), json!(st.config.mcp_servers));
        Some(PersistenceHandle {
            provider: PROVIDER.into(),
            session_id: thread_id.clone(),
            native_handle: Some(thread_id),
            metadata,
        })
    }

    async fn close(&self) -> Result<()> {
        let (pending, active) = {
            let mut st = lock(&self.shared.state);
            st.closed = true;
            st.connected = false;
            (std::mem::take(&mut st.permissions), st.active_turn.take())
        };
        let rpc = lock(&self.shared.rpc).take();
        if let Some(rpc) = &rpc {
            for p in &pending {
                rpc.respond(&p.server_id, p.cancel_reply());
            }
        }
        if let Some(mut a) = active {
            for w in a.ident_waiters.drain(..) {
                let _ = w.send(None);
            }
            if let Some(tx) = a.outcome.take() {
                let _ = tx.send(TurnOutcome::Closed("session closed".into()));
            }
        }
        if let Some(rpc) = rpc {
            rpc.close().await;
        }
        Ok(())
    }
}

/// Rebuild a [`SessionConfig`] from a persistence handle's metadata (what the TS adapter calls
/// `storedConfig`), with the caller's `cwd` fallback.
pub fn config_from_handle(handle: &PersistenceHandle) -> SessionConfig {
    let m = &handle.metadata;
    let s = |k: &str| m.get(k).and_then(Value::as_str).map(str::to_string);
    SessionConfig {
        cwd: s("cwd").unwrap_or_default(),
        system_prompt: s("systemPrompt"),
        mode_id: s("modeId"),
        model: s("model"),
        thinking_option_id: s("thinkingOptionId"),
        title: s("title"),
        tool_policy: m
            .get("toolPolicy")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        mcp_servers: m
            .get("mcpServers")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default(),
        ..Default::default()
    }
}
