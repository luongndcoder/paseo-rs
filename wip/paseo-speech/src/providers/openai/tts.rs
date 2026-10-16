//! OpenAI-compatible REST TTS (port of `providers/openai/tts.ts`).

use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::json;

use crate::error::{Result, SpeechError};
use crate::provider::{SpeechStreamResult, TtsProvider};

use super::config::{DEFAULT_OPENAI_TTS_MODEL, DEFAULT_OPENAI_TTS_VOICE};
use super::stt::DEFAULT_OPENAI_BASE_URL;

#[derive(Debug, Clone)]
pub struct TtsConfig {
    pub api_key: String,
    pub base_url: Option<String>,
    pub model: String,
    pub voice: String,
    /// `mp3 | opus | aac | flac | wav | pcm`.
    pub response_format: String,
}

impl TtsConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: None,
            model: DEFAULT_OPENAI_TTS_MODEL.to_string(),
            voice: DEFAULT_OPENAI_TTS_VOICE.to_string(),
            response_format: "pcm".to_string(),
        }
    }
}

pub struct OpenAiTts {
    client: reqwest::Client,
    config: TtsConfig,
}

impl OpenAiTts {
    pub fn new(config: TtsConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
        }
    }

    pub fn config(&self) -> &TtsConfig {
        &self.config
    }

    fn endpoint(&self) -> String {
        let base = self
            .config
            .base_url
            .as_deref()
            .unwrap_or(DEFAULT_OPENAI_BASE_URL);
        format!("{}/audio/speech", base.trim_end_matches('/'))
    }

    async fn request(&self, text: &str) -> Result<SpeechStreamResult> {
        let response = self
            .client
            .post(self.endpoint())
            .bearer_auth(&self.config.api_key)
            .json(&json!({
                "model": self.config.model,
                "voice": self.config.voice,
                "input": text,
                "response_format": self.config.response_format,
            }))
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| {
                    v.pointer("/error/message")
                        .and_then(|m| m.as_str().map(str::to_string))
                })
                .unwrap_or_else(|| body.trim().to_string());
            return Err(SpeechError::msg(
                format!("{} {}", status.as_u16(), message)
                    .trim()
                    .to_string(),
            ));
        }
        let stream = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other));
        Ok(SpeechStreamResult {
            stream: Box::pin(stream),
            format: self.config.response_format.clone(),
        })
    }
}

#[async_trait]
impl TtsProvider for OpenAiTts {
    async fn synthesize_speech(&self, text: &str) -> Result<SpeechStreamResult> {
        if text.trim().is_empty() {
            return Err(SpeechError::msg("Cannot synthesize empty text"));
        }
        match self.request(text).await {
            Ok(result) => Ok(result),
            Err(err) => {
                tracing::error!(error = %err, "Speech synthesis error");
                Err(SpeechError::msg(format!("TTS synthesis failed: {err}")))
            }
        }
    }
}
