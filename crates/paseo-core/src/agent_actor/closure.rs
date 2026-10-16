//! Actor-side pieces of close: coalescer flush, provider-subagent cancel, closure.

use paseo_agent_types::{AgentStreamEvent, ProviderSubagentInputEvent, ProviderSubagentStatus};
use paseo_timeline::store::SeedOptions;
use paseo_timeline::TimelineState;

use super::AgentActor;
use crate::events::ManagerEvent;
use crate::model::{Lifecycle, ManagedAgentView};

impl AgentActor {
    /// `agentStreamCoalescer.flushAll()` for this agent.
    pub(crate) fn flush_all_coalescer(&mut self) {
        let flushes = self
            .coalescer
            .flush_all(tokio::time::Instant::now().into_std());
        self.apply_flushes(flushes);
    }

    /// `cancelRunningProviderSubagents`: running children become `canceled`.
    pub(crate) fn cancel_running_provider_subagents(&mut self) {
        let parent = self.view.id.clone();
        let running: Vec<(String, String)> = {
            let store = self.ctx.subagents.lock().unwrap_or_else(|e| e.into_inner());
            store
                .list(&parent)
                .into_iter()
                .filter(|s| s.status == ProviderSubagentStatus::Running)
                .map(|s| (s.id, s.provider))
                .collect()
        };
        for (id, provider) in running {
            let input = ProviderSubagentInputEvent::Upsert {
                id,
                title: None,
                description: None,
                status: Some(ProviderSubagentStatus::Canceled),
                tool_call_id: None,
                cwd: None,
                subtitle: None,
                parent_subagent_id: None,
                timestamp: None,
            };
            let now = self.now_iso();
            let event = self
                .ctx
                .subagents
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .apply(&parent, &provider, &input, &now);
            self.ctx.bus.emit(ManagerEvent::ProviderSubagent {
                event,
                parent_internal: self.view.internal,
            });
        }
    }

    /// `prepareAgentForClosure`, one atomic step: flush and discard the coalescer, leave the
    /// registry, drop the session subscription, cancel waiters with `turn_canceled{reason}`,
    /// clear the run. The actor ends afterwards. The timeline is retained for a later
    /// registration of the same id (TS keeps `timelineStore` across close).
    pub(crate) fn prepare_for_closure(&mut self, cancel_reason: &str) -> ManagedAgentView {
        let flushes = self
            .coalescer
            .flush_and_discard(&self.view.id, tokio::time::Instant::now().into_std());
        self.apply_flushes(flushes);
        self.ctx
            .registry
            .remove_generation(&self.view.id, self.generation);
        self.previous_status = None;
        self.events = None;
        let provider = self.view.provider.clone();
        let reason = cancel_reason.to_owned();
        self.waiters
            .cancel_all(|turn_id| AgentStreamEvent::TurnCanceled {
                provider: provider.clone(),
                reason: reason.clone(),
                turn_id: Some(turn_id.to_owned()),
            });
        self.run.clear();
        let mut closed = self.snapshot();
        closed.lifecycle = Lifecycle::Closed;
        closed.active_foreground_turn_id = None;
        closed.active_turn_id = None;
        closed.active_turn_started_at_ms = None;
        closed.pending_permissions.clear();
        closed.pending_replacement = false;
        closed.has_run = false;
        closed.pending_run_start = None;
        let timeline = std::mem::replace(
            &mut self.timeline,
            TimelineState::initialize(SeedOptions::default()),
        );
        self.ctx.registry.retain_timeline(&self.view.id, timeline);
        self.terminated = true;
        closed
    }
}
