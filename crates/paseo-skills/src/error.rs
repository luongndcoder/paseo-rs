//! Error type shared by every module. Display strings are the TS error messages where TS
//! throws a plain `Error`, so callers can match on them as they do today.

#[derive(Debug, thiserror::Error)]
pub enum SkillsError {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// The `skills.selection` config store refused a read or write.
    #[error("{0}")]
    Store(String),
    /// A condition TS reports with `throw new Error(message)`.
    #[error("{0}")]
    Failed(String),
    /// The actor thread is gone (all handles dropped or the thread died).
    #[error("skills actor stopped")]
    ActorStopped,
}

pub type Result<T> = std::result::Result<T, SkillsError>;

pub(crate) fn failed(message: impl Into<String>) -> SkillsError {
    SkillsError::Failed(message.into())
}
