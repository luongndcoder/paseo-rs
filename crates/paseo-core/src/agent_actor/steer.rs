//! Actor-side pieces of steer admission (`agent-manager.ts:2831-2991`).

use paseo_provider_api::PromptInput;

use super::timeline_record::{PromptOutcome, PromptRecordOptions};
use super::AgentActor;
use crate::error::{CoreError, Result};

impl AgentActor {
    /// `assertSteerAdmissionOwnsTurn`.
    pub(crate) fn assert_steer_owns_turn(&self, expected_turn_id: &str) -> Result<()> {
        if self.view.active_turn_id.as_deref() == Some(expected_turn_id) {
            Ok(())
        } else {
            Err(CoreError::msg(
                "Active turn changed before steering could be delivered",
            ))
        }
    }

    /// Admission, one atomic block: apply delivered events, flush this agent's coalescer
    /// buffer, assert the turn is still the expected one, install the barrier.
    pub(crate) fn begin_steer_admission(&mut self, expected_turn_id: &str) -> Result<u64> {
        self.drain_events();
        self.flush_coalescer();
        self.assert_steer_owns_turn(expected_turn_id)?;
        Ok(self.gate.install_barrier())
    }

    /// Release the barrier and re-queue what it held (the `finally` of `runSteerAdmission`).
    pub(crate) fn end_steer_admission(&mut self, barrier_id: u64) {
        let held = self.gate.release_barrier(barrier_id);
        self.requeue(held);
    }

    /// `recordAcceptedSteer`: the accepted prompt is canonical, tied to the steered turn.
    pub(crate) fn record_accepted_steer(
        &mut self,
        prompt: &PromptInput,
        client_message_id: &str,
        expected_turn_id: &str,
    ) {
        self.record_submitted_prompt(
            prompt,
            client_message_id,
            PromptOutcome::Accepted,
            PromptRecordOptions {
                message_id: Some(client_message_id.to_owned()),
                turn_id: Some(expected_turn_id.to_owned()),
                ..PromptRecordOptions::default()
            },
        );
        self.emit_state(true);
    }

    /// `replaceAgentRun` / `replaceAdmittedForegroundTurn` preamble: hold `running` while the
    /// old run is cancelled so no `idle` frame is painted in between.
    pub(crate) fn begin_replacement(&mut self) {
        self.view.pending_replacement = true;
        self.view.lifecycle = crate::model::Lifecycle::Running;
        self.touch_updated_at();
        self.emit_state(true);
    }
}
