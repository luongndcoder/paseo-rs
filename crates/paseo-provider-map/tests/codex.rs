//! Codex mapper: WP0 goldens (provider-events + mapper-vectors) and synthetic transcripts for
//! the paths the recorded fixtures do not reach (legacy channel dedupe, boundary marker,
//! compaction, plan mode, sub-agent routing, history).

use paseo_agent_types::AgentStreamEvent;
use paseo_provider_map::codex::tool_call::{
    map_codex_tool_call_envelope_value, map_codex_tool_call_from_thread_item,
};
use paseo_provider_map::codex::CodexMapper;
use paseo_provider_map::testing::{load_mapper_vectors, load_provider_events};
use paseo_provider_map::{CounterEnv, ProviderMapper};
use serde_json::{json, Value};

fn run(mapper: &mut CodexMapper, lines: &[Value]) -> Vec<Value> {
    let mut env = CounterEnv::new();
    let mut out: Vec<AgentStreamEvent> = Vec::new();
    for l in lines {
        out.extend(mapper.feed(l, &mut env));
    }
    out.extend(mapper.finish(&mut env));
    out.iter().map(|e| e.to_value()).collect()
}

/// DEVIATION(codex-turn-id-on-untyped-events): the Rust event types that have no `turn_id`
/// cannot carry the tag TS puts on them.
fn strip_untyped_turn_ids(events: &mut [Value]) {
    for e in events {
        if matches!(
            e["type"].as_str(),
            Some(
                "thread_started"
                    | "provider_subagent"
                    | "mode_changed"
                    | "model_changed"
                    | "thinking_option_changed"
                    | "attention_required"
            )
        ) {
            if let Some(o) = e.as_object_mut() {
                o.remove("turnId");
            }
        }
    }
}

#[test]
fn provider_event_goldens() {
    let goldens = load_provider_events("codex");
    assert_eq!(goldens.len(), 10);
    let mut checked = 0;
    for g in goldens {
        if g.status == "failed" {
            // catalog/list-only process: nothing to map
            assert!(g.expected.is_empty(), "{}", g.fixture);
            continue;
        }
        assert!(!g.raw.is_empty(), "{}: raw transcript missing", g.fixture);
        let mut mapper = CodexMapper::new().with_deny_message("denied in fixture");
        let actual = run(&mut mapper, &g.raw);
        let mut expected = g.expected.clone();
        strip_untyped_turn_ids(&mut expected);
        assert_eq!(actual, expected, "codex/{} events differ", g.fixture);
        checked += 1;
    }
    assert_eq!(checked, 8);
}

#[test]
fn mapper_vectors() {
    let vectors = load_mapper_vectors("codex");
    assert_eq!(vectors.len(), 29);
    for v in vectors {
        let actual = match v.func.as_str() {
            "mapCodexToolCallFromThreadItem" => {
                let (item, cwd) =
                    if v.input.get("item").is_some() && v.input.get("options").is_some() {
                        (
                            v.input["item"].clone(),
                            v.input["options"]["cwd"].as_str().map(str::to_string),
                        )
                    } else {
                        (v.input.clone(), None)
                    };
                map_codex_tool_call_from_thread_item(&item, cwd.as_deref()).map(|t| t.to_value())
            }
            "mapCodexToolCallEnvelope" => {
                map_codex_tool_call_envelope_value(&v.input).map(|t| t.to_value())
            }
            other => panic!("unexpected vector fn {other}"),
        };
        let expected = if v.output.is_null() {
            None
        } else {
            Some(v.output.clone())
        };
        assert_eq!(actual, expected, "{} ({})", v.func, v.name);
    }
}

// ---------------------------------------------------------------------------------------------
// synthetic transcripts
// ---------------------------------------------------------------------------------------------

fn out(method: &str, params: Value) -> Value {
    json!({"dir": "out", "line": {"method": method, "params": params}})
}

fn inn(method: &str, id: i64, params: Value) -> Value {
    json!({"dir": "in", "line": {"id": id, "method": method, "params": params}})
}

fn resp(id: i64, result: Value) -> Value {
    json!({"dir": "out", "line": {"id": id, "result": result}})
}

const T: &str = "thread-1";

fn start() -> Vec<Value> {
    vec![
        json!({"argv": ["app-server"]}),
        inn("thread/start", 1, json!({"cwd": "/w"})),
        resp(1, json!({"thread": {"id": T}})),
        out("thread/started", json!({"thread": {"id": T}})),
        inn("turn/start", 2, json!({"threadId": T, "input": []})),
        out(
            "turn/started",
            json!({"threadId": T, "turn": {"id": "n-turn"}}),
        ),
    ]
}

fn types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| {
            let t = e["type"].as_str().unwrap_or("");
            match e["item"]["type"].as_str() {
                Some(i) => format!("{t}:{i}"),
                None => t.to_string(),
            }
        })
        .collect()
}

fn item(kind: &str, extra: Value) -> Value {
    let mut m = extra.as_object().cloned().unwrap_or_default();
    m.insert("type".into(), kind.into());
    Value::Object(m)
}

#[test]
fn boundary_marker_prefixes_the_first_delta_after_a_completed_assistant_message() {
    let mut l = start();
    l.push(out(
        "item/agentMessage/delta",
        json!({"threadId": T, "itemId": "a", "delta": "one"}),
    ));
    l.push(out(
        "item/completed",
        json!({"threadId": T, "item": item("agentMessage", json!({"id": "a", "text": "one"}))}),
    ));
    l.push(out(
        "item/agentMessage/delta",
        json!({"threadId": T, "itemId": "b", "delta": "two"}),
    ));
    l.push(out(
        "item/agentMessage/delta",
        json!({"threadId": T, "itemId": "b", "delta": "!"}),
    ));
    let ev = run(&mut CodexMapper::new(), &l);
    let texts: Vec<&str> = ev
        .iter()
        .filter_map(|e| e["item"]["text"].as_str())
        .collect();
    assert_eq!(texts, ["one", "\n\n---\n\ntwo", "!"]);
}

#[test]
fn final_text_that_does_not_extend_the_stream_is_emitted_whole() {
    let mut l = start();
    l.push(out(
        "item/agentMessage/delta",
        json!({"threadId": T, "itemId": "a", "delta": "abc"}),
    ));
    l.push(out(
        "item/completed",
        json!({"threadId": T, "item": item("agentMessage", json!({"id": "a", "text": "xyz"}))}),
    ));
    let ev = run(&mut CodexMapper::new(), &l);
    let texts: Vec<&str> = ev
        .iter()
        .filter_map(|e| e["item"]["text"].as_str())
        .collect();
    assert_eq!(texts, ["abc", "xyz"]);
}

#[test]
fn legacy_exec_events_win_over_the_mirrored_item_events() {
    let mut l = start();
    l.push(out(
        "codex/event/exec_command_begin",
        json!({"threadId": T, "msg": {"type": "exec_command_begin", "call_id": "c1", "command": ["bash", "-lc", "ls"], "cwd": "/w"}}),
    ));
    l.push(out(
        "item/started",
        json!({"threadId": T, "item": item("commandExecution", json!({"id": "c1", "command": "ls", "cwd": "/w", "status": "inProgress"}))}),
    ));
    l.push(out(
        "codex/event/exec_command_output_delta",
        json!({"threadId": T, "msg": {"type": "exec_command_output_delta", "call_id": "c1", "chunk": "aGkK"}}),
    ));
    l.push(out(
        "codex/event/exec_command_end",
        json!({"threadId": T, "msg": {"type": "exec_command_end", "call_id": "c1", "command": ["bash", "-lc", "ls"], "cwd": "/w", "exit_code": 0}}),
    ));
    l.push(out(
        "item/completed",
        json!({"threadId": T, "item": item("commandExecution", json!({"id": "c1", "command": "ls", "cwd": "/w", "status": "completed", "exitCode": 0}))}),
    ));
    let ev = run(&mut CodexMapper::new(), &l);
    let calls: Vec<&Value> = ev
        .iter()
        .filter(|e| e["item"]["type"] == "tool_call")
        .collect();
    assert_eq!(calls.len(), 2, "{ev:#?}");
    assert_eq!(calls[0]["item"]["status"], "running");
    assert_eq!(calls[1]["item"]["status"], "completed");
    // the base64 output delta is decoded and attached to the completed call
    assert_eq!(calls[1]["item"]["detail"]["output"], "hi\n");
}

#[test]
fn compaction_is_reported_once_across_both_channels() {
    let mut l = start();
    l.push(out(
        "item/started",
        json!({"threadId": T, "item": item("contextCompaction", json!({"id": "k1"}))}),
    ));
    l.push(out(
        "item/completed",
        json!({"threadId": T, "item": item("contextCompaction", json!({"id": "k1"}))}),
    ));
    l.push(out(
        "thread/compacted",
        json!({"threadId": T, "turnId": "n-turn"}),
    ));
    let ev = run(&mut CodexMapper::new(), &l);
    let statuses: Vec<&str> = ev
        .iter()
        .filter(|e| e["item"]["type"] == "compaction")
        .filter_map(|e| e["item"]["status"].as_str())
        .collect();
    assert_eq!(statuses, ["loading", "completed"]);
}

#[test]
fn failed_turn_carries_the_error_and_clears_the_foreground_turn() {
    let mut l = start();
    l.push(out(
        "turn/completed",
        json!({"threadId": T, "turn": {"id": "n-turn", "status": "failed", "error": {"message": "boom"}}}),
    ));
    // after the terminal event nothing is tagged any more
    l.push(out(
        "thread/tokenUsage/updated",
        json!({"threadId": T, "tokenUsage": {"last": {"inputTokens": 1}}}),
    ));
    let ev = run(&mut CodexMapper::new(), &l);
    assert_eq!(
        types(&ev),
        [
            "thread_started",
            "turn_started",
            "turn_failed",
            "usage_updated"
        ]
    );
    assert_eq!(ev[2]["error"], "boom");
    assert_eq!(ev[2]["turnId"], "codex-turn-0");
    assert!(ev[3].get("turnId").is_none());
}

#[test]
fn plan_mode_synthesizes_an_approval_request_on_completion() {
    let mut l = vec![
        json!({"argv": ["app-server"]}),
        inn("thread/start", 1, json!({})),
        resp(1, json!({"thread": {"id": T}})),
        inn(
            "turn/start",
            2,
            json!({"threadId": T, "collaborationMode": {"mode": "plan"}}),
        ),
        out("turn/started", json!({"threadId": T, "turn": {"id": "n"}})),
    ];
    l.push(out(
        "item/completed",
        json!({"threadId": T, "turnId": "n", "item": item("plan", json!({"id": "p1", "text": "1. do it"}))}),
    ));
    l.push(out(
        "turn/completed",
        json!({"threadId": T, "turn": {"id": "n", "status": "completed"}}),
    ));
    let ev = run(&mut CodexMapper::new(), &l);
    assert_eq!(
        types(&ev),
        [
            "turn_started",
            "timeline:tool_call",
            "permission_requested",
            "turn_completed"
        ],
        "{ev:#?}"
    );
    assert_eq!(ev[1]["item"]["name"], "plan_approval");
    assert_eq!(ev[2]["request"]["name"], "CodexPlanApproval");
    assert_eq!(
        ev[2]["request"]["id"],
        "permission-00000000-0000-4000-8000-000000000001"
    );
}

#[test]
fn sub_agent_children_route_into_provider_subagent_events() {
    let mut l = start();
    // the parent announces the spawn
    l.push(out(
        "item/completed",
        json!({"threadId": T, "item": item("collabAgentToolCall", json!({
            "id": "ca1", "tool": "spawnAgent", "status": "inProgress",
            "receiverThreadIds": ["child-1"], "prompt": "dig", "agentsStates": {}
        }))}),
    ));
    // child notifications are routed, not emitted on the root timeline
    l.push(out(
        "item/agentMessage/delta",
        json!({"threadId": "child-1", "itemId": "m", "delta": "hey"}),
    ));
    let ev = run(&mut CodexMapper::new(), &l);
    let kinds = types(&ev);
    assert!(
        kinds.contains(&"provider_subagent".to_string()),
        "{kinds:?}"
    );
    let upsert = ev
        .iter()
        .find(|e| e["type"] == "provider_subagent" && e["event"]["type"] == "upsert")
        .expect("upsert");
    assert_eq!(upsert["event"]["id"], "child-1");
    assert_eq!(upsert["event"]["toolCallId"], "ca1");
    assert_eq!(upsert["event"]["title"], "Sub-agent");
    // the child's assistant text is a provider_subagent timeline event, never a root timeline row
    assert!(ev.iter().any(|e| e["type"] == "provider_subagent"
        && e["event"]["type"] == "timeline"
        && e["event"]["item"]["text"] == "hey"));
    assert!(!ev
        .iter()
        .any(|e| e["type"] == "timeline" && e["item"]["type"] == "assistant_message"));
    // the parent row carries the child's activity as its log
    let parent = ev
        .iter()
        .rev()
        .find(|e| e["item"]["callId"] == "ca1")
        .expect("parent row");
    assert!(parent["item"]["detail"]["log"]
        .as_str()
        .unwrap()
        .contains("hey"));
}

#[test]
fn child_notifications_that_arrive_before_the_spawn_are_replayed() {
    let mut l = start();
    l.push(out(
        "item/agentMessage/delta",
        json!({"threadId": "child-1", "itemId": "m", "delta": "early"}),
    ));
    l.push(out(
        "item/completed",
        json!({"threadId": T, "item": item("collabAgentToolCall", json!({
            "id": "ca1", "tool": "spawnAgent", "status": "inProgress",
            "receiverThreadIds": ["child-1"], "agentsStates": {}
        }))}),
    ));
    let ev = run(&mut CodexMapper::new(), &l);
    assert!(ev.iter().any(|e| e["type"] == "provider_subagent"
        && e["event"]["type"] == "timeline"
        && e["event"]["item"]["text"] == "early"));
}

#[test]
fn history_projection_registers_user_messages_and_streams_in_order() {
    let thread_read = json!({"thread": {"id": T, "turns": [{
        "id": "t1", "startedAt": 1700000000, "completedAt": 1700000005,
        "items": [
            item("userMessage", json!({"id": "u1", "content": [{"type": "text", "text": "hi"}]})),
            item("agentMessage", json!({"id": "a1", "text": "hello"})),
            item("reasoning", json!({"id": "r1", "summary": ["thinking"], "content": []})),
        ]
    }]}});
    let lines = vec![
        json!({"argv": ["app-server"]}),
        inn("thread/resume", 1, json!({"threadId": T})),
        resp(1, json!({"thread": {"id": T}})),
        inn(
            "thread/read",
            2,
            json!({"threadId": T, "includeTurns": true}),
        ),
        resp(2, thread_read),
    ];
    let mut mapper = CodexMapper::new();
    assert!(run(&mut mapper, &lines).is_empty());
    let history = mapper.stream_history();
    let json: Vec<Value> = history.iter().map(|e| e.to_value()).collect();
    assert_eq!(
        types(&json),
        [
            "timeline:user_message",
            "timeline:assistant_message",
            "timeline:reasoning"
        ]
    );
    assert_eq!(json[0]["timestamp"], "2023-11-14T22:13:20.000Z");
    assert_eq!(json[1]["timestamp"], "2023-11-14T22:13:25.000Z");
    // a second drain is empty
    assert!(mapper.stream_history().is_empty());

    // the replayed user message is already known, so a live echo is not emitted twice
    let live = vec![
        inn("turn/start", 3, json!({"threadId": T})),
        out(
            "item/started",
            json!({"threadId": T, "turnId": "n", "item": item("userMessage", json!({"id": "u1", "content": [{"type": "text", "text": "hi"}]}))}),
        ),
    ];
    assert!(run(&mut mapper, &live).is_empty());
}

#[test]
fn approval_requests_for_file_changes_and_questions() {
    let mut l = start();
    l.push(
        json!({"dir": "out", "line": {"id": 7, "method": "item/fileChange/requestApproval",
        "params": {"itemId": "f1", "threadId": T, "turnId": "n-turn", "reason": "write it"}}}),
    );
    l.push(json!({"dir": "in", "line": {"id": 7, "result": {"decision": "cancel"}}}));
    l.push(json!({"dir": "out", "line": {"id": 8, "method": "item/tool/requestUserInput",
        "params": {"itemId": "q1", "threadId": T, "turnId": "n-turn", "questions": [
            {"id": "a", "header": "Pick", "question": "Which?", "options": [{"label": "x"}, {"label": "y"}]}
        ]}}}));
    l.push(
        json!({"dir": "in", "line": {"id": 8, "result": {"answers": {"a": {"answers": ["x"]}}}}}),
    );
    let ev = run(&mut CodexMapper::new(), &l);
    let kinds = types(&ev);
    assert_eq!(
        kinds,
        [
            "thread_started",
            "turn_started",
            "permission_requested",
            "timeline:tool_call",
            "permission_resolved",
            "timeline:tool_call",
            "permission_requested",
            "permission_resolved",
            "timeline:tool_call",
        ]
    );
    // a cancelled file change is a denied tool call plus an interrupting resolution
    assert_eq!(ev[3]["item"]["name"], "apply_patch");
    assert_eq!(ev[3]["item"]["error"]["message"], "Permission denied");
    assert_eq!(
        ev[4]["resolution"],
        json!({"behavior": "deny", "interrupt": true})
    );
    // the question row is emitted before its request, then completes with the answers
    assert_eq!(ev[5]["item"]["status"], "running");
    assert_eq!(ev[8]["item"]["status"], "completed");
    assert!(ev[8]["item"]["detail"]["text"]
        .as_str()
        .unwrap()
        .contains("a: x"));
}

#[test]
fn interrupt_dismisses_async_questions() {
    let mut l = start();
    l.push(out(
        "item/completed",
        json!({"threadId": T, "item": item("agentMessage", json!({
            "id": "aq", "text": "", "delivery": "async",
            "questions": [{"title": "Ready?", "options": ["yes", "no"]}]
        }))}),
    ));
    l.push(out(
        "turn/completed",
        json!({"threadId": T, "turn": {"id": "n-turn", "status": "interrupted"}}),
    ));
    let ev = run(&mut CodexMapper::new(), &l);
    assert_eq!(
        types(&ev),
        [
            "thread_started",
            "turn_started",
            "permission_requested",
            // the completed async message renders as a question row, the interrupt closes it
            "timeline:tool_call",
            "timeline:tool_call",
            "permission_resolved",
            "turn_canceled",
        ]
    );
    assert_eq!(ev[2]["request"]["name"], "request_user_input_async");
    assert!(!ev[3]["item"]["detail"]["text"]
        .as_str()
        .unwrap()
        .ends_with("Dismissed"));
    assert!(ev[4]["item"]["detail"]["text"]
        .as_str()
        .unwrap()
        .ends_with("Dismissed"));
}
