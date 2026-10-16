//! Interop: rmcp's real streamable-HTTP client against `/mcp/agents` over a TCP socket. Guards
//! the hand-rolled transport (see the DEVIATION note in `lib.rs`) against drifting from what MCP
//! clients speak.

use std::sync::Arc;

use async_trait::async_trait;
use paseo_agent_types::AgentTimelineItem;
use paseo_mcp::{mcp_router_with, McpOptions};
use paseo_tools::core_api::*;
use paseo_tools::ToolError;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use serde_json::{json, Map, Value};

struct Core;

fn nope<T>() -> Result<T, ToolError> {
    Err(ToolError::new("not stubbed"))
}

#[async_trait]
impl CoreApi for Core {
    fn get_agent(&self, _: &str) -> Option<AgentInfo> {
        None
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
}

#[tokio::test]
async fn rmcp_client_lists_and_calls_tools() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = mcp_router_with(Arc::new(Core), McpOptions::new("t"));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}/mcp/agents"));
    let client = ().serve(transport).await.expect("initialize over streamable http");
    let info = client.peer_info().expect("server info");
    assert_eq!(
        info.server_info.as_ref().map(|i| i.name.as_str()),
        Some("agent-mcp")
    );

    let tools = client.list_all_tools().await.expect("tools/list");
    assert_eq!(tools.len(), 39);
    assert!(tools.iter().any(|t| t.name == "list_providers"));

    // PARITY-QUIRK(mcp-omitted-arguments-rejected): rmcp omits `arguments` for a call without
    // any, and the TS server validates `undefined` against the input object schema, so the call
    // fails for every tool. Clients that send `arguments: {}` are fine.
    let omitted = client
        .call_tool(CallToolRequestParams::new("list_providers"))
        .await
        .expect("tools/call");
    assert_eq!(omitted.is_error, Some(true));
    let text = serde_json::to_value(&omitted.content[0]).unwrap();
    assert!(text["text"]
        .as_str()
        .unwrap()
        .contains("expected object, received undefined"));

    let result = client
        .call_tool(
            CallToolRequestParams::new("list_providers")
                .with_arguments(json!({}).as_object().unwrap().clone()),
        )
        .await
        .expect("tools/call");
    assert_eq!(result.is_error, None);
    assert_eq!(
        result.structured_content.as_ref().unwrap()["providers"][0]["id"],
        json!("claude")
    );
    let text = serde_json::to_value(&result.content[0]).unwrap();
    assert!(text["text"]
        .as_str()
        .unwrap()
        .starts_with("providers_count=1"));
    client.cancel().await.ok();
}
