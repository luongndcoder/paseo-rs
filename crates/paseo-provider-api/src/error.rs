use std::time::Duration;

/// Failure of a provider call. Variants mirror the host error codes (`RPC.md`) so a host
/// adapter maps one to one; native adapters use `Provider` for anything the provider threw.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The provider threw (`-32000`). `message` is the provider's own message.
    #[error("{message}")]
    Provider { message: String },
    #[error("unknown provider {0}")]
    UnknownProvider(String),
    #[error("unknown session {0}")]
    UnknownSession(String),
    #[error("provider is shutting down")]
    ShuttingDown,
    #[error("request canceled")]
    Canceled,
    /// The provider or session does not implement this operation.
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("provider host exited ({reason}); stderr tail: {stderr_tail}")]
    HostExited { reason: String, stderr_tail: String },
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("protocol violation: {0}")]
    Protocol(String),
    /// A payload did not decode into the typed model.
    #[error("decode error: {0}")]
    Decode(String),
    /// The event stream ended before the turn reached a terminal event.
    #[error("event stream closed before the turn finished")]
    StreamClosed,
    /// A turn ended with `turn_failed`.
    #[error("{0}")]
    TurnFailed(String),
}

impl ProviderError {
    pub fn provider(message: impl Into<String>) -> Self {
        Self::Provider {
            message: message.into(),
        }
    }
}

pub type Result<T> = std::result::Result<T, ProviderError>;
