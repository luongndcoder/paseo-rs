use serde_json::Value;

/// JSON-RPC error codes emitted by the host (see RPC.md).
pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL: i64 = -32603;
    pub const PROVIDER: i64 = -32000;
    pub const UNKNOWN_PROVIDER: i64 = -32001;
    pub const UNKNOWN_SESSION: i64 = -32002;
    pub const SHUTTING_DOWN: i64 = -32003;
    pub const CANCELED: i64 = -32800;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to spawn provider host: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("io error talking to provider host: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("host returned error {code}: {message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    #[error("provider host exited ({reason}); stderr tail: {stderr_tail}")]
    HostExited { reason: String, stderr_tail: String },
    #[error("request timed out after {0:?}")]
    Timeout(std::time::Duration),
    #[error("protocol violation: {0}")]
    Protocol(String),
}

impl Error {
    /// The JSON-RPC error code when this is a host-reported error.
    pub fn rpc_code(&self) -> Option<i64> {
        match self {
            Error::Rpc { code, .. } => Some(*code),
            _ => None,
        }
    }

    pub fn is_canceled(&self) -> bool {
        self.rpc_code() == Some(code::CANCELED)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
