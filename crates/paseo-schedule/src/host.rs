//! The agent-launching seam. The edge/core implements [`ScheduleAgentHost`] against
//! the real agent manager, storage and workspace services; this crate never
//! spawns an agent itself. All methods are synchronous: wrap async work with
//! `block_on` / `spawn_blocking` in the implementation.

use crate::error::Result;
use crate::types::{Isolation, NewAgentConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTargetState {
    /// No stored record for the agent.
    Missing,
    Archived,
    Active,
}

/// What `waitForAgentEvent` reports once the agent settles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentWaitOutcome {
    pub permission_pending: bool,
    pub errored: bool,
    pub last_message: Option<String>,
}

/// Result of running a prompt on a freshly created agent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentRunResult {
    pub canceled: bool,
    pub final_text: String,
    /// The activity timeline already curated to plain text.
    pub timeline_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunWorkspace {
    pub workspace_id: String,
    pub cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedAgent {
    pub agent_id: String,
    /// Set when the agent was created but its initial prompt failed to start.
    pub initial_prompt_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateScheduledAgent {
    /// `provider` or `provider/model` (see [`format_provider_model`]).
    pub provider: String,
    /// The stored config with `cwd` replaced by the run workspace cwd.
    pub config: NewAgentConfig,
    pub cwd: String,
    pub workspace_id: String,
    /// `paseo.schedule-id` and `paseo.schedule-run`.
    pub labels: Vec<(String, String)>,
    /// The schedule prompt; the host derives the provisional title from it.
    pub prompt: String,
}

pub trait ScheduleAgentHost: Send + Sync {
    /// Look up the target agent of an `agent` schedule.
    fn agent_target_state(&self, agent_id: &str) -> Result<AgentTargetState>;

    /// Deliver `prompt` to an existing agent (load it if needed, fail if it already
    /// has an active run, steer an active turn) and wait for it to settle.
    fn run_existing_agent(&self, agent_id: &str, prompt: &str) -> Result<AgentWaitOutcome>;

    /// Create the workspace for one new-agent run (directory or worktree).
    fn create_run_workspace(
        &self,
        cwd: &str,
        isolation: Isolation,
        prompt: &str,
    ) -> Result<RunWorkspace>;

    fn create_agent(&self, request: &CreateScheduledAgent) -> Result<CreatedAgent>;

    fn run_agent(&self, agent_id: &str, prompt: &str) -> Result<AgentRunResult>;

    fn wait_for_agent_event(&self, agent_id: &str) -> Result<AgentWaitOutcome>;

    fn archive_workspace(&self, workspace_id: &str) -> Result<()>;
}

/// `provider` when no model, or the provider already carries one (`a/b`).
pub fn format_provider_model(provider: &str, model: Option<&str>) -> String {
    match model {
        Some(m) if !m.is_empty() && !provider.contains('/') => format!("{provider}/{m}"),
        _ => provider.to_string(),
    }
}

/// `<paseo-system>` envelope that marks a prompt as system-injected context.
pub fn format_system_notification_prompt(reason: &str) -> String {
    format!("<paseo-system>\n{reason}\n</paseo-system>")
}
