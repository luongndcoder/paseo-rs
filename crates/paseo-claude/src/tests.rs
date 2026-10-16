//! Session driver tests over [`InMemoryTransport`].

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::callbacks::{McpCallContext, McpTool, McpToolResult};
use crate::protocol::{CanUseToolRequest, HookEvent, HookInput, HookJsonOutput, PermissionMode};
use crate::*;

fn opts() -> ClaudeOptions {
    ClaudeOptions::new().control_timeout(Duration::from_secs(5))
}

async fn start(options: ClaudeOptions) -> (Session, InMemoryPeer, String) {
    let (t, mut peer) = InMemoryTransport::new();
    let session = Session::start_with_transport(options, Box::new(t)).expect("start");
    let init = peer.recv_json().await.expect("initialize line");
    assert_eq!(init["type"], "control_request");
    assert_eq!(init["request"]["subtype"], "initialize");
    let id = init["request_id"].as_str().expect("request_id").to_string();
    (session, peer, id)
}

fn ok_response(id: &str, payload: Option<Value>) -> Value {
    let mut r = json!({"subtype": "success", "request_id": id});
    if let Some(p) = payload {
        r["response"] = p;
    }
    json!({"type": "control_response", "response": r})
}

fn cli_request(id: &str, request: Value) -> Value {
    json!({"type": "control_request", "request_id": id, "request": request})
}

async fn next_written(peer: &mut InMemoryPeer) -> Value {
    tokio::time::timeout(Duration::from_secs(5), peer.recv_json())
        .await
        .expect("timed out waiting for a written line")
        .expect("stdin closed")
}

#[tokio::test]
async fn initialize_is_first_then_user_message_with_uuid() {
    let (session, mut peer, init_id) = start(opts().append_system_prompt("be brief")).await;
    peer.send_json(&ok_response(
        &init_id,
        Some(json!({"commands": [{"name": "compact", "description": "d"}], "models": [{"value": "haiku"}]})),
    ));
    let uuid = session
        .send_user_message(UserInput::text("hello").push_image_base64("image/png", "AAAA"))
        .expect("send");
    let line = next_written(&mut peer).await;
    assert_eq!(line["type"], "user");
    assert_eq!(line["session_id"], "");
    assert!(line["parent_tool_use_id"].is_null());
    assert_eq!(line["uuid"], uuid);
    assert_eq!(line["message"]["content"][0]["text"], "hello");
    assert_eq!(
        line["message"]["content"][1]["source"]["media_type"],
        "image/png"
    );
    assert_eq!(
        session.supported_commands().await.expect("cmds")[0].name,
        "compact"
    );
    assert_eq!(
        session.supported_models().await.expect("models")[0].value,
        "haiku"
    );
}

#[tokio::test]
async fn initialize_carries_prompt_agents_and_hook_ids() {
    struct Noop;
    #[async_trait]
    impl HookCallback for Noop {
        async fn call(
            &self,
            _: HookInput,
            _: Option<String>,
            _: CancellationToken,
        ) -> Result<HookJsonOutput, BoxError> {
            Ok(HookJsonOutput::default())
        }
    }
    let options = opts()
        .hook(HookEvent::PreToolUse, Some("Bash"), Some(5), Arc::new(Noop))
        .hook(HookEvent::PostToolUse, None, None, Arc::new(Noop))
        .append_system_prompt("x")
        .system_prompt(SystemPrompt::ClaudeCodePreset);
    let (_s, _peer, _) = start(options).await;
    let p = opts()
        .hook(HookEvent::PreToolUse, Some("Bash"), Some(5), Arc::new(Noop))
        .hook(HookEvent::PostToolUse, None, None, Arc::new(Noop))
        .append_system_prompt("x")
        .system_prompt(SystemPrompt::ClaudeCodePreset)
        .prepare()
        .expect("prepare");
    let v = serde_json::to_value(&p.init).expect("json");
    assert_eq!(v["hooks"]["PreToolUse"][0]["hookCallbackIds"][0], "hook_0");
    assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "Bash");
    assert_eq!(v["hooks"]["PreToolUse"][0]["timeout"], 5);
    assert_eq!(v["hooks"]["PostToolUse"][0]["hookCallbackIds"][0], "hook_1");
    assert!(v.get("systemPrompt").is_none());
    assert_eq!(v["appendSystemPrompt"], "x");
    let empty = serde_json::to_value(&opts().prepare().expect("prepare").init).expect("json");
    assert_eq!(empty["systemPrompt"], json!([""]));
}

#[tokio::test]
async fn control_requests_correlate_out_of_order() {
    let (session, mut peer, init_id) = start(opts()).await;
    peer.send_json(&ok_response(&init_id, Some(json!({}))));
    let h1 = session.handle();
    let h2 = session.handle();
    let a = tokio::spawn(async move { h1.set_permission_mode(PermissionMode::Plan).await });
    let first = next_written(&mut peer).await;
    let b = tokio::spawn(async move { h2.interrupt().await });
    let second = next_written(&mut peer).await;
    assert_eq!(first["request"]["subtype"], "set_permission_mode");
    assert_eq!(first["request"]["mode"], "plan");
    assert_eq!(second["request"]["subtype"], "interrupt");
    // Answer in reverse order.
    peer.send_json(&ok_response(
        second["request_id"].as_str().expect("id"),
        Some(json!({"still_queued": ["u1"]})),
    ));
    peer.send_json(&ok_response(
        first["request_id"].as_str().expect("id"),
        Some(json!({"mode": "plan"})),
    ));
    let interrupt = b.await.expect("join").expect("interrupt");
    assert_eq!(interrupt.still_queued, vec!["u1".to_string()]);
    assert_eq!(a.await.expect("join").expect("mode"), PermissionMode::Plan);
}

#[tokio::test]
async fn set_model_none_sends_null_and_rewind_parses() {
    let (session, mut peer, init_id) = start(opts()).await;
    peer.send_json(&ok_response(&init_id, None));
    let h = session.handle();
    let t = tokio::spawn(async move { h.set_model(None).await });
    let line = next_written(&mut peer).await;
    assert_eq!(line["request"]["subtype"], "set_model");
    assert!(line["request"]["model"].is_null());
    peer.send_json(&ok_response(line["request_id"].as_str().expect("id"), None));
    t.await.expect("join").expect("set_model");

    let h = session.handle();
    let t = tokio::spawn(async move { h.rewind_files("u-1", true).await });
    let line = next_written(&mut peer).await;
    assert_eq!(line["request"]["user_message_id"], "u-1");
    assert_eq!(line["request"]["dry_run"], true);
    peer.send_json(&ok_response(
        line["request_id"].as_str().expect("id"),
        Some(
            json!({"canRewind": true, "filesChanged": ["a.txt"], "insertions": 1, "deletions": 0}),
        ),
    ));
    let r = t.await.expect("join").expect("rewind");
    assert!(r.can_rewind);
    assert_eq!(r.files_changed, Some(vec!["a.txt".to_string()]));
}

#[tokio::test]
async fn error_response_and_timeout() {
    let (session, mut peer, init_id) =
        start(opts().control_timeout(Duration::from_millis(150))).await;
    peer.send_json(&ok_response(&init_id, None));
    let h = session.handle();
    let t = tokio::spawn(async move { h.mcp_server_status().await });
    let line = next_written(&mut peer).await;
    peer.send_json(&json!({"type": "control_response", "response": {
        "subtype": "error", "request_id": line["request_id"], "error": "boom"}}));
    match t.await.expect("join") {
        Err(Error::Control { subtype, message }) => {
            assert_eq!(subtype, "mcp_status");
            assert_eq!(message, "boom");
        }
        other => panic!("unexpected: {other:?}"),
    }
    // No answer: times out and tells the CLI to cancel.
    let h = session.handle();
    let t = tokio::spawn(async move { h.interrupt().await });
    let req = next_written(&mut peer).await;
    let cancel = next_written(&mut peer).await;
    assert_eq!(cancel["type"], "control_cancel_request");
    assert_eq!(cancel["request_id"], req["request_id"]);
    assert!(matches!(t.await.expect("join"), Err(Error::Timeout { .. })));
}

struct Perm {
    seen: Mutex<Vec<String>>,
}

#[async_trait]
impl CanUseTool for Perm {
    async fn can_use_tool(
        &self,
        req: CanUseToolRequest,
        _cancel: CancellationToken,
    ) -> Result<PermissionDecision, BoxError> {
        self.seen.lock().expect("lock").push(req.tool_name.clone());
        match req.tool_name.as_str() {
            "Write" => Ok(PermissionDecision::allow()),
            "Bash" => Ok(PermissionDecision::deny_and_interrupt("no shell")),
            "Edit" => Ok(PermissionDecision::allow_with_input(
                json!({"changed": true}),
            )),
            _ => Err("unexpected tool".into()),
        }
    }
}

#[tokio::test]
async fn can_use_tool_allow_deny_and_error() {
    let perm = Arc::new(Perm {
        seen: Mutex::new(Vec::new()),
    });
    let (_s, mut peer, init_id) = start(opts().can_use_tool(perm.clone())).await;
    peer.send_json(&ok_response(&init_id, None));
    for (id, tool) in [
        ("c1", "Write"),
        ("c2", "Bash"),
        ("c3", "Edit"),
        ("c4", "Other"),
    ] {
        peer.send_json(&cli_request(
            id,
            json!({"subtype": "can_use_tool", "tool_name": tool, "input": {"path": "a"}, "tool_use_id": format!("tu_{id}")}),
        ));
    }
    let mut by_id = std::collections::HashMap::new();
    for _ in 0..4 {
        let l = next_written(&mut peer).await;
        by_id.insert(
            l["response"]["request_id"]
                .as_str()
                .expect("id")
                .to_string(),
            l,
        );
    }
    let allow = &by_id["c1"]["response"];
    assert_eq!(allow["subtype"], "success");
    assert_eq!(allow["response"]["behavior"], "allow");
    assert_eq!(allow["response"]["updatedInput"], json!({"path": "a"}));
    assert_eq!(allow["response"]["toolUseID"], "tu_c1");
    let deny = &by_id["c2"]["response"]["response"];
    assert_eq!(deny["behavior"], "deny");
    assert_eq!(deny["message"], "no shell");
    assert_eq!(deny["interrupt"], true);
    assert_eq!(
        by_id["c3"]["response"]["response"]["updatedInput"],
        json!({"changed": true})
    );
    assert_eq!(by_id["c4"]["response"]["subtype"], "error");
    assert_eq!(by_id["c4"]["response"]["error"], "unexpected tool");
}

#[tokio::test]
async fn missing_callbacks_and_unknown_subtypes_answer_with_errors() {
    let (_s, mut peer, init_id) = start(opts()).await;
    peer.send_json(&ok_response(&init_id, None));
    peer.send_json(&cli_request(
        "a",
        json!({"subtype": "can_use_tool", "tool_name": "X", "input": {}, "tool_use_id": "t"}),
    ));
    peer.send_json(&cli_request("b", json!({"subtype": "mystery_op"})));
    peer.send_json(&cli_request(
        "c",
        json!({"subtype": "hook_callback", "callback_id": "hook_9", "input": {"hook_event_name": "Stop"}}),
    ));
    peer.send_json(&cli_request(
        "d",
        json!({"subtype": "mcp_message", "server_name": "nope", "message": {"jsonrpc": "2.0", "id": 1, "method": "ping"}}),
    ));
    peer.send_json(&cli_request(
        "e",
        json!({"subtype": "elicitation", "mcp_server_name": "s", "message": "m"}),
    ));
    let mut got = std::collections::HashMap::new();
    for _ in 0..5 {
        let l = next_written(&mut peer).await;
        got.insert(
            l["response"]["request_id"]
                .as_str()
                .expect("id")
                .to_string(),
            l["response"].clone(),
        );
    }
    assert_eq!(got["a"]["error"], "canUseTool callback is not provided.");
    assert_eq!(
        got["b"]["error"],
        "Unsupported control request subtype: mystery_op"
    );
    assert_eq!(got["c"]["error"], "No hook callback found for ID: hook_9");
    assert_eq!(got["d"]["error"], "SDK MCP server not found: nope");
    assert_eq!(got["e"]["response"], json!({"action": "decline"}));
}

#[tokio::test]
async fn hook_callback_runs_registered_callback() {
    struct Deny;
    #[async_trait]
    impl HookCallback for Deny {
        async fn call(
            &self,
            input: HookInput,
            tool_use_id: Option<String>,
            _: CancellationToken,
        ) -> Result<HookJsonOutput, BoxError> {
            let HookInput::PreToolUse { tool_name, .. } = input else {
                return Err("wrong event".into());
            };
            Ok(HookJsonOutput {
                hook_specific_output: Some(json!({
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": format!("{tool_name}/{}", tool_use_id.unwrap_or_default()),
                })),
                ..HookJsonOutput::default()
            })
        }
    }
    let (_s, mut peer, init_id) =
        start(opts().hook(HookEvent::PreToolUse, Some("Bash"), None, Arc::new(Deny))).await;
    peer.send_json(&ok_response(&init_id, None));
    peer.send_json(&cli_request(
        "h1",
        json!({"subtype": "hook_callback", "callback_id": "hook_0", "tool_use_id": "tu1", "input": {
            "hook_event_name": "PreToolUse", "session_id": "s", "transcript_path": "/t", "cwd": "/c",
            "tool_name": "Bash", "tool_input": {"command": "ls"}, "tool_use_id": "tu1"}}),
    ));
    let l = next_written(&mut peer).await;
    assert_eq!(l["response"]["subtype"], "success");
    assert_eq!(
        l["response"]["response"]["hookSpecificOutput"]["permissionDecisionReason"],
        "Bash/tu1"
    );
}

struct Calc;

#[async_trait]
impl SdkMcpServer for Calc {
    fn name(&self) -> &str {
        "calc"
    }

    async fn list_tools(&self) -> Result<Vec<McpTool>, BoxError> {
        Ok(vec![
            McpTool::new("add", json!({"type": "object"})).description("adds")
        ])
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        ctx: McpCallContext,
    ) -> Result<McpToolResult, BoxError> {
        match name {
            "add" => {
                let a = arguments["a"].as_i64().unwrap_or(0);
                let b = arguments["b"].as_i64().unwrap_or(0);
                Ok(McpToolResult::text(format!(
                    "{} (via {:?})",
                    a + b,
                    ctx.tool_use_id
                )))
            }
            _ => Err("no such tool".into()),
        }
    }
}

#[tokio::test]
async fn sdk_mcp_server_tunnel() {
    let options = opts().sdk_mcp_server("calc", Arc::new(Calc));
    let prepared = opts()
        .sdk_mcp_server("calc", Arc::new(Calc))
        .prepare()
        .expect("prepare");
    assert_eq!(
        prepared.init.sdk_mcp_servers,
        Some(vec!["calc".to_string()])
    );
    assert!(!prepared.spawn.args.iter().any(|a| a == "--mcp-config"));

    let (_s, mut peer, init_id) = start(options).await;
    peer.send_json(&ok_response(&init_id, None));
    let mcp = |id: &str, msg: Value| {
        cli_request(
            id,
            json!({"subtype": "mcp_message", "server_name": "calc", "message": msg}),
        )
    };
    peer.send_json(&mcp("m1", json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25"}})));
    peer.send_json(&mcp(
        "m2",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    ));
    peer.send_json(&mcp(
        "m3",
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    ));
    peer.send_json(&mcp("m4", json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"add","arguments":{"a":2,"b":3},"_meta":{"claudecode/toolUseId":"tu9"}}})));
    peer.send_json(&mcp(
        "m5",
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"zzz"}}),
    ));
    peer.send_json(&mcp(
        "m6",
        json!({"jsonrpc":"2.0","id":4,"method":"resources/list"}),
    ));
    let mut got = std::collections::HashMap::new();
    for _ in 0..6 {
        let l = next_written(&mut peer).await;
        got.insert(
            l["response"]["request_id"]
                .as_str()
                .expect("id")
                .to_string(),
            l["response"]["response"]["mcp_response"].clone(),
        );
    }
    assert_eq!(got["m1"]["id"], 0);
    assert_eq!(got["m1"]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(got["m1"]["result"]["serverInfo"]["name"], "calc");
    assert_eq!(got["m2"], json!({"jsonrpc":"2.0","result":{},"id":0}));
    assert_eq!(got["m3"]["id"], 1);
    assert_eq!(got["m3"]["result"]["tools"][0]["name"], "add");
    assert_eq!(got["m3"]["result"]["tools"][0]["description"], "adds");
    assert_eq!(
        got["m4"]["result"]["content"][0]["text"],
        "5 (via Some(\"tu9\"))"
    );
    assert!(got["m4"]["result"].get("isError").is_none());
    assert_eq!(got["m5"]["result"]["isError"], true);
    assert_eq!(got["m6"]["error"]["code"], -32601);
}

#[tokio::test]
async fn cancel_request_drops_handler_and_response() {
    struct Slow {
        cancelled: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl CanUseTool for Slow {
        async fn can_use_tool(
            &self,
            _: CanUseToolRequest,
            cancel: CancellationToken,
        ) -> Result<PermissionDecision, BoxError> {
            cancel.cancelled().await;
            *self.cancelled.lock().expect("lock") = true;
            Ok(PermissionDecision::allow())
        }
    }
    let flag = Arc::new(Mutex::new(false));
    let (session, mut peer, init_id) = start(opts().can_use_tool(Arc::new(Slow {
        cancelled: flag.clone(),
    })))
    .await;
    peer.send_json(&ok_response(&init_id, None));
    peer.send_json(&cli_request(
        "p1",
        json!({"subtype": "can_use_tool", "tool_name": "X", "input": {}, "tool_use_id": "t"}),
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    peer.send_json(&json!({"type": "control_cancel_request", "request_id": "p1"}));
    // A following request must be the next thing written: no response for p1.
    let h = session.handle();
    let t = tokio::spawn(async move { h.set_model(Some("haiku")).await });
    let l = next_written(&mut peer).await;
    assert_eq!(l["request"]["subtype"], "set_model");
    assert_eq!(l["request"]["model"], "haiku");
    peer.send_json(&ok_response(l["request_id"].as_str().expect("id"), None));
    t.await.expect("join").expect("set_model");
}

#[tokio::test]
async fn stream_skips_control_and_garbage_and_reports_abnormal_exit() {
    let (mut session, peer, init_id) = start(opts()).await;
    peer.send_json(&ok_response(&init_id, None));
    peer.send("not json at all");
    peer.send("");
    peer.send_json(&json!({"type": "keep_alive"}));
    peer.send_json(&json!({"type": "system", "subtype": "init", "session_id": "s1"}));
    peer.send_json(&json!({"type": "future_thing", "x": 1}));
    peer.send_json(&json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "result": "bad", "session_id": "s1"}));
    peer.exit(ExitInfo {
        code: Some(1),
        signal: None,
        stderr_tail: "oops".into(),
    });

    assert!(matches!(
        session.next_message().await,
        Some(Ok(SdkMessage::System(_)))
    ));
    assert!(
        matches!(session.next_message().await, Some(Ok(SdkMessage::Other(v))) if v["type"] == "future_thing")
    );
    assert!(matches!(
        session.next_message().await,
        Some(Ok(SdkMessage::Result(_)))
    ));
    match session.next_message().await {
        Some(Err(Error::ErrorResult(t))) => assert_eq!(t, "bad"),
        other => panic!("unexpected: {other:?}"),
    }
    assert!(session.next_message().await.is_none());
    assert!(matches!(session.send_user_message("x"), Err(Error::Closed)));
}

#[tokio::test]
async fn nonzero_exit_without_result_carries_stderr() {
    let (mut session, peer, _) = start(opts()).await;
    peer.exit(ExitInfo {
        code: Some(2),
        signal: None,
        stderr_tail: "no auth".into(),
    });
    match session.next_message().await {
        Some(Err(Error::ProcessExited { status, stderr })) => {
            assert_eq!(status, "code 2");
            assert_eq!(stderr, "no auth");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn pending_requests_fail_when_process_exits() {
    let (session, peer, _) = start(opts()).await;
    let h = session.handle();
    let t = tokio::spawn(async move { h.interrupt().await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    peer.exit(ExitInfo {
        code: Some(0),
        ..ExitInfo::default()
    });
    assert!(matches!(t.await.expect("join"), Err(Error::Closed)));
    assert!(matches!(
        session.supported_commands().await,
        Err(Error::Protocol(_))
    ));
}

#[tokio::test]
async fn close_closes_stdin_then_kills_and_suppresses_error() {
    let (mut session, mut peer, _) = start(opts()).await;
    let h = session.handle();
    let closer = tokio::spawn(async move { h.close().await });
    // Drain: stdin must reach EOF.
    while peer.recv().await.is_some() {}
    // Peer ignores EOF; the 2 s grace expires and the kill fires.
    closer.await.expect("join");
    assert!(peer.was_killed());
    assert!(session.next_message().await.is_none());
}

#[tokio::test]
async fn dropping_last_handle_kills() {
    let (session, peer, _) = start(opts()).await;
    drop(session);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(peer.was_killed());
}

#[tokio::test]
async fn host_mcp_notification_is_fire_and_forget() {
    let (session, mut peer, _) = start(opts().sdk_mcp_server("calc", Arc::new(Calc))).await;
    session.notify_mcp_tools_changed("calc").expect("notify");
    let l = next_written(&mut peer).await;
    assert_eq!(l["request"]["subtype"], "mcp_message");
    assert_eq!(
        l["request"]["message"]["method"],
        "notifications/tools/list_changed"
    );
    // A reply with an unknown id is ignored.
    peer.send_json(&ok_response(l["request_id"].as_str().expect("id"), None));
}

#[test]
fn prepare_builds_argv_env_and_validates() {
    let p = ClaudeOptions::new()
        .executable("/bin/claude")
        .cwd("/work")
        .model("haiku")
        .resume("sess-1")
        .fork_session(true)
        .permission_mode(PermissionMode::Plan)
        .setting_sources(["user", "project"])
        .include_partial_messages(true)
        .enable_file_checkpointing(true)
        .emit_session_state_events(true)
        .env("FOO", "bar")
        .env_remove("BAZ")
        .append_args(["--extra"])
        .prepare()
        .expect("prepare");
    let a = &p.spawn.args;
    for want in [
        "--model",
        "haiku",
        "--resume=sess-1",
        "--fork-session",
        "--permission-mode",
        "plan",
        "--setting-sources=user,project",
        "--include-partial-messages",
    ] {
        assert!(a.iter().any(|x| x == want), "missing {want} in {a:?}");
    }
    assert_eq!(
        &a[..5],
        [
            "--output-format",
            "stream-json",
            "--verbose",
            "--input-format",
            "stream-json"
        ]
    );
    assert_eq!(p.spawn.args_suffix, vec!["--extra".to_string()]);
    assert_eq!(p.spawn.command, "/bin/claude");
    let env = &p.spawn.env;
    assert_eq!(env["FOO"].as_deref(), Some("bar"));
    assert_eq!(env["BAZ"], None);
    assert_eq!(env["NODE_OPTIONS"], None);
    assert_eq!(
        env["CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING"].as_deref(),
        Some("true")
    );
    assert_eq!(
        env["CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS"].as_deref(),
        Some("1")
    );
    assert_eq!(
        env["CLAUDE_AGENT_SDK_VERSION"].as_deref(),
        Some(SDK_VERSION)
    );

    assert!(matches!(
        ClaudeOptions::new()
            .model("a")
            .fallback_model("a")
            .prepare(),
        Err(Error::InvalidOptions(_))
    ));
    let with_perm = Arc::new(Perm {
        seen: Mutex::new(Vec::new()),
    });
    let mut o = ClaudeOptions::new().can_use_tool(with_perm);
    o.argv.permission_prompt_tool_name = Some("x".into());
    assert!(matches!(o.prepare(), Err(Error::InvalidOptions(_))));
    let p = ClaudeOptions::new()
        .can_use_tool(Arc::new(Perm {
            seen: Mutex::new(Vec::new()),
        }))
        .command_argv(["npx", "claude-wrapper"])
        .prepare()
        .expect("prepare");
    assert!(p
        .spawn
        .args
        .windows(2)
        .any(|w| w == ["--permission-prompt-tool", "stdio"]));
    assert_eq!(p.spawn.command, "npx");
    assert_eq!(p.spawn.args_prefix, vec!["claude-wrapper".to_string()]);
}

/// Replays a captured CLI session (golden fixture) through the driver.
#[tokio::test]
async fn replays_golden_fixture_01() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/fixtures/01-single-prompt.jsonl"
    );
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let (mut session, peer, init_id) = start(opts()).await;
    let mut lines = text
        .lines()
        .skip(1)
        .filter_map(|l| serde_json::from_str::<Value>(l).ok());
    let mut expected_msgs = 0;
    let mut saw_result = false;
    for rec in lines.by_ref() {
        if rec["dir"] != "out" {
            continue;
        }
        let mut line = rec["line"].clone();
        match line["type"].as_str() {
            Some("control_response") => line["response"]["request_id"] = json!(init_id),
            Some("control_request") | None => continue,
            _ => expected_msgs += 1,
        }
        saw_result |= line["type"] == "result";
        peer.send_json(&line);
    }
    peer.exit(ExitInfo {
        code: Some(0),
        ..ExitInfo::default()
    });
    assert!(saw_result && expected_msgs > 2);
    let mut got = 0;
    let mut result_seen = false;
    while let Some(m) = session.next_message().await {
        let m = m.expect("no error items for a clean replay");
        assert!(!matches!(m, SdkMessage::Other(ref v) if v["type"] == "assistant"));
        result_seen |= m.is_result();
        got += 1;
    }
    assert_eq!(got, expected_msgs);
    assert!(result_seen);
    assert!(!session
        .supported_commands()
        .await
        .expect("commands")
        .is_empty());
}

#[tokio::test]
async fn supported_commands_follow_commands_changed() {
    let (session, peer, init_id) = start(opts()).await;
    peer.send_json(&ok_response(
        &init_id,
        Some(json!({"commands": [{"name": "a"}]})),
    ));
    assert_eq!(
        session.supported_commands().await.expect("cmds")[0].name,
        "a"
    );
    peer.send_json(&json!({
        "type": "system", "subtype": "commands_changed", "uuid": "u", "session_id": "s",
        "commands": [{"name": "a"}, {"name": "skill-b"}]
    }));
    peer.send_json(&json!({"type": "keep_alive"}));
    let mut session = session;
    // The message is still forwarded to the consumer.
    let first = tokio::time::timeout(Duration::from_secs(5), session.next_message())
        .await
        .expect("timeout");
    assert!(first.is_some());
    let cmds = session.supported_commands().await.expect("cmds");
    assert_eq!(cmds.len(), 2);
    assert_eq!(cmds[1].name, "skill-b");
}

#[tokio::test]
async fn close_input_still_answers_callbacks_until_first_result() {
    let perm = Arc::new(Perm {
        seen: Mutex::new(Vec::new()),
    });
    let (session, mut peer, init_id) = start(opts().can_use_tool(perm)).await;
    peer.send_json(&ok_response(&init_id, None));
    session.send_user_message("hi").expect("send");
    let _ = next_written(&mut peer).await;
    session.close_input();
    peer.send_json(&cli_request(
        "c1",
        json!({"subtype": "can_use_tool", "tool_name": "Write", "input": {}, "tool_use_id": "tu"}),
    ));
    let l = next_written(&mut peer).await;
    assert_eq!(l["response"]["request_id"], "c1");
    assert_eq!(l["response"]["response"]["behavior"], "allow");
    peer.send_json(&json!({
        "type": "result", "subtype": "success", "is_error": false, "result": "ok",
        "session_id": "s", "uuid": "r", "duration_ms": 1, "duration_api_ms": 1,
        "num_turns": 1, "total_cost_usd": 0, "usage": {}, "modelUsage": {},
        "permission_denials": []
    }));
    // stdin closes after the result.
    let closed = tokio::time::timeout(Duration::from_secs(5), peer.recv_json())
        .await
        .expect("timeout");
    assert!(closed.is_none());
}
