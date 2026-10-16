//! `resume_agent_from_persistence` (`agent-manager.ts:1353-1459`): reconstruct an agent from a
//! provider persistence handle, on the lifecycle lane. Callers hydrate the timeline afterwards
//! (see `ensure_agent_loaded`).

use std::sync::Arc;

use paseo_agent_types::PersistenceHandle;
use paseo_provider_api::{CreateSessionOptions, JsonObject};
use serde_json::Value;

use crate::config::{config_to_object, merge_config};
use crate::error::{CoreError, Result};
use crate::ids::validate_agent_id;
use crate::lifecycle::register::RegisterOptions;
use crate::manager::config_prep::{OpenReason, Opening, ResumePurpose};
use crate::manager::AgentManager;
use crate::model::{ManagedAgentView, ResumeOptions};

impl AgentManager {
    /// `resumeAgentFromPersistence`. `overrides` is a partial config where `null` stands for a
    /// JS `undefined` that shadows the same key of the handle metadata.
    pub async fn resume_agent_from_persistence(
        &self,
        handle: PersistenceHandle,
        overrides: Option<JsonObject>,
        agent_id: Option<&str>,
        options: ResumeOptions,
        purpose: Option<ResumePurpose>,
    ) -> Result<Arc<ManagedAgentView>> {
        let id = validate_agent_id(
            &agent_id
                .map(str::to_owned)
                .unwrap_or_else(|| (self.inner.options.id_factory)()),
            "resumeAgentFromPersistence",
        )?;
        let _tracked = self.inner.registrations.track();
        self.inner
            .lifecycle_lanes
            .run(&id, || {
                self.resume_inner(handle, overrides, id.clone(), options, purpose)
            })
            .await
    }

    async fn resume_inner(
        &self,
        handle: PersistenceHandle,
        overrides: Option<JsonObject>,
        id: String,
        options: ResumeOptions,
        requested_purpose: Option<ResumePurpose>,
    ) -> Result<Arc<ManagedAgentView>> {
        self.assert_accepting_registrations()?;
        let metadata = handle.metadata.clone().unwrap_or_default();
        let merged = merge_config(&metadata, overrides.as_ref(), &handle.provider);
        // Residency is decided from durable state inside the lane: a loader may have read the
        // record before a queued archive or restore completed. Settled before the config is
        // prepared, because a history load reads an archived agent whose cwd may be gone.
        let record = match self.store() {
            Some(store) => store
                .get(&id)
                .await
                .map_err(|e| CoreError::msg(e.to_string()))?,
            None => None,
        };
        let purpose = match &record {
            Some(record) if record.is_archived() => ResumePurpose::History,
            Some(_) => ResumePurpose::Interactive,
            None => requested_purpose.unwrap_or_default(),
        };
        let prepared = self.prepare_session_config(merged, &id, purpose).await?;
        let client = self.require_client(&handle.provider)?;
        if !client.is_available(None).await? {
            return Err(CoreError::msg(format!(
                "Provider '{}' is not available. Please ensure the CLI is installed.",
                handle.provider
            )));
        }
        self.set_tool_policy(&id, prepared.policy.clone());
        let context = self
            .build_launch_context(
                &id,
                &client,
                &prepared.stored.cwd,
                prepared.policy.as_ref(),
                None,
                Opening {
                    reason: OpenReason::Resume,
                    purpose,
                    workspace_id: options.workspace_id.clone(),
                },
            )
            .await?;
        let launch = Self::resolve_provider_launch_config(prepared.launch, &context);
        let launch_overrides = config_to_object(&launch);
        let session_options = CreateSessionOptions {
            purpose: Some(purpose.as_str().to_owned()),
            ..CreateSessionOptions::default()
        };
        let session = client
            .resume_session(
                &handle,
                Some(&launch_overrides),
                Some(&context),
                Some(&session_options),
            )
            .await?;
        self.require_external_mcp_support(session.as_ref(), &prepared.stored)
            .await?;
        self.register_session(
            session,
            prepared.stored,
            &id,
            RegisterOptions {
                created_at_ms: options.created_at_ms,
                updated_at_ms: options.updated_at_ms,
                last_user_message_at_ms: options.last_user_message_at_ms,
                labels: options.labels,
                workspace_id: options.workspace_id,
                owner: options.owner,
                attention: options.attention.unwrap_or_default(),
                persistence: Some(handle),
                restoring: true,
                ..RegisterOptions::default()
            },
        )
        .await
    }
}

/// `Value::Null` is the "explicitly undefined" marker of resume overrides.
pub fn overrides_from_json(value: Value) -> Option<JsonObject> {
    match value {
        Value::Object(map) => Some(map),
        _ => None,
    }
}
