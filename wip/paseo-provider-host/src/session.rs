use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::client::{Inner, Pending, SessionEvent};
use crate::error::Result;
use crate::wire::{
    AgentPersistenceHandle, AgentRunResult, PermissionRequest, PermissionResponse, PromptInput,
    SessionInfo, TurnStarted,
};

/// Events of one session, in host order. Events produced before this is taken are buffered.
/// The stream ends when the session closes or the host exits.
pub struct EventStream {
    rx: mpsc::UnboundedReceiver<SessionEvent>,
}

impl EventStream {
    pub(crate) fn new(rx: mpsc::UnboundedReceiver<SessionEvent>) -> Self {
        Self { rx }
    }

    pub async fn next(&mut self) -> Option<SessionEvent> {
        self.rx.recv().await
    }

    pub fn try_next(&mut self) -> Option<SessionEvent> {
        self.rx.try_recv().ok()
    }
}

/// Releases the host-side session when the last [`Session`] clone drops without `close()`.
struct SessionGuard {
    inner: Arc<Inner>,
    handle: String,
    closed: AtomicBool,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.inner.forget_session(&self.handle);
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let inner = Arc::clone(&self.inner);
        let handle = self.handle.clone();
        runtime.spawn(async move {
            let _ = inner
                .call::<Value>("session.close", json!({"handle": handle}))
                .await;
        });
    }
}

/// A provider session living inside the host (the TS `AgentSession`). Dropping the last clone
/// without calling [`Session::close`] closes the host-side session in the background.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
    info: SessionInfo,
    guard: Arc<SessionGuard>,
}

impl Session {
    pub(crate) fn new(inner: Arc<Inner>, info: SessionInfo) -> Self {
        let guard = Arc::new(SessionGuard {
            inner: Arc::clone(&inner),
            handle: info.handle.clone(),
            closed: AtomicBool::new(false),
        });
        Self { inner, info, guard }
    }

    pub fn info(&self) -> &SessionInfo {
        &self.info
    }

    pub fn handle(&self) -> &str {
        &self.info.handle
    }

    /// Take the event stream. Returns `None` after the first call.
    pub fn take_events(&self) -> Option<EventStream> {
        self.inner.take_events(&self.info.handle)
    }

    fn params(&self, extra: Value) -> Value {
        let mut params = json!({"handle": self.info.handle});
        if let (Some(base), Value::Object(extra)) = (params.as_object_mut(), extra) {
            base.extend(extra);
        }
        params
    }

    async fn call<T: serde::de::DeserializeOwned>(&self, method: &str, extra: Value) -> Result<T> {
        self.inner.call(method, self.params(extra)).await
    }

    /// Start `session.run` without waiting, so it can be canceled with [`Pending::cancel`].
    pub async fn run_cancellable(
        &self,
        prompt: impl Into<PromptInput>,
        options: Option<Value>,
    ) -> Result<Pending<AgentRunResult>> {
        let prompt: PromptInput = prompt.into();
        self.inner
            .request(
                "session.run",
                self.params(json!({"prompt": prompt, "options": options})),
            )
            .await
    }

    pub async fn run(
        &self,
        prompt: impl Into<PromptInput>,
        options: Option<Value>,
    ) -> Result<AgentRunResult> {
        self.run_cancellable(prompt, options).await?.wait().await
    }

    pub async fn start_turn(
        &self,
        prompt: impl Into<PromptInput>,
        options: Option<Value>,
    ) -> Result<TurnStarted> {
        let prompt: PromptInput = prompt.into();
        self.call(
            "session.startTurn",
            json!({"prompt": prompt, "options": options}),
        )
        .await
    }

    /// `{"status": "accepted" | "unavailable"}`.
    pub async fn steer_active_turn(
        &self,
        prompt: impl Into<PromptInput>,
        options: Value,
    ) -> Result<Value> {
        let prompt: PromptInput = prompt.into();
        self.call(
            "session.steerActiveTurn",
            json!({"prompt": prompt, "options": options}),
        )
        .await
    }

    pub async fn interrupt(&self) -> Result<()> {
        self.call::<Value>("session.interrupt", json!({}))
            .await
            .map(|_| ())
    }

    pub async fn stream_history(&self) -> Result<Vec<Value>> {
        #[derive(Deserialize)]
        struct R {
            events: Vec<Value>,
        }
        Ok(self
            .call::<R>("session.streamHistory", json!({}))
            .await?
            .events)
    }

    pub async fn runtime_info(&self) -> Result<Value> {
        self.call("session.getRuntimeInfo", json!({})).await
    }

    pub async fn available_modes(&self) -> Result<Vec<Value>> {
        #[derive(Deserialize)]
        struct R {
            modes: Vec<Value>,
        }
        Ok(self
            .call::<R>("session.getAvailableModes", json!({}))
            .await?
            .modes)
    }

    pub async fn current_mode(&self) -> Result<Option<String>> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct R {
            mode_id: Option<String>,
        }
        Ok(self
            .call::<R>("session.getCurrentMode", json!({}))
            .await?
            .mode_id)
    }

    /// Returns the provider notice, if any.
    pub async fn set_mode(&self, mode_id: &str) -> Result<Option<Value>> {
        self.notice("session.setMode", json!({"modeId": mode_id}))
            .await
    }

    pub async fn set_model(&self, model_id: Option<&str>) -> Result<()> {
        self.call::<Value>("session.setModel", json!({"modelId": model_id}))
            .await
            .map(|_| ())
    }

    pub async fn set_thinking_option(&self, option_id: Option<&str>) -> Result<Option<Value>> {
        self.notice(
            "session.setThinkingOption",
            json!({"thinkingOptionId": option_id}),
        )
        .await
    }

    pub async fn set_feature(&self, feature_id: &str, value: Value) -> Result<()> {
        self.call::<Value>(
            "session.setFeature",
            json!({"featureId": feature_id, "value": value}),
        )
        .await
        .map(|_| ())
    }

    async fn notice(&self, method: &str, extra: Value) -> Result<Option<Value>> {
        #[derive(Deserialize)]
        struct R {
            notice: Option<Value>,
        }
        Ok(self.call::<R>(method, extra).await?.notice)
    }

    pub async fn list_commands(&self) -> Result<Vec<Value>> {
        #[derive(Deserialize)]
        struct R {
            commands: Vec<Value>,
        }
        Ok(self
            .call::<R>("session.listCommands", json!({}))
            .await?
            .commands)
    }

    pub async fn pending_permissions(&self) -> Result<Vec<PermissionRequest>> {
        #[derive(Deserialize)]
        struct R {
            requests: Vec<PermissionRequest>,
        }
        Ok(self
            .call::<R>("session.getPendingPermissions", json!({}))
            .await?
            .requests)
    }

    /// Answer a permission request. Returns the provider's `AgentPermissionResult` (which may
    /// carry a `followUpPrompt`) when there is one.
    pub async fn respond_to_permission(
        &self,
        request_id: &str,
        response: &PermissionResponse,
    ) -> Result<Option<Value>> {
        #[derive(Deserialize)]
        struct R {
            result: Option<Value>,
        }
        let r: R = self
            .call(
                "session.respondToPermission",
                json!({"requestId": request_id, "response": response}),
            )
            .await?;
        Ok(r.result)
    }

    pub async fn describe_persistence(&self) -> Result<Option<AgentPersistenceHandle>> {
        #[derive(Deserialize)]
        struct R {
            persistence: Option<AgentPersistenceHandle>,
        }
        Ok(self
            .call::<R>("session.describePersistence", json!({}))
            .await?
            .persistence)
    }

    /// Revert to a message. `kind` is `conversation`, `files` or `both`.
    pub async fn revert(&self, kind: RevertKind, message_id: &str) -> Result<()> {
        let method = match kind {
            RevertKind::Conversation => "session.revertConversation",
            RevertKind::Files => "session.revertFiles",
            RevertKind::Both => "session.revertBoth",
        };
        self.call::<Value>(method, json!({"messageId": message_id}))
            .await
            .map(|_| ())
    }

    /// Release the session in the host. The handle is invalid afterwards.
    pub async fn close(&self) -> Result<()> {
        self.call::<Value>("session.close", json!({})).await?;
        self.guard.closed.store(true, Ordering::Release);
        self.inner.forget_session(&self.info.handle);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevertKind {
    Conversation,
    Files,
    Both,
}
