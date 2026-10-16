//! Exercises callbacks and controls against the real CLI: canUseTool, a PreToolUse hook,
//! an in-process MCP tool, set_model, set_permission_mode, mcp status, rewind_files,
//! interrupt, partial messages and resume.
//!
//! ```text
//! cargo run --example live_check -- /path/to/empty/scratch/dir
//! ```
//! Env: `CLAUDE_BIN`, `CLAUDE_MODEL` (default `haiku`). Spends a few cents.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use paseo_claude::protocol::{
    CanUseToolRequest, HookEvent, HookInput, HookJsonOutput, PermissionMode,
};
use paseo_claude::{
    BoxError, CanUseTool, ClaudeOptions, HookCallback, McpCallContext, McpTool, McpToolResult,
    PermissionDecision, SdkMcpServer, SdkMessage, Session, SessionHandle, UserInput,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

struct Perm(Arc<AtomicUsize>);

#[async_trait]
impl CanUseTool for Perm {
    async fn can_use_tool(
        &self,
        req: CanUseToolRequest,
        _: CancellationToken,
    ) -> Result<PermissionDecision, BoxError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        eprintln!("[can_use_tool] {} {}", req.tool_name, req.input);
        Ok(PermissionDecision::allow())
    }
}

struct Hook(Arc<AtomicUsize>);

#[async_trait]
impl HookCallback for Hook {
    async fn call(
        &self,
        input: HookInput,
        _: Option<String>,
        _: CancellationToken,
    ) -> Result<HookJsonOutput, BoxError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        if let HookInput::PreToolUse { tool_name, .. } = &input {
            eprintln!("[hook PreToolUse] {tool_name}");
        }
        Ok(HookJsonOutput::default())
    }
}

struct Adder(Arc<AtomicUsize>);

#[async_trait]
impl SdkMcpServer for Adder {
    fn name(&self) -> &str {
        "calc"
    }

    async fn list_tools(&self) -> Result<Vec<McpTool>, BoxError> {
        Ok(vec![McpTool::new(
            "add_numbers",
            json!({"type":"object","properties":{"a":{"type":"number"},"b":{"type":"number"}},"required":["a","b"]}),
        )
        .description("Add two numbers")])
    }

    async fn call_tool(
        &self,
        _: &str,
        args: Value,
        _: McpCallContext,
    ) -> Result<McpToolResult, BoxError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let sum = args["a"].as_f64().unwrap_or(0.0) + args["b"].as_f64().unwrap_or(0.0);
        Ok(McpToolResult::text(format!("sum={sum}")))
    }
}

async fn run_turn(
    session: &mut Session,
    prompt: UserInput,
) -> Result<(String, usize), Box<dyn std::error::Error>> {
    let uuid = session.send_user_message(prompt)?;
    let mut partials = 0;
    while let Some(msg) = session.next_message().await {
        match msg? {
            SdkMessage::StreamEvent(_) => partials += 1,
            SdkMessage::Result(r) => {
                eprintln!("[result] {} is_error={:?}", r.subtype, r.is_error);
                break;
            }
            _ => {}
        }
    }
    Ok((uuid, partials))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::args()
        .nth(1)
        .ok_or("usage: live_check <scratch dir>")?;
    let model = std::env::var("CLAUDE_MODEL").unwrap_or_else(|_| "haiku".into());
    let (perm, hook, mcp) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
        Arc::new(AtomicUsize::new(0)),
    );

    let options = |resume: Option<&str>| {
        let mut o = ClaudeOptions::new()
            .executable(std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".into()))
            .cwd(&dir)
            .model(model.clone())
            .setting_sources(Vec::<String>::new())
            .permission_mode(PermissionMode::Default)
            .include_partial_messages(true)
            .enable_file_checkpointing(true)
            .allowed_tools(["mcp__calc__add_numbers"])
            .sdk_mcp_server("calc", Arc::new(Adder(mcp.clone())))
            .can_use_tool(Arc::new(Perm(perm.clone())))
            .hook(
                HookEvent::PreToolUse,
                None,
                None,
                Arc::new(Hook(hook.clone())),
            );
        if let Some(r) = resume {
            o = o.resume(r);
        }
        o
    };

    let mut s = Session::start(options(None))?;
    let (u1, partials) = run_turn(
        &mut s,
        UserInput::text("Use the add_numbers tool with a=2 and b=40, then tell me the sum."),
    )
    .await?;
    println!(
        "turn1 partial events={partials} mcp_calls={}",
        mcp.load(Ordering::SeqCst)
    );

    let (u2, _) = run_turn(&mut s, UserInput::text("Use the Write tool to create file hello.txt containing the word original. Nothing else.")).await?;
    println!(
        "turn2 can_use_tool={} hooks={}",
        perm.load(Ordering::SeqCst),
        hook.load(Ordering::SeqCst)
    );

    println!(
        "models={} commands={}",
        s.supported_models().await?.len(),
        s.supported_commands().await?.len()
    );
    println!(
        "mode -> {:?}",
        s.set_permission_mode(PermissionMode::Plan).await?
    );
    s.set_permission_mode(PermissionMode::Default).await?;
    s.set_model(Some("haiku")).await?;
    s.set_model(None).await?;
    println!("mcp_status servers={}", s.mcp_server_status().await?.len());
    let dry = s.rewind_files(&u2, true).await?;
    println!(
        "rewind dry_run can_rewind={} files={:?} (u1 {u1})",
        dry.can_rewind, dry.files_changed
    );

    // Interrupt a running turn.
    let h: SessionHandle = s.handle();
    s.send_user_message("Count slowly from 1 to 200, one number per line.")?;
    let mut interrupted = false;
    while let Some(msg) = s.next_message().await {
        match msg? {
            SdkMessage::StreamEvent(_) if !interrupted => {
                interrupted = true;
                println!("interrupt -> {:?}", h.interrupt().await?.still_queued);
            }
            SdkMessage::Result(r) => {
                println!(
                    "interrupted result subtype={} terminal={:?}",
                    r.subtype, r.terminal_reason
                );
                break;
            }
            _ => {}
        }
    }

    let session_id = {
        let raw = s.initialize_response_raw().await?;
        raw.get("session_id")
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    s.close().await;

    // Resume: need the session id from system/init; run a fresh query to read it.
    let mut s2 = Session::start(options(None))?;
    s2.send_user_message("Remember the word: pineapple. Reply OK.")?;
    let mut sid = None;
    while let Some(msg) = s2.next_message().await {
        match msg? {
            SdkMessage::System(m) => {
                if let Ok(v) = serde_json::to_value(&m) {
                    if v["subtype"] == "init" {
                        sid = v["session_id"].as_str().map(str::to_string);
                    }
                }
            }
            SdkMessage::Result(_) => break,
            _ => {}
        }
    }
    s2.close().await;
    let sid = sid.or(session_id).ok_or("no session id")?;
    let mut s3 = Session::start(options(Some(&sid)))?;
    let mut text = String::new();
    s3.send_user_message("What word did I ask you to remember? One word.")?;
    while let Some(msg) = s3.next_message().await {
        match msg? {
            SdkMessage::Assistant(a) => {
                for b in &a.message.content {
                    if let Some(t) = b.as_text() {
                        text.push_str(t);
                    }
                }
            }
            SdkMessage::Result(_) => break,
            _ => {}
        }
    }
    println!("resume answer: {}", text.trim());
    s3.close().await;
    Ok(())
}
