//! The JSON-RPC surface of the agent MCP server (`McpServer` of `@modelcontextprotocol/sdk`
//! 1.29, tools only): `initialize`, `ping`, `tools/list`, `tools/call`.
//!
//! Stateless: every request builds its own catalog, nothing survives between requests, and no
//! `initialize` is required before another method (the SDK server does not enforce one).

use paseo_tools::serialization::mcp_tool_listing;
use paseo_tools::zod::{issues_message, obj, opt, record, str_z, Seg, Z};
use paseo_tools::ToolCatalog;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;

pub const SERVER_NAME: &str = "agent-mcp";
pub const SERVER_VERSION: &str = "2.0.0";
pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 5] = [
    LATEST_PROTOCOL_VERSION,
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
    "2024-10-07",
];

/// A JSON-RPC error: `ErrorCode.MethodNotFound` is -32601, an unhandled throw -32603.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// One decoded JSON-RPC message of a POST body.
#[derive(Debug, Clone)]
pub enum Message {
    Request {
        id: Value,
        method: String,
        params: Option<Value>,
    },
    Notification,
    /// A response from the client: this server never sends requests, so it is ignored.
    Response,
}

/// `JSONRPCMessageSchema`: `None` for anything that is not a valid message.
pub fn parse_message(value: &Value) -> Option<Message> {
    let map = value.as_object()?;
    if map.get("jsonrpc")?.as_str()? != "2.0" {
        return None;
    }
    let id = map.get("id");
    let valid_id = |v: &Value| v.is_string() || v.as_i64().is_some() || v.as_u64().is_some();
    if let Some(method) = map.get("method") {
        let method = method.as_str()?.to_owned();
        let params = match map.get("params") {
            None => None,
            Some(p @ Value::Object(_)) => Some(p.clone()),
            Some(_) => return None,
        };
        return match id {
            None => Some(Message::Notification),
            Some(id) if valid_id(id) => Some(Message::Request {
                id: id.clone(),
                method,
                params,
            }),
            Some(_) => None,
        };
    }
    if !valid_id(id?) {
        return None;
    }
    match (map.get("result"), map.get("error")) {
        (Some(Value::Object(_)), None) => Some(Message::Response),
        (None, Some(Value::Object(e)))
            if e.get("code").is_some_and(Value::is_number)
                && e.get("message").is_some_and(Value::is_string) =>
        {
            Some(Message::Response)
        }
        _ => None,
    }
}

/// Validates `params` against `schema` the way the SDK's request schemas do; a failure is a
/// `ZodError` thrown from the handler, so it surfaces as an internal error whose message is the
/// pretty-printed issue list.
fn validate_params(schema: Z, params: Option<&Value>) -> Result<Map<String, Value>, RpcError> {
    let (value, issues) = schema.parse(params, &[Seg::Key("params".into())]);
    if issues.is_empty() {
        Ok(value
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default())
    } else {
        Err(RpcError {
            code: -32603,
            message: issues_message(&issues),
        })
    }
}

fn initialize(params: Option<&Value>) -> Result<Value, RpcError> {
    let schema = obj(vec![
        ("protocolVersion", str_z()),
        ("capabilities", record(Z::Unknown)),
        (
            "clientInfo",
            obj(vec![("name", str_z()), ("version", str_z())])
                .passthrough()
                .z(),
        ),
    ])
    .passthrough()
    .z();
    let params = validate_params(schema, params)?;
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        LATEST_PROTOCOL_VERSION
    };
    Ok(json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": true}},
        "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
    }))
}

/// Runs one request against `catalog`.
pub async fn handle_request(
    catalog: &ToolCatalog,
    method: &str,
    params: Option<&Value>,
    cancel: CancellationToken,
) -> Result<Value, RpcError> {
    match method {
        "initialize" => initialize(params),
        "ping" => Ok(json!({})),
        "tools/list" => {
            let tools: Vec<Value> = catalog.tools().map(|t| mcp_tool_listing(t.def())).collect();
            Ok(json!({"tools": tools}))
        }
        // PARITY-QUIRK(mcp-omitted-arguments-rejected): a call without `arguments` reaches the
        // tool validation as `undefined` and fails for every tool, as in TS.
        "tools/call" => {
            let schema = obj(vec![
                ("name", str_z()),
                ("arguments", opt(record(Z::Unknown))),
            ])
            .passthrough()
            .z();
            let params = validate_params(schema, params)?;
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = params
                .get("arguments")
                .map(|a| Value::Object(a.as_object().cloned().unwrap_or_default()));
            Ok(catalog
                .call_tool_for_mcp(name, arguments.as_ref(), cancel)
                .await)
        }
        _ => Err(RpcError {
            code: -32601,
            message: "Method not found".into(),
        }),
    }
}

/// `{result, jsonrpc, id}` as `Protocol._onrequest` writes it.
pub fn result_response(id: &Value, result: Value) -> Value {
    json!({"result": result, "jsonrpc": "2.0", "id": id})
}

/// `{jsonrpc, id, error: {code, message}}`.
pub fn error_response(id: &Value, error: &RpcError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": error.code, "message": error.message}})
}
