//! Error type of the client runtime.

use std::time::Duration;

/// Boxed error returned by user callbacks (`CanUseTool`, `HookCallback`, `SdkMcpServer`).
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to spawn `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session closed")]
    Closed,
    #[error("timed out after {after:?} waiting for {what}")]
    Timeout { what: String, after: Duration },
    /// The CLI answered a control request with `subtype: "error"`.
    #[error("control request `{subtype}` failed: {message}")]
    Control { subtype: String, message: String },
    /// A control response did not have the expected shape.
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("Claude Code process exited with {status}. stderr: {stderr}")]
    ProcessExited { status: String, stderr: String },
    #[error("Claude Code returned an error result: {0}")]
    ErrorResult(String),
    #[error("invalid options: {0}")]
    InvalidOptions(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
