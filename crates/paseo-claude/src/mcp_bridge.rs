//! Answers the CLI's tunnelled MCP JSON-RPC for in-process servers (`mcp_message` requests).

use std::sync::Arc;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::callbacks::{McpCallContext, McpToolResult, SdkMcpServer};

const DEFAULT_PROTOCOL_VERSION: &str = "2025-11-25";

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

fn tool_result_json(r: McpToolResult) -> Value {
    let mut v = json!({"content": r.content});
    if let Some(obj) = v.as_object_mut() {
        // MCP omits `isError` for successful results (golden fixture 05).
        if r.is_error {
            obj.insert("isError".into(), Value::Bool(true));
        }
        if let Some(sc) = r.structured_content {
            obj.insert("structuredContent".into(), sc);
        }
    }
    v
}

/// Handle one JSON-RPC *request* (non-null `id`) and produce the JSON-RPC response.
pub(crate) async fn handle_request(
    server: &Arc<dyn SdkMcpServer>,
    message: &Value,
    cancel: CancellationToken,
) -> Value {
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL_VERSION);
            rpc_result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": true}},
                    "serverInfo": {"name": server.name(), "version": server.version()},
                }),
            )
        }
        "ping" => rpc_result(id, json!({})),
        "tools/list" => match server.list_tools().await {
            Ok(tools) => {
                let tools: Vec<Value> = tools
                    .into_iter()
                    .map(|t| {
                        let mut o = json!({"name": t.name, "inputSchema": t.input_schema});
                        if let Some(map) = o.as_object_mut() {
                            if let Some(d) = t.description {
                                map.insert("description".into(), Value::String(d));
                            }
                            if let Some(a) = t.annotations {
                                map.insert("annotations".into(), a);
                            }
                            map.insert(
                                "execution".into(),
                                t.execution
                                    .unwrap_or_else(|| json!({"taskSupport": "forbidden"})),
                            );
                        }
                        o
                    })
                    .collect();
                rpc_result(id, json!({"tools": tools}))
            }
            Err(e) => rpc_error(id, -32603, e.to_string()),
        },
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return rpc_error(id, -32602, "tools/call requires params.name");
            };
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let tool_use_id = params
                .get("_meta")
                .and_then(|m| m.get("claudecode/toolUseId"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let ctx = McpCallContext {
                tool_use_id,
                cancel,
            };
            let result = match server.call_tool(name, arguments, ctx).await {
                Ok(r) => r,
                Err(e) => McpToolResult::error(e.to_string()),
            };
            rpc_result(id, tool_result_json(result))
        }
        other => rpc_error(id, -32601, format!("Method not found: {other}")),
    }
}
