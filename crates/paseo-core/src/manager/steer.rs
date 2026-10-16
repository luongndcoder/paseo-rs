//! Replace and steer (`agent-manager.ts:2799-2991`): `replace_agent_run`, `steer_agent_run`,
//! `steer_or_replace_active_turn` and the barrier-guarded steer admission.

use paseo_provider_api::{PromptInput, SteerResult};

use super::runs::RunStream;
use super::AgentManager;
use crate::agent_actor::ActorHandle;
use crate::error::{CoreError, Result};
use crate::model::{RunOpts, SteerRunOpts};

/// `ActiveTurnSteerDispatchResult`.
pub enum SteerDispatch {
    Inactive,
    Steered,
    Replaced(Box<RunStream>),
}

impl AgentManager {
    /// `replaceAgentRun`: cancel the active run (holding `running`) and stream the new prompt.
    pub async fn replace_agent_run(
        &self,
        agent_id: &str,
        prompt: PromptInput,
        opts: RunOpts,
    ) -> Result<RunStream> {
        let handle = self.require_agent(agent_id)?;
        let busy = handle
            .ask(|a| a.view.has_in_flight_run() || a.run.has_run())
            .await?;
        if !busy {
            return self.stream_agent(agent_id, prompt, opts).await;
        }
        handle.ask(|a| a.begin_replacement()).await?;
        self.cancel_then_stream(agent_id, prompt, opts).await
    }

    async fn cancel_then_stream(
        &self,
        agent_id: &str,
        prompt: PromptInput,
        opts: RunOpts,
    ) -> Result<RunStream> {
        let outcome = async {
            self.cancel_agent_run_before(agent_id, "replace").await?;
            self.stream_agent(agent_id, prompt, opts).await
        }
        .await;
        if outcome.is_err() {
            if let Some(latest) = self.inner.registry.get(agent_id) {
                let _ = latest.ask(|a| a.view.pending_replacement = false).await;
            }
        }
        outcome
    }

    /// `steerAgentRun`.
    pub async fn steer_agent_run(
        &self,
        agent_id: &str,
        prompt: PromptInput,
        opts: SteerRunOpts,
    ) -> Result<SteerResult> {
        let handle = self.require_agent(agent_id)?;
        let Some(expected) = expected_turn_id(&handle).await? else {
            return Ok(SteerResult::Unavailable);
        };
        let result = self
            .run_steer_admission(&handle, &expected, || {
                steer_once(&handle, &prompt, &opts, &expected)
            })
            .await?;
        // `unavailable` is only safe to fall back from while this admission still owns the turn.
        if result == SteerResult::Unavailable {
            let expected_owned = expected.clone();
            let owns = handle
                .ask(move |a| a.view.active_turn_id.as_deref() == Some(expected_owned.as_str()))
                .await?;
            if !owns {
                return Err(CoreError::msg(
                    "Active turn changed before steering could be delivered",
                ));
            }
        }
        Ok(result)
    }

    /// `steerOrReplaceActiveTurn`.
    pub async fn steer_or_replace_active_turn(
        &self,
        agent_id: &str,
        prompt: PromptInput,
        opts: SteerRunOpts,
    ) -> Result<SteerDispatch> {
        let handle = self.require_agent(agent_id)?;
        let Some(expected) = expected_turn_id(&handle).await? else {
            return Ok(SteerDispatch::Inactive);
        };
        let result = self
            .run_steer_admission(&handle, &expected, || {
                steer_once(&handle, &prompt, &opts, &expected)
            })
            .await?;
        if result == SteerResult::Accepted {
            return Ok(SteerDispatch::Steered);
        }
        let expected_owned = expected.clone();
        let passive = handle
            .ask(move |a| {
                a.view.active_foreground_turn_id.is_none()
                    && a.view.active_turn_id.as_deref() == Some(expected_owned.as_str())
            })
            .await?;
        if passive {
            return Ok(SteerDispatch::Inactive);
        }
        self.hooks()
            .before_steer_unavailable_fallback(agent_id, &expected)
            .await;
        let expected_owned = expected.clone();
        handle
            .ask(move |a| {
                a.assert_steer_owns_turn(&expected_owned)?;
                a.begin_replacement();
                Ok::<(), CoreError>(())
            })
            .await??;
        let stream = self.cancel_then_stream(agent_id, prompt, opts.run).await?;
        Ok(SteerDispatch::Replaced(Box::new(stream)))
    }

    /// `runSteerAdmission`: on the foreground lane, drain delivered events, flush the coalescer,
    /// assert ownership, hold provider events on a barrier while `operation` runs, then release
    /// them in order.
    async fn run_steer_admission<T, Fut>(
        &self,
        handle: &ActorHandle,
        expected: &str,
        operation: impl FnOnce() -> Fut,
    ) -> Result<T>
    where
        Fut: std::future::Future<Output = Result<T>>,
    {
        let key = handle.id.clone();
        self.inner
            .foreground_lanes
            .run(&key, || async {
                let expected_owned = expected.to_owned();
                let barrier = handle
                    .ask(move |a| a.begin_steer_admission(&expected_owned))
                    .await??;
                let result = operation().await;
                let _ = handle.ask(move |a| a.end_steer_admission(barrier)).await;
                result
            })
            .await
    }
}

async fn expected_turn_id(handle: &ActorHandle) -> Result<Option<String>> {
    handle
        .ask(|a| {
            a.view
                .active_foreground_turn_id
                .clone()
                .or_else(|| a.view.active_turn_id.clone())
        })
        .await
}

async fn steer_once(
    handle: &ActorHandle,
    prompt: &PromptInput,
    opts: &SteerRunOpts,
    expected: &str,
) -> Result<SteerResult> {
    let admission = handle
        .session
        .steer_active_turn(prompt.clone(), opts.to_provider(expected))
        .await?;
    if admission == SteerResult::Accepted {
        if let Some(client_message_id) = opts.run.client_message_id.clone() {
            let (prompt, expected) = (prompt.clone(), expected.to_owned());
            handle
                .ask(move |a| a.record_accepted_steer(&prompt, &client_message_id, &expected))
                .await?;
        }
    }
    Ok(admission)
}
