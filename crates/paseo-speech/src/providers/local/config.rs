//! Port of `providers/local/config.ts`.

use std::path::{Path, PathBuf};

use crate::config::{env_get, first_defined, Env, PersistedSpeechConfig, SttLanguages};
use crate::error::{Result, SpeechError};
use crate::types::{RequestedSpeechProvider, RequestedSpeechProviders, SpeechProviderId};

use super::catalog::{
    default_stt_model, default_tts_model, parse_stt_model_id, parse_tts_model_id,
};

const DEFAULT_STT_LANGUAGE: &str = "en";

#[derive(Debug, Clone, PartialEq)]
pub struct LocalSpeechModelConfig {
    pub dictation_stt: String,
    pub voice_stt: String,
    pub voice_tts: String,
    pub voice_tts_speaker_id: Option<i64>,
    pub voice_tts_speed: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalSpeechProviderConfig {
    pub models_dir: PathBuf,
    pub models: LocalSpeechModelConfig,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedLocalSpeechConfig {
    pub local: Option<LocalSpeechProviderConfig>,
    pub stt_languages: SttLanguages,
}

/// Feature-scoped persisted model only counts while that feature uses the local provider.
fn persisted_local_model<'a>(
    provider: &RequestedSpeechProvider,
    model: Option<&'a str>,
) -> Option<&'a str> {
    if provider.provider != SpeechProviderId::Local || provider.enabled == Some(false) {
        return None;
    }
    model
}

fn local_or_soniox(provider: &RequestedSpeechProvider) -> bool {
    provider.enabled != Some(false)
        && matches!(
            provider.provider,
            SpeechProviderId::Local | SpeechProviderId::Soniox
        )
}

fn should_include_local_provider_config(
    providers: &RequestedSpeechProviders,
    env: &Env,
    persisted: &PersistedSpeechConfig,
) -> bool {
    let requested = local_or_soniox(&providers.dictation_stt)
        || local_or_soniox(&providers.voice_stt)
        || (providers.voice_tts.enabled != Some(false)
            && providers.voice_tts.provider == SpeechProviderId::Local);
    requested
        || env.contains_key("PASEO_LOCAL_MODELS_DIR")
        || persisted
            .providers
            .as_ref()
            .and_then(|p| p.local.as_ref())
            .is_some_and(|l| l.models_dir.is_some())
}

fn first_non_empty<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> Option<&'a str> {
    values
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|v| !v.is_empty())
}

fn parse_number(label: &str, raw: &str) -> Result<f64> {
    raw.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .ok_or_else(|| SpeechError::msg(format!("Invalid {label}: {raw:?}")))
}

fn parse_integer(label: &str, raw: &str) -> Result<i64> {
    let value = parse_number(label, raw)?;
    if value.fract() != 0.0 {
        return Err(SpeechError::msg(format!(
            "Invalid {label}: {raw:?} (expected an integer)"
        )));
    }
    Ok(value as i64)
}

fn resolve_model<'a>(
    label: &str,
    candidates: impl IntoIterator<Item = Option<&'a str>>,
    default: &'static str,
    parse: fn(&str) -> Option<&'static str>,
) -> Result<String> {
    let value = first_defined(candidates).unwrap_or(default);
    parse(value)
        .map(str::to_string)
        .ok_or_else(|| SpeechError::msg(format!("Invalid {label} model id: {value:?}")))
}

pub fn resolve_local_speech_config(
    paseo_home: &Path,
    env: &Env,
    persisted: &PersistedSpeechConfig,
    providers: &RequestedSpeechProviders,
) -> Result<ResolvedLocalSpeechConfig> {
    let include = should_include_local_provider_config(providers, env, persisted);

    let default_dir = paseo_home.join("models").join("local-speech");
    let models_dir_raw = first_defined([
        env_get(env, "PASEO_LOCAL_MODELS_DIR").map(str::to_string),
        persisted
            .providers
            .as_ref()
            .and_then(|p| p.local.as_ref())
            .and_then(|l| l.models_dir.clone()),
        Some(default_dir.to_string_lossy().into_owned()),
    ])
    .expect("default is always present");
    let models_dir = models_dir_raw.trim();
    if models_dir.is_empty() {
        return Err(SpeechError::msg(
            "Invalid local models dir: must not be empty",
        ));
    }

    let dictation_model = resolve_model(
        "dictation STT",
        [
            env_get(env, "PASEO_DICTATION_LOCAL_STT_MODEL"),
            persisted_local_model(
                &providers.dictation_stt,
                persisted.dictation_stt().and_then(|s| s.model.as_deref()),
            ),
        ],
        default_stt_model(),
        parse_stt_model_id,
    )?;
    let voice_stt_model = resolve_model(
        "voice STT",
        [
            env_get(env, "PASEO_VOICE_LOCAL_STT_MODEL"),
            persisted_local_model(
                &providers.voice_stt,
                persisted.voice_stt().and_then(|s| s.model.as_deref()),
            ),
        ],
        default_stt_model(),
        parse_stt_model_id,
    )?;
    let voice_tts_model = resolve_model(
        "voice TTS",
        [
            env_get(env, "PASEO_VOICE_LOCAL_TTS_MODEL"),
            persisted_local_model(
                &providers.voice_tts,
                persisted.voice_tts().and_then(|t| t.model.as_deref()),
            ),
        ],
        default_tts_model(),
        parse_tts_model_id,
    )?;

    let dictation_language = first_non_empty([
        env_get(env, "PASEO_DICTATION_LANGUAGE"),
        persisted
            .dictation_stt()
            .and_then(|s| s.language.as_deref()),
        Some(DEFAULT_STT_LANGUAGE),
    ])
    .unwrap_or(DEFAULT_STT_LANGUAGE)
    .to_string();
    let voice_language = first_non_empty([
        env_get(env, "PASEO_VOICE_LANGUAGE"),
        env_get(env, "PASEO_DICTATION_LANGUAGE"),
        persisted.voice_stt().and_then(|s| s.language.as_deref()),
        persisted
            .dictation_stt()
            .and_then(|s| s.language.as_deref()),
        Some(DEFAULT_STT_LANGUAGE),
    ])
    .unwrap_or(DEFAULT_STT_LANGUAGE)
    .to_string();

    let speaker_id = match env_get(env, "PASEO_VOICE_LOCAL_TTS_SPEAKER_ID") {
        Some(raw) => Some(parse_integer("voice TTS speaker id", raw)?),
        None => persisted.voice_tts().and_then(|t| t.speaker_id),
    };
    let speed = match env_get(env, "PASEO_VOICE_LOCAL_TTS_SPEED") {
        Some(raw) => Some(parse_number("voice TTS speed", raw)?),
        None => persisted.voice_tts().and_then(|t| t.speed),
    };
    let resolved_speaker_id =
        speaker_id.or_else(|| (voice_tts_model == "kokoro-en-v0_19").then_some(0));

    Ok(ResolvedLocalSpeechConfig {
        stt_languages: SttLanguages {
            dictation: dictation_language,
            voice: voice_language,
        },
        local: include.then(|| LocalSpeechProviderConfig {
            models_dir: PathBuf::from(models_dir),
            models: LocalSpeechModelConfig {
                dictation_stt: dictation_model,
                voice_stt: voice_stt_model,
                voice_tts: voice_tts_model,
                voice_tts_speaker_id: resolved_speaker_id,
                voice_tts_speed: speed,
            },
        }),
    })
}
