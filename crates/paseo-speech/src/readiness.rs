//! Speech readiness (port of the readiness builders in `speech-runtime.ts`).
//!
//! The TypeScript runtime also owns a reconcile/download monitor loop; that orchestration lives in
//! the daemon. This module is the pure part: given what is configured, what resolved and what is
//! still downloading, say whether voice features can be used and why not.

use serde::Serialize;

use crate::types::RequestedSpeechProviders;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechReadinessReasonCode {
    Ready,
    Disabled,
    ModelDownloadInProgress,
    ModelsMissing,
    ModelDownloadFailed,
    TurnDetectionUnavailable,
    SttUnavailable,
    TtsUnavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechReadinessState {
    pub enabled: bool,
    pub available: bool,
    pub reason_code: SpeechReadinessReasonCode,
    pub message: String,
    pub retryable: bool,
    pub missing_model_ids: Vec<String>,
}

impl SpeechReadinessState {
    fn disabled(message: &str) -> Self {
        Self {
            enabled: false,
            available: false,
            reason_code: SpeechReadinessReasonCode::Disabled,
            message: message.to_string(),
            retryable: false,
            missing_model_ids: Vec::new(),
        }
    }

    fn unavailable(reason_code: SpeechReadinessReasonCode, message: &str) -> Self {
        Self {
            enabled: true,
            available: false,
            reason_code,
            message: message.to_string(),
            retryable: false,
            missing_model_ids: Vec::new(),
        }
    }

    fn ready(message: &str) -> Self {
        Self {
            enabled: true,
            available: true,
            reason_code: SpeechReadinessReasonCode::Ready,
            message: message.to_string(),
            retryable: false,
            missing_model_ids: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadState {
    pub in_progress: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeechReadinessSnapshot {
    /// RFC 3339.
    pub generated_at: String,
    pub required_local_model_ids: Vec<String>,
    pub missing_local_model_ids: Vec<String>,
    pub download: DownloadState,
    pub realtime_voice: SpeechReadinessState,
    pub dictation: SpeechReadinessState,
    pub voice_feature: SpeechReadinessState,
}

/// What the runtime knows right now.
#[derive(Debug, Clone)]
pub struct ReadinessInputs {
    pub providers: RequestedSpeechProviders,
    pub turn_detection_available: bool,
    pub stt_available: bool,
    pub tts_available: bool,
    pub dictation_stt_available: bool,
    pub required_local_model_ids: Vec<String>,
    pub missing_local_model_ids: Vec<String>,
    pub missing_vietnamese_dictation_model: bool,
    /// Soniox with an API key does not need the local Vietnamese model.
    pub soniox_configured: bool,
    pub download_in_progress: bool,
    pub download_error: Option<String>,
}

fn join_model_ids(ids: &[String]) -> String {
    if ids.is_empty() {
        "none".to_string()
    } else {
        ids.join(", ")
    }
}

pub fn build_realtime_voice_readiness(
    providers: &RequestedSpeechProviders,
    turn_detection_available: bool,
    stt_available: bool,
    tts_available: bool,
) -> SpeechReadinessState {
    let turn = providers.voice_turn_detection.is_enabled();
    let stt = providers.voice_stt.is_enabled();
    let tts = providers.voice_tts.is_enabled();
    if !(turn || stt || tts) {
        return SpeechReadinessState::disabled("Realtime voice is disabled in daemon config.");
    }
    if turn && !turn_detection_available {
        return SpeechReadinessState::unavailable(
            SpeechReadinessReasonCode::TurnDetectionUnavailable,
            "Realtime voice is unavailable: turn-detection service is not ready.",
        );
    }
    if stt && !stt_available {
        return SpeechReadinessState::unavailable(
            SpeechReadinessReasonCode::SttUnavailable,
            "Realtime voice is unavailable: speech-to-text service is not ready.",
        );
    }
    if tts && !tts_available {
        return SpeechReadinessState::unavailable(
            SpeechReadinessReasonCode::TtsUnavailable,
            "Realtime voice is unavailable: text-to-speech service is not ready.",
        );
    }
    SpeechReadinessState::ready("Realtime voice is ready.")
}

pub fn build_dictation_readiness(
    providers: &RequestedSpeechProviders,
    dictation_stt_available: bool,
    missing_vietnamese_dictation_model: bool,
    download_in_progress: bool,
    download_error: Option<&str>,
) -> SpeechReadinessState {
    if !providers.dictation_stt.is_enabled() {
        return SpeechReadinessState::disabled("Dictation is disabled in daemon config.");
    }
    if missing_vietnamese_dictation_model {
        let (reason_code, message) = if download_in_progress {
            (
                SpeechReadinessReasonCode::ModelDownloadInProgress,
                "Vietnamese dictation is unavailable while its local speech model downloads."
                    .to_string(),
            )
        } else if let Some(error) = download_error {
            (
                SpeechReadinessReasonCode::ModelDownloadFailed,
                format!("Vietnamese dictation model download failed ({error})."),
            )
        } else {
            (
                SpeechReadinessReasonCode::ModelsMissing,
                "Vietnamese dictation is unavailable: its local speech model is missing."
                    .to_string(),
            )
        };
        return SpeechReadinessState {
            enabled: true,
            available: false,
            reason_code,
            message,
            retryable: download_error.is_none(),
            missing_model_ids: Vec::new(),
        };
    }
    if !dictation_stt_available {
        return SpeechReadinessState::unavailable(
            SpeechReadinessReasonCode::SttUnavailable,
            "Dictation is unavailable: speech-to-text service is not ready.",
        );
    }
    SpeechReadinessState::ready("Dictation is ready.")
}

pub fn build_voice_feature_readiness(
    realtime_voice: &SpeechReadinessState,
    dictation: &SpeechReadinessState,
    missing_local_model_ids: &[String],
    missing_vietnamese_dictation_model: bool,
    download_in_progress: bool,
    download_error: Option<&str>,
) -> SpeechReadinessState {
    if !(realtime_voice.enabled || dictation.enabled) {
        return SpeechReadinessState::disabled("Voice features are disabled in daemon config.");
    }
    if !missing_local_model_ids.is_empty() || missing_vietnamese_dictation_model {
        let missing = missing_local_model_ids.to_vec();
        if download_in_progress {
            return SpeechReadinessState {
                enabled: true,
                available: false,
                reason_code: SpeechReadinessReasonCode::ModelDownloadInProgress,
                message: if missing_vietnamese_dictation_model {
                    "Voice features are unavailable while local speech models download in the background."
                        .to_string()
                } else {
                    format!(
                        "Voice features are unavailable while models download in the background ({}).",
                        join_model_ids(&missing)
                    )
                },
                retryable: true,
                missing_model_ids: missing,
            };
        }
        if let Some(error) = download_error {
            return SpeechReadinessState {
                enabled: true,
                available: false,
                reason_code: SpeechReadinessReasonCode::ModelDownloadFailed,
                message: format!(
                    "Voice features are unavailable: model download failed ({error})."
                ),
                retryable: false,
                missing_model_ids: missing,
            };
        }
        return SpeechReadinessState {
            enabled: true,
            available: false,
            reason_code: SpeechReadinessReasonCode::ModelsMissing,
            message: if missing_vietnamese_dictation_model {
                "Voice features are unavailable: the Vietnamese local speech model is missing."
                    .to_string()
            } else {
                format!(
                    "Voice features are unavailable: missing local models ({}).",
                    join_model_ids(&missing)
                )
            },
            retryable: true,
            missing_model_ids: missing,
        };
    }
    SpeechReadinessState::ready("Voice features are ready.")
}

pub fn compute_readiness_snapshot(inputs: &ReadinessInputs) -> SpeechReadinessSnapshot {
    let effective_missing_vietnamese = inputs.missing_vietnamese_dictation_model
        && !(inputs.providers.dictation_stt.provider == crate::types::SpeechProviderId::Soniox
            && inputs.soniox_configured);
    let realtime_voice = build_realtime_voice_readiness(
        &inputs.providers,
        inputs.turn_detection_available,
        inputs.stt_available,
        inputs.tts_available,
    );
    let dictation = build_dictation_readiness(
        &inputs.providers,
        inputs.dictation_stt_available,
        effective_missing_vietnamese,
        inputs.download_in_progress,
        inputs.download_error.as_deref(),
    );
    let voice_feature = build_voice_feature_readiness(
        &realtime_voice,
        &dictation,
        &inputs.missing_local_model_ids,
        effective_missing_vietnamese,
        inputs.download_in_progress,
        inputs.download_error.as_deref(),
    );
    SpeechReadinessSnapshot {
        generated_at: jiff::Timestamp::now().to_string(),
        required_local_model_ids: inputs.required_local_model_ids.clone(),
        missing_local_model_ids: inputs.missing_local_model_ids.clone(),
        download: DownloadState {
            in_progress: inputs.download_in_progress,
            error: inputs.download_error.clone(),
        },
        realtime_voice,
        dictation,
        voice_feature,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RequestedSpeechProvider, SpeechProviderId};

    fn local(enabled: bool) -> RequestedSpeechProvider {
        RequestedSpeechProvider {
            provider: SpeechProviderId::Local,
            explicit: true,
            enabled: Some(enabled),
        }
    }

    fn inputs() -> ReadinessInputs {
        ReadinessInputs {
            providers: RequestedSpeechProviders::default(),
            turn_detection_available: true,
            stt_available: true,
            tts_available: true,
            dictation_stt_available: true,
            required_local_model_ids: vec![],
            missing_local_model_ids: vec![],
            missing_vietnamese_dictation_model: false,
            soniox_configured: false,
            download_in_progress: false,
            download_error: None,
        }
    }

    #[test]
    fn voice_feature_stays_available_when_only_dictation_is_enabled_and_ready() {
        let mut i = inputs();
        i.providers.voice_turn_detection = local(false);
        i.providers.voice_stt = local(false);
        i.providers.voice_tts = local(false);
        i.turn_detection_available = false;
        i.stt_available = false;
        i.tts_available = false;
        let s = compute_readiness_snapshot(&i);
        assert!(s.dictation.available);
        assert_eq!(
            s.realtime_voice.reason_code,
            SpeechReadinessReasonCode::Disabled
        );
        assert!(s.voice_feature.available);
        assert_eq!(
            s.voice_feature.reason_code,
            SpeechReadinessReasonCode::Ready
        );
    }

    #[test]
    fn voice_feature_stays_available_when_only_realtime_voice_is_enabled_and_ready() {
        let mut i = inputs();
        i.providers.dictation_stt = local(false);
        i.dictation_stt_available = false;
        let s = compute_readiness_snapshot(&i);
        assert!(s.realtime_voice.available);
        assert_eq!(s.dictation.reason_code, SpeechReadinessReasonCode::Disabled);
        assert!(s.voice_feature.available);
        assert_eq!(
            s.voice_feature.reason_code,
            SpeechReadinessReasonCode::Ready
        );
    }

    #[test]
    fn everything_disabled_reports_disabled_everywhere() {
        let mut i = inputs();
        i.providers.dictation_stt = local(false);
        i.providers.voice_turn_detection = local(false);
        i.providers.voice_stt = local(false);
        i.providers.voice_tts = local(false);
        let s = compute_readiness_snapshot(&i);
        for state in [&s.realtime_voice, &s.dictation, &s.voice_feature] {
            assert!(!state.enabled && !state.available);
            assert_eq!(state.reason_code, SpeechReadinessReasonCode::Disabled);
        }
        assert_eq!(
            s.voice_feature.message,
            "Voice features are disabled in daemon config."
        );
    }

    #[test]
    fn realtime_voice_names_the_first_missing_service() {
        let mut i = inputs();
        i.turn_detection_available = false;
        i.stt_available = false;
        let s = compute_readiness_snapshot(&i);
        assert_eq!(
            s.realtime_voice.reason_code,
            SpeechReadinessReasonCode::TurnDetectionUnavailable
        );
        i.turn_detection_available = true;
        assert_eq!(
            compute_readiness_snapshot(&i).realtime_voice.reason_code,
            SpeechReadinessReasonCode::SttUnavailable
        );
        i.stt_available = true;
        i.tts_available = false;
        assert_eq!(
            compute_readiness_snapshot(&i).realtime_voice.reason_code,
            SpeechReadinessReasonCode::TtsUnavailable
        );
    }

    #[test]
    fn dictation_without_a_service_is_stt_unavailable() {
        let mut i = inputs();
        i.dictation_stt_available = false;
        let s = compute_readiness_snapshot(&i);
        assert_eq!(
            s.dictation.reason_code,
            SpeechReadinessReasonCode::SttUnavailable
        );
        assert!(!s.dictation.retryable);
    }

    #[test]
    fn missing_models_report_missing_then_in_progress_then_failed() {
        let mut i = inputs();
        i.missing_local_model_ids =
            vec!["kokoro-en-v0_19".into(), "parakeet-tdt-0.6b-v2-int8".into()];
        let s = compute_readiness_snapshot(&i);
        assert_eq!(
            s.voice_feature.reason_code,
            SpeechReadinessReasonCode::ModelsMissing
        );
        assert!(s.voice_feature.retryable);
        assert_eq!(
            s.voice_feature.message,
            "Voice features are unavailable: missing local models (kokoro-en-v0_19, parakeet-tdt-0.6b-v2-int8)."
        );
        assert_eq!(s.voice_feature.missing_model_ids.len(), 2);

        i.download_in_progress = true;
        let s = compute_readiness_snapshot(&i);
        assert_eq!(
            s.voice_feature.reason_code,
            SpeechReadinessReasonCode::ModelDownloadInProgress
        );
        assert!(s
            .voice_feature
            .message
            .contains("download in the background (kokoro-en-v0_19"));

        i.download_in_progress = false;
        i.download_error = Some("HTTP 503".into());
        let s = compute_readiness_snapshot(&i);
        assert_eq!(
            s.voice_feature.reason_code,
            SpeechReadinessReasonCode::ModelDownloadFailed
        );
        assert!(!s.voice_feature.retryable);
        assert_eq!(
            s.voice_feature.message,
            "Voice features are unavailable: model download failed (HTTP 503)."
        );
    }

    #[test]
    fn vietnamese_model_states_and_the_soniox_exemption() {
        let mut i = inputs();
        i.missing_vietnamese_dictation_model = true;
        let s = compute_readiness_snapshot(&i);
        assert_eq!(
            s.dictation.reason_code,
            SpeechReadinessReasonCode::ModelsMissing
        );
        assert!(s.dictation.retryable);
        assert_eq!(
            s.voice_feature.message,
            "Voice features are unavailable: the Vietnamese local speech model is missing."
        );

        i.download_in_progress = true;
        let s = compute_readiness_snapshot(&i);
        assert_eq!(
            s.dictation.reason_code,
            SpeechReadinessReasonCode::ModelDownloadInProgress
        );

        i.download_in_progress = false;
        i.download_error = Some("boom".into());
        let s = compute_readiness_snapshot(&i);
        assert_eq!(
            s.dictation.reason_code,
            SpeechReadinessReasonCode::ModelDownloadFailed
        );
        assert!(!s.dictation.retryable);
        assert_eq!(
            s.dictation.message,
            "Vietnamese dictation model download failed (boom)."
        );

        // Soniox with a key does not need the local model for dictation.
        let mut i = inputs();
        i.missing_vietnamese_dictation_model = true;
        i.providers.dictation_stt.provider = SpeechProviderId::Soniox;
        i.soniox_configured = true;
        let s = compute_readiness_snapshot(&i);
        assert!(s.dictation.available);
        assert!(s.voice_feature.available);
    }

    #[test]
    fn snapshot_serializes_with_camel_case_keys_and_snake_case_reason() {
        let mut i = inputs();
        i.dictation_stt_available = false;
        let json = serde_json::to_value(compute_readiness_snapshot(&i)).unwrap();
        assert_eq!(json["dictation"]["reasonCode"], "stt_unavailable");
        assert_eq!(json["dictation"]["missingModelIds"], serde_json::json!([]));
        assert_eq!(json["download"]["inProgress"], false);
        assert!(json["generatedAt"].is_string());
        assert!(json["realtimeVoice"]["available"].as_bool().unwrap());
    }
}
