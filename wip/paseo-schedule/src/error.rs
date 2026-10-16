use std::fmt;

/// Error type for the schedule crate. `Display` yields the same message the TS
/// daemon would put on the wire as the `rpc_error` text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    /// Plain error with a TS-identical message.
    Message(String),
    /// The run target no longer exists (agent deleted/archived, cwd removed).
    /// Permanent: the service completes the schedule instead of retrying.
    TargetGone(String),
    /// Filesystem failure.
    Io(String),
}

impl ScheduleError {
    pub fn msg(message: impl Into<String>) -> Self {
        ScheduleError::Message(message.into())
    }

    pub fn message(&self) -> &str {
        match self {
            ScheduleError::Message(m) | ScheduleError::TargetGone(m) | ScheduleError::Io(m) => m,
        }
    }

    pub fn is_target_gone(&self) -> bool {
        matches!(self, ScheduleError::TargetGone(_))
    }
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for ScheduleError {}

impl From<std::io::Error> for ScheduleError {
    fn from(err: std::io::Error) -> Self {
        ScheduleError::Io(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, ScheduleError>;
