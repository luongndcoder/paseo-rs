//! Port of `providers/openai/config.ts`.

use crate::config::{env_get, Env, PersistedSpeechConfig};
use crate::error::{Result, SpeechError};
use crate::types::{RequestedSpeechProvider, RequestedSpeechProviders, SpeechProviderId};

pub const DEFAULT_OPENAI_TTS_MODEL: &str = "tts-1";
pub const DEFAULT_OPENAI_TTS_VOICE: &str = "alloy";

#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiSttConfig {
    pub api_key: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub confidence_threshold: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiTtsConfig {
    pub api_key: String,
    pub base_url: Option<String>,
    pub model: String,
    pub voice: String,
    pub response_format: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OpenAiSpeechProviderConfig {
    pub stt: Option<OpenAiSttConfig>,
    pub tts: Option<OpenAiTtsConfig>,
}

fn is_openai_active(provider: &RequestedSpeechProvider) -> bool {
    provider.enabled != Some(false) && provider.provider == SpeechProviderId::Openai
}

fn pick_if_openai<'a>(
    provider: &RequestedSpeechProvider,
    value: Option<&'a str>,
) -> Option<&'a str> {
    if is_openai_active(provider) {
        value
    } else {
        None
    }
}

/// First defined, non-blank value. Blank env vars (a copied `.env.example` with
/// `OPENAI_STT_API_KEY=`) must not shadow a later fallback such as `OPENAI_API_KEY`.
fn first_set<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> Option<&'a str> {
    values.into_iter().flatten().find(|v| !v.trim().is_empty())
}

fn trimmed(value: Option<&str>) -> Option<String> {
    let t = value?.trim();
    (!t.is_empty()).then(|| t.to_string())
}

pub fn resolve_openai_speech_config(
    env: &Env,
    persisted: &PersistedSpeechConfig,
    providers: &RequestedSpeechProviders,
) -> Result<Option<OpenAiSpeechProviderConfig>> {
    let openai = persisted.providers.as_ref().and_then(|p| p.openai.as_ref());
    let stt_cfg = openai.and_then(|o| o.stt.as_ref());
    let tts_cfg = openai.and_then(|o| o.tts.as_ref());

    let stt_api_key = trimmed(first_set([
        stt_cfg.and_then(|c| c.api_key.as_deref()),
        env_get(env, "OPENAI_STT_API_KEY"),
        openai.and_then(|o| o.api_key.as_deref()),
        env_get(env, "OPENAI_API_KEY"),
    ]));
    let stt_base_url = trimmed(first_set([
        stt_cfg.and_then(|c| c.base_url.as_deref()),
        env_get(env, "OPENAI_STT_BASE_URL"),
        openai.and_then(|o| o.base_url.as_deref()),
        env_get(env, "OPENAI_BASE_URL"),
    ]));
    let tts_api_key = trimmed(first_set([
        tts_cfg.and_then(|c| c.api_key.as_deref()),
        env_get(env, "OPENAI_TTS_API_KEY"),
        openai.and_then(|o| o.api_key.as_deref()),
        env_get(env, "OPENAI_API_KEY"),
    ]));
    let tts_base_url = trimmed(first_set([
        tts_cfg.and_then(|c| c.base_url.as_deref()),
        env_get(env, "OPENAI_TTS_BASE_URL"),
        openai.and_then(|o| o.base_url.as_deref()),
        env_get(env, "OPENAI_BASE_URL"),
    ]));

    if stt_api_key.is_none() && tts_api_key.is_none() {
        return Ok(None);
    }

    // The STT/TTS option groups parse only for the endpoint that is configured, so a stale env
    // var for an unused endpoint (a leftover TTS_VOICE in an STT-only setup) cannot break the other.
    let stt = match stt_api_key {
        None => None,
        Some(api_key) => {
            let threshold_raw = first_set([env_get(env, "STT_CONFIDENCE_THRESHOLD")]);
            let confidence_threshold = match threshold_raw {
                Some(raw) => Some(
                    raw.trim()
                        .parse::<f64>()
                        .ok()
                        .filter(|v| v.is_finite())
                        .ok_or_else(|| {
                            SpeechError::msg(format!("Invalid STT_CONFIDENCE_THRESHOLD: {raw:?}"))
                        })?,
                ),
                None => persisted
                    .dictation_stt()
                    .and_then(|s| s.confidence_threshold),
            };
            let model = trimmed(first_set([
                env_get(env, "STT_MODEL"),
                pick_if_openai(
                    &providers.voice_stt,
                    persisted.voice_stt().and_then(|s| s.model.as_deref()),
                ),
                pick_if_openai(
                    &providers.dictation_stt,
                    persisted.dictation_stt().and_then(|s| s.model.as_deref()),
                ),
            ]));
            Some(OpenAiSttConfig {
                api_key,
                base_url: stt_base_url,
                model,
                confidence_threshold,
            })
        }
    };

    let tts = tts_api_key.map(|api_key| {
        // Free-form: self-hosted OpenAI-compatible servers own their ids and their case.
        let voice = first_set([
            env_get(env, "TTS_VOICE"),
            pick_if_openai(
                &providers.voice_tts,
                persisted.voice_tts().and_then(|t| t.voice.as_deref()),
            ),
            Some(DEFAULT_OPENAI_TTS_VOICE),
        ])
        .unwrap_or(DEFAULT_OPENAI_TTS_VOICE)
        .trim()
        .to_string();
        let model = first_set([
            env_get(env, "TTS_MODEL"),
            pick_if_openai(
                &providers.voice_tts,
                persisted.voice_tts().and_then(|t| t.model.as_deref()),
            ),
            Some(DEFAULT_OPENAI_TTS_MODEL),
        ])
        .unwrap_or(DEFAULT_OPENAI_TTS_MODEL)
        .trim()
        .to_string();
        OpenAiTtsConfig {
            api_key,
            base_url: tts_base_url,
            model,
            voice,
            response_format: "pcm".to_string(),
        }
    });

    Ok(Some(OpenAiSpeechProviderConfig { stt, tts }))
}
