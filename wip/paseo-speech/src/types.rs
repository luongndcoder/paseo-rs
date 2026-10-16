use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeechProviderId {
    Openai,
    Local,
    Soniox,
}

impl SpeechProviderId {
    /// Parses like the TS schema: trimmed, lower-cased, one of three ids.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "openai" => Some(Self::Openai),
            "local" => Some(Self::Local),
            "soniox" => Some(Self::Soniox),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Local => "local",
            Self::Soniox => "soniox",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestedSpeechProvider {
    pub provider: SpeechProviderId,
    pub explicit: bool,
    /// `None` means "not specified", which every consumer treats as enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl RequestedSpeechProvider {
    pub fn is_enabled(&self) -> bool {
        self.enabled != Some(false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestedSpeechProviders {
    pub dictation_stt: RequestedSpeechProvider,
    pub voice_turn_detection: RequestedSpeechProvider,
    pub voice_stt: RequestedSpeechProvider,
    pub voice_tts: RequestedSpeechProvider,
}

impl Default for RequestedSpeechProviders {
    fn default() -> Self {
        let local = RequestedSpeechProvider {
            provider: SpeechProviderId::Local,
            explicit: false,
            enabled: Some(true),
        };
        Self {
            dictation_stt: local,
            voice_turn_detection: local,
            voice_stt: local,
            voice_tts: local,
        }
    }
}
