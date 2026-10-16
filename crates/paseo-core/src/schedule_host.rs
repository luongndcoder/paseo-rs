//! [`ScheduleAgentHost`] over the agent manager (`schedule/service.ts` `executeSchedule`).
//!
//! The schedule crate is synchronous, so every method blocks on the manager through a tokio
//! `Handle`. Call them from a thread that is not a runtime worker (`spawn_blocking` or a plain
//! thread): `Handle::block_on` panics inside async context.
//!
//! Workspace creation and archive are other domains (git worktrees, the workspace registry), so
//! they come in through [`ScheduleWorkspaces`]. What TS's bound `createAgent` command adds on
//! top of the manager (`unattended`, `notifyOnFinish`, mode/thinking/feature application) is the
//! command layer's job; the stored config already carries mode, model, thinking and features,
//! which `create_agent` passes to the provider.

use std::sync::Arc;

use paseo_agent_types::AgentTimelineItem;
use paseo_provider_api::{AgentSessionConfig, PromptInput};
use paseo_schedule::host::{
    AgentRunResult, AgentTargetState, AgentWaitOutcome, CreateScheduledAgent, CreatedAgent,
    RunWorkspace,
};
use paseo_schedule::{Isolation, Result, ScheduleAgentHost, ScheduleError};
use tokio::runtime::Handle;

use crate::activity::{curate_agent_activity, ActivityOptions};
use crate::lifecycle::create::resolve_create_agent_titles;
use crate::lifecycle::loader::LoadOptions;
use crate::manager::prompt::StartRunOptions;
use crate::manager::waits::WaitForAgentOptions;
use crate::manager::AgentManager;
use crate::model::{CreateAgentOptions, Lifecycle, RunOpts};

/// Creates and archives the workspace of one new-agent run (directory or worktree).
pub trait ScheduleWorkspaces: Send + Sync {
    fn create_run_workspace(
        &self,
        cwd: &str,
        isolation: Isolation,
        prompt: &str,
    ) -> Result<RunWorkspace>;

    fn archive_workspace(&self, workspace_id: &str) -> Result<()>;
}

pub struct CoreScheduleHost {
    manager: Arc<AgentManager>,
    runtime: Handle,
    workspaces: Arc<dyn ScheduleWorkspaces>,
}

impl CoreScheduleHost {
    pub fn new(
        manager: Arc<AgentManager>,
        runtime: Handle,
        workspaces: Arc<dyn ScheduleWorkspaces>,
    ) -> Self {
        Self {
            manager,
            runtime,
            workspaces,
        }
    }

    fn block<T>(&self, work: impl std::future::Future<Output = T>) -> T {
        self.runtime.block_on(work)
    }
}

fn to_schedule_error(error: impl std::fmt::Display) -> ScheduleError {
    ScheduleError::Message(error.to_string())
}

fn wait_outcome(result: crate::manager::waits::WaitForAgentResult) -> AgentWaitOutcome {
    AgentWaitOutcome {
        permission_pending: result.permission.is_some(),
        errored: result.status == Lifecycle::Error,
        last_message: result.last_message,
    }
}

/// `buildScheduleAgentConfig` with the run workspace's cwd.
fn schedule_agent_config(request: &CreateScheduledAgent) -> AgentSessionConfig {
    let source = &request.config;
    let mut config = AgentSessionConfig::new(source.provider.clone(), request.cwd.clone());
    config.mode_id = source.mode_id.clone();
    config.model = source.model.clone();
    config.thinking_option_id = source.thinking_option_id.clone();
    config.title = source.title.clone().flatten();
    config.provider_options = source
        .provider_options
        .clone()
        .map(serde_json::Value::Object);
    config.feature_values = source.feature_values.clone();
    config.system_prompt = source.system_prompt.clone();
    config.mcp_servers = source.mcp_servers.clone().map(serde_json::Value::Object);
    config
}

impl ScheduleAgentHost for CoreScheduleHost {
    fn agent_target_state(&self, agent_id: &str) -> Result<AgentTargetState> {
        let store = self
            .manager
            .store()
            .ok_or_else(|| ScheduleError::msg("Agent storage unavailable"))?
            .clone();
        let record = self.block(store.get(agent_id)).map_err(to_schedule_error)?;
        Ok(match record {
            None => AgentTargetState::Missing,
            Some(record) if record.is_archived() => AgentTargetState::Archived,
            Some(_) => AgentTargetState::Active,
        })
    }

    fn run_existing_agent(&self, agent_id: &str, prompt: &str) -> Result<AgentWaitOutcome> {
        let manager = &self.manager;
        self.block(async {
            let agent = manager
                .ensure_agent_loaded(agent_id, LoadOptions::default())
                .await?;
            if manager.has_in_flight_run(&agent.id) {
                return Err(crate::CoreError::msg(format!(
                    "Agent {} already has an active run",
                    agent.id
                )));
            }
            manager
                .start_agent_run(
                    &agent.id,
                    PromptInput::from(prompt),
                    StartRunOptions {
                        replace_running: true,
                        steer: true,
                        ..StartRunOptions::default()
                    },
                )
                .await?;
            let waited = manager
                .wait_for_agent_event(
                    &agent.id,
                    WaitForAgentOptions {
                        wait_for_active: true,
                        ..WaitForAgentOptions::default()
                    },
                )
                .await?;
            Ok(wait_outcome(waited))
        })
        .map_err(to_schedule_error)
    }

    fn create_run_workspace(
        &self,
        cwd: &str,
        isolation: Isolation,
        prompt: &str,
    ) -> Result<RunWorkspace> {
        self.workspaces.create_run_workspace(cwd, isolation, prompt)
    }

    fn create_agent(&self, request: &CreateScheduledAgent) -> Result<CreatedAgent> {
        let mut config = schedule_agent_config(request);
        let (_, provisional) = resolve_create_agent_titles(
            request.config.title.clone().flatten().as_deref(),
            Some(&request.prompt),
        );
        config.title = provisional;
        let labels = request.labels.iter().cloned().collect();
        let agent = self
            .block(self.manager.create_agent(
                config,
                None,
                CreateAgentOptions {
                    labels: Some(labels),
                    workspace_id: Some(request.workspace_id.clone()),
                    ..CreateAgentOptions::default()
                },
            ))
            .map_err(to_schedule_error)?;
        Ok(CreatedAgent {
            agent_id: agent.id.clone(),
            initial_prompt_error: None,
        })
    }

    fn run_agent(&self, agent_id: &str, prompt: &str) -> Result<AgentRunResult> {
        let outcome = self
            .block(
                self.manager
                    .run_agent(agent_id, PromptInput::from(prompt), RunOpts::default()),
            )
            .map_err(to_schedule_error)?;
        Ok(AgentRunResult {
            canceled: outcome.canceled,
            final_text: outcome.final_text,
            timeline_text: curate_agent_activity(
                &outcome.timeline as &[AgentTimelineItem],
                &ActivityOptions::default(),
            ),
        })
    }

    fn wait_for_agent_event(&self, agent_id: &str) -> Result<AgentWaitOutcome> {
        let waited = self
            .block(self.manager.wait_for_agent_event(
                agent_id,
                WaitForAgentOptions {
                    wait_for_active: true,
                    ..WaitForAgentOptions::default()
                },
            ))
            .map_err(to_schedule_error)?;
        Ok(wait_outcome(waited))
    }

    fn archive_workspace(&self, workspace_id: &str) -> Result<()> {
        self.workspaces.archive_workspace(workspace_id)
    }
}
