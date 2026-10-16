//! Nemotron 3.5 streaming STT (port of `sherpa/sherpa-online-nemotron-stt.ts`).
//!
//! The recognizer sits behind [`StreamingEngine`] (sherpa-onnx `OnlineRecognizer` in production),
//! so the segment/endpoint/commit logic is testable without a model. Each session runs its audio
//! on a dedicated thread; the async API only enqueues work.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::audio::{
    parse_pcm16_mono_wav, parse_pcm_rate_from_format, pcm16le_peak_abs, pcm16le_to_float32,
};
use crate::error::{Result, SpeechError};
use crate::provider::{
    stt_event_channel, CommittedEvent, SttEvent, SttEventReceiver, SttEventSender, SttProvider,
    SttSession, SttSessionParams, TranscriptEvent, TranscriptionResult,
};

use super::lazy::LazyEngine;
use super::text::{
    collapse_repeats, normalize_chunk_gain, normalize_nemotron_language, resample_linear,
};

pub const NEMOTRON_SAMPLE_RATE: u32 = 16000;
/// Trailing silence fed before `input_finished` so the transducer can flush its right context.
pub const RIGHT_CONTEXT_FLUSH_SECONDS: f64 = 0.6;
pub const DEFAULT_SILENCE_PEAK_THRESHOLD: i32 = 200;

/// A streaming recognizer plus per-utterance streams.
pub trait StreamingEngine: Send + Sync + 'static {
    type Stream: Send + 'static;

    fn sample_rate(&self) -> u32;
    fn default_language(&self) -> String;
    fn create_stream(&self, language: &str) -> Result<Self::Stream>;
    fn accept_waveform(&self, stream: &mut Self::Stream, samples: &[f32], sample_rate: u32);
    fn input_finished(&self, stream: &mut Self::Stream);
    /// Decodes while the stream has enough audio.
    fn decode_ready(&self, stream: &mut Self::Stream);
    fn is_endpoint(&self, stream: &Self::Stream) -> bool;
    /// Resets after an endpoint and re-applies the language option.
    fn reset(&self, stream: &mut Self::Stream, language: &str);
    /// Current hypothesis, trimmed.
    fn result_text(&self, stream: &Self::Stream) -> String;
}

/// One-shot transcription of a whole buffer (`engine.transcribeFloat32`).
pub fn transcribe_float32<E: StreamingEngine>(
    engine: &E,
    samples: &[f32],
    sample_rate: u32,
    language: Option<&str>,
) -> Result<TranscriptionResult> {
    let started = std::time::Instant::now();
    let rate = engine.sample_rate();
    let mono = if sample_rate == rate {
        samples.to_vec()
    } else {
        resample_linear(samples, sample_rate, rate).map_err(SpeechError::msg)?
    };
    let gained = normalize_chunk_gain(&mono);
    let lang = normalize_nemotron_language(Some(language.unwrap_or(&engine.default_language())));
    let mut stream = engine.create_stream(&lang)?;

    let chunk_size = ((rate as f64 * 0.28).round() as usize).max(1);
    for chunk in gained.chunks(chunk_size) {
        engine.accept_waveform(&mut stream, chunk, rate);
        engine.decode_ready(&mut stream);
    }
    let padding = vec![0f32; (rate as f64 * RIGHT_CONTEXT_FLUSH_SECONDS).round() as usize];
    engine.accept_waveform(&mut stream, &padding, rate);
    engine.input_finished(&mut stream);
    engine.decode_ready(&mut stream);

    let text = collapse_repeats(&engine.result_text(&stream));
    Ok(TranscriptionResult {
        is_low_confidence: Some(text.is_empty()),
        text,
        language: Some(lang),
        duration: Some(started.elapsed().as_millis() as f64),
        ..Default::default()
    })
}

/// The segment/endpoint/commit logic of one realtime session.
pub struct NemotronCore<E: StreamingEngine> {
    engine: Arc<E>,
    language: String,
    stream: E::Stream,
    sample_rate: u32,
    current_segment_id: String,
    previous_segment_id: Option<String>,
    completed_segments: Vec<String>,
    last_partial_emitted: String,
    has_audio_in_current_turn: bool,
    events: SttEventSender,
}

impl<E: StreamingEngine> NemotronCore<E> {
    pub fn new(engine: Arc<E>, language: &str, events: SttEventSender) -> Result<Self> {
        let language = normalize_nemotron_language(Some(language));
        let stream = engine.create_stream(&language)?;
        Ok(Self {
            sample_rate: engine.sample_rate(),
            engine,
            language,
            stream,
            current_segment_id: uuid::Uuid::new_v4().to_string(),
            previous_segment_id: None,
            completed_segments: Vec::new(),
            last_partial_emitted: String::new(),
            has_audio_in_current_turn: false,
            events,
        })
    }

    fn emit(&self, event: SttEvent) {
        let _ = self.events.send(event);
    }

    fn combined_transcript(&self, active: &str) -> String {
        let mut parts: Vec<&str> = self.completed_segments.iter().map(String::as_str).collect();
        if !active.trim().is_empty() {
            parts.push(active.trim());
        }
        collapse_repeats(&parts.join(" "))
    }

    fn new_turn(&mut self) -> Result<()> {
        self.stream = self.engine.create_stream(&self.language)?;
        self.completed_segments.clear();
        self.last_partial_emitted.clear();
        self.has_audio_in_current_turn = false;
        Ok(())
    }

    pub fn append_pcm16(&mut self, pcm16le: &[u8]) {
        if pcm16le.is_empty() {
            return;
        }
        let aligned = if pcm16le.len().is_multiple_of(2) {
            pcm16le
        } else {
            &pcm16le[..pcm16le.len() - 1]
        };
        if aligned.is_empty() {
            return;
        }
        let raw = match pcm16le_to_float32(aligned, 1.0) {
            Ok(r) => r,
            Err(err) => return self.emit(SttEvent::Error(err.to_string())),
        };
        let gained = normalize_chunk_gain(&raw);

        self.has_audio_in_current_turn = true;
        self.engine
            .accept_waveform(&mut self.stream, &gained, self.sample_rate);
        self.engine.decode_ready(&mut self.stream);

        let active = self.engine.result_text(&self.stream);
        if self.engine.is_endpoint(&self.stream) && !active.is_empty() {
            self.completed_segments.push(active);
            self.engine.reset(&mut self.stream, &self.language);
        }

        let current_active = self.engine.result_text(&self.stream);
        let combined = self.combined_transcript(&current_active);
        if !combined.is_empty() && combined != self.last_partial_emitted {
            self.last_partial_emitted = combined.clone();
            self.emit(SttEvent::Transcript(TranscriptEvent {
                segment_id: self.current_segment_id.clone(),
                transcript: combined,
                is_final: false,
                language: Some(self.language.clone()),
                ..Default::default()
            }));
        }
    }

    pub fn commit(&mut self) {
        let mut final_transcript = String::new();
        if self.has_audio_in_current_turn {
            let padding = vec![
                0f32;
                (self.sample_rate as f64 * RIGHT_CONTEXT_FLUSH_SECONDS).round()
                    as usize
            ];
            self.engine
                .accept_waveform(&mut self.stream, &padding, self.sample_rate);
            self.engine.input_finished(&mut self.stream);
            self.engine.decode_ready(&mut self.stream);
            let tail = self.engine.result_text(&self.stream);
            final_transcript = self.combined_transcript(&tail);
        }

        let segment_id = self.current_segment_id.clone();
        let previous = self.previous_segment_id.clone();
        let is_empty = final_transcript.is_empty();
        let transcript = TranscriptEvent {
            segment_id: segment_id.clone(),
            transcript: final_transcript,
            is_final: true,
            language: Some(self.language.clone()),
            is_low_confidence: is_empty.then_some(true),
            ..Default::default()
        };

        self.previous_segment_id = Some(segment_id.clone());
        self.current_segment_id = uuid::Uuid::new_v4().to_string();
        if let Err(err) = self.new_turn() {
            return self.emit(SttEvent::Error(err.to_string()));
        }

        self.emit(SttEvent::Committed(CommittedEvent {
            segment_id,
            previous_segment_id: previous,
        }));
        self.emit(SttEvent::Transcript(transcript));
    }

    pub fn clear(&mut self) {
        self.current_segment_id = uuid::Uuid::new_v4().to_string();
        if let Err(err) = self.new_turn() {
            self.emit(SttEvent::Error(err.to_string()));
        }
    }
}

enum Command {
    Append(Vec<u8>),
    Commit,
    Clear,
    Close,
}

struct SessionShared<E: StreamingEngine> {
    engine: Arc<LazyEngine<E>>,
    language: String,
    events: SttEventSender,
    connected: AtomicBool,
    worker: Mutex<Option<std_mpsc::Sender<Command>>>,
}

pub struct NemotronSession<E: StreamingEngine> {
    shared: Arc<SessionShared<E>>,
}

impl<E: StreamingEngine> NemotronSession<E> {
    fn not_connected(&self) {
        let _ = self.shared.events.send(SttEvent::Error(
            "Nemotron realtime session not connected".into(),
        ));
    }

    fn send(&self, command: Command) {
        if let Some(tx) = self.shared.worker.lock().unwrap().as_ref() {
            let _ = tx.send(command);
        }
    }
}

#[async_trait]
impl<E: StreamingEngine> SttSession for NemotronSession<E> {
    fn required_sample_rate(&self) -> u32 {
        NEMOTRON_SAMPLE_RATE
    }

    async fn connect(&self) -> Result<()> {
        if self.shared.connected.load(Ordering::SeqCst) {
            return Ok(());
        }
        let engine = self.shared.engine.get().await?;
        let language = self.shared.language.clone();
        let events = self.shared.events.clone();
        let core = tokio::task::spawn_blocking({
            let engine = engine.clone();
            move || NemotronCore::new(engine, &language, events)
        })
        .await
        .map_err(|e| SpeechError::msg(e.to_string()))??;

        let (tx, rx) = std_mpsc::channel::<Command>();
        std::thread::Builder::new()
            .name("paseo-nemotron-session".into())
            .spawn(move || {
                let mut core = core;
                while let Ok(command) = rx.recv() {
                    match command {
                        Command::Append(pcm) => core.append_pcm16(&pcm),
                        Command::Commit => core.commit(),
                        Command::Clear => core.clear(),
                        Command::Close => break,
                    }
                }
            })
            .map_err(SpeechError::from)?;
        *self.shared.worker.lock().unwrap() = Some(tx);
        self.shared.connected.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn append_pcm16(&self, pcm16le: &[u8]) {
        if !self.shared.connected.load(Ordering::SeqCst) {
            return self.not_connected();
        }
        self.send(Command::Append(pcm16le.to_vec()));
    }

    fn commit(&self) -> Result<()> {
        if !self.shared.connected.load(Ordering::SeqCst) {
            self.not_connected();
            return Ok(());
        }
        self.send(Command::Commit);
        Ok(())
    }

    fn clear(&self) {
        if self.shared.connected.load(Ordering::SeqCst) {
            self.send(Command::Clear);
        }
    }

    fn close(&self) -> Result<()> {
        self.shared.connected.store(false, Ordering::SeqCst);
        if let Some(tx) = self.shared.worker.lock().unwrap().take() {
            let _ = tx.send(Command::Close);
        }
        Ok(())
    }
}

impl<E: StreamingEngine> Drop for NemotronSession<E> {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

pub struct NemotronStt<E: StreamingEngine> {
    engine: Arc<LazyEngine<E>>,
    default_language: String,
    silence_peak_threshold: i32,
}

impl<E: StreamingEngine> NemotronStt<E> {
    pub fn new(engine: Arc<LazyEngine<E>>, default_language: Option<&str>) -> Self {
        Self {
            engine,
            default_language: normalize_nemotron_language(default_language),
            silence_peak_threshold: DEFAULT_SILENCE_PEAK_THRESHOLD,
        }
    }

    pub fn with_silence_peak_threshold(mut self, threshold: i32) -> Self {
        self.silence_peak_threshold = threshold;
        self
    }

    /// Single-shot transcription of `audio/wav` or `audio/pcm;rate=N` bytes.
    pub async fn transcribe_audio(
        &self,
        audio: &[u8],
        format: &str,
    ) -> Result<TranscriptionResult> {
        let started = std::time::Instant::now();
        if audio.is_empty() {
            return Ok(TranscriptionResult {
                language: Some(self.default_language.clone()),
                duration: Some(0.0),
                is_low_confidence: Some(true),
                ..Default::default()
            });
        }
        let fmt = format.trim().to_lowercase();
        let (sample_rate, pcm16): (u32, Vec<u8>) =
            if fmt.contains("audio/wav") || fmt == "audio/wave" {
                let parsed = parse_pcm16_mono_wav(audio)?;
                (parsed.sample_rate, parsed.pcm16)
            } else if fmt.contains("audio/pcm") {
                let rate = parse_pcm_rate_from_format(&fmt, Some(NEMOTRON_SAMPLE_RATE))
                    .unwrap_or(NEMOTRON_SAMPLE_RATE);
                let end = audio.len() - audio.len() % 2;
                (rate, audio[..end].to_vec())
            } else {
                return Err(SpeechError::msg(format!(
                    "Unsupported audio format for Nemotron STT: {format}"
                )));
            };

        if pcm16le_peak_abs(&pcm16)? < self.silence_peak_threshold {
            return Ok(TranscriptionResult {
                language: Some(self.default_language.clone()),
                duration: Some(started.elapsed().as_millis() as f64),
                is_low_confidence: Some(true),
                ..Default::default()
            });
        }
        let samples = pcm16le_to_float32(&pcm16, 1.0)?;
        let engine = self.engine.get().await?;
        let language = self.default_language.clone();
        tokio::task::spawn_blocking(move || {
            transcribe_float32(engine.as_ref(), &samples, sample_rate, Some(&language))
        })
        .await
        .map_err(|e| SpeechError::msg(e.to_string()))?
    }
}

impl<E: StreamingEngine> SttProvider for NemotronStt<E> {
    fn id(&self) -> &str {
        "local"
    }

    fn create_session(
        &self,
        params: &SttSessionParams,
    ) -> Result<(Arc<dyn SttSession>, SttEventReceiver)> {
        let (tx, rx) = stt_event_channel();
        let language = params
            .language
            .clone()
            .unwrap_or_else(|| self.default_language.clone());
        let session = NemotronSession {
            shared: Arc::new(SessionShared {
                engine: self.engine.clone(),
                language: normalize_nemotron_language(Some(&language)),
                events: tx,
                connected: AtomicBool::new(false),
                worker: Mutex::new(None),
            }),
        };
        Ok((Arc::new(session), rx))
    }
}
