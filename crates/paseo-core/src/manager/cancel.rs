//! `cancel_agent_run` (`agent-manager.ts:3149-3212`): the waits live here, each state change is
//! one actor command. Serialized per agent on the foreground lane.

use std::time::Duration;

use paseo_agent_types::AgentStreamEvent;
use tokio::sync::watch;
use tracing::{error, warn};

use super::{AgentManager, INTERRUPT_SESSION_TIMEOUT};
use crate::agent_actor::run_state::{RunKind, RunTicket};
use crate::agent_actor::ActorHandle;
use crate::error::{CoreError, Result};
use crate::model::CancelOutcome;

/// Wait for `rx` to report settled. A closed channel means the run (or its actor) is gone.
pub(crate) async fn wait_settled(mut rx: watch::Receiver<bool>, timeout: Option<Duration>) -> bool {
    let wait = async {
        // An `Err` means the sender is gone: the actor ended, which clears its run.
        let _ = rx.wait_for(|settled| *settled).await;
    };
    match timeout {
        Some(limit) => tokio::time::timeout(limit, wait).await.is_ok(),
        None => {
            wait.await;
            true
        }
    }
}

impl AgentManager {
    /// `cancelAgentRun`: serialized on the foreground lane.
    pub async fn cancel_agent_run(&self, agent_id: &str) -> Result<CancelOutcome> {
        let key = agent_id.to_owned();
        self.inner
            .foreground_lanes
            .run(&key, || self.cancel_agent_run_now(agent_id))
            .await
    }

    async fn cancel_agent_run_now(&self, agent_id: &str) -> Result<CancelOutcome> {
        let handle = self.require_agent(agent_id)?;
        let Some(ticket) = handle.ask(|a| a.cancel_begin()).await? else {
            return Ok(CancelOutcome::NotRunning);
        };
        let acknowledged = self.interrupt_session(&handle).await;
        let settle_timeout = if acknowledged {
            INTERRUPT_SESSION_TIMEOUT
        } else {
            self.inner.options.interrupt_session_timeout
        };
        let settled = wait_settled(ticket.settled.clone(), Some(settle_timeout)).await;
        if !acknowledged {
            return Ok(if settled {
                CancelOutcome::Settled
            } else {
                CancelOutcome::Refused
            });
        }
        if !settled {
            self.force_cancel(&handle, &ticket).await;
        }
        handle.ask(|a| a.resolve_interrupted_permissions()).await?;
        Ok(CancelOutcome::Settled)
    }

    /// Acknowledged interrupt whose run did not settle in time.
    async fn force_cancel(&self, handle: &ActorHandle, ticket: &RunTicket) {
        let run_turn_id = handle.ask(|a| a.run_turn_id()).await.ok().flatten();
        let canceled = |provider: String, turn_id: Option<String>| AgentStreamEvent::TurnCanceled {
            provider,
            reason: "interrupted".into(),
            turn_id,
        };
        if let Some(turn_id) = run_turn_id {
            warn!(agent_id = %handle.id, %turn_id, "cancel_agent_run: acknowledged turn still active after timeout, force-canceling");
            let _ = handle
                .ask(move |a| {
                    let event = canceled(a.view.provider.clone(), Some(turn_id));
                    a.process_forced_event(event);
                })
                .await;
            wait_settled(ticket.settled.clone(), None).await;
        } else if ticket.kind == RunKind::Foreground {
            warn!(agent_id = %handle.id, "cancel_agent_run: acknowledged pending turn still active after timeout, clearing it");
            let token = ticket.token.clone();
            let _ = handle
                .ask(move |a| a.clear_pending_foreground_run(&token))
                .await;
        } else {
            warn!(agent_id = %handle.id, "cancel_agent_run: acknowledged turn still active after timeout, force-canceling");
            let _ = handle
                .ask(move |a| {
                    let event = canceled(a.view.provider.clone(), None);
                    a.process_forced_event(event);
                })
                .await;
        }
    }

    /// `interruptSession`: `true` only for an acknowledged interrupt inside the timeout. A late
    /// failure is logged, the call keeps running.
    async fn interrupt_session(&self, handle: &ActorHandle) -> bool {
        let session = handle.session.clone();
        let mut task = tokio::spawn(async move { session.interrupt().await });
        let limit = self.inner.options.interrupt_session_timeout;
        match tokio::time::timeout(limit, &mut task).await {
            Ok(Ok(Ok(()))) => true,
            Ok(Ok(Err(err))) => {
                error!(agent_id = %handle.id, %err, "Failed to interrupt session");
                false
            }
            Ok(Err(_)) => false,
            Err(_) => {
                warn!(agent_id = %handle.id, ?limit, "Timed out interrupting session during cancel");
                let agent_id = handle.id.clone();
                tokio::spawn(async move {
                    if let Ok(Err(err)) = task.await {
                        warn!(%agent_id, %err, "Session interrupt failed after timeout during cancel");
                    }
                });
                false
            }
        }
    }

    pub(crate) async fn cancel_agent_run_before(
        &self,
        agent_id: &str,
        action: &'static str,
    ) -> Result<()> {
        if self.cancel_agent_run(agent_id).await? == CancelOutcome::Refused {
            return Err(CoreError::RunCancellation {
                agent_id: agent_id.to_owned(),
                action,
            });
        }
        Ok(())
    }
}
