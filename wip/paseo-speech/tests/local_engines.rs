//! Local STT/TTS session logic with fake engines (no models needed). Ports the TS unit tests
//! `sherpa-parakeet-stt.test.ts`, `sherpa-tts.test.ts` and the helper cases of
//! `sherpa-online-nemotron-stt.test.ts`; the real-model flows live in `tests/sherpa_live.rs`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::Duration;

use paseo_speech::provider::{
    CommittedEvent, SttEvent, SttEventReceiver, SttProvider, SttSessionParams, TtsProvider,
};
use paseo_speech::providers::local::lazy::LazyEngine;
use paseo_speech::providers::local::nemotron::{
    NemotronCore, NemotronStt, StreamingEngine, NEMOTRON_SAMPLE_RATE,
};
use paseo_speech::providers::local::parakeet::{OfflineEngine, ParakeetRealtimeStt, ParakeetStt};
use paseo_speech::providers::local::tts::{LocalTts, TtsAudio, TtsEngine};
use paseo_speech::{Result, SpeechError};

fn pcm(values: &[i16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn drain(rx: &mut SttEventReceiver) -> Vec<SttEvent> {
    let mut out = Vec::new();
    while let Ok(e) = rx.try_recv() {
        out.push(e);
    }
    out
}

async fn next(rx: &mut SttEventReceiver) -> SttEvent {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("event")
        .expect("open")
}

// ======================= Nemotron =================================================

#[derive(Default)]
struct FakeStream {
    samples: usize,
    max_abs: f32,
    finished: bool,
    resets: usize,
    language: String,
}

/// `hypothesis(stream)` and `endpoint(stream)` script the recognizer.
struct FakeStreamingInner {
    hypothesis: Box<dyn Fn(&FakeStream) -> String + Send + Sync>,
    endpoint: Box<dyn Fn(&FakeStream) -> bool + Send + Sync>,
    log: Mutex<Vec<String>>,
}

#[derive(Clone)]
struct FakeStreaming(Arc<FakeStreamingInner>);

impl std::ops::Deref for FakeStreaming {
    type Target = FakeStreamingInner;
    fn deref(&self) -> &FakeStreamingInner {
        &self.0
    }
}

impl FakeStreaming {
    fn new(
        hypothesis: impl Fn(&FakeStream) -> String + Send + Sync + 'static,
        endpoint: impl Fn(&FakeStream) -> bool + Send + Sync + 'static,
    ) -> FakeStreaming {
        FakeStreaming(Arc::new(FakeStreamingInner {
            hypothesis: Box::new(hypothesis),
            endpoint: Box::new(endpoint),
            log: Mutex::new(vec![]),
        }))
    }
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl StreamingEngine for FakeStreaming {
    type Stream = FakeStream;
    fn sample_rate(&self) -> u32 {
        NEMOTRON_SAMPLE_RATE
    }
    fn default_language(&self) -> String {
        "vi".into()
    }
    fn create_stream(&self, language: &str) -> Result<FakeStream> {
        self.log.lock().unwrap().push(format!("create:{language}"));
        Ok(FakeStream {
            language: language.to_string(),
            ..Default::default()
        })
    }
    fn accept_waveform(&self, stream: &mut FakeStream, samples: &[f32], sample_rate: u32) {
        assert_eq!(sample_rate, NEMOTRON_SAMPLE_RATE);
        stream.samples += samples.len();
        stream.max_abs = samples.iter().fold(stream.max_abs, |m, s| m.max(s.abs()));
    }
    fn input_finished(&self, stream: &mut FakeStream) {
        stream.finished = true;
        self.log.lock().unwrap().push("finished".into());
    }
    fn decode_ready(&self, _stream: &mut FakeStream) {}
    fn is_endpoint(&self, stream: &FakeStream) -> bool {
        (self.endpoint)(stream)
    }
    fn reset(&self, stream: &mut FakeStream, language: &str) {
        stream.samples = 0;
        stream.resets += 1;
        stream.language = language.to_string();
        self.log.lock().unwrap().push(format!("reset:{language}"));
    }
    fn result_text(&self, stream: &FakeStream) -> String {
        (self.hypothesis)(stream).trim().to_string()
    }
}

fn core(engine: &FakeStreaming, language: &str) -> (NemotronCore<FakeStreaming>, SttEventReceiver) {
    let (tx, rx) = paseo_speech::provider::stt_event_channel();
    (
        NemotronCore::new(Arc::new(engine.clone()), language, tx).unwrap(),
        rx,
    )
}

fn loud(samples: usize) -> Vec<u8> {
    pcm(&vec![8000i16; samples])
}

#[test]
fn nemotron_emits_a_partial_only_when_the_combined_text_changes() {
    let engine = FakeStreaming::new(
        |s| match s.samples {
            0 => String::new(),
            1..=3200 => "xin".into(),
            3201..=6400 => "xin chào".into(),
            _ => "xin chào các bạn".into(),
        },
        |_| false,
    );
    let (mut core, mut rx) = core(&engine, "vi-VN");
    assert_eq!(engine.log(), vec!["create:vi"]);
    core.append_pcm16(&loud(3200));
    core.append_pcm16(&loud(100)); // still "xin"? 3300 samples -> "xin chào"
    core.append_pcm16(&loud(0)); // empty chunk: no-op
    core.append_pcm16(&loud(3200)); // 6500 -> "xin chào các bạn"
    let partials: Vec<String> = drain(&mut rx)
        .into_iter()
        .map(|e| match e {
            SttEvent::Transcript(t) => {
                assert!(!t.is_final);
                assert_eq!(t.language.as_deref(), Some("vi"));
                t.transcript
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(partials, vec!["xin", "xin chào", "xin chào các bạn"]);
}

#[test]
fn nemotron_endpoint_moves_text_into_completed_segments_and_resets_the_stream() {
    // Every 1600 samples the engine reports an endpoint; hypothesis is the sample count in words.
    let engine = FakeStreaming::new(
        |s| match s.samples {
            0 => String::new(),
            n => format!("w{}", n / 1600),
        },
        |s| s.samples >= 1600,
    );
    let (mut core, mut rx) = core(&engine, "vi");
    core.append_pcm16(&loud(1600)); // "w1" is an endpoint -> completed, reset
    core.append_pcm16(&loud(1600)); // "w1" again -> completed again
    let texts: Vec<String> = drain(&mut rx)
        .into_iter()
        .filter_map(|e| match e {
            SttEvent::Transcript(t) => Some(t.transcript),
            _ => None,
        })
        .collect();
    // second chunk produces "w1 w1" which collapse_repeats folds to "w1", equal to the last partial
    assert_eq!(texts, vec!["w1"]);
    assert_eq!(
        engine
            .log()
            .iter()
            .filter(|l| l.starts_with("reset:"))
            .count(),
        2
    );

    core.commit();
    let events = drain(&mut rx);
    match &events[1] {
        SttEvent::Transcript(t) => {
            assert!(t.is_final);
            // the flush padding (0.6 s) makes the fake report "w6" for the open tail
            assert_eq!(t.transcript, "w1 w6");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn nemotron_commit_flushes_right_context_and_chains_segment_ids() {
    let engine = FakeStreaming::new(
        |s| {
            if s.finished {
                "tail words".into()
            } else if s.samples > 0 {
                "head".into()
            } else {
                String::new()
            }
        },
        |_| false,
    );
    let (mut core, mut rx) = core(&engine, "vi");
    core.append_pcm16(&loud(1600));
    drain(&mut rx);
    core.commit();
    let events = drain(&mut rx);
    let (first_id, first_prev) = match (&events[0], &events[1]) {
        (SttEvent::Committed(c), SttEvent::Transcript(t)) => {
            assert_eq!(c.segment_id, t.segment_id);
            assert!(t.is_final);
            assert_eq!(t.transcript, "tail words");
            assert_eq!(t.is_low_confidence, None);
            (c.segment_id.clone(), c.previous_segment_id.clone())
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(first_prev, None);
    assert!(engine.log().contains(&"finished".to_string()));

    // second turn: new stream, previous id chained, no audio => empty low-confidence final
    core.commit();
    let events = drain(&mut rx);
    match (&events[0], &events[1]) {
        (
            SttEvent::Committed(CommittedEvent {
                segment_id,
                previous_segment_id,
            }),
            SttEvent::Transcript(t),
        ) => {
            assert_ne!(segment_id, &first_id);
            assert_eq!(previous_segment_id.as_deref(), Some(first_id.as_str()));
            assert_eq!(t.transcript, "");
            assert_eq!(t.is_low_confidence, Some(true));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn nemotron_clear_starts_a_fresh_turn_without_events_and_tolerates_odd_pcm() {
    let engine = FakeStreaming::new(|_| String::new(), |_| false);
    let (mut core, mut rx) = core(&engine, "vi");
    core.clear();
    assert!(drain(&mut rx).is_empty());
    assert_eq!(engine.log(), vec!["create:vi", "create:vi"]);
    // odd trailing byte is dropped, and a quiet chunk is lifted before it reaches the engine
    let mut quiet = pcm(&[1000, -1000]);
    quiet.push(0x55);
    core.append_pcm16(&quiet);
    assert!(drain(&mut rx).is_empty());
}

#[tokio::test]
async fn nemotron_session_runs_audio_on_its_own_thread_and_reports_events() {
    let engine = FakeStreaming::new(
        |s| {
            if s.finished {
                "xin chào".into()
            } else if s.samples > 0 {
                "xin".into()
            } else {
                String::new()
            }
        },
        |_| false,
    );
    let lazy = LazyEngine::ready(engine.clone());
    let stt = NemotronStt::new(lazy, Some("vi"));
    assert_eq!(stt.id(), "local");
    let (session, mut rx) = stt
        .create_session(&SttSessionParams {
            language: Some("en-US".into()),
            prompt: None,
        })
        .unwrap();
    assert_eq!(session.required_sample_rate(), 16000);

    // not connected yet
    session.append_pcm16(&loud(100));
    assert_eq!(
        next(&mut rx).await,
        SttEvent::Error("Nemotron realtime session not connected".into())
    );
    session.commit().unwrap();
    assert_eq!(
        next(&mut rx).await,
        SttEvent::Error("Nemotron realtime session not connected".into())
    );

    session.connect().await.unwrap();
    session.connect().await.unwrap(); // idempotent
    assert!(
        engine.log().contains(&"create:en".to_string()),
        "language normalised: {:?}",
        engine.log()
    );
    session.append_pcm16(&loud(1600));
    session.commit().unwrap();
    match next(&mut rx).await {
        SttEvent::Transcript(t) => assert_eq!((t.transcript.as_str(), t.is_final), ("xin", false)),
        other => panic!("{other:?}"),
    }
    assert!(matches!(next(&mut rx).await, SttEvent::Committed(_)));
    match next(&mut rx).await {
        SttEvent::Transcript(t) => {
            assert_eq!((t.transcript.as_str(), t.is_final), ("xin chào", true))
        }
        other => panic!("{other:?}"),
    }
    session.close().unwrap();
    session.append_pcm16(&loud(100));
    assert_eq!(
        next(&mut rx).await,
        SttEvent::Error("Nemotron realtime session not connected".into())
    );
}

#[tokio::test]
async fn nemotron_single_shot_handles_formats_silence_and_resampling() {
    let seen = Arc::new(Mutex::new(Vec::<(usize, bool)>::new()));
    let seen_in = seen.clone();
    let engine = FakeStreaming::new(
        move |s| {
            if s.finished {
                seen_in.lock().unwrap().push((s.samples, true));
                "đất nước tôi".into()
            } else {
                String::new()
            }
        },
        |_| false,
    );
    let stt = NemotronStt::new(LazyEngine::ready(engine.clone()), Some("vi"));

    // silence is short-circuited
    let silent = stt
        .transcribe_audio(&pcm(&vec![5i16; 1600]), "audio/pcm;rate=16000")
        .await
        .unwrap();
    assert_eq!(silent.text, "");
    assert_eq!(silent.is_low_confidence, Some(true));
    assert!(engine.log().is_empty(), "engine untouched for silence");

    // empty and unsupported
    assert_eq!(
        stt.transcribe_audio(&[], "audio/wav")
            .await
            .unwrap()
            .duration,
        Some(0.0)
    );
    let err = stt
        .transcribe_audio(&[1, 2], "audio/mp3")
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Unsupported audio format for Nemotron STT: audio/mp3"
    );

    // 48 kHz PCM is resampled to 16 kHz (+0.6 s of flush padding = 9600 samples)
    let result = stt
        .transcribe_audio(&pcm(&vec![9000i16; 4800]), "audio/pcm;rate=48000")
        .await
        .unwrap();
    assert_eq!(result.text, "đất nước tôi");
    assert_eq!(result.language.as_deref(), Some("vi"));
    assert_eq!(result.is_low_confidence, Some(false));
    assert_eq!(seen.lock().unwrap()[0].0, 1600 + 9600);

    // WAV input
    let wav = paseo_speech::audio::pcm_to_wav(&pcm(&vec![9000i16; 3200]), 16000, 1, 16);
    let result = stt.transcribe_audio(&wav, "audio/wav").await.unwrap();
    assert_eq!(result.text, "đất nước tôi");
    assert_eq!(seen.lock().unwrap()[1].0, 3200 + 9600);
}

// ======================= Parakeet =================================================

type TextFn = Box<dyn Fn(&[f32]) -> String + Send + Sync>;

struct FakeOfflineInner {
    calls: AtomicUsize,
    delay: Duration,
    text: TextFn,
    sample_counts: Mutex<Vec<usize>>,
    uppercase: bool,
}

#[derive(Clone)]
struct FakeOffline(Arc<FakeOfflineInner>);

impl std::ops::Deref for FakeOffline {
    type Target = FakeOfflineInner;
    fn deref(&self) -> &FakeOfflineInner {
        &self.0
    }
}

impl FakeOffline {
    fn build(
        text: impl Fn(&[f32]) -> String + Send + Sync + 'static,
        delay: Duration,
        uppercase: bool,
    ) -> FakeOffline {
        FakeOffline(Arc::new(FakeOfflineInner {
            calls: AtomicUsize::new(0),
            delay,
            text: Box::new(text),
            sample_counts: Mutex::new(vec![]),
            uppercase,
        }))
    }
    fn new(text: impl Fn(&[f32]) -> String + Send + Sync + 'static) -> FakeOffline {
        Self::build(text, Duration::ZERO, false)
    }
}

impl OfflineEngine for FakeOffline {
    fn sample_rate(&self) -> u32 {
        16000
    }
    fn normalize_text(&self, text: &str) -> String {
        if self.uppercase {
            text.to_lowercase()
        } else {
            text.to_string()
        }
    }
    fn recognize(&self, samples: &[f32], sample_rate: u32) -> Result<String> {
        assert_eq!(sample_rate, 16000);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.sample_counts.lock().unwrap().push(samples.len());
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        Ok((self.text)(samples))
    }
}

#[tokio::test]
async fn parakeet_commit_snapshots_ids_and_audio_before_async_transcription() {
    // First float distinguishes the two buffers after peak normalisation: 0.3 vs 0.45.
    let engine = FakeOffline::new(|s| {
        if s[0] < 0.4 {
            "first".into()
        } else {
            "second".into()
        }
    });
    let stt = ParakeetStt::new(LazyEngine::ready(engine.clone()));
    let (session, mut rx) = stt.create_session(&SttSessionParams::default()).unwrap();
    session.connect().await.unwrap();

    session.append_pcm16(&[1, 2, 3, 4]);
    session.commit().unwrap();
    session.append_pcm16(&[5, 6, 7, 8]);
    session.commit().unwrap();

    let mut committed = Vec::new();
    let mut transcripts: HashMap<String, String> = HashMap::new();
    while transcripts.len() < 2 {
        match next(&mut rx).await {
            SttEvent::Committed(c) => committed.push(c),
            SttEvent::Transcript(t) => {
                assert!(t.is_final);
                transcripts.insert(t.segment_id, t.transcript);
            }
            SttEvent::Error(e) => panic!("{e}"),
        }
    }
    assert_eq!(committed.len(), 2);
    assert_ne!(committed[0].segment_id, committed[1].segment_id);
    assert_eq!(committed[0].previous_segment_id, None);
    assert_eq!(
        committed[1].previous_segment_id.as_deref(),
        Some(committed[0].segment_id.as_str())
    );
    assert_eq!(transcripts[&committed[0].segment_id], "first");
    assert_eq!(transcripts[&committed[1].segment_id], "second");
    assert_eq!(*engine.sample_counts.lock().unwrap(), vec![2, 2]);
}

#[tokio::test]
async fn parakeet_single_shot_skips_silence_resamples_and_normalises_text() {
    let raw = FakeOffline::build(
        |s| format!("N{}", s.len()).to_uppercase(),
        Duration::ZERO,
        true,
    );
    let stt = ParakeetStt::new(LazyEngine::ready(raw.clone()));

    let silent = stt
        .transcribe_audio(&pcm(&vec![10i16; 800]), "audio/pcm;rate=16000")
        .await
        .unwrap();
    assert_eq!(
        (silent.text.as_str(), silent.is_low_confidence),
        ("", Some(true))
    );
    assert_eq!(raw.calls.load(Ordering::SeqCst), 0);

    let result = stt
        .transcribe_audio(&pcm(&vec![9000i16; 4800]), "audio/pcm;rate=48000")
        .await
        .unwrap();
    assert_eq!(result.text, "n1600"); // lower-cased by normalize_text; 1/3 of the samples
    assert!(result.duration.is_some());

    let err = stt
        .transcribe_audio(&[0, 0], "audio/ogg")
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Unsupported audio format for sherpa Parakeet STT: audio/ogg"
    );
    let err = stt
        .transcribe_audio(b"junk", "audio/wav")
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "Invalid WAV header");
}

#[tokio::test]
async fn parakeet_batch_session_reports_unconnected_use_and_engine_load_failures() {
    let stt = ParakeetStt::new(LazyEngine::ready(FakeOffline::new(|_| "x".into())));
    let (session, mut rx) = stt.create_session(&SttSessionParams::default()).unwrap();
    session.append_pcm16(&[1, 0]);
    assert_eq!(
        next(&mut rx).await,
        SttEvent::Error("STT session not connected".into())
    );
    session.commit().unwrap();
    assert_eq!(
        next(&mut rx).await,
        SttEvent::Error("STT session not connected".into())
    );

    let failing: Arc<LazyEngine<FakeOffline>> = LazyEngine::new(|| {
        Err(SpeechError::msg(
            "Missing offline encoder: /nope/encoder.int8.onnx",
        ))
    });
    let stt = ParakeetStt::new(failing);
    let (session, _rx) = stt.create_session(&SttSessionParams::default()).unwrap();
    assert_eq!(
        session.connect().await.unwrap_err().to_string(),
        "Missing offline encoder: /nope/encoder.int8.onnx"
    );
}

#[tokio::test]
async fn parakeet_realtime_session_emits_capitalised_partials_then_a_final() {
    let engine = FakeOffline::new(|s| {
        if s.len() >= 3200 {
            "xin chào các bạn".into()
        } else {
            "xin chào".into()
        }
    });
    let stt = ParakeetRealtimeStt::new(LazyEngine::ready(engine.clone()))
        .with_min_decode_interval(Duration::ZERO);
    let (session, mut rx) = stt.create_session(&SttSessionParams::default()).unwrap();
    session.connect().await.unwrap();

    session.append_pcm16(&pcm(&vec![6000i16; 1600]));
    match next(&mut rx).await {
        SttEvent::Transcript(t) => {
            assert_eq!((t.transcript.as_str(), t.is_final), ("Xin chào", false))
        }
        other => panic!("{other:?}"),
    }
    session.append_pcm16(&pcm(&vec![6000i16; 1600]));
    match next(&mut rx).await {
        SttEvent::Transcript(t) => assert_eq!(t.transcript, "Xin chào các bạn"),
        other => panic!("{other:?}"),
    }
    session.commit().unwrap();
    let (c, t) = (next(&mut rx).await, next(&mut rx).await);
    let (SttEvent::Committed(c), SttEvent::Transcript(t)) = (c, t) else {
        panic!("expected committed+final")
    };
    assert_eq!(c.segment_id, t.segment_id);
    assert!(t.is_final);
    assert_eq!(t.transcript, "Xin chào các bạn");

    // second segment: not capitalised again
    session.append_pcm16(&pcm(&vec![6000i16; 1600]));
    match next(&mut rx).await {
        SttEvent::Transcript(t) => assert_eq!(t.transcript, "xin chào"),
        other => panic!("{other:?}"),
    }
}

/// Regression for a TS race: commit during an in-flight partial decode returned early and emitted
/// the stale partial text as the final transcript.
#[tokio::test]
async fn parakeet_realtime_commit_waits_for_an_in_flight_decode_and_decodes_everything() {
    let engine = FakeOffline::build(
        |s| format!("samples={}", s.len()),
        Duration::from_millis(80),
        false,
    );
    let stt = ParakeetRealtimeStt::new(LazyEngine::ready(engine.clone()))
        .with_min_decode_interval(Duration::ZERO);
    let (session, mut rx) = stt.create_session(&SttSessionParams::default()).unwrap();
    session.connect().await.unwrap();

    session.append_pcm16(&pcm(&vec![6000i16; 1600])); // starts a slow partial decode
    tokio::time::sleep(Duration::from_millis(20)).await;
    session.append_pcm16(&pcm(&vec![6000i16; 1600])); // arrives mid-decode
    session.commit().unwrap();

    let mut final_text = None;
    let mut partial_after_commit = false;
    let mut committed_seen = false;
    for _ in 0..10 {
        match next(&mut rx).await {
            SttEvent::Committed(_) => committed_seen = true,
            SttEvent::Transcript(t) if t.is_final => {
                final_text = Some(t.transcript);
                break;
            }
            SttEvent::Transcript(_) => partial_after_commit |= committed_seen,
            SttEvent::Error(e) => panic!("{e}"),
        }
    }
    assert_eq!(final_text.as_deref(), Some("Samples=3200"));
    assert!(!partial_after_commit);
}

#[tokio::test]
async fn parakeet_realtime_session_guards_unconnected_use() {
    let stt = ParakeetRealtimeStt::new(LazyEngine::ready(FakeOffline::new(|_| "x".into())));
    let (session, mut rx) = stt.create_session(&SttSessionParams::default()).unwrap();
    session.append_pcm16(&[1, 0]);
    assert_eq!(
        next(&mut rx).await,
        SttEvent::Error("Parakeet realtime session not connected".into())
    );
    session.commit().unwrap();
    assert_eq!(
        next(&mut rx).await,
        SttEvent::Error("Parakeet realtime session not connected".into())
    );
    session.clear(); // no-op, no panic
    session.connect().await.unwrap();
    session.clear();
    session.close().unwrap();
}

// ======================= TTS ======================================================

type Gate = std_mpsc::Receiver<()>;

struct FakeTtsEngine {
    requests: Arc<Mutex<Vec<(String, i32, f32)>>>,
    gate: Option<Mutex<Gate>>,
    fail_first: bool,
    attempts: AtomicUsize,
    released: Arc<Mutex<Vec<bool>>>,
    rate: Option<u32>,
}

impl Drop for FakeTtsEngine {
    fn drop(&mut self) {
        self.released.lock().unwrap().push(true);
    }
}

impl TtsEngine for FakeTtsEngine {
    fn sample_rate(&self) -> Option<u32> {
        self.rate
    }
    fn generate(&self, text: &str, speaker_id: i32, speed: f32) -> Result<TtsAudio> {
        if let Some(gate) = &self.gate {
            gate.lock().unwrap().recv().unwrap();
        }
        if self.fail_first && self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(SpeechError::msg("generation failed"));
        }
        self.requests
            .lock()
            .unwrap()
            .push((text.to_string(), speaker_id, speed));
        Ok(TtsAudio {
            samples: vec![0.0, 0.5, -0.5, 0.25],
            sample_rate: Some(24000),
        })
    }
}

struct TtsHarness {
    tts: Arc<LocalTts<FakeTtsEngine>>,
    requests: Arc<Mutex<Vec<(String, i32, f32)>>>,
    released: Arc<Mutex<Vec<bool>>>,
    gate: Option<std_mpsc::Sender<()>>,
}

fn tts(gated: bool, fail_first: bool) -> TtsHarness {
    let requests = Arc::new(Mutex::new(vec![]));
    let released = Arc::new(Mutex::new(vec![]));
    let (tx, rx) = std_mpsc::channel();
    let engine = FakeTtsEngine {
        requests: requests.clone(),
        gate: gated.then(|| Mutex::new(rx)),
        fail_first,
        attempts: AtomicUsize::new(0),
        released: released.clone(),
        rate: Some(24000),
    };
    TtsHarness {
        tts: Arc::new(LocalTts::new(engine, None, None)),
        requests,
        released,
        gate: gated.then_some(tx),
    }
}

async fn yield_a_lot() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test]
async fn tts_returns_chunked_pcm_with_the_requested_speaker_and_speed() {
    let h = tts(false, false);
    let result = h.tts.synthesize_speech("hello").await.unwrap();
    assert_eq!(result.format, "pcm;rate=24000");
    assert_eq!(
        *h.requests.lock().unwrap(),
        vec![("hello".to_string(), 0, 1.0)]
    );
    assert_eq!(result.collect().await.unwrap().len(), 8);

    // 50 ms chunks: 24 kHz * 0.05 * 2 bytes = 2400 bytes
    struct Long;
    impl TtsEngine for Long {
        fn sample_rate(&self) -> Option<u32> {
            None
        }
        fn generate(&self, _t: &str, sid: i32, speed: f32) -> Result<TtsAudio> {
            assert_eq!((sid, speed), (5, 1.35));
            Ok(TtsAudio {
                samples: vec![0.1; 3000],
                sample_rate: None,
            })
        }
    }
    let long = LocalTts::new(Long, Some(5), Some(1.35));
    let result = long.synthesize_speech("  padded  ").await.unwrap();
    assert_eq!(
        result.format, "pcm;rate=24000",
        "falls back to 24 kHz when nothing reports a rate"
    );
    use futures_util::StreamExt;
    let mut stream = result.stream;
    let mut sizes = vec![];
    while let Some(chunk) = stream.next().await {
        sizes.push(chunk.unwrap().len());
    }
    assert_eq!(sizes, vec![2400, 2400, 1200]);
}

#[tokio::test]
async fn tts_rejects_empty_text() {
    let h = tts(false, false);
    assert_eq!(
        h.tts
            .synthesize_speech("   ")
            .await
            .err()
            .unwrap()
            .to_string(),
        "Cannot synthesize empty text"
    );
}

#[tokio::test]
async fn tts_serializes_native_synthesis_without_blocking_the_runtime() {
    let h = tts(true, false);
    let first = tokio::spawn({
        let t = h.tts.clone();
        async move { t.synthesize_speech("first").await }
    });
    let second = tokio::spawn({
        let t = h.tts.clone();
        async move { t.synthesize_speech("second").await }
    });
    yield_a_lot().await;
    assert!(
        h.requests.lock().unwrap().is_empty(),
        "first generation is parked in the gate"
    );
    h.gate.as_ref().unwrap().send(()).unwrap();
    first.await.unwrap().unwrap();
    yield_a_lot().await;
    assert_eq!(
        h.requests.lock().unwrap().len(),
        1,
        "second waits for its own gate token"
    );
    h.gate.as_ref().unwrap().send(()).unwrap();
    second.await.unwrap().unwrap();
    let order: Vec<String> = h
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.0.clone())
        .collect();
    assert_eq!(order, vec!["first", "second"]);
}

#[tokio::test]
async fn tts_continues_queued_synthesis_after_a_native_failure() {
    let h = tts(false, true);
    let (first, second) = tokio::join!(
        h.tts.synthesize_speech("first"),
        h.tts.synthesize_speech("second")
    );
    assert_eq!(first.err().unwrap().to_string(), "generation failed");
    second.unwrap();
    assert_eq!(
        *h.requests.lock().unwrap(),
        vec![("second".to_string(), 0, 1.0)]
    );
}

#[tokio::test]
async fn tts_waits_for_active_synthesis_before_freeing_and_drops_queued_requests() {
    let h = tts(true, false);
    let first = tokio::spawn({
        let t = h.tts.clone();
        async move { t.synthesize_speech("first").await }
    });
    let second = tokio::spawn({
        let t = h.tts.clone();
        async move { t.synthesize_speech("second").await }
    });
    yield_a_lot().await;
    h.tts.free();
    assert!(
        h.released.lock().unwrap().is_empty(),
        "engine is still in use"
    );
    h.gate.as_ref().unwrap().send(()).unwrap();
    first.await.unwrap().unwrap();
    assert_eq!(
        second.await.unwrap().err().unwrap().to_string(),
        "TTS provider closed"
    );
    assert_eq!(h.requests.lock().unwrap().len(), 1);
    assert_eq!(
        h.tts
            .synthesize_speech("third")
            .await
            .err()
            .unwrap()
            .to_string(),
        "TTS provider closed"
    );
    // The engine is released once the last in-flight call is gone, and not before.
    drop(h.tts);
    yield_a_lot().await;
    assert_eq!(*h.released.lock().unwrap(), vec![true]);
}
