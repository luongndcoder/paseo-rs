//! Ported from `packages/server/src/server/messages.test.ts` (serializeAgentStreamEvent),
//! `packages/protocol/src/messages.tool-call-schema.test.ts` and `timeline-identity.test.ts`.

use paseo_agent_types::*;
use serde_json::{json, Value};

fn tc(status: ToolCallStatus, error: Option<Value>) -> AgentTimelineItem {
    let mut t = ToolCallItem::new(
        "call_123",
        "shell",
        status,
        ToolCallDetail::Shell {
            command: "pwd".into(),
            cwd: None,
            output: None,
            exit_code: None,
        },
    );
    t.error = error;
    AgentTimelineItem::tool_call(t)
}

fn timeline(item: AgentTimelineItem) -> AgentStreamEvent {
    AgentStreamEvent::Timeline {
        item: Box::new(item),
        provider: "codex".into(),
        turn_id: None,
        timestamp: Some("2026-02-13T00:00:00.000Z".into()),
    }
}

fn wire_item(item: AgentTimelineItem) -> Option<Value> {
    to_wire(&timeline(item)).map(|w| w.into_value()["item"].clone())
}

#[test]
fn preserves_turn_identity_on_lifecycle_events() {
    let w = to_wire(&AgentStreamEvent::TurnCanceled {
        provider: "codex".into(),
        turn_id: Some("turn-accepted-1".into()),
        reason: "interrupted".into(),
    })
    .unwrap();
    assert_eq!(
        w.as_value(),
        &json!({"type":"turn_canceled","provider":"codex","turnId":"turn-accepted-1","reason":"interrupted"})
    );
    assert!(w.validates_against_protocol());
}

#[test]
fn preserves_user_message_text_as_is() {
    let text = "<paseo-instructions>\nX\n</paseo-instructions>\n\nHello";
    let item = AgentTimelineItem::UserMessage {
        text: text.into(),
        message_id: Some("m1".into()),
        client_message_id: None,
    };
    let v = wire_item(item).unwrap();
    assert_eq!(v["text"], text);
    assert_eq!(v["messageId"], "m1");
}

#[test]
fn canonical_tool_call_passes_through() {
    let w = to_wire(&timeline(tc(ToolCallStatus::Running, Some(Value::Null)))).unwrap();
    assert_eq!(w.as_value()["item"]["status"], "running");
    assert_eq!(w.as_value()["item"]["error"], Value::Null);
    assert!(w.validates_against_protocol());
}

#[test]
fn timestamp_is_not_on_the_wire() {
    let w = to_wire(&timeline(tc(ToolCallStatus::Running, Some(Value::Null)))).unwrap();
    assert!(w.as_value().get("timestamp").is_none());
}

#[test]
fn unknown_detail_passes_through_with_input_and_output() {
    let item = AgentTimelineItem::tool_call(ToolCallItem::new(
        "call_unknown",
        "paseo_voice.speak",
        ToolCallStatus::Completed,
        ToolCallDetail::unknown(json!({"text":"hello"}), json!({"ok":true})),
    ));
    let v = wire_item(item).unwrap();
    assert_eq!(
        v["detail"],
        json!({"type":"unknown","input":{"text":"hello"},"output":{"ok":true}})
    );
}

#[test]
fn unknown_detail_keeps_null_keys() {
    let item = AgentTimelineItem::tool_call(ToolCallItem::new(
        "c",
        "n",
        ToolCallStatus::Running,
        ToolCallDetail::unknown(Value::Null, Value::Null),
    ));
    let v = wire_item(item).unwrap();
    assert_eq!(
        v["detail"],
        json!({"type":"unknown","input":null,"output":null})
    );
}

#[test]
fn drops_legacy_status_string() {
    let v = json!({"type":"tool_call","callId":"c","name":"shell","status":"inProgress",
        "detail":{"type":"unknown","input":{"command":"pwd"},"output":null}});
    assert!(ToolCallItem::from_value(&v).is_err());
}

#[test]
fn error_rule_per_status() {
    // failed needs a non-null error
    assert!(wire_item(tc(ToolCallStatus::Failed, Some(Value::Null))).is_none());
    assert!(wire_item(tc(ToolCallStatus::Failed, None)).is_none());
    assert!(wire_item(tc(
        ToolCallStatus::Failed,
        Some(json!({"message":"command failed"}))
    ))
    .is_some());
    assert!(wire_item(tc(ToolCallStatus::Failed, Some(json!("boom")))).is_some());
    // non-failed need an explicit null
    assert!(wire_item(tc(
        ToolCallStatus::Completed,
        Some(json!({"message":"unexpected"}))
    ))
    .is_none());
    assert!(wire_item(tc(ToolCallStatus::Completed, None)).is_none());
    assert!(wire_item(tc(ToolCallStatus::Running, Some(Value::Null))).is_some());
    assert!(wire_item(tc(ToolCallStatus::Canceled, Some(Value::Null))).is_some());
}

#[test]
fn unknown_top_level_fields_are_stripped() {
    let v = json!({"type":"tool_call","callId":"call_123","name":"shell","status":"running","error":null,
        "detail":{"type":"shell","command":"pwd"},"input":{"command":"pwd"},"output":{"exitCode":0}});
    let item = AgentTimelineItem::from_value(&v).unwrap();
    // preserved internally
    assert_eq!(item.to_value()["input"], json!({"command":"pwd"}));
    // stripped on the wire
    let w = wire_item(item).unwrap();
    assert!(w.get("input").is_none() && w.get("output").is_none());
}

#[test]
fn wire_key_order_is_schema_order() {
    let item = AgentTimelineItem::tool_call({
        let mut t = ToolCallItem::new(
            "c",
            "n",
            ToolCallStatus::Completed,
            ToolCallDetail::Plan { text: "p".into() },
        );
        t.metadata = Some(json!({"a":1}).as_object().unwrap().clone());
        t
    });
    let w = wire_item(item).unwrap();
    let keys: Vec<_> = w.as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        ["type", "callId", "name", "detail", "metadata", "status", "error"]
    );
}

#[test]
fn sub_agent_detail_parses_with_and_without_actions() {
    for with_actions in [true, false] {
        let mut d = json!({"type":"sub_agent","subAgentType":"Explore","description":"Inspect","log":"[Read] README.md"});
        if with_actions {
            d["actions"] = json!([]);
        }
        let v = json!({"type":"tool_call","callId":"c","name":"Task","status":"running","error":null,"detail":d});
        let item = AgentTimelineItem::from_value(&v).unwrap();
        assert_eq!(item.to_value(), v);
        assert!(wire_item(item).is_some());
    }
}

#[test]
fn plain_text_icon_validation() {
    let ok = json!({"type":"tool_call","callId":"c","name":"t","status":"completed","error":null,
        "detail":{"type":"plain_text","label":"x","icon":"wrench"}});
    assert!(AgentTimelineItem::from_value(&ok).is_ok());
    let bad = json!({"type":"tool_call","callId":"c","name":"t","status":"completed","error":null,
        "detail":{"type":"plain_text","label":"x","icon":"laser"}});
    assert!(AgentTimelineItem::from_value(&bad).is_err());
}

#[test]
fn missing_call_id_and_missing_detail_rejected() {
    let v = json!({"type":"tool_call","name":"shell","status":"running","detail":{"type":"shell","command":"pwd"},"error":null});
    assert!(ToolCallItem::from_value(&v).is_err());
    let v = json!({"type":"tool_call","callId":"x","name":"shell","status":"running","error":null});
    assert!(ToolCallItem::from_value(&v).is_err());
}

#[test]
fn internal_events_have_no_wire_form() {
    let events = vec![
        AgentStreamEvent::ModeChanged {
            provider: "codex".into(),
            current_mode_id: Some("build".into()),
            available_modes: vec![AgentMode {
                id: "build".into(),
                label: "Build".into(),
                description: None,
                icon: None,
                color_tier: None,
                is_unattended: None,
            }],
        },
        AgentStreamEvent::ModelChanged {
            provider: "codex".into(),
            runtime_info: AgentRuntimeInfo {
                provider: "codex".into(),
                session_id: Some("session-1".into()),
                model: Some(Some("gpt-5.4".into())),
                thinking_option_id: None,
                mode_id: None,
                extra: None,
            },
        },
        AgentStreamEvent::ThinkingOptionChanged {
            provider: "codex".into(),
            thinking_option_id: Some("high".into()),
        },
        AgentStreamEvent::UsageUpdated {
            provider: "codex".into(),
            usage: AgentUsage::default(),
            turn_id: None,
        },
        AgentStreamEvent::ProviderSubagent {
            provider: "codex".into(),
            event: Box::new(ProviderSubagentInputEvent::Remove { id: "s".into() }),
        },
    ];
    for e in &events {
        assert!(to_wire(e).is_none(), "{}", e.type_name());
    }
}

#[test]
fn attention_required_gains_should_notify_false() {
    let w = to_wire(&AgentStreamEvent::AttentionRequired {
        provider: "claude".into(),
        reason: AttentionReason::Finished,
        timestamp: "2026-01-01T00:00:00.000Z".into(),
    })
    .unwrap();
    assert_eq!(w.as_value()["shouldNotify"], false);
    assert!(w.validates_against_protocol());
}

#[test]
fn every_wire_event_validates_against_protocol() {
    let events = vec![
        AgentStreamEvent::ThreadStarted {
            session_id: "s".into(),
            provider: "codex".into(),
        },
        AgentStreamEvent::TurnStarted {
            provider: "codex".into(),
            turn_id: Some("t".into()),
        },
        AgentStreamEvent::TurnCompleted {
            provider: "codex".into(),
            usage: Some(AgentUsage {
                input_tokens: Some(5.into()),
                ..Default::default()
            }),
            turn_id: None,
        },
        AgentStreamEvent::TurnFailed {
            provider: "codex".into(),
            error: "x".into(),
            code: Some("c".into()),
            diagnostic: None,
            turn_id: None,
        },
        timeline(tc(ToolCallStatus::Completed, Some(Value::Null))),
        timeline(AgentTimelineItem::Plugin {
            id: "row".into(),
            plugin_id: "review".into(),
            kind: "k".into(),
            version: 1.into(),
            data: json!({}),
        }),
        AgentStreamEvent::PermissionRequested {
            provider: "codex".into(),
            request: Box::new(PermissionRequest {
                id: "r".into(),
                provider: "codex".into(),
                name: "n".into(),
                kind: PermissionKind::Tool,
                title: None,
                description: None,
                input: None,
                detail: None,
                suggestions: None,
                actions: None,
                metadata: None,
            }),
            turn_id: None,
        },
        AgentStreamEvent::PermissionResolved {
            provider: "codex".into(),
            request_id: "r".into(),
            resolution: PermissionResponse::Deny {
                selected_action_id: None,
                message: Some("no".into()),
                interrupt: Some(true),
            },
            turn_id: None,
        },
    ];
    for e in &events {
        let w = to_wire(e).unwrap_or_else(|| panic!("{} dropped", e.type_name()));
        assert!(w.validates_against_protocol(), "{}", e.type_name());
    }
}

#[test]
fn plugin_id_pattern_enforced() {
    let item = |id: &str| AgentTimelineItem::Plugin {
        id: "row".into(),
        plugin_id: id.into(),
        kind: "k".into(),
        version: 1.into(),
        data: json!(null),
    };
    assert!(wire_item(item("review-2")).is_some());
    assert!(wire_item(item("Review")).is_none());
    assert!(wire_item(item("2review")).is_none());
}

#[test]
fn timeline_item_identity() {
    let t = AgentTimelineItem::tool_call(ToolCallItem::new(
        "call-1",
        "read",
        ToolCallStatus::Running,
        ToolCallDetail::unknown(Value::Null, Value::Null),
    ));
    assert_eq!(t.identity().as_deref(), Some("call-1"));
    let p = AgentTimelineItem::Plugin {
        id: "row-1".into(),
        plugin_id: "review".into(),
        kind: "review".into(),
        version: 1.into(),
        data: json!({}),
    };
    assert_eq!(p.identity().as_deref(), Some("review/row-1"));
    assert_eq!(
        AgentTimelineItem::Reasoning { text: "hi".into() }.identity(),
        None
    );
}

#[test]
fn json_roundtrip_all_event_types_preserves_value_and_key_order() {
    let events = vec![
        json!({"type":"thread_started","sessionId":"s","provider":"p"}),
        json!({"type":"turn_completed","provider":"p","usage":{"inputTokens":1,"totalCostUsd":0.5},"turnId":"t"}),
        json!({"type":"mode_changed","provider":"p","currentModeId":null,"availableModes":[{"id":"a","label":"A","isUnattended":true}]}),
        json!({"type":"timeline","item":{"type":"compaction","status":"loading","futureField":{"a":1}},"provider":"p"}),
        json!({"type":"provider_subagent","provider":"p","event":{"type":"upsert","id":"x","title":null,"status":"running"}}),
        json!({"type":"permission_resolved","provider":"p","requestId":"r","resolution":{"behavior":"allow","selectedActionId":"a"}}),
    ];
    for e in events {
        let ev = AgentStreamEvent::from_value(&e).unwrap();
        let back = serde_json::to_string(&ev).unwrap();
        assert_eq!(back, serde_json::to_string(&e).unwrap(), "{e}");
    }
}

#[test]
fn turn_id_accessor_matches_ts() {
    let e = AgentStreamEvent::ThreadStarted {
        session_id: "s".into(),
        provider: "p".into(),
    };
    assert_eq!(e.turn_id(), None);
    let e = AgentStreamEvent::TurnStarted {
        provider: "p".into(),
        turn_id: Some("t".into()),
    };
    assert_eq!(e.turn_id(), Some("t"));
}
