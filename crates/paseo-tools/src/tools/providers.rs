//! Provider tools: list_providers, list_models, list_profiles, inspect_provider.

use std::sync::Arc;

use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

use super::{handler, obj_arg, ok_structured, required_str, str_arg, ToolEnv, ToolOutcome};
use crate::catalog::Registrar;
use crate::core_api::ProviderEntry;
use crate::error::ToolError;
use crate::util::resolve_schedule_provider_and_model;

pub(crate) fn register(r: &mut Registrar) {
    r.register("list_providers", handler(list_providers));
    r.register("list_models", handler(list_models));
    r.register("list_profiles", handler(list_profiles));
    r.register("inspect_provider", handler(inspect_provider));
}

/// `toProviderSummary`.
fn provider_summary(entry: &ProviderEntry) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), json!(entry.provider));
    map.insert(
        "label".into(),
        json!(entry
            .label
            .clone()
            .unwrap_or_else(|| entry.provider.clone())),
    );
    map.insert(
        "description".into(),
        json!(entry.description.clone().unwrap_or_default()),
    );
    map.insert("enabled".into(), json!(entry.enabled));
    map.insert(
        "modes".into(),
        Value::Array(entry.modes.clone().unwrap_or_default()),
    );
    let status = if entry.status == "ready" {
        "available".to_owned()
    } else {
        entry.status.clone()
    };
    map.insert("status".into(), json!(status));
    if let Some(error) = entry.error.as_ref().filter(|e| !e.is_empty()) {
        map.insert("error".into(), json!(error));
    }
    Value::Object(map)
}

async fn list_providers(env: Arc<ToolEnv>, _input: Value, _c: CancellationToken) -> ToolOutcome {
    let providers: Vec<Value> = env
        .core
        .list_providers()
        .await?
        .iter()
        .map(provider_summary)
        .collect();
    ok_structured(json!({"providers": providers}))
}

async fn list_models(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    let provider = required_str(&input, "provider");
    let models = env.core.list_models(provider).await?;
    ok_structured(json!({"provider": provider, "models": models}))
}

async fn list_profiles(env: Arc<ToolEnv>, _input: Value, _c: CancellationToken) -> ToolOutcome {
    ok_structured(json!({"profiles": env.core.agent_profiles()}))
}

fn truthy(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.is_empty())
}

async fn inspect_provider(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    let provider_input = required_str(&input, "provider");
    let (provider_id, resolved_model) =
        resolve_schedule_provider_and_model(Some(provider_input), provider_input)?;
    let resolved_cwd = env.resolve_scoped_cwd(str_arg(&input, "cwd"), true)?;
    let entry = env.core.get_provider(&resolved_cwd, &provider_id).await?;
    let summary = provider_summary(&entry);
    if !entry.enabled {
        return Err(ToolError::new(format!(
            "Provider '{provider_id}' is disabled"
        )));
    }
    if entry.status != "ready" {
        return Err(ToolError::new(
            entry
                .error
                .clone()
                .filter(|e| !e.is_empty())
                .unwrap_or_else(|| format!("Provider '{provider_id}' is unavailable")),
        ));
    }
    let settings = obj_arg(&input, "settings");
    let setting_str = |key: &str| settings.and_then(|s| s.get(key)).and_then(Value::as_str);
    // `settings?.model ?? resolvedProviderModel.model`
    let selected_model: Option<String> = setting_str("model").map(str::to_owned).or(resolved_model);

    let mut request = Map::new();
    request.insert("provider".into(), json!(provider_id));
    request.insert("cwd".into(), json!(resolved_cwd));
    if let Some(mode) = truthy(setting_str("modeId")) {
        request.insert("modeId".into(), json!(mode));
    }
    if let Some(model) = truthy(selected_model.as_deref()) {
        request.insert("model".into(), json!(model));
    }
    if let Some(thinking) = truthy(setting_str("thinkingOptionId")) {
        request.insert("thinkingOptionId".into(), json!(thinking));
    }
    if let Some(values) = settings.and_then(|s| s.get("features")) {
        request.insert("featureValues".into(), values.clone());
    }
    let features = env.core.list_draft_features(Value::Object(request)).await?;
    ok_structured(json!({
        "provider": provider_id,
        "label": summary["label"],
        "description": summary["description"],
        "enabled": summary["enabled"],
        "status": summary["status"],
        "modes": summary["modes"],
        "selectedModel": selected_model,
        "features": features,
    }))
}
