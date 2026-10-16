//! sherpa-onnx adapters (feature `sherpa`): the Nemotron/Parakeet recognizers, Kokoro TTS and the
//! Silero VAD behind the engine traits the rest of the local provider is written against.
//!
//! Binding choice: the official `sherpa-onnx` crate (1.13.x, same C API the Node addon wraps,
//! maintained by k2-fsa, RAII wrappers for online/offline ASR, TTS and VAD, `Send + Sync`
//! handles) over `sherpa-rs` (third-party, last release 2025-10, no online recognizer streaming
//! options such as per-stream `set_option`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use sherpa_onnx::{
    GenerationConfig, OfflineRecognizer, OfflineRecognizerConfig, OfflineTts, OfflineTtsConfig,
    OnlineRecognizer, OnlineRecognizerConfig, OnlineStream, SileroVadModelConfig, VadModelConfig,
    VoiceActivityDetector,
};

use crate::error::{Result, SpeechError};
use crate::provider::{SttProvider, TtsProvider, TurnDetectionProvider};
use crate::types::RequestedSpeechProviders;

use super::config::LocalSpeechProviderConfig;
use super::lazy::LazyEngine;
use super::nemotron::{NemotronStt, StreamingEngine};
use super::parakeet::{OfflineEngine, ParakeetRealtimeStt, ParakeetStt};
use super::runtime::{
    compute_required_local_model_ids_with, should_ensure_vietnamese_dictation_model_with,
    should_use_nemotron_stt_with, ResolvedLocalModels,
};
use super::silero::ensure_silero_vad_model;
use super::text::normalize_nemotron_language;
use super::tts::{LocalTts, TtsAudio, TtsEngine};
use super::vad::{
    SpeechDetector, VadSessionConfig, VadTurnDetectionProvider, DEFAULT_BUFFER_SIZE_SECONDS,
    DEFAULT_SILERO_THRESHOLD, DEFAULT_WINDOW_SIZE, SILERO_MIN_SILENCE_DURATION,
    SILERO_MIN_SPEECH_DURATION,
};
use super::vietnamese_model::{
    resolve_installed_nemotron_model_with, vietnamese_dictation_model_directory, ProcessModelEnv,
};

fn require_file(path: &Path, label: &str) -> Result<()> {
    if path.exists() {
        Ok(())
    } else {
        Err(SpeechError::msg(format!(
            "Missing {label}: {}",
            path.display()
        )))
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---- Nemotron (online transducer) --------------------------------------------------

#[derive(Debug, Clone)]
pub struct NemotronEngineConfig {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
    pub num_threads: i32,
    pub sample_rate: u32,
    pub feature_dim: i32,
    pub default_language: Option<String>,
}

impl NemotronEngineConfig {
    pub fn from_model_dir(dir: &Path) -> Self {
        Self {
            encoder: dir.join("encoder.int8.onnx"),
            decoder: dir.join("decoder.int8.onnx"),
            joiner: dir.join("joiner.int8.onnx"),
            tokens: dir.join("tokens.txt"),
            num_threads: 4,
            sample_rate: 16000,
            feature_dim: 128,
            default_language: None,
        }
    }
}

pub struct SherpaOnlineEngine {
    recognizer: OnlineRecognizer,
    sample_rate: u32,
    default_language: String,
    /// The C library is thread-safe per object, not per recognizer shared across sessions.
    decode_lock: Mutex<()>,
}

impl SherpaOnlineEngine {
    pub fn new(config: &NemotronEngineConfig) -> Result<Self> {
        for (path, label) in [
            (&config.encoder, "Nemotron encoder"),
            (&config.decoder, "Nemotron decoder"),
            (&config.joiner, "Nemotron joiner"),
            (&config.tokens, "Nemotron tokens"),
        ] {
            require_file(path, label)?;
        }
        let mut rc = OnlineRecognizerConfig::default();
        rc.feat_config.sample_rate = config.sample_rate as i32;
        rc.feat_config.feature_dim = config.feature_dim;
        rc.model_config.transducer.encoder = Some(path_string(&config.encoder));
        rc.model_config.transducer.decoder = Some(path_string(&config.decoder));
        rc.model_config.transducer.joiner = Some(path_string(&config.joiner));
        rc.model_config.tokens = Some(path_string(&config.tokens));
        rc.model_config.num_threads = config.num_threads;
        rc.model_config.provider = Some("cpu".into());
        rc.model_config.debug = false;
        rc.decoding_method = Some("greedy_search".into());
        rc.max_active_paths = 4;
        rc.enable_endpoint = true;
        rc.rule1_min_trailing_silence = 2.4;
        rc.rule2_min_trailing_silence = 1.2;
        rc.rule3_min_utterance_length = 20.0;
        tracing::info!(encoder = %config.encoder.display(), threads = config.num_threads, "Initializing sherpa-onnx OnlineRecognizer for Nemotron 3.5 ASR");
        let recognizer = OnlineRecognizer::create(&rc).ok_or_else(|| {
            SpeechError::msg("Failed to create the sherpa-onnx OnlineRecognizer for Nemotron")
        })?;
        Ok(Self {
            recognizer,
            sample_rate: config.sample_rate,
            default_language: normalize_nemotron_language(config.default_language.as_deref()),
            decode_lock: Mutex::new(()),
        })
    }
}

impl StreamingEngine for SherpaOnlineEngine {
    type Stream = OnlineStream;

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn default_language(&self) -> String {
        self.default_language.clone()
    }
    fn create_stream(&self, language: &str) -> Result<OnlineStream> {
        let stream = self.recognizer.create_stream();
        stream.set_option("language", &normalize_nemotron_language(Some(language)));
        Ok(stream)
    }
    fn accept_waveform(&self, stream: &mut OnlineStream, samples: &[f32], sample_rate: u32) {
        stream.accept_waveform(sample_rate as i32, samples);
    }
    fn input_finished(&self, stream: &mut OnlineStream) {
        stream.input_finished();
    }
    fn decode_ready(&self, stream: &mut OnlineStream) {
        let _guard = self.decode_lock.lock().unwrap_or_else(|e| e.into_inner());
        while self.recognizer.is_ready(stream) {
            self.recognizer.decode(stream);
        }
    }
    fn is_endpoint(&self, stream: &OnlineStream) -> bool {
        self.recognizer.is_endpoint(stream)
    }
    fn reset(&self, stream: &mut OnlineStream, language: &str) {
        self.recognizer.reset(stream);
        stream.set_option("language", &normalize_nemotron_language(Some(language)));
    }
    fn result_text(&self, stream: &OnlineStream) -> String {
        self.recognizer
            .get_result(stream)
            .map(|r| r.text.trim().to_string())
            .unwrap_or_default()
    }
}

// ---- Parakeet (offline transducer) ---------------------------------------------------

#[derive(Debug, Clone)]
pub struct OfflineEngineConfig {
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
    pub num_threads: i32,
    /// Some models (zipformer-vi) emit ALL CAPS because they were trained on upper-cased text.
    pub uppercase_vocabulary: bool,
}

impl OfflineEngineConfig {
    pub fn parakeet(model_dir: &Path) -> Self {
        Self {
            encoder: model_dir.join("encoder.int8.onnx"),
            decoder: model_dir.join("decoder.int8.onnx"),
            joiner: model_dir.join("joiner.int8.onnx"),
            tokens: model_dir.join("tokens.txt"),
            num_threads: 2,
            uppercase_vocabulary: false,
        }
    }
}

pub struct SherpaOfflineEngine {
    recognizer: OfflineRecognizer,
    uppercase_vocabulary: bool,
    decode_lock: Mutex<()>,
}

impl SherpaOfflineEngine {
    pub fn new(config: &OfflineEngineConfig) -> Result<Self> {
        require_file(&config.encoder, "offline encoder")?;
        require_file(&config.decoder, "offline decoder")?;
        require_file(&config.joiner, "offline joiner")?;
        require_file(&config.tokens, "tokens")?;
        let mut rc = OfflineRecognizerConfig::default();
        rc.feat_config.sample_rate = 16000;
        rc.feat_config.feature_dim = 80;
        rc.model_config.transducer.encoder = Some(path_string(&config.encoder));
        rc.model_config.transducer.decoder = Some(path_string(&config.decoder));
        rc.model_config.transducer.joiner = Some(path_string(&config.joiner));
        rc.model_config.tokens = Some(path_string(&config.tokens));
        rc.model_config.model_type = Some("nemo_transducer".into());
        rc.model_config.num_threads = config.num_threads;
        rc.model_config.provider = Some("cpu".into());
        rc.model_config.debug = false;
        rc.decoding_method = Some("greedy_search".into());
        rc.max_active_paths = 4;
        let recognizer = OfflineRecognizer::create(&rc).ok_or_else(|| {
            SpeechError::msg("Failed to create the sherpa-onnx OfflineRecognizer")
        })?;
        tracing::info!(
            threads = config.num_threads,
            "Sherpa offline recognizer initialized"
        );
        Ok(Self {
            recognizer,
            uppercase_vocabulary: config.uppercase_vocabulary,
            decode_lock: Mutex::new(()),
        })
    }
}

impl OfflineEngine for SherpaOfflineEngine {
    fn sample_rate(&self) -> u32 {
        16000
    }
    fn normalize_text(&self, text: &str) -> String {
        if self.uppercase_vocabulary {
            text.to_lowercase()
        } else {
            text.to_string()
        }
    }
    fn recognize(&self, samples: &[f32], sample_rate: u32) -> Result<String> {
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(sample_rate as i32, samples);
        {
            let _guard = self.decode_lock.lock().unwrap_or_else(|e| e.into_inner());
            self.recognizer.decode(&stream);
        }
        Ok(stream
            .get_result()
            .map(|r| r.text.trim().to_string())
            .unwrap_or_default())
    }
}

// ---- Kokoro TTS ------------------------------------------------------------------------

pub struct SherpaKokoroEngine {
    tts: OfflineTts,
}

impl SherpaKokoroEngine {
    pub fn new(
        model_dir: &Path,
        num_threads: Option<i32>,
        length_scale: Option<f32>,
    ) -> Result<Self> {
        let model = model_dir.join("model.onnx");
        let voices = model_dir.join("voices.bin");
        let tokens = model_dir.join("tokens.txt");
        let data_dir = model_dir.join("espeak-ng-data");
        require_file(&model, "TTS model")?;
        require_file(&voices, "TTS voices")?;
        require_file(&tokens, "TTS tokens")?;
        require_file(&data_dir, "TTS espeak-ng dataDir")?;

        let mut config = OfflineTtsConfig::default();
        config.model.num_threads = num_threads.unwrap_or(2);
        config.model.provider = Some("cpu".into());
        config.model.kokoro.model = Some(path_string(&model));
        config.model.kokoro.voices = Some(path_string(&voices));
        config.model.kokoro.tokens = Some(path_string(&tokens));
        config.model.kokoro.data_dir = Some(path_string(&data_dir));
        config.model.kokoro.length_scale = length_scale.unwrap_or(1.0);
        config.max_num_sentences = 1;
        let tts = OfflineTts::create(&config).ok_or_else(|| {
            SpeechError::msg("Failed to create the sherpa-onnx OfflineTts (Kokoro)")
        })?;
        tracing::info!(model_dir = %model_dir.display(), "Sherpa offline TTS initialized");
        Ok(Self { tts })
    }
}

impl TtsEngine for SherpaKokoroEngine {
    fn sample_rate(&self) -> Option<u32> {
        let rate = self.tts.sample_rate();
        (rate > 0).then_some(rate as u32)
    }

    fn generate(&self, text: &str, speaker_id: i32, speed: f32) -> Result<TtsAudio> {
        let config = GenerationConfig {
            sid: speaker_id,
            speed,
            ..Default::default()
        };
        let audio = self
            .tts
            .generate_with_config(text, &config, None::<fn(&[f32], f32) -> bool>)
            .ok_or_else(|| SpeechError::msg("Unexpected sherpa TTS output: generation failed"))?;
        let rate = audio.sample_rate();
        Ok(TtsAudio {
            samples: audio.samples().to_vec(),
            sample_rate: (rate > 0).then_some(rate as u32),
        })
    }
}

// ---- Silero VAD -------------------------------------------------------------------------

pub struct SherpaSileroDetector {
    vad: VoiceActivityDetector,
}

impl SherpaSileroDetector {
    pub fn new(
        model_path: &Path,
        threshold: f32,
        window_size: usize,
        sample_rate: u32,
    ) -> Result<Self> {
        require_file(model_path, "Silero VAD model")?;
        let config = VadModelConfig {
            silero_vad: SileroVadModelConfig {
                model: Some(path_string(model_path)),
                threshold,
                min_silence_duration: SILERO_MIN_SILENCE_DURATION,
                min_speech_duration: SILERO_MIN_SPEECH_DURATION,
                window_size: window_size as i32,
                max_speech_duration: 20.0,
            },
            sample_rate: sample_rate as i32,
            num_threads: 1,
            provider: Some("cpu".into()),
            ..Default::default()
        };
        let vad = VoiceActivityDetector::create(&config, DEFAULT_BUFFER_SIZE_SECONDS)
            .ok_or_else(|| SpeechError::msg("Failed to create the sherpa-onnx Silero VAD"))?;
        Ok(Self { vad })
    }
}

impl SpeechDetector for SherpaSileroDetector {
    fn accept_waveform(&mut self, window: &[f32]) {
        self.vad.accept_waveform(window);
        // Only `detected()` is consulted; drop queued segments so the VAD's own buffer stays small.
        while !self.vad.is_empty() {
            self.vad.pop();
        }
    }
    fn is_detected(&self) -> bool {
        self.vad.detected()
    }
    fn flush(&mut self) {
        self.vad.flush();
        self.vad.clear();
    }
    fn reset(&mut self) {
        self.vad.reset();
    }
}

/// A turn-detection provider whose sessions use the Silero model in `models_dir`
/// (provisioned from the embedded asset on first use).
pub async fn silero_turn_detection_provider(models_dir: &Path) -> Result<VadTurnDetectionProvider> {
    let model_path = ensure_silero_vad_model(models_dir).await?;
    let config = VadSessionConfig::default();
    let (rate, window) = (config.sample_rate, DEFAULT_WINDOW_SIZE);
    Ok(VadTurnDetectionProvider::new(config, move || {
        Ok(Box::new(SherpaSileroDetector::new(
            &model_path,
            DEFAULT_SILERO_THRESHOLD,
            window,
            rate,
        )?) as Box<dyn SpeechDetector>)
    }))
}

// ---- assembly (providers/local/runtime.ts `initializeLocalSpeechServices`) ---------------------

#[derive(Default)]
pub struct LocalSpeechServices {
    pub turn_detection: Option<Arc<dyn TurnDetectionProvider>>,
    pub stt: Option<Arc<dyn SttProvider>>,
    pub dictation_stt: Option<Arc<dyn SttProvider>>,
    pub tts: Option<Arc<dyn TtsProvider>>,
    /// Always built when local config exists, so Soniox can fall back to it.
    pub local_voice_stt_provider: Option<Arc<dyn SttProvider>>,
    pub local_dictation_stt_provider: Option<Arc<dyn SttProvider>>,
    pub required_model_ids: Vec<String>,
    pub ensure_vietnamese_dictation_model: bool,
}

fn is_local(p: &crate::types::RequestedSpeechProvider) -> bool {
    p.enabled != Some(false) && p.provider == crate::types::SpeechProviderId::Local
}

/// Builds the local providers. No model is loaded here: engines load lazily when a session
/// connects (or the first synthesis runs).
pub fn initialize_local_speech_services(
    providers: &RequestedSpeechProviders,
    local: Option<&LocalSpeechProviderConfig>,
    dictation_language: &str,
    voice_language: &str,
) -> LocalSpeechServices {
    let Some(local) = local else {
        return LocalSpeechServices::default();
    };
    let env = ProcessModelEnv;
    let models_dir = local.models_dir.clone();
    let models = ResolvedLocalModels {
        dictation_stt: &local.models.dictation_stt,
        voice_stt: &local.models.voice_stt,
        voice_tts: &local.models.voice_tts,
    };
    let required_model_ids = compute_required_local_model_ids_with(
        &env,
        providers,
        &models,
        dictation_language,
        Some(&models_dir),
    );
    let ensure_vi = should_ensure_vietnamese_dictation_model_with(
        &env,
        providers,
        &models,
        dictation_language,
        Some(&models_dir),
    );

    let mut engines = Engines::new(models_dir.clone());
    let voice_provider = engines.stt_provider(
        &local.models.voice_stt,
        dictation_language,
        voice_language,
        false,
    );
    let dictation_provider = engines.stt_provider(
        &local.models.dictation_stt,
        dictation_language,
        dictation_language,
        true,
    );

    let mut services = LocalSpeechServices {
        local_voice_stt_provider: Some(voice_provider.clone()),
        local_dictation_stt_provider: Some(dictation_provider.clone()),
        required_model_ids,
        ensure_vietnamese_dictation_model: ensure_vi,
        ..Default::default()
    };
    if is_local(&providers.voice_stt) {
        services.stt = Some(voice_provider);
    }
    if is_local(&providers.dictation_stt) {
        services.dictation_stt = Some(dictation_provider);
    }
    if is_local(&providers.voice_tts) {
        services.tts = Some(engines.tts_provider(local));
    }
    if is_local(&providers.voice_turn_detection) {
        services.turn_detection = Some(Arc::new(LazyTurnDetection::new(models_dir)));
    }
    services
}

struct Engines {
    models_dir: PathBuf,
    nemotron: Option<Arc<LazyEngine<SherpaOnlineEngine>>>,
    offline: std::collections::HashMap<String, Arc<LazyEngine<SherpaOfflineEngine>>>,
}

impl Engines {
    fn new(models_dir: PathBuf) -> Self {
        Self {
            models_dir,
            nemotron: None,
            offline: Default::default(),
        }
    }

    fn nemotron_engine(&mut self) -> Arc<LazyEngine<SherpaOnlineEngine>> {
        let models_dir = self.models_dir.clone();
        self.nemotron
            .get_or_insert_with(|| {
                LazyEngine::new(move || {
                    let config = match resolve_installed_nemotron_model_with(
                        &ProcessModelEnv,
                        Some(&models_dir),
                    ) {
                        Some(found) => NemotronEngineConfig {
                            encoder: found.encoder_path,
                            decoder: found.decoder_path,
                            joiner: found.joiner_path,
                            tokens: found.tokens_path,
                            ..NemotronEngineConfig::from_model_dir(&found.model_dir)
                        },
                        None => NemotronEngineConfig::from_model_dir(
                            &vietnamese_dictation_model_directory(&models_dir),
                        ),
                    };
                    SherpaOnlineEngine::new(&config)
                })
            })
            .clone()
    }

    fn offline_engine(&mut self, model_id: &str) -> Arc<LazyEngine<SherpaOfflineEngine>> {
        let models_dir = self.models_dir.clone();
        let id = model_id.to_string();
        self.offline
            .entry(model_id.to_string())
            .or_insert_with(|| {
                LazyEngine::new(move || {
                    let dir = super::downloader::model_dir(&models_dir, &id)?;
                    SherpaOfflineEngine::new(&OfflineEngineConfig::parakeet(&dir))
                })
            })
            .clone()
    }

    fn stt_provider(
        &mut self,
        model_id: &str,
        selection_language: &str,
        default_language: &str,
        dictation: bool,
    ) -> Arc<dyn SttProvider> {
        if should_use_nemotron_stt_with(
            &ProcessModelEnv,
            model_id,
            selection_language,
            Some(&self.models_dir),
        ) {
            let default = if default_language.trim().is_empty() {
                "vi"
            } else {
                default_language
            };
            return Arc::new(NemotronStt::new(self.nemotron_engine(), Some(default)));
        }
        let engine = self.offline_engine(model_id);
        if dictation {
            Arc::new(ParakeetRealtimeStt::new(engine))
        } else {
            Arc::new(ParakeetStt::new(engine))
        }
    }

    fn tts_provider(&mut self, local: &LocalSpeechProviderConfig) -> Arc<dyn TtsProvider> {
        Arc::new(LazyTts::new(local.clone()))
    }
}

/// TTS whose Kokoro model loads on the first synthesis.
struct LazyTts {
    config: LocalSpeechProviderConfig,
    inner: tokio::sync::OnceCell<Arc<LocalTts<SherpaKokoroEngine>>>,
}

impl LazyTts {
    fn new(config: LocalSpeechProviderConfig) -> Self {
        Self {
            config,
            inner: tokio::sync::OnceCell::new(),
        }
    }
}

#[async_trait::async_trait]
impl TtsProvider for LazyTts {
    async fn synthesize_speech(&self, text: &str) -> Result<crate::provider::SpeechStreamResult> {
        let tts = self
            .inner
            .get_or_try_init(|| async {
                let config = self.config.clone();
                tokio::task::spawn_blocking(move || -> Result<Arc<LocalTts<SherpaKokoroEngine>>> {
                    let dir =
                        super::downloader::model_dir(&config.models_dir, &config.models.voice_tts)?;
                    let engine = SherpaKokoroEngine::new(&dir, None, None)?;
                    Ok(Arc::new(LocalTts::new(
                        engine,
                        config.models.voice_tts_speaker_id.map(|v| v as i32),
                        config.models.voice_tts_speed.map(|v| v as f32),
                    )))
                })
                .await
                .map_err(|e| SpeechError::msg(e.to_string()))?
            })
            .await?;
        tts.synthesize_speech(text).await
    }
}

/// Turn detection that provisions the Silero model asset when the first session is created.
struct LazyTurnDetection {
    models_dir: PathBuf,
    provider: Mutex<Option<Arc<VadTurnDetectionProvider>>>,
}

impl LazyTurnDetection {
    fn new(models_dir: PathBuf) -> Self {
        Self {
            models_dir,
            provider: Mutex::new(None),
        }
    }
}

impl TurnDetectionProvider for LazyTurnDetection {
    fn id(&self) -> &str {
        "local"
    }

    fn create_session(
        &self,
    ) -> Result<(
        Arc<dyn crate::provider::TurnDetectionSession>,
        crate::provider::TurnEventReceiver,
    )> {
        let existing = self.provider.lock().unwrap().clone();
        let provider = match existing {
            Some(p) => p,
            None => {
                // Copying the 600 KB asset is quick and happens once; keep it synchronous so
                // create_session stays a plain call.
                let model_path =
                    std::fs::create_dir_all(self.models_dir.join(super::silero::SILERO_VAD_DIR))
                        .map_err(SpeechError::from)
                        .and_then(|_| {
                            let dest = super::silero::silero_vad_model_path(&self.models_dir);
                            let usable = std::fs::metadata(&dest)
                                .map(|m| m.is_file() && m.len() > 0)
                                .unwrap_or(false);
                            if !usable {
                                std::fs::write(&dest, super::silero::SILERO_VAD_MODEL_BYTES)?;
                            }
                            Ok(dest)
                        })?;
                let config = VadSessionConfig::default();
                let (rate, window) = (config.sample_rate, DEFAULT_WINDOW_SIZE);
                let p = Arc::new(VadTurnDetectionProvider::new(config, move || {
                    Ok(Box::new(SherpaSileroDetector::new(
                        &model_path,
                        DEFAULT_SILERO_THRESHOLD,
                        window,
                        rate,
                    )?) as Box<dyn SpeechDetector>)
                }));
                *self.provider.lock().unwrap() = Some(p.clone());
                p
            }
        };
        provider.create_session()
    }
}
