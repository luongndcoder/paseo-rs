//! Error type shared by every layer of the crate.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Json(serde_json::Error),
    /// The provider binary could not be started.
    Spawn {
        command: String,
        source: std::io::Error,
    },
    /// The provider answered a request with `success: false`.
    Rpc {
        command: String,
        message: String,
    },
    /// No response arrived in time. `stderr` is the tail of the process stderr.
    Timeout {
        command: String,
        elapsed_ms: u128,
        timeout_ms: u128,
        stderr: String,
    },
    /// The process exited or the session was closed while work was pending.
    Closed(String),
    /// The operation does not exist on this provider flavor.
    Unsupported(&'static str),
    /// The peer sent something the protocol does not allow.
    Protocol(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Json(e) => write!(f, "json error: {e}"),
            Error::Spawn { command, source } => write!(f, "failed to spawn `{command}`: {source}"),
            Error::Rpc { message, .. } => f.write_str(message),
            Error::Timeout {
                command,
                elapsed_ms,
                timeout_ms,
                stderr,
            } => {
                write!(
                    f,
                    "request timed out phase={command} elapsedMs={elapsed_ms} timeoutMs={timeout_ms}"
                )?;
                if !stderr.is_empty() {
                    write!(f, "\n{stderr}")?;
                }
                Ok(())
            }
            Error::Closed(message) => f.write_str(message),
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::Protocol(message) => write!(f, "protocol error: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Json(e) => Some(e),
            Error::Spawn { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}
