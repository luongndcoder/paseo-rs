//! Provider registry (`updateProviderRegistry`, `requireAvailableClient`, availability) and
//! the draft listings that borrow a client (`listDraftCommands`, `listDraftFeatures`).

use std::collections::HashMap;
use std::sync::Arc;

use indexmap::IndexMap;
use paseo_provider_api::{AgentClient, AgentSessionConfig};
use serde_json::Value;
use tracing::warn;

use super::config_prep::NormalizeOpts;
use super::AgentManager;
use crate::error::{CoreError, Result};

/// `AgentSessionConfig` rewrite a provider applies for a tool policy (`applyToolPolicy`).
pub type ApplyToolPolicy =
    Arc<dyn Fn(&AgentSessionConfig, Option<&Value>) -> AgentSessionConfig + Send + Sync>;

/// `ProviderEnabledFlag`.
#[derive(Clone)]
pub struct ProviderDefinition {
    pub enabled: bool,
    pub derived_from_provider_id: Option<String>,
    pub apply_tool_policy: Option<ApplyToolPolicy>,
}

impl ProviderDefinition {
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            derived_from_provider_id: None,
            apply_tool_policy: None,
        }
    }

    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::enabled()
        }
    }
}

/// Input of `updateProviderRegistry` and of [`crate::ManagerOptions`].
#[derive(Clone, Default)]
pub struct ProviderRegistryUpdate {
    pub definitions: IndexMap<String, ProviderDefinition>,
    pub clients: IndexMap<String, Arc<dyn AgentClient>>,
    /// Live agents of these providers are closed (best effort, in the background).
    pub retired_providers: Vec<String>,
}

#[derive(Default)]
pub(crate) struct ProviderState {
    pub clients: IndexMap<String, Arc<dyn AgentClient>>,
    pub enabled: HashMap<String, bool>,
    pub definitions: HashMap<String, ProviderDefinition>,
}

/// `ProviderAvailability`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAvailability {
    pub provider: String,
    pub available: bool,
    pub error: Option<String>,
}

fn format_provider_list(providers: &[String]) -> String {
    if providers.is_empty() {
        "none".to_owned()
    } else {
        providers.join(", ")
    }
}

impl AgentManager {
    fn providers(&self) -> std::sync::MutexGuard<'_, ProviderState> {
        self.inner
            .providers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// `registerClient`.
    pub fn register_client(&self, provider: &str, client: Arc<dyn AgentClient>) {
        self.providers().clients.insert(provider.to_owned(), client);
    }

    pub(crate) fn install_providers(&self, update: &ProviderRegistryUpdate) {
        let mut state = self.providers();
        state.enabled.clear();
        state.definitions.clear();
        for (provider, definition) in &update.definitions {
            state.enabled.insert(provider.clone(), definition.enabled);
            state
                .definitions
                .insert(provider.clone(), definition.clone());
        }
        state.clients.clear();
        for (provider, client) in &update.clients {
            state.clients.insert(provider.clone(), client.clone());
        }
    }

    /// `updateProviderRegistry`. Live agents of retired providers are closed in the background.
    pub fn update_provider_registry(&self, update: ProviderRegistryUpdate) {
        self.install_providers(&update);
        for provider in &update.retired_providers {
            for view in self.list_agents(true) {
                if view.provider != *provider {
                    continue;
                }
                let (manager, id, provider) = (self.clone(), view.id.clone(), provider.clone());
                tokio::spawn(async move {
                    if let Err(err) = manager.close_agent(&id).await {
                        warn!(%err, agent_id = %id, %provider, "Failed to close agent after provider retirement");
                    }
                });
            }
        }
    }

    /// `getRegisteredProviderIds`.
    pub fn registered_provider_ids(&self) -> Vec<String> {
        self.providers().clients.keys().cloned().collect()
    }

    pub(crate) fn client_for(&self, provider: &str) -> Option<Arc<dyn AgentClient>> {
        self.providers().clients.get(provider).cloned()
    }

    pub(crate) fn definition_for(&self, provider: &str) -> Option<ProviderDefinition> {
        self.providers().definitions.get(provider).cloned()
    }

    /// `getConfiguredProviderIds`: enabled-map keys, then client keys, deduplicated.
    fn configured_provider_ids(&self) -> Vec<String> {
        let state = self.providers();
        let mut ids: Vec<String> = Vec::new();
        for key in state.enabled.keys().chain(state.clients.keys()) {
            if !ids.contains(key) {
                ids.push(key.clone());
            }
        }
        ids
    }

    /// `requireClient`.
    pub(crate) fn require_client(&self, provider: &str) -> Result<Arc<dyn AgentClient>> {
        self.client_for(provider).ok_or_else(|| {
            CoreError::msg(format!("No client registered for provider '{provider}'"))
        })
    }

    /// `requireEnabledProvider`.
    pub(crate) fn require_enabled_provider(&self, provider: &str) -> Result<()> {
        if self.providers().enabled.get(provider) == Some(&false) {
            return Err(CoreError::msg(format!("Provider '{provider}' is disabled")));
        }
        Ok(())
    }

    /// `requireAvailableClient`.
    pub(crate) async fn require_available_client(
        &self,
        provider: &str,
    ) -> Result<Arc<dyn AgentClient>> {
        let Some(client) = self.client_for(provider) else {
            let configured = self.configured_provider_ids();
            return Err(CoreError::msg(format!(
                "Unknown provider '{provider}'. Configured providers: {}.",
                format_provider_list(&configured)
            )));
        };
        let mut unavailable_reason: Option<String> = None;
        match client.is_available(None).await {
            Ok(true) => return Ok(client),
            Ok(false) => {}
            Err(error) => unavailable_reason = Some(error.to_string()),
        }
        let available: Vec<String> = self
            .list_provider_availability()
            .await
            .into_iter()
            .filter(|entry| entry.available)
            .map(|entry| entry.provider)
            .collect();
        let reason = unavailable_reason
            .map(|r| format!(" Reason: {r}."))
            .unwrap_or_default();
        Err(CoreError::msg(format!(
            "Provider '{provider}' is not available.{reason} Available providers: {}. Use one of those providers, or install/configure '{provider}'.",
            format_provider_list(&available)
        )))
    }

    /// `listProviderAvailability`.
    pub async fn list_provider_availability(&self) -> Vec<ProviderAvailability> {
        let ids = self.registered_provider_ids();
        let checks = ids.iter().map(|id| self.get_provider_availability(id));
        futures_join_all(checks).await
    }

    /// `getProviderAvailability`.
    pub async fn get_provider_availability(&self, provider: &str) -> ProviderAvailability {
        let Some(client) = self.client_for(provider) else {
            return ProviderAvailability {
                provider: provider.to_owned(),
                available: false,
                error: Some(format!("No client registered for provider '{provider}'")),
            };
        };
        match client.is_available(None).await {
            Ok(available) => ProviderAvailability {
                provider: provider.to_owned(),
                available,
                error: None,
            },
            Err(error) => {
                warn!(%error, %provider, "Failed to check provider availability");
                ProviderAvailability {
                    provider: provider.to_owned(),
                    available: false,
                    error: Some(error.to_string()),
                }
            }
        }
    }

    /// `listDraftCommands`: no model means no guess.
    pub async fn list_draft_commands(&self, config: AgentSessionConfig) -> Result<Vec<Value>> {
        let normalized = self
            .normalize_config(
                config,
                NormalizeOpts {
                    resolve_default_model: false,
                    ..NormalizeOpts::default()
                },
            )
            .await?;
        let client = self.require_client(&normalized.provider)?;
        if normalized.model.is_none() {
            return Ok(Vec::new());
        }
        if !client.is_available(None).await? {
            return Err(CoreError::msg(format!(
                "Provider '{}' is not available. Please ensure the CLI is installed.",
                normalized.provider
            )));
        }
        Ok(client.list_commands(&normalized).await?)
    }

    /// `listDraftFeatures`.
    pub async fn list_draft_features(&self, config: AgentSessionConfig) -> Result<Vec<Value>> {
        let normalized = self
            .normalize_config(
                config,
                NormalizeOpts {
                    resolve_default_model: false,
                    ..NormalizeOpts::default()
                },
            )
            .await?;
        let client = self.require_client(&normalized.provider)?;
        if !client.is_available(None).await? {
            return Err(CoreError::msg(format!(
                "Provider '{}' is not available. Please ensure the CLI is installed.",
                normalized.provider
            )));
        }
        Ok(client.list_features(&normalized).await?)
    }
}

/// Run futures to completion concurrently, keeping their order (`Promise.all`).
pub(crate) async fn futures_join_all<F: std::future::Future>(
    futures: impl IntoIterator<Item = F>,
) -> Vec<F::Output> {
    let mut set = Vec::new();
    for future in futures {
        set.push(Box::pin(future));
    }
    let mut results: Vec<Option<F::Output>> = (0..set.len()).map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (index, future) in set.iter_mut().enumerate() {
            if results[index].is_some() {
                continue;
            }
            match future.as_mut().poll(cx) {
                std::task::Poll::Ready(value) => results[index] = Some(value),
                std::task::Poll::Pending => pending = true,
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    results.into_iter().flatten().collect()
}
