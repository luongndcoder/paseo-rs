//! `emit_state`, attention edge detection, persistence enqueue and stream dispatch.

use std::sync::Arc;

use paseo_agent_types::{AgentStreamEvent, AttentionReason};
use paseo_timeline::limits::limit_item_content;
use tracing::error;

use super::AgentActor;
use crate::events::ManagerEvent;
use crate::hooks::{AttentionNotice, PersistOptions};
use crate::model::{AttentionState, Lifecycle, ManagedAgentView};
use crate::time::{iso8601_from_millis, next_updated_at};

/// Metadata of an `agent_stream` dispatch (`seq`, `epoch`, `timestamp`).
#[derive(Debug, Clone, Default)]
pub(crate) struct StreamMeta {
    pub seq: Option<i64>,
    pub epoch: Option<String>,
    pub timestamp: Option<String>,
}

impl StreamMeta {
    pub fn timestamp_only(timestamp: String) -> Self {
        Self {
            timestamp: Some(timestamp),
            ..Self::default()
        }
    }
}

impl AgentActor {
    pub(crate) fn now_ms(&self) -> i64 {
        self.ctx.clock.now_ms()
    }

    pub(crate) fn now_iso(&self) -> String {
        iso8601_from_millis(self.now_ms())
    }

    /// The view plus the run slot summary (what is emitted and published).
    pub(crate) fn snapshot(&self) -> ManagedAgentView {
        let mut view = self.view.clone();
        view.has_run = self.run.has_run();
        view.pending_run_start = self.run.pending().map(|run| run.start.clone());
        view
    }

    /// `touchUpdatedAt`: `max(now, previous + 1)`, so `updatedAt` is monotonic per agent.
    pub(crate) fn touch_updated_at(&mut self) -> i64 {
        let next = next_updated_at(self.view.updated_at_ms, self.now_ms());
        self.view.updated_at_ms = next;
        next
    }

    /// `emitState`. Attention detection always runs first; then features sync from the
    /// session, the persist is enqueued (unless `persist` is false), and `agent_state` is
    /// dispatched. The attention mutation precedes the snapshot, so the emitted state has it.
    pub(crate) fn emit_state(&mut self, persist: bool) {
        self.check_and_set_attention();
        self.view.features = self.session.features().to_vec();
        let snapshot = Arc::new(self.snapshot());
        if persist {
            self.enqueue_background_persist(&snapshot);
        }
        self.ctx
            .bus
            .emit(ManagerEvent::AgentState { agent: snapshot });
    }

    /// `checkAndSetAttention`: edge-triggered, never level-triggered.
    fn check_and_set_attention(&mut self) {
        let previous = self.previous_status;
        let current = self.view.lifecycle;
        self.previous_status = Some(current);
        if self.view.internal || self.view.attention.requires_attention() {
            return;
        }
        let reason = if previous == Some(Lifecycle::Running) && current == Lifecycle::Idle {
            AttentionReason::Finished
        } else if previous != Some(Lifecycle::Error) && current == Lifecycle::Error {
            AttentionReason::Error
        } else {
            return;
        };
        self.view.attention = AttentionState::Required {
            reason,
            timestamp_ms: self.now_ms(),
        };
        self.broadcast_attention(reason);
    }

    /// `broadcastAgentAttention`: delegated agents never notify.
    pub(crate) fn broadcast_attention(&self, reason: AttentionReason) {
        if self.view.is_delegated() {
            return;
        }
        self.ctx.hooks.on_attention(AttentionNotice {
            agent_id: self.view.id.clone(),
            provider: self.view.provider.clone(),
            reason,
        });
    }

    /// `enqueueBackgroundPersist`: fire and forget, errors logged. Internal agents never persist.
    fn enqueue_background_persist(&self, snapshot: &Arc<ManagedAgentView>) {
        let Some(persister) = self.ctx.persister.clone() else {
            return;
        };
        if snapshot.internal {
            return;
        }
        let snapshot = snapshot.clone();
        // The write order is fixed here, in the actor step, like TS's synchronous `applySnapshot`.
        let mut link = self.ctx.persist_chain.begin(&snapshot.id);
        self.ctx.background.spawn(async move {
            link.wait_turn().await;
            if let Err(err) = persister
                .persist(&snapshot, PersistOptions::default())
                .await
            {
                error!(agent_id = %snapshot.id, %err, "Failed to persist agent snapshot");
            }
        });
    }

    /// `dispatchStream`: broadcast one stream event; non-timeline events also go to the plugin
    /// stream hook for live, non-internal agents.
    pub(crate) fn dispatch_stream(&self, event: AgentStreamEvent, meta: StreamMeta) {
        let event = limit_stream_event(event);
        let hook_event = (!self.view.internal
            && !matches!(event, AgentStreamEvent::Timeline { .. }))
        .then(|| event.clone());
        self.ctx.bus.emit(ManagerEvent::AgentStream {
            agent_id: self.view.id.clone(),
            event,
            seq: meta.seq,
            epoch: meta.epoch,
            timestamp: meta.timestamp,
            internal: self.view.internal,
        });
        if let Some(event) = hook_event {
            self.ctx
                .hooks
                .publish_agent_stream(&self.view, &event, &self.timeline);
        }
    }
}

/// `limitAgentStreamEventContent`: only timeline items are limited.
pub(crate) fn limit_stream_event(event: AgentStreamEvent) -> AgentStreamEvent {
    match event {
        AgentStreamEvent::Timeline {
            item,
            provider,
            turn_id,
            timestamp,
        } => AgentStreamEvent::Timeline {
            item: Box::new(limit_item_content(*item)),
            provider,
            turn_id,
            timestamp,
        },
        other => other,
    }
}
