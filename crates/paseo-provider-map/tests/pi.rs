//! Pi mapper: WP0 goldens (provider-events, mapper-vectors), history conversion, extension
//! adapters and the session state machine.

use paseo_agent_types::AgentStreamEvent;
use paseo_provider_map::pi::{
    map_pi_history, ExtensionResult, PiCapturedUserMessageEntry, PiChildSessionMapper,
    PiExtensionHost, PiMapper,
};
use paseo_provider_map::testing::{load_mapper_vectors, load_provider_events, replay};
use paseo_provider_map::{CounterEnv, ProviderMapper};
use paseo_tool_detail::detail::pi::{
    map_tool_detail, parse_tool_args, parse_tool_result, PiTrackedToolCall,
};
use serde_json::{json, Value};

// ---- goldens ------------------------------------------------------------------------------

fn golden(fixture: &str, status: &str, min_events: usize) {
    let g = load_provider_events("pi")
        .into_iter()
        .find(|g| g.fixture == fixture)
        .unwrap_or_else(|| panic!("no pi golden {fixture}"));
    assert_eq!(g.status, status, "{fixture}");
    assert!(!g.raw.is_empty(), "{fixture}: raw transcript missing");
    let mut mapper = PiMapper::new();
    let actual = replay(&mut mapper, &g.raw);
    assert!(
        actual.len() >= min_events,
        "{fixture}: {} events",
        actual.len()
    );
    assert_eq!(actual, g.expected, "{fixture}");
}

#[test]
fn golden_01_prompt_reply() {
    golden("01-prompt-reply", "ok", 23);
}

#[test]
fn golden_02_tool_bash() {
    golden("02-tool-bash", "ok", 47);
}

#[test]
fn golden_03_permission_allow() {
    golden("03-permission-allow", "ok", 436);
}

#[test]
fn golden_04_permission_deny() {
    golden("04-permission-deny", "ok", 264);
}

#[test]
fn golden_05_interrupt_partial() {
    golden("05-interrupt", "partial", 42);
}

#[test]
fn golden_06_resume_first() {
    golden("06-resume-first", "ok", 31);
}

#[test]
fn golden_06_resume() {
    golden("06-resume", "ok", 57);
}

#[test]
fn golden_07_model_thinking_switch_partial() {
    golden("07-model-thinking-switch", "partial", 6);
}

#[test]
fn failed_goldens_have_no_expected_events_and_the_catalog_maps_to_nothing() {
    let failed: Vec<_> = load_provider_events("pi")
        .into_iter()
        .filter(|g| g.status == "failed")
        .collect();
    assert!(failed.len() >= 4);
    for g in &failed {
        assert!(g.expected.is_empty(), "{}", g.fixture);
    }
    // OMP is host-only in P4 (Q8) so the capture has no events for it; the catalog has no turn.
    let catalog = failed.iter().find(|g| g.fixture == "08-catalog").unwrap();
    let mut mapper = PiMapper::new();
    assert!(replay(&mut mapper, &catalog.raw).is_empty());
}

// ---- mapper vectors -----------------------------------------------------------------------

fn kind_of(call: &PiTrackedToolCall) -> &'static str {
    match call {
        PiTrackedToolCall::Bash { .. } => "bash",
        PiTrackedToolCall::Read { .. } => "read",
        PiTrackedToolCall::Edit { .. } => "edit",
        PiTrackedToolCall::Write { .. } => "write",
        PiTrackedToolCall::Find { .. } => "find",
        PiTrackedToolCall::Grep { .. } => "grep",
        PiTrackedToolCall::Ls { .. } => "ls",
        PiTrackedToolCall::Unknown { .. } => "unknown",
    }
}

#[test]
fn mapper_vectors_parse_tool_args() {
    let vectors = load_mapper_vectors("pi");
    let mut n = 0;
    for v in vectors.iter().filter(|v| v.func == "pi.parseToolArgs") {
        let tool = v.input["toolName"].as_str().unwrap();
        let call = parse_tool_args(tool, Some(&v.input["args"]));
        assert_eq!(kind_of(&call), v.output["kind"], "{}", v.name);
        assert_eq!(tool, v.output["toolName"], "{}", v.name);
        if let PiTrackedToolCall::Unknown { args, .. } = &call {
            assert_eq!(args, &v.output["args"], "{}", v.name);
        }
        n += 1;
    }
    assert!(n >= 17, "{n}");
}

#[test]
fn mapper_vectors_parse_tool_result() {
    let mut n = 0;
    for v in load_mapper_vectors("pi")
        .iter()
        .filter(|v| v.func == "pi.parseToolResult")
    {
        let got = parse_tool_result(Some(&v.input["rawResult"])).to_value();
        assert_eq!(got, v.output, "{}", v.name);
        n += 1;
    }
    assert!(n >= 17, "{n}");
}

#[test]
fn mapper_vectors_map_tool_detail() {
    let mut n = 0;
    for v in load_mapper_vectors("pi")
        .iter()
        .filter(|v| v.func == "pi.mapToolDetail")
    {
        let tool = v.input["toolName"].as_str().unwrap();
        let call = parse_tool_args(tool, Some(&v.input["args"]));
        let result = if v.input["rawResultUndefined"] == json!(true) {
            None
        } else {
            Some(parse_tool_result(Some(&v.input["rawResult"])))
        };
        let got = map_tool_detail(&call, result.as_ref()).to_value();
        assert_eq!(got, v.output, "{}", v.name);
        n += 1;
    }
    assert!(n >= 17, "{n}");
}

// ---- history ------------------------------------------------------------------------------

fn history(messages: Value, entries: &[(&str, &str)]) -> Vec<Value> {
    let entries = entries
        .iter()
        .map(|(id, text)| PiCapturedUserMessageEntry {
            id: (*id).into(),
            text: (*text).into(),
        })
        .collect();
    let (events, _) = map_pi_history("pi", messages.as_array().unwrap(), entries);
    events.iter().map(AgentStreamEvent::to_value).collect()
}

fn items(events: &[Value]) -> Vec<Value> {
    events.iter().map(|e| e["item"].clone()).collect()
}

#[test]
fn history_custom_messages_are_completed_tools_and_private_context_is_hidden() {
    let events = history(
        json!([
            {"role": "custom", "customType": "project-context", "content": "Project instructions", "display": true},
            {"role": "custom", "customType": "private-context", "content": "Hidden instructions", "display": false},
            {"role": "custom", "customType": "project-context", "content": "Project instructions"},
        ]),
        &[],
    );
    let expected: Vec<Value> = [1, 2]
        .iter()
        .map(|i| {
            json!({
                "type": "tool_call", "callId": format!("pi-custom-{i}"), "name": "project-context",
                "status": "completed", "detail": {"type": "plain_text", "text": "Project instructions"},
                "metadata": {"synthetic": true, "customType": "project-context"}, "error": null,
            })
        })
        .collect();
    assert_eq!(items(&events), expected);
}

#[test]
fn history_replays_user_assistant_reasoning_and_tools() {
    let events = history(
        json!([
            {"role": "user", "content": [
                {"type": "text", "text": "read this"},
                {"type": "image", "data": "base64", "mimeType": "image/png"},
                {"type": "text", "text": "then answer"},
            ]},
            {"role": "assistant", "responseId": "response-1", "content": [
                {"type": "thinking", "thinking": "checking file"},
                {"type": "toolCall", "id": "tool-1", "name": "read", "arguments": {"path": "note.txt"}},
                {"type": "text", "text": "done"},
            ]},
            {"role": "toolResult", "toolCallId": "tool-1", "toolName": "read",
             "content": [{"type": "text", "text": "file contents"}]},
        ]),
        &[],
    );
    assert_eq!(
        items(&events),
        vec![
            json!({"type": "user_message", "text": "read this\n\nthen answer"}),
            json!({"type": "reasoning", "text": "checking file"}),
            json!({"type": "tool_call", "callId": "tool-1", "name": "read", "status": "running",
                   "detail": {"type": "read", "filePath": "note.txt"}, "error": null}),
            json!({"type": "assistant_message", "text": "done", "messageId": "response-1"}),
            json!({"type": "tool_call", "callId": "tool-1", "name": "read", "status": "completed",
                   "detail": {"type": "read", "filePath": "note.txt", "content": "file contents"},
                   "error": null}),
        ]
    );
    assert!(events.iter().all(|e| e["provider"] == "pi"));
}

#[test]
fn history_bash_execution_records_are_completed_shell_calls() {
    let events = history(
        json!([{"role": "bashExecution", "command": "echo hi", "output": "hi\n", "exitCode": 0, "timestamp": 123}]),
        &[],
    );
    assert_eq!(
        items(&events),
        vec![json!({
            "type": "tool_call", "callId": "pi-bash-123", "name": "bash", "status": "completed",
            "detail": {"type": "shell", "command": "echo hi", "output": "hi\n", "exitCode": 0},
            "error": null,
        })]
    );
    let cancelled = history(
        json!([{"role": "bashExecution", "command": "sleep", "cancelled": true, "timestamp": 5}]),
        &[],
    );
    assert_eq!(cancelled[0]["item"]["status"], "canceled");
    assert_eq!(cancelled[0]["item"]["detail"]["exitCode"], Value::Null);
}

#[test]
fn history_custom_message_without_type_is_a_custom_message_tool() {
    let events = history(
        json!([{"role": "custom", "content": "Extension command output"}]),
        &[],
    );
    assert_eq!(
        items(&events),
        vec![json!({
            "type": "tool_call", "callId": "pi-custom-1", "name": "custom-message",
            "status": "completed",
            "detail": {"type": "plain_text", "text": "Extension command output"},
            "metadata": {"synthetic": true, "customType": "custom-message"}, "error": null,
        })]
    );
}

#[test]
fn history_uses_tree_entry_ids_for_user_messages() {
    let events = history(
        json!([
            {"role": "user", "content": "first prompt"},
            {"role": "assistant", "content": [{"type": "text", "text": "first answer"}]},
            {"role": "user", "content": "second prompt"},
        ]),
        &[
            ("entry-user-1", "first prompt"),
            ("entry-user-2", "second prompt"),
        ],
    );
    assert_eq!(
        items(&events),
        vec![
            json!({"type": "user_message", "text": "first prompt", "messageId": "entry-user-1"}),
            json!({"type": "assistant_message", "text": "first answer", "messageId": "pi-history-assistant-1"}),
            json!({"type": "user_message", "text": "second prompt", "messageId": "entry-user-2"}),
        ]
    );
}

#[test]
fn history_failed_tool_results_carry_the_error_text() {
    let events = history(
        json!([
            {"role": "assistant", "content": [{"type": "toolCall", "id": "t", "name": "bash", "arguments": {"command": "false"}}]},
            {"role": "toolResult", "toolCallId": "t", "toolName": "bash", "isError": true,
             "content": [{"type": "text", "text": "boom"}]},
            {"role": "toolResult", "toolCallId": "gone", "toolName": "other", "isError": true, "content": []},
        ]),
        &[],
    );
    assert_eq!(events[1]["item"]["status"], "failed");
    assert_eq!(events[1]["item"]["error"], "boom");
    assert_eq!(events[2]["item"]["error"], "Tool call failed");
    assert_eq!(events[2]["item"]["detail"]["type"], "unknown");
}

#[test]
fn history_extension_detail_matches_the_live_mapping_and_unrelated_tools_stay_generic() {
    let events = history(
        json!([
            {"role": "assistant", "content": [{"type": "toolCall", "id": "call-1", "name": "subagent",
                "arguments": {"agent": "scout", "task": "Inspect files"}}]},
            {"role": "toolResult", "toolCallId": "call-1", "toolName": "subagent",
             "content": [{"type": "text", "text": "Found two files"}]},
        ]),
        &[],
    );
    // running row, its running upsert, then the completed row
    assert_eq!(events[1]["type"], "provider_subagent");
    assert_eq!(events[1]["event"]["status"], "running");
    assert_eq!(
        events[2]["item"]["detail"],
        json!({"type": "sub_agent", "subAgentType": "scout", "description": "Inspect files",
               "log": "Found two files"})
    );
    // without result details the completed mapping carries no sub-agent upsert
    assert_eq!(events.len(), 3);
}

// ---- extension host -----------------------------------------------------------------------

#[test]
fn host_claims_subagent_tools_and_declines_unrelated_ones() {
    use paseo_provider_map::pi::ExtToolCall;
    let mut host = PiExtensionHost::new();
    let result = parse_tool_result(Some(
        &json!({"content": [{"type": "text", "text": "Found two files"}]}),
    ));
    let call = |name: &str, status| ExtToolCall {
        call_id: "call-1".into(),
        tool_name: name.into(),
        args: json!({"agent": "scout", "task": "Inspect files"}),
        status,
        result: result.clone(),
    };
    let out = host
        .map_tool_call(&call("subagent", "completed"))
        .expect("claimed");
    assert_eq!(
        out.detail.unwrap().to_value(),
        json!({"type": "sub_agent", "subAgentType": "scout", "description": "Inspect files", "log": "Found two files"})
    );
    assert!(host
        .map_tool_call(&call("unrelated", "completed"))
        .is_none());
}

#[test]
fn host_records_child_sessions_for_the_session_layer() {
    use paseo_provider_map::pi::ExtToolCall;
    let mut host = PiExtensionHost::new();
    let details = json!({"details": {
        "mode": "single",
        "results": [{"agent": "scout", "success": true, "sessionFile": "/tmp/child.jsonl"}],
    }});
    let result = parse_tool_result(Some(&details));
    let out = host
        .map_tool_call(&ExtToolCall {
            call_id: "c1".into(),
            tool_name: "subagent".into(),
            args: json!({"agent": "scout", "task": "t"}),
            status: "completed",
            result,
        })
        .unwrap();
    assert_eq!(out.child_sessions.len(), 1);
    let taken = host.take_child_sessions();
    assert_eq!(taken[0].file, "/tmp/child.jsonl");
    assert_eq!(taken[0].id, "c1");
    assert!(host.take_child_sessions().is_empty());
    assert_eq!(out.events[0]["event"]["status"], "completed");
}

#[test]
fn child_session_mapper_holds_only_timeline_rows() {
    let mut m = PiChildSessionMapper::new("child-1");
    assert!(m.map_line("not json").is_empty());
    assert!(m.map_line(r#"{"type":"session"}"#).is_empty());
    let events = m.map_line(
        r#"{"type":"message","timestamp":"2026-01-01T00:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":"BEGIN"}]}}"#,
    );
    let v: Vec<Value> = events.iter().map(AgentStreamEvent::to_value).collect();
    assert_eq!(v.len(), 1);
    assert_eq!(v[0]["type"], "provider_subagent");
    assert_eq!(v[0]["event"]["id"], "child-1");
    assert_eq!(v[0]["event"]["timestamp"], "2026-01-01T00:00:00Z");
    assert_eq!(v[0]["event"]["item"]["text"], "BEGIN");
}

// ---- session state machine ----------------------------------------------------------------

struct Run {
    mapper: PiMapper,
    env: CounterEnv,
    events: Vec<Value>,
}

impl Run {
    fn new() -> Self {
        let mut r = Self {
            mapper: PiMapper::new(),
            env: CounterEnv::new(),
            events: Vec::new(),
        };
        r.out(
            json!({"type": "response", "command": "get_state", "success": true,
                     "data": {"sessionId": "sess-1"}}),
        );
        r
    }

    fn feed(&mut self, raw: Value) -> &mut Self {
        let evs = self.mapper.feed(&raw, &mut self.env);
        self.events
            .extend(evs.iter().map(AgentStreamEvent::to_value));
        self
    }

    fn out(&mut self, line: Value) -> &mut Self {
        self.feed(json!({"dir": "out", "line": line}))
    }

    fn inp(&mut self, line: Value) -> &mut Self {
        self.feed(json!({"dir": "in", "line": line}))
    }

    fn types(&self) -> Vec<String> {
        self.events
            .iter()
            .map(|e| e["type"].as_str().unwrap().to_string())
            .collect()
    }
}

fn turn(r: &mut Run) {
    r.inp(json!({"type": "prompt", "message": "hi"}));
    r.out(json!({"type": "agent_start"}));
    r.out(json!({"type": "turn_start"}));
}

#[test]
fn error_terminal_message_fails_the_turn_with_details() {
    let mut r = Run::new();
    turn(&mut r);
    r.out(json!({"type": "agent_end", "messages": [
        {"role": "assistant", "errorMessage": " rate limited ", "stopReason": "error",
         "provider": "nvidia", "model": "m", "responseModel": "rm", "responseId": "rid",
         "content": [{"type": "text", "text": "partial answer"}]},
    ]}));
    let failed = r.events.last().unwrap();
    assert_eq!(failed["type"], "turn_failed");
    assert_eq!(
        failed["error"],
        "rate limited (stopReason=error, model=nvidia/m, responseModel=rm, responseId=rid, partial=\"partial answer\")"
    );
    assert_eq!(failed["turnId"], "00000000-0000-4000-8000-000000000002");
}

#[test]
fn agent_end_with_will_retry_waits_for_agent_settled() {
    let mut r = Run::new();
    turn(&mut r);
    r.out(json!({"type": "agent_end", "willRetry": true, "messages": []}));
    assert!(!r.types().contains(&"turn_completed".to_string()));
    r.out(json!({"type": "agent_settled"}));
    assert_eq!(r.types().last().unwrap(), "turn_completed");
    // boundary events without an active turn are ignored
    let n = r.events.len();
    r.out(json!({"type": "agent_settled"}));
    r.out(json!({"type": "agent_end", "messages": []}));
    assert_eq!(r.events.len(), n);
}

#[test]
fn interrupt_cancels_the_active_turn_and_late_terminals_are_ignored() {
    let mut r = Run::new();
    turn(&mut r);
    r.inp(json!({"type": "abort"}));
    let last = r.events.last().unwrap().clone();
    assert_eq!(last["type"], "turn_canceled");
    assert_eq!(last["reason"], "interrupted");
    r.out(json!({"type": "agent_end", "messages": [{"role": "assistant", "stopReason": "aborted", "content": []}]}));
    assert_eq!(
        r.types().iter().filter(|t| *t == "turn_canceled").count(),
        1
    );
    assert!(!r.types().contains(&"turn_failed".to_string()));
}

#[test]
fn terminal_error_during_interrupt_is_held_until_the_abort_rpc_fails() {
    let mut m = PiMapper::new();
    let mut env = CounterEnv::new();
    m.start_turn("hi", None, &mut env).unwrap();
    m.handle_runtime_event(&json!({"type": "agent_start"}), &mut env);
    let token = m.begin_interrupt();
    m.handle_runtime_event(
        &json!({"type": "agent_end", "messages": [{"role": "assistant", "errorMessage": "x", "content": []}]}),
        &mut env,
    );
    // held: no terminal event yet
    m.finish_interrupt(token, Err("abort failed".into()));
    // `feed` drains the buffer; the public handlers leave events for the next feed
    let evs = m.feed(&json!({}), &mut env);
    let v: Vec<Value> = evs.iter().map(AgentStreamEvent::to_value).collect();
    assert_eq!(v.last().unwrap()["type"], "turn_failed");
    assert_eq!(v.last().unwrap()["error"], "x");
}

#[test]
fn process_exit_fails_the_active_turn_only() {
    let mut r = Run::new();
    r.out(json!({"type": "process_exit", "error": "pi crashed"}));
    assert!(r.events.is_empty());
    turn(&mut r);
    r.out(json!({"type": "process_exit", "error": "pi crashed"}));
    let last = r.events.last().unwrap();
    assert_eq!(last["type"], "turn_failed");
    assert_eq!(last["error"], "pi crashed");
}

#[test]
fn compaction_retry_and_thinking_events() {
    let mut r = Run::new();
    turn(&mut r);
    r.out(json!({"type": "compaction_start", "reason": "manual"}));
    r.out(json!({"type": "compaction_end", "reason": "threshold"}));
    r.out(json!({"type": "auto_retry_start", "attempt": 2, "maxAttempts": 3, "delayMs": 1, "errorMessage": "overloaded"}));
    r.out(json!({"type": "thinking_level_changed", "level": "high"}));
    r.out(json!({"type": "thinking_level_changed", "level": "bogus"}));
    let tail: Vec<&Value> = r.events.iter().skip(2).collect();
    assert_eq!(
        tail[0]["item"],
        json!({"type": "compaction", "status": "loading", "trigger": "manual"})
    );
    assert_eq!(
        tail[1]["item"],
        json!({"type": "compaction", "status": "completed", "trigger": "auto"})
    );
    assert_eq!(
        tail[2]["item"],
        json!({"type": "error", "message": "Provider retry (attempt 2): overloaded"})
    );
    assert_eq!(tail[3]["type"], "thinking_option_changed");
    assert_eq!(tail[3]["thinkingOptionId"], "high");
    assert_eq!(tail.len(), 4);
}

#[test]
fn message_updates_without_message_start_mint_an_id_once() {
    let mut r = Run::new();
    turn(&mut r);
    for d in ["a", "b"] {
        r.out(json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": d}}));
    }
    r.out(json!({"type": "message_update", "message": {"role": "user"}, "assistantMessageEvent": {"type": "text_delta", "delta": "x"}}));
    let msgs: Vec<&Value> = r
        .events
        .iter()
        .filter(|e| e["type"] == "timeline" && e["item"]["type"] == "assistant_message")
        .collect();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["item"]["messageId"], msgs[1]["item"]["messageId"]);
    // uuid 1 = session key, 2 = turn id, 3 = assistant message
    assert_eq!(
        msgs[0]["item"]["messageId"],
        "00000000-0000-4000-8000-000000000003"
    );
}

#[test]
fn response_id_names_the_assistant_message() {
    let mut r = Run::new();
    turn(&mut r);
    r.out(
        json!({"type": "message_start", "message": {"role": "assistant", "responseId": "resp-9"}}),
    );
    r.out(json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "hello"}}));
    r.out(json!({"type": "message_end", "message": {"role": "assistant"}}));
    r.out(json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": "again"}}));
    let ids: Vec<&Value> = r
        .events
        .iter()
        .filter(|e| e["type"] == "timeline" && e["item"]["type"] == "assistant_message")
        .map(|e| &e["item"]["messageId"])
        .collect();
    assert_eq!(ids[0], "resp-9");
    assert_ne!(ids[1], "resp-9");
}

#[test]
fn failed_tool_execution_reports_the_raw_result_as_error() {
    let mut r = Run::new();
    turn(&mut r);
    r.out(json!({"type": "tool_execution_start", "toolCallId": "t1", "toolName": "bash", "args": {"command": "false"}}));
    r.out(json!({"type": "tool_execution_update", "toolCallId": "t1", "toolName": "bash", "partialResult": "so far"}));
    r.out(json!({"type": "tool_execution_update", "toolCallId": "nope", "toolName": "bash", "partialResult": "x"}));
    r.out(json!({"type": "tool_execution_end", "toolCallId": "t1", "toolName": "bash", "isError": true,
                 "result": {"content": [{"type": "text", "text": "exit 1"}], "exitCode": 1}}));
    let tools: Vec<&Value> = r
        .events
        .iter()
        .filter(|e| e["item"]["type"] == "tool_call")
        .collect();
    assert_eq!(tools.len(), 3);
    assert_eq!(tools[1]["item"]["detail"]["output"], "so far");
    assert_eq!(tools[2]["item"]["status"], "failed");
    assert_eq!(tools[2]["item"]["error"]["exitCode"], 1);
    assert_eq!(tools[2]["item"]["detail"]["exitCode"], 1);
}

#[test]
fn notifications_markers_and_user_entries() {
    let mut r = Run::new();
    r.out(json!({"type": "extension_ui_request", "id": "n0", "method": "notify", "notifyType": "warning", "message": "heads up"}));
    r.out(json!({"type": "extension_ui_request", "id": "n1", "method": "notify",
                 "message": "PASEO_ENTRY_CAPTURE {\"requestId\":\"req-1\",\"treeEntries\":[{\"id\":\" a \",\"parentId\":null,\"text\":\"t\"}],\"contextEntries\":[{\"id\":\"b\",\"parentId\":\"a\",\"text\":\"u\"},{\"text\":\"no id\"}]}"}));
    assert_eq!(r.events.len(), 1);
    assert_eq!(
        r.events[0]["item"],
        json!({"type": "notification", "level": "warning", "message": "heads up"})
    );
    assert_eq!(r.mapper.context_user_entries().len(), 1);
    assert_eq!(r.mapper.tree_user_entries()[0].id, "a");
    assert_eq!(
        r.mapper.context_user_entries()[0].parent_id.as_deref(),
        Some("a")
    );
    assert_eq!(
        r.mapper.take_extension_results(),
        vec![ExtensionResult::Resolved {
            request_id: "req-1".into(),
            result: Value::Null
        }]
    );
    r.out(json!({"type": "extension_ui_request", "id": "n2", "method": "notify",
                 "message": "PASEO_COMMAND_RESULT {\"requestId\":\"req-2\",\"ok\":false,\"error\":\"nope\"}"}));
    assert_eq!(
        r.mapper.take_extension_results(),
        vec![ExtensionResult::Rejected {
            request_id: "req-2".into(),
            error: "nope".into()
        }]
    );
    assert_eq!(r.events.len(), 1);
    // a submitted user entry becomes a user message with the steer client id
    r.mapper.steer_accepted("steered", Some("client-7"));
    r.out(json!({"type": "extension_ui_request", "id": "n3", "method": "notify",
                 "message": "PASEO_SUBMITTED_USER_ENTRY {\"entry\":{\"id\":\"e9\",\"text\":\"steered\"}}"}));
    assert_eq!(
        r.events[1]["item"],
        json!({"type": "user_message", "text": "steered", "messageId": "e9", "clientMessageId": "client-7"})
    );
}

#[test]
fn dialogs_become_question_permissions_and_default_responses() {
    let mut r = Run::new();
    r.out(json!({"type": "extension_ui_request", "id": "d1", "method": "confirm", "title": "Sure?", "message": "Really"}));
    r.out(json!({"type": "extension_ui_request", "id": "d2", "method": "input", "title": "Add a comment", "placeholder": "optional, skip allowed"}));
    r.out(json!({"type": "extension_ui_request", "id": "d3", "method": "setWidget"}));
    assert_eq!(r.events.len(), 2);
    assert_eq!(r.events[0]["request"]["title"], "Sure?\n\nReally");
    assert_eq!(
        r.events[0]["request"]["input"]["questions"][0]["options"],
        json!([{"label": "Yes"}, {"label": "No"}])
    );
    let q = &r.events[1]["request"]["input"]["questions"][0];
    assert_eq!(q["question"], "Optional comment");
    assert_eq!(q["allowEmpty"], true);
    assert_eq!(q["dismissLabel"], "Skip");
    assert_eq!(r.mapper.pending_permissions().len(), 2);

    let answer = json!({"behavior": "allow", "updatedInput": {"answers": {"Response": " YES "}}});
    r.mapper.respond_to_permission("d1", &answer).unwrap();
    assert_eq!(
        r.mapper.take_ui_responses(),
        vec![("d1".to_string(), json!({"confirmed": true}))]
    );
    assert!(r.mapper.respond_to_permission("d1", &answer).is_err());
    r.mapper
        .respond_to_permission("d2", &json!({"behavior": "deny"}))
        .unwrap();
    assert_eq!(
        r.mapper.take_ui_responses(),
        vec![("d2".to_string(), json!({"cancelled": true}))]
    );
}

#[test]
fn slash_command_without_agent_run_completes_with_buffered_output() {
    let mut r = Run::new();
    r.inp(json!({"type": "prompt", "message": "/status"}));
    r.out(json!({"type": "command_output", "text": "\u{1b}[32mall good\u{1b}[0m\n"}));
    r.out(json!({"type": "prompt_result", "id": "req_2", "agentInvoked": false}));
    assert!(r.events.is_empty(), "result before ack is only recorded");
    r.mapper.prompt_acked(Some("req_2"), None);
    let evs = r.mapper.feed(&json!({}), &mut r.env);
    let v: Vec<Value> = evs.iter().map(AgentStreamEvent::to_value).collect();
    assert_eq!(
        v[0]["item"],
        json!({"type": "user_message", "text": "/status"})
    );
    assert_eq!(
        v[1]["item"],
        json!({"type": "assistant_message", "text": "all good"})
    );
    assert_eq!(v[2]["type"], "turn_completed");
}

#[test]
fn custom_message_end_outside_a_turn_completes_and_maps_a_tool_row() {
    let mut r = Run::new();
    r.inp(json!({"type": "prompt", "message": "/ext"}));
    r.out(json!({"type": "message_end", "message": {"role": "custom", "customType": "ext-note", "content": "note"}}));
    assert_eq!(
        r.events[0]["item"]["callId"],
        "pi-custom-00000000-0000-4000-8000-000000000003"
    );
    assert_eq!(r.events[0]["item"]["metadata"]["customType"], "ext-note");
    assert_eq!(r.events[1]["type"], "turn_completed");
}

#[test]
fn second_prompt_while_active_is_rejected() {
    let mut m = PiMapper::new();
    let mut env = CounterEnv::new();
    assert!(m.start_turn("a", None, &mut env).is_ok());
    assert_eq!(
        m.start_turn("b", None, &mut env).unwrap_err(),
        "A Pi turn is already active"
    );
}

#[test]
fn ask_user_question_flows_through_permission_and_result() {
    let mut r = Run::new();
    turn(&mut r);
    let args = json!({"questions": [{"question": "Pick", "header": "Choice",
        "options": [{"label": "A", "description": "a"}, {"label": "B", "description": "b"}]}]});
    r.out(json!({"type": "tool_execution_start", "toolCallId": "q1", "toolName": "ask_user_question", "args": args}));
    assert_eq!(r.events[2]["type"], "permission_requested");
    assert_eq!(r.events[2]["request"]["id"], "rpiv-question:q1");
    // a dialog arriving before the answer is deferred, then answered from the answers map
    r.out(
        json!({"type": "extension_ui_request", "id": "ui-1", "method": "select", "title": "Pick",
                 "options": ["1. A — a", "2. B — b", "3. Type something."]}),
    );
    assert!(r.mapper.take_ui_responses().is_empty());
    r.mapper
        .respond_to_permission(
            "rpiv-question:q1",
            &json!({"behavior": "allow", "updatedInput": {"answers": {"Choice": "B"}}}),
        )
        .unwrap();
    assert_eq!(
        r.mapper.take_ui_responses(),
        vec![("ui-1".to_string(), json!({"value": "2. B — b"}))]
    );
    r.out(
        json!({"type": "tool_execution_end", "toolCallId": "q1", "toolName": "ask_user_question",
                 "result": {"content": [], "details": {"cancelled": false,
                    "answers": [{"question": "Pick", "kind": "option", "answer": "B"}]}}}),
    );
    let last = r.events.last().unwrap();
    assert_eq!(
        last["item"]["detail"],
        json!({"type": "plain_text", "label": "Questions", "text": "Pick: B"})
    );
}

#[test]
fn mcp_adapter_names_and_todo_timelines() {
    let mut r = Run::new();
    turn(&mut r);
    r.out(
        json!({"type": "tool_execution_start", "toolCallId": "m1", "toolName": "mcp",
                 "args": {"server": "github", "tool": "github_create_issue"}}),
    );
    assert_eq!(r.events[2]["item"]["name"], "github.create_issue");
    r.out(json!({"type": "tool_execution_end", "toolCallId": "td", "toolName": "todo",
        "result": {"content": [], "details": {"action": "list", "params": {}, "nextId": 3,
            "tasks": [
                {"id": 1, "subject": "one", "status": "completed"},
                {"id": 2, "subject": "gone", "status": "deleted"},
                {"id": 3, "subject": "two", "status": "in_progress", "activeForm": "Doing two"}]}}}));
    let todo = r
        .events
        .iter()
        .find(|e| e["item"]["type"] == "todo")
        .expect("todo item");
    assert_eq!(
        todo["item"]["items"],
        json!([
            {"id": "1", "text": "one", "status": "completed", "completed": true},
            {"id": "3", "text": "two", "status": "in_progress", "completed": false, "activeForm": "Doing two"},
        ])
    );
}

#[test]
fn command_output_strips_csi_and_osc_sequences() {
    let mut r = Run::new();
    turn(&mut r);
    r.out(json!({"type": "command_output", "text": "\u{1b}]0;title\u{7}\u{1b}[1;31mred\u{1b}[0m plain \u{1b}[2K"}));
    r.out(json!({"type": "command_output", "text": "\u{1b}[0m  "}));
    let texts: Vec<&Value> = r
        .events
        .iter()
        .filter(|e| e["item"]["type"] == "assistant_message")
        .map(|e| &e["item"]["text"])
        .collect();
    assert_eq!(texts, vec!["red plain"]);
}
