//! `paseo-provider-api` implementation over the TS provider host.
//!
//! One [`ProviderHostClient`] serves every provider of a daemon (it is owned by the core);
//! a [`HostAgentClient`] is a view of it for one provider id.

use std::sync::Mutex;

use async_trait::async_trait;
use paseo_agent_types::{
    AgentMode, AgentRuntimeInfo, AgentStreamEvent, AgentTimelineItem, PermissionRequest,
    PermissionResponse, PersistenceHandle,
};
use paseo_provider_api::{
    event_channel, AgentClient, AgentSession, AgentSessionConfig, CreateSessionOptions,
    EventStream, FetchCatalogOptions, ImportedSession, ImportedTimelineEntry, JsonObject,
    LaunchContext, PermissionResult, PromptInput, ProviderCatalog, ProviderError, Result,
    RunOptions, SessionOpenInfo, SteerOptions, SteerResult, TurnHandle, DEFAULT_EVENT_CAPACITY,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::client::{ProviderHostClient, SessionEvent};
use crate::error::{code, Error};
use crate::session::Session;

impl From<Error> for ProviderError {
    fn from(error: Error) -> Self {
        match error {
            Error::Rpc {
                code: rpc_code,
                message,
                ..
            } => match rpc_code {
                code::UNKNOWN_PROVIDER => Self::UnknownProvider(message),
                code::UNKNOWN_SESSION => Self::UnknownSession(message),
                code::SHUTTING_DOWN => Self::ShuttingDown,
                code::CANCELED => Self::Canceled,
                code::INVALID_REQUEST => Self::Unsupported(message),
                code::PROVIDER => Self::Provider { message },
                other => Self::Protocol(format!("host error {other}: {message}")),
            },
            Error::HostExited {
                reason,
                stderr_tail,
            } => Self::HostExited {
                reason,
                stderr_tail,
            },
            Error::Timeout(limit) => Self::Timeout(limit),
            Error::Json(error) => Self::Decode(error.to_string()),
            other => Self::Protocol(other.to_string()),
        }
    }
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|error| ProviderError::Decode(error.to_string()))
}

fn to_value<T: serde::Serialize>(value: &T) -> Result<Value> {
    serde_json::to_value(value).map_err(|error| ProviderError::Decode(error.to_string()))
}

fn decode_with<T, E: std::fmt::Display>(
    value: &Value,
    from: impl Fn(&Value) -> std::result::Result<T, E>,
) -> Result<T> {
    from(value).map_err(|error| ProviderError::Decode(error.to_string()))
}

fn decode_list<T, E: std::fmt::Display>(
    value: Option<&Value>,
    from: impl Fn(&Value) -> std::result::Result<T, E>,
) -> Result<Vec<T>> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().map(|item| decode_with(item, &from)).collect())
        .unwrap_or_else(|| Ok(Vec::new()))
}

/// `AgentClient` for one provider served by a provider host.
pub struct HostAgentClient {
    host: ProviderHostClient,
    provider: String,
}

impl HostAgentClient {
    /// `provider` is the provider id the host registry knows.
    pub fn new(host: ProviderHostClient, provider: impl Into<String>) -> Self {
        Self {
            host,
            provider: provider.into(),
        }
    }

    fn open(&self, session: Session) -> Result<Box<dyn AgentSession>> {
        Ok(Box::new(HostAgentSession::new(session)?))
    }
}

#[async_trait]
impl AgentClient for HostAgentClient {
    fn provider(&self) -> &str {
        &self.provider
    }

    async fn capabilities(&self) -> Result<JsonObject> {
        Ok(self.host.get_capabilities(&self.provider).await?)
    }

    async fn create_session(
        &self,
        config: &AgentSessionConfig,
        launch_context: Option<&LaunchContext>,
        options: Option<&CreateSessionOptions>,
    ) -> Result<Box<dyn AgentSession>> {
        let wire_config: crate::wire::AgentSessionConfig = decode(to_value(config)?)?;
        let session = self
            .host
            .create_session(
                &wire_config,
                launch_context.map(to_value).transpose()?,
                options.map(to_value).transpose()?,
            )
            .await?;
        self.open(session)
    }

    async fn resume_session(
        &self,
        handle: &PersistenceHandle,
        overrides: Option<&JsonObject>,
        launch_context: Option<&LaunchContext>,
        options: Option<&CreateSessionOptions>,
    ) -> Result<Box<dyn AgentSession>> {
        let persistence: crate::wire::AgentPersistenceHandle = decode(handle.to_value())?;
        let session = self
            .host
            .resume_session_with(
                &persistence,
                overrides,
                launch_context.map(to_value).transpose()?,
                options.map(to_value).transpose()?,
            )
            .await?;
        self.open(session)
    }

    async fn fetch_catalog(&self, options: &FetchCatalogOptions) -> Result<ProviderCatalog> {
        let value = self
            .host
            .call_raw(
                "provider.fetchCatalog",
                json!({"provider": self.provider, "options": options}),
            )
            .await?;
        decode(value)
    }

    async fn is_available(&self, options: Option<&FetchCatalogOptions>) -> Result<bool> {
        let value = self
            .host
            .call_raw(
                "provider.isAvailable",
                json!({"provider": self.provider, "options": options}),
            )
            .await?;
        Ok(value
            .get("available")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    async fn list_commands(&self, config: &AgentSessionConfig) -> Result<Vec<Value>> {
        Ok(self
            .host
            .list_commands(&self.provider, &to_value(config)?)
            .await?)
    }

    async fn list_features(&self, config: &AgentSessionConfig) -> Result<Vec<Value>> {
        Ok(self
            .host
            .list_features(&self.provider, &to_value(config)?)
            .await?)
    }

    async fn get_diagnostic(&self) -> Result<Option<Value>> {
        Ok(self.host.get_diagnostic(&self.provider).await?)
    }

    async fn list_importable_sessions(&self, options: Option<&JsonObject>) -> Result<Vec<Value>> {
        let options = options.cloned().map(Value::Object);
        Ok(self
            .host
            .list_importable_sessions(&self.provider, options)
            .await?)
    }

    async fn import_session(&self, input: &Value, context: &Value) -> Result<ImportedSession> {
        let imported = self
            .host
            .import_session(&self.provider, input, context)
            .await?;
        let timeline = imported
            .timeline
            .iter()
            .map(|entry| {
                let item = entry
                    .get("item")
                    .ok_or_else(|| ProviderError::Decode("timeline entry without item".into()))?;
                Ok(ImportedTimelineEntry {
                    item: decode_with(item, AgentTimelineItem::from_value)?,
                    timestamp: entry
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let provider_subagent_events = imported
            .provider_subagent_events
            .iter()
            .map(|event| decode_with(event, AgentStreamEvent::from_value))
            .collect::<Result<Vec<_>>>()?;
        Ok(ImportedSession {
            session: self.open(imported.session)?,
            config: decode(imported.config)?,
            persistence: decode_with(
                &to_value(&imported.persistence)?,
                PersistenceHandle::from_value,
            )?,
            timeline,
            provider_subagent_events,
        })
    }

    async fn archive_native_session(&self, persistence: &PersistenceHandle) -> Result<()> {
        let persistence: crate::wire::AgentPersistenceHandle = decode(persistence.to_value())?;
        Ok(self.host.archive_native_session(&persistence).await?)
    }

    async fn unarchive_native_session(&self, persistence: &PersistenceHandle) -> Result<()> {
        let persistence: crate::wire::AgentPersistenceHandle = decode(persistence.to_value())?;
        Ok(self.host.unarchive_native_session(&persistence).await?)
    }

    /// No-op: the host is shared by every provider and shut down by its owner
    /// ([`ProviderHostClient::shutdown`]).
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

/// `AgentSession` backed by a host session handle.
pub struct HostAgentSession {
    session: Session,
    provider: String,
    session_id: Option<String>,
    info: SessionOpenInfo,
    events: Mutex<Option<EventStream>>,
}

impl HostAgentSession {
    /// Wrap a host session and start forwarding its events into a bounded channel.
    ///
    /// Host events are decoded into the typed model; one that does not decode is logged and
    /// skipped. Synthetic terminal events from the client (host exit, sequence gap) are
    /// forwarded like any other. Delivery awaits a full channel and never drops.
    pub fn new(session: Session) -> Result<Self> {
        let host_info = session.info();
        let info = SessionOpenInfo {
            capabilities: host_info.capabilities.clone(),
            features: host_info.features.clone(),
            initial_timeline: host_info
                .initial_timeline
                .iter()
                .map(|item| decode_with(item, AgentTimelineItem::from_value))
                .collect::<Result<_>>()?,
        };
        let provider = host_info.provider.clone();
        let session_id = host_info.session_id.clone();
        let events = match session.take_events() {
            Some(mut host_events) => {
                let (sender, stream) = event_channel(DEFAULT_EVENT_CAPACITY);
                let handle = session.handle().to_owned();
                let provider_name = provider.clone();
                tokio::spawn(async move {
                    while let Some(SessionEvent { raw, .. }) = host_events.next().await {
                        match AgentStreamEvent::from_value(&raw) {
                            Ok(event) => {
                                if sender.send(event).await.is_err() {
                                    break;
                                }
                            }
                            Err(error) => {
                                let kind = raw.get("type").and_then(Value::as_str).unwrap_or("");
                                if !matches!(
                                    kind,
                                    "turn_completed"
                                        | "turn_failed"
                                        | "turn_canceled"
                                        | "timeline"
                                        | "permission_requested"
                                ) {
                                    tracing::warn!(%handle, %error, kind, "skipping undecodable host event");
                                    continue;
                                }
                                // Dropping a terminal or content event would hang or corrupt the
                                // turn, so it becomes a synthetic failure carrying the raw turnId.
                                tracing::error!(%handle, %error, kind, "undecodable host event; failing turn");
                                let failed = AgentStreamEvent::TurnFailed {
                                    provider: provider_name.clone(),
                                    error: format!("Undecodable provider event ({kind}): {error}"),
                                    code: Some("provider_host_decode".to_owned()),
                                    diagnostic: Some(raw.to_string()),
                                    turn_id: raw
                                        .get("turnId")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned),
                                };
                                if sender.send(failed).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
                stream
            }
            None => EventStream::closed(),
        };
        Ok(Self {
            session,
            provider,
            session_id,
            info,
            events: Mutex::new(Some(events)),
        })
    }

    /// The underlying host session (handle, raw calls).
    pub fn host_session(&self) -> &Session {
        &self.session
    }
}

#[async_trait]
impl AgentSession for HostAgentSession {
    fn provider(&self) -> &str {
        &self.provider
    }

    fn id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    fn capabilities(&self) -> &JsonObject {
        &self.info.capabilities
    }

    fn features(&self) -> &[Value] {
        &self.info.features
    }

    fn initial_timeline(&self) -> &[AgentTimelineItem] {
        &self.info.initial_timeline
    }

    fn subscribe(&self) -> EventStream {
        let taken = self.events.lock().expect("events lock").take();
        taken.unwrap_or_else(|| {
            tracing::warn!(handle = self.session.handle(), "session subscribed twice");
            EventStream::closed()
        })
    }

    async fn start_turn(
        &self,
        prompt: PromptInput,
        options: Option<RunOptions>,
    ) -> Result<TurnHandle> {
        let options = options.map(Value::Object);
        let started = self
            .session
            .start_turn(prompt_to_host(prompt)?, options)
            .await?;
        Ok(TurnHandle {
            turn_id: started.turn_id,
        })
    }

    async fn steer_active_turn(
        &self,
        prompt: PromptInput,
        options: SteerOptions,
    ) -> Result<SteerResult> {
        let value = self
            .session
            .steer_active_turn(prompt_to_host(prompt)?, to_value(&options)?)
            .await?;
        match value.get("status").and_then(Value::as_str) {
            Some("accepted") => Ok(SteerResult::Accepted),
            Some("unavailable") => Ok(SteerResult::Unavailable),
            other => Err(ProviderError::Protocol(format!(
                "unexpected steer status {other:?}"
            ))),
        }
    }

    async fn interrupt(&self) -> Result<()> {
        Ok(self.session.interrupt().await?)
    }

    async fn respond_to_permission(
        &self,
        request_id: &str,
        response: PermissionResponse,
    ) -> Result<Option<PermissionResult>> {
        let value = self
            .session
            .call_raw(
                "session.respondToPermission",
                json!({"requestId": request_id, "response": response.to_value()}),
            )
            .await?;
        match value.get("result") {
            None | Some(Value::Null) => Ok(None),
            Some(result) => Ok(Some(decode(result.clone())?)),
        }
    }

    // PARITY-QUIRK(stream-history-single-frame): TS collects history into one array frame.
    async fn stream_history(&self) -> Result<Vec<AgentStreamEvent>> {
        let value = self
            .session
            .call_raw("session.streamHistory", json!({}))
            .await?;
        decode_list(value.get("events"), AgentStreamEvent::from_value)
    }

    async fn try_handle_out_of_band(&self, prompt: PromptInput) -> Result<bool> {
        Ok(self
            .session
            .try_handle_out_of_band(prompt_to_host(prompt)?)
            .await?)
    }

    async fn available_modes(&self) -> Result<Vec<AgentMode>> {
        let value = self
            .session
            .call_raw("session.getAvailableModes", json!({}))
            .await?;
        decode_list(value.get("modes"), AgentMode::from_value)
    }

    async fn current_mode(&self) -> Result<Option<String>> {
        Ok(self.session.current_mode().await?)
    }

    async fn set_mode(&self, mode_id: &str) -> Result<Option<Value>> {
        Ok(self.session.set_mode(mode_id).await?)
    }

    async fn set_model(&self, model_id: Option<&str>) -> Result<()> {
        Ok(self.session.set_model(model_id).await?)
    }

    async fn set_thinking_option(&self, option_id: Option<&str>) -> Result<Option<Value>> {
        Ok(self.session.set_thinking_option(option_id).await?)
    }

    async fn set_feature(&self, feature_id: &str, value: Value) -> Result<()> {
        Ok(self.session.set_feature(feature_id, value).await?)
    }

    async fn list_commands(&self) -> Result<Vec<Value>> {
        Ok(self.session.list_commands().await?)
    }

    async fn revert_conversation(&self, message_id: &str) -> Result<()> {
        Ok(self
            .session
            .revert(crate::session::RevertKind::Conversation, message_id)
            .await?)
    }

    async fn revert_files(&self, message_id: &str) -> Result<()> {
        Ok(self
            .session
            .revert(crate::session::RevertKind::Files, message_id)
            .await?)
    }

    async fn revert_both(&self, message_id: &str) -> Result<()> {
        Ok(self
            .session
            .revert(crate::session::RevertKind::Both, message_id)
            .await?)
    }

    async fn describe_persistence(&self) -> Result<Option<PersistenceHandle>> {
        let value = self
            .session
            .call_raw("session.describePersistence", json!({}))
            .await?;
        match value.get("persistence") {
            None | Some(Value::Null) => Ok(None),
            Some(handle) => Ok(Some(decode_with(handle, PersistenceHandle::from_value)?)),
        }
    }

    async fn get_runtime_info(&self) -> Result<AgentRuntimeInfo> {
        let value = self
            .session
            .call_raw("session.getRuntimeInfo", json!({}))
            .await?;
        decode_with(&value, AgentRuntimeInfo::from_value)
    }

    async fn pending_permissions(&self) -> Result<Vec<PermissionRequest>> {
        let value = self
            .session
            .call_raw("session.getPendingPermissions", json!({}))
            .await?;
        decode_list(value.get("requests"), PermissionRequest::from_value)
    }

    async fn close(&self) -> Result<()> {
        Ok(self.session.close().await?)
    }
}

fn prompt_to_host(prompt: PromptInput) -> Result<crate::wire::PromptInput> {
    decode(to_value(&prompt)?)
}
