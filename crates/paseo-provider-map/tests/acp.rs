use paseo_provider_map::acp::AcpMapper;
use paseo_provider_map::testing::{load_provider_events, replay};

#[test]
fn acp_goldens() {
    let mut checked = 0;
    for g in load_provider_events("acp") {
        if g.status == "failed" {
            continue;
        }
        let mut mapper = AcpMapper::new();
        let actual = replay(&mut mapper, &g.raw);
        assert_eq!(actual, g.expected, "acp/{} events differ", g.fixture);
        checked += 1;
    }
    assert!(
        checked >= 8,
        "expected >= 8 replayable acp goldens, got {checked}"
    );
}

use paseo_provider_map::{CounterEnv, ProviderMapper};
use serde_json::{json, Value};

fn update(u: Value) -> Value {
    json!({"dir": "out", "line": {"jsonrpc": "2.0", "method": "session/update",
        "params": {"sessionId": "s", "update": u}}})
}

fn run(frames: &[Value]) -> (AcpMapper, Vec<Value>) {
    let mut m = AcpMapper::new();
    let out = replay(&mut m, frames);
    (m, out)
}

#[test]
fn history_is_recorded_during_session_load() {
    let frames = [
        json!({"dir": "in", "line": {"jsonrpc": "2.0", "id": 1, "method": "session/load", "params": {}}}),
        update(
            json!({"sessionUpdate": "user_message_chunk", "messageId": "u1",
            "content": {"type": "text", "text": "hi "}}),
        ),
        update(
            json!({"sessionUpdate": "user_message_chunk", "messageId": "u1",
            "content": {"type": "text", "text": "there"}}),
        ),
        update(
            json!({"sessionUpdate": "plan", "entries": [{"content": "a", "status": "completed"}]}),
        ),
        json!({"dir": "out", "line": {"jsonrpc": "2.0", "id": 1, "result": {}}}),
        update(json!({"sessionUpdate": "plan", "entries": []})),
    ];
    let (mut m, out) = run(&frames);
    assert_eq!(out.len(), 3);
    assert_eq!(
        out[0]["item"],
        json!({"type": "user_message", "text": "hi there", "messageId": "u1"})
    );
    let history = m.take_history();
    assert_eq!(history.len(), 2);
    assert_eq!(
        history[1],
        json!({"type": "todo", "items": [{"text": "a", "completed": true}]})
    );
}

#[test]
fn tool_snapshots_merge_and_map_details() {
    let frames = [
        update(
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Read f", "kind": "read",
            "status": "pending", "locations": [{"path": "/a/b.rs"}], "rawInput": {"offset": 3}}),
        ),
        update(
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "failed",
            "rawOutput": {"error": "boom"}}),
        ),
        update(json!({"sessionUpdate": "tool_call", "toolCallId": "t2", "title": "Weird"})),
        update(
            json!({"sessionUpdate": "tool_call", "toolCallId": "t3", "title": "Del", "kind": "delete",
            "content": [{"type": "diff", "path": "x", "oldText": "old", "newText": "new"}]}),
        ),
    ];
    let (_, out) = run(&frames);
    assert_eq!(out[1]["item"]["status"], "failed");
    assert_eq!(out[1]["item"]["error"], json!({"message": "boom"}));
    assert_eq!(
        out[1]["item"]["detail"],
        json!({"type": "read", "filePath": "/a/b.rs", "offset": 3})
    );
    assert_eq!(out[2]["item"]["name"], "Weird");
    assert_eq!(
        out[2]["item"]["detail"],
        json!({"type": "unknown", "input": null, "output": null})
    );
    assert_eq!(
        out[3]["item"]["detail"],
        json!({"type": "edit", "filePath": "Del", "oldString": "old", "newString": ""})
    );
}

#[test]
fn usage_update_precedes_flushed_user_message() {
    let frames = [
        update(
            json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "q"}}),
        ),
        update(json!({"sessionUpdate": "usage_update", "size": 100, "used": 7})),
        update(json!({"sessionUpdate": "usage_update", "size": 0, "used": 7})),
    ];
    let (_, out) = run(&frames);
    assert_eq!(out[0]["type"], "usage_updated");
    assert_eq!(
        out[0]["usage"],
        json!({"contextWindowMaxTokens": 100, "contextWindowUsedTokens": 7})
    );
    assert_eq!(out[1]["item"]["type"], "user_message");
    assert_eq!(out.len(), 2);
}

#[test]
fn cancelled_prompt_synthesizes_canceled_tool_calls() {
    let mut m = AcpMapper::new();
    let mut env = CounterEnv::new();
    let frames = [
        json!({"dir": "in", "line": {"jsonrpc": "2.0", "id": 5, "method": "session/prompt", "params": {}}}),
        update(
            json!({"sessionUpdate": "tool_call", "toolCallId": "t", "title": "T", "kind": "execute",
            "status": "in_progress", "rawInput": {"command": "ls", "args": ["-l"]}}),
        ),
        json!({"dir": "out", "line": {"jsonrpc": "2.0", "id": 5, "result": {"stopReason": "cancelled"}}}),
    ];
    let mut out = Vec::new();
    for f in &frames {
        out.extend(m.feed(f, &mut env));
    }
    let last = serde_json::to_value(out.last().unwrap()).unwrap();
    assert_eq!(last["item"]["status"], "canceled");
    assert_eq!(last["item"]["detail"]["command"], "ls -l");
    assert_eq!(last["turnId"], "turn-1");
    assert!(m.active_turn_id().is_none());
}
