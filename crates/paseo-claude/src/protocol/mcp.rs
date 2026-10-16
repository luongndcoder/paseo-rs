//! MCP server configs (`--mcp-config`, `mcp_set_servers`), status, and the SDK-MCP
//! JSON-RPC tunnel carried by `mcp_message` control requests.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use super::lossless::Extra;
use super::permission::string_enum;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpStdioConfig {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpRemoteConfig {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpSdkConfig {
    pub name: String,
    #[serde(flatten)]
    pub extra: Extra,
}

/// One value of the `mcpServers` map. A config without `type` is stdio.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpServerConfig {
    #[serde(rename = "stdio")]
    Stdio(McpStdioConfig),
    #[serde(rename = "sse")]
    Sse(McpRemoteConfig),
    #[serde(rename = "http")]
    Http(McpRemoteConfig),
    /// In-process server: the host answers its `mcp_message` requests.
    #[serde(rename = "sdk")]
    Sdk(McpSdkConfig),
    /// Stdio config with the `type` key omitted (keeps the omission on reserialize).
    #[serde(untagged)]
    ImplicitStdio(McpStdioConfig),
    #[serde(untagged)]
    Unknown(Value),
}

/// Body of the `--mcp-config` argument: `{"mcpServers": {...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct McpConfigFile {
    #[serde(rename = "mcpServers")]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    #[serde(flatten)]
    pub extra: Extra,
}

string_enum!(McpConnectionStatus {
    Connected = "connected",
    Failed = "failed",
    NeedsAuth = "needs-auth",
    Pending = "pending",
    Disabled = "disabled",
});

/// Element of `mcp_status` → `response.mcpServers`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    pub status: McpConnectionStatus,
    #[serde(
        rename = "serverInfo",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub server_info: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `mcp_status` response payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpStatusResponse {
    #[serde(rename = "mcpServers")]
    pub mcp_servers: Vec<McpServerStatus>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// JSON-RPC 2.0 message tunnelled through `mcp_message` (kept as raw JSON: the host's MCP
/// server implementation owns the typing).
pub type JsonRpcMessage = Value;

/// `true` for a JSON-RPC request (has `method` and a non-null `id`); notifications have no `id`.
pub fn is_jsonrpc_request(msg: &JsonRpcMessage) -> bool {
    msg.get("method").is_some() && msg.get("id").is_some_and(|id| !id.is_null())
}

/// `response` payload of the control response that answers an `mcp_message` request.
pub fn mcp_response_payload(jsonrpc_response: JsonRpcMessage) -> Value {
    serde_json::json!({ "mcp_response": jsonrpc_response })
}

/// Payload for a notification (no JSON-RPC response exists); the TS SDK sends this.
pub fn mcp_notification_ack() -> Value {
    mcp_response_payload(serde_json::json!({ "jsonrpc": "2.0", "result": {}, "id": 0 }))
}
