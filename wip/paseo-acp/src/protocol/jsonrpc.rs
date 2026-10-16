//! JSON-RPC 2.0 framing exactly as `@agentclientprotocol/sdk` 0.17 writes it.
//!
//! One JSON object per line. Client request ids are integers from 0; the agent numbers
//! its own requests independently (also from 0), so the two id spaces never mix.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn method_not_found(method: &str) -> Self {
        RpcError {
            code: -32601,
            message: format!("\"Method not found\": {method}"),
            data: Some(json!({ "method": method })),
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        RpcError {
            code: -32602,
            message: format!("Invalid params: {}", message.into()),
            data: None,
        }
    }

    pub fn internal(details: impl Into<String>) -> Self {
        RpcError {
            code: -32603,
            message: "Internal error".to_string(),
            data: Some(json!({ "details": details.into() })),
        }
    }

    pub fn resource_not_found(uri: &str) -> Self {
        RpcError {
            code: -32002,
            message: format!("Resource not found: {uri}"),
            data: Some(json!({ "uri": uri })),
        }
    }
}

/// One classified inbound frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        result: Result<Value, RpcError>,
    },
}

/// Classify a decoded line. `Err` returns the value untouched (not a JSON-RPC message).
pub fn classify(value: Value) -> Result<Frame, Value> {
    let Some(obj) = value.as_object() else {
        return Err(value);
    };
    let method = obj.get("method").and_then(Value::as_str).map(str::to_owned);
    let id = obj.get("id").cloned();
    match (method, id) {
        (Some(method), Some(id)) => Ok(Frame::Request {
            id,
            method,
            params: obj.get("params").cloned().unwrap_or(Value::Null),
        }),
        (Some(method), None) => Ok(Frame::Notification {
            method,
            params: obj.get("params").cloned().unwrap_or(Value::Null),
        }),
        (None, Some(id)) => {
            let id = normalize_response_id(id);
            if let Some(err) = obj.get("error") {
                match serde_json::from_value::<RpcError>(err.clone()) {
                    Ok(e) => Ok(Frame::Response { id, result: Err(e) }),
                    Err(_) => Err(value),
                }
            } else if let Some(result) = obj.get("result") {
                Ok(Frame::Response {
                    id,
                    result: Ok(result.clone()),
                })
            } else {
                Err(value)
            }
        }
        (None, None) => Err(value),
    }
}

/// COMPAT(deepseek-tui-acp-id): the TS adapter rewrites stringified numeric response ids
/// ("3") to numbers. Keep the same rule so such agents keep working.
pub fn normalize_response_id(id: Value) -> Value {
    if let Value::String(s) = &id {
        if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(n) = s.parse::<i64>() {
                if n.unsigned_abs() < (1u64 << 53) {
                    return Value::from(n);
                }
            }
        }
    }
    id
}

pub fn request_frame(id: i64, method: &str, params: Value) -> Value {
    let mut m = Map::new();
    m.insert("jsonrpc".into(), json!("2.0"));
    m.insert("id".into(), json!(id));
    m.insert("method".into(), json!(method));
    m.insert("params".into(), params);
    Value::Object(m)
}

pub fn notification_frame(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// A handler that returns nothing answers `result: null`.
pub fn response_ok_frame(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn response_err_frame(id: Value, error: &RpcError) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

/// Pending-request map key for an id value.
pub fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => format!("s:{s}"),
        other => format!("n:{other}"),
    }
}
