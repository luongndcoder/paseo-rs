//! Local TTS (port of `sherpa/sherpa-tts.ts`): one native generation at a time, PCM16 output
//! chunked into ~50 ms pieces. The synthesizer sits behind [`TtsEngine`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;

use crate::audio::{chunk_buffer, float32_to_pcm16le};
use crate::error::{Result, SpeechError};
use crate::provider::{SpeechStreamResult, TtsProvider};

pub struct TtsAudio {
    pub samples: Vec<f32>,
    pub sample_rate: Option<u32>,
}

pub trait TtsEngine: Send + Sync + 'static {
    /// Model sample rate when the generation result does not carry one.
    fn sample_rate(&self) -> Option<u32>;
    fn generate(&self, text: &str, speaker_id: i32, speed: f32) -> Result<TtsAudio>;
}

pub struct LocalTts<E: TtsEngine> {
    engine: Arc<E>,
    speaker_id: i32,
    speed: f32,
    /// Native handles are not reentrant: one generation at a time, in request order.
    queue: tokio::sync::Mutex<()>,
    closed: AtomicBool,
}

impl<E: TtsEngine> LocalTts<E> {
    pub fn new(engine: E, speaker_id: Option<i32>, speed: Option<f32>) -> Self {
        Self {
            engine: Arc::new(engine),
            speaker_id: speaker_id.unwrap_or(0),
            speed: speed.unwrap_or(1.0),
            queue: tokio::sync::Mutex::new(()),
            closed: AtomicBool::new(false),
        }
    }

    /// Stops accepting work. A generation already running finishes first; queued ones fail with
    /// "TTS provider closed". The engine is dropped when the last in-flight call releases it.
    pub fn free(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl<E: TtsEngine> TtsProvider for LocalTts<E> {
    async fn synthesize_speech(&self, text: &str) -> Result<SpeechStreamResult> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(SpeechError::msg("Cannot synthesize empty text"));
        }
        let _turn = self.queue.lock().await;
        if self.closed.load(Ordering::SeqCst) {
            return Err(SpeechError::msg("TTS provider closed"));
        }
        let engine = self.engine.clone();
        let (speaker_id, speed) = (self.speaker_id, self.speed);
        let text = trimmed.to_string();
        let audio = tokio::task::spawn_blocking(move || engine.generate(&text, speaker_id, speed))
            .await
            .map_err(|e| SpeechError::msg(format!("TTS generation panicked: {e}")))??;

        if audio.samples.is_empty() && audio.sample_rate.is_none() {
            return Err(SpeechError::msg(
                "Unexpected sherpa TTS output: missing Float32 samples",
            ));
        }
        let sample_rate = audio
            .sample_rate
            .filter(|r| *r > 0)
            .or(self.engine.sample_rate())
            .unwrap_or(24000);
        let pcm16 = float32_to_pcm16le(&audio.samples);
        let chunk_bytes = ((sample_rate as f64 * 0.05).round() as usize * 2).max(2);
        Ok(SpeechStreamResult::from_chunks(
            chunk_buffer(&pcm16, chunk_bytes),
            format!("pcm;rate={sample_rate}"),
        ))
    }
}
