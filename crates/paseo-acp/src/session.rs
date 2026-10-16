//! One live ACP session: the Rust equivalent of `ACPAgentSession`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Notify};

use crate::agent::{
    AgentMode, CapabilityFlags, Feature, PendingPermission, PermissionResponse, PersistenceHandle,
    PromptBlock, PromptInput, RunOptions, RuntimeInfo, SessionConfig, SlashCommand, TurnUsage,
};
use crate::connection::{drive, Connection, IncomingHandler, Responder};
use crate::derive::{derive_current_config_value, derive_features, derive_modes};
use crate::error::{Error, Result};
use crate::events::SessionEvent;
use crate::host::{host_error, read_text_file, write_text_file, TerminalHost};
use crate::permission::{is_chooser_request, select_permission_option};
use crate::profile::AgentProfile;
use crate::protocol::{
    client_methods as cm, AgentCapabilities, ContentBlock, KnownContentBlock, KnownSessionUpdate,
    Lenient, ModelInfo, PromptParams, PromptResponse, RequestPermissionRequest,
    RequestPermissionResponse, RpcError, SessionConfigOption, SessionNotification, SessionState,
    Usage,
};
use crate::util::{random_uuid, BoxFuture};

pub const AUTO_ACCEPT_FEATURE_ID: &str = "auto_accept";
pub(crate) const CLOSE_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
pub(crate) struct State {
    pub session_id: Option<String>,
    pub agent_capabilities: Option<AgentCapabilities>,
    pub config_options: Vec<SessionConfigOption>,
    pub available_modes: Vec<AgentMode>,
    pub current_mode: Option<String>,
    pub available_models: Option<Vec<ModelInfo>>,
    pub current_model: Option<String>,
    pub thinking_option_id: Option<String>,
    pub title: Option<String>,
    pub last_activity_at: Option<String>,
    pub cached_commands: Vec<SlashCommand>,
    pub commands_settled: bool,
    pub active_turn: Option<String>,
    pub bootstrap_pending: bool,
    pub early_updates: Vec<SessionNotification>,
    pub replaying_history: bool,
    pub persisted_history: Vec<SessionNotification>,
    pub history_runtime_released: bool,
    pub turn_usage: Option<TurnUsage>,
}

/// The agent's answer to a permission request, plus whether to follow it with `session/cancel`.
type Decision = (RequestPermissionResponse, bool);

struct PendingEntry {
    id: String,
    request: RequestPermissionRequest,
    tx: Option<oneshot::Sender<Decision>>,
    turn_id: Option<String>,
}

pub(crate) struct Inner {
    pub profile: Arc<AgentProfile>,
    pub capabilities: CapabilityFlags,
    pub config: Mutex<SessionConfig>,
    pub conn: Mutex<Option<Connection>>,
    pub state: Mutex<State>,
    pub subscribers: Mutex<Vec<mpsc::UnboundedSender<SessionEvent>>>,
    pending: Mutex<Vec<PendingEntry>>,
    pub terminals: TerminalHost,
    pub commands_notify: Notify,
    pub closed: AtomicBool,
    pub configured_model_ids: HashSet<String>,
    pub agent_id: Option<String>,
}

/// A live session. Cheap to clone; clones share the same process and state.
#[derive(Clone)]
pub struct AcpSession {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for AcpSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpSession")
            .field("provider", &self.provider())
            .field("id", &self.id())
            .finish()
    }
}

pub(crate) struct SessionDriver(pub Arc<Inner>);

impl AcpSession {
    pub(crate) fn new(
        profile: Arc<AgentProfile>,
        config: SessionConfig,
        capabilities: CapabilityFlags,
        configured_model_ids: Vec<String>,
        agent_id: Option<String>,
        launch_env: std::collections::BTreeMap<String, String>,
    ) -> AcpSession {
        let state = State {
            available_modes: profile.default_modes.clone(),
            current_mode: config.mode_id.clone(),
            current_model: config.model.clone(),
            thinking_option_id: config.thinking_option_id.clone(),
            title: config.title.clone(),
            ..State::default()
        };
        let terminals = TerminalHost::new(config.cwd.clone(), launch_env);
        AcpSession {
            inner: Arc::new(Inner {
                profile,
                capabilities,
                config: Mutex::new(config),
                conn: Mutex::new(None),
                state: Mutex::new(state),
                subscribers: Mutex::new(Vec::new()),
                pending: Mutex::new(Vec::new()),
                terminals,
                commands_notify: Notify::new(),
                closed: AtomicBool::new(false),
                configured_model_ids: configured_model_ids.into_iter().collect(),
                agent_id,
            }),
        }
    }

    /// Attach the connection and start pumping agent traffic into this session.
    pub(crate) fn attach(
        &self,
        conn: Connection,
        incoming: mpsc::UnboundedReceiver<crate::connection::Incoming>,
    ) {
        *self.inner.conn.lock().expect("conn lock") = Some(conn.clone());
        drive(conn, incoming, Arc::new(SessionDriver(self.inner.clone())));
    }

    pub fn provider(&self) -> &str {
        &self.inner.profile.provider
    }

    pub fn capabilities(&self) -> &CapabilityFlags {
        &self.inner.capabilities
    }

    pub fn id(&self) -> Option<String> {
        self.inner.state.lock().ok()?.session_id.clone()
    }

    pub fn agent_id(&self) -> Option<&str> {
        self.inner.agent_id.as_deref()
    }

    /// Process capabilities reported by `initialize`.
    pub fn agent_capabilities(&self) -> Option<AgentCapabilities> {
        self.inner.state.lock().ok()?.agent_capabilities.clone()
    }

    /// Stream of raw events. A subscriber that joins after the session id is known first
    /// receives `ThreadStarted`.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<SessionEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(session_id) = self.id() {
            let _ = tx.send(SessionEvent::ThreadStarted { session_id });
        }
        self.inner
            .subscribers
            .lock()
            .expect("subscribers lock")
            .push(tx);
        rx
    }

    pub fn runtime_info(&self) -> RuntimeInfo {
        self.inner.runtime_info()
    }

    pub fn available_modes(&self) -> Vec<AgentMode> {
        self.inner
            .state
            .lock()
            .map(|s| s.available_modes.clone())
            .unwrap_or_default()
    }

    pub fn current_mode(&self) -> Option<String> {
        self.inner.state.lock().ok()?.current_mode.clone()
    }

    pub fn config_options(&self) -> Vec<SessionConfigOption> {
        self.inner
            .state
            .lock()
            .map(|s| s.config_options.clone())
            .unwrap_or_default()
    }

    pub fn features(&self) -> Vec<Feature> {
        let config = self.inner.config.lock().expect("config lock");
        let auto = Feature::Toggle {
            id: AUTO_ACCEPT_FEATURE_ID.into(),
            label: "Auto-accept".into(),
            description: Some("Automatically approve permission requests".into()),
            tooltip: None,
            icon: None,
            value: is_auto_accept_enabled(&config),
        };
        let state = self.inner.state.lock().expect("state lock");
        let mut out = vec![auto];
        out.extend(derive_features(
            &state.config_options,
            &self.inner.profile.config_feature_options,
        ));
        out
    }

    pub fn pending_permissions(&self) -> Vec<PendingPermission> {
        self.inner
            .pending
            .lock()
            .map(|p| {
                p.iter()
                    .map(|e| PendingPermission {
                        id: e.id.clone(),
                        provider: self.inner.profile.provider.clone(),
                        raw: e.request.clone(),
                        turn_id: e.turn_id.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn describe_persistence(&self) -> Option<PersistenceHandle> {
        let session_id = self.id()?;
        let config = self.inner.config.lock().ok()?;
        let title = self.inner.state.lock().ok()?.title.clone();
        let mut meta = serde_json::Map::new();
        meta.insert(
            "provider".into(),
            Value::String(self.provider().to_string()),
        );
        meta.insert("cwd".into(), Value::String(config.cwd.clone()));
        if let Some(v) = &config.model {
            meta.insert("model".into(), Value::String(v.clone()));
        }
        if let Some(v) = &config.mode_id {
            meta.insert("modeId".into(), Value::String(v.clone()));
        }
        if let Some(v) = &config.thinking_option_id {
            meta.insert("thinkingOptionId".into(), Value::String(v.clone()));
        }
        if !config.feature_values.is_empty() {
            meta.insert(
                "featureValues".into(),
                Value::Object(
                    config
                        .feature_values
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                ),
            );
        }
        meta.insert(
            "title".into(),
            title.map(Value::String).unwrap_or(Value::Null),
        );
        Some(PersistenceHandle {
            provider: self.provider().to_string(),
            session_id: session_id.clone(),
            native_handle: Some(session_id),
            metadata: Some(Value::Object(meta)),
        })
    }

    /// History replayed by `session/load`, drained once (`streamHistory`).
    pub fn take_history(&self) -> Vec<SessionNotification> {
        self.inner
            .state
            .lock()
            .map(|mut s| std::mem::take(&mut s.persisted_history))
            .unwrap_or_default()
    }

    /// Slash commands; waits for the agent's first batch when the profile asks for it.
    pub async fn list_commands(&self) -> Vec<SlashCommand> {
        let inner = &self.inner;
        {
            let s = inner.state.lock().expect("state lock");
            if !s.cached_commands.is_empty() {
                return s.cached_commands.clone();
            }
        }
        if !inner.profile.wait_for_initial_commands || inner.closed.load(Ordering::SeqCst) {
            return inner
                .state
                .lock()
                .map(|s| s.cached_commands.clone())
                .unwrap_or_default();
        }
        let wait = async {
            loop {
                let notified = inner.commands_notify.notified();
                tokio::pin!(notified);
                // Register before checking the flag so a settle in between is not missed.
                notified.as_mut().enable();
                if inner
                    .state
                    .lock()
                    .map(|s| s.commands_settled)
                    .unwrap_or(true)
                {
                    return;
                }
                notified.await;
            }
        };
        let _ = tokio::time::timeout(inner.profile.initial_commands_wait_timeout, wait).await;
        inner.settle_commands();
        inner
            .state
            .lock()
            .map(|s| s.cached_commands.clone())
            .unwrap_or_default()
    }

    /// Send a prompt and return the turn id; progress arrives through [`subscribe`](Self::subscribe).
    pub async fn start_turn(
        &self,
        prompt: impl Into<PromptInput>,
        options: RunOptions,
    ) -> Result<String> {
        let inner = &self.inner;
        let provider = &inner.profile.provider;
        if inner.closed.load(Ordering::SeqCst) {
            return Err(Error::Unsupported(format!("{provider} session is closed")));
        }
        let conn = inner.connection();
        let (Some(conn), Some(session_id)) = (conn, self.id()) else {
            return Err(Error::Unsupported(format!(
                "{provider} session is not initialized"
            )));
        };
        let turn_id = random_uuid();
        let message_id = options.client_message_id.unwrap_or_else(random_uuid);
        {
            let mut s = inner.state.lock().expect("state lock");
            if s.active_turn.is_some() {
                return Err(Error::Unsupported(
                    "A foreground turn is already active".into(),
                ));
            }
            s.active_turn = Some(turn_id.clone());
            s.turn_usage = s.turn_usage.take();
        }
        inner.emit_bootstrap_thread_event();
        inner.emit(SessionEvent::TurnStarted {
            turn_id: turn_id.clone(),
            message_id: message_id.clone(),
        });

        let params = PromptParams {
            session_id,
            message_id: Some(message_id),
            prompt: to_content_blocks(&prompt.into()),
        };
        let inner = self.inner.clone();
        let task_turn = turn_id.clone();
        tokio::spawn(async move {
            match conn.prompt(&params).await {
                Ok(resp) => inner.handle_prompt_response(resp, task_turn),
                // The agent went away mid-prompt: same failure as a process exit.
                Err(Error::Closed) => inner.fail_agent_exited(Some(task_turn)).await,
                Err(err) => {
                    let summary = err.summarize();
                    let fallback = summary.message.clone();
                    let diagnostic =
                        inner.collect_diagnostic(summary.diagnostic.unwrap_or(fallback));
                    inner.finish_turn(SessionEvent::TurnFailed {
                        turn_id: task_turn,
                        error: summary.message,
                        code: summary.code,
                        diagnostic: Some(diagnostic),
                    });
                }
            }
        });
        Ok(turn_id)
    }

    /// Run one turn to completion. Fails when the turn fails.
    pub async fn run(
        &self,
        prompt: impl Into<PromptInput>,
        options: RunOptions,
    ) -> Result<RunResult> {
        let mut events = self.subscribe();
        let turn_id = self.start_turn(prompt, options).await?;
        let mut result = RunResult {
            session_id: self.id().unwrap_or_default(),
            final_text: String::new(),
            usage: None,
            canceled: false,
        };
        while let Some(event) = events.recv().await {
            match event {
                SessionEvent::Update(n) => {
                    if let Lenient::Known(KnownSessionUpdate::AgentMessageChunk {
                        content: Lenient::Known(KnownContentBlock::Text { text, .. }),
                        ..
                    }) = &n.update
                    {
                        result.final_text.push_str(text);
                    }
                }
                SessionEvent::TurnCompleted { turn_id: t, usage } if t == turn_id => {
                    result.usage = usage;
                    return Ok(result);
                }
                SessionEvent::TurnCanceled { turn_id: t, .. } if t == turn_id => {
                    result.canceled = true;
                    return Ok(result);
                }
                SessionEvent::TurnFailed {
                    turn_id: t, error, ..
                } if t == turn_id => {
                    return Err(Error::Protocol(error));
                }
                _ => {}
            }
        }
        Err(Error::Closed)
    }

    /// Answer a pending permission request.
    pub async fn respond_to_permission(
        &self,
        request_id: &str,
        response: PermissionResponse,
    ) -> Result<()> {
        let (entry, option_id) = {
            let mut pending = self.inner.pending.lock().expect("pending lock");
            let Some(pos) = pending.iter().position(|e| e.id == request_id) else {
                return Err(Error::NotFound(format!(
                    "No pending permission request with id '{request_id}'"
                )));
            };
            let option = select_permission_option(&pending[pos].request.options, &response)
                .map(|o| o.option_id.clone());
            let (selected, behavior) = match &response {
                PermissionResponse::Allow { selected_action_id } => (selected_action_id, "allow"),
                PermissionResponse::Deny {
                    selected_action_id, ..
                } => (selected_action_id, "deny"),
            };
            if let (Some(id), None) = (selected, &option) {
                return Err(Error::Unsupported(format!(
                    "ACP permission action '{id}' does not exist or does not match '{behavior}' behavior"
                )));
            }
            (pending.remove(pos), option)
        };
        let mut entry = entry;
        let interrupt = matches!(
            response,
            PermissionResponse::Deny {
                interrupt: true,
                ..
            }
        );
        if let Some(tx) = entry.tx.take() {
            let decision = match option_id {
                Some(id) => RequestPermissionResponse::selected(id),
                None => RequestPermissionResponse::cancelled(),
            };
            // The request handler writes the answer, then the cancel when `interrupt` is set.
            let _ = tx.send((decision, interrupt));
        }
        self.inner.emit(SessionEvent::PermissionResolved {
            request_id: request_id.to_string(),
            turn_id: entry.turn_id.clone(),
            response: response.clone(),
        });
        Ok(())
    }

    /// Cancel the active turn and any pending permission requests.
    pub async fn interrupt(&self) -> Result<()> {
        let (Some(conn), Some(sid)) = (self.inner.connection(), self.id()) else {
            return Ok(());
        };
        self.inner.cancel_pending_permissions();
        let active = self
            .inner
            .state
            .lock()
            .map(|s| s.active_turn.is_some())
            .unwrap_or(false);
        if active {
            conn.cancel(&sid)?;
        }
        Ok(())
    }

    /// Release the process. The durable native session is left alone.
    pub async fn close(&self) {
        let inner = &self.inner;
        if inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        inner.settle_commands();
        inner.cancel_pending_permissions();
        let conn = inner.connection();
        if let (Some(conn), Some(sid)) = (&conn, self.id()) {
            let active = inner
                .state
                .lock()
                .map(|s| s.active_turn.is_some())
                .unwrap_or(false);
            if active {
                let _ = conn.cancel(&sid);
            }
            let can_close = inner
                .state
                .lock()
                .map(|s| {
                    s.agent_capabilities
                        .as_ref()
                        .is_some_and(AgentCapabilities::can_close)
                })
                .unwrap_or(false);
            if can_close {
                let _ = crate::util::with_timeout(
                    CLOSE_REQUEST_TIMEOUT,
                    "ACP closeSession timed out",
                    async { conn.close_session(&sid).await },
                )
                .await;
            }
        }
        inner.terminals.kill_all().await;
        if let Some(conn) = &conn {
            conn.process()
                .terminate(Duration::from_secs(2), Duration::from_secs(2))
                .await;
        }
        inner.subscribers.lock().expect("subscribers lock").clear();
        *inner.conn.lock().expect("conn lock") = None;
        if let Ok(mut s) = inner.state.lock() {
            s.active_turn = None;
        }
    }

    // ---- bootstrap, used by the client ----

    pub(crate) fn apply_session_state(&self, response: SessionState) {
        self.inner.apply_session_state(response);
    }

    pub(crate) fn set_session_id(&self, id: String) {
        let mut s = self.inner.state.lock().expect("state lock");
        s.session_id = Some(id);
        s.bootstrap_pending = true;
    }

    pub(crate) fn set_agent_capabilities(&self, caps: Option<AgentCapabilities>) {
        self.inner
            .state
            .lock()
            .expect("state lock")
            .agent_capabilities = caps;
    }

    pub(crate) fn begin_history_replay(&self) {
        self.inner
            .state
            .lock()
            .expect("state lock")
            .replaying_history = true;
    }

    pub(crate) fn end_history_replay(&self) {
        self.inner
            .state
            .lock()
            .expect("state lock")
            .replaying_history = false;
    }

    /// Re-deliver updates that arrived before `session/new` answered with an id.
    pub(crate) fn flush_early_updates(&self) {
        let early = std::mem::take(&mut self.inner.state.lock().expect("state lock").early_updates);
        for update in early {
            self.inner.session_update(update);
        }
    }

    /// History resume: the process is released after replay.
    pub(crate) async fn release_history_runtime(&self) {
        let conn = self.inner.connection();
        if let Some(conn) = conn {
            conn.process()
                .terminate(Duration::from_secs(2), Duration::from_secs(2))
                .await;
        }
        if let Ok(mut s) = self.inner.state.lock() {
            s.history_runtime_released = true;
        }
        *self.inner.conn.lock().expect("conn lock") = None;
        self.inner.settle_commands();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunResult {
    pub session_id: String,
    pub final_text: String,
    pub usage: Option<TurnUsage>,
    pub canceled: bool,
}

pub(crate) fn is_auto_accept_enabled(config: &SessionConfig) -> bool {
    config.feature_values.get(AUTO_ACCEPT_FEATURE_ID) == Some(&Value::Bool(true))
}

pub fn to_content_blocks(prompt: &PromptInput) -> Vec<ContentBlock> {
    match prompt {
        PromptInput::Text(t) => vec![KnownContentBlock::text(t.clone()).into()],
        PromptInput::Blocks(blocks) => blocks
            .iter()
            .map(|b| match b {
                PromptBlock::Text(t) => KnownContentBlock::text(t.clone()).into(),
                PromptBlock::Image { data, mime_type } => {
                    KnownContentBlock::image(data.clone(), mime_type.clone()).into()
                }
            })
            .collect(),
    }
}

pub fn map_usage(usage: &Usage) -> TurnUsage {
    TurnUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_input_tokens: usage.cached_read_tokens,
        ..TurnUsage::default()
    }
}

impl Inner {
    pub(crate) fn connection(&self) -> Option<Connection> {
        self.conn.lock().ok()?.clone()
    }

    pub(crate) fn emit(&self, event: SessionEvent) {
        if let Ok(mut subs) = self.subscribers.lock() {
            subs.retain(|s| s.send(event.clone()).is_ok());
        }
    }

    pub(crate) fn runtime_info(&self) -> RuntimeInfo {
        let s = self.state.lock().expect("state lock");
        RuntimeInfo {
            provider: self.profile.provider.clone(),
            session_id: s.session_id.clone(),
            model: s.current_model.clone(),
            thinking_option_id: s.thinking_option_id.clone(),
            mode_id: s.current_mode.clone(),
            title: s.title.clone(),
        }
    }

    pub(crate) fn transform_config_options(
        &self,
        options: Vec<SessionConfigOption>,
    ) -> Vec<SessionConfigOption> {
        self.profile.hooks.transform_config_options(options)
    }

    pub(crate) fn settle_commands(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.commands_settled = true;
        }
        self.commands_notify.notify_waiters();
    }

    fn emit_bootstrap_thread_event(&self) {
        let id = {
            let mut s = self.state.lock().expect("state lock");
            if !s.bootstrap_pending {
                return;
            }
            s.bootstrap_pending = false;
            s.session_id.clone()
        };
        if let Some(session_id) = id {
            self.emit(SessionEvent::ThreadStarted { session_id });
        }
    }

    pub(crate) fn apply_session_state(&self, response: SessionState) {
        let transformed = self.profile.hooks.transform_session_response(response);
        let options =
            self.transform_config_options(transformed.config_options.clone().unwrap_or_default());
        let (modes, current) = derive_modes(
            &self.profile.default_modes,
            transformed.modes.as_ref(),
            &options,
        );
        let mut s = self.state.lock().expect("state lock");
        s.available_modes = modes;
        s.current_mode = current.or(s.current_mode.take());
        s.available_models = transformed
            .models
            .as_ref()
            .and_then(|m| m.available_models.clone());
        s.current_model = transformed
            .models
            .as_ref()
            .and_then(|m| m.current_model_id.clone())
            .or_else(|| derive_current_config_value(&options, "model"));
        s.thinking_option_id =
            derive_current_config_value(&options, "thought_level").or(s.thinking_option_id.take());
        s.config_options = options;
    }

    fn collect_diagnostic(&self, message: String) -> String {
        let mut parts = vec![message];
        if let Some(conn) = self.connection() {
            if let Some(exit) = conn.process().exit() {
                if let Some(code) = exit.code {
                    parts.push(format!("exitCode={code}"));
                }
                if let Some(sig) = exit.signal {
                    parts.push(format!("signal={sig}"));
                }
            }
        }
        parts.join(" | ")
    }

    fn handle_prompt_response(&self, response: PromptResponse, turn_id: String) {
        let usage = {
            let mut s = self.state.lock().expect("state lock");
            if let Some(u) = &response.usage {
                s.turn_usage = Some(map_usage(u));
            }
            s.turn_usage.clone()
        };
        if response.is_cancelled() {
            self.finish_turn(SessionEvent::TurnCanceled {
                turn_id,
                reason: "Interrupted".into(),
            });
        } else {
            self.finish_turn(SessionEvent::TurnCompleted { turn_id, usage });
        }
    }

    pub(crate) fn finish_turn(&self, event: SessionEvent) {
        if !matches!(event, SessionEvent::TurnCompleted { .. }) {
            // A turn that ends without completing leaves no way to answer its open requests.
            self.cancel_pending_permissions();
        }
        if let Ok(mut s) = self.state.lock() {
            s.active_turn = None;
        }
        self.emit(event);
    }

    pub(crate) fn cancel_pending_permissions(&self) {
        let drained: Vec<PendingEntry> = self
            .pending
            .lock()
            .map(|mut p| p.drain(..).collect())
            .unwrap_or_default();
        for mut entry in drained {
            if let Some(tx) = entry.tx.take() {
                let _ = tx.send((RequestPermissionResponse::cancelled(), false));
            }
        }
    }

    /// Mirrors `sessionUpdate` + `translateSessionUpdate`: apply state effects, then deliver.
    pub(crate) fn session_update(&self, n: SessionNotification) {
        let events = {
            let mut s = self.state.lock().expect("state lock");
            let Some(sid) = s.session_id.clone() else {
                s.early_updates.push(n);
                return;
            };
            if n.session_id != sid {
                return;
            }
            drop(s);
            self.translate_update(&n)
        };
        self.deliver(events);
    }

    fn deliver(&self, events: Vec<SessionEvent>) {
        let replaying = self
            .state
            .lock()
            .map(|s| s.replaying_history)
            .unwrap_or(false);
        if replaying {
            let mut s = self.state.lock().expect("state lock");
            for e in events {
                if let SessionEvent::Update(n) = e {
                    if is_conversation_update(&n) {
                        s.persisted_history.push(n);
                    }
                }
            }
            return;
        }
        for e in events {
            self.emit(e);
        }
    }

    fn translate_update(&self, n: &SessionNotification) -> Vec<SessionEvent> {
        let mut events = vec![SessionEvent::Update(n.clone())];
        let Lenient::Known(update) = &n.update else {
            return events;
        };
        match update {
            KnownSessionUpdate::CurrentModeUpdate { current_mode_id } => {
                let transformed = self.profile.hooks.transform_mode_id(current_mode_id);
                let mut s = self.state.lock().expect("state lock");
                s.current_mode = transformed;
                events.push(SessionEvent::ModeChanged {
                    current_mode_id: s.current_mode.clone(),
                    available_modes: s.available_modes.clone(),
                });
            }
            KnownSessionUpdate::ConfigOptionUpdate { config_options } => {
                let options = self.transform_config_options(config_options.clone());
                let (modes, next_mode) = derive_modes(&self.profile.default_modes, None, &options);
                let next_model = derive_current_config_value(&options, "model");
                let next_thinking = derive_current_config_value(&options, "thought_level");
                {
                    let mut s = self.state.lock().expect("state lock");
                    s.config_options = options;
                    s.available_modes = modes;
                    s.current_mode = next_mode.clone().or(s.current_mode.take());
                    s.current_model = next_model.clone().or(s.current_model.take());
                    s.thinking_option_id = next_thinking.clone().or(s.thinking_option_id.take());
                }
                if next_mode.is_some() {
                    let s = self.state.lock().expect("state lock");
                    events.push(SessionEvent::ModeChanged {
                        current_mode_id: s.current_mode.clone(),
                        available_modes: s.available_modes.clone(),
                    });
                }
                if next_model.is_some() {
                    events.push(SessionEvent::ModelChanged {
                        runtime: self.runtime_info(),
                    });
                }
                if next_thinking.is_some() {
                    events.push(SessionEvent::ThinkingOptionChanged {
                        thinking_option_id: self
                            .state
                            .lock()
                            .expect("state lock")
                            .thinking_option_id
                            .clone(),
                    });
                }
            }
            KnownSessionUpdate::SessionInfoUpdate { title, updated_at } => {
                let mut s = self.state.lock().expect("state lock");
                // Presence of the key matters in TS; an absent field leaves the value alone.
                if title.is_some() {
                    s.title = title.clone();
                }
                if updated_at.is_some() {
                    s.last_activity_at = updated_at.clone();
                }
            }
            KnownSessionUpdate::UsageUpdate { used, size, .. } => {
                // Same validity guards as the TS context meter.
                if let (Some(size), Some(used)) = (size.as_f64(), used.as_f64()) {
                    if size.is_finite() && size > 0.0 && used.is_finite() && used >= 0.0 {
                        let (turn_id, mut usage) = {
                            let s = self.state.lock().expect("state lock");
                            (
                                s.active_turn.clone(),
                                s.turn_usage.clone().unwrap_or_default(),
                            )
                        };
                        usage.context_window_max_tokens = Some(size as u64);
                        usage.context_window_used_tokens = Some(used as u64);
                        events.push(SessionEvent::UsageUpdated { usage, turn_id });
                    }
                }
            }
            KnownSessionUpdate::AvailableCommandsUpdate { available_commands } => {
                let commands: Vec<SlashCommand> = available_commands
                    .iter()
                    .map(|c| SlashCommand {
                        name: c.name.clone(),
                        description: c.description.clone(),
                        argument_hint: c.input_hint(),
                        kind: crate::agent::SlashCommandKind::Command,
                    })
                    .collect();
                self.state.lock().expect("state lock").cached_commands = commands.clone();
                self.settle_commands();
                events.push(SessionEvent::CommandsUpdated { commands });
            }
            _ => {}
        }
        events
    }

    fn ext_notification(&self, method: &str, params: &Value) {
        self.emit(SessionEvent::ExtNotification {
            method: method.to_string(),
            params: params.clone(),
        });
        let Some(commands) = self.profile.hooks.extension_commands(method, params) else {
            return;
        };
        // A batch addressed to another session is ignored.
        let target = params.get("sessionId").and_then(Value::as_str);
        let own = self.state.lock().ok().and_then(|s| s.session_id.clone());
        if let (Some(t), Some(o)) = (target, &own) {
            if t != o {
                return;
            }
        }
        if !commands.is_empty() {
            self.state.lock().expect("state lock").cached_commands = commands.clone();
            self.emit(SessionEvent::CommandsUpdated { commands });
        }
        self.settle_commands();
    }

    async fn request_permission(self: Arc<Self>, params: RequestPermissionRequest) -> Decision {
        let auto = {
            let config = self.config.lock().expect("config lock");
            is_auto_accept_enabled(&config) && !is_chooser_request(&params.options)
        };
        if auto {
            if let Some(opt) =
                select_permission_option(&params.options, &PermissionResponse::allow())
            {
                return (
                    RequestPermissionResponse::selected(opt.option_id.clone()),
                    false,
                );
            }
        }
        let request_id = random_uuid();
        let (tx, rx) = oneshot::channel();
        let turn_id = self.state.lock().ok().and_then(|s| s.active_turn.clone());
        self.pending
            .lock()
            .expect("pending lock")
            .push(PendingEntry {
                id: request_id.clone(),
                request: params.clone(),
                tx: Some(tx),
                turn_id: turn_id.clone(),
            });
        self.emit(SessionEvent::PermissionRequested {
            request_id,
            turn_id,
            request: params,
        });
        rx.await
            .unwrap_or_else(|_| (RequestPermissionResponse::cancelled(), false))
    }

    /// Answer one agent request through `responder`.
    async fn handle_request(self: Arc<Self>, method: String, params: Value, responder: Responder) {
        fn parse<T: serde::de::DeserializeOwned>(
            params: Value,
        ) -> std::result::Result<T, RpcError> {
            serde_json::from_value(params).map_err(|e| RpcError::invalid_params(e.to_string()))
        }
        if method == cm::SESSION_REQUEST_PERMISSION {
            let p: RequestPermissionRequest = match parse(params) {
                Ok(p) => p,
                Err(e) => return responder.send(Err(e)),
            };
            let session_id = p.session_id.clone();
            let (response, interrupt) = self.clone().request_permission(p).await;
            responder.send(
                serde_json::to_value(response).map_err(|e| RpcError::internal(e.to_string())),
            );
            if interrupt {
                if let Some(conn) = self.connection() {
                    let _ = conn.cancel(&session_id);
                }
            }
            return;
        }
        responder.send(self.handle_host_request(&method, params).await);
    }

    async fn handle_host_request(
        &self,
        method: &str,
        params: Value,
    ) -> std::result::Result<Value, RpcError> {
        fn parse<T: serde::de::DeserializeOwned>(
            params: Value,
        ) -> std::result::Result<T, RpcError> {
            serde_json::from_value(params).map_err(|e| RpcError::invalid_params(e.to_string()))
        }
        let result = match method {
            cm::FS_READ_TEXT_FILE => read_text_file(&parse(params)?).await,
            cm::FS_WRITE_TEXT_FILE => write_text_file(&parse(params)?).await,
            cm::TERMINAL_CREATE => self.terminals.create(&parse(params)?),
            cm::TERMINAL_OUTPUT => self.terminals.output(&parse(params)?),
            cm::TERMINAL_WAIT_FOR_EXIT => self.terminals.wait_for_exit(&parse(params)?).await,
            cm::TERMINAL_RELEASE => self.terminals.release(&parse(params)?).await,
            cm::TERMINAL_KILL => self.terminals.kill(&parse(params)?).await,
            _ => return Err(RpcError::method_not_found(method)),
        };
        result.map_err(host_error)
    }

    async fn on_closed(self: Arc<Self>) {
        self.fail_agent_exited(None).await;
    }

    /// The agent process is gone. Fail the active turn, unless it already finished: a reply
    /// followed by an exit must not turn a completed turn into a failure. `only_turn` limits
    /// the failure to one turn.
    async fn fail_agent_exited(&self, only_turn: Option<String>) {
        if self.closed.load(Ordering::SeqCst)
            || self
                .state
                .lock()
                .map(|s| s.history_runtime_released)
                .unwrap_or(false)
        {
            return;
        }
        let (exit, stderr) = match self.connection() {
            Some(c) => (
                c.process().wait_exit(Duration::from_millis(500)).await,
                c.process().stderr(),
            ),
            None => (None, String::new()),
        };
        // Give a response that arrived just before the exit time to complete its turn.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let Some(turn_id) = self.state.lock().ok().and_then(|s| s.active_turn.clone()) else {
            return;
        };
        if only_turn.as_ref().is_some_and(|t| *t != turn_id) {
            return;
        }
        let code = exit
            .and_then(|e| e.code)
            .map(|c| c.to_string())
            .unwrap_or_else(|| "null".into());
        let signal = exit
            .and_then(|e| e.signal)
            .map(|s| format!(", {s}"))
            .unwrap_or_default();
        let stderr = stderr.trim().to_string();
        self.finish_turn(SessionEvent::TurnFailed {
            turn_id,
            error: format!("ACP agent exited unexpectedly ({code}{signal})"),
            code: None,
            diagnostic: (!stderr.is_empty()).then_some(stderr),
        });
    }
}

fn is_conversation_update(n: &SessionNotification) -> bool {
    matches!(
        n.update.known(),
        Some(
            KnownSessionUpdate::UserMessageChunk { .. }
                | KnownSessionUpdate::AgentMessageChunk { .. }
                | KnownSessionUpdate::AgentThoughtChunk { .. }
                | KnownSessionUpdate::ToolCall(_)
                | KnownSessionUpdate::ToolCallUpdate(_)
                | KnownSessionUpdate::Plan { .. }
        )
    )
}

impl IncomingHandler for SessionDriver {
    fn on_notification(&self, method: &str, params: Value) {
        if method == cm::SESSION_UPDATE {
            match serde_json::from_value::<SessionNotification>(params.clone()) {
                Ok(n) => self.0.session_update(n),
                Err(_) => self.0.emit(SessionEvent::ExtNotification {
                    method: method.to_string(),
                    params,
                }),
            }
        } else {
            self.0.ext_notification(method, &params);
        }
    }

    fn on_request(
        self: Arc<Self>,
        method: String,
        params: Value,
        responder: Responder,
    ) -> BoxFuture<'static, ()> {
        let inner = self.0.clone();
        Box::pin(async move { inner.handle_request(method, params, responder).await })
    }

    fn on_closed(self: Arc<Self>) -> BoxFuture<'static, ()> {
        let inner = self.0.clone();
        Box::pin(async move { inner.on_closed().await })
    }
}
