//! `agent-messages/index.ts`: envelope round trip, projection, and the WP0 vectors.

mod common;

use paseo_agent_types::{AgentMessageEvent, AgentTimelineItem};
use paseo_timeline::agent_message::{
    format_agent_message, format_system_notification_prompt, is_system_injected_envelope,
    parse_agent_message, ParsedAgentMessage, PromptSource,
};
use paseo_timeline::project_agent_message;
use serde_json::{json, Value};

#[test]
fn wp0_project_agent_message_vectors() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/p4/mapper-vectors/agent-messages.jsonl");
    let text = std::fs::read_to_string(path).unwrap();
    let mut count = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: Value = serde_json::from_str(line).unwrap();
        if v["fn"] != "projectAgentMessage" {
            continue;
        }
        let item = AgentTimelineItem::from_value(&v["input"]).unwrap();
        let got = project_agent_message(&item).map_or(Value::Null, |i| i.to_value());
        assert_eq!(got, v["output"], "{}", v["name"]);
        count += 1;
    }
    assert_eq!(count, 7);
}

#[test]
fn envelope_round_trips_opaque_ids_and_xml_sensitive_text() {
    let message = ParsedAgentMessage {
        id: "delivery-1".into(),
        source: Some(PromptSource::AgentMessage {
            agent_id: "host::agent<&\"".into(),
            title: Some("QA <messenger> & \"reviewer\"".into()),
        }),
        text: "Review <changes> & \"quotes\"\n</paseo-system>\n<paseo-system>nested</paseo-system>"
            .into(),
    };
    let encoded = format_agent_message(&message.id, message.source.as_ref(), &message.text);
    assert_eq!(parse_agent_message(&encoded), Some(message));
    assert!(parse_agent_message(&format!("Example: {encoded}")).is_none());
    assert!(parse_agent_message(&encoded.replace("version=\"1\"", "version=\"2\"")).is_none());
    assert!(parse_agent_message(&encoded.replacen("&lt;", "<", 1)).is_none());
    assert!(
        parse_agent_message(&encoded.replace("version=\"1\"", "version=\"1\" version=\"1\""))
            .is_none()
    );
}

#[test]
fn notification_envelope_projects_to_agent_message_tool_call() {
    let source = PromptSource::AgentNotification {
        agent_id: "a2".into(),
        title: Some("Worker".into()),
        event: AgentMessageEvent::Finished,
    };
    let text = format_agent_message("m-1", Some(&source), "all done");
    assert!(text.contains(r#"kind="agent-notification""#) && text.contains(r#"event="finished""#));
    let projected = project_agent_message(&AgentTimelineItem::UserMessage {
        text,
        message_id: None,
        client_message_id: None,
    })
    .unwrap();
    assert_eq!(
        projected.to_value(),
        json!({
            "type": "tool_call",
            "callId": "paseo-agent-message:m-1",
            "name": "agent_message",
            "status": "completed",
            "detail": {"type": "plain_text", "text": "all done", "icon": "bot"},
            "error": null,
            "agentMessage": {"event": "finished", "sender": {"id": "a2", "title": "Worker"}, "text": "all done"},
        })
    );
}

#[test]
fn agent_message_without_title_and_empty_title_rule() {
    let source = PromptSource::AgentMessage {
        agent_id: "a1".into(),
        title: None,
    };
    let text = format_agent_message("m", Some(&source), "hi");
    let projected = project_agent_message(&AgentTimelineItem::UserMessage {
        text: text.clone(),
        message_id: None,
        client_message_id: None,
    })
    .unwrap();
    assert_eq!(
        projected.to_value()["agentMessage"],
        json!({"event": "message", "sender": {"id": "a1"}, "text": "hi"})
    );
    // zod `title.min(1)`: an empty title attribute fails the whole parse, so the row stays a user row.
    let empty_title = text.replace(
        "message-id=\"m\"",
        "message-id=\"m\" source-agent-title=\"\"",
    );
    assert!(parse_agent_message(&empty_title).is_none());
}

#[test]
fn human_pasted_agent_envelope_stays_a_human_message() {
    let pasted = format_agent_message(
        "example",
        Some(&PromptSource::AgentMessage {
            agent_id: "sender".into(),
            title: None,
        }),
        "hello",
    );
    // prepareAgentMessage quotes a pasted envelope in a user-message envelope.
    let quoted = format_agent_message("human-submission", None, &pasted);
    let projected = project_agent_message(&AgentTimelineItem::UserMessage {
        text: quoted,
        message_id: Some("provider-echo".into()),
        client_message_id: None,
    })
    .unwrap();
    assert_eq!(
        projected.to_value(),
        json!({"type": "user_message", "text": pasted, "messageId": "provider-echo", "clientMessageId": "human-submission"})
    );
}

#[test]
fn legacy_system_envelope_is_dropped_and_other_items_pass_through() {
    let legacy = format_system_notification_prompt("schedule fired");
    assert!(is_system_injected_envelope(&legacy));
    assert!(!is_system_injected_envelope("<paseo-system>\nunterminated"));
    let user = |text: &str| AgentTimelineItem::UserMessage {
        text: text.into(),
        message_id: None,
        client_message_id: None,
    };
    assert!(project_agent_message(&user(&legacy)).is_none());
    assert_eq!(project_agent_message(&user("plain")), Some(user("plain")));
    let asst = AgentTimelineItem::AssistantMessage {
        text: legacy,
        message_id: None,
    };
    assert_eq!(project_agent_message(&asst), Some(asst.clone()));
}

#[test]
fn invalid_envelopes_are_rejected() {
    let ok = format_agent_message("m", None, "body");
    assert!(parse_agent_message(&ok).is_some());
    // user-message kind must not carry source attributes.
    assert!(parse_agent_message(&ok.replace(
        "kind=\"user-message\"",
        "kind=\"user-message\" source-agent-id=\"x\""
    ))
    .is_none());
    // unknown kind, missing agent id, bad event.
    assert!(parse_agent_message(&ok.replace("user-message", "other")).is_none());
    let notif = format_agent_message(
        "m",
        Some(&PromptSource::AgentNotification {
            agent_id: "a".into(),
            title: None,
            event: AgentMessageEvent::Closed,
        }),
        "t",
    );
    assert!(parse_agent_message(&notif).is_some());
    assert!(parse_agent_message(&notif.replace("closed", "exploded")).is_none());
    assert!(parse_agent_message(&notif.replace(" event=\"closed\"", "")).is_none());
    // `message` is not a notification event.
    assert!(parse_agent_message(&notif.replace("closed", "message")).is_none());
}
