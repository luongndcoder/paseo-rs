//! Live config mutators (`agent-manager.ts:2035-2116`): each one calls the provider, drains the
//! events that call produced (so a later explicit mutation wins over an earlier mutation's
//! events), then applies the result in one actor step and emits state.

use serde_json::Value;

use crate::error::{CoreError, Result};
use crate::manager::AgentManager;
use paseo_provider_api::ProviderError;

fn normalize_optional(value: Option<&str>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty()).map(str::to_owned)
}

impl AgentManager {
    /// `setAgentMode`: returns the provider notice, if any.
    pub async fn set_agent_mode(&self, agent_id: &str, mode_id: &str) -> Result<Option<Value>> {
        let handle = self.require_agent(agent_id)?;
        let notice = handle.session.set_mode(mode_id).await?;
        handle.ask(|a| a.drain_events()).await?;
        let current = handle
            .session
            .current_mode()
            .await?
            .unwrap_or_else(|| mode_id.to_owned());
        handle
            .ask(move |a| {
                a.view.config.mode_id = Some(current.clone());
                a.view.current_mode_id = Some(current.clone());
                if let Some(info) = &mut a.view.runtime_info {
                    info.mode_id = Some(Some(current));
                }
                a.touch_updated_at();
                a.emit_state(true);
            })
            .await?;
        Ok(notice)
    }

    /// `setAgentModel`: a blank model clears it.
    pub async fn set_agent_model(&self, agent_id: &str, model_id: Option<&str>) -> Result<()> {
        let handle = self.require_agent(agent_id)?;
        let normalized = normalize_optional(model_id);
        handle.session.set_model(normalized.as_deref()).await?;
        handle.ask(|a| a.drain_events()).await?;
        handle
            .ask(move |a| {
                a.view.config.model = normalized.clone();
                if let Some(info) = &mut a.view.runtime_info {
                    info.model = Some(normalized);
                }
                a.refresh_session_persistence();
                a.touch_updated_at();
                a.emit_state(true);
            })
            .await
    }

    /// `setAgentThinkingOption`: the session's effective (clamped) level wins when it reports
    /// one. A failed state read leaves the previous level in place.
    pub async fn set_agent_thinking_option(
        &self,
        agent_id: &str,
        thinking_option_id: Option<&str>,
    ) -> Result<Option<Value>> {
        let handle = self.require_agent(agent_id)?;
        let normalized = normalize_optional(thinking_option_id);
        let notice = handle
            .session
            .set_thinking_option(normalized.as_deref())
            .await?;
        handle.ask(|a| a.drain_events()).await?;
        let runtime = handle.session.get_runtime_info().await?;
        let effective = match runtime.thinking_option_id {
            Some(reported) => reported,
            None => normalized,
        };
        handle
            .ask(move |a| {
                a.view.config.thinking_option_id = effective.clone();
                if let Some(info) = &mut a.view.runtime_info {
                    info.thinking_option_id = Some(effective);
                }
                a.touch_updated_at();
                a.emit_state(true);
            })
            .await?;
        Ok(notice)
    }

    /// `setAgentFeature`.
    pub async fn set_agent_feature(
        &self,
        agent_id: &str,
        feature_id: &str,
        value: Value,
    ) -> Result<()> {
        let handle = self.require_agent(agent_id)?;
        handle
            .session
            .set_feature(feature_id, value.clone())
            .await
            .map_err(|error| match error {
                ProviderError::Unsupported(_) => {
                    CoreError::msg("Agent session does not support setting features")
                }
                other => other.into(),
            })?;
        handle.ask(|a| a.drain_events()).await?;
        let feature_id = feature_id.to_owned();
        handle
            .ask(move |a| {
                let values = a
                    .view
                    .config
                    .feature_values
                    .get_or_insert_with(Default::default);
                values.insert(feature_id, value);
                a.touch_updated_at();
                a.emit_state(true);
            })
            .await
    }
}
