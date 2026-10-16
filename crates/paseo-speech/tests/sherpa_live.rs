//! Real sherpa-onnx flows (feature `sherpa`). The VAD test uses the embedded Silero asset and runs
//! everywhere; the model-backed tests are `#[ignore]`d and skip themselves when the models are not
//! on disk:
//!
//! ```text
//! cargo test --features sherpa --test sherpa_live -- --ignored --test-threads=1
//! ```
//!
//! Model lookup: Nemotron via `PASEO_NEMOTRON_MODEL_DIR` / `~/.voicescribe/models/...` (same as the
//! daemon); Parakeet and Kokoro under `PASEO_LOCAL_MODELS_DIR`, defaulting to the dev home of the
//! sibling TypeScript checkout (`../../../paseo-vietnamese-voice/.dev/paseo-home/models/local-speech`).
//! Nothing is downloaded.

#![cfg(feature = "sherpa")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use paseo_speech::audio::parse_pcm16_mono_wav;
use paseo_speech::dictation::{
    DictationOutbound, DictationStreamManager, DictationStreamManagerParams,
};
use paseo_speech::dictation_debug::DictationDebug;
use paseo_speech::provider::{
    SttEvent, SttEventReceiver, SttProvider, SttSessionParams, TtsProvider, TurnDetectionProvider,
    TurnEvent,
};
use paseo_speech::providers::local::downloader::model_dir;
use paseo_speech::providers::local::lazy::LazyEngine;
use paseo_speech::providers::local::nemotron::NemotronStt;
use paseo_speech::providers::local::parakeet::{ParakeetRealtimeStt, ParakeetStt};
use paseo_speech::providers::local::sherpa::{
    silero_turn_detection_provider, NemotronEngineConfig, OfflineEngineConfig, SherpaKokoroEngine,
    SherpaOfflineEngine, SherpaOnlineEngine,
};
use paseo_speech::providers::local::tts::LocalTts;
use paseo_speech::providers::local::vietnamese_model::resolve_installed_nemotron_model;
use tokio::sync::mpsc;

fn nemotron_dir() -> Option<PathBuf> {
    resolve_installed_nemotron_model(None).map(|m| m.model_dir)
}

fn local_models_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("PASEO_LOCAL_MODELS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../paseo-vietnamese-voice/.dev/paseo-home/models/local-speech")
        });
    dir.is_dir().then_some(dir)
}

fn read_wav(path: &std::path::Path) -> Option<(u32, Vec<u8>)> {
    let bytes = std::fs::read(path).ok()?;
    let parsed = parse_pcm16_mono_wav(&bytes).ok()?;
    Some((parsed.sample_rate, parsed.pcm16))
}

fn vi_wav() -> Option<Vec<u8>> {
    Some(read_wav(&nemotron_dir()?.join("test_wavs/vi.wav"))?.1)
}

async fn next(rx: &mut SttEventReceiver) -> SttEvent {
    tokio::time::timeout(Duration::from_secs(60), rx.recv())
        .await
        .expect("event in time")
        .expect("open")
}

// ---- VAD (runs everywhere) ------------------------------------------------------------

#[tokio::test]
async fn silero_vad_loads_the_embedded_model_and_stays_quiet_on_silence() {
    let models = tempfile::tempdir().unwrap();
    let provider = silero_turn_detection_provider(models.path()).await.unwrap();
    assert!(models.path().join("silero-vad/silero_vad.onnx").exists());
    let (session, mut rx) = provider.create_session().unwrap();
    assert_eq!(session.required_sample_rate(), 16000);
    session.connect();
    for _ in 0..15 {
        session.append_pcm16(&vec![0u8; 3200 * 2]); // 200 ms of silence
    }
    session.flush();
    assert!(
        rx.try_recv().is_err(),
        "silence must not produce turn events"
    );
    session.close();
}

#[tokio::test]
async fn silero_vad_detects_speech_start_and_stop_on_a_real_utterance() {
    let Some(speech) = vi_wav() else {
        eprintln!("skipping: Nemotron test wav not installed");
        return;
    };
    let models = tempfile::tempdir().unwrap();
    let provider = silero_turn_detection_provider(models.path()).await.unwrap();
    let (session, mut rx) = provider.create_session().unwrap();
    session.connect();
    for chunk in speech.chunks(3200 * 2) {
        session.append_pcm16(chunk);
    }
    // trailing silence long enough for the 1 s end-of-speech rule
    for _ in 0..10 {
        session.append_pcm16(&vec![0u8; 3200 * 2]);
    }
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e.unwrap());
    }
    assert_eq!(
        events,
        vec![TurnEvent::SpeechStarted, TurnEvent::SpeechStopped],
        "{events:?}"
    );
}

// ---- model-backed flows ------------------------------------------------------------------

fn nemotron_engine() -> Option<Arc<LazyEngine<SherpaOnlineEngine>>> {
    let dir = nemotron_dir()?;
    Some(LazyEngine::new(move || {
        SherpaOnlineEngine::new(&NemotronEngineConfig::from_model_dir(&dir))
    }))
}

#[tokio::test]
#[ignore = "loads the 600 MB Nemotron model"]
async fn nemotron_transcribes_vietnamese_single_shot_and_streaming() {
    let (Some(engine), Some(dir)) = (nemotron_engine(), nemotron_dir()) else {
        eprintln!("skipping: Nemotron model not installed");
        return;
    };
    let wav_bytes = std::fs::read(dir.join("test_wavs/vi.wav")).unwrap();
    let stt = NemotronStt::new(engine, Some("vi"));

    let single = stt.transcribe_audio(&wav_bytes, "audio/wav").await.unwrap();
    assert!(
        single.text.to_lowercase().contains("đất nước"),
        "{single:?}"
    );

    let pcm = parse_pcm16_mono_wav(&wav_bytes).unwrap().pcm16;
    let (session, mut rx) = stt
        .create_session(&SttSessionParams {
            language: Some("vi".into()),
            prompt: None,
        })
        .unwrap();
    session.connect().await.unwrap();
    for chunk in pcm.chunks(3200 * 2) {
        session.append_pcm16(chunk);
    }
    session.commit().unwrap();

    let (mut partials, mut committed, mut finals) = (0, 0, Vec::new());
    while finals.is_empty() {
        match next(&mut rx).await {
            SttEvent::Transcript(t) if t.is_final => finals.push(t.transcript),
            SttEvent::Transcript(_) => partials += 1,
            SttEvent::Committed(_) => committed += 1,
            SttEvent::Error(e) => panic!("{e}"),
        }
    }
    session.close().unwrap();
    assert!(partials > 0);
    assert_eq!(committed, 1);
    assert!(finals[0].to_lowercase().contains("đất nước"), "{finals:?}");
}

#[tokio::test]
#[ignore = "loads the 600 MB Nemotron model"]
async fn dictation_manager_end_to_end_with_the_local_nemotron_provider() {
    let (Some(engine), Some(wav)) = (nemotron_engine(), vi_wav()) else {
        eprintln!("skipping: Nemotron model not installed");
        return;
    };
    let provider: Arc<dyn SttProvider> = Arc::new(NemotronStt::new(engine, Some("vi")));
    let (tx, mut out) = mpsc::unbounded_channel();
    let mut params = DictationStreamManagerParams::new(tx, "live", provider);
    params.language = Some("vi".into());
    params.debug = DictationDebug::Disabled;
    params.final_timeout_ms = Some(60_000);
    let manager = DictationStreamManager::new(params);

    let format = "audio/pcm;rate=16000;bits=16";
    manager.handle_start("d", format).await;
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut seq = 0i64;
    for chunk in wav.chunks(3200 * 2) {
        manager.handle_chunk("d", seq, &b64.encode(chunk), format);
        seq += 1;
    }
    manager.handle_finish("d", seq - 1);

    let mut final_text = None;
    let mut saw_partial = false;
    while final_text.is_none() {
        let message = tokio::time::timeout(Duration::from_secs(60), out.recv())
            .await
            .expect("message")
            .expect("open");
        match message {
            DictationOutbound::DictationStreamPartial(_) => saw_partial = true,
            DictationOutbound::DictationStreamFinal(f) => final_text = Some(f.text),
            DictationOutbound::DictationStreamError(e) => panic!("{e:?}"),
            _ => {}
        }
    }
    assert!(saw_partial);
    assert!(final_text.unwrap().to_lowercase().contains("đất nước"));
    assert!(!manager.has_demand());
}

#[tokio::test]
#[ignore = "loads the 600 MB Parakeet model"]
async fn parakeet_transcribes_english_in_batch_and_realtime_modes() {
    let Some(models) = local_models_dir() else {
        eprintln!("skipping: no local models dir");
        return;
    };
    let Ok(dir) = model_dir(&models, "parakeet-tdt-0.6b-v2-int8") else {
        return;
    };
    let Some((rate, pcm)) = read_wav(&dir.join("test_wavs/0.wav")) else {
        eprintln!("skipping: Parakeet model/test wav not installed");
        return;
    };
    assert_eq!(rate, 16000);
    let engine =
        LazyEngine::new(move || SherpaOfflineEngine::new(&OfflineEngineConfig::parakeet(&dir)));

    let batch = ParakeetStt::new(engine.clone());
    let single = batch
        .transcribe_audio(&pcm, "audio/pcm;rate=16000")
        .await
        .unwrap();
    assert!(single.text.to_lowercase().contains("phebe"), "{single:?}");

    // realtime (dictation) flavour: capitalised first segment, partials, final
    let realtime = ParakeetRealtimeStt::new(engine);
    let (session, mut rx) = realtime
        .create_session(&SttSessionParams::default())
        .unwrap();
    session.connect().await.unwrap();
    for chunk in pcm.chunks(3200 * 2) {
        session.append_pcm16(chunk);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    session.commit().unwrap();
    let mut final_text = None;
    while final_text.is_none() {
        match next(&mut rx).await {
            SttEvent::Transcript(t) if t.is_final => final_text = Some(t.transcript),
            SttEvent::Error(e) => panic!("{e}"),
            _ => {}
        }
    }
    let text = final_text.unwrap();
    assert!(text.to_lowercase().contains("phebe"), "{text}");
    assert!(
        text.chars().next().unwrap().is_uppercase(),
        "first segment is capitalised: {text}"
    );
}

#[tokio::test]
#[ignore = "loads the 330 MB Kokoro model"]
async fn kokoro_synthesizes_audible_pcm() {
    let Some(models) = local_models_dir() else {
        eprintln!("skipping: no local models dir");
        return;
    };
    let dir = model_dir(&models, "kokoro-en-v0_19").unwrap();
    if !dir.join("model.onnx").exists() {
        eprintln!("skipping: Kokoro model not installed");
        return;
    }
    let tts = LocalTts::new(
        SherpaKokoroEngine::new(&dir, None, None).unwrap(),
        Some(0),
        None,
    );
    let result = tts.synthesize_speech("Hello from Paseo.").await.unwrap();
    assert_eq!(result.format, "pcm;rate=24000");
    let audio = result.collect().await.unwrap();
    assert!(
        audio.len() > 24000,
        "more than 0.5 s of PCM16 audio, got {} bytes",
        audio.len()
    );
    let peak = audio
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]).unsigned_abs())
        .max()
        .unwrap();
    assert!(peak > 1000, "audible, peak {peak}");
    tts.free();
}

// ---- assembly (no model loaded) ------------------------------------------------------------

#[tokio::test]
async fn local_services_assemble_lazily_and_report_missing_models_on_connect() {
    use paseo_speech::providers::local::config::{
        LocalSpeechModelConfig, LocalSpeechProviderConfig,
    };
    use paseo_speech::providers::local::sherpa::initialize_local_speech_services;
    use paseo_speech::types::RequestedSpeechProviders;

    let empty = tempfile::tempdir().unwrap();
    let local = LocalSpeechProviderConfig {
        models_dir: empty.path().to_path_buf(),
        models: LocalSpeechModelConfig {
            dictation_stt: "parakeet-tdt-0.6b-v2-int8".into(),
            voice_stt: "parakeet-tdt-0.6b-v2-int8".into(),
            voice_tts: "kokoro-en-v0_19".into(),
            voice_tts_speaker_id: Some(0),
            voice_tts_speed: None,
        },
    };
    let mut providers = RequestedSpeechProviders::default();
    providers.voice_stt.enabled = Some(false);

    let services = initialize_local_speech_services(&providers, Some(&local), "en", "en");
    assert!(services.dictation_stt.is_some());
    assert!(services.stt.is_none(), "voice STT is disabled");
    assert!(
        services.local_voice_stt_provider.is_some(),
        "kept for the Soniox fallback"
    );
    assert!(services.tts.is_some());
    assert!(services.turn_detection.is_some());
    assert_eq!(services.dictation_stt.as_ref().unwrap().id(), "local");
    // Nemotron is only needed for Vietnamese or when installed elsewhere; offline models are catalog ids.
    assert!(services
        .required_model_ids
        .contains(&"kokoro-en-v0_19".to_string()));

    // The turn-detection provider provisions the Silero asset on first use.
    let (session, _rx) = services
        .turn_detection
        .as_ref()
        .unwrap()
        .create_session()
        .unwrap();
    assert!(empty.path().join("silero-vad/silero_vad.onnx").exists());
    session.close();

    // Nothing was loaded at assembly time; the missing model surfaces when a session connects.
    let (session, _rx) = services
        .dictation_stt
        .as_ref()
        .unwrap()
        .create_session(&SttSessionParams::default())
        .unwrap();
    let err = session.connect().await.unwrap_err();
    assert!(
        err.to_string().starts_with("Missing offline encoder:"),
        "{err}"
    );

    // ...and TTS reports it on first synthesis.
    let err = services
        .tts
        .as_ref()
        .unwrap()
        .synthesize_speech("hi")
        .await
        .err()
        .expect("error");
    assert!(err.to_string().starts_with("Missing TTS model:"), "{err}");

    let none = initialize_local_speech_services(&providers, None, "en", "en");
    assert!(none.dictation_stt.is_none() && none.tts.is_none());
}
