//! `HostAgentClient` / `HostAgentSession` against the real TS provider-host with the mock
//! provider. Skips (with a message) when node/tsx or the TS checkout is unavailable; see
//! `integration.rs` for the environment variables.

use std::path::PathBuf;
use std::time::Duration;

use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem, PermissionResponse};
use paseo_provider_api::{
    replace_with_assistant_message, run_session_turn, AgentClient, AgentSessionConfig,
    FetchCatalogOptions, PromptInput, ProviderError, SteerOptions, SteerResult,
};
use paseo_provider_host::{HostAgentClient, HostConfig, InitializeOptions, ProviderHostClient};
use serde_json::json;
use tokio::time::timeout;

const DEFAULT_SERVER_DIR: &str = "/path/to/paseo/.claude/worktrees/rust-provider-host/packages/server";

fn host_config() -> Option<HostConfig> {
    let node = std::env::var("PASEO_PROVIDER_HOST_NODE").unwrap_or_else(|_| "node".into());
    let server_dir = PathBuf::from(
        std::env::var("PASEO_PROVIDER_HOST_SERVER_DIR")
            .unwrap_or_else(|_| DEFAULT_SERVER_DIR.into()),
    );
    if !server_dir.join("src/provider-host/main.ts").exists() {
        eprintln!(
            "SKIP: provider-host entry not found in {}",
            server_dir.display()
        );
        return None;
    }
    let tsx_ok = std::process::Command::new(&node)
        .args(["--import", "tsx", "-e", "0"])
        .current_dir(&server_dir)
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !tsx_ok {
        eprintln!("SKIP: `{node} --import tsx` is unavailable");
        return None;
    }
    Some(
        HostConfig::node_tsx(node, &server_dir, "src/provider-host/main.ts").with_initialize(
            InitializeOptions {
                is_dev: Some(true),
                providers: Some(vec!["mock".into()]),
                ..InitializeOptions::default()
            },
        ),
    )
}

macro_rules! mock_client {
    () => {{
        let Some(config) = host_config() else { return };
        let host = ProviderHostClient::spawn(config, None)
            .await
            .expect("spawn host");
        (host.clone(), HostAgentClient::new(host, "mock"))
    }};
}

fn mock_config(assistant_response: Option<&str>) -> AgentSessionConfig {
    let mut config =
        AgentSessionConfig::new("mock", std::env::current_dir().unwrap().to_string_lossy());
    config.model = Some("ten-second-stream".into());
    if let Some(text) = assistant_response {
        config.feature_values = json!({"mockAssistantResponse": text}).as_object().cloned();
    }
    config
}

#[tokio::test]
async fn client_level_calls_decode() {
    let (host, client) = mock_client!();
    assert_eq!(client.provider(), "mock");
    assert!(client.is_available(None).await.unwrap());
    assert!(!client.capabilities().await.unwrap().is_empty());
    let catalog = client
        .fetch_catalog(&FetchCatalogOptions::Global { force: false })
        .await
        .unwrap();
    assert!(catalog
        .models
        .iter()
        .any(|m| m["id"] == "ten-second-stream"));
    assert!(client
        .list_importable_sessions(None)
        .await
        .unwrap()
        .is_empty());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn errors_map_to_provider_errors() {
    let (host, _client) = mock_client!();
    let ghost = HostAgentClient::new(host.clone(), "ghost");
    assert!(matches!(
        ghost.is_available(None).await,
        Err(ProviderError::UnknownProvider(_))
    ));
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn run_session_turn_collects_the_final_text() {
    let (host, client) = mock_client!();
    let session = client
        .create_session(&mock_config(Some("hello from adapter")), None, None)
        .await
        .unwrap();
    assert_eq!(session.provider(), "mock");
    assert!(session.id().is_some());
    let mut events = session.subscribe();
    let result = timeout(
        Duration::from_secs(20),
        run_session_turn(
            session.as_ref(),
            &mut events,
            PromptInput::from("hi"),
            None,
            replace_with_assistant_message,
        ),
    )
    .await
    .expect("turn finishes")
    .unwrap();
    assert_eq!(result.final_text, "hello from adapter");
    assert_eq!(Some(result.session_id.as_str()), session.id());
    assert!(result
        .timeline
        .iter()
        .any(|item| matches!(item, AgentTimelineItem::AssistantMessage { .. })));
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_calls_decode_typed_results() {
    let (host, client) = mock_client!();
    let session = client
        .create_session(&mock_config(None), None, None)
        .await
        .unwrap();
    assert!(!session.available_modes().await.unwrap().is_empty());
    assert!(session.current_mode().await.unwrap().is_some());
    session.set_model(Some("one-minute-stream")).await.unwrap();
    let info = session.get_runtime_info().await.unwrap();
    assert_eq!(info.provider, "mock");
    assert_eq!(info.model, Some(Some("one-minute-stream".into())));
    assert!(session.pending_permissions().await.unwrap().is_empty());
    let steered = session
        .steer_active_turn(
            PromptInput::from("steer"),
            SteerOptions {
                expected_turn_id: "nope".into(),
                ..SteerOptions::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        steered,
        SteerResult::Accepted | SteerResult::Unavailable
    ));
    let persistence = session.describe_persistence().await.unwrap().unwrap();
    assert_eq!(persistence.provider, "mock");
    let resumed = client
        .resume_session(&persistence, None, None, None)
        .await
        .unwrap();
    assert_eq!(resumed.provider(), "mock");
    resumed.close().await.unwrap();
    session.close().await.unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn out_of_band_command_events_arrive_on_the_stream() {
    let (host, client) = mock_client!();
    let session = client
        .create_session(&mock_config(None), None, None)
        .await
        .unwrap();
    let mut events = session.subscribe();
    assert!(!session
        .try_handle_out_of_band(PromptInput::from("not a command"))
        .await
        .unwrap());
    assert!(session
        .try_handle_out_of_band(PromptInput::from("/mock handled-command"))
        .await
        .unwrap());
    let event = timeout(Duration::from_secs(10), events.next())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        event,
        AgentStreamEvent::Timeline { ref item, .. }
            if matches!(**item, AgentTimelineItem::AssistantMessage { ref text, .. } if text == "Mock command handled")
    ));
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn permission_round_trip_and_history() {
    let (host, client) = mock_client!();
    let session = client
        .create_session(&mock_config(None), None, None)
        .await
        .unwrap();
    let mut events = session.subscribe();
    let turn = session
        .start_turn(PromptInput::from("Emit synthetic plan approval"), None)
        .await
        .unwrap();
    let request = timeout(Duration::from_secs(15), async {
        loop {
            if let AgentStreamEvent::PermissionRequested { request, .. } =
                events.next().await.expect("stream open")
            {
                return request;
            }
        }
    })
    .await
    .expect("permission requested");
    let pending = session.pending_permissions().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, request.id);
    session
        .respond_to_permission(
            &request.id,
            PermissionResponse::Allow {
                selected_action_id: Some("implement".into()),
                updated_input: None,
                updated_permissions: None,
            },
        )
        .await
        .unwrap();
    timeout(Duration::from_secs(15), async {
        loop {
            match events.next().await.expect("stream open") {
                AgentStreamEvent::TurnCompleted { turn_id, .. }
                | AgentStreamEvent::TurnCanceled { turn_id, .. } => {
                    assert_eq!(turn_id.as_deref(), Some(turn.turn_id.as_str()));
                    return;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("turn ends");
    // History decodes into typed events.
    let _history = session.stream_history().await.unwrap();
    host.shutdown().await.unwrap();
}
