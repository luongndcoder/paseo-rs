//! Golden replay tests: each fixture under `fixtures/` is a capture of the TS SDK driving the
//! real CLI (see `tools/capture`). A fake CLI built on [`InMemoryTransport`] walks the records in
//! order. For every recorded `in` line (what the SDK wrote) it asserts the Rust client writes the
//! same JSON; for every `out` line (CLI stdout) it feeds the line to the client.
//!
//! Normalisation: `request_id` of client control requests is compared through a correlation map
//! (recorded id -> actual id) and the client-chosen user message `uuid` is only compared when the
//! capture carries one. A string user `content` is the same as a single text block.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use paseo_claude::protocol::{
    CanUseToolRequest, HookEvent, HookInput, HookJsonOutput, PermissionMode,
};
use paseo_claude::{
    BoxError, CanUseTool, ClaudeOptions, ExitInfo, HookCallback, InMemoryPeer, InMemoryTransport,
    McpCallContext, McpTool, McpToolResult, PermissionDecision, SdkMcpServer, SdkMessage, Session,
    UserInput,
};

const STEP: Duration = Duration::from_secs(10);

struct Fixture {
    argv: Vec<String>,
    records: Vec<Value>,
}

fn load(name: &str) -> Fixture {
    let path = format!("{}/fixtures/{name}.jsonl", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header: Value = serde_json::from_str(lines.next().expect("header")).expect("header json");
    let argv = header["argv"]
        .as_array()
        .expect("argv")
        .iter()
        .map(|v| v.as_str().expect("argv str").to_string())
        .collect();
    let records = lines
        .map(|l| serde_json::from_str(l).expect("record json"))
        .collect();
    Fixture { argv, records }
}

/// Canonical form of a client-written line for comparison.
fn normalize(mut v: Value, keep_uuid: bool) -> Value {
    match v["type"].as_str() {
        Some("control_request") => {
            v["request_id"] = json!("<id>");
        }
        Some("user") => {
            if let Some(Value::String(s)) = v["message"].get("content").cloned() {
                v["message"]["content"] = json!([{"type": "text", "text": s}]);
            }
            if !keep_uuid {
                if let Some(o) = v.as_object_mut() {
                    o.remove("uuid");
                }
            }
        }
        _ => {}
    }
    v
}

/// Fake CLI. Returns the number of conversation messages it fed to the client.
/// `wait_for_close`: the recorded exit was caused by the SDK closing the process, so the exit
/// event is delivered only once the client closed stdin.
async fn run_peer(mut peer: InMemoryPeer, records: Vec<Value>, wait_for_close: bool) -> usize {
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut fed = 0;
    for (n, rec) in records.iter().enumerate() {
        if let Some(exit) = rec.get("exit") {
            if wait_for_close {
                while tokio::time::timeout(STEP, peer.recv())
                    .await
                    .unwrap_or_else(|_| panic!("record {n}: client never closed stdin"))
                    .is_some()
                {}
            }
            peer.exit(ExitInfo {
                code: exit["code"].as_i64().map(|c| c as i32),
                ..ExitInfo::default()
            });
            continue;
        }
        let line = rec["line"].clone();
        match rec["dir"].as_str().expect("dir") {
            "in" => {
                let actual = tokio::time::timeout(STEP, peer.recv_json())
                    .await
                    .unwrap_or_else(|_| panic!("record {n}: client wrote nothing, wanted {line}"))
                    .unwrap_or_else(|| panic!("record {n}: stdin closed, wanted {line}"));
                let keep_uuid = line.get("uuid").is_some();
                if line["type"] == "control_request" {
                    let rec_id = line["request_id"].as_str().expect("request_id").to_string();
                    let act_id = actual["request_id"]
                        .as_str()
                        .expect("actual id")
                        .to_string();
                    ids.insert(rec_id, act_id);
                }
                assert_eq!(
                    normalize(actual, keep_uuid),
                    normalize(line, keep_uuid),
                    "record {n}: client line differs from the TS SDK capture"
                );
            }
            "out" => {
                let mut line = line;
                match line["type"].as_str() {
                    Some("control_response") => {
                        let rid = line["response"]["request_id"].as_str().expect("rid");
                        let actual = ids.get(rid).unwrap_or_else(|| {
                            panic!("record {n}: response to unknown request {rid}")
                        });
                        line["response"]["request_id"] = json!(actual);
                    }
                    Some("control_request") | Some("keep_alive") | Some("transcript_mirror") => {}
                    _ => fed += 1,
                }
                peer.send_json(&line);
            }
            other => panic!("record {n}: bad dir {other}"),
        }
    }
    fed
}

struct Outcome {
    messages: Vec<SdkMessage>,
    /// The final `Err` item, if the stream ended with one.
    error: Option<String>,
}

async fn drain(session: &mut Session) -> Outcome {
    let mut out = Outcome {
        messages: Vec::new(),
        error: None,
    };
    while let Some(item) = tokio::time::timeout(STEP, session.next_message())
        .await
        .expect("stream stalled")
    {
        match item {
            Ok(m) => out.messages.push(m),
            Err(e) => out.error = Some(e.to_string()),
        }
    }
    out
}

/// Drain until the first `result`, leaving the stream open.
async fn until_result(session: &mut Session) -> Vec<SdkMessage> {
    let mut msgs = Vec::new();
    loop {
        let m = tokio::time::timeout(STEP, session.next_message())
            .await
            .expect("stream stalled")
            .expect("stream ended before result")
            .expect("no error before result");
        let done = m.is_result();
        msgs.push(m);
        if done {
            return msgs;
        }
    }
}

fn base() -> ClaudeOptions {
    ClaudeOptions::new()
        .control_timeout(STEP)
        .model("haiku")
        .setting_sources(Vec::<String>::new())
        .permission_mode(PermissionMode::Default)
}

fn bash_prompt() -> &'static str {
    "Use the Bash tool to run `echo captured > out.txt` and then say done."
}

fn assert_argv(f: &Fixture, options: ClaudeOptions) {
    let p = options.prepare().expect("prepare");
    assert_eq!(p.spawn.args, f.argv, "argv differs from the TS SDK capture");
}

/// Runs `drive` against the fake CLI. `drive` gets the session and returns the number of
/// conversation messages it consumed (so the peer's `fed` count can be checked).
async fn replay<F, Fut>(
    name: &str,
    options: impl Fn() -> ClaudeOptions,
    wait_for_close: bool,
    drive: F,
) where
    F: FnOnce(Session) -> Fut,
    Fut: std::future::Future<Output = usize>,
{
    let f = load(name);
    assert_argv(&f, options());
    let (t, peer) = InMemoryTransport::new();
    let session = Session::start_with_transport(options(), Box::new(t)).expect("start");
    let peer_task = tokio::spawn(run_peer(peer, f.records, wait_for_close));
    let consumed = tokio::time::timeout(Duration::from_secs(60), drive(session))
        .await
        .expect("driver timed out");
    let fed = tokio::time::timeout(STEP, peer_task)
        .await
        .expect("peer timed out")
        .expect("peer panicked");
    assert_eq!(consumed, fed, "message count differs from the capture");
}

fn assert_ok_result(msgs: &[SdkMessage]) {
    let r = msgs
        .iter()
        .find_map(SdkMessage::as_result)
        .expect("a result message");
    let v = serde_json::to_value(r).expect("json");
    assert_eq!(v["subtype"], "success", "{v}");
}

// ---- scenarios ----

#[tokio::test]
async fn s01_single_prompt() {
    replay(
        "01-single-prompt",
        || base().max_turns(1),
        false,
        |mut s| async move {
            s.send_user_message("Reply with exactly the word: pong. No tools.")
                .expect("send");
            let out = drain(&mut s).await;
            assert!(out.error.is_none(), "{:?}", out.error);
            assert_ok_result(&out.messages);
            assert!(!s.supported_commands().await.expect("commands").is_empty());
            out.messages.len()
        },
    )
    .await;
}

struct Decide {
    allow: bool,
    calls: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl CanUseTool for Decide {
    async fn can_use_tool(
        &self,
        request: CanUseToolRequest,
        _: CancellationToken,
    ) -> Result<PermissionDecision, BoxError> {
        self.calls
            .lock()
            .expect("lock")
            .push(request.tool_name.clone());
        Ok(if self.allow {
            PermissionDecision::allow_with_input(request.input)
        } else {
            PermissionDecision::deny("Denied by capture script")
        })
    }
}

async fn can_use_tool_scenario(name: &str, allow: bool) {
    let decide = Arc::new(Decide {
        allow,
        calls: Default::default(),
    });
    let d = decide.clone();
    replay(
        name,
        move || base().max_turns(3).can_use_tool(d.clone()),
        false,
        |mut s| async move {
            s.send_user_message(bash_prompt()).expect("send");
            let out = drain(&mut s).await;
            assert!(out.error.is_none(), "{:?}", out.error);
            assert_ok_result(&out.messages);
            out.messages.len()
        },
    )
    .await;
    assert_eq!(
        *decide.calls.lock().expect("lock"),
        vec!["Bash".to_string()]
    );
}

#[tokio::test]
async fn s02_tool_allow() {
    can_use_tool_scenario("02-tool-allow", true).await;
}

#[tokio::test]
async fn s03_tool_deny() {
    can_use_tool_scenario("03-tool-deny", false).await;
}

struct Observe(std::sync::Mutex<Vec<String>>);

#[async_trait]
impl HookCallback for Observe {
    async fn call(
        &self,
        input: HookInput,
        _: Option<String>,
        _: CancellationToken,
    ) -> Result<HookJsonOutput, BoxError> {
        let name = match &input {
            HookInput::PreToolUse { tool_name, .. } => format!("pre:{tool_name}"),
            _ => "other".to_string(),
        };
        self.0.lock().expect("lock").push(name);
        Ok(HookJsonOutput {
            continue_: Some(true),
            ..HookJsonOutput::default()
        })
    }
}

#[tokio::test]
async fn s04_pretooluse_hook() {
    let seen = Arc::new(Observe(Default::default()));
    let o = seen.clone();
    replay(
        "04-pretooluse-hook",
        move || {
            base()
                .max_turns(3)
                .permission_mode(PermissionMode::BypassPermissions)
                .allow_dangerously_skip_permissions(true)
                .hook(HookEvent::PreToolUse, None, None, o.clone())
                .hook(HookEvent::PostToolUse, None, None, o.clone())
        },
        false,
        |mut s| async move {
            s.send_user_message("Use the Read tool to read hello.txt and tell me its content.")
                .expect("send");
            let out = drain(&mut s).await;
            assert!(out.error.is_none(), "{:?}", out.error);
            assert_ok_result(&out.messages);
            out.messages.len()
        },
    )
    .await;
    assert_eq!(
        seen.0.lock().expect("lock").first().map(String::as_str),
        Some("pre:Read")
    );
    assert_eq!(seen.0.lock().expect("lock").len(), 2);
}

struct Fixture5;

#[async_trait]
impl SdkMcpServer for Fixture5 {
    fn name(&self) -> &str {
        "fixture"
    }

    async fn list_tools(&self) -> Result<Vec<McpTool>, BoxError> {
        Ok(vec![McpTool::new(
            "add_numbers",
            json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
                "required": ["a", "b"],
            }),
        )
        .description("Add two integers and return the sum")])
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        _: McpCallContext,
    ) -> Result<McpToolResult, BoxError> {
        assert_eq!(name, "add_numbers");
        let sum = arguments["a"].as_i64().expect("a") + arguments["b"].as_i64().expect("b");
        Ok(McpToolResult::text(sum.to_string()))
    }
}

#[tokio::test]
async fn s05_sdk_mcp_tool() {
    replay(
        "05-sdk-mcp-tool",
        || {
            base()
                .max_turns(3)
                .permission_mode(PermissionMode::BypassPermissions)
                .allow_dangerously_skip_permissions(true)
                .sdk_mcp_server("fixture", Arc::new(Fixture5))
        },
        false,
        |mut s| async move {
            s.send_user_message(
                "Call the add_numbers tool from the fixture MCP server with a=17 and b=25, then state the result.",
            )
            .expect("send");
            let out = drain(&mut s).await;
            assert!(out.error.is_none(), "{:?}", out.error);
            assert_ok_result(&out.messages);
            out.messages.len()
        },
    )
    .await;
}

#[tokio::test]
async fn s06_interrupt() {
    replay(
        "06-interrupt",
        || base().max_turns(5).include_partial_messages(true),
        false,
        |mut s| async move {
            s.send_user_message(
                "Write a very long essay (at least 2000 words) about the history of the bicycle.",
            )
            .expect("send");
            let mut interrupt = None;
            let mut messages = 0;
            let mut error = None;
            while let Some(item) = tokio::time::timeout(STEP, s.next_message())
                .await
                .expect("stream stalled")
            {
                match item {
                    Ok(m) => {
                        messages += 1;
                        if interrupt.is_none() && matches!(m, SdkMessage::StreamEvent(_)) {
                            let h = s.handle();
                            interrupt = Some(tokio::spawn(async move { h.interrupt().await }));
                        }
                    }
                    Err(e) => error = Some(e),
                }
            }
            interrupt
                .expect("saw a stream event")
                .await
                .expect("join")
                .expect("interrupt acknowledged");
            // Exit code 1 after the error result surfaces as a trailing error item.
            assert!(error.is_some(), "expected a trailing error item");
            messages
        },
    )
    .await;
}

#[tokio::test]
async fn s07_controls() {
    replay(
        "07-controls",
        || base().max_turns(1),
        true,
        |mut s| async move {
            s.send_user_message("Reply with exactly: ok").expect("send");
            assert!(!s.supported_commands().await.expect("commands").is_empty());
            s.supported_models().await.expect("models");
            let m = s
                .set_permission_mode(PermissionMode::AcceptEdits)
                .await
                .expect("mode");
            assert_eq!(m, PermissionMode::AcceptEdits);
            s.set_model(Some("sonnet")).await.expect("model");
            s.set_permission_mode(PermissionMode::Default)
                .await
                .expect("mode");
            s.set_model(None).await.expect("model reset");
            assert!(s.mcp_server_status().await.expect("mcp status").is_empty());
            let msgs = until_result(&mut s).await;
            s.close().await;
            let rest = drain(&mut s).await;
            assert!(
                rest.error.is_none(),
                "close must not surface an error: {:?}",
                rest.error
            );
            msgs.len() + rest.messages.len()
        },
    )
    .await;
}

#[tokio::test]
async fn s08_resume() {
    replay(
        "08-resume",
        || {
            base()
                .max_turns(1)
                .resume("c013e709-4377-4a6a-8971-f68b7c332b89")
        },
        false,
        |mut s| async move {
            s.send_user_message(
                "What single word did I ask you to reply with earlier? Answer in one word.",
            )
            .expect("send");
            let out = drain(&mut s).await;
            assert!(out.error.is_none(), "{:?}", out.error);
            assert_ok_result(&out.messages);
            out.messages.len()
        },
    )
    .await;
}

#[tokio::test]
async fn s09_partial_messages() {
    replay(
        "09-partial-messages",
        || base().max_turns(1).include_partial_messages(true),
        false,
        |mut s| async move {
            s.send_user_message("Count from 1 to 10 separated by spaces. No tools.")
                .expect("send");
            let out = drain(&mut s).await;
            assert!(out.error.is_none(), "{:?}", out.error);
            assert!(out
                .messages
                .iter()
                .any(|m| matches!(m, SdkMessage::StreamEvent(_))));
            assert_ok_result(&out.messages);
            out.messages.len()
        },
    )
    .await;
}

#[tokio::test]
async fn s10_rewind_files() {
    let uuid = "6ecca4d2-cdad-4934-8b07-0962bffc5994";
    let options = || {
        base()
            .max_turns(4)
            .permission_mode(PermissionMode::BypassPermissions)
            .allow_dangerously_skip_permissions(true)
            .enable_file_checkpointing(true)
    };
    let p = options().prepare().expect("prepare");
    assert_eq!(
        p.spawn.env.get("CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING"),
        Some(&Some("true".to_string())).or(Some(&Some("1".to_string())))
    );
    replay("10-rewind-files", options, true, move |mut s| async move {
        let sent = s
            .send_user_message(
                UserInput::text(
                    "Use the Write tool to overwrite notes.txt with the single line: modified",
                )
                .uuid(uuid),
            )
            .expect("send");
        assert_eq!(sent, uuid);
        let msgs = until_result(&mut s).await;
        let dry = s.rewind_files(uuid, true).await.expect("dry run");
        assert!(dry.can_rewind);
        assert_eq!(dry.files_changed.as_deref().map(<[String]>::len), Some(1));
        let real = s.rewind_files(uuid, false).await.expect("rewind");
        assert!(real.can_rewind);
        s.close().await;
        let rest = drain(&mut s).await;
        assert!(rest.error.is_none(), "{:?}", rest.error);
        msgs.len() + rest.messages.len()
    })
    .await;
}
