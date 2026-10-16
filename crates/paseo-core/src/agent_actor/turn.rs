//! The foreground turn lifecycle: admission, the accept block, start failure and finalization.

use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem, PersistenceHandle};
use paseo_provider_api::PromptInput;
use tokio::sync::mpsc;

use super::emit::StreamMeta;
use super::run_state::{RunTicket, FINALIZED_TURN_LIMIT};
use super::timeline_record::{PromptOutcome, PromptRecordOptions};
use super::turn_identity::apply_active_turn_terminal;
use super::{attach_persistence_cwd, AgentActor};
use crate::bridge::sync_probe;
use crate::error::{CoreError, Result};
use crate::model::{Lifecycle, RunStart};
use crate::time::iso8601_from_millis;

pub(crate) struct Admission {
    pub ticket: RunTicket,
    pub is_replacement: bool,
}

/// Result of the accept block: the waiter the stream reads its events from.
pub(crate) struct Accepted {
    pub waiter_id: u64,
    pub events: mpsc::UnboundedReceiver<AgentStreamEvent>,
}

impl AgentActor {
    /// Stream admission (`streamAgent`, `agent-manager.ts:2602-2650`): check no run, clear
    /// `lastError`, create the pending run.
    pub(crate) fn admit_stream(&mut self) -> Result<Admission> {
        if self.view.active_foreground_turn_id.is_some() || self.run.has_run() {
            return Err(CoreError::msg(format!(
                "Agent {} already has an active run",
                self.view.id
            )));
        }
        let is_replacement = self.view.pending_replacement;
        self.view.last_error = None;
        let ticket = self.run.create_pending_run();
        Ok(Admission {
            ticket,
            is_replacement,
        })
    }

    pub(crate) fn open_active_turn(&mut self, turn_id: &str) {
        self.view.active_turn_id = Some(turn_id.to_owned());
        self.view.active_turn_started_at_ms = Some(self.now_ms());
    }

    fn remember_finalized(&mut self, turn_id: &str) {
        self.finalized.insert(turn_id.to_owned());
        if self.finalized.len() > FINALIZED_TURN_LIMIT {
            self.finalized.shift_remove_index(0);
        }
    }

    /// The accept block (`agent-manager.ts:2672-2712`), one atomic step: run `Started`,
    /// `activeForegroundTurnId`, lifecycle `running`, touch, `turn_started` BEFORE the canonical
    /// `user_message` row, the staged provider echo supplies `providerMessageId` and is not
    /// replayed, the staged `turn_started` of the accepted id is dropped, the remaining staged
    /// events re-route behind the event queue, then `emit_state`.
    pub(crate) fn accept_turn(
        &mut self,
        token: &str,
        turn_id: &str,
        prompt: &PromptInput,
        client_message_id: Option<&str>,
        is_replacement: bool,
    ) -> Accepted {
        if is_replacement {
            self.view.pending_replacement = false;
        }
        let started_at_ms = self.now_ms();
        if let Some(run) = self.run.pending_mut().filter(|run| run.token == token) {
            run.start = RunStart::Started {
                turn_id: turn_id.to_owned(),
            };
        }
        self.view.active_foreground_turn_id = Some(turn_id.to_owned());
        self.open_active_turn(turn_id);
        self.view.lifecycle = Lifecycle::Running;
        self.touch_updated_at();
        self.dispatch_stream(
            AgentStreamEvent::TurnStarted {
                provider: self.view.provider.clone(),
                turn_id: Some(turn_id.to_owned()),
            },
            StreamMeta::timestamp_only(iso8601_from_millis(started_at_ms)),
        );
        let staged = self
            .run
            .pending_mut()
            .map(|run| std::mem::take(&mut run.staged))
            .unwrap_or_default();
        let echo_index = client_message_id.and_then(|id| {
            staged.iter().position(|event| {
                matches!(
                    event,
                    AgentStreamEvent::Timeline { item, .. }
                        if matches!(&**item, AgentTimelineItem::UserMessage { client_message_id: Some(c), .. } if c == id)
                )
            })
        });
        if let Some(id) = client_message_id {
            let provider_message_id = echo_index.and_then(|i| match &staged[i] {
                AgentStreamEvent::Timeline { item, .. } => match &**item {
                    AgentTimelineItem::UserMessage { message_id, .. } => message_id.clone(),
                    _ => None,
                },
                _ => None,
            });
            self.record_submitted_prompt(
                prompt,
                id,
                PromptOutcome::Accepted,
                PromptRecordOptions {
                    message_id: Some(id.to_owned()),
                    provider_message_id,
                    turn_id: Some(turn_id.to_owned()),
                },
            );
        }
        for (index, event) in staged.into_iter().enumerate() {
            let accepted_turn_start = matches!(&event, AgentStreamEvent::TurnStarted { turn_id: Some(t), .. } if t == turn_id);
            if accepted_turn_start || Some(index) == echo_index {
                continue;
            }
            self.handle_session_event(event);
        }
        self.emit_state(true);
        let (waiter_id, events) = self.waiters.add(turn_id);
        Accepted { waiter_id, events }
    }

    /// `startPendingForegroundTurn`, stale/closed-session branch: no `turn_failed`, back to
    /// `idle` when no foreground turn exists.
    pub(crate) fn start_failed_stale(&mut self, token: &str, message: &str) {
        if let Some(run) = self.run.pending_mut().filter(|run| run.token == token) {
            run.start = RunStart::Failed {
                error: message.to_owned(),
            };
        }
        self.view.pending_replacement = false;
        if self.view.active_foreground_turn_id.is_none() {
            self.view.lifecycle = Lifecycle::Idle;
        }
        self.run.settle_foreground_run(token);
    }

    /// `startPendingForegroundTurn`, any other failure: record the rejected prompt (when it has
    /// a `clientMessageId`), feed a synthetic id-less `turn_failed` through the pipeline, then
    /// finalize and settle.
    pub(crate) fn start_failed(
        &mut self,
        token: &str,
        error: &str,
        prompt: &PromptInput,
        client_message_id: Option<&str>,
    ) {
        self.view.pending_replacement = false;
        if let Some(run) = self.run.pending_mut().filter(|run| run.token == token) {
            run.start = RunStart::Failed {
                error: error.to_owned(),
            };
        }
        // A terminal rejection belongs after the submitted prompt even with no provider turn.
        if let Some(id) = client_message_id {
            self.record_submitted_prompt(
                prompt,
                id,
                PromptOutcome::Rejected,
                PromptRecordOptions {
                    message_id: Some(id.to_owned()),
                    ..PromptRecordOptions::default()
                },
            );
        }
        self.handle_stream_event(
            AgentStreamEvent::TurnFailed {
                provider: self.view.provider.clone(),
                error: error.to_owned(),
                code: None,
                diagnostic: None,
                turn_id: None,
            },
            false,
        );
        self.finalize_foreground_turn(None);
        self.run.settle_foreground_run(token);
    }

    /// `finalizeForegroundTurn`: remember the id, close the turn, pick the next lifecycle
    /// (`running` when a replacement is pending and no error, `error` on `lastError`, else
    /// `idle`), refresh persistence, touch and emit unless held for a replacement.
    pub(crate) fn finalize_foreground_turn(&mut self, turn_id: Option<&str>) {
        if let Some(turn) = turn_id.filter(|t| !t.is_empty()) {
            self.remember_finalized(turn);
        }
        self.view.active_foreground_turn_id = None;
        apply_active_turn_terminal(&mut self.view, turn_id, false);
        let terminal_error = self
            .view
            .last_error
            .clone()
            .filter(|error| !error.is_empty());
        let hold_for_replacement = self.view.pending_replacement && terminal_error.is_none();
        self.view.lifecycle = if hold_for_replacement {
            Lifecycle::Running
        } else if terminal_error.is_some() {
            Lifecycle::Error
        } else {
            Lifecycle::Idle
        };
        let described = sync_probe(self.session.describe_persistence())
            .and_then(std::result::Result::ok)
            .flatten();
        let handle = described.or_else(|| {
            self.view
                .runtime_info
                .as_ref()
                .and_then(|info| info.session_id.clone())
                .map(|session_id| PersistenceHandle {
                    provider: self.view.provider.clone(),
                    session_id,
                    native_handle: None,
                    metadata: None,
                })
        });
        if let Some(handle) = handle {
            self.view.persistence = attach_persistence_cwd(Some(handle), &self.view.cwd);
        }
        if !hold_for_replacement {
            self.touch_updated_at();
            self.emit_state(true);
        }
    }

    /// The `finally` of a stream: drop the waiter and settle the run this stream owns.
    pub(crate) fn end_stream(&mut self, waiter_id: Option<u64>, token: &str) {
        if let Some(id) = waiter_id {
            self.waiters.delete(id);
        }
        self.run.settle_foreground_run(token);
    }
}
