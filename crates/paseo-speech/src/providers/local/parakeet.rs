//! Offline (Parakeet) STT sessions (port of `sherpa-parakeet-stt.ts` and
//! `sherpa-parakeet-realtime-session.ts`).
//!
//! The offline recognizer sits behind [`OfflineEngine`]. Two session flavours exist, as in TS:
//! a batch session (commit transcribes everything buffered) used for realtime voice, and a
//! "realtime" session that re-decodes the growing buffer to produce partials, used for dictation.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::audio::{
    parse_pcm16_mono_wav, parse_pcm_rate_from_format, pcm16le_peak_abs, pcm16le_to_float32,
};
use crate::error::{Result, SpeechError};
use crate::provider::{
    stt_event_channel, CommittedEvent, SttEvent, SttEventReceiver, SttEventSender, SttProvider,
    SttSession, SttSessionParams, TranscriptEvent, TranscriptionResult,
};
use crate::resampler::Pcm16MonoResampler;

use super::lazy::LazyEngine;
use super::text::{capitalize_first_letter, parakeet_gain};

pub const DEFAULT_SILENCE_PEAK_THRESHOLD: i32 = 300;
pub const DEFAULT_MIN_DECODE_INTERVAL_MS: u64 = 350;

/// An offline recognizer: one decode call per utterance.
pub trait OfflineEngine: Send + Sync + 'static {
    fn sample_rate(&self) -> u32;
    /// Undoes model quirks (some models emit upper-cased text).
    fn normalize_text(&self, text: &str) -> String;
    /// Transcribes `samples` (already at some `sample_rate`), returning trimmed raw text.
    fn recognize(&self, samples: &[f32], sample_rate: u32) -> Result<String>;
}

fn transcribe_pcm<E: OfflineEngine>(
    engine: &E,
    pcm16: &[u8],
    input_rate: u32,
    silence_peak_threshold: i32,
) -> Result<TranscriptionResult> {
    let started = Instant::now();
    if pcm16le_peak_abs(pcm16)? < silence_peak_threshold {
        return Ok(TranscriptionResult {
            duration: Some(started.elapsed().as_millis() as f64),
            is_low_confidence: Some(true),
            ..Default::default()
        });
    }
    let model_rate = engine.sample_rate();
    let for_model = if input_rate != model_rate {
        Pcm16MonoResampler::new(input_rate, model_rate).process_chunk(pcm16)?
    } else {
        pcm16.to_vec()
    };
    let gain = parakeet_gain(pcm16le_peak_abs(&for_model)?);
    let samples = pcm16le_to_float32(&for_model, gain)?;
    let text = engine.normalize_text(engine.recognize(&samples, model_rate)?.trim());
    let duration = started.elapsed().as_millis() as f64;
    Ok(TranscriptionResult {
        is_low_confidence: text.is_empty().then_some(true),
        text,
        duration: Some(duration),
        ..Default::default()
    })
}

pub struct ParakeetStt<E: OfflineEngine> {
    engine: Arc<LazyEngine<E>>,
    silence_peak_threshold: i32,
}

impl<E: OfflineEngine> ParakeetStt<E> {
    pub fn new(engine: Arc<LazyEngine<E>>) -> Self {
        Self {
            engine,
            silence_peak_threshold: DEFAULT_SILENCE_PEAK_THRESHOLD,
        }
    }

    pub fn with_silence_peak_threshold(mut self, threshold: i32) -> Self {
        self.silence_peak_threshold = threshold;
        self
    }

    pub fn engine(&self) -> Arc<LazyEngine<E>> {
        self.engine.clone()
    }

    /// Single-shot transcription of `audio/wav` or `audio/pcm;rate=N` bytes.
    pub async fn transcribe_audio(
        &self,
        audio: &[u8],
        format: &str,
    ) -> Result<TranscriptionResult> {
        transcribe_audio_with(&self.engine, self.silence_peak_threshold, audio, format).await
    }
}

async fn transcribe_audio_with<E: OfflineEngine>(
    engine: &Arc<LazyEngine<E>>,
    threshold: i32,
    audio: &[u8],
    format: &str,
) -> Result<TranscriptionResult> {
    let lower = format.to_lowercase();
    let engine = engine.get().await?;
    let (rate, pcm16): (u32, Vec<u8>) = if lower.contains("audio/wav") {
        let parsed = parse_pcm16_mono_wav(audio)?;
        (parsed.sample_rate, parsed.pcm16)
    } else if lower.contains("audio/pcm") {
        let rate = parse_pcm_rate_from_format(format, Some(engine.sample_rate()))
            .unwrap_or(engine.sample_rate());
        (rate, audio.to_vec())
    } else {
        return Err(SpeechError::msg(format!(
            "Unsupported audio format for sherpa Parakeet STT: {format}"
        )));
    };
    tokio::task::spawn_blocking(move || transcribe_pcm(engine.as_ref(), &pcm16, rate, threshold))
        .await
        .map_err(|e| SpeechError::msg(e.to_string()))?
}

// ---- batch session --------------------------------------------------------------

struct BatchState {
    connected: bool,
    segment_id: String,
    previous_segment_id: Option<String>,
    pcm16: Vec<u8>,
}

struct BatchSession<E: OfflineEngine> {
    engine: Arc<LazyEngine<E>>,
    threshold: i32,
    rate: u32,
    events: SttEventSender,
    state: Mutex<BatchState>,
}

#[async_trait]
impl<E: OfflineEngine> SttSession for BatchSession<E> {
    fn required_sample_rate(&self) -> u32 {
        self.rate
    }

    async fn connect(&self) -> Result<()> {
        // Load the model now so the first commit is not the one that pays for it.
        self.engine.get().await?;
        self.state.lock().unwrap().connected = true;
        Ok(())
    }

    fn append_pcm16(&self, chunk: &[u8]) {
        let mut st = self.state.lock().unwrap();
        if !st.connected {
            drop(st);
            let _ = self
                .events
                .send(SttEvent::Error("STT session not connected".into()));
            return;
        }
        st.pcm16.extend_from_slice(chunk);
    }

    fn commit(&self) -> Result<()> {
        // Snapshot ids and audio before the async work starts, so audio appended while this
        // segment transcribes belongs to the next one.
        let (segment_id, previous, pcm) = {
            let mut st = self.state.lock().unwrap();
            if !st.connected {
                drop(st);
                let _ = self
                    .events
                    .send(SttEvent::Error("STT session not connected".into()));
                return Ok(());
            }
            let id = std::mem::replace(&mut st.segment_id, uuid::Uuid::new_v4().to_string());
            let previous = st.previous_segment_id.replace(id.clone());
            (id, previous, std::mem::take(&mut st.pcm16))
        };
        let _ = self.events.send(SttEvent::Committed(CommittedEvent {
            segment_id: segment_id.clone(),
            previous_segment_id: previous,
        }));

        let engine = self.engine.clone();
        let events = self.events.clone();
        let threshold = self.threshold;
        let rate = self.rate;
        tokio::spawn(async move {
            let format = format!("audio/pcm;rate={rate}");
            match transcribe_audio_with(&engine, threshold, &pcm, &format).await {
                Ok(result) => {
                    let _ = events.send(SttEvent::Transcript(TranscriptEvent {
                        segment_id,
                        transcript: result.text,
                        is_final: true,
                        language: result.language,
                        logprobs: result.logprobs,
                        avg_logprob: result.avg_logprob,
                        is_low_confidence: result.is_low_confidence,
                    }));
                }
                Err(err) => {
                    let _ = events.send(SttEvent::Error(err.to_string()));
                }
            }
        });
        Ok(())
    }

    fn clear(&self) {
        let mut st = self.state.lock().unwrap();
        st.pcm16.clear();
        st.segment_id = uuid::Uuid::new_v4().to_string();
    }

    fn close(&self) -> Result<()> {
        let mut st = self.state.lock().unwrap();
        st.connected = false;
        st.pcm16.clear();
        Ok(())
    }
}

impl<E: OfflineEngine> SttProvider for ParakeetStt<E> {
    fn id(&self) -> &str {
        "local"
    }

    fn create_session(
        &self,
        _params: &SttSessionParams,
    ) -> Result<(Arc<dyn SttSession>, SttEventReceiver)> {
        let (tx, rx) = stt_event_channel();
        let session = BatchSession {
            engine: self.engine.clone(),
            threshold: self.silence_peak_threshold,
            rate: 16000,
            events: tx,
            state: Mutex::new(BatchState {
                connected: false,
                segment_id: uuid::Uuid::new_v4().to_string(),
                previous_segment_id: None,
                pcm16: Vec::new(),
            }),
        };
        Ok((Arc::new(session), rx))
    }
}

// ---- realtime (partial-decoding) session ------------------------------------------

struct RealtimeState {
    connected: bool,
    current_segment_id: Option<String>,
    previous_segment_id: Option<String>,
    last_partial_text: String,
    pcm16: Vec<u8>,
    last_decode_at: Option<Instant>,
    pending_decode: bool,
}

struct RealtimeShared<E: OfflineEngine> {
    engine: Arc<LazyEngine<E>>,
    events: SttEventSender,
    min_decode_interval: Duration,
    state: Mutex<RealtimeState>,
    /// Serialises decodes; a commit waits here for an in-flight partial decode.
    decode_lock: tokio::sync::Mutex<()>,
}

pub struct ParakeetRealtimeSession<E: OfflineEngine> {
    shared: Arc<RealtimeShared<E>>,
}

impl<E: OfflineEngine> ParakeetRealtimeSession<E> {
    pub fn new(
        engine: Arc<LazyEngine<E>>,
        min_decode_interval: Option<Duration>,
    ) -> (Self, SttEventReceiver) {
        let (tx, rx) = stt_event_channel();
        let shared = Arc::new(RealtimeShared {
            engine,
            events: tx,
            min_decode_interval: min_decode_interval
                .unwrap_or(Duration::from_millis(DEFAULT_MIN_DECODE_INTERVAL_MS)),
            state: Mutex::new(RealtimeState {
                connected: false,
                current_segment_id: None,
                previous_segment_id: None,
                last_partial_text: String::new(),
                pcm16: Vec::new(),
                last_decode_at: None,
                pending_decode: false,
            }),
            decode_lock: tokio::sync::Mutex::new(()),
        });
        (Self { shared }, rx)
    }
}

impl<E: OfflineEngine> RealtimeShared<E> {
    fn emit(&self, event: SttEvent) {
        let _ = self.events.send(event);
    }

    async fn decode_pcm(&self, pcm16: Vec<u8>, is_first_segment: bool) -> Result<String> {
        if pcm16.is_empty() {
            return Ok(String::new());
        }
        let engine = self.engine.get().await?;
        tokio::task::spawn_blocking(move || -> Result<String> {
            let gain = parakeet_gain(pcm16le_peak_abs(&pcm16)?);
            let samples = pcm16le_to_float32(&pcm16, gain)?;
            let text =
                engine.normalize_text(engine.recognize(&samples, engine.sample_rate())?.trim());
            // Later segments continue the same dictation, so only the first one starts a sentence.
            Ok(if is_first_segment {
                capitalize_first_letter(&text)
            } else {
                text
            })
        })
        .await
        .map_err(|e| SpeechError::msg(e.to_string()))?
    }

    /// Re-decodes the current buffer and emits a partial when the text changed.
    async fn maybe_decode_partial(self: &Arc<Self>) {
        loop {
            {
                let st = self.state.lock().unwrap();
                if !st.connected || st.current_segment_id.is_none() {
                    return;
                }
                if let Some(last) = st.last_decode_at {
                    if last.elapsed() < self.min_decode_interval {
                        return;
                    }
                }
            }
            let Ok(_guard) = self.decode_lock.try_lock() else {
                self.state.lock().unwrap().pending_decode = true;
                return;
            };
            let (segment_id, pcm, first) = {
                let st = self.state.lock().unwrap();
                match &st.current_segment_id {
                    Some(id) if st.connected => (
                        id.clone(),
                        st.pcm16.clone(),
                        st.previous_segment_id.is_none(),
                    ),
                    _ => return,
                }
            };
            let result = self.decode_pcm(pcm, first).await;
            let rerun = {
                let mut st = self.state.lock().unwrap();
                st.last_decode_at = Some(Instant::now());
                match result {
                    Ok(text) => {
                        // A commit may have moved on while we decoded; its final supersedes this partial.
                        if st.connected
                            && st.current_segment_id.as_deref() == Some(segment_id.as_str())
                            && text != st.last_partial_text
                        {
                            st.last_partial_text = text.clone();
                            self.emit(SttEvent::Transcript(TranscriptEvent::partial(
                                segment_id, text,
                            )));
                        }
                    }
                    Err(err) => self.emit(SttEvent::Error(err.to_string())),
                }
                std::mem::take(&mut st.pending_decode)
            };
            drop(_guard);
            if !rerun {
                return;
            }
            // A chunk arrived while decoding: go again with the newer buffer, ignoring the interval.
            self.state.lock().unwrap().last_decode_at = None;
        }
    }
}

#[async_trait]
impl<E: OfflineEngine> SttSession for ParakeetRealtimeSession<E> {
    fn required_sample_rate(&self) -> u32 {
        16000
    }

    async fn connect(&self) -> Result<()> {
        self.shared.engine.get().await?;
        let mut st = self.shared.state.lock().unwrap();
        if st.connected {
            return Ok(());
        }
        st.current_segment_id = Some(uuid::Uuid::new_v4().to_string());
        st.connected = true;
        Ok(())
    }

    fn append_pcm16(&self, chunk: &[u8]) {
        {
            let mut st = self.shared.state.lock().unwrap();
            if !st.connected || st.current_segment_id.is_none() {
                drop(st);
                self.shared.emit(SttEvent::Error(
                    "Parakeet realtime session not connected".into(),
                ));
                return;
            }
            st.pcm16.extend_from_slice(chunk);
        }
        let shared = self.shared.clone();
        tokio::spawn(async move { shared.maybe_decode_partial().await });
    }

    fn commit(&self) -> Result<()> {
        // Snapshot the segment and advance now; the final is decoded from the snapshot after any
        // in-flight partial decode finishes (the TS version could emit a stale partial as the final).
        let (segment_id, previous, pcm) = {
            let mut st = self.shared.state.lock().unwrap();
            let Some(current) = st.current_segment_id.clone().filter(|_| st.connected) else {
                drop(st);
                self.shared.emit(SttEvent::Error(
                    "Parakeet realtime session not connected".into(),
                ));
                return Ok(());
            };
            let previous = st.previous_segment_id.replace(current.clone());
            st.current_segment_id = Some(uuid::Uuid::new_v4().to_string());
            st.last_partial_text.clear();
            (current, previous, std::mem::take(&mut st.pcm16))
        };
        let shared = self.shared.clone();
        tokio::spawn(async move {
            let _guard = shared.decode_lock.lock().await;
            let first = previous.is_none();
            match shared.decode_pcm(pcm, first).await {
                Ok(text) => {
                    shared.emit(SttEvent::Committed(CommittedEvent {
                        segment_id: segment_id.clone(),
                        previous_segment_id: previous,
                    }));
                    shared.emit(SttEvent::Transcript(TranscriptEvent::final_(
                        segment_id, text,
                    )));
                }
                Err(err) => shared.emit(SttEvent::Error(err.to_string())),
            }
        });
        Ok(())
    }

    fn clear(&self) {
        let mut st = self.shared.state.lock().unwrap();
        if !st.connected {
            return;
        }
        st.pcm16.clear();
        st.current_segment_id = Some(uuid::Uuid::new_v4().to_string());
        st.last_partial_text.clear();
    }

    fn close(&self) -> Result<()> {
        let mut st = self.shared.state.lock().unwrap();
        st.connected = false;
        st.current_segment_id = None;
        st.pcm16.clear();
        Ok(())
    }
}

/// Provider that hands out realtime (partial-decoding) sessions: dictation on a non-Nemotron model.
pub struct ParakeetRealtimeStt<E: OfflineEngine> {
    engine: Arc<LazyEngine<E>>,
    min_decode_interval: Option<Duration>,
}

impl<E: OfflineEngine> ParakeetRealtimeStt<E> {
    pub fn new(engine: Arc<LazyEngine<E>>) -> Self {
        Self {
            engine,
            min_decode_interval: None,
        }
    }
    pub fn with_min_decode_interval(mut self, interval: Duration) -> Self {
        self.min_decode_interval = Some(interval);
        self
    }
}

impl<E: OfflineEngine> SttProvider for ParakeetRealtimeStt<E> {
    fn id(&self) -> &str {
        "local"
    }
    fn create_session(
        &self,
        _params: &SttSessionParams,
    ) -> Result<(Arc<dyn SttSession>, SttEventReceiver)> {
        let (session, rx) =
            ParakeetRealtimeSession::new(self.engine.clone(), self.min_decode_interval);
        Ok((Arc::new(session), rx))
    }
}
