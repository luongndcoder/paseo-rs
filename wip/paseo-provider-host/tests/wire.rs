use paseo_provider_host::wire::*;
use serde_json::{json, Value};

#[test]
fn permission_response_matches_ts_shape() {
    let allow = PermissionResponse::Allow {
        selected_action_id: Some("implement".into()),
        updated_input: None,
        updated_permissions: None,
    };
    assert_eq!(
        serde_json::to_value(&allow).unwrap(),
        json!({"behavior": "allow", "selectedActionId": "implement"})
    );
    let deny = PermissionResponse::Deny {
        selected_action_id: None,
        message: Some("no".into()),
        interrupt: Some(true),
    };
    assert_eq!(
        serde_json::to_value(&deny).unwrap(),
        json!({"behavior": "deny", "message": "no", "interrupt": true})
    );
}

#[test]
fn session_config_serializes_camel_case_and_skips_none() {
    let mut config = AgentSessionConfig::new("mock", "/tmp/x");
    config.model = Some("ten-second-stream".into());
    config.thinking_option_id = Some("low".into());
    assert_eq!(
        serde_json::to_value(&config).unwrap(),
        json!({"provider": "mock", "cwd": "/tmp/x", "model": "ten-second-stream", "thinkingOptionId": "low"})
    );
}

#[test]
fn stream_events_decode_wire_json() {
    let event: AgentStreamEvent = serde_json::from_value(json!({
        "type": "turn_completed", "provider": "mock", "turnId": "t1",
        "usage": {"inputTokens": 3, "outputTokens": 4, "totalCostUsd": 0.5}
    }))
    .unwrap();
    assert_eq!(
        event,
        AgentStreamEvent::TurnCompleted {
            provider: "mock".into(),
            turn_id: Some("t1".into()),
            usage: Some(AgentUsage {
                input_tokens: Some(3),
                output_tokens: Some(4),
                total_cost_usd: Some(0.5),
                ..AgentUsage::default()
            }),
        }
    );
}

#[test]
fn permission_requested_event_round_trips() {
    let wire = json!({
        "type": "permission_requested", "provider": "mock", "turnId": "t1",
        "request": {
            "id": "p1", "provider": "mock", "name": "MockPlanApproval", "kind": "plan",
            "input": {"plan": "1. do it"},
            "actions": [{"id": "implement", "label": "Implement", "behavior": "allow",
                         "variant": "primary", "intent": "implement"}]
        }
    });
    let event: AgentStreamEvent = serde_json::from_value(wire.clone()).unwrap();
    let AgentStreamEvent::PermissionRequested { request, .. } = &event else {
        panic!("wrong variant: {event:?}");
    };
    assert_eq!(request.kind, PermissionKind::Plan);
    assert_eq!(serde_json::to_value(&event).unwrap(), wire);
}

#[test]
fn unknown_event_types_decode_as_unknown() {
    let event: AgentStreamEvent =
        serde_json::from_value(json!({"type": "provider_subagent", "provider": "x", "event": {}}))
            .unwrap();
    assert_eq!(event, AgentStreamEvent::Unknown);
}

#[test]
fn fetch_catalog_options_use_scope_tag() {
    let global = serde_json::to_value(FetchCatalogOptions::Global { force: false }).unwrap();
    assert_eq!(global, json!({"scope": "global", "force": false}));
    let workspace = serde_json::to_value(FetchCatalogOptions::Workspace {
        cwd: "/w".into(),
        force: true,
    })
    .unwrap();
    assert_eq!(
        workspace,
        json!({"scope": "workspace", "cwd": "/w", "force": true})
    );
}

#[test]
fn prompt_input_is_text_or_blocks() {
    assert_eq!(
        serde_json::to_value(PromptInput::from("hi")).unwrap(),
        json!("hi")
    );
    let blocks = PromptInput::Blocks(vec![json!({"type": "text", "text": "hi"})]);
    assert_eq!(
        serde_json::to_value(blocks).unwrap(),
        json!([{"type": "text", "text": "hi"}])
    );
}

#[test]
fn run_result_and_session_info_decode() {
    let run: AgentRunResult = serde_json::from_value(json!({
        "sessionId": "s", "finalText": "hello", "timeline": [{"type": "assistant_message", "text": "hello"}],
        "canceled": false
    }))
    .unwrap();
    assert_eq!(run.final_text, "hello");
    assert_eq!(run.canceled, Some(false));

    let info: SessionInfo = serde_json::from_value(json!({
        "handle": "s1", "provider": "mock", "sessionId": null,
        "capabilities": {"supportsStreaming": true}, "features": [], "initialTimeline": [],
        "persistence": {"provider": "mock", "sessionId": "abc"}
    }))
    .unwrap();
    assert_eq!(info.session_id, None);
    assert_eq!(info.persistence.unwrap().session_id, "abc");
    let _: Value = Value::Null;
}
