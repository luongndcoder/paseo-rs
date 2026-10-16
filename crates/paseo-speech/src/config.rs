//! Speech config resolution (port of `speech-config-resolver.ts`).
//!
//! `env` is an explicit map so resolution stays pure; use [`env_from_process`] for the real one.
//! Persisted config is the speech-relevant subset of `config.json`; unknown keys are ignored.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

use crate::error::{Result, SpeechError};
use crate::providers::local::config::{resolve_local_speech_config, LocalSpeechProviderConfig};
use crate::providers::openai::config::{resolve_openai_speech_config, OpenAiSpeechProviderConfig};
use crate::providers::soniox::config::{
    resolve_soniox_config_from_env, SonioxSpeechProviderConfig,
};
use crate::types::{RequestedSpeechProvider, RequestedSpeechProviders, SpeechProviderId};

pub type Env = HashMap<String, String>;

pub fn env_from_process() -> Env {
    std::env::vars().collect()
}

// ---- persisted config subset ------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedSpeechConfig {
    pub features: Option<PersistedFeatures>,
    pub providers: Option<PersistedProviders>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedFeatures {
    pub dictation: Option<PersistedDictation>,
    pub voice_mode: Option<PersistedVoiceMode>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedDictation {
    pub enabled: Option<bool>,
    pub stt: Option<PersistedDictationStt>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedDictationStt {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub language: Option<String>,
    pub confidence_threshold: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedVoiceMode {
    pub enabled: Option<bool>,
    pub stt: Option<PersistedVoiceStt>,
    pub turn_detection: Option<PersistedTurnDetection>,
    pub tts: Option<PersistedVoiceTts>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedVoiceStt {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedTurnDetection {
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedVoiceTts {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub voice: Option<String>,
    pub speaker_id: Option<i64>,
    pub speed: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedProviders {
    pub openai: Option<PersistedOpenAi>,
    pub local: Option<PersistedLocal>,
    pub soniox: Option<PersistedSoniox>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedOpenAi {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub stt: Option<PersistedOpenAiEndpoint>,
    pub tts: Option<PersistedOpenAiEndpoint>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedOpenAiEndpoint {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedLocal {
    pub models_dir: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedSoniox {
    pub api_key: Option<String>,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub language_hints: Option<Vec<String>>,
}

impl PersistedSpeechConfig {
    pub fn from_json(json: &str) -> Result<Self> {
        serde_json::from_str(json)
            .map_err(|e| SpeechError::msg(format!("Invalid persisted config: {e}")))
    }

    pub(crate) fn dictation(&self) -> Option<&PersistedDictation> {
        self.features.as_ref()?.dictation.as_ref()
    }
    pub(crate) fn dictation_stt(&self) -> Option<&PersistedDictationStt> {
        self.dictation()?.stt.as_ref()
    }
    pub(crate) fn voice_mode(&self) -> Option<&PersistedVoiceMode> {
        self.features.as_ref()?.voice_mode.as_ref()
    }
    pub(crate) fn voice_stt(&self) -> Option<&PersistedVoiceStt> {
        self.voice_mode()?.stt.as_ref()
    }
    pub(crate) fn voice_tts(&self) -> Option<&PersistedVoiceTts> {
        self.voice_mode()?.tts.as_ref()
    }
}

// ---- resolution -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct SttLanguages {
    pub dictation: String,
    pub voice: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PaseoSpeechConfig {
    pub providers: RequestedSpeechProviders,
    pub stt_languages: SttLanguages,
    pub local: Option<LocalSpeechProviderConfig>,
    pub soniox: SonioxSpeechProviderConfig,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedSpeechConfig {
    pub openai: Option<OpenAiSpeechProviderConfig>,
    pub speech: PaseoSpeechConfig,
}

/// First entry that is not `None` (JS `value !== undefined && value !== null`).
pub(crate) fn first_defined<T>(values: impl IntoIterator<Item = Option<T>>) -> Option<T> {
    values.into_iter().flatten().next()
}

pub(crate) fn env_get<'a>(env: &'a Env, key: &str) -> Option<&'a str> {
    env.get(key).map(String::as_str)
}

/// Env flag parsing: booleans, `1/true/yes/y/on`, `0/false/no/n/off`; anything else is "unset".
fn parse_flag(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

enum FlagSource<'a> {
    Env(&'a str),
    Persisted(bool),
}

fn resolve_flag(sources: [Option<FlagSource<'_>>; 2]) -> bool {
    match first_defined(sources) {
        Some(FlagSource::Persisted(b)) => b,
        Some(FlagSource::Env(s)) => parse_flag(s).unwrap_or(true),
        None => true,
    }
}

fn flag(env: &Env, key: &str, persisted: Option<bool>) -> bool {
    resolve_flag([
        env_get(env, key).map(FlagSource::Env),
        persisted.map(FlagSource::Persisted),
    ])
}

fn requested(configured: Option<&str>, enabled: bool) -> Result<RequestedSpeechProvider> {
    let provider = match configured {
        None => SpeechProviderId::Local,
        Some(value) => SpeechProviderId::parse(value).ok_or_else(|| {
            SpeechError::msg(format!(
                "Invalid speech provider \"{value}\" (expected one of: openai, local, soniox)"
            ))
        })?,
    };
    Ok(RequestedSpeechProvider {
        provider,
        explicit: configured.is_some(),
        enabled: Some(enabled),
    })
}

pub fn resolve_requested_speech_providers(
    env: &Env,
    persisted: &PersistedSpeechConfig,
) -> Result<RequestedSpeechProviders> {
    let voice_enabled = flag(
        env,
        "PASEO_VOICE_MODE_ENABLED",
        persisted.voice_mode().and_then(|v| v.enabled),
    );
    let dictation_enabled = flag(
        env,
        "PASEO_DICTATION_ENABLED",
        persisted.dictation().and_then(|d| d.enabled),
    );

    let dictation_cfg = first_defined([
        env_get(env, "PASEO_DICTATION_STT_PROVIDER"),
        persisted
            .dictation_stt()
            .and_then(|s| s.provider.as_deref()),
    ]);
    let turn_cfg = first_defined([
        env_get(env, "PASEO_VOICE_TURN_DETECTION_PROVIDER"),
        persisted
            .voice_mode()
            .and_then(|v| v.turn_detection.as_ref())
            .and_then(|t| t.provider.as_deref()),
    ]);
    let voice_stt_cfg = first_defined([
        env_get(env, "PASEO_VOICE_STT_PROVIDER"),
        persisted.voice_stt().and_then(|s| s.provider.as_deref()),
    ]);
    let voice_tts_cfg = first_defined([
        env_get(env, "PASEO_VOICE_TTS_PROVIDER"),
        persisted.voice_tts().and_then(|t| t.provider.as_deref()),
    ]);

    Ok(RequestedSpeechProviders {
        dictation_stt: requested(dictation_cfg, dictation_enabled)?,
        voice_turn_detection: requested(turn_cfg, voice_enabled)?,
        voice_stt: requested(voice_stt_cfg, voice_enabled)?,
        voice_tts: requested(voice_tts_cfg, voice_enabled)?,
    })
}

pub fn resolve_speech_config(
    paseo_home: &Path,
    env: &Env,
    persisted: &PersistedSpeechConfig,
) -> Result<ResolvedSpeechConfig> {
    let providers = resolve_requested_speech_providers(env, persisted)?;
    let local = resolve_local_speech_config(paseo_home, env, persisted, &providers)?;
    let openai = resolve_openai_speech_config(env, persisted, &providers)?;
    let soniox = resolve_soniox_config_from_env(env, Some(persisted));
    Ok(ResolvedSpeechConfig {
        openai,
        speech: PaseoSpeechConfig {
            providers,
            stt_languages: local.stt_languages,
            local: local.local,
            soniox,
        },
    })
}
