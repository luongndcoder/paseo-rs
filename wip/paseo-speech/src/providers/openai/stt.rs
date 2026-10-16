//! OpenAI-compatible REST STT (port of `providers/openai/stt.ts`).
//!
//! Request shape follows the `openai` SDK's `audio.transcriptions.create`: multipart POST to
//! `{baseUrl}/audio/transcriptions` with `file`, `model`, `language`, optional `prompt`,
//! `include[]=logprobs` (gpt-4o models only) and `response_format=json`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use reqwest::multipart::{Form, Part};
use serde_json::Value;

use crate::audio::pcm_to_wav;
use crate::error::{Result, SpeechError};
use crate::provider::{
    stt_event_channel, CommittedEvent, LogprobToken, SttEvent, SttEventReceiver, SttEventSender,
    SttProvider, SttSession, SttSessionParams, TranscriptEvent, TranscriptionResult,
};

pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const OPENAI_STT_SAMPLE_RATE: u32 = 24000;
const DEFAULT_CONFIDENCE_THRESHOLD: f64 = -3.0;

#[derive(Debug, Clone, Default)]
pub struct SttConfig {
    pub api_key: String,
    pub base_url: Option<String>,
    /// `whisper-1` (default), `gpt-4o-transcribe`, `gpt-4o-mini-transcribe` or any server-specific id.
    pub model: Option<String>,
    /// Average logprob below this marks the result low-confidence. Default -3.0.
    pub confidence_threshold: Option<f64>,
}

pub struct OpenAiStt {
    shared: Arc<Shared>,
}

struct Shared {
    client: reqwest::Client,
    config: SttConfig,
}

impl OpenAiStt {
    pub fn new(config: SttConfig) -> Self {
        Self {
            shared: Arc::new(Shared {
                client: reqwest::Client::new(),
                config,
            }),
        }
    }

    pub fn config(&self) -> &SttConfig {
        &self.shared.config
    }

    /// One transcription request for a WAV payload.
    pub async fn transcribe_wav(
        &self,
        wav: Vec<u8>,
        language: &str,
        prompt: Option<&str>,
    ) -> Result<TranscriptionResult> {
        self.shared.transcribe_wav(wav, language, prompt).await
    }
}

impl Shared {
    fn endpoint(&self) -> String {
        let base = self
            .config
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_OPENAI_BASE_URL);
        format!("{}/audio/transcriptions", base.trim_end_matches('/'))
    }

    async fn transcribe_wav(
        &self,
        wav: Vec<u8>,
        language: &str,
        prompt: Option<&str>,
    ) -> Result<TranscriptionResult> {
        match self.transcribe_inner(wav, language, prompt).await {
            Ok(result) => Ok(result),
            Err(err) => {
                tracing::error!(error = %err, "Transcription error");
                Err(SpeechError::msg(format!("STT transcription failed: {err}")))
            }
        }
    }

    async fn transcribe_inner(
        &self,
        wav: Vec<u8>,
        language: &str,
        prompt: Option<&str>,
    ) -> Result<TranscriptionResult> {
        let started = std::time::Instant::now();
        let model = self
            .config
            .model
            .clone()
            .unwrap_or_else(|| "whisper-1".to_string());
        let supports_logprobs = model == "gpt-4o-transcribe" || model == "gpt-4o-mini-transcribe";

        let file = Part::bytes(wav)
            .file_name(format!("audio-{}.wav", uuid::Uuid::new_v4()))
            .mime_str("audio/wav")
            .map_err(SpeechError::from)?;
        let mut form = Form::new()
            .part("file", file)
            .text("language", language.to_string())
            .text("model", model);
        if let Some(prompt) = prompt.filter(|p| !p.is_empty()) {
            form = form.text("prompt", prompt.to_string());
        }
        if supports_logprobs {
            form = form.text("include[]", "logprobs");
        }
        form = form.text("response_format", "json");

        let response = self
            .client
            .post(self.endpoint())
            .bearer_auth(&self.config.api_key)
            .multipart(form)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(SpeechError::msg(api_error_message(status.as_u16(), &body)));
        }
        let json: Value = serde_json::from_str(&body)
            .map_err(|e| SpeechError::msg(format!("Invalid transcription response: {e}")))?;

        let text = json
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| SpeechError::msg("Invalid transcription response: missing text"))?
            .to_string();
        let threshold = self
            .config
            .confidence_threshold
            .unwrap_or(DEFAULT_CONFIDENCE_THRESHOLD);

        let mut avg_logprob = None;
        let mut is_low_confidence = false;
        let logprobs: Option<Vec<LogprobToken>> = if supports_logprobs {
            json.get("logprobs").and_then(parse_logprobs)
        } else {
            None
        };
        if let Some(tokens) = logprobs.as_ref().filter(|t| !t.is_empty()) {
            let total: f64 = tokens.iter().map(|t| t.logprob).sum();
            let avg = total / tokens.len() as f64;
            avg_logprob = Some(avg);
            is_low_confidence = avg < threshold;
            if is_low_confidence {
                tracing::debug!(avg_logprob = avg, threshold, %text, "Low confidence transcription detected");
            }
        }

        let duration = started.elapsed().as_millis() as f64;
        tracing::debug!(duration, %text, ?avg_logprob, "Transcription complete");
        Ok(TranscriptionResult {
            text,
            language: json
                .get("language")
                .and_then(Value::as_str)
                .map(str::to_string),
            duration: Some(duration),
            logprobs,
            avg_logprob,
            is_low_confidence: Some(is_low_confidence),
        })
    }
}

/// Mirrors the SDK's `APIError` text: `"<status> <error.message>"`.
fn api_error_message(status: u16, body: &str) -> String {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().to_string());
    if message.is_empty() {
        format!("{status} status code (no body)")
    } else {
        format!("{status} {message}")
    }
}

/// Accepts only a fully well-formed logprob array, like the TS type guard.
fn parse_logprobs(value: &Value) -> Option<Vec<LogprobToken>> {
    let array = value.as_array()?;
    let mut out = Vec::with_capacity(array.len());
    for entry in array {
        let object = entry.as_object()?;
        let token = object.get("token")?.as_str()?.to_string();
        let logprob = object.get("logprob")?.as_f64()?;
        let bytes = match object.get("bytes") {
            None | Some(Value::Null) => None,
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .map(|b| b.as_u64().and_then(|n| u8::try_from(n).ok()))
                    .collect::<Option<Vec<u8>>>()?,
            ),
            Some(_) => return None,
        };
        out.push(LogprobToken {
            token,
            logprob,
            bytes,
        });
    }
    Some(out)
}

impl SttProvider for OpenAiStt {
    fn id(&self) -> &str {
        "openai"
    }
    fn create_session(
        &self,
        params: &SttSessionParams,
    ) -> Result<(Arc<dyn SttSession>, SttEventReceiver)> {
        let (tx, rx) = stt_event_channel();
        let session = OpenAiSttSession {
            shared: self.shared.clone(),
            language: params.language.clone(),
            prompt: params.prompt.clone(),
            events: tx,
            state: Mutex::new(SessionState {
                connected: false,
                segment_id: uuid::Uuid::new_v4().to_string(),
                previous_segment_id: None,
                pcm16: Vec::new(),
            }),
        };
        Ok((Arc::new(session), rx))
    }
}

struct SessionState {
    connected: bool,
    segment_id: String,
    previous_segment_id: Option<String>,
    pcm16: Vec<u8>,
}

struct OpenAiSttSession {
    shared: Arc<Shared>,
    language: Option<String>,
    prompt: Option<String>,
    events: SttEventSender,
    state: Mutex<SessionState>,
}

impl OpenAiSttSession {
    fn emit(&self, event: SttEvent) {
        let _ = self.events.send(event);
    }
}

#[async_trait]
impl SttSession for OpenAiSttSession {
    fn required_sample_rate(&self) -> u32 {
        OPENAI_STT_SAMPLE_RATE
    }

    async fn connect(&self) -> Result<()> {
        self.state.lock().unwrap().connected = true;
        Ok(())
    }

    fn append_pcm16(&self, chunk: &[u8]) {
        let mut st = self.state.lock().unwrap();
        if !st.connected {
            drop(st);
            self.emit(SttEvent::Error("STT session not connected".into()));
            return;
        }
        st.pcm16.extend_from_slice(chunk);
    }

    fn commit(&self) -> Result<()> {
        // Snapshot the segment and its audio now. The TypeScript session reset them only after the
        // request finished, which discarded audio appended while a transcription was in flight and
        // let a second commit reuse the same segment id.
        let (segment_id, previous, pcm16) = {
            let mut st = self.state.lock().unwrap();
            if !st.connected {
                drop(st);
                self.emit(SttEvent::Error("STT session not connected".into()));
                return Ok(());
            }
            let segment_id =
                std::mem::replace(&mut st.segment_id, uuid::Uuid::new_v4().to_string());
            let previous = st.previous_segment_id.replace(segment_id.clone());
            (segment_id, previous, std::mem::take(&mut st.pcm16))
        };
        self.emit(SttEvent::Committed(CommittedEvent {
            segment_id: segment_id.clone(),
            previous_segment_id: previous,
        }));

        let shared = self.shared.clone();
        let events = self.events.clone();
        let language = self.language.clone();
        let prompt = self.prompt.clone();
        tokio::spawn(async move {
            if pcm16.is_empty() {
                let _ = events.send(SttEvent::Transcript(TranscriptEvent {
                    segment_id,
                    is_final: true,
                    language,
                    is_low_confidence: Some(true),
                    ..Default::default()
                }));
                return;
            }
            let wav = pcm_to_wav(&pcm16, OPENAI_STT_SAMPLE_RATE, 1, 16);
            match shared
                .transcribe_wav(wav, language.as_deref().unwrap_or("en"), prompt.as_deref())
                .await
            {
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
