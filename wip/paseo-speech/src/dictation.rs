//! Streaming dictation (port of `dictation/dictation-stream-manager.ts`).
//!
//! Wire shapes match `packages/protocol` (`dictation_stream_*` messages): inbound
//! start/chunk/finish/cancel arrive through [`DictationStreamManager::handle_inbound`]
//! (or the four typed methods) and outbound messages are pushed to the channel
//! given at construction.
//!
//! All state lives behind one mutex that is never held across an `.await`.
//! STT sessions report through a channel that a per-stream task drains, so a
//! session can emit from inside `commit()` without re-entering the manager.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::audio::{parse_pcm_rate_from_format, pcm16le_peak_abs, pcm_to_wav};
use crate::dictation_debug::{persist_debug_audio, DebugChunkWriter, DictationDebug};
use crate::error::{Result, SpeechError};
use crate::provider::{SttEvent, SttResolver, SttSession, SttSessionParams};
use crate::resampler::Pcm16MonoResampler;

const PCM_CHANNELS: u32 = 1;
const PCM_BITS_PER_SAMPLE: u32 = 16;
pub const DEFAULT_DICTATION_FINAL_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_DICTATION_AUTO_COMMIT_SECONDS: f64 = 15.0;
const FINAL_TIMEOUT_MAX_MS: u64 = 5 * 60 * 1000;
const FINAL_TIMEOUT_PER_PENDING_SEGMENT_MS: u64 = 15 * 1000;
const FINAL_TIMEOUT_PER_PENDING_AUDIO_SECOND_MS: u64 = 1500;
const FINAL_TIMEOUT_PER_MISSING_SEQ_MS: u64 = 250;
const DEFAULT_SILENCE_PEAK_THRESHOLD: i32 = 300;
const DEFAULT_TRANSCRIPTION_PROMPT: &str = "Transcribe only what the speaker says. Do not add words. Preserve punctuation and casing. If the audio is silence or non-speech noise, return an empty transcript.";

fn silence_peak_threshold() -> i32 {
    static THRESHOLD: OnceLock<i32> = OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        std::env::var("PASEO_DICTATION_SILENCE_PEAK_THRESHOLD")
            .ok()
            .and_then(|v| v.trim().parse::<i32>().ok())
            .unwrap_or(DEFAULT_SILENCE_PEAK_THRESHOLD)
    })
}

fn parse_non_negative_number(value: Option<String>) -> Option<f64> {
    let parsed = value?.trim().parse::<f64>().ok()?;
    (parsed.is_finite() && parsed >= 0.0).then_some(parsed)
}

// ---------------------------------------------------------------------------
// Wire messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AckPayload {
    pub dictation_id: String,
    pub ack_seq: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinishAcceptedPayload {
    pub dictation_id: String,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartialPayload {
    pub dictation_id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FinalPayload {
    pub dictation_id: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_recording_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorPayload {
    pub dictation_id: String,
    pub error: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_model_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub debug_recording_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActivityLogPayload {
    pub id: String,
    /// RFC 3339, the same text `JSON.stringify(new Date())` produces.
    pub timestamp: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub content: String,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum DictationOutbound {
    DictationStreamAck(AckPayload),
    DictationStreamFinishAccepted(FinishAcceptedPayload),
    DictationStreamPartial(PartialPayload),
    DictationStreamFinal(FinalPayload),
    DictationStreamError(ErrorPayload),
    ActivityLog(ActivityLogPayload),
}

impl DictationOutbound {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::DictationStreamAck(_) => "dictation_stream_ack",
            Self::DictationStreamFinishAccepted(_) => "dictation_stream_finish_accepted",
            Self::DictationStreamPartial(_) => "dictation_stream_partial",
            Self::DictationStreamFinal(_) => "dictation_stream_final",
            Self::DictationStreamError(_) => "dictation_stream_error",
            Self::ActivityLog(_) => "activity_log",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DictationInbound {
    DictationStreamStart {
        #[serde(rename = "dictationId")]
        dictation_id: String,
        format: String,
    },
    DictationStreamChunk {
        #[serde(rename = "dictationId")]
        dictation_id: String,
        seq: u64,
        audio: String,
        format: String,
    },
    DictationStreamFinish {
        #[serde(rename = "dictationId")]
        dictation_id: String,
        #[serde(rename = "finalSeq")]
        final_seq: u64,
    },
    DictationStreamCancel {
        #[serde(rename = "dictationId")]
        dictation_id: String,
    },
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

pub struct DictationStreamManagerParams {
    pub emit: mpsc::UnboundedSender<DictationOutbound>,
    pub session_id: String,
    pub stt: SttResolver,
    /// Defaults to `"en"`.
    pub language: Option<String>,
    pub final_timeout_ms: Option<u64>,
    /// `0` disables auto-commit. Falls back to `PASEO_DICTATION_AUTO_COMMIT_SECONDS`, then 15.
    pub auto_commit_seconds: Option<f64>,
    pub on_idle: Option<Arc<dyn Fn() + Send + Sync>>,
    pub debug: DictationDebug,
}

impl DictationStreamManagerParams {
    pub fn new(
        emit: mpsc::UnboundedSender<DictationOutbound>,
        session_id: impl Into<String>,
        stt: impl Into<SttResolver>,
    ) -> Self {
        Self {
            emit,
            session_id: session_id.into(),
            stt: stt.into(),
            language: None,
            final_timeout_ms: None,
            auto_commit_seconds: None,
            on_idle: None,
            debug: DictationDebug::FromEnv,
        }
    }
}

/// Insertion-ordered string map (JS `Map` iteration order matters for segment assembly).
#[derive(Default)]
struct OrderedMap(Vec<(String, String)>);

impl OrderedMap {
    fn set(&mut self, key: &str, value: String) {
        match self.0.iter_mut().find(|(k, _)| k == key) {
            Some(entry) => entry.1 = value,
            None => self.0.push((key.to_string(), value)),
        }
    }
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }
    fn retain_keys(&mut self, mut keep: impl FnMut(&str) -> bool) -> usize {
        let before = self.0.len();
        self.0.retain(|(k, _)| keep(k));
        before - self.0.len()
    }
}

struct StreamState {
    dictation_id: String,
    session_id: String,
    input_format: String,
    stt: Arc<dyn SttSession>,
    output_rate: u32,
    resampler: Option<Pcm16MonoResampler>,
    debug_audio_chunks: Vec<Vec<u8>>,
    debug_recording_path: Option<String>,
    debug_chunk_writer: Option<DebugChunkWriter>,
    received_chunks: HashMap<i64, Vec<u8>>,
    next_seq_to_forward: i64,
    ack_seq: i64,
    auto_commit_bytes: usize,
    bytes_since_commit: usize,
    peak_since_commit: i32,
    committed_segment_ids: Vec<String>,
    transcripts_by_segment_id: OrderedMap,
    final_transcript_segment_ids: HashSet<String>,
    in_flight_commit_count: u32,
    awaiting_final_commit: bool,
    finish_requested: bool,
    finish_sealed: bool,
    final_seq: Option<i64>,
    final_timeout: Option<JoinHandle<()>>,
    event_task: Option<JoinHandle<()>>,
}

type Streams = HashMap<String, StreamState>;

struct TimeoutEstimate {
    timeout_ms: u64,
    pending_segments: u64,
    pending_audio_seconds: u64,
    missing_seq_count: u64,
}

struct Inner {
    emit: mpsc::UnboundedSender<DictationOutbound>,
    session_id: String,
    resolve_stt: SttResolver,
    language: String,
    final_timeout_ms: u64,
    auto_commit_seconds: f64,
    on_idle: Option<Arc<dyn Fn() + Send + Sync>>,
    debug: DictationDebug,
    streams: Mutex<Streams>,
    weak_self: Weak<Inner>,
}

#[derive(Clone)]
pub struct DictationStreamManager {
    inner: Arc<Inner>,
}

impl DictationStreamManager {
    pub fn new(params: DictationStreamManagerParams) -> Self {
        let auto_commit_seconds = params
            .auto_commit_seconds
            .or_else(|| {
                parse_non_negative_number(std::env::var("PASEO_DICTATION_AUTO_COMMIT_SECONDS").ok())
            })
            .unwrap_or(DEFAULT_DICTATION_AUTO_COMMIT_SECONDS);
        let inner = Arc::new_cyclic(|weak| Inner {
            emit: params.emit,
            session_id: params.session_id,
            resolve_stt: params.stt,
            language: params.language.unwrap_or_else(|| "en".to_string()),
            final_timeout_ms: params
                .final_timeout_ms
                .unwrap_or(DEFAULT_DICTATION_FINAL_TIMEOUT_MS),
            auto_commit_seconds,
            on_idle: params.on_idle,
            debug: params.debug,
            streams: Mutex::new(HashMap::new()),
            weak_self: weak.clone(),
        });
        Self { inner }
    }

    /// True while at least one dictation stream is open.
    pub fn has_demand(&self) -> bool {
        !self.inner.lock().is_empty()
    }

    /// Closes every stream; keeps going when one provider's close fails and reports all failures.
    pub fn cleanup_all(&self) -> Result<()> {
        let mut fx = Vec::new();
        {
            let mut streams = self.inner.lock();
            let ids: Vec<String> = streams.keys().cloned().collect();
            for id in ids {
                self.inner.remove_stream(&mut streams, &id, &mut fx);
            }
        }
        let failures = self.inner.run_effects(fx);
        if failures.is_empty() {
            return Ok(());
        }
        Err(SpeechError::msg(format!(
            "Dictation cleanup failed: {}",
            failures
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        )))
    }

    pub async fn handle_inbound(&self, message: DictationInbound) -> Result<()> {
        match message {
            DictationInbound::DictationStreamStart {
                dictation_id,
                format,
            } => {
                self.handle_start(&dictation_id, &format).await;
            }
            DictationInbound::DictationStreamChunk {
                dictation_id,
                seq,
                audio,
                format,
            } => {
                self.handle_chunk(&dictation_id, seq as i64, &audio, &format);
            }
            DictationInbound::DictationStreamFinish {
                dictation_id,
                final_seq,
            } => {
                self.handle_finish(&dictation_id, final_seq as i64);
            }
            DictationInbound::DictationStreamCancel { dictation_id } => {
                self.handle_cancel(&dictation_id)?;
            }
        }
        Ok(())
    }

    pub async fn handle_start(&self, dictation_id: &str, format: &str) {
        self.inner.clone().start(dictation_id, format).await;
    }

    pub fn handle_chunk(&self, dictation_id: &str, seq: i64, audio_base64: &str, format: &str) {
        self.inner.chunk(dictation_id, seq, audio_base64, format);
    }

    pub fn handle_finish(&self, dictation_id: &str, final_seq: i64) {
        self.inner.finish(dictation_id, final_seq);
    }

    pub fn handle_cancel(&self, dictation_id: &str) -> Result<()> {
        let mut fx = Vec::new();
        {
            let mut streams = self.inner.lock();
            self.inner
                .remove_stream(&mut streams, dictation_id, &mut fx);
        }
        match self.inner.run_effects(fx).into_iter().next() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

fn decode_base64_lenient(input: &str) -> Vec<u8> {
    // Node's Buffer.from(_, "base64") skips characters outside the alphabet and tolerates
    // missing padding.
    let cleaned: String = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '-' | '_'))
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    let trimmed = &cleaned[..cleaned.len() - (cleaned.len() % 4 == 1) as usize];
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(trimmed)
        .unwrap_or_default()
}

fn is_buffer_too_small_error(message: &str) -> bool {
    message.to_ascii_lowercase().contains("buffer too small")
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, Streams> {
        self.streams.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn send(&self, message: DictationOutbound) {
        let _ = self.emit.send(message);
    }

    fn emit_ack(&self, dictation_id: &str, ack_seq: i64) {
        self.send(DictationOutbound::DictationStreamAck(AckPayload {
            dictation_id: dictation_id.to_string(),
            ack_seq,
        }));
    }

    fn fail_stream(&self, dictation_id: &str, error: &str, retryable: bool) {
        self.send(DictationOutbound::DictationStreamError(ErrorPayload {
            dictation_id: dictation_id.to_string(),
            error: error.to_string(),
            retryable,
            reason_code: None,
            missing_model_ids: None,
            debug_recording_path: None,
        }));
    }

    // -- teardown ----------------------------------------------------------

    /// Removes a stream and stops its tasks. The provider session is returned in `fx` so the
    /// caller can close it once the lock is released.
    fn remove_stream(
        &self,
        streams: &mut Streams,
        dictation_id: &str,
        fx: &mut Vec<Arc<dyn SttSession>>,
    ) {
        let Some(mut state) = streams.remove(dictation_id) else {
            return;
        };
        if let Some(timer) = state.final_timeout.take() {
            timer.abort();
        }
        if let Some(task) = state.event_task.take() {
            task.abort();
        }
        fx.push(state.stt.clone());
    }

    /// Closes removed sessions (all of them, even if one fails), then reports idleness.
    fn run_effects(&self, sessions: Vec<Arc<dyn SttSession>>) -> Vec<SpeechError> {
        if sessions.is_empty() {
            return Vec::new();
        }
        let mut failures = Vec::new();
        for session in sessions {
            if let Err(err) = session.close() {
                failures.push(err);
            }
        }
        if self.lock().is_empty() {
            if let Some(on_idle) = &self.on_idle {
                on_idle();
            }
        }
        failures
    }

    fn cleanup(&self, dictation_id: &str) {
        let mut fx = Vec::new();
        {
            let mut streams = self.lock();
            self.remove_stream(&mut streams, dictation_id, &mut fx);
        }
        for failure in self.run_effects(fx) {
            tracing::warn!(%dictation_id, error = %failure, "dictation provider close failed");
        }
    }

    fn persist_debug_audio(&self, state: &mut StreamState) -> Option<String> {
        if !self.debug.is_enabled() {
            return None;
        }
        if state.debug_recording_path.is_some() {
            return state.debug_recording_path.clone();
        }
        if state.debug_audio_chunks.is_empty() {
            return None;
        }
        let pcm: Vec<u8> = state.debug_audio_chunks.concat();
        let wav = pcm_to_wav(
            &pcm,
            state.output_rate,
            PCM_CHANNELS as u16,
            PCM_BITS_PER_SAMPLE as u16,
        );
        let folder = state.debug_chunk_writer.as_ref().map(|w| w.folder.clone());
        match persist_debug_audio(
            &self.debug,
            &wav,
            &state.session_id,
            &state.dictation_id,
            "audio/wav",
            folder.as_deref(),
        ) {
            Ok(path) => {
                state.debug_recording_path = path.map(|p| p.to_string_lossy().into_owned());
                state.debug_recording_path.clone()
            }
            Err(err) => {
                tracing::warn!(error = %err, "failed to persist dictation debug audio");
                None
            }
        }
    }

    fn emit_activity_log_for_recording(&self, dictation_id: &str, path: &str) {
        self.send(DictationOutbound::ActivityLog(ActivityLogPayload {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: jiff::Timestamp::now().to_string(),
            kind: "system",
            content: format!("Saved dictation audio: {path}"),
            metadata: json!({ "recordingPath": path, "dictationId": dictation_id }),
        }));
    }

    fn fail_and_cleanup(
        &self,
        streams: &mut Streams,
        dictation_id: &str,
        error: &str,
        retryable: bool,
        fx: &mut Vec<Arc<dyn SttSession>>,
    ) {
        let Some(state) = streams.get_mut(dictation_id) else {
            return;
        };
        let debug_path = self.persist_debug_audio(state);
        self.send(DictationOutbound::DictationStreamError(ErrorPayload {
            dictation_id: dictation_id.to_string(),
            error: error.to_string(),
            retryable,
            reason_code: None,
            missing_model_ids: None,
            debug_recording_path: debug_path.clone(),
        }));
        if let Some(path) = debug_path {
            self.emit_activity_log_for_recording(dictation_id, &path);
        }
        self.remove_stream(streams, dictation_id, fx);
    }

    fn fail_and_cleanup_locked(&self, dictation_id: &str, error: &str, retryable: bool) {
        let mut fx = Vec::new();
        {
            let mut streams = self.lock();
            self.fail_and_cleanup(&mut streams, dictation_id, error, retryable, &mut fx);
        }
        for failure in self.run_effects(fx) {
            tracing::warn!(%dictation_id, error = %failure, "dictation provider close failed");
        }
    }

    // -- start -------------------------------------------------------------

    async fn start(self: Arc<Self>, dictation_id: &str, format: &str) {
        self.cleanup(dictation_id);

        let Some(provider) = self.resolve_stt.resolve() else {
            self.fail_stream(dictation_id, "Dictation STT not configured", false);
            return;
        };
        let prompt = std::env::var("PASEO_DICTATION_TRANSCRIPTION_PROMPT")
            .unwrap_or_else(|_| DEFAULT_TRANSCRIPTION_PROMPT.to_string());
        let (stt, events) = match provider.create_session(&SttSessionParams {
            language: Some(self.language.clone()),
            prompt: Some(prompt),
        }) {
            Ok(created) => created,
            Err(err) => {
                self.fail_stream(dictation_id, &err.to_string(), false);
                return;
            }
        };

        let input_rate = parse_pcm_rate_from_format(format, Some(16000)).unwrap_or(16000);
        if input_rate == 0 {
            self.fail_stream(
                dictation_id,
                &format!("Invalid dictation input rate in format: {format}"),
                false,
            );
            let _ = stt.close();
            return;
        }

        let debug_chunk_writer =
            DebugChunkWriter::create(&self.debug, &self.session_id, dictation_id);
        let output_rate = stt.required_sample_rate();
        let auto_commit_bytes = if self.auto_commit_seconds > 0.0 {
            ((self.auto_commit_seconds * output_rate as f64 * 2.0).round() as usize).max(1)
        } else {
            0
        };

        let event_task = {
            let weak = self.weak_self.clone();
            let id = dictation_id.to_string();
            let stt = stt.clone();
            let mut events = events;
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    let Some(inner) = weak.upgrade() else { break };
                    inner.on_stt_event(&id, &stt, event);
                }
            })
        };

        {
            let mut streams = self.lock();
            streams.insert(
                dictation_id.to_string(),
                StreamState {
                    dictation_id: dictation_id.to_string(),
                    session_id: self.session_id.clone(),
                    input_format: format.to_string(),
                    stt: stt.clone(),
                    output_rate,
                    resampler: (input_rate != output_rate)
                        .then(|| Pcm16MonoResampler::new(input_rate, output_rate)),
                    debug_audio_chunks: Vec::new(),
                    debug_recording_path: None,
                    debug_chunk_writer,
                    received_chunks: HashMap::new(),
                    next_seq_to_forward: 0,
                    ack_seq: -1,
                    auto_commit_bytes,
                    bytes_since_commit: 0,
                    peak_since_commit: 0,
                    committed_segment_ids: Vec::new(),
                    transcripts_by_segment_id: OrderedMap::default(),
                    final_transcript_segment_ids: HashSet::new(),
                    in_flight_commit_count: 0,
                    awaiting_final_commit: false,
                    finish_requested: false,
                    finish_sealed: false,
                    final_seq: None,
                    final_timeout: None,
                    event_task: Some(event_task),
                },
            );
        }

        let connected = stt.connect().await;
        if !self.is_current(dictation_id, &stt) {
            return;
        }
        if let Err(err) = connected {
            self.fail_stream(dictation_id, &err.to_string(), true);
            self.cleanup(dictation_id);
            return;
        }
        self.emit_ack(dictation_id, -1);
    }

    fn is_current(&self, dictation_id: &str, stt: &Arc<dyn SttSession>) -> bool {
        self.lock()
            .get(dictation_id)
            .is_some_and(|s| Arc::ptr_eq(&s.stt, stt))
    }

    // -- provider events ---------------------------------------------------

    fn on_stt_event(&self, dictation_id: &str, stt: &Arc<dyn SttSession>, event: SttEvent) {
        let mut fx = Vec::new();
        {
            let mut streams = self.lock();
            let current = streams
                .get(dictation_id)
                .is_some_and(|s| Arc::ptr_eq(&s.stt, stt));
            if !current {
                return;
            }
            match event {
                SttEvent::Committed(committed) => {
                    let state = streams.get_mut(dictation_id).expect("checked above");
                    if state.in_flight_commit_count > 0 {
                        state.in_flight_commit_count -= 1;
                    }
                    state.committed_segment_ids.push(committed.segment_id);
                    if state.finish_requested && state.awaiting_final_commit {
                        state.awaiting_final_commit = false;
                    }
                    self.maybe_finalize(&mut streams, dictation_id, &mut fx);
                }
                SttEvent::Transcript(transcript) => {
                    let state = streams.get_mut(dictation_id).expect("checked above");
                    state
                        .transcripts_by_segment_id
                        .set(&transcript.segment_id, transcript.transcript.clone());
                    if transcript.is_final {
                        state
                            .final_transcript_segment_ids
                            .insert(transcript.segment_id.clone());
                    }
                    if state.finish_requested && state.awaiting_final_commit && transcript.is_final
                    {
                        state.awaiting_final_commit = false;
                    }
                    let mut ordered: Vec<&str> = state
                        .committed_segment_ids
                        .iter()
                        .map(String::as_str)
                        .collect();
                    if !state.committed_segment_ids.contains(&transcript.segment_id) {
                        ordered.push(&transcript.segment_id);
                    }
                    let partial_text = ordered
                        .iter()
                        .map(|id| state.transcripts_by_segment_id.get(id).unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join(" ")
                        .trim()
                        .to_string();
                    self.send(DictationOutbound::DictationStreamPartial(PartialPayload {
                        dictation_id: dictation_id.to_string(),
                        text: partial_text,
                    }));
                    self.maybe_seal_finish(&mut streams, dictation_id, &mut fx);
                    self.maybe_finalize(&mut streams, dictation_id, &mut fx);
                }
                SttEvent::Error(message) => {
                    let state = streams.get_mut(dictation_id).expect("checked above");
                    if state.finish_requested && is_buffer_too_small_error(&message) {
                        if state.in_flight_commit_count > 0 {
                            state.in_flight_commit_count -= 1;
                        }
                        if state.awaiting_final_commit {
                            state.awaiting_final_commit = false;
                        }
                        self.maybe_finalize(&mut streams, dictation_id, &mut fx);
                    } else {
                        self.fail_and_cleanup(&mut streams, dictation_id, &message, true, &mut fx);
                    }
                }
            }
        }
        self.finish_effects(dictation_id, fx);
    }

    fn finish_effects(&self, dictation_id: &str, fx: Vec<Arc<dyn SttSession>>) {
        for failure in self.run_effects(fx) {
            tracing::warn!(%dictation_id, error = %failure, "dictation provider close failed");
        }
    }

    // -- chunks ------------------------------------------------------------

    fn chunk(&self, dictation_id: &str, seq: i64, audio_base64: &str, format: &str) {
        let mut fx = Vec::new();
        {
            let mut streams = self.lock();
            if !streams.contains_key(dictation_id) {
                drop(streams);
                self.fail_stream(dictation_id, "Dictation stream not started", true);
                return;
            }
            let state = streams.get_mut(dictation_id).expect("checked above");

            if format != state.input_format {
                self.fail_and_cleanup(
                    &mut streams,
                    dictation_id,
                    &format!("Mismatched dictation stream format: {format}"),
                    false,
                    &mut fx,
                );
                drop(streams);
                self.finish_effects(dictation_id, fx);
                return;
            }

            if seq < state.next_seq_to_forward {
                let ack = state.ack_seq;
                drop(streams);
                self.emit_ack(dictation_id, ack);
                return;
            }

            state
                .received_chunks
                .entry(seq)
                .or_insert_with(|| decode_base64_lenient(audio_base64));

            while let Some(pcm16) = {
                let next = state.next_seq_to_forward;
                state.received_chunks.remove(&next)
            } {
                let seq_now = state.next_seq_to_forward;
                let resampled = match state.resampler.as_mut() {
                    Some(resampler) => match resampler.process_chunk(&pcm16) {
                        Ok(out) => out,
                        Err(err) => {
                            let message = err.to_string();
                            self.fail_and_cleanup(
                                &mut streams,
                                dictation_id,
                                &message,
                                true,
                                &mut fx,
                            );
                            drop(streams);
                            self.finish_effects(dictation_id, fx);
                            return;
                        }
                    },
                    None => pcm16,
                };
                if !resampled.is_empty() {
                    state.stt.append_pcm16(&resampled);
                    state.bytes_since_commit += resampled.len();
                    let peak = match pcm16le_peak_abs(&resampled) {
                        Ok(peak) => peak,
                        Err(err) => {
                            let message = err.to_string();
                            self.fail_and_cleanup(
                                &mut streams,
                                dictation_id,
                                &message,
                                true,
                                &mut fx,
                            );
                            drop(streams);
                            self.finish_effects(dictation_id, fx);
                            return;
                        }
                    };
                    state.peak_since_commit = state.peak_since_commit.max(peak);
                    if let Err(err) = self.maybe_auto_commit(state) {
                        let message = err.to_string();
                        self.fail_and_cleanup(&mut streams, dictation_id, &message, true, &mut fx);
                        drop(streams);
                        self.finish_effects(dictation_id, fx);
                        return;
                    }
                    if let Some(writer) = state.debug_chunk_writer.as_mut() {
                        if let Err(err) = writer.write_chunk(seq_now, &resampled) {
                            tracing::warn!(%dictation_id, seq = seq_now, error = %err, "Failed to write debug chunk");
                        }
                    }
                    state.debug_audio_chunks.push(resampled);
                }
                state.next_seq_to_forward += 1;
                state.ack_seq = state.next_seq_to_forward - 1;
            }

            let ack = state.ack_seq;
            self.emit_ack(dictation_id, ack);
            self.maybe_seal_finish(&mut streams, dictation_id, &mut fx);
            self.maybe_finalize(&mut streams, dictation_id, &mut fx);
        }
        self.finish_effects(dictation_id, fx);
    }

    // -- finish ------------------------------------------------------------

    fn finish(&self, dictation_id: &str, final_seq: i64) {
        let mut fx = Vec::new();
        {
            let mut streams = self.lock();
            let Some(state) = streams.get_mut(dictation_id) else {
                drop(streams);
                self.fail_stream(dictation_id, "Dictation stream not started", true);
                return;
            };
            state.finish_requested = true;
            state.final_seq = Some(final_seq);

            if final_seq >= 0
                && state.ack_seq < 0
                && state.next_seq_to_forward == 0
                && state.received_chunks.is_empty()
            {
                tracing::debug!(%dictation_id, final_seq, "Dictation finish: no chunks received (failing fast)");
                self.fail_stream(
                    dictation_id,
                    &format!("Dictation finished (finalSeq={final_seq}) but no audio chunks were received"),
                    true,
                );
                self.remove_stream(&mut streams, dictation_id, &mut fx);
                drop(streams);
                self.finish_effects(dictation_id, fx);
                return;
            }

            self.maybe_seal_finish(&mut streams, dictation_id, &mut fx);
            self.maybe_finalize(&mut streams, dictation_id, &mut fx);

            if let Some(state) = streams.get_mut(dictation_id) {
                let estimate = self.estimate_finalization_timeout(state);
                if let Some(timer) = state.final_timeout.take() {
                    timer.abort();
                }
                let weak = self.weak_self.clone();
                let id = dictation_id.to_string();
                // Fix the deadline now (like `setTimeout`), not when the task is first polled.
                let deadline =
                    tokio::time::Instant::now() + Duration::from_millis(estimate.timeout_ms);
                state.final_timeout = Some(tokio::spawn(async move {
                    tokio::time::sleep_until(deadline).await;
                    if let Some(inner) = weak.upgrade() {
                        inner.fail_and_cleanup_locked(
                            &id,
                            "Timed out waiting for final transcription",
                            true,
                        );
                    }
                }));
                self.send(DictationOutbound::DictationStreamFinishAccepted(
                    FinishAcceptedPayload {
                        dictation_id: dictation_id.to_string(),
                        timeout_ms: estimate.timeout_ms,
                    },
                ));
                tracing::debug!(
                    %dictation_id,
                    final_seq,
                    ack_seq = state.ack_seq,
                    pending_segments = estimate.pending_segments,
                    pending_audio_seconds = estimate.pending_audio_seconds,
                    missing_seq_count = estimate.missing_seq_count,
                    timeout_ms = estimate.timeout_ms,
                    "Accepted dictation finish request with adaptive timeout budget"
                );
            }
        }
        self.finish_effects(dictation_id, fx);
    }

    fn estimate_finalization_timeout(&self, state: &StreamState) -> TimeoutEstimate {
        let bytes_per_second =
            (state.output_rate as u64 * PCM_CHANNELS as u64 * (PCM_BITS_PER_SAMPLE as u64 / 8))
                .max(1);
        let pending_committed = state
            .committed_segment_ids
            .iter()
            .filter(|id| !state.final_transcript_segment_ids.contains(*id))
            .count() as u64;
        let pending_segments = pending_committed + state.in_flight_commit_count as u64;
        let pending_audio_seconds = (state.bytes_since_commit as u64).div_ceil(bytes_per_second);
        let missing_seq_count = match state.final_seq {
            None => 0,
            Some(final_seq) => (final_seq - state.ack_seq).max(0) as u64,
        };
        let extra_ms = pending_segments * FINAL_TIMEOUT_PER_PENDING_SEGMENT_MS
            + pending_audio_seconds * FINAL_TIMEOUT_PER_PENDING_AUDIO_SECOND_MS
            + missing_seq_count * FINAL_TIMEOUT_PER_MISSING_SEQ_MS;
        let timeout_ms = self
            .final_timeout_ms
            .max(FINAL_TIMEOUT_MAX_MS.min(self.final_timeout_ms + extra_ms));
        TimeoutEstimate {
            timeout_ms,
            pending_segments,
            pending_audio_seconds,
            missing_seq_count,
        }
    }

    // -- commit bookkeeping --------------------------------------------------

    fn maybe_auto_commit(&self, state: &mut StreamState) -> Result<()> {
        if state.finish_requested {
            return Ok(());
        }
        if state.auto_commit_bytes == 0 || state.bytes_since_commit < state.auto_commit_bytes {
            return Ok(());
        }
        if state.peak_since_commit < silence_peak_threshold() {
            state.stt.clear();
            state.bytes_since_commit = 0;
            state.peak_since_commit = 0;
            return Ok(());
        }
        self.request_commit(state)
    }

    fn request_commit(&self, state: &mut StreamState) -> Result<()> {
        state.bytes_since_commit = 0;
        state.peak_since_commit = 0;
        state.in_flight_commit_count += 1;
        if let Err(err) = state.stt.commit() {
            state.in_flight_commit_count -= 1;
            return Err(err);
        }
        Ok(())
    }

    fn maybe_seal_finish(
        &self,
        streams: &mut Streams,
        dictation_id: &str,
        fx: &mut Vec<Arc<dyn SttSession>>,
    ) {
        let Some(state) = streams.get_mut(dictation_id) else {
            return;
        };
        let Some(final_seq) = state.final_seq else {
            return;
        };
        if !state.finish_requested || state.ack_seq < final_seq || state.finish_sealed {
            return;
        }

        if state.bytes_since_commit > 0 {
            if state.peak_since_commit < silence_peak_threshold() {
                tracing::debug!(
                    %dictation_id,
                    bytes_since_commit = state.bytes_since_commit,
                    peak_since_commit = state.peak_since_commit,
                    "Dictation finish: clearing silence-only tail (skip final commit)"
                );
                state.stt.clear();
                state.bytes_since_commit = 0;
                state.peak_since_commit = 0;
                state.awaiting_final_commit = false;
            } else {
                state.awaiting_final_commit = true;
                if let Err(err) = self.request_commit(state) {
                    let message = err.to_string();
                    self.fail_and_cleanup(streams, dictation_id, &message, true, fx);
                    return;
                }
            }
        } else {
            state.awaiting_final_commit = false;
        }
        state.finish_sealed = true;
    }

    fn maybe_finalize(
        &self,
        streams: &mut Streams,
        dictation_id: &str,
        fx: &mut Vec<Arc<dyn SttSession>>,
    ) {
        let Some(state) = streams.get_mut(dictation_id) else {
            return;
        };
        let Some(final_seq) = state.final_seq else {
            return;
        };
        if !state.finish_requested
            || state.ack_seq < final_seq
            || state.awaiting_final_commit
            || state.in_flight_commit_count > 0
        {
            return;
        }

        // Drop abandoned partials: never committed and never finalised.
        let committed: HashSet<&str> = state
            .committed_segment_ids
            .iter()
            .map(String::as_str)
            .collect();
        let finals = &state.final_transcript_segment_ids;
        let dropped = state
            .transcripts_by_segment_id
            .retain_keys(|id| committed.contains(id) || finals.contains(id));
        if dropped > 0 {
            tracing::debug!(%dictation_id, dropped_segments = dropped, "Dropped abandoned non-final dictation transcript segments before finalization");
        }

        let committed_set: HashSet<&str> = state
            .committed_segment_ids
            .iter()
            .map(String::as_str)
            .collect();
        let mut ordered: Vec<String> = state.committed_segment_ids.clone();
        for id in state.transcripts_by_segment_id.keys() {
            if !committed_set.contains(id) {
                ordered.push(id.to_string());
            }
        }

        let text = if ordered.is_empty() {
            String::new()
        } else {
            if !ordered
                .iter()
                .all(|id| state.final_transcript_segment_ids.contains(id))
            {
                return;
            }
            ordered
                .iter()
                .map(|id| state.transcripts_by_segment_id.get(id).unwrap_or(""))
                .collect::<Vec<_>>()
                .join(" ")
                .trim()
                .to_string()
        };

        let debug_path = self.persist_debug_audio(state);
        self.send(DictationOutbound::DictationStreamFinal(FinalPayload {
            dictation_id: dictation_id.to_string(),
            text,
            debug_recording_path: debug_path.clone(),
        }));
        if let Some(path) = debug_path {
            self.emit_activity_log_for_recording(dictation_id, &path);
        }
        self.remove_stream(streams, dictation_id, fx);
    }
}
