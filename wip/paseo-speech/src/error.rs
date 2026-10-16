use thiserror::Error;

/// Error type shared by every provider and by the dictation manager.
///
/// The `Display` text is what reaches clients in `dictation_stream_error`, so
/// variants keep the exact wording the TypeScript daemon used.
#[derive(Debug, Error)]
pub enum SpeechError {
    #[error("{0}")]
    Message(String),
    #[error("Invalid WAV header")]
    InvalidWavHeader,
    #[error("Missing WAV fmt/data chunks")]
    MissingWavChunks,
    #[error("Unsupported WAV encoding (audioFormat={0})")]
    UnsupportedWavEncoding(u16),
    #[error("Unexpected WAV format: channels={channels} rate={rate} bits={bits}")]
    UnexpectedWavFormat { channels: u16, rate: u32, bits: u16 },
    #[error("WAV PCM16 data length must be even")]
    OddWavData,
    #[error("PCM16 chunk byteLength must be even, got {0}")]
    OddPcmLength(usize),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
}

impl SpeechError {
    pub fn msg(message: impl Into<String>) -> Self {
        SpeechError::Message(message.into())
    }
}

pub type Result<T, E = SpeechError> = std::result::Result<T, E>;
