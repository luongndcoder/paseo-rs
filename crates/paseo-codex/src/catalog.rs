//! Model / default discovery shared by sessions and the catalog call.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::agent::{ModelDefinition, SelectOption, Usage};
use crate::error::Result;
use crate::options::{normalize_model_label, normalize_thinking_option, PROVIDER};
use crate::protocol::notifications::ThreadTokenUsage;
use crate::protocol::params::{ModelInfo, ModelListResponse};
use crate::rpc::RpcClient;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfiguredDefaults {
    pub model: Option<String>,
    pub thinking_option_id: Option<String>,
}

fn normalize_model_id(id: Option<&str>) -> Option<String> {
    let id = id?.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// `config/read` -> the model and reasoning effort configured in `config.toml`. Failures read as
/// "nothing configured".
pub async fn read_configured_defaults(rpc: &RpcClient) -> ConfiguredDefaults {
    // `getUserSavedConfig` (removed from current app-servers) is intentionally not tried.
    match rpc.request("config/read", json!({}), None).await {
        Ok(v) => {
            let config = v.get("config");
            ConfiguredDefaults {
                model: normalize_model_id(
                    config.and_then(|c| c.get("model")).and_then(Value::as_str),
                ),
                thinking_option_id: normalize_thinking_option(
                    config
                        .and_then(|c| c.get("model_reasoning_effort"))
                        .and_then(Value::as_str),
                ),
            }
        }
        Err(_) => ConfiguredDefaults::default(),
    }
}

pub async fn list_models(rpc: &RpcClient) -> Result<Vec<ModelInfo>> {
    let v = rpc.request("model/list", json!({}), None).await?;
    Ok(serde_json::from_value::<ModelListResponse>(v)
        .map(|r| r.data)
        .unwrap_or_default())
}

pub fn build_model_definition(
    model: &ModelInfo,
    defaults: &ConfiguredDefaults,
    has_configured_default: bool,
) -> ModelDefinition {
    let default_effort = normalize_thinking_option(model.default_reasoning_effort.as_deref());
    let resolved_default = defaults.thinking_option_id.clone().or(default_effort);

    // insertion-ordered, de-duplicated
    let mut order: Vec<String> = Vec::new();
    let mut by_id: BTreeMap<String, SelectOption> = BTreeMap::new();
    for entry in &model.supported_reasoning_efforts {
        let Some(id) = normalize_thinking_option(entry.reasoning_effort.as_deref()) else {
            continue;
        };
        let description = entry.description.clone().filter(|d| !d.trim().is_empty());
        if !by_id.contains_key(&id) {
            order.push(id.clone());
        }
        by_id.insert(
            id.clone(),
            SelectOption {
                id: id.clone(),
                label: id,
                description,
                is_default: false,
            },
        );
    }
    if let Some(default) = &resolved_default {
        if !by_id.contains_key(default) {
            order.push(default.clone());
            by_id.insert(
                default.clone(),
                SelectOption {
                    id: default.clone(),
                    label: default.clone(),
                    description: Some(
                        if defaults.thinking_option_id.as_deref() == Some(default.as_str()) {
                            "Configured default reasoning effort"
                        } else {
                            "Model default reasoning effort"
                        }
                        .to_string(),
                    ),
                    is_default: false,
                },
            );
        }
    }
    let thinking_options: Vec<SelectOption> = order
        .iter()
        .filter_map(|id| by_id.remove(id))
        .map(|mut o| {
            o.is_default = resolved_default.as_deref() == Some(o.id.as_str());
            o
        })
        .collect();
    let default_thinking_option_id = resolved_default
        .or_else(|| {
            thinking_options
                .iter()
                .find(|o| o.is_default)
                .map(|o| o.id.clone())
        })
        .or_else(|| thinking_options.first().map(|o| o.id.clone()));

    let is_default = if has_configured_default {
        defaults.model.as_deref() == Some(model.id.as_str())
    } else {
        model.is_default
    };

    ModelDefinition {
        provider: PROVIDER.to_string(),
        id: model.id.clone(),
        label: normalize_model_label(model.display_name.as_deref().unwrap_or("")),
        description: model.description.clone(),
        is_default,
        thinking_options,
        default_thinking_option_id,
        metadata: json!({
            "model": model.model,
            "defaultReasoningEffort": model.default_reasoning_effort,
        }),
    }
}

pub fn build_catalog_models(
    models: &[ModelInfo],
    defaults: &ConfiguredDefaults,
) -> Vec<ModelDefinition> {
    let has_configured_default = defaults
        .model
        .as_deref()
        .is_some_and(|m| models.iter().any(|x| x.id == m));
    models
        .iter()
        .map(|m| build_model_definition(m, defaults, has_configured_default))
        .collect()
}

fn positive(v: i64) -> Option<i64> {
    (v > 0).then_some(v)
}

/// Last-turn token counts plus the context window, as Paseo reports usage.
pub fn to_usage(token_usage: &ThreadTokenUsage) -> Usage {
    Usage {
        input_tokens: Some(token_usage.last.input_tokens),
        cached_input_tokens: Some(token_usage.last.cached_input_tokens),
        output_tokens: Some(token_usage.last.output_tokens),
        context_window_max_tokens: token_usage.model_context_window.and_then(positive),
        context_window_used_tokens: positive(token_usage.last.total_tokens),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::params::ReasoningEffortEntry;

    fn model(id: &str, is_default: bool) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            model: Some(id.into()),
            display_name: Some("gpt-6-Luna".into()),
            description: None,
            hidden: false,
            is_default,
            default_reasoning_effort: Some("medium".into()),
            supported_reasoning_efforts: vec![ReasoningEffortEntry {
                reasoning_effort: Some("low".into()),
                description: Some("Fast".into()),
            }],
            service_tiers: vec![],
        }
    }

    #[test]
    fn default_effort_is_added_when_unsupported_list_omits_it() {
        let m = build_model_definition(&model("a", true), &ConfiguredDefaults::default(), false);
        assert_eq!(m.label, "GPT-6-Luna");
        assert_eq!(m.default_thinking_option_id.as_deref(), Some("medium"));
        let ids: Vec<_> = m.thinking_options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, vec!["low", "medium"]);
        assert!(m.thinking_options[1].is_default);
    }

    #[test]
    fn configured_default_model_wins_over_is_default() {
        let defaults = ConfiguredDefaults {
            model: Some("b".into()),
            thinking_option_id: None,
        };
        let models = vec![model("a", true), model("b", false)];
        let defs = build_catalog_models(&models, &defaults);
        assert!(!defs[0].is_default);
        assert!(defs[1].is_default);
    }
}
