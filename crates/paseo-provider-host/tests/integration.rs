//! Spawns the real TS provider-host with the mock provider. Skips (with a message) when
//! node/tsx or the TS checkout is unavailable.
//!
//! Environment: `PASEO_PROVIDER_HOST_NODE` (default `node`), `PASEO_PROVIDER_HOST_SERVER_DIR`
//! (the `packages/server` dir of a checkout that contains `src/provider-host/main.ts`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use paseo_provider_host::wire::*;
use paseo_provider_host::{
    code, AllowAll, Error, EventStream, HostConfig, InitializeOptions, PermissionContext,
    PermissionHandler, ProviderHostClient, Session, SessionEvent,
};
use serde_json::{json, Value};
use tokio::time::timeout;

const DEFAULT_SERVER_DIR: &str = "/path/to/paseo/.claude/worktrees/rust-provider-host/packages/server";

fn host_config() -> Option<HostConfig> {
    let node = std::env::var("PASEO_PROVIDER_HOST_NODE").unwrap_or_else(|_| "node".into());
    let server_dir = PathBuf::from(
        std::env::var("PASEO_PROVIDER_HOST_SERVER_DIR")
            .unwrap_or_else(|_| DEFAULT_SERVER_DIR.into()),
    );
    let entry = server_dir.join("src/provider-host/main.ts");
    if !entry.exists() {
        eprintln!("SKIP: {} not found", entry.display());
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
        eprintln!(
            "SKIP: `{node} --import tsx` is unavailable in {}",
            server_dir.display()
        );
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

macro_rules! host {
    ($handler:expr) => {{
        let Some(config) = host_config() else { return };
        ProviderHostClient::spawn(config, $handler)
            .await
            .expect("spawn host")
    }};
}

async fn mock_session(client: &ProviderHostClient, features: Value) -> Session {
    let mut config =
        AgentSessionConfig::new("mock", std::env::current_dir().unwrap().to_string_lossy());
    config.model = Some("ten-second-stream".into());
    config.feature_values = features.as_object().cloned();
    client
        .create_session(&config, None, None)
        .await
        .expect("create session")
}

async fn next_matching(
    events: &mut EventStream,
    pred: impl Fn(&AgentStreamEvent) -> bool,
) -> SessionEvent {
    timeout(Duration::from_secs(15), async {
        loop {
            let event = events.next().await.expect("event stream ended");
            if pred(&event.event) {
                return event;
            }
        }
    })
    .await
    .expect("timed out waiting for event")
}

#[tokio::test]
async fn initialize_lists_providers_and_catalog() {
    let client = host!(None);
    assert_eq!(client.providers().len(), 1);
    assert_eq!(client.providers()[0].id, "mock");
    assert!(client.is_available("mock", None).await.unwrap());
    let catalog = client
        .fetch_catalog("mock", &FetchCatalogOptions::Global { force: false })
        .await
        .unwrap();
    assert!(catalog
        .models
        .iter()
        .any(|m| m["id"] == "ten-second-stream"));
    assert!(!catalog.modes.is_empty());
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn unknown_provider_and_method_are_rpc_errors() {
    let client = host!(None);
    let error = client.is_available("ghost", None).await.unwrap_err();
    assert_eq!(error.rpc_code(), Some(code::UNKNOWN_PROVIDER));
    let error = client
        .call_raw("nope.nothing", json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.rpc_code(), Some(code::METHOD_NOT_FOUND));
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn run_streams_events_in_order_then_resolves() {
    let client = host!(None);
    let session = mock_session(&client, json!({"mockAssistantResponse": "hello from host"})).await;
    let mut events = session.take_events().expect("events");
    assert!(session.take_events().is_none());

    let result = session.run("hi", None).await.unwrap();
    assert_eq!(result.final_text, "hello from host");
    assert_eq!(result.canceled, Some(false));

    let mut seen = Vec::new();
    while let Some(event) = events.try_next() {
        seen.push(event);
    }
    assert!(seen
        .iter()
        .any(|e| matches!(e.event, AgentStreamEvent::TurnStarted { .. })));
    assert!(seen
        .iter()
        .any(|e| matches!(e.event, AgentStreamEvent::TurnCompleted { .. })));
    let seqs: Vec<u64> = seen.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());
    assert!(seen.iter().all(|e| e.handle == session.handle()));
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn start_turn_event_carries_turn_id() {
    let client = host!(None);
    let session = mock_session(&client, json!({"mockAssistantResponse": "turn text"})).await;
    let mut events = session.take_events().unwrap();
    let started = session.start_turn("go", None).await.unwrap();
    let event = next_matching(&mut events, |e| {
        matches!(e, AgentStreamEvent::TurnStarted { .. })
    })
    .await;
    let AgentStreamEvent::TurnStarted { turn_id, .. } = event.event else {
        unreachable!()
    };
    assert_eq!(turn_id.as_deref(), Some(started.turn_id.as_str()));
    next_matching(&mut events, |e| {
        matches!(e, AgentStreamEvent::TurnCompleted { .. })
    })
    .await;
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn permission_round_trip_with_manual_response() {
    let client = host!(None);
    let session = mock_session(&client, json!({})).await;
    let mut events = session.take_events().unwrap();
    let run = session
        .run_cancellable("Emit synthetic plan approval", None)
        .await
        .unwrap();

    let event = next_matching(&mut events, |e| {
        matches!(e, AgentStreamEvent::PermissionRequested { .. })
    })
    .await;
    let AgentStreamEvent::PermissionRequested { request, .. } = event.event else {
        unreachable!()
    };
    assert_eq!(request.kind, PermissionKind::Plan);
    let pending = session.pending_permissions().await.unwrap();
    assert_eq!(
        pending.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        [request.id.as_str()]
    );

    session
        .respond_to_permission(
            &request.id,
            &PermissionResponse::Allow {
                selected_action_id: Some("implement".into()),
                updated_input: None,
                updated_permissions: None,
            },
        )
        .await
        .unwrap();
    let result = run.wait().await.unwrap();
    assert_eq!(
        result.session_id,
        session.info().session_id.clone().unwrap()
    );
    next_matching(&mut events, |e| {
        matches!(e, AgentStreamEvent::PermissionResolved { .. })
    })
    .await;
    assert!(session.pending_permissions().await.unwrap().is_empty());
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn permission_handler_answers_automatically() {
    let client = host!(Some(Arc::new(AllowAll)));
    let session = mock_session(&client, json!({})).await;
    let mut events = session.take_events().unwrap();
    let result = timeout(
        Duration::from_secs(15),
        session.run("Emit synthetic plan approval", None),
    )
    .await
    .expect("run should finish without manual response")
    .unwrap();
    assert_ne!(result.canceled, Some(true));
    let resolved = next_matching(&mut events, |e| {
        matches!(e, AgentStreamEvent::PermissionResolved { .. })
    })
    .await;
    let AgentStreamEvent::PermissionResolved { resolution, .. } = resolved.event else {
        unreachable!()
    };
    assert_eq!(
        resolution,
        PermissionResponse::Allow {
            selected_action_id: Some("implement".into()),
            updated_input: None,
            updated_permissions: None
        }
    );
    client.shutdown().await.unwrap();
}

struct Recorder(tokio::sync::mpsc::UnboundedSender<(PermissionContext, PermissionRequest)>);

#[async_trait]
impl PermissionHandler for Recorder {
    async fn decide(
        &self,
        ctx: PermissionContext,
        request: PermissionRequest,
    ) -> PermissionResponse {
        let _ = self.0.send((ctx, request));
        PermissionResponse::Deny {
            selected_action_id: Some("dismiss".into()),
            message: Some("not now".into()),
            interrupt: None,
        }
    }
}

#[tokio::test]
async fn permission_handler_receives_context_and_can_deny() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let client = host!(Some(Arc::new(Recorder(tx))));
    let session = mock_session(&client, json!({})).await;
    let _events = session.take_events().unwrap();
    let _ = timeout(
        Duration::from_secs(15),
        session.run("Emit synthetic plan approval", None),
    )
    .await
    .expect("run should finish after the handler denies");
    let (ctx, request) = rx.recv().await.expect("handler called");
    assert_eq!(ctx.handle, session.handle());
    assert_eq!(request.name, "MockPlanApproval");
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancel_interrupts_a_running_turn() {
    let client = host!(None);
    let session = mock_session(&client, json!({})).await;
    let mut events = session.take_events().unwrap();
    let run = session.run_cancellable("long", None).await.unwrap();
    next_matching(&mut events, |e| {
        matches!(e, AgentStreamEvent::TurnStarted { .. })
    })
    .await;
    run.cancel().await.unwrap();
    match timeout(Duration::from_secs(15), run.wait())
        .await
        .expect("run settles")
    {
        Ok(result) => assert_eq!(result.canceled, Some(true)),
        Err(error) => assert!(error.is_canceled(), "{error:?}"),
    }
    next_matching(&mut events, |e| {
        matches!(e, AgentStreamEvent::TurnCanceled { .. })
    })
    .await;
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn interrupt_cancels_a_started_turn() {
    let client = host!(None);
    let session = mock_session(&client, json!({})).await;
    let mut events = session.take_events().unwrap();
    session.start_turn("long", None).await.unwrap();
    session.interrupt().await.unwrap();
    next_matching(&mut events, |e| {
        matches!(e, AgentStreamEvent::TurnCanceled { .. })
    })
    .await;
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn modes_models_and_runtime_info() {
    let client = host!(None);
    let session = mock_session(&client, json!({})).await;
    assert!(!session.available_modes().await.unwrap().is_empty());
    assert!(session.current_mode().await.unwrap().is_some());
    session.set_model(Some("one-minute-stream")).await.unwrap();
    let info = session.runtime_info().await.unwrap();
    assert_eq!(info["model"], "one-minute-stream");
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn persistence_resume_yields_new_handle() {
    let client = host!(None);
    let session = mock_session(&client, json!({})).await;
    let persistence = session
        .describe_persistence()
        .await
        .unwrap()
        .expect("persistence");
    assert_eq!(persistence.provider, "mock");
    let resumed = client.resume_session(&persistence, None).await.unwrap();
    assert_ne!(resumed.handle(), session.handle());
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn close_invalidates_the_handle_and_ends_the_stream() {
    let client = host!(None);
    let session = mock_session(&client, json!({})).await;
    let mut events = session.take_events().unwrap();
    session.close().await.unwrap();
    let error = session.interrupt().await.unwrap_err();
    assert_eq!(error.rpc_code(), Some(code::UNKNOWN_SESSION));
    assert!(timeout(Duration::from_secs(5), events.next())
        .await
        .expect("stream ends")
        .is_none());
    client.shutdown().await.unwrap();
}

#[tokio::test]
async fn calls_after_shutdown_fail_with_host_exited() {
    let client = host!(None);
    client.shutdown().await.unwrap();
    let error = client.list_providers().await.unwrap_err();
    assert!(
        matches!(error, Error::HostExited { .. } | Error::Io(_)),
        "{error:?}"
    );
}
