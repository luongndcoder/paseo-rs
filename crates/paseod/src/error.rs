//! Edge-wide error type. Builders add variants; keep messages free of secrets
//! (hello frames carry passwords, never log or embed frame bodies).

#[derive(Debug, thiserror::Error)]
pub enum EdgeError {
    /// Contract stub that a parallel builder has not implemented yet.
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
    #[error("config: {0}")]
    Config(String),
    #[error("backend: {0}")]
    Backend(String),
    #[error("relay: {0}")]
    Relay(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, EdgeError>;
