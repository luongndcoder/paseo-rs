//! Shared harness of the paseo-core integration tests: TS test fixtures in Rust form.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use paseo_agent_types::{
    AgentStreamEvent, AgentTimelineItem, AttentionReason, ToolCallDetail, ToolCallItem,
    ToolCallStatus,
};
use paseo_core::mock::{Gate, MockSession};
use paseo_core::{
    AgentManager, AttentionNotice, ManagedAgentView, ManagerEvent, ManagerHooks, ManagerOptions,
    PersistOptions, RegisterOptions, RunOpts, SharedEvent, SnapshotPersister, SteerDispatch,
    SteerRunOpts, SubscribeOpts,
};
use paseo_provider_api::{AgentSession as _, AgentSessionConfig, PromptInput};
use paseo_timeline::ProjectedEntry;
use serde_json::{json, Value};

pub const AGENT_IDS: [&str; 4] = [
    "00000000-0000-4000-8000-000000000001",
    "00000000-0000-4000-8000-000000000002",
    "00000000-0000-4000-8000-000000000003",
    "00000000-0000-4000-8000-000000000004",
];

/// `waitForSessionEventQueue`: let every task make progress.
pub async fn settle() {
    for _ in 0..400 {
        tokio::task::yield_now().await;
    }
}

/// Advance the paused clock and let everything run (`vi.advanceTimersByTimeAsync`).
pub async fn advance(ms: u64) {
    tokio::time::advance(Duration::from_millis(ms)).await;
    settle().await;
}

#[derive(Default)]
pub struct HookLog {
    pub attention: Mutex<Vec<AttentionNotice>>,
    pub plugin_emits: Mutex<Vec<(String, Value)>>,
    pub workspace_cwds: Mutex<Vec<String>>,
    pub published_streams: Mutex<Vec<AgentStreamEvent>>,
    pub steer_fallbacks: Mutex<Vec<(String, String)>>,
    pub archived: Mutex<Vec<String>>,
    /// `(hook name, input)` of every plugin `before` hook.
    pub before_hooks: Mutex<Vec<(String, Value)>>,
}

/// Hooks that record everything and can hold the steer fallback.
pub struct RecordingHooks {
    pub log: Arc<HookLog>,
    pub fallback_entered: Gate,
    pub fallback_release: Gate,
    pub hold_fallback: bool,
    /// Report a plugin lifecycle (`pluginLifecycle` is set in TS), so `before` hooks run.
    pub plugin_lifecycle: bool,
}

impl RecordingHooks {
    pub fn new() -> (Self, Arc<HookLog>) {
        let log = Arc::new(HookLog::default());
        let release = Gate::new();
        release.open();
        (
            Self {
                log: log.clone(),
                fallback_entered: Gate::new(),
                fallback_release: release,
                hold_fallback: false,
                plugin_lifecycle: false,
            },
            log,
        )
    }
}

#[async_trait]
impl ManagerHooks for RecordingHooks {
    fn has_plugin_lifecycle(&self) -> bool {
        self.plugin_lifecycle
    }

    async fn plugin_before_create(&self, input: Value) -> Result<Value, String> {
        self.log
            .before_hooks
            .lock()
            .unwrap()
            .push(("agent.create".into(), input.clone()));
        Ok(input)
    }

    async fn plugin_before_session_open(&self, input: Value) -> Result<Value, String> {
        self.log
            .before_hooks
            .lock()
            .unwrap()
            .push(("agent.session_open".into(), input.clone()));
        Ok(input)
    }

    async fn on_archived(&self, agent_id: &str) -> Result<(), String> {
        self.log.archived.lock().unwrap().push(agent_id.to_owned());
        Ok(())
    }

    fn plugin_emit(&self, hook: &str, payload: Value) {
        self.log
            .plugin_emits
            .lock()
            .unwrap()
            .push((hook.to_owned(), payload));
    }

    fn publish_agent_stream(
        &self,
        _agent: &ManagedAgentView,
        event: &AgentStreamEvent,
        _timeline: &paseo_timeline::TimelineState,
    ) {
        self.log
            .published_streams
            .lock()
            .unwrap()
            .push(event.clone());
    }

    fn on_attention(&self, notice: AttentionNotice) {
        self.log.attention.lock().unwrap().push(notice);
    }

    fn on_workspace_state_may_have_changed(&self, cwd: &str) {
        self.log.workspace_cwds.lock().unwrap().push(cwd.to_owned());
    }

    async fn before_steer_unavailable_fallback(&self, agent_id: &str, expected_turn_id: &str) {
        self.log
            .steer_fallbacks
            .lock()
            .unwrap()
            .push((agent_id.to_owned(), expected_turn_id.to_owned()));
        self.fallback_entered.open();
        if self.hold_fallback {
            self.fallback_release.wait().await;
        }
    }
}

/// A persister that records every snapshot.
#[derive(Default)]
pub struct RecordingPersister {
    pub snapshots: Mutex<Vec<(ManagedAgentView, PersistOptions)>>,
    pub fail_with: Mutex<Option<String>>,
}

#[async_trait]
impl SnapshotPersister for RecordingPersister {
    async fn persist(
        &self,
        agent: &ManagedAgentView,
        options: PersistOptions,
    ) -> Result<(), String> {
        if let Some(message) = self.fail_with.lock().unwrap().clone() {
            return Err(message);
        }
        self.snapshots
            .lock()
            .unwrap()
            .push((agent.clone(), options));
        Ok(())
    }
}

/// Everything a subscription saw, in order.
#[derive(Clone, Default)]
pub struct EventLog(pub Arc<Mutex<Vec<SharedEvent>>>);

impl EventLog {
    pub fn all(&self) -> Vec<SharedEvent> {
        self.0.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    /// `agent_stream` events of one agent.
    pub fn streams(&self, agent_id: &str) -> Vec<SharedEvent> {
        self.all()
            .into_iter()
            .filter(
                |e| matches!(&**e, ManagerEvent::AgentStream { agent_id: a, .. } if a == agent_id),
            )
            .collect()
    }

    pub fn timeline_streams(&self, agent_id: &str) -> Vec<SharedEvent> {
        self.streams(agent_id)
            .into_iter()
            .filter(|e| {
                matches!(
                    &**e,
                    ManagerEvent::AgentStream {
                        event: AgentStreamEvent::Timeline { .. },
                        ..
                    }
                )
            })
            .collect()
    }

    /// `agent_state` events of one agent.
    pub fn states(&self, agent_id: &str) -> Vec<Arc<ManagedAgentView>> {
        self.all()
            .into_iter()
            .filter_map(|e| match &*e {
                ManagerEvent::AgentState { agent } if agent.id == agent_id => Some(agent.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }
}

pub fn stream_event(e: &SharedEvent) -> &AgentStreamEvent {
    match &**e {
        ManagerEvent::AgentStream { event, .. } => event,
        other => panic!("not an agent_stream event: {other:?}"),
    }
}

pub fn stream_seq(e: &SharedEvent) -> Option<i64> {
    match &**e {
        ManagerEvent::AgentStream { seq, .. } => *seq,
        _ => None,
    }
}

pub struct Harness {
    pub manager: Arc<AgentManager>,
    pub events: EventLog,
    pub hooks: Arc<HookLog>,
    pub persister: Arc<RecordingPersister>,
}

pub struct HarnessOptions {
    pub hooks: Option<RecordingHooks>,
    pub with_persister: bool,
    pub tweak: Box<dyn FnOnce(&mut ManagerOptions)>,
}

impl Default for HarnessOptions {
    fn default() -> Self {
        Self {
            hooks: None,
            with_persister: false,
            tweak: Box::new(|_| {}),
        }
    }
}

/// Spawn a task that collects every event of a subscription.
pub async fn collect(manager: &AgentManager, opts: SubscribeOpts) -> EventLog {
    let log = EventLog::default();
    let mut subscription = manager.subscribe(opts).await.expect("subscribe");
    let sink = log.0.clone();
    tokio::spawn(async move {
        while let Some(event) = subscription.next().await {
            sink.lock().unwrap().push(event);
        }
    });
    log
}

impl Harness {
    pub async fn new() -> Self {
        Self::with(HarnessOptions::default()).await
    }

    pub async fn with(options: HarnessOptions) -> Self {
        let (hooks, log) = match options.hooks {
            Some(hooks) => {
                let log = hooks.log.clone();
                (hooks, log)
            }
            None => RecordingHooks::new(),
        };
        let persister = Arc::new(RecordingPersister::default());
        let mut manager_options = ManagerOptions {
            hooks: Arc::new(hooks),
            persister: options
                .with_persister
                .then(|| persister.clone() as Arc<dyn SnapshotPersister>),
            ..ManagerOptions::default()
        };
        (options.tweak)(&mut manager_options);
        let manager = AgentManager::new(manager_options);
        let events = collect(&manager, SubscribeOpts::default().without_replay()).await;
        Self {
            manager,
            events,
            hooks: log,
            persister,
        }
    }

    /// `createAgent` with a ready session: register it under `id`.
    pub async fn agent(&self, id: &str, session: &MockSession) -> Arc<ManagedAgentView> {
        self.agent_with(id, session, "codex", RegisterOptions::default(), |_| {})
            .await
    }

    pub async fn agent_with(
        &self,
        id: &str,
        session: &MockSession,
        provider: &str,
        options: RegisterOptions,
        tweak: impl FnOnce(&mut AgentSessionConfig),
    ) -> Arc<ManagedAgentView> {
        let mut config = AgentSessionConfig::new(provider, "/tmp/paseo-core-test");
        tweak(&mut config);
        let options = RegisterOptions {
            history_primed: Some(true),
            ..options
        };
        self.manager
            .register_session(Box::new(session.clone()), config, id, options)
            .await
            .expect("register_session")
    }

    pub fn view(&self, id: &str) -> Arc<ManagedAgentView> {
        self.manager.get_agent(id).expect("agent is live")
    }

    pub async fn rows(&self, id: &str) -> Vec<ProjectedEntry> {
        self.manager.get_timeline_rows(id).await.expect("rows")
    }

    pub async fn items(&self, id: &str) -> Vec<AgentTimelineItem> {
        self.rows(id).await.into_iter().map(|r| r.item).collect()
    }

    /// Start a foreground run and keep draining it in the background.
    pub async fn start_background_run(
        &self,
        id: &str,
        prompt: &str,
    ) -> tokio::task::JoinHandle<()> {
        let mut stream = self
            .manager
            .stream_agent(id, PromptInput::from(prompt), RunOpts::default())
            .await
            .expect("stream_agent");
        let task = tokio::spawn(async move { while let Ok(Some(_)) = stream.next().await {} });
        self.manager
            .wait_for_agent_run_start(id, None)
            .await
            .expect("run starts");
        task
    }
}

pub fn assistant(text: &str, provider: &str, turn_id: Option<&str>) -> AgentStreamEvent {
    AgentStreamEvent::Timeline {
        item: Box::new(AgentTimelineItem::AssistantMessage {
            text: text.into(),
            message_id: None,
        }),
        provider: provider.into(),
        turn_id: turn_id.map(str::to_owned),
        timestamp: None,
    }
}

pub fn reasoning(text: &str, provider: &str, turn_id: Option<&str>) -> AgentStreamEvent {
    AgentStreamEvent::Timeline {
        item: Box::new(AgentTimelineItem::Reasoning { text: text.into() }),
        provider: provider.into(),
        turn_id: turn_id.map(str::to_owned),
        timestamp: None,
    }
}

pub fn timeline_event(
    item: AgentTimelineItem,
    provider: &str,
    turn_id: Option<&str>,
) -> AgentStreamEvent {
    AgentStreamEvent::Timeline {
        item: Box::new(item),
        provider: provider.into(),
        turn_id: turn_id.map(str::to_owned),
        timestamp: None,
    }
}

pub fn assistant_item(text: &str) -> AgentTimelineItem {
    AgentTimelineItem::AssistantMessage {
        text: text.into(),
        message_id: None,
    }
}

pub fn turn_started(turn: &str) -> AgentStreamEvent {
    AgentStreamEvent::TurnStarted {
        provider: "codex".into(),
        turn_id: Some(turn.into()),
    }
}

pub fn turn_completed(turn: &str) -> AgentStreamEvent {
    AgentStreamEvent::TurnCompleted {
        provider: "codex".into(),
        usage: None,
        turn_id: Some(turn.into()),
    }
}

pub fn turn_failed(turn: Option<&str>, error: &str) -> AgentStreamEvent {
    AgentStreamEvent::TurnFailed {
        provider: "codex".into(),
        error: error.into(),
        code: None,
        diagnostic: None,
        turn_id: turn.map(str::to_owned),
    }
}

pub fn turn_canceled(turn: Option<&str>, reason: &str) -> AgentStreamEvent {
    AgentStreamEvent::TurnCanceled {
        provider: "codex".into(),
        reason: reason.into(),
        turn_id: turn.map(str::to_owned),
    }
}

/// The TS `toolCall()` helper: a `shell` call printing `ok`.
pub fn tool_call(
    call_id: &str,
    status: ToolCallStatus,
    output: &str,
    error: Option<Value>,
) -> AgentTimelineItem {
    let detail = ToolCallDetail::Shell {
        command: "printf ok".into(),
        cwd: None,
        output: Some(output.into()),
        exit_code: Some(if status == ToolCallStatus::Completed {
            Some(0.into())
        } else {
            None
        }),
    };
    let item = if status == ToolCallStatus::Failed {
        ToolCallItem::failed(
            call_id,
            "shell",
            detail,
            error.unwrap_or_else(|| json!("failed")),
        )
    } else {
        ToolCallItem::new(call_id, "shell", status, detail)
    };
    AgentTimelineItem::tool_call(item)
}

pub fn running_tool(output: &str) -> AgentTimelineItem {
    tool_call("tool-1", ToolCallStatus::Running, output, None)
}

pub fn user_message_texts(items: &[AgentTimelineItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|i| match i {
            AgentTimelineItem::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// `startAgentRun` of `agent-prompt.ts` (steer, replace, stream) without out-of-band and the
/// stale-session reload (not part of this step).
#[derive(Default, Clone)]
pub struct StartRunOptions {
    pub replace_running: bool,
    pub steer: bool,
    pub run: RunOpts,
    pub clear_pending_permissions: bool,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Disposition {
    Steered,
    TurnStarted,
}

pub async fn start_agent_run(
    manager: &Arc<AgentManager>,
    agent_id: &str,
    prompt: &str,
    options: StartRunOptions,
) -> Result<Disposition, paseo_core::CoreError> {
    let prompt = PromptInput::from(prompt);
    let mut stream = None;
    if options.steer {
        let steer = SteerRunOpts {
            run: options.run.clone(),
            clear_pending_permissions: options.clear_pending_permissions,
        };
        match manager
            .steer_or_replace_active_turn(agent_id, prompt.clone(), steer)
            .await?
        {
            SteerDispatch::Steered => return Ok(Disposition::Steered),
            SteerDispatch::Replaced(s) => stream = Some(*s),
            SteerDispatch::Inactive => {}
        }
    }
    let mut stream = match stream {
        Some(stream) => stream,
        None => {
            let replace = options.replace_running && manager.has_in_flight_run(agent_id);
            if replace {
                manager
                    .replace_agent_run(agent_id, prompt.clone(), options.run.clone())
                    .await?
            } else {
                manager
                    .stream_agent(agent_id, prompt.clone(), options.run.clone())
                    .await?
            }
        }
    };
    tokio::spawn(async move { while let Ok(Some(_)) = stream.next().await {} });
    // TS starts the generator body synchronously; give the drain task the same head start.
    settle().await;
    Ok(Disposition::TurnStarted)
}

pub fn attention_reasons(log: &HookLog) -> Vec<AttentionReason> {
    log.attention
        .lock()
        .unwrap()
        .iter()
        .map(|n| n.reason)
        .collect()
}

// ---- steering sessions (`SteeringTestSession`, `UnsupportedSteeringSession`) ----

use paseo_core::mock::{boxed, ScriptedBehavior};
use paseo_provider_api::{ProviderError, SteerResult, TurnHandle};

#[derive(Clone, Debug)]
pub enum SteerMode {
    Accepted,
    Unavailable,
    Fail(String),
}

pub fn provider_event_turn_started(provider: &str, turn: &str) -> AgentStreamEvent {
    AgentStreamEvent::TurnStarted {
        provider: provider.into(),
        turn_id: Some(turn.into()),
    }
}

/// Turns are `active-turn-<n>`; `interrupt` cancels the current one; `steer` follows `mode`
/// and echoes the prompt back as the provider would.
pub fn steering_session(provider: &str) -> (MockSession, Arc<Mutex<SteerMode>>) {
    let mode = Arc::new(Mutex::new(SteerMode::Accepted));
    let steer_mode = mode.clone();
    let start_provider = provider.to_owned();
    let interrupt_provider = provider.to_owned();
    let behavior = ScriptedBehavior::new()
        .on_start(move |session, prompt, options| {
            let provider = start_provider.clone();
            boxed(async move {
                let n = session.record_start(prompt, options);
                let turn_id = format!("active-turn-{n}");
                session.push_event_soon(provider_event_turn_started(&provider, &turn_id));
                Ok(TurnHandle { turn_id })
            })
        })
        .on_interrupt(move |session| {
            let provider = interrupt_provider.clone();
            boxed(async move {
                session.with_state(|s| s.interrupt_count += 1);
                let turn = format!("active-turn-{}", session.start_count());
                session.push_event(AgentStreamEvent::TurnCanceled {
                    provider,
                    reason: "interrupted".into(),
                    turn_id: Some(turn),
                });
                Ok(())
            })
        })
        .on_steer(move |session, prompt, options| {
            let mode = steer_mode.lock().unwrap().clone();
            boxed(async move {
                session.with_state(|s| s.steer_count += 1);
                if options.expected_turn_id != format!("active-turn-{}", session.start_count()) {
                    return Ok(SteerResult::Unavailable);
                }
                match mode {
                    SteerMode::Fail(message) => return Err(ProviderError::provider(message)),
                    SteerMode::Unavailable => return Ok(SteerResult::Unavailable),
                    SteerMode::Accepted => {}
                }
                let client_message_id = options
                    .run
                    .get("clientMessageId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                session.push_event(AgentStreamEvent::Timeline {
                    item: Box::new(AgentTimelineItem::UserMessage {
                        text: match &prompt {
                            PromptInput::Text(text) => text.clone(),
                            PromptInput::Blocks(_) => "structured".into(),
                        },
                        message_id: client_message_id
                            .as_ref()
                            .map(|id| format!("provider-{id}")),
                        client_message_id,
                    }),
                    provider: session.provider().to_owned(),
                    turn_id: None,
                    timestamp: None,
                });
                Ok(SteerResult::Accepted)
            })
        });
    (MockSession::with_behavior(provider, behavior), mode)
}

/// A provider that cannot steer: every call answers `unavailable`.
pub fn unsupported_steering_session() -> MockSession {
    let behavior = ScriptedBehavior::new()
        .on_start(|session, prompt, options| {
            boxed(async move {
                let n = session.record_start(prompt, options);
                let turn_id = format!("unsupported-turn-{n}");
                session.push_event_soon(provider_event_turn_started("codex", &turn_id));
                Ok(TurnHandle { turn_id })
            })
        })
        .on_interrupt(|session| {
            boxed(async move {
                session.with_state(|s| s.interrupt_count += 1);
                let turn = format!("unsupported-turn-{}", session.start_count());
                session.push_event(AgentStreamEvent::TurnCanceled {
                    provider: "codex".into(),
                    reason: "interrupted".into(),
                    turn_id: Some(turn),
                });
                Ok(())
            })
        });
    MockSession::with_behavior("codex", behavior)
}

pub fn has_user_message(items: &[AgentTimelineItem], client_message_id: &str) -> bool {
    items.iter().any(|item| {
        matches!(item, AgentTimelineItem::UserMessage { client_message_id: Some(c), .. } if c == client_message_id)
    })
}

/// `startAndSteerThroughManager`: an agent with a running initial turn, then a replacement
/// prompt sent through `start_agent_run` with the given active-turn behavior.
pub async fn start_and_steer_through_manager(
    h: &Harness,
    session: &MockSession,
    provider: &str,
    steer: bool,
) -> &'static str {
    let id = AGENT_IDS[0];
    h.agent_with(id, session, provider, RegisterOptions::default(), |_| {})
        .await;
    h.start_background_run(id, "initial").await;
    start_agent_run(
        &h.manager,
        id,
        "replacement",
        StartRunOptions {
            replace_running: true,
            steer,
            run: RunOpts::with_client_message_id("replacement-client"),
            ..StartRunOptions::default()
        },
    )
    .await
    .expect("replacement dispatch");
    id
}

mod lc;
#[allow(unused_imports)]
pub use lc::*;
