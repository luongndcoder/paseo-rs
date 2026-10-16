//! `respond_to_permission` and the session state refreshes that follow it.

use paseo_agent_types::{PermissionRequest, PermissionResponse};
use paseo_provider_api::PermissionResult;

use super::AgentManager;
use crate::agent_actor::permissions::SessionRefresh;
use crate::agent_actor::ActorHandle;
use crate::error::Result;
use crate::hooks::PersistOptions;

impl AgentManager {
    /// `respondToPermission`.
    pub async fn respond_to_permission(
        &self,
        agent_id: &str,
        request_id: &str,
        response: PermissionResponse,
    ) -> Result<Option<PermissionResult>> {
        let handle = self.require_agent(agent_id)?;
        let id = request_id.to_owned();
        handle
            .ask(move |a| a.begin_permission_response(&id))
            .await??;
        let outcome = self
            .respond_to_permission_claimed(&handle, request_id, response)
            .await;
        let id = request_id.to_owned();
        let _ = handle.ask(move |a| a.end_permission_response(&id)).await;
        outcome
    }

    async fn respond_to_permission_claimed(
        &self,
        handle: &ActorHandle,
        request_id: &str,
        response: PermissionResponse,
    ) -> Result<Option<PermissionResult>> {
        let result = handle
            .session
            .respond_to_permission(request_id, response)
            .await?;
        let id = request_id.to_owned();
        handle
            .ask(move |a| a.permission_response_accepted(&id))
            .await?;
        // State sync after an approval is best effort.
        self.refresh_session_state(handle, true).await;
        handle
            .ask(|a| {
                a.touch_updated_at();
            })
            .await?;
        self.persist_snapshot(handle, PersistOptions::default())
            .await?;
        let id = request_id.to_owned();
        handle
            .ask(move |a| {
                a.emit_state(true);
                a.flush_buffered_resolution(&id);
            })
            .await?;
        Ok(result)
    }

    /// `refreshRuntimeInfo`: a failed refresh keeps the existing info.
    pub(crate) async fn refresh_runtime_info(&self, handle: &ActorHandle, emit: bool) {
        if let Ok(info) = handle.session.get_runtime_info().await {
            let _ = handle.ask(move |a| a.apply_runtime_info(info, emit)).await;
        }
    }

    /// `refreshSessionState`: provider reads outside the actor, one atomic apply.
    pub(crate) async fn refresh_session_state(&self, handle: &ActorHandle, emit: bool) {
        let session = &handle.session;
        let refresh = SessionRefresh {
            modes: session.available_modes().await.map_err(|_| ()),
            current_mode: session.current_mode().await.map_err(|_| ()),
            pending: session.pending_permissions().await.map_err(|_| ()),
            runtime_info: session.get_runtime_info().await.ok(),
        };
        let _ = handle
            .ask(move |a| a.apply_session_refresh(refresh, emit))
            .await;
    }

    /// `pending_permissions` as an owned list.
    pub async fn pending_permissions(&self, agent_id: &str) -> Result<Vec<PermissionRequest>> {
        let handle = self.require_agent(agent_id)?;
        handle
            .ask(|a| a.view.pending_permissions.values().cloned().collect())
            .await
    }
}
