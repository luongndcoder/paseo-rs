//! Wire types for the Codex app-server protocol: line-delimited JSON-RPC 2.0 *without* the
//! `"jsonrpc"` member (the server neither sends nor requires it).
//!
//! Layers, bottom up:
//! - [`Frame`]: lossless envelope. `parse -> serialize` is JSON-equal for every frame.
//! - [`Incoming`]: a frame classified as response / error / server request / notification.
//! - [`ServerNotification`], [`ServerRequest`]: typed views of `params`, with a `Value` fallback for
//!   anything this crate does not model (new Codex versions add methods constantly).
//! - [`params`]: typed client request parameters.

pub mod items;
pub mod notifications;
pub mod params;
pub mod requests;

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::error::{Error, Result};

pub use items::{KnownItem, ThreadItem};
pub use notifications::ServerNotification;
pub use requests::ServerRequest;

/// JSON-RPC request id. The client only ever allocates integers; servers may use either.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Int(i64),
    Str(String),
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestId::Int(i) => write!(f, "{i}"),
            RequestId::Str(s) => f.write_str(s),
        }
    }
}

/// JSON-RPC error object. `code` is kept as a [`Value`] because Codex types it `string | number`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Value>,
    #[serde(default)]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn code_i64(&self) -> Option<i64> {
        self.code.as_ref().and_then(Value::as_i64)
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.code {
            Some(code) => write!(f, "{} (code {code})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// `null` is a value here, not an absent field.
fn some_value<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// Lossless JSON-RPC envelope. Unknown top-level members (Codex adds `emittedAtMs` to
/// notifications) land in `extra`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Frame {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(
        default,
        deserialize_with = "some_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub params: Option<Value>,
    #[serde(
        default,
        deserialize_with = "some_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub result: Option<Value>,
    #[serde(
        default,
        deserialize_with = "some_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub error: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Frame {
    pub fn parse_line(line: &str) -> Result<Frame> {
        Ok(serde_json::from_str(line)?)
    }

    pub fn to_line(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }
}

/// A notification: has `method`, no `id`.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub method: String,
    pub params: Value,
    pub emitted_at_ms: Option<i64>,
}

impl Notification {
    pub fn typed(&self) -> ServerNotification {
        ServerNotification::parse(&self.method, &self.params)
    }
}

/// A server-initiated request: has `method` and `id`; the client must answer it.
#[derive(Debug, Clone, PartialEq)]
pub struct RawServerRequest {
    pub id: RequestId,
    pub method: String,
    pub params: Value,
}

impl RawServerRequest {
    pub fn typed(&self) -> ServerRequest {
        ServerRequest::parse(&self.method, &self.params)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Response {
        id: RequestId,
        result: Value,
    },
    Error {
        id: Option<RequestId>,
        error: RpcError,
    },
    Request(RawServerRequest),
    Notification(Notification),
}

impl Incoming {
    pub fn parse_line(line: &str) -> Result<Incoming> {
        Incoming::from_frame(Frame::parse_line(line)?)
    }

    pub fn from_frame(frame: Frame) -> Result<Incoming> {
        let Frame {
            id,
            method,
            params,
            result,
            error,
            extra,
        } = frame;
        match (id, method) {
            (Some(id), Some(method)) => Ok(Incoming::Request(RawServerRequest {
                id,
                method,
                params: params.unwrap_or(Value::Null),
            })),
            (None, Some(method)) => Ok(Incoming::Notification(Notification {
                method,
                params: params.unwrap_or(Value::Null),
                emitted_at_ms: extra.get("emittedAtMs").and_then(Value::as_i64),
            })),
            (id, None) => {
                if let Some(error) = error {
                    let error: RpcError = serde_json::from_value(error).unwrap_or(RpcError {
                        code: None,
                        message: "Unknown error".into(),
                        data: None,
                    });
                    return Ok(Incoming::Error { id, error });
                }
                match (id, result) {
                    (Some(id), Some(result)) => Ok(Incoming::Response { id, result }),
                    _ => Err(Error::Protocol(
                        "frame is neither request, notification nor response".into(),
                    )),
                }
            }
        }
    }
}

/// Serialize a client request line (no trailing newline).
pub fn request_line(id: i64, method: &str, params: &Value) -> String {
    let mut o = Map::new();
    o.insert("id".into(), Value::from(id));
    o.insert("method".into(), Value::from(method));
    o.insert("params".into(), params.clone());
    Value::Object(o).to_string()
}

/// Serialize a client notification line (no trailing newline).
pub fn notification_line(method: &str, params: &Value) -> String {
    let mut o = Map::new();
    o.insert("method".into(), Value::from(method));
    o.insert("params".into(), params.clone());
    Value::Object(o).to_string()
}

/// Serialize the answer to a server request.
pub fn response_line(id: &RequestId, result: &Value) -> String {
    let mut o = Map::new();
    o.insert("id".into(), serde_json::to_value(id).unwrap_or(Value::Null));
    o.insert("result".into(), result.clone());
    Value::Object(o).to_string()
}

/// Serialize an error answer to a server request.
pub fn error_response_line(id: &RequestId, message: &str) -> String {
    let mut o = Map::new();
    o.insert("id".into(), serde_json::to_value(id).unwrap_or(Value::Null));
    o.insert("error".into(), serde_json::json!({ "message": message }));
    Value::Object(o).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_frames() {
        let r = Incoming::parse_line(r#"{"id":3,"result":{"a":1}}"#).unwrap();
        assert!(matches!(
            r,
            Incoming::Response {
                id: RequestId::Int(3),
                ..
            }
        ));
        let r = Incoming::parse_line(r#"{"id":3,"result":null}"#).unwrap();
        assert!(matches!(
            r,
            Incoming::Response {
                result: Value::Null,
                ..
            }
        ));
        let e =
            Incoming::parse_line(r#"{"id":4,"error":{"code":-32600,"message":"bad"}}"#).unwrap();
        match e {
            Incoming::Error { error, .. } => assert_eq!(error.code_i64(), Some(-32600)),
            other => panic!("{other:?}"),
        }
        let q = Incoming::parse_line(r#"{"id":0,"method":"item/x","params":{}}"#).unwrap();
        assert!(matches!(q, Incoming::Request(_)));
        let n = Incoming::parse_line(r#"{"method":"turn/started","params":{},"emittedAtMs":9}"#)
            .unwrap();
        match n {
            Incoming::Notification(n) => assert_eq!(n.emitted_at_ms, Some(9)),
            other => panic!("{other:?}"),
        }
        assert!(Incoming::parse_line(r#"{"foo":1}"#).is_err());
    }

    #[test]
    fn frame_round_trip_keeps_nulls_and_extras() {
        let v = json!({"method":"m","params":null,"emittedAtMs":5,"id":"s"});
        let f: Frame = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(serde_json::to_value(&f).unwrap(), v);
    }

    #[test]
    fn outgoing_lines_have_no_jsonrpc_member() {
        let l = request_line(1, "initialize", &json!({}));
        let v: Value = serde_json::from_str(&l).unwrap();
        assert_eq!(v, json!({"id":1,"method":"initialize","params":{}}));
    }
}
