use async_trait::async_trait;
use paseo_agent_types::{
    AgentMode, AgentRuntimeInfo, AgentStreamEvent, AgentTimelineItem, PermissionRequest,
    PermissionResponse, PersistenceHandle,
};
use serde_json::Value;

use crate::error::Result;
use crate::events::EventStream;
use crate::types::{
    AgentSessionConfig, CreateSessionOptions, FetchCatalogOptions, ImportedSession, JsonObject,
    LaunchContext, PermissionResult, PromptInput, ProviderCatalog, RunOptions, SteerOptions,
    SteerResult, ToolCallRequest, ToolCallResult, TurnHandle,
};

/// The TS `AgentClient`. One per provider.
#[async_trait]
pub trait AgentClient: Send + Sync {
    fn provider(&self) -> &str;

    async fn capabilities(&self) -> Result<JsonObject>;

    async fn create_session(
        &self,
        config: &AgentSessionConfig,
        launch_context: Option<&LaunchContext>,
        options: Option<&CreateSessionOptions>,
    ) -> Result<Box<dyn AgentSession>>;

    async fn resume_session(
        &self,
        handle: &PersistenceHandle,
        overrides: Option<&JsonObject>,
        launch_context: Option<&LaunchContext>,
        options: Option<&CreateSessionOptions>,
    ) -> Result<Box<dyn AgentSession>>;

    /// Raw catalog. Caching is the caller's (core's) job.
    async fn fetch_catalog(&self, options: &FetchCatalogOptions) -> Result<ProviderCatalog>;

    async fn is_available(&self, options: Option<&FetchCatalogOptions>) -> Result<bool>;

    async fn list_commands(&self, _config: &AgentSessionConfig) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }

    async fn list_features(&self, _config: &AgentSessionConfig) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }

    async fn get_diagnostic(&self) -> Result<Option<Value>> {
        Ok(None)
    }

    async fn list_importable_sessions(&self, _options: Option<&JsonObject>) -> Result<Vec<Value>> {
        Ok(Vec::new())
    }

    async fn import_session(&self, _input: &Value, _context: &Value) -> Result<ImportedSession> {
        Err(crate::ProviderError::Unsupported(format!(
            "provider {} cannot import sessions",
            self.provider()
        )))
    }

    async fn archive_native_session(&self, _persistence: &PersistenceHandle) -> Result<()> {
        Ok(())
    }

    async fn unarchive_native_session(&self, _persistence: &PersistenceHandle) -> Result<()> {
        Ok(())
    }

    async fn shutdown(&self) -> Result<()>;
}

/// The TS `AgentSession`. `run` is not a method: see [`crate::run_provider_turn`].
#[async_trait]
pub trait AgentSession: Send + Sync {
    fn provider(&self) -> &str;

    /// Provider session id, once known.
    fn id(&self) -> Option<&str>;

    fn capabilities(&self) -> &JsonObject;

    fn features(&self) -> &[Value];

    fn initial_timeline(&self) -> &[AgentTimelineItem];

    /// Take the event stream. Single consumer: later calls return a finished stream.
    fn subscribe(&self) -> EventStream;

    async fn start_turn(
        &self,
        prompt: PromptInput,
        options: Option<RunOptions>,
    ) -> Result<TurnHandle>;

    /// `Unavailable` when the provider cannot steer.
    async fn steer_active_turn(
        &self,
        prompt: PromptInput,
        options: SteerOptions,
    ) -> Result<SteerResult>;

    async fn interrupt(&self) -> Result<()>;

    /// May return a follow-up prompt for the manager to run.
    async fn respond_to_permission(
        &self,
        request_id: &str,
        response: PermissionResponse,
    ) -> Result<Option<PermissionResult>>;

    /// Provider history as stream events, collected in one piece
    /// (PARITY-QUIRK(stream-history-single-frame)).
    async fn stream_history(&self) -> Result<Vec<AgentStreamEvent>>;

    /// `true` when the provider handled the prompt itself; its events arrive on the stream.
    async fn try_handle_out_of_band(&self, prompt: PromptInput) -> Result<bool>;

    async fn available_modes(&self) -> Result<Vec<AgentMode>>;

    async fn current_mode(&self) -> Result<Option<String>>;

    /// Returns the provider notice, if any.
    async fn set_mode(&self, mode_id: &str) -> Result<Option<Value>>;

    async fn set_model(&self, model_id: Option<&str>) -> Result<()>;

    async fn set_thinking_option(&self, option_id: Option<&str>) -> Result<Option<Value>>;

    async fn set_feature(&self, feature_id: &str, value: Value) -> Result<()>;

    async fn list_commands(&self) -> Result<Vec<Value>>;

    async fn revert_conversation(&self, message_id: &str) -> Result<()>;

    async fn revert_files(&self, message_id: &str) -> Result<()>;

    async fn revert_both(&self, message_id: &str) -> Result<()>;

    async fn describe_persistence(&self) -> Result<Option<PersistenceHandle>>;

    async fn get_runtime_info(&self) -> Result<AgentRuntimeInfo>;

    async fn pending_permissions(&self) -> Result<Vec<PermissionRequest>>;

    async fn close(&self) -> Result<()>;
}

/// Executes internal Paseo tools on behalf of a provider (reverse call into the core).
#[async_trait]
pub trait ToolCallHandler: Send + Sync + 'static {
    async fn call(&self, request: ToolCallRequest) -> Result<ToolCallResult>;
}
