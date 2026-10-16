//! `/mcp/agents`: the Paseo tool catalog over stateless MCP streamable HTTP.
//!
//! Behavior is `bootstrap.ts` `runAgentMcpRequest` plus the SDK's `StreamableHTTPServerTransport`
//! in stateless mode (no session ids, `enableJsonResponse` off, DNS-rebinding protection off
//! because the Host allowlist runs upstream):
//!
//! 1. endpoint disabled: `404 {"error":"Agent MCP endpoint disabled"}`
//! 2. not authorized (see [`auth`]): `401 {"error":"Unauthorized"}`
//! 3. any method but POST: `405` JSON-RPC error `-32000 Method not allowed`
//! 4. POST: `callerAgentId` from the query selects an agent-scoped catalog; one catalog is built
//!    per request and dropped with it. Replies are `text/event-stream` with one `message` event
//!    per response, `202` when the body holds no requests.
//!
//! DEVIATION(mcp-hand-rolled-transport): the spec names rmcp, but rmcp 3.5's `Tool` has no
//! `execution` field and its serializer fixes key order, so it cannot emit the golden `tools/list`
//! the TS SDK produces byte for byte. The tool definitions and results are built by `paseo-tools`
//! and the JSON-RPC/HTTP layer here is small. `tests/rmcp_client.rs` drives this server with rmcp's
//! real client to keep it protocol-compatible.

pub mod auth;
pub mod rpc;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use paseo_core::PaseoToolPolicy;
use paseo_tools::{CoreApi, RuntimeCtx, ToolCatalogFactory};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

pub use auth::is_agent_mcp_request_authorized;

/// The route the endpoint is mounted on.
pub const AGENT_MCP_ROUTE: &str = "/mcp/agents";

/// Largest request body accepted.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Runtime knobs of the endpoint.
#[derive(Clone)]
pub struct McpOptions {
    /// Per-process capability token injected into agent configs (`mcpAuthToken`); never persisted.
    pub capability_token: String,
    /// bcrypt hash of the daemon password; `None` leaves the endpoint open.
    pub password_hash: Option<String>,
    /// `mcp.enabled`; flipping it takes effect on the next request.
    pub enabled: Arc<AtomicBool>,
}

impl McpOptions {
    pub fn new(capability_token: impl Into<String>) -> Self {
        Self {
            capability_token: capability_token.into(),
            password_hash: None,
            enabled: Arc::new(AtomicBool::new(true)),
        }
    }
}

struct McpState {
    core: Arc<dyn CoreApi>,
    factory: ToolCatalogFactory,
    options: McpOptions,
}

/// A fresh per-process capability token (`randomUUID()` in TS). The core mints one at startup,
/// injects it into the agents' `paseo` MCP entry and never persists it.
pub fn mint_capability_token() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The router for `/mcp/agents` with default options (enabled, no password).
pub fn mcp_router(core: Arc<dyn CoreApi>, token: String) -> Router {
    mcp_router_with(core, McpOptions::new(token))
}

/// The router for `/mcp/agents`.
pub fn mcp_router_with(core: Arc<dyn CoreApi>, options: McpOptions) -> Router {
    let state = Arc::new(McpState {
        factory: ToolCatalogFactory::new(core.clone()),
        core,
        options,
    });
    Router::new()
        .route(AGENT_MCP_ROUTE, any(handle))
        .with_state(state)
}

fn json_response(status: StatusCode, body: &Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(body.to_string()))
        .unwrap_or_default()
}

/// `createJsonErrorResponse` of the SDK transport.
fn rpc_error_response(status: StatusCode, code: i64, message: &str) -> Response {
    let body = json!({"jsonrpc": "2.0", "error": {"code": code, "message": message}, "id": null});
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_default()
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// `req.query.callerAgentId`: the first value; an empty string means no caller (falsy in TS).
fn caller_agent_id(query: Option<&str>) -> Option<String> {
    let query = query?;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == "callerAgentId")
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.is_empty())
}

async fn handle(State(state): State<Arc<McpState>>, req: Request) -> Response {
    if !state.options.enabled.load(Ordering::Relaxed) {
        return json_response(
            StatusCode::NOT_FOUND,
            &json!({"error": "Agent MCP endpoint disabled"}),
        );
    }
    let authorization = header_str(req.headers(), header::AUTHORIZATION).map(str::to_owned);
    if !is_agent_mcp_request_authorized(
        state.options.password_hash.as_deref(),
        Some(&state.options.capability_token),
        authorization.as_deref(),
    )
    .await
    {
        return json_response(StatusCode::UNAUTHORIZED, &json!({"error": "Unauthorized"}));
    }
    // Stateless: GET (standalone SSE) and DELETE (session termination) mean nothing without
    // sessions.
    if req.method() != Method::POST {
        return rpc_error_response(StatusCode::METHOD_NOT_ALLOWED, -32000, "Method not allowed");
    }

    let caller = caller_agent_id(req.uri().query());
    let (parts, body) = req.into_parts();

    // `handlePostRequest`
    let accept = header_str(&parts.headers, header::ACCEPT).unwrap_or_default();
    if !accept.contains("application/json") || !accept.contains("text/event-stream") {
        return rpc_error_response(
            StatusCode::NOT_ACCEPTABLE,
            -32000,
            "Not Acceptable: Client must accept both application/json and text/event-stream",
        );
    }
    let content_type = header_str(&parts.headers, header::CONTENT_TYPE).unwrap_or_default();
    if !content_type.contains("application/json") {
        return rpc_error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            -32000,
            "Unsupported Media Type: Content-Type must be application/json",
        );
    }
    let bytes: Bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return rpc_error_response(StatusCode::BAD_REQUEST, -32700, "Parse error: Invalid JSON")
        }
    };
    let Ok(raw) = serde_json::from_slice::<Value>(&bytes) else {
        return rpc_error_response(StatusCode::BAD_REQUEST, -32700, "Parse error: Invalid JSON");
    };
    let raw_messages: Vec<Value> = match raw {
        Value::Array(items) => items,
        single => vec![single],
    };
    let mut messages = Vec::with_capacity(raw_messages.len());
    for raw_message in &raw_messages {
        match rpc::parse_message(raw_message) {
            Some(message) => messages.push(message),
            None => {
                return rpc_error_response(
                    StatusCode::BAD_REQUEST,
                    -32700,
                    "Parse error: Invalid JSON-RPC message",
                )
            }
        }
    }

    let is_initialize = |m: &rpc::Message| matches!(m, rpc::Message::Request { method, .. } if method == "initialize");
    let has_initialize = messages.iter().any(is_initialize);
    if has_initialize && messages.len() > 1 {
        return rpc_error_response(
            StatusCode::BAD_REQUEST,
            -32600,
            "Invalid Request: Only one initialization request is allowed",
        );
    }
    if !has_initialize {
        // `validateProtocolVersion`: a present header must name a supported version.
        if let Some(version) = header_str(
            &parts.headers,
            header::HeaderName::from_static("mcp-protocol-version"),
        ) {
            if !rpc::SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
                return rpc_error_response(
                    StatusCode::BAD_REQUEST,
                    -32000,
                    &format!(
                        "Bad Request: Unsupported protocol version: {version} (supported versions: {})",
                        rpc::SUPPORTED_PROTOCOL_VERSIONS.join(", ")
                    ),
                );
            }
        }
    }
    if !messages
        .iter()
        .any(|m| matches!(m, rpc::Message::Request { .. }))
    {
        // Only notifications or responses.
        return Response::builder()
            .status(StatusCode::ACCEPTED)
            .body(Body::empty())
            .unwrap_or_default();
    }

    // One catalog per request, torn down with it. A client that goes away drops this future,
    // which cancels the token handed to the tool handlers.
    let policy: Option<PaseoToolPolicy> = caller
        .as_deref()
        .and_then(|id| state.core.paseo_tool_policy(id));
    let catalog = state
        .factory
        .build(RuntimeCtx {
            caller_agent_id: caller,
            policy,
            enable_voice: false,
            voice_only: false,
        })
        .await;
    let cancel = CancellationToken::new();
    let guard = cancel.clone().drop_guard();

    let mut events = String::new();
    for message in messages {
        let rpc::Message::Request { id, method, params } = message else {
            continue;
        };
        let response =
            match rpc::handle_request(&catalog, &method, params.as_ref(), cancel.clone()).await {
                Ok(result) => rpc::result_response(&id, result),
                Err(error) => rpc::error_response(&id, &error),
            };
        events.push_str("event: message\ndata: ");
        events.push_str(&response.to_string());
        events.push_str("\n\n");
    }
    guard.disarm();

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from(events))
        .unwrap_or_default()
}
