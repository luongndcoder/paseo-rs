//! Crate error type.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("OpenCode returned HTTP {status} for {path}: {body}")]
    Http {
        status: u16,
        path: String,
        body: String,
    },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("OpenCode binary not found: {0}")]
    BinaryNotFound(String),
    #[error("OpenCode server startup failed: {0}")]
    Startup(String),
    #[error("OpenCode server exited: {0}")]
    ServerExited(String),
    #[error("timed out: {0}")]
    Timeout(String),
    #[error("session is closed")]
    Closed,
    #[error("a foreground turn is already active")]
    TurnActive,
    #[error("no pending permission request '{0}'")]
    UnknownPermission(String),
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// True for HTTP 404 (OpenCode `NotFoundError`).
    pub fn is_not_found(&self) -> bool {
        matches!(self, Error::Http { status: 404, .. })
    }
}

pub type Result<T> = std::result::Result<T, Error>;
