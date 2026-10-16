//! `closeAgent` (`agent-manager.ts:1761-1821`).
//!
//! Concurrent calls share one in-flight close. The sequence: drain events, `session.close()`
//! (a failure keeps the runtime: no closed snapshot is published and the next attempt retries),
//! cancel running provider subagents, `prepare_for_closure`, persist the closed snapshot (the
//! error is remembered), emit the closed agent (state, plus plugin `agent.closed` unless
//! internal), then rethrow the persist error.

use std::sync::Arc;

use serde_json::json;
use tokio::sync::watch;

use crate::error::{CoreError, Result};
use crate::events::ManagerEvent;
use crate::hooks::PersistOptions;
use crate::manager::AgentManager;
use crate::model::ManagedAgentView;

impl AgentManager {
    /// `closeAgent`. A no-op when the agent is already gone (a reload or archive closed it).
    pub async fn close_agent(&self, agent_id: &str) -> Result<()> {
        let sender = {
            let mut closes = self
                .inner
                .in_flight_closes
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = closes.get(agent_id) {
                Err(existing.clone())
            } else {
                let (tx, rx) = watch::channel(None);
                closes.insert(agent_id.to_owned(), rx);
                Ok(tx)
            }
        };
        match sender {
            Err(mut existing) => {
                let Ok(done) = existing.wait_for(|r| r.is_some()).await else {
                    return Ok(());
                };
                match &*done {
                    Some(Err(message)) => Err(CoreError::Message(message.clone())),
                    _ => Ok(()),
                }
            }
            Ok(tx) => {
                let key = agent_id.to_owned();
                let result = self
                    .inner
                    .lifecycle_lanes
                    .run(&key, || async {
                        if self.inner.registry.get(agent_id).is_some() {
                            self.close_agent_runtime(agent_id).await
                        } else {
                            Ok(())
                        }
                    })
                    .await;
                self.inner
                    .in_flight_closes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(agent_id);
                let _ = tx.send(Some(
                    result.as_ref().map(|_| ()).map_err(ToString::to_string),
                ));
                result
            }
        }
    }

    /// `waitForAgentClose`: wait for queued lifecycle steps and an in-flight close.
    pub async fn wait_for_agent_close(&self, agent_id: &str) {
        self.inner
            .lifecycle_lanes
            .wait_idle(&agent_id.to_owned())
            .await;
        let existing = self
            .inner
            .in_flight_closes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(agent_id)
            .cloned();
        if let Some(mut rx) = existing {
            let _ = rx.wait_for(|r| r.is_some()).await;
        }
    }

    pub(crate) async fn close_agent_runtime(&self, agent_id: &str) -> Result<()> {
        let handle = self.require_agent(agent_id)?;
        handle.ask(|a| a.drain_events()).await?;
        // Ownership stays with the runtime until shutdown succeeds: a failed close may still
        // own a native writer, so no resumable closed snapshot is published.
        handle.session.close().await?;
        handle
            .ask(|a| a.cancel_running_provider_subagents())
            .await?;
        let closed = Arc::new(
            handle
                .ask(|a| a.prepare_for_closure("agent closed"))
                .await?,
        );
        let persist_error = self.persist_closed_snapshot(&closed).await.err();
        self.emit_closed_agent(&closed);
        match persist_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn persist_closed_snapshot(&self, closed: &Arc<ManagedAgentView>) -> Result<()> {
        let Some(persister) = &self.inner.ctx.persister else {
            return Ok(());
        };
        if closed.internal {
            return Ok(());
        }
        let mut link = self.inner.ctx.persist_chain.begin(&closed.id);
        link.wait_turn().await;
        persister
            .persist(closed, PersistOptions::default())
            .await
            .map_err(CoreError::msg)
    }

    /// `emitClosedAgent` with `persist=false`.
    pub(crate) fn emit_closed_agent(&self, closed: &Arc<ManagedAgentView>) {
        self.bus().emit(ManagerEvent::AgentState {
            agent: closed.clone(),
        });
        if !closed.internal {
            self.hooks()
                .plugin_emit("agent.closed", json!({"agent": describe_hook_view(closed)}));
        }
    }
}

/// `describeHookAgent`: the agent as plugin hooks see it.
pub(crate) fn describe_hook_agent(
    id: &str,
    workspace_id: Option<&str>,
    labels: &indexmap::IndexMap<String, String>,
    provider: &str,
    cwd: &str,
    title: Option<&str>,
) -> serde_json::Value {
    json!({
        "id": id,
        "workspaceId": workspace_id,
        "parentAgentId": labels.get(crate::model::PARENT_AGENT_ID_LABEL),
        "provider": provider,
        "cwd": cwd,
        "title": title,
    })
}

pub(crate) fn describe_hook_view(agent: &ManagedAgentView) -> serde_json::Value {
    describe_hook_agent(
        &agent.id,
        agent.workspace_id.as_deref(),
        &agent.labels,
        &agent.provider,
        &agent.cwd,
        agent.config.title.as_deref(),
    )
}
