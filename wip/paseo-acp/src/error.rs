//! Crate error type. JSON-RPC failures keep their code so callers can still tell
//! invalid params (-32602) from other failures, like `ACPRequestError` in the TS adapter.

use std::fmt;

use serde_json::Value;

use crate::protocol::RpcError;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// The agent answered a request with a JSON-RPC error.
    Rpc(RpcError),
    /// The connection (agent process) is gone.
    Closed,
    Timeout(String),
    /// A frame or payload did not have the expected shape.
    Protocol(String),
    /// Caller error or an operation the agent does not expose.
    Unsupported(String),
    NotFound(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn rpc_code(&self) -> Option<i64> {
        match self {
            Error::Rpc(e) => Some(e.code),
            _ => None,
        }
    }

    pub fn is_invalid_params(&self) -> bool {
        self.rpc_code() == Some(-32602)
    }

    /// Mirrors `summarizeACPRequestError`: message with data detail, string code, diagnostic.
    pub fn summarize(&self) -> ErrorSummary {
        match self {
            Error::Rpc(e) => {
                let code = e.code.to_string();
                let detail = e.data.as_ref().and_then(extract_data_message);
                let message = match detail {
                    Some(d) if d != e.message => format!("{}: {}", e.message, d),
                    _ => e.message.clone(),
                };
                let data = match &e.data {
                    Some(d) => format!(" | data={d}"),
                    None => String::new(),
                };
                ErrorSummary {
                    diagnostic: Some(format!("{message} | code={code}{data}")),
                    message,
                    code: Some(code),
                }
            }
            other => ErrorSummary {
                message: other.to_string(),
                code: None,
                diagnostic: None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ErrorSummary {
    pub message: String,
    pub code: Option<String>,
    pub diagnostic: Option<String>,
}

fn extract_data_message(data: &Value) -> Option<String> {
    let obj = data.as_object()?;
    for key in ["details", "errorMessage", "message", "detail", "title"] {
        if let Some(s) = obj.get(key).and_then(Value::as_str) {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    obj.get("error").and_then(extract_data_message)
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Rpc(_) => f.write_str(&self.summarize().message),
            Error::Closed => f.write_str("ACP connection closed"),
            Error::Timeout(m) | Error::Protocol(m) | Error::Unsupported(m) | Error::NotFound(m) => {
                f.write_str(m)
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Protocol(e.to_string())
    }
}
