//! Pi extension adapters over captured sessions (fixtures copied from the TS extension tests).
//! The TS tests check that the live path and the history replay agree; so do these, for the parts
//! that need no file reads (child transcripts and `status.json` polling are the session layer's).

use std::path::PathBuf;

use paseo_agent_types::AgentStreamEvent;
use paseo_provider_map::pi::{
    DialogMapping, ExtToolCall, PiChildSessionMapper, PiExtensionHost, PiHistoryMapper, PiMapper,
};
use paseo_provider_map::{CounterEnv, ProviderMapper};
use paseo_tool_detail::detail::pi::parse_tool_result;
use serde_json::{json, Value};

fn fixture(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/pi-extensions")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn live(fx: &Value) -> Vec<Value> {
    let mut mapper = PiMapper::new();
    let mut env = CounterEnv::new();
    let mut out = Vec::new();
    let mut push =
        |evs: Vec<AgentStreamEvent>| out.extend(evs.iter().map(AgentStreamEvent::to_value));
    push(mapper.feed(
        &json!({"dir": "in", "line": {"type": "prompt", "message": "go"}}),
        &mut env,
    ));
    for event in fx["events"].as_array().unwrap() {
        push(mapper.feed(event, &mut env));
    }
    push(mapper.feed(&json!({"type": "agent_settled"}), &mut env));
    out
}

fn replay(fx: &Value) -> Vec<Value> {
    let messages = fx["messages"].as_array().unwrap();
    PiHistoryMapper::new("pi", Vec::new())
        .map_messages(messages)
        .iter()
        .map(AgentStreamEvent::to_value)
        .collect()
}

fn upserts(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|e| e["type"] == "provider_subagent" && e["event"]["type"] != "timeline")
        .cloned()
        .collect()
}

fn todos(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|e| e["type"] == "timeline" && e["item"]["type"] == "todo")
        .map(|e| e["item"].clone())
        .collect()
}

fn statuses(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .map(|i| {
            i["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["status"].as_str().unwrap())
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect()
}

fn check_todos(name: &str, expected: &[&str]) {
    let fx = fixture(name);
    let from_live = todos(&live(&fx));
    assert_eq!(statuses(&from_live), expected, "{name}");
    assert_eq!(
        todos(&replay(&fx)),
        from_live,
        "{name}: replay differs from live"
    );
}

#[test]
fn rpiv_todo_session() {
    check_todos(
        "rpiv-todo--rpc-session.json",
        &[
            "pending",
            "pending,pending",
            "in_progress,pending",
            "completed,pending",
        ],
    );
}

#[test]
fn pi_example_todo_session() {
    check_todos(
        "pi-example-todo--rpc-session.json",
        &["pending", "pending,pending", "completed,pending"],
    );
}

#[test]
fn pi_goal_x_session() {
    check_todos(
        "pi-goal-x--rpc-session.json",
        &["pending,pending", "completed,pending"],
    );
}

fn check_subagents(name: &str) -> Vec<Value> {
    let fx = fixture(name);
    let from_live = live(&fx);
    let from_replay = replay(&fx);
    assert!(
        !upserts(&from_live).is_empty(),
        "{name}: no sub-agent upserts"
    );
    assert_eq!(upserts(&from_live), upserts(&from_replay), "{name}");
    // custom messages become expandable tool rows on both paths and never assistant text
    for message in fx["messages"].as_array().unwrap() {
        let (Some("custom"), Some(text)) = (message["role"].as_str(), message["content"].as_str())
        else {
            continue;
        };
        for events in [&from_live, &from_replay] {
            let rows = events
                .iter()
                .filter(|e| e["type"] == "timeline" && e["item"]["detail"]["text"] == json!(text))
                .count();
            assert_eq!(
                rows,
                usize::from(message["display"] != json!(false)),
                "{name}"
            );
            assert!(!events
                .iter()
                .any(|e| e["item"]["type"] == "assistant_message"
                    && e["item"]["text"] == json!(text)));
        }
    }
    from_live
}

#[test]
fn pi_subagents_foreground() {
    let ev = upserts(&check_subagents("pi-subagents--foreground.json"));
    assert!(ev.iter().any(|e| e["event"]["status"] == "completed"));
}

#[test]
fn pi_subagents_wait() {
    check_subagents("pi-subagents--wait.json");
}

#[test]
fn tintinweb_foreground_background_and_group() {
    for name in [
        "tintinweb-pi-subagents--foreground.json",
        "tintinweb-pi-subagents--background.json",
        "tintinweb-pi-subagents--background-group.json",
    ] {
        check_subagents(name);
    }
}

#[test]
fn gotgenes_foreground_and_background() {
    for name in [
        "gotgenes-pi-subagents--foreground.json",
        "gotgenes-pi-subagents--background.json",
    ] {
        check_subagents(name);
    }
}

#[test]
fn child_session_transcripts_map_to_timeline_rows() {
    for name in [
        "pi-subagents--child-session.jsonl",
        "tintinweb-pi-subagents--child-session.jsonl",
        "gotgenes-pi-subagents--child-session.jsonl",
    ] {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/pi-extensions")
            .join(name);
        let text = std::fs::read_to_string(path).unwrap();
        let mut m = PiChildSessionMapper::new("child-1");
        let rows: usize = text.lines().map(|l| m.map_line(l).len()).sum();
        assert!(rows > 0, "{name}");
    }
}

// ---- rpiv-ask-user-question over real RPC frames --------------------------------------------

struct Rpiv {
    host: PiExtensionHost,
    permission: Value,
    dialogs: Vec<Value>,
}

fn rpiv() -> Rpiv {
    let fx = fixture("rpiv-ask-user-question--rpc-session.json");
    let frames = fx["frames"].as_array().unwrap();
    let start = &frames[0];
    let dialogs = frames[1..]
        .iter()
        .filter(|f| f["type"] == "extension_ui_request")
        .cloned()
        .collect();
    let mut host = PiExtensionHost::new();
    let permission = host
        .on_tool_start(
            &ExtToolCall {
                call_id: start["toolCallId"].as_str().unwrap().into(),
                tool_name: start["toolName"].as_str().unwrap().into(),
                args: start["args"].clone(),
                status: "running",
                result: parse_tool_result(None),
            },
            "pi",
        )
        .expect("question permission");
    Rpiv {
        host,
        permission,
        dialogs,
    }
}

fn response(m: Option<DialogMapping>) -> Value {
    match m {
        Some(DialogMapping::Response(v)) => v,
        Some(DialogMapping::Deferred) => json!("deferred"),
        other => panic!("unexpected mapping {other:?}"),
    }
}

#[test]
fn rpiv_dialogs_use_one_form_and_preserve_choices_multi_select_and_custom_text() {
    let mut r = rpiv();
    let q = &r.permission["input"]["questions"];
    assert_eq!(r.permission["kind"], "question");
    assert_eq!(q[0]["header"], "Snack");
    assert_eq!(q[0]["options"][0]["description"], "Fresh fruit");
    assert_eq!(q[0]["allowOther"], true);
    assert_eq!(q[0]["multiSelect"], false);
    assert_eq!(q[1]["header"], "Colors");
    assert_eq!(q[1]["multiSelect"], true);
    assert_eq!(q[2]["header"], "Project");
    assert_eq!(q[2]["allowOther"], true);

    let d = r.dialogs.clone();
    assert_eq!(response(r.host.map_dialog(&d[0], "pi")), json!("deferred"));
    let replies = r
        .host
        .respond_to_permission(
            &r.permission,
            &json!({"behavior": "allow", "updatedInput": {"answers": {
                "Snack": "Cookie", "Colors": "Red, Blue", "Project": "Nebula"}}}),
        )
        .unwrap();
    assert_eq!(
        replies,
        vec![(
            d[0]["id"].as_str().unwrap().to_string(),
            json!({"value": d[0]["options"][1]})
        )]
    );
    assert_eq!(
        response(r.host.map_dialog(&d[1], "pi")),
        json!({"value": "1,2"})
    );
    assert_eq!(
        response(r.host.map_dialog(&d[2], "pi")),
        json!({"value": d[2]["options"][2]})
    );
    assert_eq!(
        response(r.host.map_dialog(&d[3], "pi")),
        json!({"value": "Nebula"})
    );
}

#[test]
fn rpiv_cancellation_declines_each_remaining_dialog() {
    let mut r = rpiv();
    let replies = r
        .host
        .respond_to_permission(&r.permission, &json!({"behavior": "deny"}))
        .unwrap();
    assert!(replies.is_empty());
    for d in r.dialogs.clone() {
        assert_eq!(
            response(r.host.map_dialog(&d, "pi")),
            json!({"cancelled": true})
        );
    }
}

#[test]
fn rpiv_cancel_result_maps_to_cancelled_text() {
    let fx = fixture("rpiv-ask-user-question--cancel-result.json");
    let mut host = PiExtensionHost::new();
    let out = host
        .map_tool_call(&ExtToolCall {
            call_id: "c".into(),
            tool_name: "ask_user_question".into(),
            args: Value::Null,
            status: "completed",
            result: parse_tool_result(Some(&fx["result"])),
        })
        .expect("claimed");
    assert_eq!(out.detail.unwrap().to_value()["text"], "Cancelled");
}
