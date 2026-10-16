//! `ensure_agent_loaded` and `ensure_unarchived_agent_loaded` (`agent-loading.ts`): bring a
//! stored agent to life exactly once, however many callers ask at the same time.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::watch;
use tracing::warn;

use crate::error::{CoreError, Result};
use crate::manager::config_prep::ResumePurpose;
use crate::manager::AgentManager;
use crate::model::{Broadcast, CreateAgentOptions, HydrateOpts, ManagedAgentView, ResumeOptions};
use crate::projection::{
    build_config_overrides, build_session_config, extract_attention, extract_timestamps,
    to_agent_persistence_handle,
};

type LoadResult = Option<std::result::Result<Arc<ManagedAgentView>, String>>;

/// One in-flight initialization (`pendingAgentInitializations`).
pub(crate) struct PendingLoad {
    /// `options.broadcastTimeline`: a second loader can upgrade it while the first is running.
    broadcast: Arc<AtomicBool>,
    result: watch::Receiver<LoadResult>,
}

/// `EnsureAgentLoadedDeps` minus the manager and storage.
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// Providers a stored record may reference; defaults to the registered providers.
    pub valid_providers: Option<Vec<String>>,
    pub broadcast_timeline: bool,
}

impl AgentManager {
    fn join_pending_load(
        &self,
        agent_id: &str,
        broadcast_timeline: bool,
    ) -> Option<watch::Receiver<LoadResult>> {
        let loads = self
            .inner
            .pending_loads
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let pending = loads.get(agent_id)?;
        if broadcast_timeline {
            pending.broadcast.store(true, Ordering::SeqCst);
        }
        Some(pending.result.clone())
    }

    async fn await_pending_load(
        mut result: watch::Receiver<LoadResult>,
    ) -> Result<Arc<ManagedAgentView>> {
        match result.wait_for(|r| r.is_some()).await {
            Ok(done) => match done.clone() {
                Some(Ok(view)) => Ok(view),
                Some(Err(message)) => Err(CoreError::Message(message)),
                None => unreachable!("waited for a value"),
            },
            Err(_) => Err(CoreError::msg("Agent load was abandoned")),
        }
    }

    /// `ensureAgentLoaded`. Two `wait_for_agent_close` barriers (before and after the live
    /// lookup) keep a load from resuming a second writer next to a close or reload in flight.
    pub async fn ensure_agent_loaded(
        &self,
        agent_id: &str,
        options: LoadOptions,
    ) -> Result<Arc<ManagedAgentView>> {
        self.wait_for_agent_close(agent_id).await;
        if let Some(result) = self.join_pending_load(agent_id, options.broadcast_timeline) {
            return Self::await_pending_load(result).await;
        }
        if let Some(existing) = self.get_agent(agent_id) {
            return Ok(existing);
        }
        // A close may have started after the first barrier saw nothing in flight.
        self.wait_for_agent_close(agent_id).await;
        let created = {
            let mut loads = self
                .inner
                .pending_loads
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match loads.get(agent_id) {
                Some(pending) => {
                    if options.broadcast_timeline {
                        pending.broadcast.store(true, Ordering::SeqCst);
                    }
                    Err(pending.result.clone())
                }
                None => {
                    let broadcast = Arc::new(AtomicBool::new(options.broadcast_timeline));
                    let (sender, receiver) = watch::channel(None);
                    loads.insert(
                        agent_id.to_owned(),
                        PendingLoad {
                            broadcast: broadcast.clone(),
                            result: receiver,
                        },
                    );
                    Ok((sender, broadcast))
                }
            }
        };
        let (sender, broadcast) = match created {
            Ok(created) => created,
            Err(result) => return Self::await_pending_load(result).await,
        };
        let outcome = self.load_stored_agent(agent_id, &options, broadcast).await;
        self.inner
            .pending_loads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(agent_id);
        let _ = sender.send(Some(
            outcome
                .as_ref()
                .map(Arc::clone)
                .map_err(ToString::to_string),
        ));
        outcome
    }

    async fn load_stored_agent(
        &self,
        agent_id: &str,
        options: &LoadOptions,
        broadcast: Arc<AtomicBool>,
    ) -> Result<Arc<ManagedAgentView>> {
        let store = self.require_store()?.clone();
        let record = store
            .get(agent_id)
            .await
            .map_err(|e| CoreError::msg(e.to_string()))?
            .ok_or_else(|| CoreError::msg(format!("Agent not found: {agent_id}")))?;
        let valid = options
            .valid_providers
            .clone()
            .unwrap_or_else(|| self.registered_provider_ids());
        let unavailable = || {
            CoreError::msg(format!(
                "Agent {agent_id} references unavailable provider '{}'",
                record.provider
            ))
        };
        if !valid.contains(&record.provider) {
            return Err(unavailable());
        }
        let snapshot = match to_agent_persistence_handle(Some(&valid), &record.persistence) {
            Some(handle) => {
                let stamps = extract_timestamps(&record);
                self.resume_agent_from_persistence(
                    handle,
                    Some(build_config_overrides(&record)),
                    Some(agent_id),
                    ResumeOptions {
                        created_at_ms: Some(stamps.created_at_ms),
                        updated_at_ms: Some(stamps.updated_at_ms),
                        last_user_message_at_ms: stamps.last_user_message_at_ms,
                        labels: Some(record.labels.clone()),
                        workspace_id: record.workspace_id.clone(),
                        owner: record.owner.clone(),
                        attention: Some(extract_attention(&record)),
                    },
                    record.is_archived().then_some(ResumePurpose::History),
                )
                .await?
            }
            None => {
                // No provider handle to resume: this starts the agent's first session rather
                // than bringing one back, so it stamps activity and carries no stored attention.
                let config = build_session_config(&record, Some(&valid)).ok_or_else(unavailable)?;
                self.create_agent(
                    config,
                    Some(agent_id),
                    CreateAgentOptions {
                        labels: Some(record.labels.clone()),
                        workspace_id: record.workspace_id.clone(),
                        owner: record.owner.clone(),
                        ..CreateAgentOptions::default()
                    },
                )
                .await?
            }
        };
        self.hydrate_timeline_from_provider(
            agent_id,
            HydrateOpts {
                broadcast: Broadcast::Deferred(broadcast),
                ..HydrateOpts::default()
            },
        )
        .await?;
        Ok(self.get_agent(agent_id).unwrap_or(snapshot))
    }

    /// `ensureUnarchivedAgentLoaded`: an archived agent is never resumed for interactive use,
    /// and one archived while loading is closed again.
    pub async fn ensure_unarchived_agent_loaded(
        &self,
        agent_id: &str,
        options: LoadOptions,
    ) -> Result<Arc<ManagedAgentView>> {
        let store = self.require_store()?.clone();
        let archived = |record: Option<paseo_store::StoredAgentRecord>| {
            record.is_some_and(|r| r.is_archived())
        };
        let record = store
            .get(agent_id)
            .await
            .map_err(|e| CoreError::msg(e.to_string()))?;
        if archived(record) {
            return Err(CoreError::msg(format!("Agent is archived: {agent_id}")));
        }
        let agent = self.ensure_agent_loaded(agent_id, options).await?;
        // DEVIATION(protected-load-waits-for-lane): a stored-only archive queued behind the load
        // on the lifecycle lane has not written its record when the load returns, so the second
        // read could still see the agent as active. Waiting for the lane makes it see the
        // archive (TS gets the same result from its I/O timing).
        self.wait_for_agent_close(agent_id).await;
        let latest = store
            .get(agent_id)
            .await
            .map_err(|e| CoreError::msg(e.to_string()))?;
        if archived(latest) {
            if let Err(err) = self.close_agent(agent_id).await {
                warn!(%err, %agent_id, "Failed to close concurrently archived agent");
            }
            return Err(CoreError::msg(format!("Agent is archived: {agent_id}")));
        }
        Ok(agent)
    }
}
