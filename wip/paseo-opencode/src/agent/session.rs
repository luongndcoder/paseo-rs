//! `AgentSession` for one OpenCode session.

use super::mapping::*;
use super::types::*;
use super::{AgentCapabilities, AgentSession};
use crate::client::RawClient;
use crate::error::{Error, Result};
use crate::events::HubMessage;
use crate::manager::ServerHandle;
use crate::options::EVENT_STREAM_READY_TIMEOUT;
use crate::protocol::*;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

/// `interrupt()` waits at most this long for the abort to settle (OpenCode 1.14.42+ blocks
/// `session.abort` until the running tool stops).
pub const INTERRUPT_SETTLE_CAP: Duration = Duration::from_secs(2);
const QUIESCE_POLL: Duration = Duration::from_millis(100);
const QUIESCE_CAP: Duration = Duration::from_secs(5);
const EVENT_CHANNEL: usize = 4096;

pub(crate) fn capabilities() -> AgentCapabilities {
    AgentCapabilities {
        supports_streaming: true,
        supports_session_persistence: true,
        supports_session_listing: true,
        supports_dynamic_modes: true,
        supports_mcp_servers: true,
        supports_reasoning_stream: true,
        supports_tool_invocations: true,
        supports_rewind_conversation: false,
        supports_rewind_files: false,
        supports_rewind_both: true,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum TurnState {
    Idle,
    /// `armed` once the server showed activity for this turn (so a stale idle cannot end it).
    Running {
        turn_id: String,
        dispatch_id: String,
        armed: bool,
    },
    /// Abort sent; the terminal `TurnCanceled` follows the abort acknowledgement or the idle event.
    Stopping {
        turn_id: String,
    },
}

impl TurnState {
    fn turn_id(&self) -> Option<&str> {
        match self {
            TurnState::Idle => None,
            TurnState::Running { turn_id, .. } | TurnState::Stopping { turn_id } => Some(turn_id),
        }
    }
}

struct State {
    turn: TurnState,
    pending: HashMap<String, AgentPermissionRequest>,
    usage_by_message: HashMap<String, AgentUsage>,
    last_usage: AgentUsage,
    config: AgentSessionConfig,
    current_mode: Option<String>,
    /// An interrupt happened and the server has not reported idle yet.
    quiesce: bool,
    mcp_configured: bool,
    modes: Option<Vec<AgentMode>>,
    commands: Option<Vec<AgentSlashCommand>>,
    server_exited: Option<String>,
}

pub(crate) struct SessionParams {
    pub config: AgentSessionConfig,
    pub session_id: String,
    pub client: RawClient,
    pub server: ServerHandle,
    pub persist: bool,
    pub context_windows: HashMap<String, u64>,
}

struct Inner {
    session_id: String,
    client: RawClient,
    server: ServerHandle,
    persist: bool,
    context_windows: HashMap<String, u64>,
    tx: broadcast::Sender<AgentEvent>,
    state: Mutex<State>,
    closed: AtomicBool,
    turn_counter: AtomicU64,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

pub struct OpenCodeAgentSession {
    inner: Arc<Inner>,
}

impl OpenCodeAgentSession {
    pub(crate) fn new(p: SessionParams) -> Self {
        let (tx, _) = broadcast::channel(EVENT_CHANNEL);
        let rx = p.server.events.subscribe();
        let current_mode = normalize_mode_id(p.config.mode_id.as_deref());
        let inner = Arc::new(Inner {
            session_id: p.session_id,
            client: p.client,
            server: p.server,
            persist: p.persist,
            context_windows: p.context_windows,
            tx,
            state: Mutex::new(State {
                turn: TurnState::Idle,
                pending: HashMap::new(),
                usage_by_message: HashMap::new(),
                last_usage: AgentUsage::default(),
                config: p.config,
                current_mode,
                quiesce: false,
                mcp_configured: false,
                modes: None,
                commands: None,
                server_exited: None,
            }),
            closed: AtomicBool::new(false),
            turn_counter: AtomicU64::new(0),
            tasks: Mutex::new(Vec::new()),
        });
        let task = tokio::spawn(event_loop(inner.clone(), rx));
        inner.tasks.lock().unwrap().push(task);
        Self { inner }
    }
}

async fn event_loop(inner: Arc<Inner>, mut rx: broadcast::Receiver<HubMessage>) {
    loop {
        match rx.recv().await {
            Ok(HubMessage::Event(event)) => inner.handle_event(event),
            Ok(HubMessage::ServerExited(why)) => inner.handle_server_exit(why),
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

fn is_aborted_error(error: &Option<Value>) -> bool {
    error
        .as_ref()
        .and_then(|e| e.get("name"))
        .and_then(Value::as_str)
        == Some("MessageAbortedError")
}

fn error_message(error: &Option<Value>) -> String {
    let Some(e) = error else {
        return "OpenCode session error".into();
    };
    e.pointer("/data/message")
        .or_else(|| e.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| e.to_string())
}

fn sum(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}

impl Inner {
    fn emit(&self, event: AgentEvent) {
        let _ = self.tx.send(event);
    }

    fn context_window_max(&self, config: &AgentSessionConfig) -> Option<u64> {
        config
            .model
            .as_ref()
            .and_then(|m| self.context_windows.get(m))
            .copied()
    }

    fn total_usage(&self, state: &State) -> AgentUsage {
        let mut total = AgentUsage {
            context_window_max_tokens: self.context_window_max(&state.config),
            ..Default::default()
        };
        for u in state.usage_by_message.values() {
            total.input_tokens = sum(total.input_tokens, u.input_tokens);
            total.cached_input_tokens = sum(total.cached_input_tokens, u.cached_input_tokens);
            total.output_tokens = sum(total.output_tokens, u.output_tokens);
            if let Some(c) = u.total_cost_usd {
                total.total_cost_usd = Some(total.total_cost_usd.unwrap_or(0.0) + c);
            }
            if u.context_window_used_tokens.is_some() {
                total.context_window_used_tokens = u.context_window_used_tokens;
            }
        }
        total
    }

    /// Ends the active turn with `terminal` unless the turn already ended.
    fn finish_turn(&self, turn_id: &str, terminal: AgentEvent) {
        let mut state = self.state.lock().unwrap();
        if state.turn.turn_id() != Some(turn_id) {
            return;
        }
        state.turn = TurnState::Idle;
        drop(state);
        self.emit(terminal);
    }

    fn handle_server_exit(&self, why: String) {
        let turn = {
            let mut state = self.state.lock().unwrap();
            state.server_exited = Some(why.clone());
            state.turn.turn_id().map(str::to_owned)
        };
        if let Some(turn_id) = turn {
            self.finish_turn(
                &turn_id,
                AgentEvent::TurnFailed {
                    turn_id: turn_id.clone(),
                    error: format!("OpenCode server exited: {why}"),
                },
            );
        }
    }

    fn handle_event(&self, event: Arc<GlobalEvent>) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let ev = &event.payload.event;
        if ev.session_id() != Some(self.session_id.as_str()) {
            return;
        }
        let mut emits: Vec<AgentEvent> = Vec::new();
        let mut finish: Option<(String, AgentEvent)> = None;
        {
            let mut state = self.state.lock().unwrap();
            let turn_id = state.turn.turn_id().map(str::to_owned);
            emits.push(AgentEvent::Provider {
                turn_id: turn_id.clone(),
                event: event.clone(),
            });
            match ev {
                Event::PermissionAsked(r) => {
                    let req = map_permission_request(r);
                    state.pending.insert(req.id.clone(), req.clone());
                    emits.push(AgentEvent::PermissionRequested {
                        turn_id: turn_id.clone(),
                        request: req,
                    });
                }
                Event::QuestionAsked(q) => {
                    let req = map_question_request(q);
                    state.pending.insert(req.id.clone(), req.clone());
                    emits.push(AgentEvent::PermissionRequested {
                        turn_id: turn_id.clone(),
                        request: req,
                    });
                }
                Event::PermissionReplied(r) => {
                    state.pending.remove(&r.request_id);
                    emits.push(AgentEvent::PermissionResolved {
                        request_id: r.request_id.clone(),
                        reply: r.reply.clone(),
                    });
                }
                Event::QuestionReplied(r) | Event::QuestionRejected(r) => {
                    state.pending.remove(&r.request_id);
                    emits.push(AgentEvent::PermissionResolved {
                        request_id: r.request_id.clone(),
                        reply: None,
                    });
                }
                Event::MessageUpdated(m) => {
                    if let Some(info) = &m.info {
                        // Our own prompt coming back as a user message proves the turn is live.
                        if let TurnState::Running {
                            dispatch_id, armed, ..
                        } = &mut state.turn
                        {
                            if info.id == *dispatch_id || info.role.as_deref() == Some("user") {
                                *armed = true;
                            }
                        }
                        if info.is_assistant() && matches!(state.turn, TurnState::Running { .. }) {
                            let usage = message_usage(info);
                            if state.usage_by_message.get(&info.id) != Some(&usage) {
                                state.usage_by_message.insert(info.id.clone(), usage);
                                let total = self.total_usage(&state);
                                if total != state.last_usage {
                                    state.last_usage = total.clone();
                                    emits.push(AgentEvent::UsageUpdated {
                                        turn_id: turn_id.clone(),
                                        usage: total,
                                    });
                                }
                            }
                        }
                    }
                }
                Event::SessionStatus(s) => match s.status.as_ref().map(|s| s.kind.as_str()) {
                    Some("busy") | Some("retry") => {
                        if let TurnState::Running { armed, .. } = &mut state.turn {
                            *armed = true;
                        }
                    }
                    Some("idle") => self.on_idle(&mut state, &mut finish),
                    _ => {}
                },
                Event::SessionIdle(_) => self.on_idle(&mut state, &mut finish),
                Event::SessionError(e) => {
                    let aborted = is_aborted_error(&e.error);
                    if aborted && state.quiesce {
                        // Late echo of an already-acknowledged interrupt.
                    } else if let Some(id) = state.turn.turn_id().map(str::to_owned) {
                        let terminal =
                            if aborted || matches!(state.turn, TurnState::Stopping { .. }) {
                                AgentEvent::TurnCanceled {
                                    turn_id: id.clone(),
                                    reason: "interrupted".into(),
                                }
                            } else {
                                AgentEvent::TurnFailed {
                                    turn_id: id.clone(),
                                    error: error_message(&e.error),
                                }
                            };
                        finish = Some((id, terminal));
                    }
                }
                _ => {}
            }
        }
        for e in emits {
            self.emit(e);
        }
        if let Some((id, terminal)) = finish {
            self.finish_turn(&id, terminal);
        }
    }

    fn on_idle(&self, state: &mut State, finish: &mut Option<(String, AgentEvent)>) {
        state.quiesce = false;
        match state.turn.clone() {
            TurnState::Running {
                turn_id,
                armed: true,
                ..
            } => {
                let usage = self.total_usage(state);
                *finish = Some((
                    turn_id.clone(),
                    AgentEvent::TurnCompleted { turn_id, usage },
                ));
            }
            TurnState::Stopping { turn_id } => {
                *finish = Some((
                    turn_id.clone(),
                    AgentEvent::TurnCanceled {
                        turn_id,
                        reason: "interrupted".into(),
                    },
                ));
            }
            _ => {}
        }
    }

    fn running_agent(&self, state: &State) -> Option<String> {
        // Legacy `full-access` mode id means the build agent.
        state.current_mode.as_ref().map(|m| {
            if m == "full-access" {
                BUILD_MODE_ID.to_owned()
            } else {
                m.clone()
            }
        })
    }

    async fn wait_quiescent(&self) -> Result<()> {
        if !self.state.lock().unwrap().quiesce {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + QUIESCE_CAP;
        loop {
            match self.client.session_status().await {
                Ok(map) if !map.contains_key(&self.session_id) => {
                    self.state.lock().unwrap().quiesce = false;
                    return Ok(());
                }
                Ok(map) if map.get(&self.session_id).is_some_and(|s| s.kind == "idle") => {
                    self.state.lock().unwrap().quiesce = false;
                    return Ok(());
                }
                _ => {}
            }
            if !self.state.lock().unwrap().quiesce {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Other(
                    "OpenCode is still stopping the previous turn".into(),
                ));
            }
            tokio::time::sleep(QUIESCE_POLL).await;
        }
    }

    async fn ensure_mcp_configured(&self) -> Result<()> {
        let servers = {
            let state = self.state.lock().unwrap();
            if state.mcp_configured || state.config.mcp_servers.is_empty() {
                return Ok(());
            }
            state.config.mcp_servers.clone()
        };
        for (name, cfg) in servers {
            let config = match cfg {
                McpServerConfig::Stdio { command, args, env } => {
                    let mut c = serde_json::Map::new();
                    c.insert("type".into(), json!("local"));
                    c.insert(
                        "command".into(),
                        json!(std::iter::once(command).chain(args).collect::<Vec<_>>()),
                    );
                    if let Some(env) = env {
                        c.insert("environment".into(), json!(env));
                    }
                    c.insert("enabled".into(), json!(true));
                    c
                }
                McpServerConfig::Http { url, headers } => {
                    let mut c = serde_json::Map::new();
                    c.insert("type".into(), json!("remote"));
                    c.insert("url".into(), json!(url));
                    if let Some(h) = headers {
                        c.insert("headers".into(), json!(h));
                    }
                    c.insert("enabled".into(), json!(true));
                    c
                }
            };
            match self
                .client
                .mcp_add(&McpAddBody {
                    name: name.clone(),
                    config,
                })
                .await
            {
                Ok(_) => {}
                Err(e) if e.to_string().to_lowercase().contains("already") => {}
                Err(e) => {
                    return Err(Error::Other(format!(
                        "Failed to add OpenCode MCP server '{name}': {e}"
                    )))
                }
            }
        }
        self.state.lock().unwrap().mcp_configured = true;
        Ok(())
    }

    async fn resolve_slash_command(&self, text: &str) -> Option<(String, String)> {
        let (name, args) = parse_slash_command(text)?;
        if name == "compact" || name == "summarize" {
            return Some((name, args));
        }
        let known = match self.commands().await {
            Ok(list) => list,
            Err(_) => return None,
        };
        known.iter().any(|c| c.name == name).then_some((name, args))
    }

    async fn commands(&self) -> Result<Vec<AgentSlashCommand>> {
        if let Some(cached) = self.state.lock().unwrap().commands.clone() {
            return Ok(cached);
        }
        let list = match self.client.commands().await {
            Ok(c) => commands_to_slash(&c),
            Err(_) => commands_to_slash(&[]),
        };
        self.state.lock().unwrap().commands = Some(list.clone());
        Ok(list)
    }

    async fn abort_with_retry(&self) -> Result<()> {
        match self.client.abort(&self.session_id).await {
            Ok(_) => Ok(()),
            Err(_) if !self.closed.load(Ordering::SeqCst) => {
                self.client.abort(&self.session_id).await.map(|_| ())
            }
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl AgentSession for OpenCodeAgentSession {
    fn provider(&self) -> &str {
        "opencode"
    }

    fn id(&self) -> String {
        self.inner.session_id.clone()
    }

    fn capabilities(&self) -> AgentCapabilities {
        capabilities()
    }

    async fn start_turn(
        &self,
        prompt: AgentPromptInput,
        options: AgentRunOptions,
    ) -> Result<String> {
        let inner = &self.inner;
        if inner.closed.load(Ordering::SeqCst) {
            return Err(Error::Closed);
        }
        {
            let state = inner.state.lock().unwrap();
            if let Some(why) = &state.server_exited {
                return Err(Error::ServerExited(why.clone()));
            }
            if state.turn != TurnState::Idle {
                return Err(Error::TurnActive);
            }
        }
        inner.wait_quiescent().await?;
        inner.ensure_mcp_configured().await?;
        inner
            .server
            .events
            .ready(EVENT_STREAM_READY_TIMEOUT)
            .await?;

        let dispatch_id = create_message_id();
        let turn_id = format!(
            "opencode-turn-{}",
            inner.turn_counter.fetch_add(1, Ordering::SeqCst)
        );
        let (config, agent) = {
            let mut state = inner.state.lock().unwrap();
            if state.turn != TurnState::Idle {
                return Err(Error::TurnActive);
            }
            state.turn = TurnState::Running {
                turn_id: turn_id.clone(),
                dispatch_id: dispatch_id.clone(),
                armed: false,
            };
            state.usage_by_message.clear();
            state.last_usage = AgentUsage::default();
            (state.config.clone(), inner.running_agent(&state))
        };
        inner.emit(AgentEvent::TurnStarted {
            turn_id: turn_id.clone(),
        });

        let model = config.model.as_deref().map(ModelRef::parse);
        let variant = normalize_variant_id(config.thinking_option_id.as_deref());
        let text = prompt_text(&prompt);
        let slash = if text.trim_start().starts_with('/') {
            inner.resolve_slash_command(&text).await
        } else {
            None
        };

        let task_inner = inner.clone();
        let task_turn = turn_id.clone();
        let task = tokio::spawn(async move {
            let result: Result<()> = async {
                match slash {
                    Some((name, _)) if name == "compact" || name == "summarize" => task_inner
                        .client
                        .summarize(&task_inner.session_id, model.as_ref())
                        .await
                        .map(|_| ()),
                    Some((name, args)) => task_inner
                        .client
                        .command(
                            &task_inner.session_id,
                            &CommandBody {
                                command: name,
                                arguments: args,
                                message_id: Some(dispatch_id),
                                model: config.model.clone(),
                                agent,
                                variant,
                            },
                        )
                        .await
                        .map(|_| ()),
                    None => {
                        let body = PromptBody {
                            message_id: Some(dispatch_id),
                            parts: build_prompt_parts(&prompt),
                            model,
                            agent,
                            variant,
                            system: compose_system_prompt(&config),
                            format: options
                                .output_schema
                                .map(|schema| json!({ "type": "json_schema", "schema": schema })),
                        };
                        task_inner
                            .client
                            .prompt_async(&task_inner.session_id, &body)
                            .await
                    }
                }
            }
            .await;
            if let Err(e) = result {
                task_inner.finish_turn(
                    &task_turn,
                    AgentEvent::TurnFailed {
                        turn_id: task_turn.clone(),
                        error: e.to_string(),
                    },
                );
            }
        });
        inner.tasks.lock().unwrap().push(task);
        Ok(turn_id)
    }

    async fn run(
        &self,
        prompt: AgentPromptInput,
        options: AgentRunOptions,
    ) -> Result<AgentRunResult> {
        let mut rx = self.subscribe();
        let turn_id = self.start_turn(prompt, options).await?;
        let mut events = Vec::new();
        let mut roles: HashMap<String, bool> = HashMap::new(); // message id -> is assistant
        let mut text_parts: HashMap<String, (String, bool)> = HashMap::new(); // part id -> (text, is_text)
        let mut order: Vec<String> = Vec::new();
        let (mut canceled, mut error, mut usage) = (false, None, AgentUsage::default());
        loop {
            let event = match rx.recv().await {
                Ok(e) => e,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return Err(Error::Closed),
            };
            if let AgentEvent::Provider { event: raw, .. } = &event {
                match &raw.payload.event {
                    Event::MessageUpdated(m) => {
                        if let Some(info) = &m.info {
                            roles.insert(info.id.clone(), info.is_assistant());
                        }
                    }
                    Event::MessagePartUpdated(p) => match &p.part {
                        Some(Part::Text(t)) | Some(Part::Reasoning(t)) => {
                            let is_text = matches!(p.part, Some(Part::Text(_)));
                            let assistant = t
                                .message_id
                                .as_ref()
                                .and_then(|m| roles.get(m))
                                .copied()
                                .unwrap_or(false);
                            if is_text && assistant {
                                if !text_parts.contains_key(&t.id) {
                                    order.push(t.id.clone());
                                }
                                text_parts.insert(t.id.clone(), (t.text.clone(), true));
                            } else if !is_text {
                                text_parts.insert(t.id.clone(), (String::new(), false));
                            }
                        }
                        _ => {}
                    },
                    Event::MessagePartDelta(d) if d.field == "text" => {
                        if let Some((text, true)) = text_parts.get_mut(&d.part_id) {
                            text.push_str(&d.delta);
                        }
                    }
                    _ => {}
                }
            }
            let terminal = match &event {
                AgentEvent::TurnCompleted {
                    turn_id: t,
                    usage: u,
                } if *t == turn_id => {
                    usage = u.clone();
                    true
                }
                AgentEvent::TurnFailed {
                    turn_id: t,
                    error: e,
                } if *t == turn_id => {
                    error = Some(e.clone());
                    true
                }
                AgentEvent::TurnCanceled { turn_id: t, .. } if *t == turn_id => {
                    canceled = true;
                    true
                }
                _ => false,
            };
            events.push(event);
            if terminal {
                break;
            }
        }
        let final_text = order
            .iter()
            .filter_map(|id| text_parts.get(id))
            .map(|(t, _)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(AgentRunResult {
            session_id: self.id(),
            turn_id,
            final_text,
            canceled,
            error,
            usage,
            events,
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.inner.tx.subscribe()
    }

    async fn stream_history(&self) -> Result<Vec<Value>> {
        self.inner
            .client
            .session_get(&self.inner.session_id)
            .await?;
        self.inner
            .client
            .session_messages_raw(&self.inner.session_id)
            .await
    }

    async fn get_runtime_info(&self) -> Result<AgentRuntimeInfo> {
        let state = self.inner.state.lock().unwrap();
        Ok(AgentRuntimeInfo {
            provider: "opencode".into(),
            session_id: Some(self.inner.session_id.clone()),
            model: state.config.model.clone(),
            mode_id: state.current_mode.clone(),
            thinking_option_id: state.config.thinking_option_id.clone(),
        })
    }

    async fn get_available_modes(&self) -> Result<Vec<AgentMode>> {
        if let Some(m) = self.inner.state.lock().unwrap().modes.clone() {
            return Ok(m);
        }
        // Discovery failure yields no modes: users can rename or delete any agent, so a fabricated
        // fallback could name an agent that does not exist.
        let modes = self
            .inner
            .client
            .agents()
            .await
            .map(|a| agents_to_modes(&a))
            .unwrap_or_default();
        self.inner.state.lock().unwrap().modes = Some(modes.clone());
        Ok(modes)
    }

    async fn get_current_mode(&self) -> Option<String> {
        self.inner.state.lock().unwrap().current_mode.clone()
    }

    async fn set_mode(&self, mode_id: &str) -> Result<()> {
        let mut state = self.inner.state.lock().unwrap();
        let normalized = normalize_mode_id(Some(mode_id)).map(|m| {
            if m == "full-access" {
                BUILD_MODE_ID.to_owned()
            } else {
                m
            }
        });
        state.config.mode_id = normalized.clone();
        state.current_mode = normalized;
        Ok(())
    }

    async fn set_model(&self, model_id: Option<&str>) -> Result<()> {
        let model = model_id
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_owned);
        let variant = self
            .inner
            .state
            .lock()
            .unwrap()
            .config
            .thinking_option_id
            .clone();
        // A selected variant only survives when the new model defines it (needs the provider list).
        let keep_variant = match (&model, normalize_variant_id(variant.as_deref())) {
            (Some(m), Some(v)) => {
                let target = ModelRef::parse(m);
                let list = self.inner.client.provider_list().await?;
                let found = list
                    .all
                    .iter()
                    .find(|p| p.id == target.provider_id)
                    .and_then(|p| p.models.get(&target.model_id))
                    .ok_or_else(|| Error::Other(format!("OpenCode model unavailable: {m}")))?;
                let has = found
                    .get("variants")
                    .and_then(Value::as_object)
                    .is_some_and(|vs| vs.contains_key(&v));
                has.then_some(v)
            }
            _ => None,
        };
        let thinking = {
            let mut state = self.inner.state.lock().unwrap();
            state.config.model = model.clone();
            state.config.thinking_option_id = keep_variant;
            state.config.thinking_option_id.clone()
        };
        self.inner.emit(AgentEvent::ModelChanged {
            model,
            thinking_option_id: thinking,
        });
        Ok(())
    }

    async fn set_thinking_option(&self, option_id: Option<&str>) -> Result<()> {
        let variant = normalize_variant_id(option_id);
        let model = {
            let mut state = self.inner.state.lock().unwrap();
            state.config.thinking_option_id = variant.clone();
            state.config.model.clone()
        };
        self.inner.emit(AgentEvent::ModelChanged {
            model,
            thinking_option_id: variant,
        });
        Ok(())
    }

    async fn list_commands(&self) -> Result<Vec<AgentSlashCommand>> {
        self.inner.commands().await
    }

    fn get_pending_permissions(&self) -> Vec<AgentPermissionRequest> {
        self.inner
            .state
            .lock()
            .unwrap()
            .pending
            .values()
            .cloned()
            .collect()
    }

    async fn respond_to_permission(
        &self,
        request_id: &str,
        response: AgentPermissionResponse,
    ) -> Result<()> {
        let pending = self
            .inner
            .state
            .lock()
            .unwrap()
            .pending
            .get(request_id)
            .cloned()
            .ok_or_else(|| Error::UnknownPermission(request_id.to_owned()))?;
        match pending.kind {
            PermissionKind::Question => {
                if response.behavior == PermissionBehavior::Deny {
                    self.inner.client.question_reject(request_id).await?;
                } else {
                    let answers = question_answers(&pending, &response);
                    self.inner
                        .client
                        .question_reply(request_id, &QuestionReplyBody { answers })
                        .await?;
                }
            }
            PermissionKind::Tool => {
                let reply = resolve_permission_reply(&response);
                let message = (response.behavior == PermissionBehavior::Deny)
                    .then(|| response.message.clone())
                    .flatten();
                self.inner
                    .client
                    .permission_reply(request_id, &PermissionReplyBody { reply, message })
                    .await?;
            }
        }
        self.inner.state.lock().unwrap().pending.remove(request_id);
        Ok(())
    }

    fn describe_persistence(&self) -> Option<AgentPersistenceHandle> {
        let state = self.inner.state.lock().unwrap();
        let mut metadata = JsonMap::new();
        metadata.insert("cwd".into(), json!(state.config.cwd));
        if let Some(m) = &state.config.mode_id {
            metadata.insert("modeId".into(), json!(m));
        }
        if let Some(m) = &state.config.model {
            metadata.insert("model".into(), json!(m));
        }
        Some(AgentPersistenceHandle {
            provider: "opencode".into(),
            session_id: self.inner.session_id.clone(),
            native_handle: Some(self.inner.session_id.clone()),
            metadata,
        })
    }

    async fn interrupt(&self) -> Result<()> {
        let inner = self.inner.clone();
        let turn = {
            let mut state = inner.state.lock().unwrap();
            state.quiesce = true;
            let turn = state.turn.turn_id().map(str::to_owned);
            if let Some(id) = &turn {
                state.turn = TurnState::Stopping {
                    turn_id: id.clone(),
                };
            }
            turn
        };
        let task_inner = inner.clone();
        let abort = tokio::spawn(async move {
            let result = task_inner.abort_with_retry().await;
            if result.is_ok() {
                // Cancellation is acknowledged as soon as an abort succeeds.
                if let Some(id) = turn {
                    task_inner.finish_turn(
                        &id,
                        AgentEvent::TurnCanceled {
                            turn_id: id.clone(),
                            reason: "interrupted".into(),
                        },
                    );
                }
            }
            result
        });
        // Only the cap is tolerated; a settled failure means the runner may still be going.
        let abort_abort = abort.abort_handle();
        match tokio::time::timeout(INTERRUPT_SETTLE_CAP, abort).await {
            Ok(Ok(result)) => result,
            Ok(Err(join)) => Err(Error::Other(format!("abort task failed: {join}"))),
            Err(_) => {
                // Let the abort finish in the background.
                let _ = abort_abort;
                Ok(())
            }
        }
    }

    async fn revert_both(&self, message_id: &str) -> Result<()> {
        self.inner
            .client
            .revert(
                &self.inner.session_id,
                &RevertBody {
                    message_id: message_id.to_owned(),
                    part_id: None,
                },
            )
            .await
            .map(|_| ())
    }

    async fn close(&self) -> Result<()> {
        let inner = &self.inner;
        if inner.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        for task in inner.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
        {
            let mut state = inner.state.lock().unwrap();
            state.turn = TurnState::Idle;
            state.pending.clear();
        }
        // Best effort: the session may already be gone.
        let _ = inner.client.abort(&inner.session_id).await;
        if !inner.persist {
            let _ = inner.client.session_delete(&inner.session_id).await;
        }
        inner.server.release().await;
        Ok(())
    }
}

impl Drop for OpenCodeAgentSession {
    fn drop(&mut self) {
        for task in self.inner.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}
