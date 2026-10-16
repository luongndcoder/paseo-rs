use serde_json::{json, Value};
use std::path::PathBuf;

use super::*;
use crate::protocol::argv::ToolsOption;
use crate::protocol::mcp::{is_jsonrpc_request, mcp_response_payload};
use crate::protocol::responses::{InitializeResponse, SetPermissionModeResponse};

fn parse(v: Value) -> Line {
    Line::parse(&v.to_string()).unwrap()
}

/// Parsing then serializing must reproduce the input value exactly.
fn assert_roundtrip(v: Value) -> Line {
    let line = parse(v.clone());
    assert_eq!(
        serde_json::to_value(&line).unwrap(),
        v,
        "not lossless: {line:?}"
    );
    line
}

fn fixture_lines() -> Vec<(String, String, Value)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    files.sort();
    let mut out = Vec::new();
    for f in files
        .iter()
        .filter(|f| f.extension().is_some_and(|e| e == "jsonl"))
    {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        for l in std::fs::read_to_string(f).unwrap().lines() {
            let rec: Value = serde_json::from_str(l).unwrap();
            if let (Some(dir), Some(line)) = (rec.get("dir"), rec.get("line")) {
                out.push((
                    name.clone(),
                    dir.as_str().unwrap().to_string(),
                    line.clone(),
                ));
            }
        }
    }
    out
}

// ---------- golden traffic ----------

#[test]
fn golden_fixtures_are_lossless_and_typed() {
    let lines = fixture_lines();
    let mut untyped = Vec::new();
    for (file, dir, v) in &lines {
        let line = Line::from_value(v.clone());
        assert_eq!(
            &serde_json::to_value(&line).unwrap(),
            v,
            "{file} {dir}: not lossless"
        );
        if let Line::Unknown(u) = &line {
            untyped.push(format!(
                "{file} {dir} {}",
                u.get("type").and_then(Value::as_str).unwrap_or("?")
            ));
        }
    }
    // Every observed top-level line type is modelled; Unknown is only for future types.
    assert!(untyped.is_empty(), "untyped fixture lines: {untyped:#?}");
}

#[test]
fn golden_fixtures_payloads_decode() {
    let mut saw_init = 0;
    let mut saw_cwd_free_result = 0;
    for (file, dir, v) in fixture_lines() {
        match Line::from_value(v) {
            Line::ControlResponse(ControlResponse {
                response: ControlResponseBody::Success(s),
            }) if dir == "out" => {
                if s.response
                    .as_ref()
                    .is_some_and(|r| r.get("commands").is_some())
                {
                    let init: InitializeResponse = parse_response(&s).expect(&file);
                    assert!(!init.models.is_empty());
                    saw_init += 1;
                }
            }
            Line::Result(r) => {
                assert!(r.subtype.starts_with("success") || r.subtype.starts_with("error"));
                saw_cwd_free_result += 1;
            }
            _ => {}
        }
    }
    if !fixture_lines().is_empty() {
        assert!(saw_init >= 10, "initialize responses: {saw_init}");
        assert!(saw_cwd_free_result >= 10);
    }
}

#[test]
fn golden_argv_matches_sdk_headers() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let read = |n: &str| std::fs::read_to_string(dir.join(n)).ok();
    let header = |n: &str| -> Option<Vec<String>> {
        let first = read(n)?;
        let v: Value = serde_json::from_str(first.lines().next()?).ok()?;
        v["argv"]
            .as_array()
            .map(|a| a.iter().map(|s| s.as_str().unwrap().to_string()).collect())
    };
    if let Some(argv) = header("02-tool-allow.jsonl") {
        let o = ArgvOptions {
            max_turns: Some(3),
            model: Some("haiku".into()),
            can_use_tool: true,
            setting_sources: Some(vec![]),
            permission_mode: Some("default".into()),
            ..Default::default()
        };
        assert_eq!(build_argv(&o), argv);
    }
    if let Some(argv) = header("04-pretooluse-hook.jsonl") {
        let o = ArgvOptions {
            max_turns: Some(3),
            model: Some("haiku".into()),
            setting_sources: Some(vec![]),
            permission_mode: Some("bypassPermissions".into()),
            allow_dangerously_skip_permissions: true,
            ..Default::default()
        };
        assert_eq!(build_argv(&o), argv);
    }
    if let Some(argv) = header("08-resume.jsonl") {
        let resume = argv
            .iter()
            .find_map(|a| a.strip_prefix("--resume="))
            .unwrap()
            .to_string();
        let o = ArgvOptions {
            max_turns: Some(1),
            model: Some("haiku".into()),
            resume: Some(resume),
            setting_sources: Some(vec![]),
            permission_mode: Some("default".into()),
            ..Default::default()
        };
        assert_eq!(build_argv(&o), argv);
    }
    if let Some(argv) = header("09-partial-messages.jsonl") {
        let o = ArgvOptions {
            max_turns: Some(1),
            model: Some("haiku".into()),
            setting_sources: Some(vec![]),
            permission_mode: Some("default".into()),
            include_partial_messages: true,
            ..Default::default()
        };
        assert_eq!(build_argv(&o), argv);
    }
}

// ---------- argv ----------

#[test]
fn argv_flag_forms() {
    let mut o = ArgvOptions {
        thinking: Some(ThinkingConfig::Enabled {
            budget_tokens: Some(8000),
            display: Some("summarized".into()),
        }),
        effort: Some("xhigh".into()),
        allowed_tools: vec!["Read".into(), "mcp__a__b".into()],
        tools: Some(ToolsOption::List(vec![])),
        fork_session: true,
        resume: Some("abc".into()),
        persist_session: Some(false),
        settings: Some("{\"fastMode\":true}".into()),
        ..Default::default()
    };
    o.extra_args.insert("flag".into(), None);
    o.extra_args.insert("neg".into(), Some("-5".into()));
    let a = build_argv(&o);
    let s = a.join(" ");
    assert!(s.contains("--max-thinking-tokens 8000 --thinking-display summarized"));
    assert!(s.contains("--effort xhigh"));
    assert!(s.contains("--allowedTools Read,mcp__a__b"));
    assert!(a.windows(2).any(|w| w == ["--tools", ""]));
    assert!(a.contains(&"--resume=abc".to_string()));
    assert!(a.contains(&"--fork-session".to_string()));
    assert!(a.contains(&"--no-session-persistence".to_string()));
    assert!(a.contains(&"--flag".to_string()));
    assert!(a.contains(&"--neg=-5".to_string()));
    assert!(a
        .windows(2)
        .any(|w| w == ["--settings", "{\"fastMode\":true}"]));
}

#[test]
fn argv_mcp_config_json() {
    let mut o = ArgvOptions::default();
    o.mcp_servers.insert(
        "paseo".into(),
        serde_json::from_value(json!({"type":"http","url":"http://127.0.0.1:1/mcp"})).unwrap(),
    );
    let a = build_argv(&o);
    let i = a.iter().position(|x| x == "--mcp-config").unwrap();
    let v: Value = serde_json::from_str(&a[i + 1]).unwrap();
    assert_eq!(
        v,
        json!({"mcpServers":{"paseo":{"type":"http","url":"http://127.0.0.1:1/mcp"}}})
    );
}

// ---------- control requests host -> CLI ----------

#[test]
fn initialize_with_hooks_agents_and_sdk_mcp() {
    let v = json!({
        "type":"control_request","request_id":"abc123",
        "request":{
            "subtype":"initialize",
            "hooks":{"PreToolUse":[{"matcher":"Bash","hookCallbackIds":["hook_0","hook_1"],"timeout":5}],
                     "SubagentStop":[{"hookCallbackIds":["hook_2"]}]},
            "sdkMcpServers":["paseo"],
            "systemPrompt":[""],
            "appendSystemPrompt":"be terse",
            "agents":{"reviewer":{"description":"d","prompt":"p","tools":["Read"],"model":"haiku","maxTurns":3}},
            "forwardSubagentText":true,
            "perTaskStopAffordance":true,
            "someFutureKey":{"x":1}
        }
    });
    let line = assert_roundtrip(v);
    let Line::ControlRequest(ControlRequest {
        request: ControlRequestBody::Initialize(init),
        ..
    }) = line
    else {
        panic!("not typed")
    };
    let hooks = init.hooks.unwrap();
    assert_eq!(
        hooks["PreToolUse"][0].hook_callback_ids,
        ["hook_0", "hook_1"]
    );
    assert_eq!(init.sdk_mcp_servers.unwrap(), ["paseo"]);
    assert_eq!(
        init.agents.unwrap()["reviewer"]
            .max_turns
            .as_ref()
            .unwrap()
            .as_u64(),
        Some(3)
    );
    assert!(init.extra.contains_key("someFutureKey"));
}

#[test]
fn simple_controls_roundtrip() {
    let cases = [
        json!({"type":"control_request","request_id":"1","request":{"subtype":"interrupt"}}),
        json!({"type":"control_request","request_id":"1","request":{"subtype":"interrupt","cancel_queued":true}}),
        json!({"type":"control_request","request_id":"2","request":{"subtype":"set_permission_mode","mode":"acceptEdits"}}),
        json!({"type":"control_request","request_id":"3","request":{"subtype":"set_model","model":"sonnet"}}),
        json!({"type":"control_request","request_id":"3","request":{"subtype":"set_model"}}),
        json!({"type":"control_request","request_id":"3","request":{"subtype":"set_model","model":null}}),
        json!({"type":"control_request","request_id":"4","request":{"subtype":"rewind_files","user_message_id":"u","dry_run":true}}),
        json!({"type":"control_request","request_id":"5","request":{"subtype":"mcp_status"}}),
        json!({"type":"control_request","request_id":"6","request":{"subtype":"apply_flag_settings","settings":{"fastMode":true}}}),
        json!({"type":"control_request","request_id":"7","request":{"subtype":"set_max_thinking_tokens","max_thinking_tokens":null}}),
        json!({"type":"control_request","request_id":"8","request":{"subtype":"mcp_toggle","serverName":"x","enabled":false}}),
        json!({"type":"control_request","request_id":"9","request":{"subtype":"mcp_set_servers","servers":{"a":{"type":"sdk","name":"a"},"b":{"command":"x","args":["y"]}}}}),
        json!({"type":"control_cancel_request","request_id":"9"}),
    ];
    for c in cases {
        let line = assert_roundtrip(c.clone());
        let Line::ControlRequest(r) = &line else {
            assert!(matches!(line, Line::ControlCancelRequest(_)));
            continue;
        };
        assert!(
            !matches!(r.request, ControlRequestBody::Unknown(_)),
            "untyped: {c}"
        );
        assert_eq!(r.request.subtype(), c["request"]["subtype"].as_str());
    }
}

#[test]
fn build_requests_from_rust() {
    let line = Line::control_request(
        "r1",
        ControlRequestBody::SetModel {
            model: Some(None),
            extra: Default::default(),
        },
    );
    assert_eq!(line.to_json_line(), "{\"type\":\"control_request\",\"request_id\":\"r1\",\"request\":{\"subtype\":\"set_model\",\"model\":null}}\n");
    let line = Line::control_request("r2", ControlRequestBody::McpStatus {});
    assert_eq!(line.to_json_line(), "{\"type\":\"control_request\",\"request_id\":\"r2\",\"request\":{\"subtype\":\"mcp_status\"}}\n");
    let u = Line::User(UserMessage::text("hi").with_uuid("u-1"));
    let v: Value = serde_json::from_str(u.to_json_line().trim()).unwrap();
    assert_eq!(
        v,
        json!({"type":"user","session_id":"","uuid":"u-1","parent_tool_use_id":null,
               "message":{"role":"user","content":[{"type":"text","text":"hi"}]}})
    );
}

#[test]
fn user_message_with_image_roundtrips() {
    let v = json!({"type":"user","session_id":"","parent_tool_use_id":null,
        "message":{"role":"user","content":[
            {"type":"text","text":"look"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}]}});
    let Line::User(u) = assert_roundtrip(v) else {
        panic!()
    };
    let UserContent::Blocks(b) = u.message.content else {
        panic!()
    };
    assert!(
        matches!(&b[1], ContentBlock::Image(i) if matches!(i.source, ImageSource::Base64 { .. }))
    );
}

// ---------- CLI -> host requests ----------

#[test]
fn can_use_tool_real_sample() {
    let v = json!({"type":"control_request","request_id":"93c698bd-2b95-46bf-9ba7-f1433c10d562",
        "request":{"subtype":"can_use_tool","tool_name":"Write","display_name":"Write",
        "input":{"file_path":"/tmp/zz.txt","content":"x"},"description":"zz.txt",
        "permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}],
        "tool_use_id":"toolu_01GQmDLpGr4pTkdpfBY5RsJJ"}});
    let Line::ControlRequest(ControlRequest {
        request_id,
        request: ControlRequestBody::CanUseTool(c),
    }) = assert_roundtrip(v)
    else {
        panic!()
    };
    assert_eq!(c.tool_name, "Write");
    assert!(matches!(
        c.permission_suggestions.as_ref().unwrap()[0],
        PermissionUpdate::SetMode {
            mode: PermissionMode::AcceptEdits,
            ..
        }
    ));
    // Answer: allow echoes input and tool_use_id.
    let answer = PermissionResult::allow(c.input.clone(), c.tool_use_id.clone());
    let resp = Line::control_success(request_id, Some(serde_json::to_value(answer).unwrap()));
    let v: Value = serde_json::from_str(resp.to_json_line().trim()).unwrap();
    assert_eq!(v["response"]["response"]["behavior"], "allow");
    assert_eq!(
        v["response"]["response"]["toolUseID"],
        "toolu_01GQmDLpGr4pTkdpfBY5RsJJ"
    );
    assert_eq!(v["response"]["response"]["updatedInput"]["content"], "x");
}

#[test]
fn deny_serialization() {
    let v = serde_json::to_value(PermissionResult::deny("nope", true, "t1")).unwrap();
    assert_eq!(
        v,
        json!({"behavior":"deny","message":"nope","interrupt":true,"toolUseID":"t1"})
    );
    let back: PermissionResult = serde_json::from_value(v).unwrap();
    assert!(matches!(
        back,
        PermissionResult::Deny {
            interrupt: Some(true),
            ..
        }
    ));
}

#[test]
fn hook_callback_real_sample() {
    let v = json!({"type":"control_request","request_id":"7d85","request":{
        "subtype":"hook_callback","callback_id":"hook_0",
        "input":{"session_id":"s","transcript_path":"/t.jsonl","cwd":"/w","prompt_id":"p",
                 "permission_mode":"default","effort":{"level":"medium"},
                 "hook_event_name":"PreToolUse","tool_name":"Bash",
                 "tool_input":{"command":"echo hello"},"tool_use_id":"toolu_1"},
        "tool_use_id":"toolu_1"}});
    let Line::ControlRequest(ControlRequest {
        request: ControlRequestBody::HookCallback {
            callback_id, input, ..
        },
        ..
    }) = assert_roundtrip(v)
    else {
        panic!()
    };
    assert_eq!(callback_id, "hook_0");
    let HookInput::PreToolUse {
        base, tool_name, ..
    } = &input
    else {
        panic!("{input:?}")
    };
    assert_eq!(base.effort.as_ref().unwrap()["level"], "medium");
    assert_eq!(tool_name, "Bash");
    assert_eq!(input.base().unwrap().session_id, "s");
}

#[test]
fn hook_inputs_for_subagent_events() {
    for v in [
        json!({"session_id":"s","transcript_path":"t","cwd":"c","hook_event_name":"SubagentStart","agent_id":"a1","agent_type":"Explore"}),
        json!({"session_id":"s","transcript_path":"t","cwd":"c","hook_event_name":"SubagentStop","stop_hook_active":false,"agent_id":"a1","agent_type":"Explore","agent_transcript_path":"p","last_assistant_message":"done"}),
        json!({"session_id":"s","transcript_path":"t","cwd":"c","hook_event_name":"PreCompact","trigger":"auto","custom_instructions":null}),
        json!({"session_id":"s","transcript_path":"t","cwd":"c","hook_event_name":"SomethingNew","x":1}),
    ] {
        let h: HookInput = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(serde_json::to_value(&h).unwrap(), v);
    }
    let h: HookInput = serde_json::from_value(json!({"session_id":"s","transcript_path":"t","cwd":"c","hook_event_name":"SubagentStart","agent_id":"a1","agent_type":"Explore"})).unwrap();
    assert_eq!(h.base().unwrap().agent_id.as_deref(), Some("a1"));
}

#[test]
fn hook_output_noop_is_empty_object() {
    assert_eq!(
        serde_json::to_value(HookJsonOutput::default()).unwrap(),
        json!({})
    );
    let o: HookJsonOutput = serde_json::from_value(json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny"},"continue":false})).unwrap();
    assert_eq!(o.continue_, Some(false));
}

#[test]
fn mcp_message_tunnel() {
    let req = json!({"type":"control_request","request_id":"3e35","request":{"subtype":"mcp_message","server_name":"fixture",
        "message":{"method":"tools/call","params":{"name":"add_numbers","arguments":{"a":17,"b":25}},"jsonrpc":"2.0","id":2}}});
    let Line::ControlRequest(ControlRequest {
        request:
            ControlRequestBody::McpMessage {
                server_name,
                message,
                ..
            },
        ..
    }) = assert_roundtrip(req)
    else {
        panic!()
    };
    assert_eq!(server_name, "fixture");
    assert!(is_jsonrpc_request(&message));
    assert!(!is_jsonrpc_request(
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    ));
    let payload = mcp_response_payload(
        json!({"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"42"}]}}),
    );
    assert_eq!(payload["mcp_response"]["id"], 2);
}

// ---------- responses ----------

#[test]
fn control_responses() {
    let ok = assert_roundtrip(
        json!({"type":"control_response","response":{"subtype":"success","request_id":"r","response":{"mode":"plan"}}}),
    );
    let Line::ControlResponse(r) = ok else {
        panic!()
    };
    assert_eq!(r.request_id(), Some("r"));
    let ControlResponseBody::Success(s) = r.response else {
        panic!()
    };
    assert_eq!(
        parse_response::<SetPermissionModeResponse>(&s)
            .unwrap()
            .mode,
        PermissionMode::Plan
    );

    let err = assert_roundtrip(
        json!({"type":"control_response","response":{"subtype":"error","request_id":"r4","error":"Unsupported control request subtype: bogus_x"}}),
    );
    assert!(matches!(
        err,
        Line::ControlResponse(ControlResponse {
            response: ControlResponseBody::Error(_)
        })
    ));

    // no `response` key (set_model)
    assert_roundtrip(
        json!({"type":"control_response","response":{"subtype":"success","request_id":"r"}}),
    );
    // initialize carries pending_* beside `response`
    assert_roundtrip(
        json!({"type":"control_response","response":{"subtype":"success","request_id":"r","response":{},"pending_permission_requests":[],"pending_user_dialog_requests":[]}}),
    );
}

#[test]
fn rewind_and_interrupt_payloads() {
    let s = json!({"canRewind":false,"error":"No file checkpoint found for this message."});
    let r: RewindFilesResult = serde_json::from_value(s.clone()).unwrap();
    assert!(!r.can_rewind);
    assert_eq!(serde_json::to_value(&r).unwrap(), s);
    let i: InterruptResponse = serde_json::from_value(json!({"still_queued":[]})).unwrap();
    assert!(i.still_queued.is_empty());
}

// ---------- messages ----------

#[test]
fn assistant_and_stream_events() {
    let a = assert_roundtrip(
        json!({"type":"assistant","message":{"model":"claude-haiku-5-5","id":"msg_1","type":"message","role":"assistant",
        "content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}},{"type":"thinking","thinking":"","signature":"sig"}],
        "container":null,"stop_reason":null,"usage":{"input_tokens":1}},
        "parent_tool_use_id":null,"session_id":"s","uuid":"u"}),
    );
    let Line::Assistant(m) = a else { panic!() };
    assert_eq!(m.message.content.len(), 3);
    assert_eq!(m.parent_tool_use_id(), None);
    assert_eq!(m.message.stop_reason, Some(None));

    let e = assert_roundtrip(
        json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}},
        "session_id":"s","parent_tool_use_id":"toolu_p","uuid":"u","api_message_id":"msg_1"}),
    );
    let Line::StreamEvent(ev) = e else { panic!() };
    assert_eq!(ev.parent_tool_use_id(), Some("toolu_p"));
    assert!(
        matches!(ev.kind(), StreamEventKind::ContentBlockDelta { delta: Delta::Text { ref text, .. }, .. } if text == "hi")
    );
    let ev2 = StreamEvent {
        event: json!({"type":"brand_new"}),
        ..ev
    };
    assert!(matches!(ev2.kind(), StreamEventKind::Unknown(_)));
}

#[test]
fn result_variants() {
    let ok = assert_roundtrip(
        json!({"type":"result","subtype":"success","is_error":false,"duration_ms":1329,"duration_api_ms":1580,"num_turns":1,
        "result":"hi","stop_reason":"end_turn","total_cost_usd":0,"usage":{},"modelUsage":{},"permission_denials":[],"terminal_reason":"completed",
        "uuid":"u","session_id":"s","api_error_status":null}),
    );
    let Line::Result(r) = ok else { panic!() };
    assert_eq!(r.result.as_deref(), Some("hi"));
    assert_eq!(r.terminal_reason.as_deref(), Some("completed"));
    let err = assert_roundtrip(
        json!({"type":"result","subtype":"error_during_execution","is_error":true,"errors":["boom"],"duration_ms":1,"duration_api_ms":1,"num_turns":0,"total_cost_usd":0.5,"uuid":"u","session_id":"s"}),
    );
    assert!(err.is_result());
}

#[test]
fn system_messages() {
    let init = assert_roundtrip(
        json!({"type":"system","subtype":"init","cwd":"/w","session_id":"s","tools":["Bash"],"mcp_servers":[{"name":"a","status":"pending","source":"user"}],
        "model":"claude-haiku-5-5","permissionMode":"default","slash_commands":["x"],"apiKeySource":"none","claude_code_version":"2.1.301","output_style":"default",
        "agents":["Explore"],"skills":[],"plugins":[],"uuid":"u","analytics_disabled":false}),
    );
    let Line::System(SystemMessage::Init(i)) = init else {
        panic!("untyped init")
    };
    assert_eq!(i.permission_mode, PermissionMode::Default);
    assert!(i.extra.contains_key("analytics_disabled"));

    for v in [
        json!({"type":"system","subtype":"status","status":null,"permissionMode":"plan","uuid":"u","session_id":"s"}),
        json!({"type":"system","subtype":"status","status":"requesting","uuid":"u","session_id":"s"}),
        json!({"type":"system","subtype":"session_state_changed","state":"idle","uuid":"u","session_id":"s"}),
        json!({"type":"system","subtype":"thinking_tokens","estimated_tokens":50,"estimated_tokens_delta":50,"uuid":"u","session_id":"s"}),
        json!({"type":"system","subtype":"hook_started","hook_id":"h","hook_name":"SessionStart:startup","hook_event":"SessionStart","uuid":"u","session_id":"s"}),
        json!({"type":"system","subtype":"task_notification","task_id":"t","status":"completed","output_file":"/o","summary":"s","uuid":"u","session_id":"s"}),
        json!({"type":"system","subtype":"api_retry","attempt":1,"max_retries":3,"retry_delay_ms":500,"error_status":null,"error":"overloaded","uuid":"u","session_id":"s"}),
    ] {
        let l = assert_roundtrip(v.clone());
        assert!(
            matches!(l, Line::System(ref s) if !matches!(s, SystemMessage::Unknown(_))),
            "untyped {v}"
        );
    }
    // unknown system subtype is kept verbatim
    let l = assert_roundtrip(
        json!({"type":"system","subtype":"informational","content":"x","level":"info","uuid":"u","session_id":"s"}),
    );
    assert!(matches!(l, Line::System(SystemMessage::Unknown(_))));
}

#[test]
fn command_lifecycle_and_misc() {
    let l = assert_roundtrip(
        json!({"type":"command_lifecycle","command_uuid":"c","state":"queued","uuid":"u","session_id":"s"}),
    );
    assert!(matches!(l, Line::CommandLifecycle(_)));
    assert_roundtrip(
        json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{}},"uuid":"u","session_id":"s"}),
    );
    assert_roundtrip(
        json!({"type":"tool_progress","tool_use_id":"t","tool_name":"Bash","parent_tool_use_id":null,"elapsed_time_seconds":2.5,"uuid":"u","session_id":"s"}),
    );
    assert_roundtrip(json!({"type":"keep_alive"}));
    assert_roundtrip(json!({"type":"transcript_mirror","filePath":"/p.jsonl","entries":[{"a":1}]}));
}

// ---------- unknowns never error ----------

#[test]
fn unknown_everything_is_preserved() {
    for v in [
        json!({"type":"future_type","a":[1,2,{"b":null}]}),
        json!({"no_type":true}),
        json!({"type":"control_request","request_id":"1","request":{"subtype":"brand_new","x":1}}),
        json!({"type":"control_request","request_id":"1","request":{"nosubtype":1}}),
        json!({"type":"control_response","response":{"subtype":"weird","request_id":"1"}}),
        json!({"type":"system","subtype":"brand_new","uuid":"u"}),
        json!({"type":"assistant","message":"not an object"}),
        json!({"type":"result"}),
        json!([1, 2, 3]),
        json!("a string"),
        json!(null),
    ] {
        let l = assert_roundtrip(v.clone());
        let _ = l.to_json_line();
    }
    let l = parse(
        json!({"type":"control_request","request_id":"1","request":{"subtype":"brand_new","x":1}}),
    );
    let Line::ControlRequest(r) = l else { panic!() };
    assert_eq!(r.request.subtype(), Some("brand_new"));
}

#[test]
fn null_vs_absent_and_int_vs_float_stay_exact() {
    // Missing nullable field, explicit null, integer-valued float: all reserialize exactly.
    for v in [
        json!({"type":"result","subtype":"success","total_cost_usd":1.0,"num_turns":1}),
        json!({"type":"result","subtype":"success","stop_reason":null}),
        json!({"type":"result","subtype":"success"}),
        json!({"type":"user","message":{"role":"user","content":"plain"}}),
        json!({"type":"user","message":{"role":"user","content":"plain"},"parent_tool_use_id":null}),
    ] {
        assert_roundtrip(v);
    }
}

#[test]
fn non_json_line_is_an_error_not_a_panic() {
    assert!(Line::parse("not json").is_err());
    assert!(Line::parse("").is_err());
}

#[test]
fn permission_enums_have_fallbacks() {
    let m: PermissionMode = serde_json::from_value(json!("somethingNew")).unwrap();
    assert_eq!(m, PermissionMode::Other("somethingNew".into()));
    assert_eq!(serde_json::to_value(&m).unwrap(), json!("somethingNew"));
    assert_eq!(
        serde_json::to_value(PermissionMode::BypassPermissions).unwrap(),
        json!("bypassPermissions")
    );
    let u: PermissionUpdate = serde_json::from_value(json!({"type":"mystery","a":1})).unwrap();
    assert!(matches!(u, PermissionUpdate::Unknown(_)));
}

#[test]
fn mcp_configs() {
    for v in [
        json!({"type":"stdio","command":"x","args":["a"],"env":{"K":"V"}}),
        json!({"command":"x"}),
        json!({"type":"http","url":"http://h","headers":{"Authorization":"Bearer t"}}),
        json!({"type":"sse","url":"http://h"}),
        json!({"type":"sdk","name":"paseo"}),
        json!({"type":"claudeai-proxy","url":"u","id":"i"}),
    ] {
        let c: McpServerConfig = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(serde_json::to_value(&c).unwrap(), v);
    }
    let c: McpServerConfig = serde_json::from_value(json!({"command":"x"})).unwrap();
    assert!(matches!(c, McpServerConfig::ImplicitStdio(_)));
    let st: McpServerStatus = serde_json::from_value(
        json!({"name":"a","status":"needs-auth","scope":"user","source":"user"}),
    )
    .unwrap();
    assert_eq!(st.status, mcp::McpConnectionStatus::NeedsAuth);
}
