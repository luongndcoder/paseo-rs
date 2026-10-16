//! Local provider selection rules (port of the pure parts of `providers/local/runtime.ts`).

use std::path::Path;

use crate::types::{RequestedSpeechProvider, RequestedSpeechProviders, SpeechProviderId};

use super::catalog::NEMOTRON_MODEL_ID;
use super::vietnamese_model::{resolve_installed_nemotron_model_with, ModelEnv, ProcessModelEnv};

fn local_or_soniox(provider: &RequestedSpeechProvider) -> bool {
    provider.enabled != Some(false)
        && matches!(
            provider.provider,
            SpeechProviderId::Local | SpeechProviderId::Soniox
        )
}

fn is_local(provider: &RequestedSpeechProvider) -> bool {
    provider.enabled != Some(false) && provider.provider == SpeechProviderId::Local
}

/// Test fixtures use throwaway models dirs; a Nemotron model installed on the developer's machine
/// must not change which models those tests ask for.
pub fn is_temp_test_models_dir(models_dir: Option<&Path>) -> bool {
    let Some(dir) = models_dir else { return false };
    let normalized = dir.to_string_lossy();
    let tmp = std::env::temp_dir();
    dir.starts_with(&tmp)
        || normalized.starts_with("/tmp/")
        || normalized.starts_with("/private/tmp/")
        || normalized.contains("paseo-missing-models-")
        || normalized.contains("missing-local-speech-models")
}

fn language_is_vietnamese(language: &str) -> bool {
    let lower = language.to_ascii_lowercase();
    lower == "vi" || lower.starts_with("vi-")
}

/// Whether STT for `model_id`/`language` runs on the Nemotron streaming engine instead of an
/// offline catalog model.
pub fn should_use_nemotron_stt_with(
    env: &dyn ModelEnv,
    model_id: &str,
    language: &str,
    models_dir: Option<&Path>,
) -> bool {
    if model_id == NEMOTRON_MODEL_ID {
        return true;
    }
    if language_is_vietnamese(language) {
        return true;
    }
    !is_temp_test_models_dir(models_dir)
        && resolve_installed_nemotron_model_with(env, models_dir).is_some()
}

pub fn should_use_nemotron_stt(model_id: &str, language: &str, models_dir: Option<&Path>) -> bool {
    should_use_nemotron_stt_with(&ProcessModelEnv, model_id, language, models_dir)
}

#[derive(Debug, Clone)]
pub struct ResolvedLocalModels<'a> {
    pub dictation_stt: &'a str,
    pub voice_stt: &'a str,
    pub voice_tts: &'a str,
}

/// Catalog models that must exist on disk for the enabled features (Nemotron is handled separately).
pub fn compute_required_local_model_ids_with(
    env: &dyn ModelEnv,
    providers: &RequestedSpeechProviders,
    models: &ResolvedLocalModels<'_>,
    language: &str,
    models_dir: Option<&Path>,
) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    let mut add = |id: &str| {
        if !ids.iter().any(|existing| existing == id) {
            ids.push(id.to_string());
        }
    };
    if local_or_soniox(&providers.dictation_stt)
        && !should_use_nemotron_stt_with(env, models.dictation_stt, language, models_dir)
    {
        add(models.dictation_stt);
    }
    if local_or_soniox(&providers.voice_stt)
        && !should_use_nemotron_stt_with(env, models.voice_stt, language, models_dir)
    {
        add(models.voice_stt);
    }
    if is_local(&providers.voice_tts) {
        add(models.voice_tts);
    }
    ids
}

pub fn should_ensure_vietnamese_dictation_model_with(
    env: &dyn ModelEnv,
    providers: &RequestedSpeechProviders,
    models: &ResolvedLocalModels<'_>,
    language: &str,
    models_dir: Option<&Path>,
) -> bool {
    (local_or_soniox(&providers.dictation_stt)
        && should_use_nemotron_stt_with(env, models.dictation_stt, language, models_dir))
        || (local_or_soniox(&providers.voice_stt)
            && should_use_nemotron_stt_with(env, models.voice_stt, language, models_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    struct Env {
        vars: HashMap<String, String>,
        home: PathBuf,
    }
    impl ModelEnv for Env {
        fn var(&self, key: &str) -> Option<String> {
            self.vars.get(key).cloned()
        }
        fn home_dir(&self) -> Option<PathBuf> {
            Some(self.home.clone())
        }
    }

    fn empty_env() -> Env {
        Env {
            vars: HashMap::new(),
            home: PathBuf::from("/nonexistent-home-for-paseo-tests"),
        }
    }

    fn installed_env(dir: &Path) -> Env {
        std::fs::create_dir_all(dir).unwrap();
        for f in [
            "encoder.int8.onnx",
            "decoder.int8.onnx",
            "joiner.int8.onnx",
            "tokens.txt",
        ] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        Env {
            vars: HashMap::from([(
                "PASEO_NEMOTRON_MODEL_DIR".to_string(),
                dir.to_string_lossy().into_owned(),
            )]),
            home: PathBuf::from("/nonexistent-home-for-paseo-tests"),
        }
    }

    fn models() -> ResolvedLocalModels<'static> {
        ResolvedLocalModels {
            dictation_stt: "parakeet-tdt-0.6b-v2-int8",
            voice_stt: "parakeet-tdt-0.6b-v2-int8",
            voice_tts: "kokoro-en-v0_19",
        }
    }

    #[test]
    fn nemotron_is_chosen_by_model_id_or_vietnamese_language() {
        let env = empty_env();
        let dir = Some(Path::new("/var/models"));
        assert!(should_use_nemotron_stt_with(
            &env,
            NEMOTRON_MODEL_ID,
            "en",
            dir
        ));
        assert!(should_use_nemotron_stt_with(
            &env,
            "parakeet-tdt-0.6b-v2-int8",
            "vi",
            dir
        ));
        assert!(should_use_nemotron_stt_with(
            &env,
            "parakeet-tdt-0.6b-v2-int8",
            "VI-VN",
            dir
        ));
        assert!(!should_use_nemotron_stt_with(
            &env,
            "parakeet-tdt-0.6b-v2-int8",
            "en",
            dir
        ));
        assert!(!should_use_nemotron_stt_with(
            &env,
            "parakeet-tdt-0.6b-v2-int8",
            "vie",
            dir
        ));
    }

    #[test]
    fn an_installed_nemotron_wins_except_for_temp_test_dirs() {
        let root = tempfile::tempdir().unwrap();
        let env = installed_env(&root.path().join("nemotron"));
        assert!(should_use_nemotron_stt_with(
            &env,
            "parakeet-tdt-0.6b-v2-int8",
            "en",
            Some(Path::new("/var/models"))
        ));
        assert!(!should_use_nemotron_stt_with(
            &env,
            "parakeet-tdt-0.6b-v2-int8",
            "en",
            Some(Path::new("/tmp/missing-local-speech-models"))
        ));
        assert!(is_temp_test_models_dir(Some(
            &std::env::temp_dir().join("x")
        )));
        assert!(is_temp_test_models_dir(Some(Path::new(
            "/srv/paseo-missing-models-1"
        ))));
        assert!(!is_temp_test_models_dir(None));
    }

    #[test]
    fn required_model_ids_follow_enabled_local_features() {
        let env = empty_env();
        let dir = Some(Path::new("/var/models"));
        let mut providers = RequestedSpeechProviders::default();
        assert_eq!(
            compute_required_local_model_ids_with(&env, &providers, &models(), "en", dir),
            vec!["parakeet-tdt-0.6b-v2-int8", "kokoro-en-v0_19"]
        );
        // Vietnamese: STT moves to Nemotron (handled separately), TTS still needs kokoro
        assert_eq!(
            compute_required_local_model_ids_with(&env, &providers, &models(), "vi", dir),
            vec!["kokoro-en-v0_19"]
        );
        // soniox counts as local (it falls back to local STT)
        providers.dictation_stt.provider = SpeechProviderId::Soniox;
        providers.voice_stt.enabled = Some(false);
        providers.voice_tts.provider = SpeechProviderId::Openai;
        assert_eq!(
            compute_required_local_model_ids_with(&env, &providers, &models(), "en", dir),
            vec!["parakeet-tdt-0.6b-v2-int8"]
        );
        providers.dictation_stt.enabled = Some(false);
        assert!(
            compute_required_local_model_ids_with(&env, &providers, &models(), "en", dir)
                .is_empty()
        );
    }

    #[test]
    fn vietnamese_model_is_ensured_when_any_stt_feature_uses_nemotron() {
        let env = empty_env();
        let dir = Some(Path::new("/var/models"));
        let providers = RequestedSpeechProviders::default();
        assert!(!should_ensure_vietnamese_dictation_model_with(
            &env,
            &providers,
            &models(),
            "en",
            dir
        ));
        assert!(should_ensure_vietnamese_dictation_model_with(
            &env,
            &providers,
            &models(),
            "vi",
            dir
        ));
        let mut off = providers;
        off.dictation_stt.enabled = Some(false);
        off.voice_stt.enabled = Some(false);
        assert!(!should_ensure_vietnamese_dictation_model_with(
            &env,
            &off,
            &models(),
            "vi",
            dir
        ));
    }
}
