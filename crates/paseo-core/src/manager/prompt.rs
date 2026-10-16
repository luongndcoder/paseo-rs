//! `startAgentRun` of `agent-prompt.ts`: the one entry every prompt surface goes through.
//!
//! Out-of-band commands first (they must not cancel a running turn), then steer or replace or
//! plain stream, draining the run in the background. A stale provider session (a plugin runtime
//! was replaced) is reloaded from persistence and the start retried once.

use paseo_provider_api::PromptInput;
use tracing::{error, info};

use super::steer::SteerDispatch;
use super::AgentManager;
use crate::error::Result;
use crate::model::{ReloadOptions, RunOpts, SteerRunOpts};

/// `PromptDispatchDisposition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDisposition {
    OutOfBand,
    Steered,
    TurnStarted,
}

/// `StartAgentRunOptions` (`activeTurnBehavior: "steer"` is `steer`).
#[derive(Debug, Clone, Default)]
pub struct StartRunOptions {
    pub replace_running: bool,
    pub steer: bool,
    pub run: RunOpts,
    pub clear_pending_permissions: bool,
}

impl AgentManager {
    /// `startAgentRun`.
    pub async fn start_agent_run(
        &self,
        agent_id: &str,
        prompt: PromptInput,
        options: StartRunOptions,
    ) -> Result<PromptDisposition> {
        if self
            .try_run_out_of_band(agent_id, prompt.clone(), options.run.clone())
            .await?
        {
            return Ok(PromptDisposition::OutOfBand);
        }
        match self
            .start_agent_run_inner(agent_id, &prompt, &options)
            .await
        {
            Err(error) if error.is_stale_provider_session() => {
                info!(%agent_id, %error, "Provider session went stale; reopening from persistence");
                // The live session belongs to a retired plugin runtime: reload swaps in a fresh
                // one while preserving history and labels.
                self.reload_agent_session(agent_id, None, ReloadOptions::default())
                    .await?;
                self.start_agent_run_inner(agent_id, &prompt, &options)
                    .await
            }
            other => other,
        }
    }

    async fn start_agent_run_inner(
        &self,
        agent_id: &str,
        prompt: &PromptInput,
        options: &StartRunOptions,
    ) -> Result<PromptDisposition> {
        let mut stream = None;
        if options.steer {
            let steer = SteerRunOpts {
                run: options.run.clone(),
                clear_pending_permissions: options.clear_pending_permissions,
            };
            match self
                .steer_or_replace_active_turn(agent_id, prompt.clone(), steer)
                .await?
            {
                SteerDispatch::Steered => return Ok(PromptDisposition::Steered),
                SteerDispatch::Replaced(replaced) => stream = Some(*replaced),
                SteerDispatch::Inactive => {}
            }
        }
        let mut stream = match stream {
            Some(stream) => stream,
            None => self.start_or_replace_run(agent_id, prompt, options).await?,
        };
        let (manager, agent_id, prompt, options) = (
            self.clone(),
            agent_id.to_owned(),
            prompt.clone(),
            options.clone(),
        );
        tokio::spawn(async move {
            let drained = match stream.drain().await {
                Err(error) if error.is_stale_provider_session() => {
                    info!(%agent_id, %error, "Provider session went stale; reopening from persistence");
                    manager
                        .retry_after_reload(&agent_id, &prompt, &options)
                        .await
                }
                other => other,
            };
            if let Err(err) = drained {
                error!(%err, %agent_id, "Agent stream failed");
            }
        });
        Ok(PromptDisposition::TurnStarted)
    }

    async fn retry_after_reload(
        &self,
        agent_id: &str,
        prompt: &PromptInput,
        options: &StartRunOptions,
    ) -> Result<()> {
        self.reload_agent_session(agent_id, None, ReloadOptions::default())
            .await?;
        self.start_or_replace_run(agent_id, prompt, options)
            .await?
            .drain()
            .await
    }

    async fn start_or_replace_run(
        &self,
        agent_id: &str,
        prompt: &PromptInput,
        options: &StartRunOptions,
    ) -> Result<super::runs::RunStream> {
        let replace = options.replace_running && self.has_in_flight_run(agent_id);
        if replace {
            self.replace_agent_run(agent_id, prompt.clone(), options.run.clone())
                .await
        } else {
            self.stream_agent(agent_id, prompt.clone(), options.run.clone())
                .await
        }
    }
}
