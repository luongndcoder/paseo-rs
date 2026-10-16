//! Test doubles for the provider seam (behind the `testing` feature): [`MockSession`] and
//! [`MockClient`].
//!
//! `MockSession` is the Rust analog of the TS `TestAgentSession` subclasses: the default
//! behavior is a quiet session, and a test overrides only the calls it cares about by
//! implementing [`MockBehavior`] (or scripting it with [`ScriptedBehavior`]). Events are pushed
//! synchronously with [`MockSession::push_event`]; like the TS test sessions, an event pushed
//! before the manager subscribes is lost (PARITY-QUIRK(pre-subscribe-events-lost)).

mod behavior;
mod client;

pub use behavior::{boxed, BoxFut, MockBehavior, QuietBehavior, ScriptedBehavior};
pub use client::{CreateRequest, MockClient, ResumeRequest};

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use async_trait::async_trait;
use paseo_agent_types::{
    AgentMode, AgentRuntimeInfo, AgentStreamEvent, AgentTimelineItem, PermissionRequest,
    PermissionResponse, PersistenceHandle,
};
use paseo_provider_api::{
    event_channel, AgentSession, EventSender, EventStream, JsonObject, PermissionResult,
    PromptInput, Result, RunOptions, SteerOptions, SteerResult, TurnHandle,
};
use serde_json::Value;
use tokio::sync::watch;

/// A one-shot latch: tests open it, mock behaviors wait on it (TS `deferred<void>()`).
#[derive(Debug, Clone)]
pub struct Gate {
    tx: Arc<watch::Sender<bool>>,
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub fn new() -> Self {
        Self {
            tx: Arc::new(watch::channel(false).0),
        }
    }

    pub fn open(&self) {
        self.tx.send_replace(true);
    }

    pub fn is_open(&self) -> bool {
        *self.tx.borrow()
    }

    pub async fn wait(&self) {
        let mut rx = self.tx.subscribe();
        let _ = rx.wait_for(|open| *open).await;
    }
}

/// Mutable, test-visible state of a [`MockSession`].
#[derive(Debug, Default)]
pub struct MockState {
    pub start_count: u32,
    pub interrupt_count: u32,
    pub steer_count: u32,
    pub start_prompts: Vec<PromptInput>,
    pub start_options: Vec<Option<RunOptions>>,
    pub runtime_model: Option<String>,
    pub mode_id: Option<String>,
    pub available_modes: Vec<AgentMode>,
    pub pending_permissions: Vec<PermissionRequest>,
    pub permission_responses: Vec<(String, PermissionResponse)>,
    pub history: Vec<AgentStreamEvent>,
    pub closed: bool,
    /// `None` hides persistence; the default is `{provider, sessionId}`.
    pub persistence_hidden: bool,
    /// Replaces the computed runtime info.
    pub runtime_info: Option<AgentRuntimeInfo>,
    /// Replaces the `{provider, sessionId}` persistence handle.
    pub persistence: Option<PersistenceHandle>,
}

struct Shared {
    provider: String,
    id: String,
    capabilities: JsonObject,
    config_model: Option<String>,
    behavior: Arc<dyn MockBehavior>,
    state: Mutex<MockState>,
    sender: Mutex<Option<EventSender>>,
    subscribed: Mutex<bool>,
    turn_counter: AtomicU32,
    initial_timeline: Vec<AgentTimelineItem>,
    /// Leaked on every `set_features` call (test-only): `features()` must return a borrow.
    features: Mutex<&'static [Value]>,
}

/// A scriptable provider session. Cheap to clone: clones share state, so a test keeps one and
/// hands another to the manager.
#[derive(Clone)]
pub struct MockSession {
    shared: Arc<Shared>,
}

impl MockSession {
    pub fn new(provider: &str) -> Self {
        Self::with_behavior(provider, QuietBehavior)
    }

    pub fn with_behavior(provider: &str, behavior: impl MockBehavior) -> Self {
        Self::build(provider, None, Arc::new(behavior), Vec::new())
    }

    fn build(
        provider: &str,
        config_model: Option<String>,
        behavior: Arc<dyn MockBehavior>,
        initial_timeline: Vec<AgentTimelineItem>,
    ) -> Self {
        Self::build_with(
            provider,
            config_model,
            None,
            JsonObject::new(),
            behavior,
            initial_timeline,
        )
    }

    fn build_with(
        provider: &str,
        config_model: Option<String>,
        config_mode: Option<String>,
        capabilities: JsonObject,
        behavior: Arc<dyn MockBehavior>,
        initial_timeline: Vec<AgentTimelineItem>,
    ) -> Self {
        let mode_id = config_mode.clone();
        Self {
            shared: Arc::new(Shared {
                provider: provider.to_owned(),
                id: uuid::Uuid::new_v4().to_string(),
                capabilities,
                config_model,
                behavior,
                state: Mutex::new(MockState {
                    mode_id,
                    ..MockState::default()
                }),
                sender: Mutex::new(None),
                subscribed: Mutex::new(false),
                turn_counter: AtomicU32::new(0),
                initial_timeline,
                features: Mutex::new(&[]),
            }),
        }
    }

    /// A builder for a session of `config`'s provider: model and mode come from the config like
    /// the TS `TestAgentSession`.
    pub fn builder(provider: &str) -> MockSessionBuilder {
        MockSessionBuilder::new(provider)
    }

    /// The runtime info computed from the session's state (model: the runtime model, else the
    /// config's; mode: the current mode).
    pub fn default_runtime_info(&self) -> AgentRuntimeInfo {
        let (model, mode) = self.with_state(|s| (s.runtime_model.clone(), s.mode_id.clone()));
        AgentRuntimeInfo {
            provider: self.shared.provider.clone(),
            session_id: Some(self.shared.id.clone()),
            model: Some(model.or_else(|| self.shared.config_model.clone())),
            thinking_option_id: None,
            mode_id: Some(mode),
            extra: None,
        }
    }

    /// The start of the TS `TestAgentSession`: count the start, then report `turn_started` and
    /// `turn_completed` one tick later and move the runtime model to `gpt-5.2-codex`.
    pub fn auto_start(&self, prompt: PromptInput, options: Option<RunOptions>) -> TurnHandle {
        let n = self.record_start(prompt, options);
        let turn_id = format!("turn-{n}");
        let provider = self.provider().to_owned();
        let started = AgentStreamEvent::TurnStarted {
            provider: provider.clone(),
            turn_id: Some(turn_id.clone()),
        };
        let completed = AgentStreamEvent::TurnCompleted {
            provider,
            usage: None,
            turn_id: Some(turn_id.clone()),
        };
        let soon = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::ZERO).await;
            soon.push_event(started);
            soon.push_event(completed);
            soon.with_state(|s| s.runtime_model = Some("gpt-5.2-codex".into()));
        });
        TurnHandle { turn_id }
    }

    /// Override the persistence handle the session describes.
    pub fn set_persistence(&self, handle: PersistenceHandle) {
        self.with_state(|s| s.persistence = Some(handle));
    }

    /// Session id the provider reports.
    pub fn session_id(&self) -> &str {
        &self.shared.id
    }

    pub fn with_state<R>(&self, f: impl FnOnce(&mut MockState) -> R) -> R {
        f(&mut self.shared.state.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn start_count(&self) -> u32 {
        self.with_state(|s| s.start_count)
    }

    pub fn interrupt_count(&self) -> u32 {
        self.with_state(|s| s.interrupt_count)
    }

    pub fn steer_count(&self) -> u32 {
        self.with_state(|s| s.steer_count)
    }

    pub fn start_prompts(&self) -> Vec<PromptInput> {
        self.with_state(|s| s.start_prompts.clone())
    }

    pub fn is_closed(&self) -> bool {
        self.with_state(|s| s.closed)
    }

    pub fn set_pending_permissions(&self, requests: Vec<PermissionRequest>) {
        self.with_state(|s| s.pending_permissions = requests);
    }

    /// Replace the features the session reports (leaks the old slice; test-only).
    pub fn set_features(&self, features: Vec<Value>) {
        *self
            .shared
            .features
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Box::leak(features.into_boxed_slice());
    }

    pub fn set_mode_id(&self, mode_id: Option<&str>) {
        self.with_state(|s| s.mode_id = mode_id.map(str::to_owned));
    }

    pub fn set_available_modes(&self, modes: Vec<AgentMode>) {
        self.with_state(|s| s.available_modes = modes);
    }

    /// `setTimeout(() => pushEvent(event), 0)`: deliver after the current call returns.
    pub fn push_event_soon(&self, event: AgentStreamEvent) {
        let session = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::ZERO).await;
            session.push_event(event);
        });
    }

    /// The TS `TestAgentSession`: every turn starts and completes by itself, one tick after
    /// `start_turn` returns.
    pub fn auto_completing(provider: &str) -> Self {
        let session = Self::with_behavior(
            provider,
            ScriptedBehavior::new().on_start(|session, prompt, options| {
                boxed(async move {
                    let n = session.record_start(prompt, options);
                    let turn_id = format!("turn-{n}");
                    let provider = session.provider().to_owned();
                    session.with_state(|s| s.runtime_model = Some("gpt-5.2-codex".into()));
                    let started = AgentStreamEvent::TurnStarted {
                        provider: provider.clone(),
                        turn_id: Some(turn_id.clone()),
                    };
                    let completed = AgentStreamEvent::TurnCompleted {
                        provider,
                        usage: None,
                        turn_id: Some(turn_id.clone()),
                    };
                    let soon = session.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::ZERO).await;
                        soon.push_event(started);
                        soon.push_event(completed);
                    });
                    Ok(TurnHandle { turn_id })
                })
            }),
        );
        session
    }

    pub fn set_history(&self, events: Vec<AgentStreamEvent>) {
        self.with_state(|s| s.history = events);
    }

    /// Count a `start_turn` and return its 1-based number.
    pub fn record_start(&self, prompt: PromptInput, options: Option<RunOptions>) -> u32 {
        self.with_state(|s| {
            s.start_count += 1;
            s.start_prompts.push(prompt);
            s.start_options.push(options);
            s.start_count
        })
    }

    /// A fresh turn number (independent of `start_count`).
    pub fn next_turn(&self) -> u32 {
        self.shared.turn_counter.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Deliver an event to the manager. Dropped when nobody is subscribed yet.
    ///
    /// # Panics
    /// When more than the channel capacity of events is queued unread.
    pub fn push_event(&self, event: AgentStreamEvent) {
        let sender = self
            .shared
            .sender
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(sender) = sender else {
            return;
        };
        // `unconstrained`: tokio's cooperative budget would report `Pending` after 128 sends.
        let mut send = std::pin::pin!(tokio::task::unconstrained(sender.send(event)));
        let mut cx = Context::from_waker(Waker::noop());
        match send.as_mut().poll(&mut cx) {
            Poll::Ready(_) => {}
            Poll::Pending => panic!("mock event channel is full"),
        }
    }
}

/// Capacity of the mock event channel: large enough for the 1000-event bursts of the TS tests.
const MOCK_EVENT_CAPACITY: usize = 1 << 15;

#[async_trait]
impl AgentSession for MockSession {
    fn provider(&self) -> &str {
        &self.shared.provider
    }

    fn id(&self) -> Option<&str> {
        Some(&self.shared.id)
    }

    fn capabilities(&self) -> &JsonObject {
        &self.shared.capabilities
    }

    fn features(&self) -> &[Value] {
        *self
            .shared
            .features
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn initial_timeline(&self) -> &[AgentTimelineItem] {
        &self.shared.initial_timeline
    }

    fn subscribe(&self) -> EventStream {
        let mut subscribed = self
            .shared
            .subscribed
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *subscribed {
            return EventStream::closed();
        }
        *subscribed = true;
        let (sender, stream) = event_channel(MOCK_EVENT_CAPACITY);
        *self.shared.sender.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender);
        stream
    }

    async fn start_turn(
        &self,
        prompt: PromptInput,
        options: Option<RunOptions>,
    ) -> Result<TurnHandle> {
        self.shared.behavior.start_turn(self, prompt, options).await
    }

    async fn steer_active_turn(
        &self,
        prompt: PromptInput,
        options: SteerOptions,
    ) -> Result<SteerResult> {
        self.shared
            .behavior
            .steer_active_turn(self, prompt, options)
            .await
    }

    async fn interrupt(&self) -> Result<()> {
        self.shared.behavior.interrupt(self).await
    }

    async fn respond_to_permission(
        &self,
        request_id: &str,
        response: PermissionResponse,
    ) -> Result<Option<PermissionResult>> {
        self.shared
            .behavior
            .respond_to_permission(self, request_id, response)
            .await
    }

    async fn stream_history(&self) -> Result<Vec<AgentStreamEvent>> {
        self.shared.behavior.stream_history(self).await
    }

    async fn try_handle_out_of_band(&self, prompt: PromptInput) -> Result<bool> {
        self.shared
            .behavior
            .try_handle_out_of_band(self, prompt)
            .await
    }

    async fn available_modes(&self) -> Result<Vec<AgentMode>> {
        Ok(self.with_state(|s| s.available_modes.clone()))
    }

    async fn current_mode(&self) -> Result<Option<String>> {
        Ok(self.with_state(|s| s.mode_id.clone()))
    }

    async fn set_mode(&self, mode_id: &str) -> Result<Option<Value>> {
        self.shared.behavior.set_mode(self, mode_id).await
    }

    async fn set_model(&self, model_id: Option<&str>) -> Result<()> {
        self.shared.behavior.set_model(self, model_id).await
    }

    async fn set_thinking_option(&self, option_id: Option<&str>) -> Result<Option<Value>> {
        self.shared
            .behavior
            .set_thinking_option(self, option_id)
            .await
    }

    async fn set_feature(&self, feature_id: &str, value: Value) -> Result<()> {
        self.shared
            .behavior
            .set_feature(self, feature_id, value)
            .await
    }

    async fn list_commands(&self) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }

    async fn revert_conversation(&self, message_id: &str) -> Result<()> {
        self.shared
            .behavior
            .revert(self, "conversation", message_id)
            .await
    }

    async fn revert_files(&self, message_id: &str) -> Result<()> {
        self.shared.behavior.revert(self, "files", message_id).await
    }

    async fn revert_both(&self, message_id: &str) -> Result<()> {
        self.shared.behavior.revert(self, "both", message_id).await
    }

    async fn describe_persistence(&self) -> Result<Option<PersistenceHandle>> {
        if self.with_state(|s| s.persistence_hidden) {
            return Ok(None);
        }
        if let Some(handle) = self.with_state(|s| s.persistence.clone()) {
            return Ok(Some(handle));
        }
        Ok(Some(PersistenceHandle {
            provider: self.shared.provider.clone(),
            session_id: self.shared.id.clone(),
            native_handle: None,
            metadata: None,
        }))
    }

    async fn get_runtime_info(&self) -> Result<AgentRuntimeInfo> {
        if let Some(info) = self.with_state(|s| s.runtime_info.clone()) {
            return Ok(info);
        }
        self.shared.behavior.get_runtime_info(self).await
    }

    async fn pending_permissions(&self) -> Result<Vec<PermissionRequest>> {
        Ok(self.with_state(|s| s.pending_permissions.clone()))
    }

    async fn close(&self) -> Result<()> {
        self.shared.behavior.close(self).await
    }
}

/// Builder of a [`MockSession`] with capabilities, config-derived model/mode and an initial
/// timeline.
pub struct MockSessionBuilder {
    provider: String,
    behavior: Arc<dyn MockBehavior>,
    capabilities: JsonObject,
    model: Option<String>,
    mode: Option<String>,
    initial_timeline: Vec<AgentTimelineItem>,
    auto_complete: bool,
}

impl MockSessionBuilder {
    fn new(provider: &str) -> Self {
        Self {
            provider: provider.to_owned(),
            behavior: Arc::new(QuietBehavior),
            capabilities: JsonObject::new(),
            model: None,
            mode: None,
            initial_timeline: Vec::new(),
            auto_complete: false,
        }
    }

    pub fn behavior(mut self, behavior: impl MockBehavior) -> Self {
        self.behavior = Arc::new(behavior);
        self
    }

    /// Every turn starts and completes by itself (the TS `TestAgentSession.startTurn`).
    pub fn auto_completing(mut self) -> Self {
        self.auto_complete = true;
        self
    }

    pub fn capability(mut self, key: &str, value: bool) -> Self {
        self.capabilities.insert(key.into(), Value::Bool(value));
        self
    }

    /// Model and mode of the session config (`config.model`, `config.modeId`).
    pub fn config(mut self, config: &paseo_provider_api::AgentSessionConfig) -> Self {
        self.model = config.model.clone();
        self.mode = config.mode_id.clone();
        self
    }

    pub fn initial_timeline(mut self, items: Vec<AgentTimelineItem>) -> Self {
        self.initial_timeline = items;
        self
    }

    pub fn build(self) -> MockSession {
        let behavior: Arc<dyn MockBehavior> = if self.auto_complete {
            Arc::new(AutoCompleting {
                inner: self.behavior,
            })
        } else {
            self.behavior
        };
        MockSession::build_with(
            &self.provider,
            self.model,
            self.mode,
            self.capabilities,
            behavior,
            self.initial_timeline,
        )
    }
}

/// Wraps a behavior so `start_turn` auto-completes one tick later unless the inner behavior was
/// given its own start.
struct AutoCompleting {
    inner: Arc<dyn MockBehavior>,
}

#[async_trait]
impl MockBehavior for AutoCompleting {
    async fn start_turn(
        &self,
        session: &MockSession,
        prompt: PromptInput,
        options: Option<RunOptions>,
    ) -> Result<TurnHandle> {
        Ok(session.auto_start(prompt, options))
    }

    async fn steer_active_turn(
        &self,
        session: &MockSession,
        prompt: PromptInput,
        options: SteerOptions,
    ) -> Result<SteerResult> {
        self.inner.steer_active_turn(session, prompt, options).await
    }

    async fn interrupt(&self, session: &MockSession) -> Result<()> {
        self.inner.interrupt(session).await
    }

    async fn respond_to_permission(
        &self,
        session: &MockSession,
        request_id: &str,
        response: PermissionResponse,
    ) -> Result<Option<PermissionResult>> {
        self.inner
            .respond_to_permission(session, request_id, response)
            .await
    }

    async fn close(&self, session: &MockSession) -> Result<()> {
        self.inner.close(session).await
    }

    async fn stream_history(&self, session: &MockSession) -> Result<Vec<AgentStreamEvent>> {
        self.inner.stream_history(session).await
    }

    async fn try_handle_out_of_band(
        &self,
        session: &MockSession,
        prompt: PromptInput,
    ) -> Result<bool> {
        self.inner.try_handle_out_of_band(session, prompt).await
    }

    async fn get_runtime_info(&self, session: &MockSession) -> Result<AgentRuntimeInfo> {
        self.inner.get_runtime_info(session).await
    }

    async fn set_model(&self, session: &MockSession, model_id: Option<&str>) -> Result<()> {
        self.inner.set_model(session, model_id).await
    }

    async fn set_thinking_option(
        &self,
        session: &MockSession,
        option_id: Option<&str>,
    ) -> Result<Option<Value>> {
        self.inner.set_thinking_option(session, option_id).await
    }

    async fn set_feature(
        &self,
        session: &MockSession,
        feature_id: &str,
        value: Value,
    ) -> Result<()> {
        self.inner.set_feature(session, feature_id, value).await
    }

    async fn set_mode(&self, session: &MockSession, mode_id: &str) -> Result<Option<Value>> {
        self.inner.set_mode(session, mode_id).await
    }

    async fn revert(&self, session: &MockSession, kind: &str, message_id: &str) -> Result<()> {
        self.inner.revert(session, kind, message_id).await
    }
}
