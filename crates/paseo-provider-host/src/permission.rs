use async_trait::async_trait;

use crate::wire::{PermissionRequest, PermissionResponse};

/// Where a permission request came from.
#[derive(Debug, Clone)]
pub struct PermissionContext {
    /// Host session handle (`s1`, `s2`, ...).
    pub handle: String,
    pub turn_id: Option<String>,
}

/// Decides permission requests raised by provider sessions.
///
/// The client calls this when a `permission_requested` event arrives and sends the returned
/// response back with `session.respondToPermission`. The event is still delivered to the
/// session's event stream. Without a handler, callers respond manually through
/// [`crate::Session::respond_to_permission`].
#[async_trait]
pub trait PermissionHandler: Send + Sync + 'static {
    async fn decide(
        &self,
        ctx: PermissionContext,
        request: PermissionRequest,
    ) -> PermissionResponse;
}

/// Allows every request, selecting the first allow action when the request lists actions.
pub struct AllowAll;

#[async_trait]
impl PermissionHandler for AllowAll {
    async fn decide(
        &self,
        _ctx: PermissionContext,
        request: PermissionRequest,
    ) -> PermissionResponse {
        let selected = request.actions.as_ref().and_then(|actions| {
            actions
                .iter()
                .find(|a| a.behavior == crate::wire::PermissionBehavior::Allow)
                .map(|a| a.id.clone())
        });
        PermissionResponse::Allow {
            selected_action_id: selected,
            updated_input: None,
            updated_permissions: None,
        }
    }
}

/// Denies every request.
pub struct DenyAll;

#[async_trait]
impl PermissionHandler for DenyAll {
    async fn decide(
        &self,
        _ctx: PermissionContext,
        _request: PermissionRequest,
    ) -> PermissionResponse {
        PermissionResponse::deny("denied by policy")
    }
}
