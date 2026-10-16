//! Provider traits (port of `speech-provider.ts` and `turn-detection-provider.ts`).
//!
//! The TypeScript sessions are `EventEmitter`s. Here a session is an
//! `Arc<dyn ...Session>` with `&self` methods plus an unbounded event channel
//! returned by `create_session`, so a consumer can drive and observe a session
//! from different tasks and a session never re-enters its consumer.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogprobToken {
    pub token: String,
    pub logprob: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranscriptionResult {
    pub text: String,
    pub language: Option<String>,
    /// Milliseconds spent transcribing (TS reports wall-clock duration).
    pub duration: Option<f64>,
    pub logprobs: Option<Vec<LogprobToken>>,
    pub avg_logprob: Option<f64>,
    pub is_low_confidence: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommittedEvent {
    pub segment_id: String,
    pub previous_segment_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranscriptEvent {
    pub segment_id: String,
    pub transcript: String,
    pub is_final: bool,
    pub language: Option<String>,
    pub logprobs: Option<Vec<LogprobToken>>,
    pub avg_logprob: Option<f64>,
    pub is_low_confidence: Option<bool>,
}

impl TranscriptEvent {
    pub fn partial(segment_id: impl Into<String>, transcript: impl Into<String>) -> Self {
        Self {
            segment_id: segment_id.into(),
            transcript: transcript.into(),
            ..Default::default()
        }
    }
    pub fn final_(segment_id: impl Into<String>, transcript: impl Into<String>) -> Self {
        Self {
            segment_id: segment_id.into(),
            transcript: transcript.into(),
            is_final: true,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SttEvent {
    Committed(CommittedEvent),
    Transcript(TranscriptEvent),
    /// Display text of the failure (TS passes an `Error`; consumers only read its message).
    Error(String),
}

pub type SttEventSender = mpsc::UnboundedSender<SttEvent>;
pub type SttEventReceiver = mpsc::UnboundedReceiver<SttEvent>;

pub fn stt_event_channel() -> (SttEventSender, SttEventReceiver) {
    mpsc::unbounded_channel()
}

#[derive(Debug, Clone, Default)]
pub struct SttSessionParams {
    pub language: Option<String>,
    pub prompt: Option<String>,
}

#[async_trait]
pub trait SttSession: Send + Sync {
    /// Required PCM16LE sample rate for `append_pcm16`. Callers resample first.
    fn required_sample_rate(&self) -> u32;
    async fn connect(&self) -> Result<()>;
    fn append_pcm16(&self, pcm16le: &[u8]);
    fn commit(&self) -> Result<()>;
    fn clear(&self);
    fn close(&self) -> Result<()>;
}

pub trait SttProvider: Send + Sync {
    /// `"openai"`, `"local"`, `"soniox"` or a custom id.
    fn id(&self) -> &str;
    fn create_session(
        &self,
        params: &SttSessionParams,
    ) -> Result<(Arc<dyn SttSession>, SttEventReceiver)>;
}

pub type AudioByteStream = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

pub struct SpeechStreamResult {
    pub stream: AudioByteStream,
    /// `pcm;rate=24000`, `mp3`, ...
    pub format: String,
}

impl SpeechStreamResult {
    pub fn from_chunks(chunks: Vec<Vec<u8>>, format: impl Into<String>) -> Self {
        let stream = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<Bytes, std::io::Error>(Bytes::from(c))),
        );
        Self {
            stream: Box::pin(stream),
            format: format.into(),
        }
    }

    /// Drains the stream into one buffer.
    pub async fn collect(mut self) -> std::io::Result<Vec<u8>> {
        use futures_util::StreamExt;
        let mut out = Vec::new();
        while let Some(chunk) = self.stream.next().await {
            out.extend_from_slice(&chunk?);
        }
        Ok(out)
    }
}

#[async_trait]
pub trait TtsProvider: Send + Sync {
    async fn synthesize_speech(&self, text: &str) -> Result<SpeechStreamResult>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEvent {
    SpeechStarted,
    SpeechStopped,
}

pub type TurnEventSender = mpsc::UnboundedSender<Result<TurnEvent, String>>;
pub type TurnEventReceiver = mpsc::UnboundedReceiver<Result<TurnEvent, String>>;

pub trait TurnDetectionSession: Send + Sync {
    fn required_sample_rate(&self) -> u32;
    fn connect(&self);
    fn append_pcm16(&self, pcm16le: &[u8]);
    fn flush(&self);
    fn reset(&self);
    fn close(&self);
}

pub trait TurnDetectionProvider: Send + Sync {
    fn id(&self) -> &str;
    fn create_session(&self) -> Result<(Arc<dyn TurnDetectionSession>, TurnEventReceiver)>;
}

/// Late-bound access to a provider, so config reloads can swap it
/// (`Resolvable<SpeechToTextProvider | null>` in TS).
#[derive(Clone)]
pub struct SttResolver(Arc<dyn Fn() -> Option<Arc<dyn SttProvider>> + Send + Sync>);

impl SttResolver {
    pub fn from_fn(f: impl Fn() -> Option<Arc<dyn SttProvider>> + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }
    pub fn none() -> Self {
        Self::from_fn(|| None)
    }
    pub fn resolve(&self) -> Option<Arc<dyn SttProvider>> {
        (self.0)()
    }
}

impl From<Arc<dyn SttProvider>> for SttResolver {
    fn from(provider: Arc<dyn SttProvider>) -> Self {
        Self::from_fn(move || Some(provider.clone()))
    }
}

impl<P: SttProvider + 'static> From<Arc<P>> for SttResolver {
    fn from(provider: Arc<P>) -> Self {
        let provider: Arc<dyn SttProvider> = provider;
        provider.into()
    }
}
