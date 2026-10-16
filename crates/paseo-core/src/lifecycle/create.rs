//! `create_agent` and `import_provider_session` (`agent-manager.ts:1282-1342`, `:1461-1546`).
//!
//! Neither runs on the lifecycle lane: both are tracked as registrations so shutdown can join
//! them, and re-check the shutdown flag after every await.

use std::sync::Arc;

use paseo_provider_api::{AgentSessionConfig, CreateSessionOptions, ProviderError};
use paseo_timeline::limits::limit_item_content;
use paseo_timeline::{project_agent_message, Row};
use serde_json::{json, Value};
use tracing::warn;

use crate::config::config_to_object;
use crate::error::{CoreError, Result};
use crate::ids::validate_agent_id;
use crate::lifecycle::close::describe_hook_view;
use crate::lifecycle::register::RegisterOptions;
use crate::manager::config_prep::{
    env_from_value, env_to_value, OpenReason, Opening, ResumePurpose,
};
use crate::manager::AgentManager;
use crate::model::{CreateAgentOptions, ImportInput, ManagedAgentView};
use crate::time::iso8601_from_millis;

/// Longest derived initial title, in UTF-16 units (`MAX_INITIAL_AGENT_TITLE_CHARS`).
const MAX_INITIAL_AGENT_TITLE_CHARS: usize = 60;

/// `deriveInitialAgentTitle`: the first non-empty line, whitespace collapsed, clamped.
pub fn derive_initial_agent_title(prompt: &str) -> Option<String> {
    let line = prompt.lines().map(str::trim).find(|l| !l.is_empty())?;
    let normalized = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    let mut units = 0usize;
    let mut clamped = String::new();
    for ch in normalized.chars() {
        units += ch.len_utf16();
        if units > MAX_INITIAL_AGENT_TITLE_CHARS {
            break;
        }
        clamped.push(ch);
    }
    let clamped = clamped.trim().to_owned();
    (!clamped.is_empty()).then_some(clamped)
}

/// `resolveCreateAgentTitles`: `(explicit, provisional)`.
pub fn resolve_create_agent_titles(
    config_title: Option<&str>,
    initial_prompt: Option<&str>,
) -> (Option<String>, Option<String>) {
    let explicit = config_title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    let prompt = initial_prompt.map(str::trim).filter(|p| !p.is_empty());
    let provisional = explicit
        .clone()
        .or_else(|| prompt.and_then(derive_initial_agent_title));
    (explicit, provisional)
}

/// `resolveImportedAgentTitle`: from the first non-blank user message.
fn resolve_imported_agent_title(config: &AgentSessionConfig, rows: &[Row]) -> Option<String> {
    let prompt = rows.iter().find_map(|row| match &row.item {
        paseo_agent_types::AgentTimelineItem::UserMessage { text, .. }
            if !text.trim().is_empty() =>
        {
            Some(text.trim().to_owned())
        }
        _ => None,
    })?;
    let (explicit, provisional) =
        resolve_create_agent_titles(config.title.as_deref(), Some(&prompt));
    explicit.or(provisional)
}

impl AgentManager {
    /// `createAgent`.
    pub async fn create_agent(
        &self,
        config: AgentSessionConfig,
        agent_id: Option<&str>,
        options: CreateAgentOptions,
    ) -> Result<Arc<ManagedAgentView>> {
        let _tracked = self.inner.registrations.track();
        self.create_agent_inner(config, agent_id, options).await
    }

    async fn create_agent_inner(
        &self,
        mut config: AgentSessionConfig,
        agent_id: Option<&str>,
        mut options: CreateAgentOptions,
    ) -> Result<Arc<ManagedAgentView>> {
        self.assert_accepting_registrations()?;
        let id = validate_agent_id(
            &agent_id
                .map(str::to_owned)
                .unwrap_or_else(|| (self.inner.options.id_factory)()),
            "createAgent",
        )?;
        if self.hooks().has_plugin_lifecycle() && config.internal != Some(true) {
            let request = json!({"config": &config, "env": env_to_value(&options.env)});
            let transformed = self.run_plugin_before_create(request).await?;
            let internal = config.internal;
            config =
                serde_json::from_value(transformed.get("config").cloned().unwrap_or(Value::Null))
                    .map_err(|e| {
                    CoreError::msg(format!("agent.create hook returned an invalid config: {e}"))
                })?;
            config.internal = internal;
            options.env = Some(env_from_value(transformed.get("env")));
        }
        self.delete_agent_state(&id);
        let prepared = self
            .prepare_session_config(config, &id, ResumePurpose::Interactive)
            .await?;
        self.require_enabled_provider(&prepared.stored.provider)?;
        let client = self
            .require_available_client(&prepared.stored.provider)
            .await?;
        self.set_tool_policy(&id, prepared.policy.clone());
        let context = self
            .build_launch_context(
                &id,
                &client,
                &prepared.stored.cwd,
                prepared.policy.as_ref(),
                options.env.clone(),
                Opening {
                    reason: OpenReason::Create,
                    purpose: ResumePurpose::Interactive,
                    workspace_id: options.workspace_id.clone(),
                },
            )
            .await?;
        let launch = Self::resolve_provider_launch_config(prepared.launch, &context);
        let create_options = options.persist_session.map(|persist| CreateSessionOptions {
            persist_session: Some(persist),
            ..CreateSessionOptions::default()
        });
        let session = client
            .create_session(&launch, Some(&context), create_options.as_ref())
            .await?;
        self.require_external_mcp_support(session.as_ref(), &prepared.stored)
            .await?;
        let agent = self
            .register_session(
                session,
                prepared.stored,
                &id,
                RegisterOptions {
                    labels: options.labels,
                    initial_title: options.initial_title,
                    workspace_id: options.workspace_id,
                    owner: options.owner,
                    history_primed: Some(true),
                    ..RegisterOptions::default()
                },
            )
            .await?;
        if !agent.internal {
            self.hooks().plugin_emit(
                "agent.created",
                json!({"agent": describe_hook_view(&agent)}),
            );
        }
        Ok(agent)
    }

    /// `requireExternalMcpSupport`: a session that cannot honor MCP servers is closed.
    pub(crate) async fn require_external_mcp_support(
        &self,
        session: &dyn paseo_provider_api::AgentSession,
        stored: &AgentSessionConfig,
    ) -> Result<()> {
        let has_servers = matches!(&stored.mcp_servers, Some(Value::Object(s)) if !s.is_empty());
        if !has_servers
            || session.capabilities().get("supportsMcpServers") == Some(&Value::Bool(true))
        {
            return Ok(());
        }
        close_unregistered_session(session).await;
        Err(CoreError::msg(format!(
            "Provider '{}' does not support MCP servers",
            stored.provider
        )))
    }

    /// `importProviderSession`.
    pub async fn import_provider_session(
        &self,
        input: ImportInput,
    ) -> Result<Arc<ManagedAgentView>> {
        let _tracked = self.inner.registrations.track();
        self.import_inner(input).await
    }

    async fn import_inner(&self, input: ImportInput) -> Result<Arc<ManagedAgentView>> {
        self.assert_accepting_registrations()?;
        let id = validate_agent_id(&(self.inner.options.id_factory)(), "importProviderSession")?;
        self.require_enabled_provider(&input.provider)?;
        let client = self.require_available_client(&input.provider).await?;
        let prepared = self
            .prepare_session_config(
                AgentSessionConfig::new(input.provider.clone(), input.cwd.clone()),
                &id,
                ResumePurpose::Interactive,
            )
            .await?;
        self.set_tool_policy(&id, prepared.policy.clone());
        let context = self
            .build_launch_context(
                &id,
                &client,
                &prepared.stored.cwd,
                prepared.policy.as_ref(),
                None,
                Opening {
                    reason: OpenReason::Import,
                    purpose: ResumePurpose::Interactive,
                    workspace_id: Some(input.workspace_id.clone()),
                },
            )
            .await?;
        let launch = Self::resolve_provider_launch_config(prepared.launch, &context);
        let imported = client
            .import_session(
                &json!({"providerHandleId": input.provider_handle_id, "cwd": input.cwd}),
                &json!({
                    "config": config_to_object(&launch),
                    "storedConfig": config_to_object(&prepared.stored),
                    "launchContext": context,
                }),
            )
            .await
            .map_err(|error| match error {
                ProviderError::Unsupported(_) => CoreError::msg(format!(
                    "Provider '{}' does not support importing sessions",
                    input.provider
                )),
                other => other.into(),
            })?;
        let normalized = self
            .normalize_config(
                crate::config::strip_internal_paseo_mcp_server(imported.config),
                Default::default(),
            )
            .await;
        let config = match normalized {
            Ok(config) => config,
            Err(error) => {
                close_unregistered_session(imported.session.as_ref()).await;
                return Err(error);
            }
        };
        let now = self.clock().now_ms();
        let mut rows: Vec<Row> = Vec::new();
        for entry in &imported.timeline {
            let Some(item) = project_agent_message(&entry.item) else {
                continue;
            };
            rows.push(Row::new(
                rows.len() as i64 + 1,
                entry
                    .timestamp
                    .clone()
                    .unwrap_or_else(|| iso8601_from_millis(now)),
                limit_item_content(item),
            ));
        }
        let initial_title = resolve_imported_agent_title(&config, &rows);
        let next_seq = rows.len() as i64 + 1;
        let agent = self
            .register_session(
                imported.session,
                config,
                &id,
                RegisterOptions {
                    labels: input.labels,
                    workspace_id: Some(input.workspace_id),
                    timeline_rows: rows,
                    timeline_next_seq: Some(next_seq),
                    persistence: Some(imported.persistence),
                    history_primed: Some(true),
                    initial_title,
                    publish_when_ready: true,
                    ..RegisterOptions::default()
                },
            )
            .await?;
        for event in imported.provider_subagent_events {
            if let paseo_agent_types::AgentStreamEvent::ProviderSubagent { provider, event } = event
            {
                self.apply_provider_subagent_event(&agent.id, &provider, &event);
            }
        }
        Ok(agent)
    }

    /// Apply a provider-child event to the store and dispatch the update to subscribers.
    pub(crate) fn apply_provider_subagent_event(
        &self,
        parent: &str,
        provider: &str,
        input: &paseo_agent_types::ProviderSubagentInputEvent,
    ) {
        let now = iso8601_from_millis(self.clock().now_ms());
        let update = self
            .inner
            .ctx
            .subagents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .apply(parent, provider, input, &now);
        self.dispatch_subagent_update(update);
    }

    /// `dispatch({type: "provider_subagent"})` from manager level. TS decides whether the
    /// parent is internal by looking it up in `agents` at dispatch time, so an event for a
    /// parent that already left the registry is treated as public
    /// (PARITY-QUIRK(subagent-update-after-close-is-public)).
    pub(crate) fn dispatch_subagent_update(
        &self,
        update: crate::provider_subagents::SubagentStoreEvent,
    ) {
        let parent_internal = self
            .inner
            .registry
            .get(update.parent_agent_id())
            .is_some_and(|h| h.view().internal);
        self.bus()
            .emit(crate::events::ManagerEvent::ProviderSubagent {
                event: update,
                parent_internal,
            });
    }

    /// `discardRetainedAgentState`: timeline, tool policy, provider children (with removes).
    pub(crate) fn discard_retained_agent_state(&self, agent_id: &str) {
        drop(self.inner.registry.take_retained_timeline(agent_id));
        self.inner
            .tool_policies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(agent_id);
        let removed = self
            .inner
            .ctx
            .subagents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .delete_parent(agent_id);
        for update in removed {
            self.dispatch_subagent_update(update);
        }
    }

    /// `deleteAgentState`.
    pub fn delete_agent_state(&self, agent_id: &str) {
        self.discard_retained_agent_state(agent_id);
    }
}

/// `closeUnregisteredSession`: errors are logged, never raised.
pub(crate) async fn close_unregistered_session(session: &dyn paseo_provider_api::AgentSession) {
    if let Err(err) = session.close().await {
        warn!(%err, "Failed to close unregistered agent session");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_titles_use_the_first_line_collapsed_and_clamped() {
        assert_eq!(
            derive_initial_agent_title("\n  hello   wide\tworld \nsecond").as_deref(),
            Some("hello wide world")
        );
        assert_eq!(derive_initial_agent_title("   \n "), None);
        let long = "x".repeat(80);
        assert_eq!(derive_initial_agent_title(&long).unwrap().len(), 60);
        // Clamped in UTF-16 units: 31 surrogate pairs would be 62 units.
        let emoji = "\u{1F600}".repeat(40);
        assert_eq!(
            derive_initial_agent_title(&emoji).unwrap().chars().count(),
            30
        );
    }

    #[test]
    fn explicit_title_wins_over_the_prompt() {
        assert_eq!(
            resolve_create_agent_titles(Some(" Mine "), Some("prompt")),
            (Some("Mine".into()), Some("Mine".into()))
        );
        assert_eq!(
            resolve_create_agent_titles(None, Some(" do it \n")),
            (None, Some("do it".into()))
        );
        assert_eq!(resolve_create_agent_titles(Some("  "), None), (None, None));
    }
}
