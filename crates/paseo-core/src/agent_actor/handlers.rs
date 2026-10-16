//! Per-type stream event handlers (`dispatchStreamEventByType` and the `onStream*` family).

use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem, AgentUsage, PersistenceHandle};
use paseo_timeline::project_agent_message;

use super::events::StreamFlags;
use super::timeline_record::{format_turn_failed_message, RecordOptions};
use super::turn_identity::TerminalDisposition;
use super::{attach_persistence_cwd, AgentActor};
use crate::bridge::sync_probe;
use crate::model::Lifecycle;

/// `{...previous, ...next}` over the optional usage fields.
fn merge_usage(previous: Option<&AgentUsage>, next: &AgentUsage) -> AgentUsage {
    let mut merged = previous.cloned().unwrap_or_default();
    macro_rules! take {
        ($($field:ident),*) => {$(
            if next.$field.is_some() {
                merged.$field = next.$field.clone();
            }
        )*};
    }
    take!(
        input_tokens,
        cached_input_tokens,
        output_tokens,
        total_cost_usd,
        context_window_max_tokens,
        context_window_used_tokens
    );
    merged
}

impl AgentActor {
    /// `applyObservedMode`: a provider-side mode switch must reach `config.modeId` too, or a
    /// reload resumes with the stale mode.
    pub(crate) fn apply_observed_mode(&mut self, mode_id: Option<String>) {
        if let Some(mode) = &mode_id {
            self.view.config.mode_id = Some(mode.clone());
        }
        self.view.current_mode_id = mode_id;
    }

    /// `refreshSessionPersistence`.
    pub(crate) fn refresh_session_persistence(&mut self) {
        let Some(Ok(Some(handle))) = sync_probe(self.session.describe_persistence()) else {
            return;
        };
        self.view.persistence = attach_persistence_cwd(Some(handle), &self.view.cwd);
    }

    fn persistence_from_runtime_session(&mut self, session_id: Option<String>) {
        if self.view.persistence.is_some() {
            return;
        }
        if let Some(session_id) = session_id {
            self.view.persistence = attach_persistence_cwd(
                Some(PersistenceHandle {
                    provider: self.view.provider.clone(),
                    session_id,
                    native_handle: None,
                    metadata: None,
                }),
                &self.view.cwd,
            );
        }
    }

    pub(crate) fn handle_by_type(
        &mut self,
        event: &mut AgentStreamEvent,
        from_history: bool,
        is_foreground: bool,
        event_turn: Option<&str>,
        disposition: TerminalDisposition,
        flags: &mut StreamFlags,
    ) {
        use AgentStreamEvent as E;
        match event {
            E::ThreadStarted { .. } => self.on_thread_started(),
            E::UsageUpdated { usage, .. } => {
                self.view.last_usage = Some(usage.clone());
                self.emit_state(true);
            }
            E::ModeChanged {
                current_mode_id,
                available_modes,
                ..
            } => {
                self.apply_observed_mode(current_mode_id.clone());
                self.view.available_modes = available_modes.clone();
                if let Some(info) = &mut self.view.runtime_info {
                    info.mode_id = Some(current_mode_id.clone());
                }
                flags.dispatch = false;
                self.emit_state(true);
            }
            E::ModelChanged { runtime_info, .. } => {
                self.view.runtime_info = Some(runtime_info.clone());
                self.persistence_from_runtime_session(runtime_info.session_id.clone());
                let mode = runtime_info
                    .mode_id
                    .clone()
                    .flatten()
                    .or_else(|| self.view.current_mode_id.clone());
                self.apply_observed_mode(mode);
                flags.dispatch = false;
                self.emit_state(true);
            }
            E::ThinkingOptionChanged {
                thinking_option_id, ..
            } => {
                self.view.config.thinking_option_id = thinking_option_id.clone();
                if let Some(info) = &mut self.view.runtime_info {
                    info.thinking_option_id = Some(thinking_option_id.clone());
                }
                flags.dispatch = false;
                self.emit_state(true);
            }
            E::Timeline { .. } => self.on_timeline_event(event, from_history, flags),
            E::TurnCompleted { usage, .. } => {
                let usage = usage.clone();
                self.on_turn_completed(usage, is_foreground, disposition);
            }
            E::TurnFailed {
                provider,
                error,
                code,
                diagnostic,
                ..
            } => {
                let message =
                    format_turn_failed_message(error, code.as_deref(), diagnostic.as_deref());
                let (provider, error) = (provider.clone(), error.clone());
                self.on_turn_failed(
                    &provider,
                    &error,
                    &message,
                    is_foreground,
                    disposition,
                    from_history,
                );
            }
            E::TurnCanceled { provider, .. } => {
                let provider = provider.clone();
                self.on_turn_canceled(&provider, is_foreground, disposition, from_history);
            }
            E::TurnStarted { .. } => self.on_turn_started(event_turn, is_foreground, flags),
            E::PermissionRequested { request, .. } => {
                let request = (**request).clone();
                self.on_permission_requested(request);
            }
            E::PermissionResolved { request_id, .. } => {
                let request_id = request_id.clone();
                self.on_permission_resolved(event, &request_id, from_history, flags);
            }
            E::AttentionRequired { .. } | E::ProviderSubagent { .. } => {}
        }
    }

    fn on_thread_started(&mut self) {
        let previous = self.view.persistence.as_ref().map(|p| p.session_id.clone());
        self.refresh_session_persistence();
        if self.view.persistence.as_ref().map(|p| &p.session_id) != previous.as_ref() {
            self.emit_state(true);
        }
        self.spawn_refresh_runtime_info();
    }

    fn on_timeline_event(
        &mut self,
        event: &mut AgentStreamEvent,
        from_history: bool,
        flags: &mut StreamFlags,
    ) {
        let AgentStreamEvent::Timeline {
            item,
            provider,
            turn_id,
            timestamp,
        } = event
        else {
            return;
        };
        let Some(projected) = project_agent_message(item) else {
            flags.dispatch = false;
            flags.notify_waiters = false;
            return;
        };
        **item = projected;
        if matches!(
            &**item,
            AgentTimelineItem::UserMessage { client_message_id: Some(id), .. } if !id.is_empty()
        ) && self
            .reconcile_submitted_prompt_echo(item, turn_id.as_deref())
            .is_some()
        {
            flags.dispatch = false;
            flags.notify_waiters = false;
            return;
        }
        if from_history {
            self.record_timeline(
                (**item).clone(),
                RecordOptions {
                    timestamp: timestamp.clone(),
                    ..RecordOptions::default()
                },
            );
            flags.dispatch = false;
            flags.notify_waiters = false;
            return;
        }
        let (item, provider, turn_id) = ((**item).clone(), provider.clone(), turn_id.clone());
        let is_user = matches!(item, AgentTimelineItem::UserMessage { .. });
        self.record_and_dispatch_timeline_item(item, &provider, turn_id, None);
        if is_user {
            self.view.last_user_message_at_ms = Some(self.now_ms());
            self.emit_state(true);
        }
        flags.dispatch = false;
        flags.notify_waiters = true;
    }

    fn on_turn_completed(
        &mut self,
        usage: Option<AgentUsage>,
        is_foreground: bool,
        disposition: TerminalDisposition,
    ) {
        if disposition == TerminalDisposition::Stale {
            return;
        }
        // A completion without usage keeps what streaming accumulated.
        if let Some(usage) = usage {
            self.view.last_usage = Some(merge_usage(self.view.last_usage.as_ref(), &usage));
        }
        self.view.last_error = None;
        if !is_foreground
            && self.view.active_foreground_turn_id.is_none()
            && self.view.lifecycle != Lifecycle::Idle
            && !self.view.pending_replacement
        {
            self.view.lifecycle = Lifecycle::Idle;
            self.emit_state(true);
        }
        self.spawn_refresh_runtime_info();
    }

    fn on_turn_failed(
        &mut self,
        provider: &str,
        error: &str,
        message: &str,
        is_foreground: bool,
        disposition: TerminalDisposition,
        from_history: bool,
    ) {
        if disposition == TerminalDisposition::Stale {
            return;
        }
        let standalone = !is_foreground && self.view.active_foreground_turn_id.is_none();
        if standalone {
            self.view.lifecycle = Lifecycle::Error;
        }
        self.view.last_error = Some(error.to_owned());
        if !from_history {
            self.append_system_error_timeline_message(provider, message);
        }
        self.resolve_pending_permissions(provider, from_history, "Turn failed");
        if standalone {
            self.emit_state(true);
        }
    }

    fn on_turn_canceled(
        &mut self,
        provider: &str,
        is_foreground: bool,
        disposition: TerminalDisposition,
        from_history: bool,
    ) {
        if disposition == TerminalDisposition::Stale {
            return;
        }
        let standalone = !is_foreground && self.view.active_foreground_turn_id.is_none();
        if standalone && !self.view.pending_replacement {
            self.view.lifecycle = Lifecycle::Idle;
        }
        self.view.last_error = None;
        self.resolve_pending_permissions(provider, from_history, "Interrupted");
        if standalone {
            self.emit_state(true);
        }
    }

    fn on_turn_started(
        &mut self,
        event_turn: Option<&str>,
        is_foreground: bool,
        flags: &mut StreamFlags,
    ) {
        // The provider's own start for the foreground turn is suppressed: the manager already
        // dispatched the accepted-turn boundary.
        if is_foreground || self.view.active_foreground_turn_id.is_some() {
            flags.dispatch = false;
            flags.notify_waiters = false;
            return;
        }
        self.run.track_autonomous_run(event_turn.map(str::to_owned));
        if let Some(turn) = event_turn {
            self.open_active_turn(turn);
        }
        self.view.lifecycle = Lifecycle::Running;
        self.emit_state(true);
    }
}
