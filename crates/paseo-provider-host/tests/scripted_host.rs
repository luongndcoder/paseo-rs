//! Client behavior against scripted `sh` hosts. Request ids are deterministic (1, 2, ...) because
//! every call here is sequential, so the scripts can answer with fixed ids.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use paseo_agent_types::AgentStreamEvent as TypedEvent;
use paseo_provider_api::{
    AgentClient, AgentSessionConfig, ProviderError, ToolCallHandler, ToolCallRequest,
    ToolCallResult,
};
use paseo_provider_host::wire::{AgentSessionConfig as WireConfig, AgentStreamEvent};
use paseo_provider_host::{
    HostAgentClient, HostCallbacks, HostConfig, ProviderHostClient, SessionEvent,
};
use serde_json::{json, Value};
use tokio::time::timeout;

const INIT: &str = r#"read l; echo '{"id":1,"result":{"protocolVersion":1,"providers":[]}}'"#;
const SESSION: &str = r#"{"handle":"s1","provider":"mock","sessionId":"sess","capabilities":{},"features":[],"initialTimeline":[],"persistence":null}"#;

fn event_line(seq: u64, event: Value) -> String {
    format!(
        "echo '{}'",
        json!({"method": "session.event", "params": {"handle": "s1", "seq": seq, "event": event}})
    )
}

fn turn_started(seq: u64, turn: &str) -> String {
    event_line(
        seq,
        json!({"type": "turn_started", "provider": "mock", "turnId": turn}),
    )
}

fn turn_completed(seq: u64, turn: &str) -> String {
    event_line(
        seq,
        json!({"type": "turn_completed", "provider": "mock", "turnId": turn}),
    )
}

/// A host that answers `host.initialize`, then `provider.createSession` with handle `s1`, then
/// runs `body`.
fn script(body: &[String]) -> HostConfig {
    let mut lines = vec![
        INIT.to_owned(),
        format!("read l; echo '{{\"id\":2,\"result\":{SESSION}}}'"),
    ];
    lines.extend(body.iter().cloned());
    HostConfig::new("sh", ["-c".to_owned(), lines.join("\n")])
}

async fn open(config: HostConfig) -> (ProviderHostClient, paseo_provider_host::Session) {
    open_with(config, HostCallbacks::default()).await
}

async fn open_with(
    config: HostConfig,
    callbacks: HostCallbacks,
) -> (ProviderHostClient, paseo_provider_host::Session) {
    let client = ProviderHostClient::spawn_with(config, None, callbacks)
        .await
        .expect("spawn scripted host");
    let session = client
        .create_session(&WireConfig::new("mock", "/tmp"), None, None)
        .await
        .expect("create session");
    (client, session)
}

async fn drain(stream: &mut paseo_provider_host::EventStream) -> Vec<SessionEvent> {
    let mut seen = Vec::new();
    while let Some(event) = timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("stream should end")
    {
        seen.push(event);
    }
    seen
}

#[tokio::test]
async fn seq_gap_fails_the_session_with_one_terminal_event() {
    let (_client, session) = open(script(&[
        turn_started(1, "t1"),
        // seq 2 never arrives
        event_line(
            3,
            json!({"type": "timeline", "provider": "mock", "turnId": "t1",
                             "item": {"type": "assistant_message", "text": "late"}}),
        ),
        "sleep 30".into(),
    ]))
    .await;
    let mut events = session.take_events().unwrap();
    let seen = drain(&mut events).await;
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(!seen[0].synthetic);
    assert!(seen[1].synthetic);
    match &seen[1].event {
        AgentStreamEvent::TurnFailed {
            error,
            code,
            turn_id,
            ..
        } => {
            assert!(error.contains("expected 2, got 3"), "{error}");
            assert_eq!(code.as_deref(), Some("provider_host_seq_gap"));
            assert_eq!(turn_id.as_deref(), Some("t1"));
        }
        other => panic!("unexpected event {other:?}"),
    }
}

#[tokio::test]
async fn host_exit_yields_one_terminal_event_per_active_turn() {
    let (_client, session) = open(script(&[
        turn_started(1, "t1"),
        turn_started(2, "t2"),
        turn_completed(3, "t1"),
        "exit 0".into(),
    ]))
    .await;
    let mut events = session.take_events().unwrap();
    let seen = drain(&mut events).await;
    let synthetic: Vec<_> = seen.iter().filter(|e| e.synthetic).collect();
    assert_eq!(synthetic.len(), 1, "{seen:?}");
    match &synthetic[0].event {
        AgentStreamEvent::TurnFailed {
            error,
            turn_id,
            provider,
            ..
        } => {
            assert_eq!(error, "Provider host exited");
            assert_eq!(turn_id.as_deref(), Some("t2"));
            assert_eq!(provider, "mock");
        }
        other => panic!("unexpected event {other:?}"),
    }
}

#[tokio::test]
async fn host_exit_with_no_active_turn_adds_no_event() {
    let (_client, session) = open(script(&[
        turn_started(1, "t1"),
        turn_completed(2, "t1"),
        "exit 0".into(),
    ]))
    .await;
    let mut events = session.take_events().unwrap();
    let seen = drain(&mut events).await;
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|e| !e.synthetic));
}

#[tokio::test]
async fn start_turn_response_counts_as_active_before_turn_started() {
    let (_client, session) = open(script(&[
        r#"read l; echo '{"id":3,"result":{"turnId":"t9"}}'"#.into(),
        "exit 0".into(),
    ]))
    .await;
    let mut events = session.take_events().unwrap();
    let started = session.start_turn("go", None).await.unwrap();
    assert_eq!(started.turn_id, "t9");
    let seen = drain(&mut events).await;
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(seen[0].synthetic);
    assert_eq!(
        seen[0].event.clone(),
        AgentStreamEvent::TurnFailed {
            provider: "mock".into(),
            error: "Provider host exited".into(),
            code: None,
            diagnostic: None,
            turn_id: Some("t9".into()),
        }
    );
}

#[tokio::test]
async fn turn_finished_before_start_turn_response_is_not_active() {
    let (_client, session) = open(script(&[
        turn_started(1, "t9"),
        turn_completed(2, "t9"),
        r#"read l; echo '{"id":3,"result":{"turnId":"t9"}}'"#.into(),
        "exit 0".into(),
    ]))
    .await;
    let mut events = session.take_events().unwrap();
    session.start_turn("go", None).await.unwrap();
    let seen = drain(&mut events).await;
    assert!(seen.iter().all(|e| !e.synthetic), "{seen:?}");
}

struct Echo;

#[async_trait]
impl ToolCallHandler for Echo {
    async fn call(&self, request: ToolCallRequest) -> Result<ToolCallResult, ProviderError> {
        if request.tool == "boom" {
            return Err(ProviderError::provider("tool exploded"));
        }
        Ok(ToolCallResult {
            content: vec![json!({"type": "text", "text": format!(
                "{}:{}:{}", request.agent_id.unwrap_or_default(), request.tool, request.arguments)
            })],
            structured_content: None,
            is_error: None,
        })
    }
}

fn reverse_call_script(out: &std::path::Path, params: Value) -> Vec<String> {
    let request = json!({"id": "h1", "method": "tool.call", "params": params});
    vec![
        format!("echo '{request}'"),
        format!(
            "read -r resp; printf '%s\\n' \"$resp\" > '{}'",
            out.display()
        ),
        "sleep 1".into(),
    ]
}

async fn read_response(out: &std::path::Path) -> Value {
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(out) {
            if !text.trim().is_empty() {
                return serde_json::from_str(text.trim()).unwrap();
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("host never received a response");
}

#[tokio::test]
async fn tool_call_reverse_request_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("resp.json");
    let callbacks = HostCallbacks {
        tool_handler: Some(Arc::new(Echo)),
    };
    let (_client, _session) = open_with(
        script(&reverse_call_script(
            &out,
            json!({"agentId": "a1", "tool": "list_agents", "arguments": {"x": 1}}),
        )),
        callbacks,
    )
    .await;
    let response = read_response(&out).await;
    assert_eq!(response["id"], "h1");
    assert_eq!(
        response["result"]["content"][0]["text"],
        "a1:list_agents:{\"x\":1}"
    );
}

#[tokio::test]
async fn tool_call_handler_error_becomes_provider_error_response() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("resp.json");
    let callbacks = HostCallbacks {
        tool_handler: Some(Arc::new(Echo)),
    };
    let (_client, _session) = open_with(
        script(&reverse_call_script(
            &out,
            json!({"tool": "boom", "arguments": {}}),
        )),
        callbacks,
    )
    .await;
    let response = read_response(&out).await;
    assert_eq!(response["error"]["code"], -32000);
    assert_eq!(response["error"]["message"], "tool exploded");
}

#[tokio::test]
async fn tool_call_without_handler_is_method_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("resp.json");
    let (_client, _session) = open(script(&reverse_call_script(
        &out,
        json!({"tool": "list_agents", "arguments": {}}),
    )))
    .await;
    let response = read_response(&out).await;
    assert_eq!(response["id"], "h1");
    assert_eq!(response["error"]["code"], -32601);
}

#[tokio::test]
async fn invalid_tool_call_params_are_invalid_params() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("resp.json");
    let callbacks = HostCallbacks {
        tool_handler: Some(Arc::new(Echo)),
    };
    let (_client, _session) = open_with(
        script(&reverse_call_script(&out, json!({"arguments": {}}))),
        callbacks,
    )
    .await;
    let response = read_response(&out).await;
    assert_eq!(response["error"]["code"], -32602);
}

#[tokio::test]
async fn import_session_registers_the_nested_session_channel() {
    let import_result = json!({"session": serde_json::from_str::<Value>(SESSION).unwrap(),
        "config": {"provider": "mock", "cwd": "/tmp"},
        "persistence": {"provider": "mock", "sessionId": "sess"},
        "timeline": [{"item": {"type": "assistant_message", "text": "old"}, "timestamp": "t"}],
        "providerSubagentEvents": []});
    let config = HostConfig::new(
        "sh",
        [
            "-c".to_owned(),
            [
                INIT.to_owned(),
                format!(
                    "read l; echo '{}'",
                    json!({"id": 2, "result": import_result})
                ),
                turn_started(1, "t1"),
                turn_completed(2, "t1"),
                "sleep 1".into(),
            ]
            .join("\n"),
        ],
    );
    let client = ProviderHostClient::spawn(config, None).await.unwrap();
    let imported = client
        .import_session("mock", &json!({}), &json!({}))
        .await
        .unwrap();
    assert_eq!(imported.timeline.len(), 1);
    let mut events = imported.session.take_events().unwrap();
    let first = timeout(Duration::from_secs(10), events.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.seq, 1);
}

#[tokio::test]
async fn adapter_forwards_typed_events_and_synthetic_failure() {
    let config = script(&[
        turn_started(1, "t1"),
        event_line(
            2,
            json!({"type": "mode_changed", "provider": "mock", "currentModeId": "plan",
                   "availableModes": [{"id": "plan", "label": "Plan"}]}),
        ),
        "exit 0".into(),
    ]);
    let host = ProviderHostClient::spawn(config, None).await.unwrap();
    let client = HostAgentClient::new(host, "mock");
    let session = client
        .create_session(&AgentSessionConfig::new("mock", "/tmp"), None, None)
        .await
        .unwrap();
    assert_eq!(session.id(), Some("sess"));
    let mut events = session.subscribe();
    let mut seen = Vec::new();
    while let Some(event) = timeout(Duration::from_secs(10), events.next())
        .await
        .unwrap()
    {
        seen.push(event);
    }
    assert!(matches!(seen[0], TypedEvent::TurnStarted { .. }));
    assert!(matches!(seen[1], TypedEvent::ModeChanged { .. }));
    assert!(matches!(
        &seen[2],
        TypedEvent::TurnFailed { error, turn_id, .. }
            if error == "Provider host exited" && turn_id.as_deref() == Some("t1")
    ));
    assert_eq!(seen.len(), 3);
    // Single consumer: a second subscribe is a finished stream.
    assert!(session.subscribe().next().await.is_none());
}

fn malformed_turn_completed(seq: u64, turn: &str) -> String {
    event_line(
        seq,
        json!({"type": "turn_completed", "provider": "mock", "turnId": turn,
               "usage": {"inputTokens": "many"}}),
    )
}

#[tokio::test]
async fn undecodable_turn_completed_still_finishes_the_turn_in_the_client() {
    let (_client, session) = open(script(&[
        turn_started(1, "t1"),
        malformed_turn_completed(2, "t1"),
        "exit 0".into(),
    ]))
    .await;
    let mut events = session.take_events().unwrap();
    let seen = drain(&mut events).await;
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(seen.iter().all(|e| !e.synthetic), "{seen:?}");
    assert_eq!(seen[1].raw["type"], "turn_completed");
}

#[tokio::test]
async fn adapter_turns_an_undecodable_terminal_event_into_turn_failed() {
    let config = script(&[
        turn_started(1, "t1"),
        malformed_turn_completed(2, "t1"),
        "exit 0".into(),
    ]);
    let host = ProviderHostClient::spawn(config, None).await.unwrap();
    let client = HostAgentClient::new(host, "mock");
    let session = client
        .create_session(&AgentSessionConfig::new("mock", "/tmp"), None, None)
        .await
        .unwrap();
    let mut events = session.subscribe();
    let mut seen = Vec::new();
    while let Some(event) = timeout(Duration::from_secs(10), events.next())
        .await
        .unwrap()
    {
        seen.push(event);
    }
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(matches!(
        &seen[1],
        TypedEvent::TurnFailed { code, turn_id, .. }
            if code.as_deref() == Some("provider_host_decode") && turn_id.as_deref() == Some("t1")
    ));
}
