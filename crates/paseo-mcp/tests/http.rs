//! HTTP behavior of `/mcp/agents`: auth (token, password, 401), 404 when disabled, 405 for
//! GET/DELETE, transport validation, JSON-RPC handling and request-abort cancellation.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use paseo_agent_types::AgentTimelineItem;
use paseo_core::PaseoToolPolicy;
use paseo_mcp::{mcp_router_with, McpOptions};
use paseo_tools::core_api::*;
use paseo_tools::ToolError;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const CAP_TOKEN: &str = "cap-token-abc123";

#[derive(Default)]
struct TestCore {
    speak_started: AtomicBool,
    speak_cancelled: Arc<AtomicBool>,
}

fn nope<T>() -> Result<T, ToolError> {
    Err(ToolError::new("not stubbed"))
}

#[async_trait]
impl CoreApi for TestCore {
    fn has(&self, capability: Capability) -> bool {
        matches!(
            capability,
            Capability::Terminals | Capability::Schedules | Capability::WorkspaceRegistry
        )
    }
    fn paseo_tool_policy(&self, id: &str) -> Option<PaseoToolPolicy> {
        (id == "restricted").then(|| PaseoToolPolicy {
            enabled: None,
            disabled_tools: vec!["list_providers".into()],
        })
    }
    fn caller_context(&self, id: &str) -> Option<CallerContext> {
        (id == "voice-caller").then(|| CallerContext {
            enable_voice_tools: true,
            ..CallerContext::default()
        })
    }
    fn get_agent(&self, id: &str) -> Option<AgentInfo> {
        matches!(id, "voice-caller" | "restricted").then(|| AgentInfo {
            id: id.into(),
            provider: "claude".into(),
            cwd: "/work".into(),
            lifecycle: "idle".into(),
            ..AgentInfo::default()
        })
    }
    fn list_agent_permissions(&self) -> Vec<AgentPermissions> {
        vec![]
    }
    async fn agent_status_snapshot(
        &self,
        _: &str,
    ) -> Result<Option<AgentStatusSnapshot>, ToolError> {
        nope()
    }
    async fn list_agent_snapshots(&self, _: bool, _: bool) -> Result<Vec<Value>, ToolError> {
        nope()
    }
    async fn ensure_agent_loaded(&self, _: &str) -> Result<(), ToolError> {
        nope()
    }
    fn agent_timeline(&self, _: &str) -> Vec<AgentTimelineItem> {
        vec![]
    }
    async fn create_agent(&self, _: CreateAgentRequest) -> Result<CreateAgentResult, ToolError> {
        nope()
    }
    async fn send_prompt_to_agent(
        &self,
        _: SendPromptRequest,
        _: OnDispatch<'_>,
    ) -> Result<PromptDisposition, ToolError> {
        nope()
    }
    async fn wait_for_agent_run_start(&self, _: &str) -> Result<(), ToolError> {
        nope()
    }
    fn setup_finish_notification(&self, _: FinishNotificationRequest) -> StopNotification {
        Box::new(|| {})
    }
    async fn cancel_agent_run(&self, _: &str) -> Result<bool, ToolError> {
        nope()
    }
    async fn archive_agent(&self, _: &str) -> Result<(), ToolError> {
        nope()
    }
    async fn close_agent(&self, _: &str) -> Result<(), ToolError> {
        nope()
    }
    async fn update_agent_metadata(
        &self,
        _: &str,
        _: Option<String>,
        _: Option<Map<String, Value>>,
    ) -> Result<(), ToolError> {
        nope()
    }
    async fn set_agent_mode(&self, _: &str, _: &str) -> Result<(), ToolError> {
        nope()
    }
    async fn set_agent_model(&self, _: &str, _: Option<&str>) -> Result<(), ToolError> {
        nope()
    }
    async fn set_agent_thinking_option(&self, _: &str, _: Option<&str>) -> Result<(), ToolError> {
        nope()
    }
    async fn set_agent_feature(&self, _: &str, _: &str, _: &Value) -> Result<(), ToolError> {
        nope()
    }
    async fn respond_to_permission(&self, _: &str, _: &str, _: &Value) -> Result<(), ToolError> {
        nope()
    }
    async fn list_providers(&self) -> Result<Vec<ProviderEntry>, ToolError> {
        Ok(vec![ProviderEntry {
            provider: "claude".into(),
            label: Some("Claude".into()),
            enabled: true,
            status: "ready".into(),
            ..ProviderEntry::default()
        }])
    }
    async fn get_provider(&self, _: &str, _: &str) -> Result<ProviderEntry, ToolError> {
        nope()
    }
    async fn list_models(&self, _: &str) -> Result<Vec<Value>, ToolError> {
        nope()
    }
    async fn list_draft_features(&self, _: Value) -> Result<Vec<Value>, ToolError> {
        nope()
    }
    fn has_speak_handler(&self, _: &str) -> bool {
        true
    }
    async fn speak(&self, _: &str, _: &str, cancel: CancellationToken) -> Result<(), ToolError> {
        self.speak_started.store(true, Ordering::SeqCst);
        // Detached work (a playback task) is what the token exists for: dropping the request
        // future stops everything it awaits, the token stops what was spawned.
        let flag = self.speak_cancelled.clone();
        tokio::spawn(async move {
            cancel.cancelled().await;
            flag.store(true, Ordering::SeqCst);
        });
        std::future::pending::<()>().await;
        Ok(())
    }
}

fn router(core: Arc<TestCore>, options: McpOptions) -> Router {
    mcp_router_with(core, options)
}

fn open_router() -> Router {
    router(Arc::new(TestCore::default()), McpOptions::new(CAP_TOKEN))
}

struct Reply {
    status: StatusCode,
    content_type: String,
    body: String,
}

impl Reply {
    /// The JSON payloads of an SSE body (`event: message` / `data: <json>`).
    fn events(&self) -> Vec<Value> {
        self.body
            .split("\n\n")
            .filter(|chunk| !chunk.is_empty())
            .map(|chunk| {
                let data = chunk
                    .strip_prefix("event: message\ndata: ")
                    .expect("sse event");
                serde_json::from_str(data).unwrap()
            })
            .collect()
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap()
    }
}

async fn send(router: Router, request: Request<Body>) -> Reply {
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        content_type,
        body: String::from_utf8(body.to_vec()).unwrap(),
    }
}

fn post(uri: &str, body: &Value) -> Request<Body> {
    Request::post(uri)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn rpc(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

const UNAUTHORIZED: &str = r#"{"error":"Unauthorized"}"#;

#[tokio::test]
async fn tools_list_over_http_equals_the_golden_default_list() {
    let fixture: Value = serde_json::from_str(
        &std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/p4/tool-catalog/tools-list.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let golden = fixture["lists"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["name"] == "default")
        .unwrap();

    let reply = send(
        open_router(),
        post("/mcp/agents", &rpc(1, "tools/list", json!({}))),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.content_type, "text/event-stream");
    let events = reply.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["id"], 1);
    assert_eq!(events[0]["jsonrpc"], "2.0");
    assert_eq!(
        serde_json::to_string(&events[0]["result"]).unwrap(),
        serde_json::to_string(&golden["mcpToolsList"]).unwrap()
    );
}

#[tokio::test]
async fn initialize_ping_and_unknown_methods() {
    let reply = send(
        open_router(),
        post(
            "/mcp/agents",
            &rpc(7, "initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}})),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.events()[0],
        json!({"result": {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": true}},
            "serverInfo": {"name": "agent-mcp", "version": "2.0.0"},
        }, "jsonrpc": "2.0", "id": 7})
    );
    // An unsupported client version negotiates down to the latest.
    let reply = send(
        open_router(),
        post("/mcp/agents", &rpc(1, "initialize", json!({"protocolVersion": "1999-01-01", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}))),
    )
    .await;
    assert_eq!(reply.events()[0]["result"]["protocolVersion"], "2025-11-25");
    // initialize without params is a validation failure of the request schema.
    let reply = send(
        open_router(),
        post("/mcp/agents", &rpc(1, "initialize", Value::Null)),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);

    let reply = send(
        open_router(),
        post("/mcp/agents", &rpc(2, "ping", json!({}))),
    )
    .await;
    assert_eq!(reply.events()[0]["result"], json!({}));

    let reply = send(
        open_router(),
        post("/mcp/agents", &rpc(3, "resources/list", json!({}))),
    )
    .await;
    assert_eq!(
        reply.events()[0],
        json!({"jsonrpc": "2.0", "id": 3, "error": {"code": -32601, "message": "Method not found"}})
    );
}

#[tokio::test]
async fn tools_call_over_http_returns_text_and_structured_content() {
    let reply = send(
        open_router(),
        post(
            "/mcp/agents",
            &rpc(
                5,
                "tools/call",
                json!({"name": "list_providers", "arguments": {}}),
            ),
        ),
    )
    .await;
    let result = &reply.events()[0]["result"];
    assert!(result["isError"].is_null());
    assert_eq!(result["structuredContent"]["providers"][0]["id"], "claude");
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("providers_count=1\nproviders_ids=claude\n\n{"));

    let reply = send(
        open_router(),
        post(
            "/mcp/agents",
            &rpc(
                6,
                "tools/call",
                json!({"name": "list_models", "arguments": {}}),
            ),
        ),
    )
    .await;
    let result = &reply.events()[0]["result"];
    assert_eq!(result["isError"], true);
    assert!(result["content"][0]["text"].as_str().unwrap().starts_with(
        "MCP error -32602: Input validation error: Invalid arguments for tool list_models: ["
    ));

    let reply = send(
        open_router(),
        post(
            "/mcp/agents",
            &rpc(8, "tools/call", json!({"name": "nope", "arguments": {}})),
        ),
    )
    .await;
    assert_eq!(
        reply.events()[0]["result"]["content"][0]["text"],
        "MCP error -32602: Tool nope not found"
    );
}

#[tokio::test]
async fn malformed_tool_call_params_are_internal_errors_with_the_zod_issues() {
    let reply = send(
        open_router(),
        post(
            "/mcp/agents",
            &rpc(
                9,
                "tools/call",
                json!({"name": "list_providers", "arguments": "str"}),
            ),
        ),
    )
    .await;
    let error = &reply.events()[0]["error"];
    assert_eq!(error["code"], -32603);
    assert_eq!(
        error["message"],
        "[\n  {\n    \"expected\": \"record\",\n    \"code\": \"invalid_type\",\n    \"path\": [\n      \"params\",\n      \"arguments\"\n    ],\n    \"message\": \"Invalid input: expected record, received string\"\n  }\n]"
    );
    // Omitted arguments reach the tool validation as `undefined`.
    let reply = send(
        open_router(),
        post(
            "/mcp/agents",
            &rpc(10, "tools/call", json!({"name": "list_providers"})),
        ),
    )
    .await;
    assert!(reply.events()[0]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("expected object, received undefined"));
}

#[tokio::test]
async fn caller_agent_id_selects_the_catalog() {
    async fn names(query: &str) -> Vec<String> {
        let reply = send(
            open_router(),
            post(
                &format!("/mcp/agents{query}"),
                &rpc(1, "tools/list", json!({})),
            ),
        )
        .await;
        reply.events()[0]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    }
    let anonymous = names("").await;
    assert_eq!(anonymous.len(), 39);
    assert!(!anonymous.contains(&"speak".to_owned()));
    // The caller context turns voice tools on.
    let voice = names("?callerAgentId=voice-caller").await;
    assert_eq!(voice[0], "speak");
    assert_eq!(voice.len(), 40);
    // The caller's provider policy removes tools from the list.
    let restricted = names("?callerAgentId=restricted").await;
    assert!(!restricted.contains(&"list_providers".to_owned()));
    assert_eq!(restricted.len(), 38);
    // An empty id is no caller, a repeated key takes the first.
    assert_eq!(names("?callerAgentId=").await.len(), 39);
    assert_eq!(
        names("?callerAgentId=voice-caller&callerAgentId=restricted")
            .await
            .len(),
        40
    );
    // Disabled tools cannot be called either.
    let reply = send(
        open_router(),
        post(
            "/mcp/agents?callerAgentId=restricted",
            &rpc(
                2,
                "tools/call",
                json!({"name": "list_providers", "arguments": {}}),
            ),
        ),
    )
    .await;
    assert_eq!(
        reply.events()[0]["result"]["content"][0]["text"],
        "MCP error -32602: Tool list_providers not found"
    );
}

#[tokio::test]
async fn get_and_delete_are_405_and_never_start_a_session() {
    for method in ["GET", "DELETE"] {
        let request = Request::builder()
            .method(method)
            .uri("/mcp/agents")
            .body(Body::empty())
            .unwrap();
        let reply = send(open_router(), request).await;
        assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED, "{method}");
        assert_eq!(
            reply.json(),
            json!({"jsonrpc": "2.0", "error": {"code": -32000, "message": "Method not allowed"}, "id": null})
        );
    }
}

#[tokio::test]
async fn disabled_endpoint_is_404_before_auth() {
    let options = McpOptions::new(CAP_TOKEN);
    options.enabled.store(false, Ordering::Relaxed);
    let router = router(Arc::new(TestCore::default()), options.clone());
    for request in [
        post("/mcp/agents", &rpc(1, "ping", json!({}))),
        Request::get("/mcp/agents").body(Body::empty()).unwrap(),
    ] {
        let reply = send(router.clone(), request).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
        assert_eq!(reply.body, r#"{"error":"Agent MCP endpoint disabled"}"#);
        assert!(reply.content_type.starts_with("application/json"));
    }
    // Re-enabling takes effect on the next request.
    options.enabled.store(true, Ordering::Relaxed);
    let reply = send(router, post("/mcp/agents", &rpc(1, "ping", json!({})))).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn password_protected_endpoint_accepts_token_or_password() {
    let mut options = McpOptions::new(CAP_TOKEN);
    options.password_hash = Some(bcrypt::hash("correct-password", 4).unwrap());
    let router = router(Arc::new(TestCore::default()), options);
    let ping = rpc(1, "ping", json!({}));
    let with_auth = |value: Option<&str>| {
        let mut builder = Request::post("/mcp/agents")
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json");
        if let Some(value) = value {
            builder = builder.header("authorization", value);
        }
        builder.body(Body::from(ping.to_string())).unwrap()
    };

    for (auth, want) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some("Bearer wrong-token"), StatusCode::UNAUTHORIZED),
        (Some("Basic correct-password"), StatusCode::UNAUTHORIZED),
        (Some("Bearer cap-token-abc123"), StatusCode::OK),
        (Some("Bearer correct-password"), StatusCode::OK),
    ] {
        let reply = send(router.clone(), with_auth(auth)).await;
        assert_eq!(reply.status, want, "{auth:?}");
        if want == StatusCode::UNAUTHORIZED {
            assert_eq!(reply.body, UNAUTHORIZED);
            assert!(reply.content_type.starts_with("application/json"));
        }
    }
    // Auth runs before the method check: an unauthenticated GET is a 401, not a 405.
    let reply = send(
        router.clone(),
        Request::get("/mcp/agents").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    // An authenticated GET is 405.
    let reply = send(
        router,
        Request::get("/mcp/agents")
            .header("authorization", "Bearer cap-token-abc123")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(reply.status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn transport_validation() {
    let body = rpc(1, "ping", json!({}));
    // Accept must name both content types.
    for accept in [None, Some("application/json"), Some("text/event-stream")] {
        let mut builder = Request::post("/mcp/agents").header("content-type", "application/json");
        if let Some(accept) = accept {
            builder = builder.header("accept", accept);
        }
        let reply = send(
            open_router(),
            builder.body(Body::from(body.to_string())).unwrap(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_ACCEPTABLE);
        assert_eq!(
            reply.json()["error"]["message"],
            "Not Acceptable: Client must accept both application/json and text/event-stream"
        );
    }
    // Content-Type must be JSON.
    let request = Request::post("/mcp/agents")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "text/plain")
        .body(Body::from(body.to_string()))
        .unwrap();
    let reply = send(open_router(), request).await;
    assert_eq!(reply.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(reply.json()["error"]["code"], -32000);
    // Invalid JSON and invalid JSON-RPC.
    let invalid_json = Request::post("/mcp/agents")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .body(Body::from("{nope"))
        .unwrap();
    let reply = send(open_router(), invalid_json).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json()["error"],
        json!({"code": -32700, "message": "Parse error: Invalid JSON"})
    );
    let reply = send(
        open_router(),
        post(
            "/mcp/agents",
            &json!({"jsonrpc": "1.0", "id": 1, "method": "ping"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json()["error"]["message"],
        "Parse error: Invalid JSON-RPC message"
    );
    // Unsupported protocol version header on a non-initialize request.
    let request = Request::post("/mcp/agents")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "1999-01-01")
        .body(Body::from(body.to_string()))
        .unwrap();
    let reply = send(open_router(), request).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.json()["error"]["message"]
        .as_str()
        .unwrap()
        .starts_with(
        "Bad Request: Unsupported protocol version: 1999-01-01 (supported versions: 2025-11-25, "
    ));
    // A supported version passes.
    let request = Request::post("/mcp/agents")
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", "2025-06-18")
        .body(Body::from(body.to_string()))
        .unwrap();
    assert_eq!(send(open_router(), request).await.status, StatusCode::OK);
}

#[tokio::test]
async fn notifications_are_accepted_and_batches_answer_every_request() {
    let reply = send(
        open_router(),
        post(
            "/mcp/agents",
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    assert!(reply.body.is_empty());

    let batch = json!([
        {"jsonrpc": "2.0", "id": 1, "method": "ping"},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": "b", "method": "ping"},
    ]);
    let reply = send(open_router(), post("/mcp/agents", &batch)).await;
    assert_eq!(reply.status, StatusCode::OK);
    let ids: Vec<Value> = reply.events().iter().map(|e| e["id"].clone()).collect();
    assert_eq!(ids, vec![json!(1), json!("b")]);

    // initialize cannot share a batch.
    let batch = json!([
        rpc(1, "initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}})),
        {"jsonrpc": "2.0", "id": 2, "method": "ping"},
    ]);
    let reply = send(open_router(), post("/mcp/agents", &batch)).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.json()["error"]["code"], -32600);
}

#[tokio::test]
async fn aborting_the_request_cancels_the_running_tool() {
    let core = Arc::new(TestCore::default());
    let router = router(core.clone(), McpOptions::new(CAP_TOKEN));
    let request = post(
        "/mcp/agents?callerAgentId=voice-caller",
        &rpc(
            1,
            "tools/call",
            json!({"name": "speak", "arguments": {"text": "hello"}}),
        ),
    );
    let task = tokio::spawn(router.oneshot(request));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !core.speak_started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("speak started");
    assert!(!core.speak_cancelled.load(Ordering::SeqCst));
    task.abort();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !core.speak_cancelled.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the handler's cancellation token fired");
}
