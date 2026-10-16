//! Model catalog mapping (`mapPiModel`, `resolvePiThinkingConfig`).

use serde_json::json;

use super::{ModelDefinition, SelectOption};
use crate::protocol::Model;

pub(crate) const DEFAULT_THINKING_LEVEL: &str = "medium";

const THINKING_OPTIONS: [(&str, &str, &str); 7] = [
    ("off", "Off", "No extra reasoning"),
    ("minimal", "Minimal", "Light reasoning"),
    ("low", "Low", "Faster reasoning"),
    ("medium", "Medium", "Balanced reasoning"),
    ("high", "High", "Deeper reasoning"),
    ("xhigh", "XHigh", "Very deep reasoning"),
    ("max", "Max", "Extreme reasoning"),
];

/// Thinking options a model supports, and the default one. Non-reasoning models have none.
pub fn thinking_config(model: &Model) -> (Vec<SelectOption>, Option<String>) {
    if model.reasoning != Some(true) {
        return (Vec::new(), None);
    }
    let map = model.thinking_level_map.as_ref();
    let supported: Vec<usize> = THINKING_OPTIONS
        .iter()
        .enumerate()
        .filter(|(_, (id, _, _))| {
            let mapped = map.and_then(|m| m.get(*id));
            match mapped {
                Some(v) if v.is_null() => false,
                None if matches!(*id, "xhigh" | "max") => false,
                _ => true,
            }
        })
        .map(|(i, _)| i)
        .collect();
    let default_index = THINKING_OPTIONS
        .iter()
        .position(|(id, _, _)| *id == DEFAULT_THINKING_LEVEL)
        .unwrap_or(0);
    // Pi clamps upward first, then falls back to the highest remaining lower level.
    let default = supported
        .iter()
        .copied()
        .find(|&i| i >= default_index)
        .or_else(|| supported.last().copied());
    let default_id = default.map_or("off", |i| THINKING_OPTIONS[i].0).to_string();
    let options = supported
        .iter()
        .map(|&i| {
            let (id, label, description) = THINKING_OPTIONS[i];
            SelectOption {
                id: id.to_string(),
                label: label.to_string(),
                description: Some(description.to_string()),
                is_default: id == default_id,
            }
        })
        .collect();
    (options, Some(default_id))
}

/// Pi model -> Paseo model definition. Label is the model name without vendor prefix, like
/// `transformPiModels`.
pub fn map_model(model: &Model) -> ModelDefinition {
    let (thinking_options, default_thinking_option_id) = thinking_config(model);
    let raw_label = model.name.clone().unwrap_or_else(|| model.id.clone());
    let qualified = model.qualified_id();
    let label = normalize_label(
        raw_label
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(&raw_label),
    );
    ModelDefinition {
        id: qualified.clone(),
        label,
        description: Some(qualified),
        is_default: false,
        thinking_options,
        default_thinking_option_id,
        metadata: json!({"provider": model.provider, "modelId": model.id}),
    }
}

fn normalize_label(label: &str) -> String {
    let collapsed = label
        .trim()
        .split(|c: char| c == '_' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    match collapsed.find(": ") {
        Some(i) => collapsed[i + 2..].trim().to_string(),
        None => collapsed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(json: serde_json::Value) -> Model {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn non_reasoning_has_no_thinking() {
        let m = model(json!({"provider":"nvidia","id":"google/gemma-3-4b-it","reasoning":false}));
        assert_eq!(thinking_config(&m), (Vec::new(), None));
    }

    #[test]
    fn level_map_prunes_and_picks_default() {
        // deepseek: only high and max are mapped; minimal/low/medium are null.
        let m = model(
            json!({"provider":"nvidia","id":"deepseek-v4.1-flash","reasoning":true,
            "thinkingLevelMap":{"minimal":null,"low":null,"medium":null,"high":"high","max":"max"}}),
        );
        let (options, default) = thinking_config(&m);
        let ids: Vec<_> = options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["off", "high", "max"]);
        assert_eq!(default.as_deref(), Some("high"));
    }

    #[test]
    fn xhigh_needs_an_explicit_mapping() {
        let m = model(
            json!({"provider":"p","id":"m","reasoning":true,"thinkingLevelMap":{"xhigh":"xhigh"}}),
        );
        let ids: Vec<_> = thinking_config(&m).0.into_iter().map(|o| o.id).collect();
        assert!(ids.contains(&"xhigh".to_string()));
        assert!(!ids.contains(&"max".to_string()));
    }

    #[test]
    fn labels_drop_vendor_prefix() {
        let m = model(
            json!({"provider":"nvidia","id":"openai/gpt-oss-20b","name":"OpenAI: GPT OSS 20B"}),
        );
        let def = map_model(&m);
        assert_eq!(def.id, "nvidia/openai/gpt-oss-20b");
        assert_eq!(def.label, "GPT OSS 20B");
    }
}
