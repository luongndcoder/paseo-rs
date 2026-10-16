//! Ported/derived tests for status, ids, JS helpers, merge, limits, names and the display model:
//! `agent-timeline-content.test.ts`, `tool-name-normalization.test.ts`,
//! `tool-call-display.test.ts`, `timeline-identity.test.ts`, merge cases from
//! `agent-stream-coalescer.test.ts` and `timeline-projection.test.ts`.

use paseo_agent_types::{AgentTimelineItem, ToolCallDetail, ToolCallItem, ToolCallStatus};
use paseo_tool_detail::js::*;
use paseo_tool_detail::names::*;
use paseo_tool_detail::*;
use serde_json::{json, Value};

// ---- status ----------------------------------------------------------------------------------

#[test]
fn status_vocabulary() {
    use ToolCallStatus::*;
    let n =
        |s: Option<&str>, e: Option<&Value>, o: Option<&Value>| normalize_tool_call_status(s, e, o);
    assert_eq!(n(Some("  ERROR "), None, None), Failed);
    assert_eq!(n(Some("declined"), None, None), Failed);
    assert_eq!(n(Some("Cancelled"), None, None), Canceled);
    assert_eq!(n(Some("aborted"), None, None), Canceled);
    assert_eq!(n(Some("complete"), None, None), Completed);
    assert_eq!(n(Some("succeeded"), None, None), Completed);
    assert_eq!(n(Some("inProgress"), None, None), Running);
    // error wins over status; null error is no error
    assert_eq!(n(Some("completed"), Some(&json!("boom")), None), Failed);
    assert_eq!(n(Some("completed"), Some(&Value::Null), None), Completed);
    // blank status falls through to output presence
    assert_eq!(n(Some("   "), None, Some(&json!({}))), Completed);
    assert_eq!(n(None, None, Some(&Value::Null)), Running);
    assert_eq!(n(None, None, None), Running);
}

// ---- ids / js helpers ---------------------------------------------------------------------------

#[test]
fn hash_text_matches_js() {
    // vectors produced with node: hashText from tool-call-mapper-utils.ts
    assert_eq!(hash_text(""), "0");
    assert_eq!(hash_text("a"), "2p");
    assert_eq!(hash_text("shell:{\"command\":\"pwd\"}"), "xakifs");
    assert_eq!(hash_text("read:null"), "izcv11");
    assert_eq!(hash_text("x:"), "2wy");
    assert_eq!(hash_text("😀 emoji 𝒳 test"), "2ts1a5");
    let long = "The quick brown fox jumps over the lazy dog".repeat(20);
    assert_eq!(hash_text(&long), "v0v7b4");
}

#[test]
fn coerce_tool_call_id_rules() {
    assert_eq!(
        coerce_tool_call_id(
            &json!({"providerPrefix":"pi","rawCallId":"abc","toolName":"x","input":null})
        ),
        "abc"
    );
    // blank raw id is replaced, undefined input hashes as ""
    assert_eq!(
        coerce_tool_call_id(&json!({"providerPrefix":"pi","rawCallId":"  ","toolName":"x"})),
        "pi-2wy"
    );
    // null input stringifies to "null"
    assert_eq!(
        coerce_tool_call_id(
            &json!({"providerPrefix":"pi","rawCallId":null,"toolName":"read","input":null})
        ),
        "pi-izcv11"
    );
    assert_eq!(
        coerce_tool_call_id(
            &json!({"providerPrefix":"c","toolName":"shell","input":{"command":"pwd"}})
        ),
        "c-xakifs"
    );
}

#[test]
fn js_number_formatting_and_stringify() {
    assert_eq!(js_number_to_string(1e21), "1e+21");
    assert_eq!(js_number_to_string(1.5), "1.5");
    assert_eq!(js_number_to_string(0.000001), "0.000001");
    assert_eq!(js_number_to_string(1e-7), "1e-7");
    assert_eq!(
        js_number_to_string(123456789012345680000.0),
        "123456789012345680000"
    );
    assert_eq!(js_number_to_string(-0.0), "0");
    assert_eq!(js_number_to_string(0.1 + 0.2), "0.30000000000000004");
    assert_eq!(js_number_to_string(5e-324), "5e-324");
    assert_eq!(js_number_to_string(f64::MAX), "1.7976931348623157e+308");
    assert_eq!(js_number_to_string(0.0000125), "0.0000125");
    let v = json!({"a":[1,{"b":1e21}],"c":"x\u{2028}y\u{7f}\n","d":{},"e":[]});
    assert_eq!(
        json_stringify_pretty(&v),
        "{\n  \"a\": [\n    1,\n    {\n      \"b\": 1e+21\n    }\n  ],\n  \"c\": \"x\u{2028}y\u{7f}\\n\",\n  \"d\": {},\n  \"e\": []\n}"
    );
    assert_eq!(
        json_stringify(&json!({"a":[1,2],"b":null})),
        "{\"a\":[1,2],\"b\":null}"
    );
}

#[test]
fn utf16_helpers() {
    assert_eq!(utf16_len("a😀b"), 4);
    assert_eq!(utf16_prefix("a😀b", 1), "a");
    assert_eq!(utf16_prefix("a😀b", 3), "a😀");
    assert_eq!(utf16_prefix("a😀b", 4), "a😀b");
    // PARITY-QUIRK(surrogate-truncation): a cut inside a pair leaves a lone high surrogate,
    // carried as a private-use scalar and written as the \ud83d escape
    let cut = utf16_prefix("a😀b", 2);
    assert_eq!(utf16_len(&cut), 2);
    assert_eq!(
        paseo_agent_types::js_str::to_js_json_string(&json!(cut)),
        "\"a\\ud83d\""
    );
    // the same escape V8 prints
    assert_eq!(
        paseo_agent_types::js_str::utf16_units(&cut),
        vec![0x61, 0xD83D]
    );
    assert_eq!(js_trim("\u{feff} x \u{2028}"), "x");
    assert_eq!(js_trim("\u{85}x"), "\u{85}x"); // NEL is not JS whitespace
}

// ---- limits (agent-timeline-content.test.ts) -------------------------------------------------------

fn tool(status: ToolCallStatus, detail: ToolCallDetail) -> ToolCallItem {
    ToolCallItem::new("c", "n", status, detail)
}

#[test]
fn marks_truncated_plain_text_within_budget() {
    let oversized = "x".repeat(64 * 1024 + 1);
    let item = AgentTimelineItem::tool_call(ToolCallItem::new(
        "terminal-session-4242",
        "terminal",
        ToolCallStatus::Completed,
        ToolCallDetail::PlainText {
            label: None,
            text: Some(oversized),
            icon: Some(paseo_agent_types::ToolCallIcon::SquareTerminal),
        },
    ));
    let limited = limit_item_content(item);
    assert_eq!(limit_item_content(limited.clone()), limited);
    let want_text = format!("{}\n\n[Content truncated]", "x".repeat(64 * 1024 - 21));
    assert_eq!(
        limited.to_value(),
        json!({"type":"tool_call","callId":"terminal-session-4242","name":"terminal",
            "status":"completed","error":null,
            "detail":{"type":"plain_text","text":want_text,"icon":"square_terminal"}})
    );
}

#[test]
fn limits_shell_output_and_failed_error_content() {
    let big = "y".repeat(64 * 1024 + 10);
    let item = AgentTimelineItem::tool_call(tool(
        ToolCallStatus::Completed,
        ToolCallDetail::Shell {
            command: "c".into(),
            cwd: None,
            output: Some(big.clone()),
            exit_code: None,
        },
    ));
    let v = limit_item_content(item).to_value();
    assert_eq!(v["detail"]["output"].as_str().unwrap().len(), 64 * 1024);

    let mut failed = ToolCallItem::failed(
        "c",
        "n",
        ToolCallDetail::Shell {
            command: "c".into(),
            cwd: None,
            output: None,
            exit_code: None,
        },
        json!({"content": big, "other": 1}),
    );
    failed.status = ToolCallStatus::Failed;
    let v = limit_item_content(AgentTimelineItem::tool_call(failed)).to_value();
    assert_eq!(v["error"]["content"].as_str().unwrap().len(), 64 * 1024);
    assert_eq!(v["error"]["other"], 1);
}

#[test]
fn limit_counts_utf16_units_not_chars() {
    // 32768 astral chars = 65536 UTF-16 units: exactly at the limit, untouched
    let at_limit = "😀".repeat(32 * 1024);
    let item = AgentTimelineItem::tool_call(tool(
        ToolCallStatus::Completed,
        ToolCallDetail::Shell {
            command: "c".into(),
            cwd: None,
            output: Some(at_limit.clone()),
            exit_code: None,
        },
    ));
    let v = limit_item_content(item).to_value();
    assert_eq!(v["detail"]["output"], at_limit);

    // one unit over: cut lands inside the last pair
    let over = format!("{at_limit}a");
    let item = AgentTimelineItem::tool_call(tool(
        ToolCallStatus::Completed,
        ToolCallDetail::Shell {
            command: "c".into(),
            cwd: None,
            output: Some(over),
            exit_code: None,
        },
    ));
    let v = limit_item_content(item).to_value();
    assert_eq!(v["detail"]["output"], at_limit);

    // odd boundary: 'a' + astral pairs; cut falls mid-pair (PARITY-QUIRK(surrogate-truncation))
    let odd = format!("a{}", "😀".repeat(32 * 1024));
    let item = AgentTimelineItem::tool_call(tool(
        ToolCallStatus::Completed,
        ToolCallDetail::Shell {
            command: "c".into(),
            cwd: None,
            output: Some(odd),
            exit_code: None,
        },
    ));
    let v = limit_item_content(item).to_value();
    let out = v["detail"]["output"].as_str().unwrap();
    assert_eq!(utf16_len(out), 64 * 1024);
    assert!(
        paseo_agent_types::js_str::to_js_json_string(&v["detail"]["output"]).ends_with("\\ud83d\"")
    );
    // a second pass is a no-op (length stays at the limit)
    let again = limit_item_content(
        AgentTimelineItem::from_value(&json!({
        "type":"tool_call","callId":"c","name":"n","status":"completed","error":null,
        "detail":{"type":"shell","command":"c","output":out}}))
        .unwrap(),
    )
    .to_value();
    assert_eq!(again["detail"]["output"], v["detail"]["output"]);
}

#[test]
fn non_tool_items_untouched_and_plugin_size_check() {
    let item = AgentTimelineItem::AssistantMessage {
        text: "z".repeat(200_000),
        message_id: None,
    };
    assert_eq!(limit_item_content(item.clone()), item);
    assert!(assert_plugin_timeline_data_size(&json!({"a": "x"})).is_ok());
    let big = json!({"a": "x".repeat(PLUGIN_TIMELINE_DATA_MAX_BYTES)});
    assert_eq!(
        assert_plugin_timeline_data_size(&big).unwrap_err(),
        "Plugin timeline item data exceeds 65536 bytes"
    );
}

// ---- merge (coalescer + projection cases) ---------------------------------------------------------------

fn shell_item(status: ToolCallStatus, output: &str, exit: Option<i64>) -> ToolCallItem {
    ToolCallItem::new(
        "tool-1",
        "shell",
        status,
        ToolCallDetail::Shell {
            command: "printf ok".into(),
            cwd: None,
            output: Some(output.into()),
            exit_code: Some(exit.map(Into::into)),
        },
    )
}

#[test]
fn merge_latest_snapshot_wins() {
    let a = shell_item(ToolCallStatus::Running, "first", None);
    let b = shell_item(ToolCallStatus::Running, "second", None);
    let m = merge_tool_call_items(&a, &b);
    assert_eq!(
        m.to_value(),
        json!({"type":"tool_call","callId":"tool-1","name":"shell","status":"running","error":null,
            "detail":{"type":"shell","command":"printf ok","output":"second","exitCode":null}})
    );
}

#[test]
fn merge_terminal_status_clears_error_and_failed_takes_incoming_error() {
    let running = shell_item(ToolCallStatus::Running, "r", None);
    let done = shell_item(ToolCallStatus::Completed, "done", Some(0));
    let m = merge_tool_call_items(&running, &done);
    assert_eq!(m.status, ToolCallStatus::Completed);
    assert_eq!(m.error, Some(Value::Null));

    let mut failed = shell_item(ToolCallStatus::Failed, "x", Some(1));
    failed.error = Some(json!({"message":"boom"}));
    let m = merge_tool_call_items(&running, &failed);
    assert_eq!(m.error, Some(json!({"message":"boom"})));
    // a failed item with no error key: error becomes absent
    failed.error = None;
    let m = merge_tool_call_items(&running, &failed);
    assert_eq!(m.error, None);
}

#[test]
fn merge_keeps_known_detail_over_unknown() {
    let mut a = ToolCallItem::new(
        "agent-call",
        "Task",
        ToolCallStatus::Running,
        ToolCallDetail::SubAgent {
            sub_agent_type: Some("general-purpose".into()),
            description: Some("List files".into()),
            child_session_id: None,
            log: "".into(),
            actions: Some(vec![]),
        },
    );
    a.error = Some(Value::Null);
    let b = ToolCallItem::new(
        "agent-call",
        "Agent",
        ToolCallStatus::Completed,
        ToolCallDetail::unknown(json!({"description":"List files"}), Value::Null),
    );
    let m = merge_tool_call_items(&a, &b);
    assert_eq!(m.name, "Agent");
    assert_eq!(m.status, ToolCallStatus::Completed);
    assert_eq!(
        m.detail.to_value(),
        json!({"type":"sub_agent","subAgentType":"general-purpose","description":"List files","log":"","actions":[]})
    );
    // and the reverse: a known incoming detail replaces an unknown one
    let m2 = merge_tool_call_items(&b, &a);
    assert_eq!(m2.detail.type_name(), "sub_agent");
}

#[test]
fn merge_unknown_lifecycle_replaces_output() {
    let a = ToolCallItem::new(
        "call_1",
        "shell",
        ToolCallStatus::Running,
        ToolCallDetail::unknown(json!({"cmd":"pwd"}), Value::Null),
    );
    let b = ToolCallItem::new(
        "call_1",
        "shell",
        ToolCallStatus::Completed,
        ToolCallDetail::unknown(json!({"cmd":"pwd"}), json!({"stdout":"/tmp"})),
    );
    let m = merge_tool_call_items(&a, &b);
    assert_eq!(m.status, ToolCallStatus::Completed);
    assert_eq!(
        m.detail.to_value(),
        json!({"type":"unknown","input":{"cmd":"pwd"},"output":{"stdout":"/tmp"}})
    );
}

#[test]
fn merge_metadata_and_extras() {
    let mut a = shell_item(ToolCallStatus::Running, "a", None);
    a.metadata = json!({"x":1,"y":1}).as_object().cloned();
    a.extra.insert("futureField".into(), json!({"kept":true}));
    let mut b = shell_item(ToolCallStatus::Running, "b", None);
    b.metadata = json!({"y":2}).as_object().cloned();
    let m = merge_tool_call_items(&a, &b);
    assert_eq!(m.metadata, json!({"x":1,"y":2}).as_object().cloned());
    assert_eq!(m.extra["futureField"], json!({"kept":true}));
    // neither side has metadata -> absent
    let m = merge_tool_call_items(
        &shell_item(ToolCallStatus::Running, "a", None),
        &shell_item(ToolCallStatus::Running, "b", None),
    );
    assert_eq!(m.metadata, None);
}

// ---- names (tool-name-normalization.test.ts) ----------------------------------------------------------

#[test]
fn paseo_tool_names() {
    assert!(is_paseo_tool_name("mcp__paseo__create_agent"));
    assert!(is_paseo_tool_name("mcp__paseo__list_agents"));
    assert!(is_paseo_tool_name("mcp__paseo_voice__create_agent"));
    assert!(is_paseo_tool_name("paseo_voice.create_agent"));
    assert!(!is_paseo_tool_name("mcp__paseo_voice__speak"));
    assert!(!is_paseo_tool_name("mcp__paseo__speak"));
    assert!(!is_paseo_tool_name("paseo.speak"));
    assert!(is_paseo_tool_name("paseo.create_agent"));
    assert!(!is_paseo_tool_name("Bash"));
    assert!(!is_paseo_tool_name("Read"));
    assert!(!is_paseo_tool_name("mcp__other_server__some_tool"));
    assert_eq!(
        get_paseo_tool_leaf_name("mcp__paseo__create_agent").as_deref(),
        Some("create_agent")
    );
    assert_eq!(
        get_paseo_tool_leaf_name("paseo.create_agent").as_deref(),
        Some("create_agent")
    );
    assert_eq!(
        get_paseo_tool_leaf_name("paseo.list_agents").as_deref(),
        Some("list_agents")
    );
    assert_eq!(get_paseo_tool_leaf_name("Bash"), None);
}

#[test]
fn namespacing_helpers() {
    assert!(is_likely_namespaced_tool_name("a.b"));
    assert!(is_likely_namespaced_tool_name("mcp__srv__do_x"));
    assert!(is_likely_namespaced_tool_name("srv__do_x"));
    assert!(!is_likely_namespaced_tool_name("srv__do"));
    assert!(is_likely_external_tool_name("speak"));
    assert!(!is_likely_external_tool_name(""));
    assert_eq!(
        tokenize_tool_name("Mcp__Paseo-Voice.Speak"),
        ["mcp", "paseo", "voice", "speak"]
    );
}

#[test]
fn cwd_prefix_stripping() {
    assert_eq!(
        strip_cwd_prefix("/tmp/repo/src/index.ts", Some("/tmp/repo")),
        "src/index.ts"
    );
    assert_eq!(strip_cwd_prefix("/tmp/repo", Some("/tmp/repo/")), ".");
    assert_eq!(strip_cwd_prefix("C:\\repo\\a.ts", Some("C:\\repo")), "a.ts");
    assert_eq!(
        strip_cwd_prefix("/other/a.ts", Some("/tmp/repo")),
        "/other/a.ts"
    );
    assert_eq!(strip_cwd_prefix("/a", None), "/a");
}

// ---- display (tool-call-display.test.ts) ------------------------------------------------------------------

fn display(
    name: &str,
    status: ToolCallStatus,
    error: Option<Value>,
    detail: ToolCallDetail,
    cwd: Option<&str>,
) -> ToolCallDisplayModel {
    build_tool_call_display_model(&ToolCallDisplayInput {
        name,
        status,
        error: error.as_ref(),
        metadata: None,
        detail: &detail,
        agent_message: None,
        cwd,
    })
}

fn unk() -> ToolCallDetail {
    ToolCallDetail::unknown(Value::Null, Value::Null)
}

fn dm(name: &str, summary: Option<&str>) -> ToolCallDisplayModel {
    ToolCallDisplayModel {
        display_name: name.into(),
        summary: summary.map(Into::into),
        error_text: None,
    }
}

#[test]
fn display_summaries() {
    use ToolCallStatus::*;
    assert_eq!(
        display(
            "read_file",
            Running,
            Some(Value::Null),
            ToolCallDetail::Read {
                file_path: "/tmp/repo/src/index.ts".into(),
                content: None,
                offset: None,
                limit: None
            },
            Some("/tmp/repo")
        ),
        dm("Read", Some("src/index.ts"))
    );
    assert_eq!(
        display(
            "exec_command",
            Running,
            None,
            ToolCallDetail::unknown(json!({"command":"npm test"}), Value::Null),
            None
        ),
        dm("Exec command", None)
    );
    assert_eq!(
        display(
            "task",
            Running,
            None,
            ToolCallDetail::SubAgent {
                sub_agent_type: Some("Explore".into()),
                description: Some("Inspect repository structure".into()),
                child_session_id: None,
                log: "[Read] README.md".into(),
                actions: None
            },
            None
        ),
        dm("Explore", Some("Inspect repository structure"))
    );
    assert_eq!(
        display(
            "paseo_worktree_setup",
            Running,
            None,
            ToolCallDetail::WorktreeSetup {
                worktree_path: "/tmp".into(),
                branch_name: "feature-branch".into(),
                log: "".into(),
                commands: vec![],
                truncated: None
            },
            None
        ),
        dm("Worktree setup", Some("feature-branch"))
    );
}

#[test]
fn display_error_text_and_terminal() {
    use ToolCallStatus::*;
    let d = display(
        "shell",
        Failed,
        Some(json!({"message":"boom"})),
        unk(),
        None,
    );
    assert_eq!(
        d.error_text.as_deref(),
        Some("{\n  \"message\": \"boom\"\n}")
    );
    let d = display(
        "shell",
        Failed,
        Some(json!({"content":"stderr text"})),
        unk(),
        None,
    );
    assert_eq!(d.error_text.as_deref(), Some("stderr text"));
    let d = display("shell", Completed, Some(json!("ignored")), unk(), None);
    assert_eq!(d.error_text, None);

    let plain = |label: Option<&str>| ToolCallDetail::PlainText {
        label: label.map(Into::into),
        text: None,
        icon: Some(paseo_agent_types::ToolCallIcon::SquareTerminal),
    };
    assert_eq!(
        display("terminal", Completed, None, plain(None), None),
        dm("Terminal", None)
    );
    assert_eq!(
        display(
            "terminal",
            Completed,
            None,
            plain(Some("npm run test")),
            None
        ),
        dm("Terminal", Some("npm run test"))
    );
}

#[test]
fn display_name_humanizing() {
    use ToolCallStatus::*;
    for (name, want) in [
        ("mcp__paseo__create_agent", "Create agent"),
        ("paseo.create_agent", "Create agent"),
        ("mcp__paseo__list_agents", "List agents"),
        ("speak", "Speak"),
        ("some.dotted", "some.dotted"),
        ("a__b", "a__b"),
        ("web-fetch_tool", "Web fetch tool"),
    ] {
        assert_eq!(
            display(name, Running, None, unk(), None).display_name,
            want,
            "{name}"
        );
    }
    assert_eq!(
        display(
            "plan",
            Completed,
            None,
            ToolCallDetail::Plan {
                text: "### Login".into()
            },
            None
        ),
        dm("Plan", None)
    );
}

#[test]
fn display_task_thinking_overrides_and_agent_message() {
    use paseo_agent_types::{AgentMessage, AgentMessageEvent};
    let mut meta = serde_json::Map::new();
    meta.insert("subAgentActivity".into(), json!("Reading files"));
    let d = build_tool_call_display_model(&ToolCallDisplayInput {
        name: "Task",
        status: ToolCallStatus::Running,
        error: None,
        metadata: Some(&meta),
        detail: &unk(),
        agent_message: None,
        cwd: None,
    });
    assert_eq!(d, dm("Task", Some("Reading files")));
    assert_eq!(
        display("thinking", ToolCallStatus::Running, None, unk(), None),
        dm("Thinking", None)
    );
    let msg = AgentMessage {
        event: AgentMessageEvent::PermissionRequired,
        sender_id: "agent-1".into(),
        sender_title: Some("Planner".into()),
        text: "t".into(),
    };
    let d = build_tool_call_display_model(&ToolCallDisplayInput {
        name: "x",
        status: ToolCallStatus::Running,
        error: None,
        metadata: None,
        detail: &unk(),
        agent_message: Some(&msg),
        cwd: None,
    });
    assert_eq!(d, dm("Planner needs permission", None));
}
