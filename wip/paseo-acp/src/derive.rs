//! Mode, model, feature and selector derivation from ACP session state
//! (`deriveModesFromACP`, `deriveModelDefinitionsFromACP`, `deriveFeaturesFromACP`, ...).

use serde_json::json;

use crate::agent::{AgentMode, ConfigFeatureOption, Feature, ModelDefinition, SelectorOption};
use crate::protocol::{
    as_select, SelectChoice, SelectConfigOption, SelectEntry, SessionConfigOption,
    SessionModeState, SessionModelState,
};

#[derive(Debug, Clone, PartialEq)]
pub struct FlatChoice {
    pub choice: SelectChoice,
    pub group: Option<String>,
}

pub fn flatten_select_options(option: &SelectConfigOption) -> Vec<FlatChoice> {
    let mut out = Vec::new();
    for entry in &option.options {
        match entry {
            SelectEntry::Choice(c) => out.push(FlatChoice {
                choice: c.clone(),
                group: None,
            }),
            SelectEntry::Group(g) => {
                for c in &g.options {
                    out.push(FlatChoice {
                        choice: c.clone(),
                        group: Some(g.group.clone()),
                    });
                }
            }
        }
    }
    out
}

pub fn find_select_config_option<'a>(
    options: &'a [SessionConfigOption],
    category: &str,
    id: Option<&str>,
) -> Option<&'a SelectConfigOption> {
    options
        .iter()
        .filter_map(as_select)
        .find(|o| o.category.as_deref() == Some(category) && id.is_none_or(|i| o.id == i))
}

pub fn find_select_config_option_by_id<'a>(
    options: &'a [SessionConfigOption],
    id: &str,
) -> Option<&'a SelectConfigOption> {
    options.iter().filter_map(as_select).find(|o| o.id == id)
}

pub fn find_select_config_feature_option<'a>(
    options: &'a [SessionConfigOption],
    feature: &ConfigFeatureOption,
) -> Option<&'a SelectConfigOption> {
    options.iter().filter_map(as_select).find(|o| {
        o.id == feature.config_id
            && feature
                .category
                .as_ref()
                .is_none_or(|c| o.category.as_deref() == Some(c))
    })
}

pub fn find_select_config_choice(
    option: Option<&SelectConfigOption>,
    value: &str,
) -> Option<FlatChoice> {
    flatten_select_options(option?)
        .into_iter()
        .find(|c| c.choice.value == value)
}

fn group_metadata(group: &Option<String>) -> Option<serde_json::Value> {
    group.as_ref().map(|g| json!({ "group": g }))
}

pub fn derive_selector_options(
    options: &[SessionConfigOption],
    category: &str,
) -> Vec<SelectorOption> {
    let Some(option) = find_select_config_option(options, category, None) else {
        return Vec::new();
    };
    flatten_select_options(option)
        .into_iter()
        .map(|f| SelectorOption {
            is_default: f.choice.value == option.current_value,
            id: f.choice.value,
            label: f.choice.name,
            description: f.choice.description,
            metadata: group_metadata(&f.group),
        })
        .collect()
}

pub fn derive_current_config_value(
    options: &[SessionConfigOption],
    category: &str,
) -> Option<String> {
    options
        .iter()
        .filter_map(as_select)
        .find(|o| o.category.as_deref() == Some(category))
        .map(|o| o.current_value.clone())
}

pub fn derive_modes(
    fallback: &[AgentMode],
    mode_state: Option<&SessionModeState>,
    options: &[SessionConfigOption],
) -> (Vec<AgentMode>, Option<String>) {
    if let Some(modes) = mode_state
        .and_then(|s| s.available_modes.as_ref())
        .filter(|m| !m.is_empty())
    {
        return (
            modes
                .iter()
                .map(|m| AgentMode {
                    id: m.id.clone(),
                    label: m.name.clone(),
                    description: m.description.clone(),
                })
                .collect(),
            mode_state.and_then(|s| s.current_mode_id.clone()),
        );
    }
    if let Some(option) = find_select_config_option(options, "mode", None) {
        return (
            flatten_select_options(option)
                .into_iter()
                .map(|f| AgentMode {
                    id: f.choice.value,
                    label: f.choice.name,
                    description: f.choice.description,
                })
                .collect(),
            Some(option.current_value.clone()),
        );
    }
    (fallback.to_vec(), None)
}

pub fn derive_model_definitions(
    provider: &str,
    models: Option<&SessionModelState>,
    options: &[SessionConfigOption],
) -> Vec<ModelDefinition> {
    let thinking = derive_selector_options(options, "thought_level");
    let default_thinking = thinking.iter().find(|o| o.is_default).map(|o| o.id.clone());

    if let Some(available) = models
        .and_then(|m| m.available_models.as_ref())
        .filter(|m| !m.is_empty())
    {
        let current = models.and_then(|m| m.current_model_id.as_deref());
        return available
            .iter()
            .map(|m| ModelDefinition {
                provider: provider.to_string(),
                id: m.model_id.clone(),
                label: m.name.clone(),
                description: m.description.clone(),
                is_default: Some(m.model_id.as_str()) == current,
                metadata: None,
                thinking_options: thinking.clone(),
                default_thinking_option_id: default_thinking.clone(),
            })
            .collect();
    }

    derive_selector_options(options, "model")
        .into_iter()
        .map(|o| ModelDefinition {
            provider: provider.to_string(),
            id: o.id,
            label: o.label,
            description: o.description,
            is_default: o.is_default,
            metadata: o.metadata,
            thinking_options: thinking.clone(),
            default_thinking_option_id: default_thinking.clone(),
        })
        .collect()
}

fn feature_option_label(choice: &SelectChoice, feature: &ConfigFeatureOption) -> String {
    let name = choice.name.trim();
    if !name.is_empty() {
        return name.to_string();
    }
    if choice.value.is_empty() {
        if let Some(label) = &feature.empty_option_label {
            return label.clone();
        }
    }
    choice.value.clone()
}

pub fn derive_features(
    options: &[SessionConfigOption],
    features: &[ConfigFeatureOption],
) -> Vec<Feature> {
    features
        .iter()
        .filter_map(|feature| {
            let option = find_select_config_feature_option(options, feature)?;
            Some(Feature::Select {
                id: feature.id.clone(),
                label: feature.label.clone(),
                description: feature.description.clone(),
                tooltip: feature.tooltip.clone(),
                icon: feature.icon.clone(),
                value: Some(option.current_value.clone()),
                options: flatten_select_options(option)
                    .into_iter()
                    .map(|f| SelectorOption {
                        is_default: f.choice.value == option.current_value,
                        label: feature_option_label(&f.choice, feature),
                        description: f.choice.description.clone(),
                        metadata: group_metadata(&f.group),
                        id: f.choice.value,
                    })
                    .collect(),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModeSelection<'a> {
    pub available_mode: Option<AgentMode>,
    pub config_option: Option<&'a SelectConfigOption>,
    pub config_choice: Option<FlatChoice>,
    pub has_available_modes: bool,
}

pub fn resolve_mode_selection<'a>(
    mode_id: &str,
    available_modes: &[AgentMode],
    options: &'a [SessionConfigOption],
) -> ModeSelection<'a> {
    let config_option = find_select_config_option(options, "mode", None);
    ModeSelection {
        available_mode: available_modes.iter().find(|m| m.id == mode_id).cloned(),
        config_option,
        config_choice: find_select_config_choice(config_option, mode_id),
        has_available_modes: !available_modes.is_empty(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelSelection<'a> {
    pub available_model: Option<crate::protocol::ModelInfo>,
    pub config_option: Option<&'a SelectConfigOption>,
    pub config_choice: Option<FlatChoice>,
    pub has_available_models: bool,
}

pub fn resolve_model_selection<'a>(
    model_id: &str,
    available_models: Option<&[crate::protocol::ModelInfo]>,
    options: &'a [SessionConfigOption],
) -> ModelSelection<'a> {
    let config_option = find_select_config_option(options, "model", None);
    ModelSelection {
        available_model: available_models
            .and_then(|m| m.iter().find(|x| x.model_id == model_id).cloned()),
        config_option,
        config_choice: find_select_config_choice(config_option, model_id),
        has_available_models: available_models.is_some_and(|m| !m.is_empty()),
    }
}
