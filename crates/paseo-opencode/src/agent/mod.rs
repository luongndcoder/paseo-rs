//! Provider-agnostic surface (`AgentClient` / `AgentSession`) and the OpenCode implementation.

mod mapping;
mod opencode;
mod session;
pub mod types;

pub use opencode::OpenCodeAgentClient;
pub use session::OpenCodeAgentSession;
pub use types::*;

use crate::error::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Rust equivalent of `AgentClient` (agent-sdk-types.ts).
#[async_trait]
pub trait AgentClient: Send + Sync {
    fn provider(&self) -> &str;
    fn capabilities(&self) -> AgentCapabilities;
    async fn create_session(
        &self,
        config: AgentSessionConfig,
        launch: Option<AgentLaunchContext>,
        options: AgentCreateSessionOptions,
    ) -> Result<Arc<dyn AgentSession>>;
    async fn resume_session(
        &self,
        handle: &AgentPersistenceHandle,
        overrides: Option<AgentSessionConfig>,
        launch: Option<AgentLaunchContext>,
    ) -> Result<Arc<dyn AgentSession>>;
    /// Models and modes together.
    async fn fetch_catalog(&self, options: FetchCatalogOptions) -> Result<ProviderCatalog>;
    async fn list_commands(&self, config: &AgentSessionConfig) -> Result<Vec<AgentSlashCommand>>;
    /// True when the CLI binary can be found.
    async fn is_available(&self) -> bool;
    /// Release provider-owned resources (idempotent).
    async fn shutdown(&self);
}

/// Rust equivalent of `AgentSession`. Events arrive through `subscribe`.
#[async_trait]
pub trait AgentSession: Send + Sync {
    fn provider(&self) -> &str;
    fn id(&self) -> String;
    fn capabilities(&self) -> AgentCapabilities;
    /// Starts a turn and returns its id; progress and the terminal event arrive via `subscribe`.
    async fn start_turn(
        &self,
        prompt: AgentPromptInput,
        options: AgentRunOptions,
    ) -> Result<String>;
    /// Starts a turn and waits for its terminal event.
    async fn run(
        &self,
        prompt: AgentPromptInput,
        options: AgentRunOptions,
    ) -> Result<AgentRunResult>;
    fn subscribe(&self) -> broadcast::Receiver<AgentEvent>;
    /// The persisted conversation as provider-native JSON messages.
    async fn stream_history(&self) -> Result<Vec<serde_json::Value>>;
    async fn get_runtime_info(&self) -> Result<AgentRuntimeInfo>;
    async fn get_available_modes(&self) -> Result<Vec<AgentMode>>;
    async fn get_current_mode(&self) -> Option<String>;
    async fn set_mode(&self, mode_id: &str) -> Result<()>;
    async fn set_model(&self, model_id: Option<&str>) -> Result<()>;
    async fn set_thinking_option(&self, option_id: Option<&str>) -> Result<()>;
    async fn list_commands(&self) -> Result<Vec<AgentSlashCommand>>;
    fn get_pending_permissions(&self) -> Vec<AgentPermissionRequest>;
    async fn respond_to_permission(
        &self,
        request_id: &str,
        response: AgentPermissionResponse,
    ) -> Result<()>;
    fn describe_persistence(&self) -> Option<AgentPersistenceHandle>;
    /// Abort the foreground turn. A `TurnCanceled` event follows once the abort is acknowledged.
    async fn interrupt(&self) -> Result<()>;
    /// Revert conversation and files to just before `message_id`.
    async fn revert_both(&self, message_id: &str) -> Result<()>;
    /// Release runtime resources; deletes the native session unless it is persistent.
    async fn close(&self) -> Result<()>;
}
