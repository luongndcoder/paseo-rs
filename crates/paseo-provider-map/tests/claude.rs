use paseo_provider_map::claude::ClaudeMapper;
use paseo_provider_map::testing::{load_mapper_vectors, load_provider_events, replay};
use paseo_tool_detail::detail::claude::{
    map_claude_canceled_tool_call, map_claude_completed_tool_call, map_claude_failed_tool_call,
    map_claude_running_tool_call, ClaudeToolCallParams,
};
use serde_json::{json, Value};

/// `AgentStreamEvent::ThreadStarted` and `ModeChanged` have no `turn_id` (the schema omits it),
/// while the TS adapter tags every event it emits at runtime. The tag on those events is dropped
/// on both sides; the manager re-tags events that arrive without one.
fn without_thread_started_turn_id(events: Vec<Value>) -> Vec<Value> {
    events
        .into_iter()
        .map(|mut e| {
            if matches!(e["type"].as_str(), Some("thread_started" | "mode_changed")) {
                if let Some(o) = e.as_object_mut() {
                    o.remove("turnId");
                }
            }
            e
        })
        .collect()
}

fn replay_golden(g: &paseo_provider_map::testing::ProviderEventGolden) -> (Vec<Value>, Vec<Value>) {
    let mut mapper = ClaudeMapper::new();
    (
        without_thread_started_turn_id(replay(&mut mapper, &g.raw)),
        without_thread_started_turn_id(g.expected.clone()),
    )
}

#[test]
fn claude_goldens() {
    // `ok` fixtures replay the whole adapter; `partial` ones cover the mapping seam only (hook and
    // mcp callbacks are not driven) and are compared on the events that exist.
    let goldens = load_provider_events("claude");
    assert_eq!(goldens.len(), 10);
    assert!(goldens.iter().all(|g| g.status != "failed"));
    assert!(
        goldens.iter().all(|g| !g.raw.is_empty()),
        "raw transcripts missing"
    );
    for g in &goldens {
        let (actual, expected) = replay_golden(g);
        assert_eq!(
            actual, expected,
            "claude/{} ({}) events differ",
            g.fixture, g.status
        );
    }
}

fn run_vector(function: &str, input: &Value) -> Option<Value> {
    let call_id = input.get("callId").and_then(Value::as_str);
    let name = input.get("name").and_then(Value::as_str).unwrap_or("");
    let params = ClaudeToolCallParams {
        call_id,
        name,
        input: input.get("input"),
        output: input.get("output"),
        metadata: input.get("metadata").and_then(Value::as_object),
    };
    let error = input.get("error").cloned().unwrap_or(Value::Null);
    let item = match function {
        "mapClaudeRunningToolCall" => map_claude_running_tool_call(&params),
        "mapClaudeCompletedToolCall" => map_claude_completed_tool_call(&params),
        "mapClaudeFailedToolCall" => map_claude_failed_tool_call(&params, &error),
        "mapClaudeCanceledToolCall" => map_claude_canceled_tool_call(&params),
        other => panic!("unexpected vector fn {other}"),
    };
    item.map(|i| i.to_value())
}

#[test]
fn claude_tool_call_mapper_vectors() {
    let vectors = load_mapper_vectors("claude");
    assert!(vectors.len() >= 90);
    for v in vectors {
        let actual = run_vector(&v.func, &v.input);
        let expected = (!v.output.is_null()).then(|| v.output.clone());
        assert_eq!(actual, expected, "{} {}", v.func, v.name);
    }
}

fn out(line: Value) -> Value {
    json!({"dir": "out", "line": line})
}

fn input(line: Value) -> Value {
    json!({"dir": "in", "line": line})
}

fn user_prompt(text: &str) -> Value {
    input(
        json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}}),
    )
}

fn init(session: &str) -> Value {
    out(
        json!({"type": "system", "subtype": "init", "session_id": session, "model": "claude-sonnet-5-5", "permissionMode": "default", "cwd": "/work"}),
    )
}

fn result_ok(session: &str) -> Value {
    out(json!({
        "type": "result", "subtype": "success", "session_id": session, "total_cost_usd": 0.5,
        "usage": {"input_tokens": 3, "cache_read_input_tokens": 10, "output_tokens": 4},
    }))
}

fn types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| {
            let t = e["type"].as_str().unwrap().to_owned();
            match e["item"]["type"].as_str() {
                Some(i) => format!("{t}:{i}"),
                None => t,
            }
        })
        .collect()
}

fn run(rows: &[Value]) -> Vec<Value> {
    replay(&mut ClaudeMapper::new(), rows)
}

#[test]
fn streamed_tool_input_emits_running_updates_and_dedups_primitives() {
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        out(
            json!({"type": "stream_event", "event": {"type": "content_block_start", "index": 1,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}}}),
        ),
        out(
            json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": "{\"command\": \"ls"}}}),
        ),
        out(
            json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": " -la\"}"}}}),
        ),
        out(
            json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": " "}}}),
        ),
    ];
    let events = run(&rows);
    let calls: Vec<&Value> = events
        .iter()
        .filter(|e| e["item"]["type"] == "tool_call" && e["item"]["status"] == "running")
        .collect();
    // content_block_start (empty input), then the completed command; the trailing whitespace
    // delta re-parses to the same input and is skipped (the close then cancels the call).
    assert_eq!(calls.len(), 2, "{events:#?}");
    assert_eq!(
        calls[0]["item"]["detail"],
        json!({"type": "unknown", "input": {}, "output": null})
    );
    assert_eq!(calls[1]["item"]["detail"]["command"], "ls -la");
}

#[test]
fn error_result_fails_the_turn_with_the_exit_code() {
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        out(
            json!({"type": "result", "subtype": "error_during_execution", "errors": ["process exited with code 137"]}),
        ),
    ];
    let events = run(&rows);
    let last = events.last().unwrap();
    assert_eq!(last["type"], "turn_failed");
    assert_eq!(last["error"], "process exited with code 137");
    assert_eq!(last["code"], "137");
    assert_eq!(last["turnId"], "foreground-turn-1");
}

#[test]
fn interrupt_cancels_started_tools_and_suppresses_the_stale_result() {
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        out(
            json!({"type": "assistant", "message": {"id": "m1", "content": [
            {"type": "tool_use", "id": "toolu_9", "name": "Read", "input": {"file_path": "/work/a.txt"}}]}}),
        ),
        input(json!({"type": "control_request", "request": {"subtype": "interrupt"}})),
        result_ok("s1"),
    ];
    let events = run(&rows);
    assert_eq!(
        types(&events),
        [
            "turn_started",
            "timeline:user_message",
            "thread_started",
            "timeline:tool_call",
            "timeline:tool_call",
            "turn_canceled",
        ]
    );
    assert_eq!(events[4]["item"]["status"], "canceled");
    assert_eq!(events[5]["reason"], "Interrupted");
}

#[test]
fn session_close_cancels_the_open_turn_and_rejects_pending_permissions() {
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        out(
            json!({"type": "control_request", "request_id": "r1", "request": {
            "subtype": "can_use_tool", "tool_name": "Bash", "tool_use_id": "toolu_2",
            "input": {"command": "rm x"}}}),
        ),
    ];
    let events = run(&rows);
    let tail: Vec<&str> = events[events.len() - 2..]
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(tail, ["permission_resolved", "turn_canceled"]);
    assert_eq!(
        events[events.len() - 2]["resolution"]["message"],
        "Claude session closed"
    );
}

#[test]
fn subagent_frames_never_reach_the_parent_transcript() {
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        out(
            json!({"type": "assistant", "parent_tool_use_id": "toolu_task", "message": {"id": "m9", "content": [
            {"type": "text", "text": "inside the subagent"}]}}),
        ),
    ];
    let events = run(&rows);
    assert!(!types(&events).contains(&"timeline:assistant_message".to_owned()));
    // Without a task announcement the tracker still routes the frame to its child.
    assert!(subagent_events(&events)
        .iter()
        .any(|e| e["item"]["text"] == "inside the subagent"));
}

#[test]
fn history_conversion_replays_user_assistant_and_tool_records() {
    let transcript = [
        json!({"type": "user", "uuid": "u1", "timestamp": "2026-10-16T17:28:21.854Z",
               "message": {"role": "user", "content": "hello <b>"}}),
        json!({"type": "assistant", "uuid": "a1", "message": {"role": "assistant", "content": [
            {"type": "text", "text": "Reading."},
            {"type": "tool_use", "id": "toolu_7", "name": "Read", "input": {"file_path": "/work/f.txt"}}]}}),
        json!({"type": "user", "uuid": "u2", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_7", "content": "1\tbody\n2\t"}]}}),
        json!({"type": "user", "uuid": "u3", "isMeta": true, "message": {"role": "user", "content": "meta"}}),
        json!({"type": "assistant", "uuid": "a3", "message": {"role": "assistant", "content": [{"type": "text", "text": "Done."}]}}),
        json!({"type": "assistant", "uuid": "a2", "isSidechain": true, "message": {"content": "skip"}}),
        json!({"type": "user", "uuid": "u4", "message": {"role": "user", "content": "[Request interrupted by user]"}}),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    let mut mapper = ClaudeMapper::new().with_cwd("/work");
    let history = mapper.ingest_history(&transcript);
    let kinds: Vec<(&str, Option<&str>)> = history
        .iter()
        .map(|h| (h.item["type"].as_str().unwrap(), h.item["status"].as_str()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("user_message", None),
            ("assistant_message", None),
            ("tool_call", Some("running")),
            ("tool_call", Some("completed")),
            ("assistant_message", None),
        ]
    );
    assert_eq!(
        history[0].timestamp.as_deref(),
        Some("2026-10-16T17:28:21.854Z")
    );
    assert_eq!(history[0].item["messageId"], "u1");
    // An assistant record with tool blocks keeps its items as mapped (no message id), a plain
    // text record takes the transcript uuid.
    assert!(history[1].item.get("messageId").is_none());
    assert_eq!(history[4].item["messageId"], "a3");
    assert_eq!(history[3].item["detail"]["content"], "body\n");
}

#[test]
fn task_notification_user_message_becomes_a_tool_call() {
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        out(
            json!({"type": "user", "uuid": "n1", "message": {"role": "user", "content":
            "<task-notification><task-id>t1</task-id><status>failed</status><summary>Build broke</summary></task-notification>"}}),
        ),
        result_ok("s1"),
    ];
    let events = run(&rows);
    let call = events
        .iter()
        .find(|e| e["item"]["name"] == "task_notification")
        .expect("task notification call");
    assert_eq!(call["item"]["status"], "failed");
    assert_eq!(call["item"]["error"], json!({"message": "Build broke"}));
    assert_eq!(call["item"]["callId"], "task_notification_n1");
}

#[test]
fn todo_write_emits_a_todo_snapshot() {
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        out(
            json!({"type": "assistant", "message": {"id": "m2", "content": [
            {"type": "tool_use", "id": "toolu_t", "name": "TodoWrite", "input": {"todos": [
                {"content": "write tests", "status": "in_progress", "activeForm": "Writing tests"},
                {"content": "ship", "status": "pending"}]}}]}}),
        ),
    ];
    let events = run(&rows);
    let todo = events
        .iter()
        .find(|e| e["item"]["type"] == "todo")
        .expect("todo snapshot");
    assert_eq!(todo["item"]["items"][0]["status"], "in_progress");
    assert_eq!(todo["item"]["items"][0]["id"], "legacy:0");
    assert_eq!(todo["item"]["items"][1]["completed"], false);
}

#[test]
fn compaction_status_and_boundary_emit_markers_and_usage() {
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        out(json!({"type": "system", "subtype": "status", "status": "compacting"})),
        out(json!({"type": "system", "subtype": "status", "status": "compacting"})),
        out(json!({"type": "system", "subtype": "compact_boundary",
                   "compact_metadata": {"trigger": "manual", "pre_tokens": 900, "post_tokens": 100}})),
    ];
    let events = run(&rows);
    let marks: Vec<&Value> = events
        .iter()
        .filter(|e| e["item"]["type"] == "compaction")
        .collect();
    assert_eq!(marks.len(), 2, "the repeated status opens a single marker");
    assert_eq!(marks[0]["item"]["status"], "loading");
    assert_eq!(marks[1]["item"]["trigger"], "manual");
    assert_eq!(marks[1]["item"]["preTokens"], 900);
    let usage = events
        .iter()
        .find(|e| e["type"] == "usage_updated")
        .unwrap();
    assert_eq!(usage["usage"]["contextWindowUsedTokens"], 100);
}

#[test]
fn plan_permission_allow_completes_the_native_call() {
    let rows = vec![
        user_prompt("plan it"),
        init("s1"),
        out(
            json!({"type": "control_request", "request_id": "r1", "request": {
            "subtype": "can_use_tool", "tool_name": "ExitPlanMode", "tool_use_id": "toolu_p",
            "input": {"plan": "1. do it"}}}),
        ),
        input(
            json!({"type": "control_response", "response": {"subtype": "success", "request_id": "r1",
            "response": {"behavior": "allow"}}}),
        ),
    ];
    let events = run(&rows);
    let requested = events
        .iter()
        .find(|e| e["type"] == "permission_requested")
        .unwrap();
    assert_eq!(requested["request"]["kind"], "plan");
    assert_eq!(requested["request"]["actions"].as_array().unwrap().len(), 2);
    assert_eq!(requested["request"]["metadata"]["planText"], "1. do it");
    let completed = events
        .iter()
        .find(|e| e["item"]["name"] == "plan_approval")
        .expect("plan_approval call");
    assert_eq!(completed["item"]["callId"], "toolu_p");
    assert_eq!(completed["item"]["status"], "completed");
}

fn task_parent_assistant() -> Value {
    out(json!({
        "type": "assistant", "session_id": "s1", "parent_tool_use_id": null,
        "message": {"id": "msg_p", "role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_task", "name": "Task",
             "input": {"description": "Find the bug", "subagent_type": "Explore", "prompt": "look"}}
        ]},
    }))
}

fn task_started(tool_use_id: &str, extra: Value) -> Value {
    let mut line = json!({
        "type": "system", "subtype": "task_started", "session_id": "s1", "task_id": "task-1",
        "tool_use_id": tool_use_id, "description": "Find the bug", "subagent_type": "Explore",
        "task_type": "local_agent", "prompt": "look around",
    });
    for (k, v) in extra.as_object().unwrap() {
        line[k] = v.clone();
    }
    out(line)
}

fn sidechain_assistant(parent: &str, content: Value) -> Value {
    out(json!({
        "type": "assistant", "session_id": "s1", "parent_tool_use_id": parent,
        "message": {"id": "msg_c", "role": "assistant", "model": "claude-haiku-4-5", "content": content},
    }))
}

fn subagent_events(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|e| e["type"] == "provider_subagent")
        .map(|e| &e["event"])
        .collect()
}

fn sidechain_user_result(parent: &str, tool_use_id: &str, content: &str) -> Value {
    out(
        json!({"type": "user", "session_id": "s1", "parent_tool_use_id": parent,
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": tool_use_id, "content": content}]}}),
    )
}

fn parent_tool_result(tool_use_id: &str, is_error: bool) -> Value {
    out(json!({"type": "user", "session_id": "s1",
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": tool_use_id, "content": "done", "is_error": is_error}]}}))
}

fn task_card_logs(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e["type"] == "timeline" && e["item"]["detail"]["type"] == "sub_agent")
        .map(|e| e["item"]["detail"]["log"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn task_protocol_declares_subagent_and_routes_sidechain_frames() {
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        task_parent_assistant(),
        task_started("toolu_task", json!({})),
        sidechain_assistant(
            "toolu_task",
            json!([
                {"type": "text", "text": " found it "},
                {"type": "tool_use", "id": "toolu_r", "name": "Read", "input": {"file_path": "/work/a.rs"}}
            ]),
        ),
        sidechain_user_result("toolu_task", "toolu_r", "fn main() {}"),
        out(
            json!({"type": "system", "subtype": "task_progress", "task_id": "task-1",
            "usage": {"total_tokens": 1500}}),
        ),
        out(
            json!({"type": "system", "subtype": "task_notification", "task_id": "task-1",
            "tool_use_id": "toolu_task", "status": "completed"}),
        ),
        parent_tool_result("toolu_task", false),
        result_ok("s1"),
    ];
    let events = run(&rows);
    let sub = subagent_events(&events);
    assert_eq!(
        sub[0],
        &json!({"type": "upsert", "id": "toolu_task", "title": "Explore",
            "description": "Find the bug", "status": "running", "toolCallId": "toolu_task"})
    );
    assert_eq!(
        sub[1],
        &json!({"type": "timeline", "id": "toolu_task",
            "item": {"type": "user_message", "text": "look around"}})
    );
    // The model read off the child's own frame, then its token usage, build one subtitle.
    assert!(sub.contains(
        &&json!({"type": "upsert", "id": "toolu_task", "subtitle": "Explore \u{b7} Haiku 4.5"})
    ));
    assert!(sub.contains(&&json!({"type": "upsert", "id": "toolu_task",
        "subtitle": "Explore \u{b7} Haiku 4.5 \u{b7} 1.5k tokens"})));
    assert!(
        sub.contains(&&json!({"type": "timeline", "id": "toolu_task",
        "item": {"type": "assistant_message", "text": "found it", "messageId": "msg_c"}}))
    );
    // Child tool calls land on the child, running then completed.
    let child_reads: Vec<&str> = sub
        .iter()
        .filter(|e| e["type"] == "timeline" && e["item"]["callId"] == "toolu_r")
        .map(|e| e["item"]["status"].as_str().unwrap())
        .collect();
    assert_eq!(child_reads, ["running", "completed"]);
    // The task protocol owns the descriptor: the tracker adds no `Claude subagent` upsert and the
    // parent's result does not close the child a second time.
    assert!(!sub.iter().any(|e| e["title"] == "Claude subagent"));
    assert_eq!(
        sub.iter().filter(|e| e["status"] == "completed").count(),
        1,
        "{sub:?}"
    );
    // The synthetic Task card carries the child's action log.
    assert_eq!(
        task_card_logs(&events),
        ["", "[Read] /work/a.rs", "[Read] /work/a.rs"]
    );
    // No sidechain frame leaks into the parent transcript.
    assert!(!events
        .iter()
        .any(|e| e["type"] == "timeline" && e["item"]["text"] == "found it"));
}

#[test]
fn task_call_name_wins_over_subagent_type() {
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        out(
            json!({"type": "assistant", "session_id": "s1", "message": {"id": "m", "content": [
            {"type": "tool_use", "id": "toolu_task", "name": "Task",
             "input": {"name": "bug-hunter", "subagent_type": "Explore"}}]}}),
        ),
        task_started("toolu_task", json!({})),
    ];
    let events = run(&rows);
    assert_eq!(subagent_events(&events)[0]["title"], "bug-hunter");
}

#[test]
fn legacy_sidechain_derives_descriptor_from_frames_and_closes_on_result() {
    // No `task_started`: an older Claude Code release, so the tracker owns the descriptor.
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        task_parent_assistant(),
        sidechain_assistant(
            "toolu_task",
            json!([{"type": "tool_use", "id": "toolu_r", "name": "Read", "input": {"file_path": "/work/a.rs"}}]),
        ),
        parent_tool_result("toolu_task", true),
    ];
    let events = run(&rows);
    let sub = subagent_events(&events);
    assert_eq!(
        sub[0],
        &json!({"type": "upsert", "id": "toolu_task", "title": "Explore",
            "description": "Find the bug", "status": "running", "toolCallId": "toolu_task"})
    );
    assert_eq!(
        sub.last().unwrap(),
        &&json!({"type": "upsert", "id": "toolu_task", "title": "Explore",
            "description": "Find the bug", "status": "failed", "toolCallId": "toolu_task"})
    );
    assert_eq!(task_card_logs(&events), ["[Read] /work/a.rs"]);
}

#[test]
fn legacy_sidechain_finishes_as_completed_when_the_turn_succeeds() {
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        task_parent_assistant(),
        sidechain_assistant("toolu_task", json!([{"type": "text", "text": "hi"}])),
        result_ok("s1"),
    ];
    let events = run(&rows);
    let last = subagent_events(&events).into_iter().last().unwrap().clone();
    assert_eq!(last["status"], "completed");
    assert_eq!(last["toolCallId"], "toolu_task");
}

#[test]
fn frames_for_undeclared_tasks_are_dropped_once_the_cli_announces_tasks() {
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        // A background shell is announced but is not a subagent.
        out(
            json!({"type": "system", "subtype": "task_started", "task_id": "task-b",
            "tool_use_id": "toolu_bash", "task_type": "local_bash"}),
        ),
        sidechain_assistant("toolu_other", json!([{"type": "text", "text": "ghost"}])),
    ];
    let events = run(&rows);
    assert!(subagent_events(&events).is_empty(), "{events:?}");
}

#[test]
fn nested_tool_calls_make_their_tasks_children_of_the_emitting_subagent() {
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        task_parent_assistant(),
        task_started("toolu_task", json!({})),
        sidechain_assistant(
            "toolu_task",
            json!([{"type": "tool_use", "id": "toolu_grand", "name": "Task",
                "input": {"description": "inner", "subagent_type": "Explore"}}]),
        ),
        out(
            json!({"type": "system", "subtype": "task_started", "task_id": "task-2",
            "tool_use_id": "toolu_grand", "description": "inner", "subagent_type": "Explore",
            "task_type": "local_agent"}),
        ),
    ];
    let events = run(&rows);
    let grandchild = subagent_events(&events)
        .into_iter()
        .find(|e| e["id"] == "toolu_grand" && e["type"] == "upsert")
        .unwrap();
    assert_eq!(grandchild["parentSubagentId"], "toolu_task");
    // The launching sidechain already owns the grandchild's card.
    assert!(!events.iter().any(|e| e["type"] == "timeline"
        && e["item"]["callId"] == "toolu_grand"
        && e["item"]["detail"]["type"] == "sub_agent"
        && e["item"]["detail"]["log"] == ""));
}

#[test]
fn subagent_handback_is_shown_as_the_childs_final_message_and_never_as_a_card() {
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        task_parent_assistant(),
        task_started("toolu_task", json!({})),
        sidechain_assistant(
            "toolu_task",
            json!([{"type": "tool_use", "id": "toolu_hb", "name": "SubagentHandback",
                "input": {"message": "all done"}}]),
        ),
        sidechain_user_result("toolu_task", "toolu_hb", "ack"),
    ];
    let events = run(&rows);
    let sub = subagent_events(&events);
    assert!(
        sub.contains(&&json!({"type": "timeline", "id": "toolu_task",
        "item": {"type": "assistant_message", "text": "all done", "messageId": "msg_c"}}))
    );
    assert!(!sub.iter().any(|e| e["item"]["callId"] == "toolu_hb"));
}

#[test]
fn background_child_survives_an_interrupt_while_foreground_children_are_canceled() {
    let rows = vec![
        user_prompt("go"),
        init("s1"),
        task_parent_assistant(),
        task_started("toolu_task", json!({})),
        out(
            json!({"type": "system", "subtype": "task_started", "task_id": "task-2",
            "tool_use_id": "toolu_bg", "description": "bg", "subagent_type": "Explore",
            "task_type": "local_agent", "is_backgrounded": true}),
        ),
        input(json!({"type": "control_request", "request": {"subtype": "interrupt"}})),
    ];
    let events = run(&rows);
    let canceled: Vec<&str> = subagent_events(&events)
        .iter()
        .filter(|e| e["status"] == "canceled")
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(canceled, ["toolu_task"]);
}
