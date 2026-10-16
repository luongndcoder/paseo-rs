//! Overridable provider calls of a [`MockSession`](super::MockSession).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use paseo_agent_types::{AgentRuntimeInfo, AgentStreamEvent, PermissionResponse};
use paseo_provider_api::{
    PermissionResult, PromptInput, ProviderError, Result, RunOptions, SteerOptions, SteerResult,
    TurnHandle,
};
use serde_json::Value;

use super::MockSession;

/// Overridable provider calls. Every method has a quiet default.
#[async_trait]
pub trait MockBehavior: Send + Sync + 'static {
    /// Default: count the call and answer `turn-<n>`; no events.
    async fn start_turn(
        &self,
        session: &MockSession,
        prompt: PromptInput,
        options: Option<RunOptions>,
    ) -> Result<TurnHandle> {
        let n = session.record_start(prompt, options);
        Ok(TurnHandle {
            turn_id: format!("turn-{n}"),
        })
    }

    /// Default: the provider cannot steer.
    async fn steer_active_turn(
        &self,
        session: &MockSession,
        _prompt: PromptInput,
        _options: SteerOptions,
    ) -> Result<SteerResult> {
        session.with_state(|s| s.steer_count += 1);
        Ok(SteerResult::Unavailable)
    }

    /// Default: count the call and acknowledge.
    async fn interrupt(&self, session: &MockSession) -> Result<()> {
        session.with_state(|s| s.interrupt_count += 1);
        Ok(())
    }

    async fn respond_to_permission(
        &self,
        session: &MockSession,
        request_id: &str,
        response: PermissionResponse,
    ) -> Result<Option<PermissionResult>> {
        session.with_state(|s| {
            s.permission_responses
                .push((request_id.to_owned(), response));
        });
        Ok(None)
    }

    async fn close(&self, session: &MockSession) -> Result<()> {
        session.with_state(|s| s.closed = true);
        Ok(())
    }

    /// Default: the history the test set with `MockSession::set_history`.
    async fn stream_history(&self, session: &MockSession) -> Result<Vec<AgentStreamEvent>> {
        Ok(session.with_state(|s| s.history.clone()))
    }

    /// Default: nothing is handled out of band.
    async fn try_handle_out_of_band(
        &self,
        _session: &MockSession,
        _prompt: PromptInput,
    ) -> Result<bool> {
        Ok(false)
    }

    /// Default: the runtime info the session computes from its state.
    async fn get_runtime_info(&self, session: &MockSession) -> Result<AgentRuntimeInfo> {
        Ok(session.default_runtime_info())
    }

    async fn set_model(&self, session: &MockSession, model_id: Option<&str>) -> Result<()> {
        session.with_state(|s| s.runtime_model = model_id.map(str::to_owned));
        Ok(())
    }

    async fn set_thinking_option(
        &self,
        _session: &MockSession,
        _option_id: Option<&str>,
    ) -> Result<Option<Value>> {
        Ok(None)
    }

    /// Default: the session has no settable features.
    async fn set_feature(
        &self,
        _session: &MockSession,
        _feature_id: &str,
        _value: Value,
    ) -> Result<()> {
        Err(ProviderError::Unsupported("set_feature".into()))
    }

    async fn set_mode(&self, session: &MockSession, mode_id: &str) -> Result<Option<Value>> {
        session.with_state(|s| s.mode_id = Some(mode_id.to_owned()));
        Ok(None)
    }

    /// Default: the session does not support rewinding.
    async fn revert(&self, _session: &MockSession, kind: &str, _message_id: &str) -> Result<()> {
        Err(ProviderError::Unsupported(format!("revert_{kind}")))
    }
}

/// The all-defaults behavior.
#[derive(Debug, Default)]
pub struct QuietBehavior;

impl MockBehavior for QuietBehavior {}

/// A boxed, sendable future (what scripted behaviors return).
pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

pub fn boxed<T>(future: impl Future<Output = T> + Send + 'static) -> BoxFut<T> {
    Box::pin(future)
}

type StartFn = Arc<
    dyn Fn(MockSession, PromptInput, Option<RunOptions>) -> BoxFut<Result<TurnHandle>>
        + Send
        + Sync,
>;
type SteerFn = Arc<
    dyn Fn(MockSession, PromptInput, SteerOptions) -> BoxFut<Result<SteerResult>> + Send + Sync,
>;
type InterruptFn = Arc<dyn Fn(MockSession) -> BoxFut<Result<()>> + Send + Sync>;
type RespondFn = Arc<
    dyn Fn(MockSession, String, PermissionResponse) -> BoxFut<Result<Option<PermissionResult>>>
        + Send
        + Sync,
>;
type CloseFn = Arc<dyn Fn(MockSession) -> BoxFut<Result<()>> + Send + Sync>;
type HistoryFn = Arc<dyn Fn(MockSession) -> BoxFut<Result<Vec<AgentStreamEvent>>> + Send + Sync>;
type OutOfBandFn = Arc<dyn Fn(MockSession, PromptInput) -> BoxFut<Result<bool>> + Send + Sync>;
type RuntimeInfoFn = Arc<dyn Fn(MockSession) -> BoxFut<Result<AgentRuntimeInfo>> + Send + Sync>;
type SetModelFn = Arc<dyn Fn(MockSession, Option<String>) -> BoxFut<Result<()>> + Send + Sync>;
type SetThinkingFn =
    Arc<dyn Fn(MockSession, Option<String>) -> BoxFut<Result<Option<Value>>> + Send + Sync>;
type SetFeatureFn = Arc<dyn Fn(MockSession, String, Value) -> BoxFut<Result<()>> + Send + Sync>;
type SetModeFn = Arc<dyn Fn(MockSession, String) -> BoxFut<Result<Option<Value>>> + Send + Sync>;
type RevertFn = Arc<dyn Fn(MockSession, String, String) -> BoxFut<Result<()>> + Send + Sync>;

/// Closure-based [`MockBehavior`]: override only the calls a test needs; the rest keep the
/// quiet defaults.
#[derive(Default)]
pub struct ScriptedBehavior {
    start: Option<StartFn>,
    steer: Option<SteerFn>,
    interrupt: Option<InterruptFn>,
    respond: Option<RespondFn>,
    close: Option<CloseFn>,
    history: Option<HistoryFn>,
    out_of_band: Option<OutOfBandFn>,
    runtime_info: Option<RuntimeInfoFn>,
    set_model: Option<SetModelFn>,
    set_thinking: Option<SetThinkingFn>,
    set_feature: Option<SetFeatureFn>,
    set_mode: Option<SetModeFn>,
    revert: Option<RevertFn>,
}

impl ScriptedBehavior {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_start(
        mut self,
        f: impl Fn(MockSession, PromptInput, Option<RunOptions>) -> BoxFut<Result<TurnHandle>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.start = Some(Arc::new(f));
        self
    }

    pub fn on_steer(
        mut self,
        f: impl Fn(MockSession, PromptInput, SteerOptions) -> BoxFut<Result<SteerResult>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.steer = Some(Arc::new(f));
        self
    }

    pub fn on_interrupt(
        mut self,
        f: impl Fn(MockSession) -> BoxFut<Result<()>> + Send + Sync + 'static,
    ) -> Self {
        self.interrupt = Some(Arc::new(f));
        self
    }

    pub fn on_respond(
        mut self,
        f: impl Fn(MockSession, String, PermissionResponse) -> BoxFut<Result<Option<PermissionResult>>>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.respond = Some(Arc::new(f));
        self
    }

    pub fn on_close(
        mut self,
        f: impl Fn(MockSession) -> BoxFut<Result<()>> + Send + Sync + 'static,
    ) -> Self {
        self.close = Some(Arc::new(f));
        self
    }

    pub fn on_history(
        mut self,
        f: impl Fn(MockSession) -> BoxFut<Result<Vec<AgentStreamEvent>>> + Send + Sync + 'static,
    ) -> Self {
        self.history = Some(Arc::new(f));
        self
    }

    pub fn on_out_of_band(
        mut self,
        f: impl Fn(MockSession, PromptInput) -> BoxFut<Result<bool>> + Send + Sync + 'static,
    ) -> Self {
        self.out_of_band = Some(Arc::new(f));
        self
    }

    pub fn on_runtime_info(
        mut self,
        f: impl Fn(MockSession) -> BoxFut<Result<AgentRuntimeInfo>> + Send + Sync + 'static,
    ) -> Self {
        self.runtime_info = Some(Arc::new(f));
        self
    }

    pub fn on_set_model(
        mut self,
        f: impl Fn(MockSession, Option<String>) -> BoxFut<Result<()>> + Send + Sync + 'static,
    ) -> Self {
        self.set_model = Some(Arc::new(f));
        self
    }

    pub fn on_set_thinking(
        mut self,
        f: impl Fn(MockSession, Option<String>) -> BoxFut<Result<Option<Value>>> + Send + Sync + 'static,
    ) -> Self {
        self.set_thinking = Some(Arc::new(f));
        self
    }

    pub fn on_set_feature(
        mut self,
        f: impl Fn(MockSession, String, Value) -> BoxFut<Result<()>> + Send + Sync + 'static,
    ) -> Self {
        self.set_feature = Some(Arc::new(f));
        self
    }

    pub fn on_set_mode(
        mut self,
        f: impl Fn(MockSession, String) -> BoxFut<Result<Option<Value>>> + Send + Sync + 'static,
    ) -> Self {
        self.set_mode = Some(Arc::new(f));
        self
    }

    /// `kind` is `conversation`, `files` or `both`.
    pub fn on_revert(
        mut self,
        f: impl Fn(MockSession, String, String) -> BoxFut<Result<()>> + Send + Sync + 'static,
    ) -> Self {
        self.revert = Some(Arc::new(f));
        self
    }
}

#[async_trait]
impl MockBehavior for ScriptedBehavior {
    async fn start_turn(
        &self,
        session: &MockSession,
        prompt: PromptInput,
        options: Option<RunOptions>,
    ) -> Result<TurnHandle> {
        match &self.start {
            Some(f) => f(session.clone(), prompt, options).await,
            None => QuietBehavior.start_turn(session, prompt, options).await,
        }
    }

    async fn steer_active_turn(
        &self,
        session: &MockSession,
        prompt: PromptInput,
        options: SteerOptions,
    ) -> Result<SteerResult> {
        match &self.steer {
            Some(f) => f(session.clone(), prompt, options).await,
            None => {
                QuietBehavior
                    .steer_active_turn(session, prompt, options)
                    .await
            }
        }
    }

    async fn interrupt(&self, session: &MockSession) -> Result<()> {
        match &self.interrupt {
            Some(f) => f(session.clone()).await,
            None => QuietBehavior.interrupt(session).await,
        }
    }

    async fn respond_to_permission(
        &self,
        session: &MockSession,
        request_id: &str,
        response: PermissionResponse,
    ) -> Result<Option<PermissionResult>> {
        match &self.respond {
            Some(f) => f(session.clone(), request_id.to_owned(), response).await,
            None => {
                QuietBehavior
                    .respond_to_permission(session, request_id, response)
                    .await
            }
        }
    }

    async fn close(&self, session: &MockSession) -> Result<()> {
        match &self.close {
            Some(f) => f(session.clone()).await,
            None => QuietBehavior.close(session).await,
        }
    }

    async fn stream_history(&self, session: &MockSession) -> Result<Vec<AgentStreamEvent>> {
        match &self.history {
            Some(f) => f(session.clone()).await,
            None => QuietBehavior.stream_history(session).await,
        }
    }

    async fn try_handle_out_of_band(
        &self,
        session: &MockSession,
        prompt: PromptInput,
    ) -> Result<bool> {
        match &self.out_of_band {
            Some(f) => f(session.clone(), prompt).await,
            None => Ok(false),
        }
    }

    async fn get_runtime_info(&self, session: &MockSession) -> Result<AgentRuntimeInfo> {
        match &self.runtime_info {
            Some(f) => f(session.clone()).await,
            None => QuietBehavior.get_runtime_info(session).await,
        }
    }

    async fn set_model(&self, session: &MockSession, model_id: Option<&str>) -> Result<()> {
        match &self.set_model {
            Some(f) => f(session.clone(), model_id.map(str::to_owned)).await,
            None => QuietBehavior.set_model(session, model_id).await,
        }
    }

    async fn set_thinking_option(
        &self,
        session: &MockSession,
        option_id: Option<&str>,
    ) -> Result<Option<Value>> {
        match &self.set_thinking {
            Some(f) => f(session.clone(), option_id.map(str::to_owned)).await,
            None => QuietBehavior.set_thinking_option(session, option_id).await,
        }
    }

    async fn set_feature(
        &self,
        session: &MockSession,
        feature_id: &str,
        value: Value,
    ) -> Result<()> {
        match &self.set_feature {
            Some(f) => f(session.clone(), feature_id.to_owned(), value).await,
            None => QuietBehavior.set_feature(session, feature_id, value).await,
        }
    }

    async fn set_mode(&self, session: &MockSession, mode_id: &str) -> Result<Option<Value>> {
        match &self.set_mode {
            Some(f) => f(session.clone(), mode_id.to_owned()).await,
            None => QuietBehavior.set_mode(session, mode_id).await,
        }
    }

    async fn revert(&self, session: &MockSession, kind: &str, message_id: &str) -> Result<()> {
        match &self.revert {
            Some(f) => f(session.clone(), kind.to_owned(), message_id.to_owned()).await,
            None => QuietBehavior.revert(session, kind, message_id).await,
        }
    }
}
