//! Terminal tools.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{
    bool_arg, handler, ok_structured, required_str, str_arg, success, ToolEnv, ToolOutcome,
};
use crate::catalog::Registrar;
use crate::core_api::{Capability, CaptureOptions, TerminalSummary};
use crate::error::ToolError;
use crate::util::{js_trim, resolve_terminal_key_token};

pub(crate) fn register(r: &mut Registrar) {
    r.register("list_terminals", handler(list_terminals));
    r.register("create_terminal", handler(create_terminal));
    r.register("kill_terminal", handler(kill_terminal));
    r.register("capture_terminal", handler(capture_terminal));
    r.register("send_terminal_keys", handler(send_terminal_keys));
}

fn require_terminals(env: &ToolEnv) -> Result<(), ToolError> {
    if env.core.has(Capability::Terminals) {
        Ok(())
    } else {
        Err(ToolError::new("Terminal manager is not configured"))
    }
}

fn summary(terminal: &TerminalSummary) -> Value {
    json!({"id": terminal.id, "name": terminal.name, "cwd": terminal.cwd})
}

async fn list_terminals(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    require_terminals(&env)?;
    let terminals: Vec<Value> = if bool_arg(&input, "all").unwrap_or(false) {
        let mut all = Vec::new();
        for directory in env.core.terminal_directories() {
            for terminal in env.core.get_terminals(&directory).await? {
                all.push(summary(&terminal));
            }
        }
        all
    } else {
        let cwd = env.resolve_scoped_cwd(str_arg(&input, "cwd"), true)?;
        env.core
            .get_terminals(&cwd)
            .await?
            .iter()
            .map(summary)
            .collect()
    };
    ok_structured(json!({"terminals": terminals}))
}

/// `resolveTerminalWorkspaceId`: the caller agent's workspace, else a minted one.
async fn resolve_terminal_workspace_id(
    env: &ToolEnv,
    resolved_cwd: &str,
) -> Result<String, ToolError> {
    let caller = env
        .caller_agent_id
        .as_deref()
        .and_then(|id| env.core.get_agent(id));
    if let Some(workspace_id) = caller.and_then(|c| c.workspace_id) {
        return Ok(workspace_id);
    }
    if !env.core.has(Capability::WorkspaceMinting) {
        return Err(ToolError::new(match &env.caller_agent_id {
            Some(id) => format!(
                "Caller agent {id} has no workspace and workspace minting is not configured"
            ),
            None => "workspaceId is required outside an agent-scoped session".to_owned(),
        }));
    }
    env.core
        .ensure_workspace_for_create(resolved_cwd, None, None)
        .await
}

async fn create_terminal(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    require_terminals(&env)?;
    let resolved_cwd = env.resolve_scoped_cwd(str_arg(&input, "cwd"), true)?;
    let workspace_id = resolve_terminal_workspace_id(&env, &resolved_cwd).await?;
    let name = str_arg(&input, "name")
        .map(js_trim)
        .filter(|n| !n.is_empty());
    let terminal = env
        .core
        .create_terminal(&resolved_cwd, &workspace_id, name)
        .await?;
    ok_structured(summary(&terminal))
}

async fn kill_terminal(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    require_terminals(&env)?;
    let terminal_id = required_str(&input, "terminalId");
    if env.core.get_terminal(terminal_id).is_none() {
        return Err(ToolError::new(format!("Terminal {terminal_id} not found")));
    }
    env.core.kill_terminal(terminal_id);
    success()
}

async fn capture_terminal(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    require_terminals(&env)?;
    let terminal_id = required_str(&input, "terminalId");
    if env.core.get_terminal(terminal_id).is_none() {
        return Err(ToolError::new(format!("Terminal {terminal_id} not found")));
    }
    let scrollback = bool_arg(&input, "scrollback").unwrap_or(false);
    let capture = env
        .core
        .capture_terminal(
            terminal_id,
            CaptureOptions {
                start: if scrollback {
                    Some(0.0)
                } else {
                    input.get("start").and_then(Value::as_f64)
                },
                end: input.get("end").and_then(Value::as_f64),
                strip_ansi: bool_arg(&input, "stripAnsi").unwrap_or(true),
            },
        )
        .await?;
    ok_structured(json!({
        "terminalId": terminal_id,
        "lines": capture.lines,
        "totalLines": capture.total_lines,
    }))
}

async fn send_terminal_keys(env: Arc<ToolEnv>, input: Value, _c: CancellationToken) -> ToolOutcome {
    require_terminals(&env)?;
    let terminal_id = required_str(&input, "terminalId");
    if env.core.get_terminal(terminal_id).is_none() {
        return Err(ToolError::new(format!("Terminal {terminal_id} not found")));
    }
    let literal = bool_arg(&input, "literal").unwrap_or(false);
    let data = resolve_terminal_key_token(required_str(&input, "keys"), literal);
    env.core.send_terminal_input(terminal_id, &data);
    success()
}
