//! Ports of `speech-config-resolver.test.ts`, `openai/config.test.ts` and the Soniox config test,
//! plus the dictation-language cases from `dictation-stream-manager.test.ts`.

use std::path::{Path, PathBuf};

use paseo_speech::config::{
    resolve_speech_config, Env, PersistedSpeechConfig, ResolvedSpeechConfig,
};
use paseo_speech::providers::local::config::resolve_local_speech_config;
use paseo_speech::providers::openai::config::resolve_openai_speech_config;
use paseo_speech::providers::soniox::config::{
    resolve_soniox_config_from_env, SonioxSpeechProviderConfig, DEFAULT_SONIOX_STT_MODEL,
    DEFAULT_SONIOX_WS_ENDPOINT,
};
use paseo_speech::types::{RequestedSpeechProvider, RequestedSpeechProviders, SpeechProviderId};

fn env(pairs: &[(&str, &str)]) -> Env {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn persisted(json: &str) -> PersistedSpeechConfig {
    PersistedSpeechConfig::from_json(json).unwrap()
}

fn resolve(env: &Env, persisted: &PersistedSpeechConfig) -> ResolvedSpeechConfig {
    resolve_speech_config(Path::new("/tmp/paseo-home"), env, persisted).unwrap()
}

fn provider(p: SpeechProviderId, explicit: bool, enabled: Option<bool>) -> RequestedSpeechProvider {
    RequestedSpeechProvider {
        provider: p,
        explicit,
        enabled,
    }
}

#[test]
fn resolves_local_first_defaults_without_env_overrides() {
    let result = resolve(&env(&[]), &persisted("{}"));
    assert!(result.openai.is_none());
    let local = provider(SpeechProviderId::Local, false, Some(true));
    assert_eq!(result.speech.providers.dictation_stt, local);
    assert_eq!(result.speech.providers.voice_turn_detection, local);
    assert_eq!(result.speech.providers.voice_stt, local);
    assert_eq!(result.speech.providers.voice_tts, local);

    let local_cfg = result.speech.local.expect("local config");
    assert_eq!(
        local_cfg.models_dir,
        PathBuf::from("/tmp/paseo-home/models/local-speech")
    );
    assert_eq!(local_cfg.models.dictation_stt, "parakeet-tdt-0.6b-v2-int8");
    assert_eq!(local_cfg.models.voice_stt, "parakeet-tdt-0.6b-v2-int8");
    assert_eq!(local_cfg.models.voice_tts, "kokoro-en-v0_19");
    assert_eq!(local_cfg.models.voice_tts_speaker_id, Some(0));
    assert_eq!(local_cfg.models.voice_tts_speed, None);
    assert_eq!(result.speech.stt_languages.dictation, "en");
    assert_eq!(result.speech.stt_languages.voice, "en");
}

#[test]
fn resolves_feature_scoped_local_speech_settings() {
    let persisted = persisted(
        r#"{
        "features": { "voiceMode": {
            "turnDetection": { "provider": "local" },
            "stt": { "provider": "openai", "model": "gpt-4o-transcribe" } } },
        "providers": { "openai": { "apiKey": "persisted-key" } }
    }"#,
    );
    let env = env(&[
        (
            "PASEO_DICTATION_LOCAL_STT_MODEL",
            "parakeet-tdt-0.6b-v2-int8",
        ),
        ("PASEO_VOICE_LOCAL_STT_MODEL", "parakeet-tdt-0.6b-v2-int8"),
        ("PASEO_VOICE_LOCAL_TTS_MODEL", "kokoro-en-v0_19"),
        ("PASEO_VOICE_LOCAL_TTS_SPEAKER_ID", "5"),
        ("PASEO_VOICE_LOCAL_TTS_SPEED", "1.35"),
        ("PASEO_DICTATION_LANGUAGE", "es"),
        ("PASEO_VOICE_LANGUAGE", "pt"),
        ("PASEO_LOCAL_MODELS_DIR", "/tmp/models"),
        ("OPENAI_API_KEY", "env-key"),
        ("PASEO_VOICE_STT_PROVIDER", "openai"),
        ("PASEO_DICTATION_STT_PROVIDER", "local"),
        ("PASEO_VOICE_TTS_PROVIDER", "local"),
    ]);
    let result = resolve(&env, &persisted);

    let local = result.speech.local.expect("local");
    assert_eq!(local.models_dir, PathBuf::from("/tmp/models"));
    assert_eq!(local.models.voice_tts_speaker_id, Some(5));
    assert_eq!(local.models.voice_tts_speed, Some(1.35));
    let p = result.speech.providers;
    assert_eq!(
        p.dictation_stt,
        provider(SpeechProviderId::Local, true, Some(true))
    );
    assert_eq!(
        p.voice_stt,
        provider(SpeechProviderId::Openai, true, Some(true))
    );
    assert_eq!(
        p.voice_turn_detection,
        provider(SpeechProviderId::Local, true, Some(true))
    );
    assert_eq!(
        p.voice_tts,
        provider(SpeechProviderId::Local, true, Some(true))
    );
    assert_eq!(result.speech.stt_languages.dictation, "es");
    assert_eq!(result.speech.stt_languages.voice, "pt");
    let openai = result.openai.expect("openai");
    assert_eq!(openai.stt.as_ref().unwrap().api_key, "persisted-key");
    assert_eq!(openai.tts.as_ref().unwrap().api_key, "persisted-key");
    assert_eq!(
        openai.stt.as_ref().unwrap().model.as_deref(),
        Some("gpt-4o-transcribe")
    );
}

#[test]
fn resolves_stt_language_from_env_settings_and_voice_to_dictation_fallback() {
    let persisted = persisted(
        r#"{"features":{"dictation":{"stt":{"language":"fr"}},"voiceMode":{"stt":{"language":"de"}}}}"#,
    );
    let result = resolve(
        &env(&[
            ("PASEO_DICTATION_LANGUAGE", "es"),
            ("PASEO_VOICE_LANGUAGE", "  "),
        ]),
        &persisted,
    );
    assert_eq!(result.speech.stt_languages.dictation, "es");
    assert_eq!(result.speech.stt_languages.voice, "es");
}

#[test]
fn respects_disabled_dictation_and_voice_mode_feature_flags() {
    let persisted =
        persisted(r#"{"features":{"dictation":{"enabled":false},"voiceMode":{"enabled":false}}}"#);
    let result = resolve(&env(&[]), &persisted);
    let off = provider(SpeechProviderId::Local, false, Some(false));
    let p = result.speech.providers;
    assert_eq!(p.dictation_stt, off);
    assert_eq!(p.voice_turn_detection, off);
    assert_eq!(p.voice_stt, off);
    assert_eq!(p.voice_tts, off);
}

#[test]
fn env_flag_strings_are_parsed_and_unknown_values_mean_enabled() {
    let off = resolve(
        &env(&[("PASEO_DICTATION_ENABLED", " No ")]),
        &persisted("{}"),
    );
    assert_eq!(off.speech.providers.dictation_stt.enabled, Some(false));
    let unknown = resolve(
        &env(&[("PASEO_DICTATION_ENABLED", "maybe")]),
        &persisted("{}"),
    );
    assert_eq!(unknown.speech.providers.dictation_stt.enabled, Some(true));
    // env wins over persisted even when it is not a recognised flag
    let env_over = resolve(
        &env(&[("PASEO_VOICE_MODE_ENABLED", "")]),
        &persisted(r#"{"features":{"voiceMode":{"enabled":false}}}"#),
    );
    assert_eq!(env_over.speech.providers.voice_stt.enabled, Some(true));
}

#[test]
fn provider_ids_are_trimmed_and_case_insensitive_and_unknown_ids_error() {
    let r = resolve(
        &env(&[("PASEO_DICTATION_STT_PROVIDER", "  SONIOX ")]),
        &persisted("{}"),
    );
    assert_eq!(
        r.speech.providers.dictation_stt.provider,
        SpeechProviderId::Soniox
    );
    assert!(r.speech.providers.dictation_stt.explicit);
    let err = resolve_speech_config(
        Path::new("/tmp/h"),
        &env(&[("PASEO_VOICE_STT_PROVIDER", "azure")]),
        &persisted("{}"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("azure"), "{err}");
}

#[test]
fn invalid_local_model_id_is_an_error_not_a_panic() {
    let err = resolve_speech_config(
        Path::new("/tmp/h"),
        &env(&[("PASEO_VOICE_LOCAL_TTS_MODEL", "parakeet-tdt-0.6b-v2-int8")]),
        &persisted("{}"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("voice TTS"), "{err}");
}

#[test]
fn persisted_local_models_only_apply_when_that_feature_uses_the_local_provider() {
    let persisted = persisted(
        r#"{"features":{"dictation":{"stt":{"provider":"openai","model":"parakeet-tdt-0.6b-v3-int8"}},
            "voiceMode":{"stt":{"model":"nemotron-3.5-560ms-int8"}}}}"#,
    );
    let r = resolve(&env(&[]), &persisted);
    let models = r.speech.local.unwrap().models;
    assert_eq!(models.dictation_stt, "parakeet-tdt-0.6b-v2-int8"); // ignored: provider is openai
    assert_eq!(models.voice_stt, "nemotron-3.5-560ms-int8");
}

#[test]
fn local_config_is_omitted_when_no_feature_needs_it() {
    let all_openai = persisted(
        r#"{"features":{"dictation":{"stt":{"provider":"openai"}},
            "voiceMode":{"stt":{"provider":"openai"},"tts":{"provider":"openai"},"turnDetection":{"provider":"openai"}}}}"#,
    );
    let r = resolve(&env(&[]), &all_openai);
    assert!(r.speech.local.is_none());
    // ...but an explicit models dir brings it back
    let r = resolve(&env(&[("PASEO_LOCAL_MODELS_DIR", "/models")]), &all_openai);
    assert_eq!(r.speech.local.unwrap().models_dir, PathBuf::from("/models"));
}

// ---- dictation language (from dictation-stream-manager.test.ts) ----------------

fn dictation_language(env_pairs: &[(&str, &str)], persisted_json: &str) -> String {
    resolve(&env(env_pairs), &persisted(persisted_json))
        .speech
        .stt_languages
        .dictation
}

#[test]
fn dictation_language_resolution() {
    assert_eq!(dictation_language(&[], "{}"), "en");
    assert_eq!(
        dictation_language(&[("PASEO_DICTATION_LANGUAGE", "pt")], "{}"),
        "pt"
    );
    assert_eq!(
        dictation_language(&[("PASEO_DICTATION_LANGUAGE", "  ")], "{}"),
        "en"
    );
    let fr = r#"{"features":{"dictation":{"stt":{"language":"fr"}}}}"#;
    assert_eq!(dictation_language(&[], fr), "fr");
    assert_eq!(
        dictation_language(&[("PASEO_DICTATION_LANGUAGE", "pt")], fr),
        "pt"
    );
}

// ---- OpenAI config ------------------------------------------------------------

fn all_openai() -> RequestedSpeechProviders {
    RequestedSpeechProviders {
        dictation_stt: provider(SpeechProviderId::Openai, true, None),
        voice_turn_detection: provider(SpeechProviderId::Local, false, None),
        voice_stt: provider(SpeechProviderId::Openai, true, None),
        voice_tts: provider(SpeechProviderId::Openai, true, None),
    }
}

fn openai(
    env_pairs: &[(&str, &str)],
    persisted_json: &str,
) -> Option<paseo_speech::providers::openai::config::OpenAiSpeechProviderConfig> {
    resolve_openai_speech_config(&env(env_pairs), &persisted(persisted_json), &all_openai())
        .unwrap()
}

#[test]
fn openai_treats_empty_api_key_as_unset() {
    let local = RequestedSpeechProviders {
        dictation_stt: provider(SpeechProviderId::Local, false, None),
        voice_stt: provider(SpeechProviderId::Local, false, None),
        voice_tts: provider(SpeechProviderId::Local, false, None),
        ..all_openai()
    };
    let resolved =
        resolve_openai_speech_config(&env(&[("OPENAI_API_KEY", "")]), &persisted("{}"), &local)
            .unwrap();
    assert!(resolved.is_none());
}

#[test]
fn openai_applies_trimmed_api_key_to_stt_and_tts() {
    let r = openai(&[("OPENAI_API_KEY", "  sk-test  ")], "{}").unwrap();
    assert_eq!(r.stt.unwrap().api_key, "sk-test");
    assert_eq!(r.tts.unwrap().api_key, "sk-test");
}

#[test]
fn openai_resolves_distinct_endpoints() {
    let r = openai(
        &[],
        r#"{"providers":{"openai":{
            "stt":{"apiKey":"stt-key","baseUrl":" https://stt.example.com/v1 "},
            "tts":{"apiKey":"tts-key","baseUrl":" https://tts.example.com/v1 "}}}}"#,
    )
    .unwrap();
    let (stt, tts) = (r.stt.unwrap(), r.tts.unwrap());
    assert_eq!(
        (stt.api_key.as_str(), stt.base_url.as_deref()),
        ("stt-key", Some("https://stt.example.com/v1"))
    );
    assert_eq!(
        (tts.api_key.as_str(), tts.base_url.as_deref()),
        ("tts-key", Some("https://tts.example.com/v1"))
    );
}

#[test]
fn openai_prefers_nested_config_over_env_and_global_fallbacks() {
    let r = openai(
        &[
            ("OPENAI_API_KEY", "env-key"),
            ("OPENAI_STT_API_KEY", "stt-env-key"),
            ("OPENAI_STT_BASE_URL", "https://stt-env.example.com/v1"),
            ("OPENAI_TTS_API_KEY", "tts-env-key"),
            ("OPENAI_TTS_BASE_URL", "https://tts-env.example.com/v1"),
            ("OPENAI_BASE_URL", "https://env.example.com/v1"),
        ],
        r#"{"providers":{"openai":{
            "apiKey":"fallback-config-key","baseUrl":"https://global-config.example.com/v1",
            "stt":{"apiKey":"stt-config-key","baseUrl":" https://stt.example.com/v1 "},
            "tts":{"apiKey":"tts-config-key","baseUrl":" https://tts.example.com/v1 "}}}}"#,
    )
    .unwrap();
    let (stt, tts) = (r.stt.unwrap(), r.tts.unwrap());
    assert_eq!(stt.api_key, "stt-config-key");
    assert_eq!(stt.base_url.as_deref(), Some("https://stt.example.com/v1"));
    assert_eq!(tts.api_key, "tts-config-key");
    assert_eq!(tts.base_url.as_deref(), Some("https://tts.example.com/v1"));
}

#[test]
fn openai_uses_stt_tts_env_when_nested_config_is_unset() {
    let r = openai(
        &[
            ("OPENAI_API_KEY", "sk-test"),
            ("OPENAI_STT_API_KEY", "stt-env-key"),
            ("OPENAI_STT_BASE_URL", " https://stt-env.example.com/v1 "),
            ("OPENAI_TTS_API_KEY", "tts-env-key"),
            ("OPENAI_TTS_BASE_URL", " https://tts-env.example.com/v1 "),
            ("OPENAI_BASE_URL", "https://env.example.com/v1"),
        ],
        "{}",
    )
    .unwrap();
    let (stt, tts) = (r.stt.unwrap(), r.tts.unwrap());
    assert_eq!(stt.api_key, "stt-env-key");
    assert_eq!(
        stt.base_url.as_deref(),
        Some("https://stt-env.example.com/v1")
    );
    assert_eq!(tts.api_key, "tts-env-key");
    assert_eq!(
        tts.base_url.as_deref(),
        Some("https://tts-env.example.com/v1")
    );
}

#[test]
fn openai_falls_back_to_global_config_then_global_env() {
    let r = openai(
        &[],
        r#"{"providers":{"openai":{"apiKey":"fallback-config-key","baseUrl":" https://global-config.example.com/v1 "}}}"#,
    )
    .unwrap();
    let (stt, tts) = (r.stt.unwrap(), r.tts.unwrap());
    assert_eq!(stt.api_key, "fallback-config-key");
    assert_eq!(
        stt.base_url.as_deref(),
        Some("https://global-config.example.com/v1")
    );
    assert_eq!(tts.api_key, "fallback-config-key");
    assert_eq!(
        tts.base_url.as_deref(),
        Some("https://global-config.example.com/v1")
    );

    let r = openai(
        &[
            ("OPENAI_API_KEY", "env-key"),
            ("OPENAI_BASE_URL", " https://env.example.com/v1 "),
        ],
        "{}",
    )
    .unwrap();
    assert_eq!(
        r.stt.unwrap().base_url.as_deref(),
        Some("https://env.example.com/v1")
    );
    assert_eq!(r.tts.unwrap().api_key, "env-key");
}

#[test]
fn openai_ignores_blank_endpoint_env_vars() {
    let r = openai(
        &[
            ("OPENAI_API_KEY", "global-key"),
            ("OPENAI_STT_API_KEY", ""),
            ("OPENAI_STT_BASE_URL", "  "),
            ("OPENAI_TTS_API_KEY", ""),
            ("OPENAI_TTS_BASE_URL", ""),
        ],
        "{}",
    )
    .unwrap();
    let (stt, tts) = (r.stt.unwrap(), r.tts.unwrap());
    assert_eq!(stt.api_key, "global-key");
    assert_eq!(stt.base_url, None);
    assert_eq!(tts.api_key, "global-key");
}

#[test]
fn openai_omits_tts_when_only_an_stt_key_is_configured() {
    let r = openai(
        &[],
        r#"{"providers":{"openai":{"stt":{"apiKey":"stt-only-key"}}}}"#,
    )
    .unwrap();
    assert_eq!(r.stt.unwrap().api_key, "stt-only-key");
    assert!(r.tts.is_none());
}

#[test]
fn openai_resolves_stt_even_when_an_unused_tts_env_var_is_invalid() {
    let r = openai(
        &[
            ("TTS_VOICE", "not-a-real-voice"),
            ("TTS_MODEL", "bogus-model"),
        ],
        r#"{"providers":{"openai":{"stt":{"apiKey":"stt-only-key"}}}}"#,
    )
    .unwrap();
    assert_eq!(r.stt.unwrap().api_key, "stt-only-key");
    assert!(r.tts.is_none());
}

#[test]
fn openai_sends_model_and_voice_as_written_for_compatible_servers() {
    let r = openai(
        &[],
        r#"{"providers":{"openai":{"tts":{"apiKey":"tts-key","baseUrl":"https://tts.example.com/v1"}}},
            "features":{"voiceMode":{"tts":{"provider":"openai","model":"kokoro-v1:Q8_0","voice":"Aiden"}}}}"#,
    )
    .unwrap();
    let tts = r.tts.unwrap();
    assert_eq!(tts.model, "kokoro-v1:Q8_0");
    assert_eq!(tts.voice, "Aiden");
    assert_eq!(tts.response_format, "pcm");
}

#[test]
fn openai_accepts_gpt_4o_mini_tts_and_defaults_voice() {
    let r = openai(
        &[],
        r#"{"providers":{"openai":{"tts":{"apiKey":"tts-key"}}},
            "features":{"voiceMode":{"tts":{"provider":"openai","model":"gpt-4o-mini-tts"}}}}"#,
    )
    .unwrap();
    let tts = r.tts.unwrap();
    assert_eq!(tts.model, "gpt-4o-mini-tts");
    assert_eq!(tts.voice, "alloy");
}

#[test]
fn openai_stt_threshold_comes_from_env_then_dictation_settings() {
    let r = openai(
        &[("STT_CONFIDENCE_THRESHOLD", "-1.5")],
        r#"{"providers":{"openai":{"apiKey":"k"}},"features":{"dictation":{"stt":{"confidenceThreshold":-9}}}}"#,
    )
    .unwrap();
    assert_eq!(r.stt.unwrap().confidence_threshold, Some(-1.5));
    let r = openai(
        &[],
        r#"{"providers":{"openai":{"apiKey":"k"}},"features":{"dictation":{"stt":{"confidenceThreshold":-9}}}}"#,
    )
    .unwrap();
    assert_eq!(r.stt.unwrap().confidence_threshold, Some(-9.0));
    let err = resolve_openai_speech_config(
        &env(&[
            ("OPENAI_API_KEY", "k"),
            ("STT_CONFIDENCE_THRESHOLD", "high"),
        ]),
        &persisted("{}"),
        &all_openai(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("STT_CONFIDENCE_THRESHOLD"));
}

// ---- Soniox config ---------------------------------------------------------------

#[test]
fn soniox_config_resolves_from_env_with_defaults() {
    let resolved = resolve_soniox_config_from_env(
        &env(&[
            ("SONIOX_API_KEY", "  sx_test_key_123  "),
            ("SONIOX_LANGUAGE_HINTS", "vi, en, ja"),
        ]),
        None,
    );
    assert_eq!(
        resolved,
        SonioxSpeechProviderConfig {
            api_key: Some("sx_test_key_123".into()),
            endpoint: Some(DEFAULT_SONIOX_WS_ENDPOINT.into()),
            model: Some(DEFAULT_SONIOX_STT_MODEL.into()),
            language_hints: Some(vec!["vi".into(), "en".into(), "ja".into()]),
        }
    );
}

#[test]
fn soniox_config_falls_back_to_persisted_then_defaults_and_dedupes_env_hints() {
    let p = persisted(
        r#"{"providers":{"soniox":{"apiKey":"persisted","endpoint":"wss://x/y","model":"m","languageHints":["de"]}}}"#,
    );
    let r = resolve_soniox_config_from_env(&env(&[]), Some(&p));
    assert_eq!(r.api_key.as_deref(), Some("persisted"));
    assert_eq!(r.endpoint.as_deref(), Some("wss://x/y"));
    assert_eq!(r.model.as_deref(), Some("m"));
    assert_eq!(r.language_hints, Some(vec!["de".to_string()]));

    let r =
        resolve_soniox_config_from_env(&env(&[("SONIOX_LANGUAGE_HINTS", "EN, en ,vi,,")]), None);
    assert_eq!(
        r.language_hints,
        Some(vec!["en".to_string(), "vi".to_string()])
    );
    assert_eq!(r.api_key, None);
    let r = resolve_soniox_config_from_env(&env(&[("SONIOX_LANGUAGE_HINTS", " , ")]), None);
    assert_eq!(
        r.language_hints,
        Some(vec!["vi".to_string(), "en".to_string()])
    );
}

#[test]
fn resolve_local_speech_config_is_reachable_directly() {
    let providers = RequestedSpeechProviders::default();
    let r = resolve_local_speech_config(Path::new("/h"), &env(&[]), &persisted("{}"), &providers)
        .unwrap();
    assert!(r.local.is_some());
}
