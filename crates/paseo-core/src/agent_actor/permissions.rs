//! Pending permissions: request/resolve handlers and the respond protocol.
//!
//! One in-flight response per request id. A provider `permission_resolved` that arrives while
//! its response is in flight is buffered and dispatched only after the refreshed `agent_state`;
//! the pending entry is deleted only on success, and the buffered entry is always cleared.

use paseo_agent_types::{
    AgentMode, AgentRuntimeInfo, AgentStreamEvent, AttentionReason, PermissionRequest,
    PermissionResponse,
};

use super::emit::StreamMeta;
use super::events::StreamFlags;
use super::AgentActor;
use crate::error::{CoreError, Result};

/// What the session reported during `refreshSessionState`; each part may have failed.
pub(crate) struct SessionRefresh {
    pub modes: std::result::Result<Vec<AgentMode>, ()>,
    pub current_mode: std::result::Result<Option<String>, ()>,
    pub pending: std::result::Result<Vec<PermissionRequest>, ()>,
    pub runtime_info: Option<AgentRuntimeInfo>,
}

impl AgentActor {
    pub(crate) fn on_permission_requested(&mut self, request: PermissionRequest) {
        let had_pending = !self.view.pending_permissions.is_empty();
        self.view
            .pending_permissions
            .insert(request.id.clone(), request);
        self.refresh_session_persistence();
        if !had_pending && !self.view.internal {
            self.broadcast_attention(AttentionReason::Permission);
        }
        self.emit_state(true);
    }

    pub(crate) fn on_permission_resolved(
        &mut self,
        event: &AgentStreamEvent,
        request_id: &str,
        from_history: bool,
        flags: &mut StreamFlags,
    ) {
        self.view.pending_permissions.shift_remove(request_id);
        self.refresh_session_persistence();
        if !from_history && self.in_flight_permissions.contains(request_id) {
            self.buffered_resolutions
                .insert(request_id.to_owned(), event.clone());
            flags.dispatch = false;
            return;
        }
        self.emit_state(true);
    }

    /// `resolvePendingPermissionsForAgent`: deny everything pending, dispatching a
    /// `permission_resolved` for each unless replaying history.
    pub(crate) fn resolve_pending_permissions(
        &mut self,
        provider: &str,
        from_history: bool,
        message: &str,
    ) {
        let pending: Vec<String> = self.view.pending_permissions.keys().cloned().collect();
        self.view.pending_permissions.clear();
        if from_history {
            return;
        }
        for request_id in pending {
            self.dispatch_stream(
                AgentStreamEvent::PermissionResolved {
                    provider: provider.to_owned(),
                    request_id,
                    resolution: PermissionResponse::Deny {
                        selected_action_id: None,
                        message: Some(message.to_owned()),
                        interrupt: None,
                    },
                    turn_id: None,
                },
                StreamMeta::default(),
            );
        }
    }

    /// Start of `respondToPermission`: claim the request id.
    pub(crate) fn begin_permission_response(&mut self, request_id: &str) -> Result<()> {
        if !self.in_flight_permissions.insert(request_id.to_owned()) {
            return Err(CoreError::msg(
                "A response to this permission request is already being submitted",
            ));
        }
        Ok(())
    }

    /// The `finally` of `respondToPermission`.
    pub(crate) fn end_permission_response(&mut self, request_id: &str) {
        self.in_flight_permissions.remove(request_id);
        self.buffered_resolutions.remove(request_id);
    }

    /// The provider accepted the response: drop the pending entry.
    pub(crate) fn permission_response_accepted(&mut self, request_id: &str) {
        self.view.pending_permissions.shift_remove(request_id);
    }

    /// Dispatch a resolution buffered during the in-flight response.
    pub(crate) fn flush_buffered_resolution(&mut self, request_id: &str) {
        if let Some(event) = self.buffered_resolutions.remove(request_id) {
            let timestamp = self.now_iso();
            self.dispatch_stream(event, StreamMeta::timestamp_only(timestamp));
        }
    }

    /// Apply a `refreshSessionState` result as one atomic block.
    pub(crate) fn apply_session_refresh(&mut self, refresh: SessionRefresh, emit: bool) {
        self.view.available_modes = refresh.modes.unwrap_or_default();
        match refresh.current_mode {
            Ok(mode) => self.apply_observed_mode(mode),
            Err(()) => self.view.current_mode_id = None,
        }
        match refresh.pending {
            Ok(list) => {
                self.view.pending_permissions =
                    list.into_iter().map(|r| (r.id.clone(), r)).collect();
            }
            Err(()) => self.view.pending_permissions.clear(),
        }
        self.view.features = self.session.features().to_vec();
        if let Some(info) = refresh.runtime_info {
            self.apply_runtime_info(info, emit);
        }
    }
}
