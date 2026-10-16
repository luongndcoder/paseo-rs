//! `reload_agent_session` (`agent-manager.ts:1548-1693`): hot-reload a live agent's session
//! with config overrides, on the lifecycle lane.
//!
//! Order: cancel any run, prepare config and launch context and check MCP support BEFORE the
//! old session is touched, close the old session once (memoized, bounded by 10 s), drain
//! events, terminalize provider children, prepare closure, persist the closed snapshot (a
//! failure aborts: no replacement is created), resume (or create when there is no handle),
//! register with the preserved state. A failure after closure emits the closed state with
//! `persist=false`; a failure before leaves the live agent in `error`.

use std::sync::Arc;

use paseo_provider_api::JsonObject;
use serde_json::Value;
use tokio::sync::watch;
use tracing::warn;

use crate::agent_actor::ActorHandle;
use crate::config::{config_to_object, merge_config};
use crate::error::{CoreError, Result};
use crate::lifecycle::create::close_unregistered_session;
use crate::lifecycle::register::RegisterOptions;
use crate::manager::config_prep::{OpenReason, Opening, ResumePurpose};
use crate::manager::AgentManager;
use crate::model::{ManagedAgentView, ReloadOptions};

/// `RELOAD_SESSION_CLOSE_TIMEOUT_ERROR`.
pub const RELOAD_SESSION_CLOSE_TIMEOUT_ERROR: &str =
    "Timed out closing previous session during refresh";

/// `stripTransientReloadError`.
fn strip_transient_reload_error(last_error: Option<String>) -> Option<String> {
    last_error.filter(|e| e != RELOAD_SESSION_CLOSE_TIMEOUT_ERROR)
}

impl AgentManager {
    /// `reloadAgentSession`.
    pub async fn reload_agent_session(
        &self,
        agent_id: &str,
        overrides: Option<JsonObject>,
        options: ReloadOptions,
    ) -> Result<Arc<ManagedAgentView>> {
        let _tracked = self.inner.registrations.track();
        let key = agent_id.to_owned();
        self.inner
            .lifecycle_lanes
            .run(&key, || self.reload_inner(agent_id, overrides, options))
            .await
    }

    async fn reload_inner(
        &self,
        agent_id: &str,
        overrides: Option<JsonObject>,
        options: ReloadOptions,
    ) -> Result<Arc<ManagedAgentView>> {
        self.assert_accepting_registrations()?;
        let mut existing = self.require_agent(agent_id)?;
        if existing.view().has_in_flight_run() {
            self.cancel_agent_run_before(agent_id, "reload").await?;
            existing = self.require_agent(agent_id)?;
        }
        let view = existing.view();
        let rehydrate = options.rehydrate_from_disk;
        let preserved_history_primed = view.history_primed;
        let preserved_last_usage = view.last_usage.clone();
        let preserved_last_error = strip_transient_reload_error(view.last_error.clone());
        let preserved_attention = view.attention.clone();
        let handle = view.persistence.clone();
        let provider = handle
            .as_ref()
            .map_or_else(|| view.provider.clone(), |h| h.provider.clone());
        let client = self.require_client(&provider)?;
        let refresh = merge_config(
            &config_to_object(&view.config),
            overrides.as_ref(),
            &provider,
        );
        let prepared = self
            .prepare_session_config(refresh, agent_id, ResumePurpose::Interactive)
            .await?;
        let previous_policy = self.tool_policy_entry(agent_id);
        let context = self
            .build_launch_context(
                agent_id,
                &client,
                &prepared.stored.cwd,
                prepared.policy.as_ref(),
                None,
                Opening {
                    reason: OpenReason::Refresh,
                    purpose: ResumePurpose::Interactive,
                    workspace_id: view.workspace_id.clone(),
                },
            )
            .await?;
        let launch = Self::resolve_provider_launch_config(prepared.launch, &context);
        let wants_mcp =
            matches!(&prepared.stored.mcp_servers, Some(Value::Object(s)) if !s.is_empty());
        if wants_mcp
            && existing.session.capabilities().get("supportsMcpServers") != Some(&Value::Bool(true))
        {
            return Err(CoreError::msg(format!(
                "Provider '{provider}' does not support MCP servers"
            )));
        }

        let mut session = None;
        let mut closed_existing: Option<Arc<ManagedAgentView>> = None;
        let mut handed = false;
        let outcome: Result<Arc<ManagedAgentView>> = async {
            // A persisted thread can have only one writer, even when its turn is idle.
            self.close_reloaded_session(&existing).await?;
            existing.ask(|a| a.drain_events()).await?;
            existing
                .ask(|a| a.cancel_running_provider_subagents())
                .await?;
            let closed = Arc::new(
                existing
                    .ask(|a| a.prepare_for_closure("agent reloaded"))
                    .await?,
            );
            closed_existing = Some(closed.clone());
            self.persist_view(&closed, Default::default()).await?;
            self.assert_accepting_registrations()?;

            self.set_tool_policy(agent_id, prepared.policy.clone());
            let new_session = match &handle {
                Some(handle) => {
                    let overrides = config_to_object(&launch);
                    client
                        .resume_session(handle, Some(&overrides), Some(&context), None)
                        .await?
                }
                None => client.create_session(&launch, Some(&context), None).await?,
            };
            let new_session = session.insert(new_session);
            self.require_external_mcp_support(new_session.as_ref(), &prepared.stored)
                .await?;
            self.assert_accepting_registrations()?;

            if rehydrate {
                // Wipe the timeline so registration mints a new epoch and the next hydration
                // re-streams the freshly read provider history.
                drop(self.inner.registry.take_retained_timeline(agent_id));
                let removed = self
                    .inner
                    .ctx
                    .subagents
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .delete_parent(agent_id);
                for update in removed {
                    self.dispatch_subagent_update(update);
                }
            }

            handed = true;
            let new_session = session.take().expect("session was just stored");
            self.register_session(
                new_session,
                prepared.stored.clone(),
                agent_id,
                RegisterOptions {
                    labels: Some(view.labels.clone()),
                    workspace_id: view.workspace_id.clone(),
                    owner: view.owner.clone(),
                    created_at_ms: Some(view.created_at_ms),
                    updated_at_ms: Some(view.updated_at_ms),
                    last_user_message_at_ms: view.last_user_message_at_ms,
                    history_primed: Some(if rehydrate {
                        false
                    } else {
                        preserved_history_primed
                    }),
                    last_usage: preserved_last_usage.clone(),
                    last_error: preserved_last_error.clone(),
                    attention: preserved_attention.clone(),
                    restoring: true,
                    ..RegisterOptions::default()
                },
            )
            .await
        }
        .await;

        match outcome {
            Ok(agent) => Ok(agent),
            Err(error) => {
                if let Some(closed) = &closed_existing {
                    self.emit_closed_agent(closed);
                } else if self
                    .inner
                    .registry
                    .get(agent_id)
                    .is_some_and(|h| h.generation == existing.generation)
                {
                    let message = error.to_string();
                    let _ = existing
                        .ask(move |a| {
                            a.view.lifecycle = crate::model::Lifecycle::Error;
                            a.view.last_error = Some(message);
                            a.emit_state(true);
                        })
                        .await;
                }
                if !handed {
                    self.restore_tool_policy(agent_id, previous_policy);
                    if let Some(session) = session {
                        close_unregistered_session(session.as_ref()).await;
                    }
                }
                Err(error)
            }
        }
    }

    /// `closeReloadedSession`: one close per session, shared by retries, bounded by the
    /// reload-close timeout. A close that fails before the timeout fails the reload; one that
    /// fails after it is only logged.
    async fn close_reloaded_session(&self, handle: &ActorHandle) -> Result<()> {
        let mut receiver = self.start_reloaded_close(handle);
        let limit = self.inner.options.reload_session_close_timeout;
        let finished = {
            let wait = receiver.wait_for(|r| r.is_some());
            match tokio::time::timeout(limit, wait).await {
                Ok(Ok(done)) => Some(done.clone()),
                Ok(Err(_)) => Some(None),
                Err(_) => None,
            }
        };
        match finished {
            Some(Some(Err(message))) => Err(CoreError::Message(message)),
            Some(_) => Ok(()),
            None => {
                let id = handle.id.clone();
                tokio::spawn(async move {
                    if let Ok(done) = receiver.wait_for(|r| r.is_some()).await {
                        if let Some(Err(error)) = done.clone() {
                            warn!(%error, agent_id = %id, "Previous session close failed after refresh timeout");
                        }
                    }
                });
                Err(CoreError::msg(RELOAD_SESSION_CLOSE_TIMEOUT_ERROR))
            }
        }
    }

    fn start_reloaded_close(
        &self,
        handle: &ActorHandle,
    ) -> watch::Receiver<crate::manager::CloseResult> {
        let mut closes = self
            .inner
            .reloaded_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = closes.get(&handle.generation) {
            return existing.clone();
        }
        let (tx, rx) = watch::channel(None);
        closes.insert(handle.generation, rx.clone());
        let session = handle.session.clone();
        let inner = self.inner.clone();
        let generation = handle.generation;
        tokio::spawn(async move {
            let result = session.close().await.map_err(|e| e.to_string());
            if result.is_err() {
                // A failed close is not remembered: the retry closes again.
                inner
                    .reloaded_sessions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&generation);
            }
            let _ = tx.send(Some(result));
        });
        rx
    }
}
