use paseo_provider_api::ProviderError;

/// Failure of a manager call. Display text is the TS `Error.message` verbatim: callers and
/// tests assert on it.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("{0}")]
    Message(String),
    #[error("Agent manager is shutting down")]
    ShuttingDown,
    #[error(
        "Cannot {action} agent {agent_id} because its active run cancellation was not acknowledged"
    )]
    RunCancellation {
        agent_id: String,
        action: &'static str,
    },
    #[error("Provider session {session_id} is stale after its plugin reloaded")]
    StaleProviderSession { session_id: String },
    /// A wait was aborted through its [`crate::AbortSignal`] (TS `AbortError`).
    #[error("{0}")]
    Aborted(String),
    #[error(transparent)]
    Provider(ProviderError),
}

const STALE_PREFIX: &str = "Provider session ";
const STALE_SUFFIX: &str = " is stale after its plugin reloaded";

/// `StaleProviderSessionError` crosses the provider seam as a plain provider error carrying its
/// message (the host sends `error.message`), so staleness is recognized by that message.
fn stale_session_id(message: &str) -> Option<&str> {
    message
        .strip_prefix(STALE_PREFIX)?
        .strip_suffix(STALE_SUFFIX)
        .filter(|id| !id.is_empty())
}

impl From<ProviderError> for CoreError {
    fn from(error: ProviderError) -> Self {
        if let ProviderError::Provider { message } = &error {
            if let Some(session_id) = stale_session_id(message) {
                return Self::StaleProviderSession {
                    session_id: session_id.to_owned(),
                };
            }
        }
        Self::Provider(error)
    }
}

impl CoreError {
    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    pub fn unknown_agent(id: &str) -> Self {
        Self::Message(format!("Unknown agent '{id}'"))
    }

    pub fn is_stale_provider_session(&self) -> bool {
        matches!(self, Self::StaleProviderSession { .. })
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;

/// `isClosedProviderSessionError`: `/\bsession is closed$/i` on the trimmed message.
pub(crate) fn is_closed_provider_session_message(message: &str) -> bool {
    const TAIL: &str = "session is closed";
    let lower = message.trim().to_lowercase();
    let Some(head) = lower.strip_suffix(TAIL) else {
        return false;
    };
    head.chars()
        .next_back()
        .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_provider_error_is_recognized_by_its_message() {
        let stale = CoreError::from(ProviderError::provider(
            "Provider session stale-bridge is stale after its plugin reloaded",
        ));
        assert!(stale.is_stale_provider_session());
        assert_eq!(
            stale.to_string(),
            "Provider session stale-bridge is stale after its plugin reloaded"
        );
        assert!(!CoreError::from(ProviderError::provider("boom")).is_stale_provider_session());
        assert!(!CoreError::from(ProviderError::provider(
            "Provider session  is stale after its plugin reloaded"
        ))
        .is_stale_provider_session());
    }

    #[test]
    fn closed_session_message_needs_a_word_boundary() {
        assert!(is_closed_provider_session_message("Session is closed"));
        assert!(is_closed_provider_session_message(
            "  codex: session is closed \n"
        ));
        assert!(!is_closed_provider_session_message("subsession is closed"));
        assert!(!is_closed_provider_session_message("session is closed now"));
    }
}
