//! Error types shared by every layer of the crate.

use serde_json::Value;

use crate::protocol::RpcError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// The app-server answered a request with a JSON-RPC error object.
    #[error("codex app-server rpc error: {0}")]
    Rpc(Box<RpcError>),
    /// The app-server process (or replay transport) went away. Carries the stderr tail.
    #[error("codex app-server exited: {0}")]
    Exited(String),
    #[error("codex app-server request timed out: {method}")]
    Timeout { method: String },
    #[error("codex app-server session is closed")]
    Closed,
    #[error("{0}")]
    Invalid(String),
    #[error("protocol violation: {0}")]
    Protocol(String),
    #[error("no pending permission request with id '{0}'")]
    UnknownPermission(String),
    #[error("a foreground turn is already active")]
    TurnActive,
    #[error("codex turn failed: {0}")]
    TurnFailed(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// `no active turn to interrupt`: the turn finished before the interrupt landed.
    pub fn is_already_idle_interrupt(&self) -> bool {
        matches!(self, Error::Rpc(e) if e.code_i64() == Some(-32600) && e.message == "no active turn to interrupt")
    }

    /// Rejections that prove the steer input was never submitted (safe to fall back to a new turn).
    pub fn is_definitive_steer_rejection(&self) -> bool {
        let Error::Rpc(e) = self else { return false };
        match e.code_i64() {
            Some(-32601) => true,
            Some(-32600) => {
                let active_turn_not_steerable = e
                    .data
                    .as_ref()
                    .and_then(|d| d.get("codexErrorInfo"))
                    .and_then(Value::as_object)
                    .is_some_and(|o| o.contains_key("activeTurnNotSteerable"));
                active_turn_not_steerable
                    || e.message == "no active turn to steer"
                    || is_expected_active_turn_mismatch(&e.message)
                    || e.message == "active turn uses a different output schema"
            }
            _ => false,
        }
    }
}

/// ``expected active turn id `X` but found `Y` ``, matched exactly.
fn is_expected_active_turn_mismatch(message: &str) -> bool {
    let Some(rest) = message.strip_prefix("expected active turn id `") else {
        return false;
    };
    let Some((first, rest)) = rest.split_once("` but found `") else {
        return false;
    };
    let Some(second) = rest.strip_suffix('`') else {
        return false;
    };
    !first.is_empty() && !first.contains('`') && !second.is_empty() && !second.contains('`')
}
