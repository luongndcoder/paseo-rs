//! Manager reads and small state mutations: published views, timelines, attention.
//!
//! `get_agent`, `list_agents`, `has_in_flight_run` and `get_pending_permissions` read the view
//! each actor publishes after every handled message, so they need no mailbox round trip. TS reads
//! live mutated objects, so a read between two emits can see unemitted fields; publishing per
//! message is the closest equivalent (decision Q17, covered by the `reads_*` tests).

use std::sync::Arc;

use paseo_agent_types::{AgentTimelineItem, PermissionRequest};
use paseo_timeline::store::{FetchOptions, FetchResult};
use paseo_timeline::ProjectedEntry;

use super::AgentManager;
use crate::agent_actor::ActorHandle;
use crate::error::{CoreError, Result};
use crate::hooks::PersistOptions;
use crate::model::{AttentionState, Lifecycle, ManagedAgentView};
use crate::provider_subagents::SubagentDescriptor;
use paseo_agent_types::AttentionReason;
use paseo_store::Nullable;

/// `getLastAssistantMessageSegmentFromTimeline` text: the last run of contiguous assistant rows.
pub(crate) fn last_assistant_message(timeline: &[AgentTimelineItem]) -> Option<String> {
    let mut chunks: Vec<&str> = Vec::new();
    for item in timeline.iter().rev() {
        match item {
            AgentTimelineItem::AssistantMessage { text, .. } => chunks.push(text),
            _ if !chunks.is_empty() => break,
            _ => {}
        }
    }
    if chunks.is_empty() {
        return None;
    }
    chunks.reverse();
    Some(chunks.concat())
}

impl AgentManager {
    /// `getAgent`: the live agent's published view, `None` once closed.
    pub fn get_agent(&self, id: &str) -> Option<Arc<ManagedAgentView>> {
        self.inner.registry.get(id).map(|h| h.view())
    }

    /// `listAgents`.
    pub fn list_agents(&self, include_internal: bool) -> Vec<Arc<ManagedAgentView>> {
        self.inner
            .registry
            .all()
            .into_iter()
            .map(|h| h.view())
            .filter(|view| include_internal || !view.internal)
            .collect()
    }

    /// `hasInFlightRun`.
    pub fn has_in_flight_run(&self, id: &str) -> bool {
        self.get_agent(id).is_some_and(|v| v.has_in_flight_run())
    }

    /// `getPendingPermissions` (requires a managed session).
    pub fn get_pending_permissions(&self, id: &str) -> Result<Vec<PermissionRequest>> {
        let handle = self.require_agent(id)?;
        Ok(handle
            .view()
            .pending_permissions
            .values()
            .cloned()
            .collect())
    }

    pub async fn fetch_timeline(&self, id: &str, options: FetchOptions) -> Result<FetchResult> {
        let handle = self.require_agent(id)?;
        handle.ask(move |a| a.timeline.fetch(&options)).await
    }

    /// `getTimeline`: items of the projected rows.
    pub async fn get_timeline(&self, id: &str) -> Result<Vec<AgentTimelineItem>> {
        let handle = self.require_agent(id)?;
        handle.ask(|a| a.timeline.items()).await
    }

    /// `getTimelineRows`: projected rows.
    pub async fn get_timeline_rows(&self, id: &str) -> Result<Vec<ProjectedEntry>> {
        let handle = self.require_agent(id)?;
        handle.ask(|a| a.timeline.rows().to_vec()).await
    }

    /// `getLastAssistantMessage`: `None` for an agent that is not live.
    pub async fn get_last_assistant_message(&self, id: &str) -> Option<String> {
        let handle = self.inner.registry.get(id)?;
        handle
            .ask(|a| last_assistant_message(&a.timeline.items()))
            .await
            .ok()
            .flatten()
    }

    /// `appendTimelineItem`: record and dispatch, then persist (awaited, errors propagate).
    pub async fn append_timeline_item(
        &self,
        id: &str,
        item: AgentTimelineItem,
    ) -> Result<(i64, String)> {
        let handle = self.require_agent(id)?;
        let appended = handle.ask(move |a| a.append_timeline_item(item)).await?;
        self.persist_snapshot(&handle, PersistOptions::default())
            .await?;
        Ok(appended)
    }

    /// `emitLiveTimelineItem`: broadcast only.
    pub async fn emit_live_timeline_item(&self, id: &str, item: AgentTimelineItem) -> Result<()> {
        let handle = self.require_agent(id)?;
        handle.ask(move |a| a.emit_live_timeline_item(item)).await
    }

    /// `persistSnapshot`: awaited write of the current view. Internal agents never persist.
    pub(crate) async fn persist_snapshot(
        &self,
        handle: &ActorHandle,
        options: PersistOptions,
    ) -> Result<()> {
        if self.inner.ctx.persister.is_none() {
            return Ok(());
        }
        // Take the place in the write order inside the actor step, next to the snapshot read.
        let (view, link) = handle
            .ask(|a| {
                (
                    Arc::new(a.snapshot()),
                    a.ctx.persist_chain.begin(&a.view.id),
                )
            })
            .await?;
        self.persist_view_in_order(&view, options, link).await
    }

    /// `persistSnapshot` for a view that is no longer (or not yet) behind an actor, such as the
    /// closed snapshot of a reload.
    pub(crate) async fn persist_view(
        &self,
        view: &Arc<ManagedAgentView>,
        options: PersistOptions,
    ) -> Result<()> {
        let link = self.inner.ctx.persist_chain.begin(&view.id);
        self.persist_view_in_order(view, options, link).await
    }

    async fn persist_view_in_order(
        &self,
        view: &Arc<ManagedAgentView>,
        options: PersistOptions,
        mut link: crate::persist_chain::Link,
    ) -> Result<()> {
        let Some(persister) = self.inner.ctx.persister.clone() else {
            return Ok(());
        };
        // Internal agents are ephemeral system tasks and never touch disk.
        if view.internal {
            return Ok(());
        }
        link.wait_turn().await;
        persister
            .persist(view, options)
            .await
            .map_err(CoreError::msg)
    }

    /// `notifyAgentState`: touch and emit for a live, non-internal agent; silent otherwise.
    pub async fn notify_agent_state(&self, id: &str) {
        let Some(handle) = self.inner.registry.get(id) else {
            return;
        };
        let _ = handle
            .ask(|a| {
                if a.view.internal {
                    return;
                }
                a.touch_updated_at();
                a.emit_state(true);
            })
            .await;
    }

    /// `clearAgentAttention` (live agents; stored-only agents arrive with the store seam).
    pub async fn clear_agent_attention(&self, id: &str) -> Result<()> {
        let handle = self.require_agent(id)?;
        let had_attention = handle
            .ask(|a| {
                let had = a.view.attention.requires_attention();
                a.view.attention = AttentionState::None;
                had
            })
            .await?;
        if had_attention {
            self.persist_snapshot(&handle, PersistOptions::default())
                .await?;
            handle.ask(|a| a.emit_state(false)).await?;
        }
        Ok(())
    }

    /// `markAgentUnread`: a live agent that is finished, read and permission-free, or a stored
    /// agent that is idle or closed, not internal, not archived and read.
    pub async fn mark_agent_unread(&self, id: &str) -> Result<()> {
        let not_finished = || CoreError::msg(format!("Agent is no longer finished and read: {id}"));
        let Some(handle) = self.inner.registry.get(id) else {
            let store = self.require_store()?.clone();
            let record = store
                .get(id)
                .await
                .map_err(|e| CoreError::msg(e.to_string()))?;
            let Some(record) = record.filter(|r| {
                matches!(r.last_status, Lifecycle::Idle | Lifecycle::Closed)
                    && r.internal != Some(true)
                    && !r.is_archived()
                    && r.requires_attention != Some(true)
            }) else {
                return Err(not_finished());
            };
            let updated_at = self.next_stored_updated_at(&record);
            let mut next = record;
            next.updated_at = updated_at.clone();
            next.requires_attention = Some(true);
            next.attention_reason =
                Nullable::Value(paseo_store::agent_record::AttentionReason::Finished);
            next.attention_timestamp = Nullable::Value(updated_at);
            store
                .upsert(next.clone())
                .await
                .map_err(|e| CoreError::msg(e.to_string()))?;
            self.dispatch_stored_agent_state(&next);
            return Ok(());
        };
        let marked = handle
            .ask(|a| {
                let can_mark = a.view.lifecycle == Lifecycle::Idle
                    && !a.view.attention.requires_attention()
                    && a.view.pending_permissions.is_empty();
                if can_mark {
                    a.view.attention = AttentionState::Required {
                        reason: AttentionReason::Finished,
                        timestamp_ms: a.now_ms(),
                    };
                }
                can_mark
            })
            .await?;
        if !marked {
            return Err(not_finished());
        }
        self.persist_snapshot(&handle, PersistOptions::default())
            .await?;
        handle.ask(|a| a.emit_state(false)).await
    }

    /// `listProviderSubagents`: unknown for internal parents.
    pub fn list_provider_subagents(&self, parent: &str) -> Result<Vec<SubagentDescriptor>> {
        self.require_public_agent(parent)?;
        Ok(self.subagents().list(parent))
    }

    /// `listProviderSubagentActivity`: children of public live agents.
    pub fn list_provider_subagent_activity(&self) -> Vec<SubagentDescriptor> {
        let public: std::collections::HashSet<String> = self
            .list_agents(false)
            .iter()
            .map(|v| v.id.clone())
            .collect();
        self.subagents()
            .list_all()
            .into_iter()
            .filter(|s| public.contains(&s.parent_agent_id))
            .collect()
    }

    pub fn get_provider_subagent(
        &self,
        parent: &str,
        sub: &str,
    ) -> Result<Option<SubagentDescriptor>> {
        self.require_public_agent(parent)?;
        Ok(self.subagents().get(parent, sub))
    }

    pub fn fetch_provider_subagent_timeline(
        &self,
        parent: &str,
        sub: &str,
        options: &FetchOptions,
    ) -> Result<Option<FetchResult>> {
        self.require_public_agent(parent)?;
        Ok(self.subagents().fetch_timeline(parent, sub, options))
    }

    /// Test seam: mutate the live view the way TS tests poke `getAgent(id)` internals.
    #[cfg(any(test, feature = "testing"))]
    pub async fn testing_mutate_agent(
        &self,
        id: &str,
        mutate: impl FnOnce(&mut ManagedAgentView) + Send + 'static,
    ) -> Result<()> {
        let handle = self.require_agent(id)?;
        handle.ask(move |a| mutate(&mut a.view)).await
    }

    /// Test seam: drop an agent from the live registry without closing it or touching storage,
    /// the way TS tests `manager.agents.delete(id)` to make an agent "off memory".
    #[cfg(any(test, feature = "testing"))]
    pub fn testing_forget_agent(&self, id: &str) {
        self.inner
            .registry
            .agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shift_remove(id);
    }

    fn require_public_agent(&self, id: &str) -> Result<ActorHandle> {
        let handle = self.require_agent(id)?;
        if handle.view().internal {
            return Err(CoreError::unknown_agent(id));
        }
        Ok(handle)
    }

    fn subagents(
        &self,
    ) -> std::sync::MutexGuard<'_, crate::provider_subagents::ProviderSubagentStore> {
        self.inner
            .ctx
            .subagents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(text: &str) -> AgentTimelineItem {
        AgentTimelineItem::AssistantMessage {
            text: text.into(),
            message_id: None,
        }
    }

    #[test]
    fn last_assistant_message_joins_the_trailing_run_only() {
        let items = vec![
            assistant("old"),
            AgentTimelineItem::Reasoning { text: "r".into() },
            assistant("a"),
            assistant("b"),
        ];
        assert_eq!(last_assistant_message(&items).as_deref(), Some("ab"));
        let trailing_other = vec![
            assistant("a"),
            AgentTimelineItem::Reasoning { text: "r".into() },
        ];
        assert_eq!(
            last_assistant_message(&trailing_other).as_deref(),
            Some("a")
        );
        assert_eq!(last_assistant_message(&[]), None);
    }
}
