//! Provider-history hydration and rewind (`agent-manager.ts:3264-3330`, `:4119-4277`).
//!
//! The history is read completely before anything is applied (a stream that fails halfway leaves
//! the timeline untouched), then one actor step replaces or extends the timeline.

use paseo_agent_types::AgentTimelineItem;
use paseo_timeline::Row;
use serde_json::Value;
use tracing::warn;

use crate::agent_actor::hydrate::collect_history;
use crate::error::{CoreError, Result};
use crate::events::ManagerEvent;
use crate::manager::AgentManager;
use crate::model::{HydrateOpts, RewindMode};

impl AgentManager {
    /// `hydrateTimelineFromProvider`: no-op when already primed unless forced.
    pub async fn hydrate_timeline_from_provider(
        &self,
        agent_id: &str,
        options: HydrateOpts,
    ) -> Result<()> {
        let handle = self.require_agent(agent_id)?;
        if handle.view().history_primed && !options.force {
            return Ok(());
        }
        let history = match handle.session.stream_history().await {
            Ok(history) => history,
            Err(error) => {
                warn!(%error, %agent_id, "Failed to hydrate provider history");
                return Err(error.into());
            }
        };
        let collected = collect_history(history);
        let broadcast = options.broadcast.clone();
        let broadcast_timeline = options
            .broadcast_timeline
            .unwrap_or_else(|| options.broadcast.clone());
        let force = options.force;
        handle
            .ask(move |a| {
                if force {
                    a.apply_force_history(collected, broadcast.now(), broadcast_timeline.now());
                } else {
                    a.apply_prime_history(collected, &broadcast);
                }
            })
            .await
    }

    /// `rewind`: revert provider state to a message, re-read history, publish the replacement.
    ///
    /// The run slot is parked as a lock while the provider reverts, so a concurrent prompt fails
    /// with the usual active-run error and events the provider emits meanwhile are staged and
    /// dropped (PARITY-QUIRK(rewind-staged-events-dropped)).
    pub async fn rewind(&self, agent_id: &str, message_id: &str, mode: RewindMode) -> Result<()> {
        let handle = self.require_agent(agent_id)?;
        let wanted = message_id.to_owned();
        let provider_message_id = handle
            .ask(move |a| {
                let submitted = a.timeline.rows().iter().find(|row| {
                    matches!(
                        &row.item,
                        AgentTimelineItem::UserMessage { message_id: Some(m), client_message_id: Some(c), .. }
                            if *m == wanted && *c == wanted
                    )
                });
                match submitted {
                    Some(row) => row
                        .provider_message_id
                        .clone()
                        .ok_or_else(|| {
                            CoreError::msg("Cannot rewind before the provider acknowledges the submitted prompt")
                        }),
                    None => Ok(wanted),
                }
            })
            .await??;
        if handle.view().has_in_flight_run() {
            self.cancel_agent_run_before(agent_id, "rewind").await?;
        }
        let lock = handle.ask(|a| a.run.create_pending_run()).await?;
        let outcome = self
            .rewind_locked(agent_id, &provider_message_id, mode)
            .await;
        let token = lock.token;
        let _ = handle
            .ask(move |a| a.run.settle_foreground_run(&token))
            .await;
        if let Err(error) = &outcome {
            warn!(%error, %agent_id, message_id, mode = mode.as_str(), "agent.rewind.failed");
        }
        outcome
    }

    async fn rewind_locked(
        &self,
        agent_id: &str,
        provider_message_id: &str,
        mode: RewindMode,
    ) -> Result<()> {
        let handle = self.require_agent(agent_id)?;
        let flag = match mode {
            RewindMode::Conversation => "supportsRewindConversation",
            RewindMode::Files => "supportsRewindFiles",
            RewindMode::Both => "supportsRewindBoth",
        };
        if handle.session.capabilities().get(flag) != Some(&Value::Bool(true)) {
            return Err(CoreError::msg(format!(
                "Provider does not support rewinding {}",
                mode.as_str()
            )));
        }
        match mode {
            RewindMode::Conversation => {
                handle
                    .session
                    .revert_conversation(provider_message_id)
                    .await?
            }
            RewindMode::Files => handle.session.revert_files(provider_message_id).await?,
            RewindMode::Both => handle.session.revert_both(provider_message_id).await?,
        }
        if mode != RewindMode::Files {
            self.hydrate_timeline_from_provider(
                agent_id,
                HydrateOpts {
                    force: true,
                    broadcast: true.into(),
                    broadcast_timeline: Some(false.into()),
                },
            )
            .await?;
            let epoch = handle.ask(|a| a.timeline.epoch().to_owned()).await?;
            self.bus().emit(ManagerEvent::TimelineReplacement {
                agent_id: agent_id.to_owned(),
                epoch,
            });
        }
        // Rewind stages provider events under the run lock; publish its final state directly.
        handle.ask(|a| a.refresh_session_persistence()).await?;
        self.refresh_session_state(&handle, false).await;
        self.persist_snapshot(&handle, Default::default()).await?;
        handle.ask(|a| a.emit_state(false)).await
    }
}

/// Rows the importer hands to `register_session`, re-exported for the host adapter.
pub type ImportedRows = Vec<Row>;
