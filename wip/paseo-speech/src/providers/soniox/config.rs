//! Port of `providers/soniox/config.ts`.

use crate::config::{env_get, Env, PersistedSpeechConfig};

pub const DEFAULT_SONIOX_WS_ENDPOINT: &str = "wss://stt-rt.soniox.com/transcribe-websocket";
pub const DEFAULT_SONIOX_STT_MODEL: &str = "stt-rt-v5";

pub fn default_soniox_language_hints() -> Vec<String> {
    vec!["vi".to_string(), "en".to_string()]
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SonioxSpeechProviderConfig {
    pub api_key: Option<String>,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub language_hints: Option<Vec<String>>,
}

fn non_empty(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn parse_language_hints_env(value: Option<&str>) -> Option<Vec<String>> {
    let trimmed = non_empty(value)?;
    let mut hints: Vec<String> = Vec::new();
    for item in trimmed
        .split(',')
        .map(|i| i.trim().to_lowercase())
        .filter(|i| !i.is_empty())
    {
        if !hints.contains(&item) {
            hints.push(item);
        }
    }
    (!hints.is_empty()).then_some(hints)
}

pub fn resolve_soniox_config_from_env(
    env: &Env,
    persisted: Option<&PersistedSpeechConfig>,
) -> SonioxSpeechProviderConfig {
    let soniox = persisted
        .and_then(|p| p.providers.as_ref())
        .and_then(|p| p.soniox.as_ref());
    let api_key = non_empty(env_get(env, "SONIOX_API_KEY"))
        .or_else(|| non_empty(soniox.and_then(|s| s.api_key.as_deref())));
    let endpoint = non_empty(env_get(env, "SONIOX_STT_ENDPOINT"))
        .or_else(|| non_empty(soniox.and_then(|s| s.endpoint.as_deref())))
        .unwrap_or_else(|| DEFAULT_SONIOX_WS_ENDPOINT.to_string());
    let model = non_empty(env_get(env, "SONIOX_STT_MODEL"))
        .or_else(|| non_empty(soniox.and_then(|s| s.model.as_deref())))
        .unwrap_or_else(|| DEFAULT_SONIOX_STT_MODEL.to_string());
    let language_hints = parse_language_hints_env(env_get(env, "SONIOX_LANGUAGE_HINTS"))
        .unwrap_or_else(|| match soniox.and_then(|s| s.language_hints.as_ref()) {
            Some(hints) if !hints.is_empty() => hints.clone(),
            _ => default_soniox_language_hints(),
        });
    SonioxSpeechProviderConfig {
        api_key,
        endpoint: Some(endpoint),
        model: Some(model),
        language_hints: Some(language_hints),
    }
}
