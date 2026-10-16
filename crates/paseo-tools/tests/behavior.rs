//! Behavior of the tools beyond the golden transcripts, ported from `mcp-server.test.ts`: caller
//! scoping, create_agent placement (canonical and COMPAT legacy), prompt dispatch ordering,
//! schedules (including the COMPAT `every` shim), workspaces, browser rendering, policy gates.

mod common;

use std::sync::Arc;

use common::script::*;
use paseo_core::PaseoToolPolicy;
use paseo_store::WorkspaceKind;
use paseo_tools::core_api::*;
use paseo_tools::{RuntimeCtx, ToolCatalog};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const CALLER: &str = "caller-1";

fn caller_runtime() -> RuntimeCtx {
    RuntimeCtx {
        caller_agent_id: Some(CALLER.into()),
        ..RuntimeCtx::default()
    }
}

fn caller_script() -> Arc<Script> {
    Script::new().with(|s| {
        s.agents.insert(
            CALLER.into(),
            agent(CALLER, "claude", "/work/repo", Some("wks_caller")),
        );
    })
}

async fn run(catalog: &ToolCatalog, tool: &str, args: Value) -> Value {
    catalog
        .call_tool_for_mcp(tool, Some(&args), CancellationToken::new())
        .await
}

fn structured(result: &Value) -> &Value {
    assert!(result["isError"].is_null(), "unexpected error: {result}");
    &result["structuredContent"]
}

fn error_text(result: &Value) -> &str {
    assert_eq!(result["isError"], true, "expected an error: {result}");
    result["content"][0]["text"].as_str().unwrap()
}

// ---- create_agent -----------------------------------------------------------------------------

#[tokio::test]
async fn create_agent_top_level_mints_a_local_workspace() {
    let core = Script::new().caps(&[Capability::WorkspaceMinting]);
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(
        &catalog,
        "create_agent",
        json!({"title": " Fix bug ", "provider": " claude/model-a ", "initialPrompt": " do it ", "settings": {"modeId": "plan", "thinkingOptionId": "high", "features": {"fast": true}}, "labels": {"a": "b"}}),
    )
    .await;
    let out = structured(&result);
    assert_eq!(out["agentId"], "new-agent");
    assert_eq!(out["type"], "claude/model-a");
    assert_eq!(out["status"], "idle");
    assert_eq!(out["workspaceId"], "wks_new");
    assert_eq!(out["currentModeId"], "default");
    assert!(out["guidance"]
        .as_str()
        .unwrap()
        .starts_with("Creation acknowledged."));

    let [create] = core.args_of("create_agent").try_into().unwrap();
    assert_eq!(create["provider"], "claude/model-a");
    assert_eq!(create["title"], "Fix bug");
    assert_eq!(create["initialPrompt"], "do it");
    assert_eq!(create["cwd"], "/proc/cwd");
    assert_eq!(create["workspaceId"], "wks_minted");
    assert_eq!(create["mode"], "plan");
    assert_eq!(create["thinking"], "high");
    assert_eq!(create["features"], json!({"fast": true}));
    assert_eq!(create["labels"], json!({"a": "b"}));
    assert_eq!(create["notifyOnFinish"], false);
    assert_eq!(create["detached"], false);
    assert_eq!(create["callerAgentId"], Value::Null);
    // The workspace is minted for the process cwd with the prompt as first-agent context.
    assert_eq!(
        core.args_of("ensure_workspace_for_create"),
        vec![json!(["/proc/cwd", {"prompt": "do it"}, null])]
    );
}

#[tokio::test]
async fn create_agent_top_level_needs_workspace_minting() {
    let core = Script::new();
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(
        &catalog,
        "create_agent",
        json!({"title": "t", "provider": "claude/m", "initialPrompt": "p"}),
    )
    .await;
    assert_eq!(error_text(&result), "Workspace creation is not configured");
}

#[tokio::test]
async fn create_agent_input_validation_messages() {
    let core = Script::new().caps(&[Capability::WorkspaceMinting]);
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let base = json!({"title": "t", "provider": "claude/m", "initialPrompt": "p"});
    for (patch, needle) in [
        (
            json!({"title": "x".repeat(61)}),
            "Title must be 60 characters or fewer",
        ),
        (json!({"title": "  "}), "Title is required"),
        (json!({"initialPrompt": " "}), "initialPrompt is required"),
        (
            json!({"provider": "claude"}),
            "provider must be provider/model, for example codex/gpt-5.4",
        ),
        (
            json!({"provider": "claude/"}),
            "provider must be provider/model, for example codex/gpt-5.4",
        ),
    ] {
        let mut args = base.clone();
        for (k, v) in patch.as_object().unwrap() {
            args[k] = v.clone();
        }
        let result = run(&catalog, "create_agent", args).await;
        let text = error_text(&result);
        assert!(
            text.starts_with(
                "MCP error -32602: Input validation error: Invalid arguments for tool create_agent"
            ),
            "{text}"
        );
        assert!(text.contains(needle), "{text}");
    }
    assert!(core.args_of("create_agent").is_empty());
}

#[tokio::test]
async fn create_agent_rejects_unknown_keys_with_the_strict_zod_message() {
    let core = Script::new().caps(&[Capability::WorkspaceMinting]);
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(
        &catalog,
        "create_agent",
        json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "model": "x"}),
    )
    .await;
    let text = error_text(&result);
    assert!(text.contains("\"code\": \"unrecognized_keys\""), "{text}");
    assert!(
        text.contains("Unrecognized key: \\\"model\\\"")
            || text.contains("Unrecognized key: \"model\""),
        "{text}"
    );
}

#[tokio::test]
async fn create_agent_for_a_caller_defaults_to_its_workspace_and_notifies() {
    let core = caller_script();
    let catalog = core.catalog(caller_runtime()).await;
    let result = run(
        &catalog,
        "create_agent",
        json!({"title": "t", "provider": "claude/m", "initialPrompt": "p"}),
    )
    .await;
    let out = structured(&result);
    assert!(out["guidance"]
        .as_str()
        .unwrap()
        .starts_with("You will get notified when the created agent finishes"));
    let [create] = core.args_of("create_agent").try_into().unwrap();
    assert_eq!(create["workspaceId"], "wks_caller");
    assert_eq!(create["cwd"], Value::Null);
    assert_eq!(create["notifyOnFinish"], true);
    assert_eq!(create["callerAgentId"], CALLER);
    assert_eq!(create["detached"], false);

    // Opting out falls back to the plain acknowledgement.
    let result = run(&catalog, "create_agent", json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "notifyOnFinish": false})).await;
    assert!(structured(&result)["guidance"]
        .as_str()
        .unwrap()
        .starts_with("Creation acknowledged."));
}

#[tokio::test]
async fn create_agent_inherits_provider_options_only_for_the_same_provider() {
    let core = Script::new().with(|s| {
        let mut caller = agent(CALLER, "claude", "/work/repo", Some("wks_caller"));
        caller.config.provider_options = Some(json!({"effort": "max"}));
        s.agents.insert(CALLER.into(), caller);
    });
    let catalog = core.catalog(caller_runtime()).await;
    run(
        &catalog,
        "create_agent",
        json!({"title": "t", "provider": "claude/m", "initialPrompt": "p"}),
    )
    .await;
    run(
        &catalog,
        "create_agent",
        json!({"title": "t", "provider": "codex/m", "initialPrompt": "p"}),
    )
    .await;
    let creates = core.args_of("create_agent");
    assert_eq!(creates[0]["providerOptions"], json!({"effort": "max"}));
    assert_eq!(creates[1]["providerOptions"], Value::Null);
}

#[tokio::test]
async fn create_agent_missing_caller_or_caller_workspace() {
    let core = Script::new();
    let catalog = core.catalog(caller_runtime()).await;
    let result = run(
        &catalog,
        "create_agent",
        json!({"title": "t", "provider": "claude/m", "initialPrompt": "p"}),
    )
    .await;
    assert_eq!(
        error_text(&result),
        format!("Parent agent {CALLER} not found")
    );

    let core = Script::new().with(|s| {
        s.agents
            .insert(CALLER.into(), agent(CALLER, "claude", "/w", None));
    });
    let catalog = core.catalog(caller_runtime()).await;
    let result = run(
        &catalog,
        "create_agent",
        json!({"title": "t", "provider": "claude/m", "initialPrompt": "p"}),
    )
    .await;
    assert_eq!(
        error_text(&result),
        format!("Caller agent {CALLER} has no current workspace")
    );
}

#[tokio::test]
async fn create_agent_into_an_existing_workspace() {
    let core = Script::new()
        .caps(&[Capability::ActiveWorkspaces])
        .with(|s| {
            s.active.push(ActiveWorkspaceRef {
                workspace_id: "wks_x".into(),
                cwd: "/ws/x".into(),
            });
        });
    let catalog = core.catalog(RuntimeCtx::default()).await;
    run(
        &catalog,
        "create_agent",
        json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "workspaceId": "wks_x"}),
    )
    .await;
    let [create] = core.args_of("create_agent").try_into().unwrap();
    assert_eq!(create["workspaceId"], "wks_x");
    assert_eq!(create["cwd"], "/ws/x");

    let result = run(&catalog, "create_agent", json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "workspaceId": "missing"})).await;
    assert_eq!(error_text(&result), "Workspace missing not found");

    let core = Script::new();
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(
        &catalog,
        "create_agent",
        json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "workspaceId": "wks_x"}),
    )
    .await;
    assert_eq!(error_text(&result), "Workspace lookup is not configured");
}

#[tokio::test]
async fn create_agent_surfaces_core_failures() {
    let core = Script::new()
        .caps(&[Capability::WorkspaceMinting])
        .with(|s| s.create_error = Some("provider exploded".into()));
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(
        &catalog,
        "create_agent",
        json!({"title": "t", "provider": "claude/m", "initialPrompt": "p"}),
    )
    .await;
    assert_eq!(error_text(&result), "provider exploded");
}

// COMPAT(nestedCreateAgentPlacement)
#[tokio::test]
async fn compat_nested_create_agent_placement() {
    // Top-level legacy: a bare cwd becomes a detached directory workspace.
    let core = Script::new().caps(&[Capability::WorkspaceMinting]);
    let catalog = core.catalog(RuntimeCtx::default()).await;
    run(&catalog, "create_agent", json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "cwd": "/legacy/dir", "mode": "plan", "thinking": "low", "features": {"f": 1}})).await;
    let [create] = core.args_of("create_agent").try_into().unwrap();
    assert_eq!(create["cwd"], "/legacy/dir");
    assert_eq!(create["workspaceId"], "wks_minted");
    assert_eq!(create["detached"], true);
    assert_eq!(create["mode"], "plan");
    assert_eq!(create["thinking"], "low");
    assert_eq!(create["features"], json!({"f": 1}));

    // Legacy worktree targets.
    let core = Script::new();
    let catalog = core.catalog(RuntimeCtx::default()).await;
    run(&catalog, "create_agent", json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "cwd": "/repo", "refName": "feature"})).await;
    run(&catalog, "create_agent", json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "cwd": "/repo", "githubPrNumber": 12})).await;
    run(&catalog, "create_agent", json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "cwd": "/repo", "worktreeName": "wt", "branchName": "br", "baseBranch": "main"})).await;
    let creates = core.args_of("create_agent");
    assert_eq!(creates[0]["worktree"]["action"], "checkout");
    assert_eq!(creates[0]["worktree"]["refName"], "feature");
    assert_eq!(creates[0]["workspaceId"], Value::Null);
    assert_eq!(creates[1]["worktree"]["githubPrNumber"], 12);
    assert_eq!(
        creates[2]["worktree"],
        json!({"action": "branch-off", "worktreeName": "wt", "branchName": "br", "baseBranch": "main", "refName": null, "githubPrNumber": null})
    );

    // Explicit relationship + workspace.
    let core = Script::new().caps(&[Capability::WorkspaceMinting]);
    let catalog = core.catalog(RuntimeCtx::default()).await;
    run(&catalog, "create_agent", json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "relationship": {"kind": "detached"}, "workspace": {"kind": "create", "source": {"kind": "directory", "path": "/explicit"}}})).await;
    assert_eq!(core.args_of("create_agent")[0]["cwd"], "/explicit");

    // Errors.
    let result = run(&catalog, "create_agent", json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "relationship": {"kind": "detached"}})).await;
    assert_eq!(
        error_text(&result),
        "relationship and workspace must be provided together"
    );
    let result = run(
        &catalog,
        "create_agent",
        json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "worktreeName": "w"}),
    )
    .await;
    assert_eq!(
        error_text(&result),
        "cwd is required for legacy top-level create_agent calls"
    );
    let result = run(&catalog, "create_agent", json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "relationship": {"kind": "subagent"}, "workspace": {"kind": "current"}})).await;
    assert_eq!(
        error_text(&result),
        "relationship subagent requires an agent-scoped tool session"
    );
    let result = run(&catalog, "create_agent", json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "relationship": {"kind": "detached"}, "workspace": {"kind": "current"}})).await;
    assert_eq!(
        error_text(&result),
        "workspace current requires an agent-scoped tool session"
    );
}

#[tokio::test]
async fn compat_nested_placement_for_callers() {
    let core = caller_script().caps(&[Capability::WorkspaceMinting]);
    let catalog = core.catalog(caller_runtime()).await;
    run(&catalog, "create_agent", json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "relationship": {"kind": "detached"}, "workspace": {"kind": "current", "cwd": "sub"}})).await;
    let [create] = core.args_of("create_agent").try_into().unwrap();
    assert_eq!(create["detached"], true);
    assert_eq!(create["workspaceId"], "wks_caller");
    assert_eq!(create["cwd"], "sub");
    assert_eq!(create["notifyOnFinish"], true);

    // A directory source resolves against the caller's cwd and mints a workspace.
    run(&catalog, "create_agent", json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "relationship": {"kind": "subagent"}, "workspace": {"kind": "create", "source": {"kind": "directory", "path": "pkg"}}})).await;
    let creates = core.args_of("create_agent");
    assert_eq!(creates[1]["cwd"], "/work/repo/pkg");
    assert_eq!(creates[1]["detached"], false);
    assert_eq!(
        core.args_of("ensure_workspace_for_create")[0],
        json!(["/work/repo/pkg", {"prompt": "p"}, "wks_caller"])
    );

    // Worktree sources carry the target.
    run(&catalog, "create_agent", json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "relationship": {"kind": "subagent"}, "workspace": {"kind": "create", "source": {"kind": "worktree", "target": {"kind": "checkout-branch", "branch": "dev"}}}})).await;
    let creates = core.args_of("create_agent");
    assert_eq!(creates[2]["cwd"], "/work/repo");
    assert_eq!(creates[2]["worktree"]["refName"], "dev");
}

#[tokio::test]
async fn caller_locked_cwd_confines_existing_workspaces() {
    let core = caller_script()
        .caps(&[Capability::ActiveWorkspaces])
        .with(|s| {
            s.caller_context = Some(CallerContext {
                locked_cwd: Some("/locked".into()),
                allow_custom_cwd: Some(true),
                ..CallerContext::default()
            });
            s.active.push(ActiveWorkspaceRef {
                workspace_id: "wks_in".into(),
                cwd: "/locked/in".into(),
            });
            s.active.push(ActiveWorkspaceRef {
                workspace_id: "wks_out".into(),
                cwd: "/other".into(),
            });
        });
    let catalog = core.catalog(caller_runtime()).await;
    let ok = run(&catalog, "create_agent", json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "workspaceId": "wks_in"})).await;
    structured(&ok);
    // The workspace's cwd is kept; the core applies the locked cwd when it places the agent.
    assert_eq!(core.args_of("create_agent")[0]["cwd"], "/locked/in");
    let result = run(&catalog, "create_agent", json!({"title": "t", "provider": "claude/m", "initialPrompt": "p", "workspaceId": "wks_out"})).await;
    assert_eq!(
        error_text(&result),
        "Workspace wks_out is outside the allowed cwd"
    );
}

// ---- send_agent_prompt ------------------------------------------------------------------------

#[tokio::test]
async fn send_prompt_top_level_acknowledges_without_a_notification() {
    let core = Script::new().with(|s| {
        s.agents.insert("child".into(), {
            let mut a = agent("child", "claude", "/c", None);
            a.lifecycle = "running".into();
            a
        });
    });
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(
        &catalog,
        "send_agent_prompt",
        json!({"agentId": "child", "prompt": "go", "sessionMode": "plan"}),
    )
    .await;
    assert_eq!(
        structured(&result),
        &json!({"success": true, "disposition": "turn_started", "status": "running",
                "guidance": "Dispatch acknowledged. Observe completion using get_agent_status/get_agent_activity or SDK/RPC subscriptions and wait APIs."})
    );
    // Order: dispatch, then the run-start wait.
    assert_eq!(
        core.methods(),
        vec!["send_prompt_to_agent", "wait_for_agent_run_start"]
    );
    assert_eq!(
        core.args_of("send_prompt_to_agent")[0],
        json!({"agentId": "child", "prompt": "go", "source": null, "sessionMode": "plan"})
    );
    // Top-level callers do not get finish notifications even when asking.
    run(
        &catalog,
        "send_agent_prompt",
        json!({"agentId": "child", "prompt": "go", "notifyOnFinish": true}),
    )
    .await;
    assert!(core.args_of("setup_finish_notification").is_empty());
}

#[tokio::test]
async fn send_prompt_from_an_agent_arms_the_finish_notification_inside_the_dispatch() {
    let core = caller_script().with(|s| {
        s.agents
            .insert("child".into(), agent("child", "claude", "/c", None));
    });
    let catalog = core.catalog(caller_runtime()).await;
    let result = run(
        &catalog,
        "send_agent_prompt",
        json!({"agentId": "child", "prompt": "go"}),
    )
    .await;
    assert!(structured(&result)["guidance"]
        .as_str()
        .unwrap()
        .starts_with("You will get notified when the prompted agent finishes"));
    // Dispatch, arm (inside the dispatch), then wait for the run start.
    assert_eq!(
        core.methods(),
        vec![
            "send_prompt_to_agent",
            "setup_finish_notification",
            "wait_for_agent_run_start"
        ]
    );
    assert_eq!(
        core.args_of("setup_finish_notification")[0],
        json!({"child": "child", "caller": CALLER, "waitForTurnStart": true})
    );
    assert_eq!(core.args_of("send_prompt_to_agent")[0]["source"], CALLER);

    // A steered or out-of-band dispatch returns without waiting for a turn start.
    for disposition in [PromptDisposition::Steered, PromptDisposition::OutOfBand] {
        core.with(|s| s.dispatch = Some(disposition));
        let before = core.methods().len();
        let result = run(
            &catalog,
            "send_agent_prompt",
            json!({"agentId": "child", "prompt": "go"}),
        )
        .await;
        assert_eq!(structured(&result)["disposition"], disposition.as_str());
        let methods = core.methods();
        assert_eq!(
            &methods[before..],
            ["send_prompt_to_agent", "setup_finish_notification"]
        );
    }
    assert_eq!(
        core.args_of("setup_finish_notification")[1]["waitForTurnStart"],
        false
    );

    // Opting out arms nothing.
    let before = core.args_of("setup_finish_notification").len();
    let result = run(
        &catalog,
        "send_agent_prompt",
        json!({"agentId": "child", "prompt": "go", "notifyOnFinish": false}),
    )
    .await;
    assert!(structured(&result)["guidance"]
        .as_str()
        .unwrap()
        .starts_with("Dispatch acknowledged."));
    assert_eq!(core.args_of("setup_finish_notification").len(), before);
}

#[tokio::test]
async fn send_prompt_stops_the_notification_when_dispatch_or_start_fails() {
    let core = caller_script();
    let catalog = core.catalog(caller_runtime()).await;
    core.with(|s| s.send_error = Some("dispatch failed".into()));
    let result = run(
        &catalog,
        "send_agent_prompt",
        json!({"agentId": "child", "prompt": "go"}),
    )
    .await;
    assert_eq!(error_text(&result), "dispatch failed");
    assert_eq!(
        core.stopped.lock().unwrap().len(),
        1,
        "armed inside the dispatch, stopped on its failure"
    );

    core.with(|s| {
        s.send_error = None;
        s.wait_error = Some("start timed out".into());
    });
    let result = run(
        &catalog,
        "send_agent_prompt",
        json!({"agentId": "child", "prompt": "go"}),
    )
    .await;
    assert_eq!(error_text(&result), "start timed out");
    assert_eq!(core.stopped.lock().unwrap().len(), 2);

    // A success leaves the notification running.
    core.with(|s| s.wait_error = None);
    run(
        &catalog,
        "send_agent_prompt",
        json!({"agentId": "child", "prompt": "go"}),
    )
    .await;
    assert_eq!(core.stopped.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn send_prompt_status_defaults_to_idle_when_the_agent_vanished() {
    let core = Script::new();
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(
        &catalog,
        "send_agent_prompt",
        json!({"agentId": "gone", "prompt": "go"}),
    )
    .await;
    assert_eq!(structured(&result)["status"], "idle");
}

// ---- list_agents ------------------------------------------------------------------------------

fn payload(
    id: &str,
    status: &str,
    cwd: &str,
    updated: &str,
    attention: bool,
    archived: Option<&str>,
) -> Value {
    json!({
        "id": id, "title": id, "provider": "claude", "model": null, "thinkingOptionId": null,
        "effectiveThinkingOptionId": null, "status": status, "cwd": cwd,
        "createdAt": "2026-01-01T00:00:00.000Z", "updatedAt": updated, "lastUserMessageAt": null,
        "archivedAt": archived, "requiresAttention": attention, "attentionReason": null,
        "attentionTimestamp": null, "labels": {},
    })
}

fn ids(result: &Value) -> Vec<String> {
    structured(result)["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn list_agents_sorts_by_attention_status_then_activity() {
    let core = Script::new()
        .caps(&[Capability::WorkspaceRegistry])
        .with(|s| {
            s.snapshots = vec![
                payload(
                    "idle-old",
                    "idle",
                    "/w",
                    "2026-01-31T10:00:00.000Z",
                    false,
                    None,
                ),
                payload(
                    "idle-new",
                    "idle",
                    "/w",
                    "2026-01-31T11:00:00.000Z",
                    false,
                    None,
                ),
                payload(
                    "running",
                    "running",
                    "/w",
                    "2026-01-31T09:00:00.000Z",
                    false,
                    None,
                ),
                payload(
                    "error",
                    "error",
                    "/w",
                    "2026-01-31T12:00:00.000Z",
                    false,
                    None,
                ),
                payload(
                    "closed",
                    "closed",
                    "/w",
                    "2026-01-31T12:30:00.000Z",
                    false,
                    None,
                ),
                payload(
                    "attention-closed",
                    "closed",
                    "/w",
                    "2026-01-01T00:00:00.000Z",
                    true,
                    None,
                ),
                payload(
                    "init",
                    "initializing",
                    "/w",
                    "2026-01-31T08:00:00.000Z",
                    false,
                    None,
                ),
            ];
        });
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(&catalog, "list_agents", json!({})).await;
    assert_eq!(
        ids(&result),
        [
            "attention-closed",
            "running",
            "init",
            "idle-new",
            "idle-old",
            "error",
            "closed"
        ]
    );
    // limit and statuses
    assert_eq!(
        ids(&run(&catalog, "list_agents", json!({"limit": 2})).await),
        ["attention-closed", "running"]
    );
    assert_eq!(
        ids(&run(
            &catalog,
            "list_agents",
            json!({"statuses": ["idle", "error"]})
        )
        .await),
        ["idle-new", "idle-old", "error"]
    );
    // The directory is asked for what the flags say.
    run(
        &catalog,
        "list_agents",
        json!({"includeBackground": true, "includeArchived": true}),
    )
    .await;
    assert_eq!(
        core.args_of("list_agent_snapshots").last().unwrap(),
        &json!([true, true])
    );
}

#[tokio::test]
async fn list_agents_scopes_to_the_callers_cwd_and_bounds_archived_agents() {
    let core = caller_script()
        .caps(&[Capability::WorkspaceRegistry])
        .with(|s| {
            s.snapshots = vec![
                payload(
                    "in",
                    "idle",
                    "/work/repo/sub",
                    "2026-01-31T11:00:00.000Z",
                    false,
                    None,
                ),
                payload(
                    "out",
                    "idle",
                    "/work/other",
                    "2026-01-31T11:00:00.000Z",
                    false,
                    None,
                ),
                payload(
                    "sibling-prefix",
                    "idle",
                    "/work/repo2",
                    "2026-01-31T11:00:00.000Z",
                    false,
                    None,
                ),
                payload(
                    "arch-old",
                    "closed",
                    "/work/repo",
                    "2026-01-01T00:00:00.000Z",
                    false,
                    Some("2026-01-10T00:00:00.000Z"),
                ),
                payload(
                    "arch-new",
                    "closed",
                    "/work/repo",
                    "2026-01-31T11:30:00.000Z",
                    false,
                    Some("2026-01-31T11:30:00.000Z"),
                ),
            ];
        });
    let catalog = core.catalog(caller_runtime()).await;
    // Default window is 48h before 2026-02-01T12:00Z: the old archived agent drops out.
    assert_eq!(
        ids(&run(&catalog, "list_agents", json!({"includeArchived": true})).await),
        ["in", "arch-new"]
    );
    // An explicit cwd overrides the caller's, `sinceHours` widens the window.
    let result = run(&catalog, "list_agents", json!({"cwd": "/work/other"})).await;
    assert_eq!(ids(&result), ["out"]);
    let result = run(
        &catalog,
        "list_agents",
        json!({"cwd": "/work/repo", "includeArchived": true, "sinceHours": 720}),
    )
    .await;
    assert_eq!(ids(&result), ["in", "arch-new", "arch-old"]);
}

#[tokio::test]
async fn list_agents_needs_the_workspace_registry() {
    let core = Script::new();
    let catalog = core.catalog(RuntimeCtx::default()).await;
    assert_eq!(
        error_text(&run(&catalog, "list_agents", json!({})).await),
        "Workspace registry is not configured"
    );
}

// ---- agent lifecycle -------------------------------------------------------------------------

#[tokio::test]
async fn update_agent_applies_settings_before_metadata() {
    let core = Script::new();
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(
        &catalog,
        "update_agent",
        json!({"agentId": "a", "name": " New ", "labels": {"k": "v"}, "settings": {"modeId": "plan", "model": null, "thinkingOptionId": "high", "features": {"f1": true, "f2": 2}}}),
    )
    .await;
    assert_eq!(structured(&result), &json!({"success": true}));
    assert_eq!(
        core.methods(),
        [
            "set_agent_mode",
            "set_agent_model",
            "set_agent_thinking_option",
            "set_agent_feature",
            "set_agent_feature",
            "update_agent_metadata"
        ]
    );
    assert_eq!(core.args_of("set_agent_model")[0], json!(["a", null]));
    assert_eq!(
        core.args_of("update_agent_metadata")[0],
        json!(["a", "New", {"k": "v"}])
    );
}

#[tokio::test]
async fn update_agent_without_changes_succeeds_and_failures_skip_metadata() {
    let core = Script::new();
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(
        &catalog,
        "update_agent",
        json!({"agentId": "a", "name": "  ", "labels": {}}),
    )
    .await;
    assert_eq!(structured(&result), &json!({"success": true}));
    assert!(core.calls().is_empty());

    core.with(|s| s.set_mode_error = Some("bad mode".into()));
    let result = run(
        &catalog,
        "update_agent",
        json!({"agentId": "a", "name": "x", "settings": {"modeId": "nope"}}),
    )
    .await;
    assert_eq!(error_text(&result), "bad mode");
    assert_eq!(core.methods(), ["set_agent_mode"]);
}

#[tokio::test]
async fn cancel_archive_kill_and_status() {
    let core = Script::new().with(|s| s.cancel_result = true);
    let catalog = core.catalog(RuntimeCtx::default()).await;
    assert_eq!(
        structured(&run(&catalog, "cancel_agent", json!({"agentId": "a"})).await),
        &json!({"success": true})
    );
    core.with(|s| s.cancel_result = false);
    assert_eq!(
        structured(&run(&catalog, "cancel_agent", json!({"agentId": "a"})).await),
        &json!({"success": false})
    );
    assert_eq!(
        structured(&run(&catalog, "archive_agent", json!({"agentId": "a"})).await),
        &json!({"success": true})
    );
    assert_eq!(
        structured(&run(&catalog, "kill_agent", json!({"agentId": "a"})).await),
        &json!({"success": true})
    );
    assert_eq!(
        core.methods(),
        [
            "cancel_agent_run",
            "cancel_agent_run",
            "archive_agent",
            "close_agent"
        ]
    );
    assert_eq!(
        error_text(&run(&catalog, "get_agent_status", json!({"agentId": "zzz"})).await),
        "Agent zzz not found"
    );
}

#[tokio::test]
async fn get_agent_activity_loads_the_agent_first_and_counts_projected_entries() {
    use paseo_agent_types::AgentTimelineItem;
    let message = |text: &str| AgentTimelineItem::AssistantMessage {
        text: text.into(),
        message_id: None,
    };
    let user = |text: &str| AgentTimelineItem::UserMessage {
        text: text.into(),
        message_id: None,
        client_message_id: None,
    };
    let core = Script::new().with(|s| {
        s.agents
            .insert("a".into(), agent("a", "claude", "/w", None));
        // Four raw assistant deltas collapse to one projected message.
        s.timeline = vec![
            user("one"),
            message("a "),
            message("b "),
            message("c "),
            message("d"),
            user("two"),
        ];
    });
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(
        &catalog,
        "get_agent_activity",
        json!({"agentId": "a", "limit": 2}),
    )
    .await;
    let out = structured(&result);
    assert_eq!(out["updateCount"], 6);
    assert_eq!(
        out["content"],
        "Showing 2 of 3 activities (limited to 2)\n\na b c d\n[User] two"
    );
    assert_eq!(core.methods()[0], "ensure_agent_loaded");
    // limit 0 or absent shows everything.
    let result = run(
        &catalog,
        "get_agent_activity",
        json!({"agentId": "a", "limit": 0}),
    )
    .await;
    assert!(structured(&result)["content"]
        .as_str()
        .unwrap()
        .starts_with("Showing all 3 activities\n\n"));
}

// ---- schedules ---------------------------------------------------------------------------------

fn new_agent_schedule() -> Value {
    json!({"id": "s1", "target": {"type": "new-agent", "config": {"provider": "claude", "cwd": "/w"}}, "runs": []})
}

fn schedule_script() -> Arc<Script> {
    Script::new().caps(&[Capability::Schedules]).with(|s| {
        s.schedules.insert("s1".into(), new_agent_schedule());
        s.schedules.insert("hb".into(), json!({"id": "hb", "target": {"type": "agent", "agentId": CALLER}, "runs": []}));
        s.schedules.insert("other-hb".into(), json!({"id": "other-hb", "target": {"type": "agent", "agentId": "someone-else"}, "runs": []}));
        s.agents.insert(CALLER.into(), {
            let mut a = agent(CALLER, "claude", "/work/repo", Some("wks_caller"));
            a.config = AgentConfigInfo {
                model: Some("opus".into()),
                thinking_option_id: Some("high".into()),
                provider_options: Some(json!({"effort": "max"})),
                feature_values: Some(json!({"fast": true})),
                system_prompt: Some("be brief".into()),
                mcp_servers: Some(json!({"x": {}})),
            };
            a
        });
    })
}

#[tokio::test]
async fn create_schedule_top_level_builds_the_target_and_expiry() {
    let core = schedule_script();
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(
        &catalog,
        "create_schedule",
        json!({"prompt": "p", "cron": " 0 2 * * * ", "timezone": " UTC ", "provider": "codex/gpt-5", "isolation": "worktree", "maxRuns": 3, "expiresIn": "1h", "name": "  "}),
    )
    .await;
    structured(&result);
    assert_eq!(
        core.args_of("schedule_create_or_replace")[0],
        json!({
            "prompt": "p",
            "cadence": {"type": "cron", "expression": "0 2 * * *", "timezone": "UTC"},
            "target": {"type": "new-agent", "config": {"provider": "codex", "cwd": "/proc/cwd", "model": "gpt-5", "isolation": "worktree"}},
            "maxRuns": 3,
            "expiresAt": "2026-02-01T13:00:00.000Z",
        })
    );
    // The summary drops runs; key order of the request is part of the contract.
    assert_eq!(
        serde_json::to_string(&core.args_of("schedule_create_or_replace")[0]).unwrap(),
        r#"{"prompt":"p","cadence":{"type":"cron","expression":"0 2 * * *","timezone":"UTC"},"target":{"type":"new-agent","config":{"provider":"codex","cwd":"/proc/cwd","model":"gpt-5","isolation":"worktree"}},"maxRuns":3,"expiresAt":"2026-02-01T13:00:00.000Z"}"#
    );
    assert!(structured(&result).get("runs").is_none());

    let result = run(
        &catalog,
        "create_schedule",
        json!({"prompt": "p", "cron": "* * * * *", "provider": "claude", "expiresIn": "soon"}),
    )
    .await;
    assert_eq!(
        error_text(&result),
        "Invalid duration format: soon. Use formats like: 5m, 30s, 1h, 2h30m"
    );
    let result = run(
        &catalog,
        "create_schedule",
        json!({"prompt": "p", "cron": "* * * * *", "provider": "claude/"}),
    )
    .await;
    assert_eq!(
        error_text(&result),
        "provider must be <provider> or <provider>/<model>"
    );
}

#[tokio::test]
async fn create_schedule_for_a_caller_inherits_its_configuration() {
    let core = schedule_script();
    let catalog = core.catalog(caller_runtime()).await;
    run(
        &catalog,
        "create_schedule",
        json!({"prompt": "p", "cron": "* * * * *", "isolation": "local"}),
    )
    .await;
    let target = &core.args_of("schedule_create_or_replace")[0]["target"];
    assert_eq!(
        serde_json::to_string(target).unwrap(),
        r#"{"type":"new-agent","config":{"provider":"claude","cwd":"/work/repo","modeId":"default","model":"opus","thinkingOptionId":"high","providerOptions":{"effort":"max"},"featureValues":{"fast":true},"systemPrompt":"be brief","mcpServers":{"x":{}},"isolation":"local"}}"#
    );
    // Another provider drops the mode, the model and the provider options.
    run(
        &catalog,
        "create_schedule",
        json!({"prompt": "p", "cron": "* * * * *", "provider": "codex", "cwd": "sub/../x"}),
    )
    .await;
    let config = &core.args_of("schedule_create_or_replace")[1]["target"]["config"];
    assert_eq!(config["provider"], "codex");
    assert!(
        config.get("modeId").is_none()
            && config.get("model").is_none()
            && config.get("providerOptions").is_none()
    );
    assert_eq!(config["cwd"], expand("sub/../x"));
    assert_eq!(config["thinkingOptionId"], "high");
    // provider/model override.
    run(
        &catalog,
        "create_schedule",
        json!({"prompt": "p", "cron": "* * * * *", "provider": "claude/haiku"}),
    )
    .await;
    assert_eq!(
        core.args_of("schedule_create_or_replace")[2]["target"]["config"]["model"],
        "haiku"
    );
}

fn expand(path: &str) -> String {
    paseo_tools::util::expand_user_path(path, "/proc/cwd")
}

#[tokio::test]
async fn heartbeats_belong_to_their_caller() {
    let core = schedule_script();
    let catalog = core.catalog(caller_runtime()).await;
    let result = run(
        &catalog,
        "create_heartbeat",
        json!({"prompt": "ping", "cron": "*/5 * * * *", "timezone": "Europe/Paris", "maxRuns": 2}),
    )
    .await;
    structured(&result);
    assert_eq!(
        core.args_of("schedule_create_or_replace")[0],
        json!({"prompt": "ping", "cadence": {"type": "cron", "expression": "*/5 * * * *", "timezone": "Europe/Paris"}, "target": {"type": "agent", "agentId": CALLER}, "maxRuns": 2})
    );
    assert_eq!(
        structured(&run(&catalog, "delete_heartbeat", json!({"id": "hb"})).await),
        &json!({"success": true})
    );
    assert_eq!(core.args_of("schedule_delete")[0], json!(["hb"]));
    assert_eq!(
        error_text(&run(&catalog, "delete_heartbeat", json!({"id": "other-hb"})).await),
        format!("Heartbeat other-hb does not belong to caller {CALLER}")
    );
    assert_eq!(
        error_text(&run(&catalog, "delete_heartbeat", json!({"id": "s1"})).await),
        "Heartbeat not found: s1"
    );

    let anonymous = schedule_script().catalog(RuntimeCtx::default()).await;
    assert_eq!(
        error_text(&run(&anonymous, "delete_heartbeat", json!({"id": "hb"})).await),
        "Heartbeat operations require an agent-scoped session"
    );
    assert_eq!(
        error_text(
            &run(
                &anonymous,
                "create_heartbeat",
                json!({"prompt": "p", "cron": "* * * * *"})
            )
            .await
        ),
        "create_heartbeat requires an agent-scoped session"
    );
}

#[tokio::test]
async fn schedule_tools_only_see_new_agent_schedules() {
    let core = schedule_script();
    let catalog = core.catalog(RuntimeCtx::default()).await;
    for tool in [
        "inspect_schedule",
        "pause_schedule",
        "resume_schedule",
        "delete_schedule",
        "schedule_logs",
        "run_schedule_once",
    ] {
        assert_eq!(
            error_text(&run(&catalog, tool, json!({"id": "hb"})).await),
            "Schedule not found: hb",
            "{tool}"
        );
    }
    let listed = run(&catalog, "list_schedules", json!({})).await;
    assert_eq!(
        structured(&listed)["schedules"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        structured(&run(&catalog, "schedule_logs", json!({"id": "s1"})).await),
        &json!({"runs": [{"id": "run-1"}]})
    );
}

#[tokio::test]
async fn schedule_tools_without_a_service() {
    let core = Script::new();
    let catalog = core.catalog(RuntimeCtx::default()).await;
    for (tool, args) in [
        (
            "create_schedule",
            json!({"prompt": "p", "cron": "* * * * *", "provider": "claude"}),
        ),
        ("list_schedules", json!({})),
        ("inspect_schedule", json!({"id": "x"})),
        ("update_schedule", json!({"id": "x"})),
        ("schedule_logs", json!({"id": "x"})),
    ] {
        assert_eq!(
            error_text(&run(&catalog, tool, args).await),
            "Schedule service is not configured",
            "{tool}"
        );
    }
}

// COMPAT(scheduleEveryInput)
#[tokio::test]
async fn compat_update_schedule_every_becomes_cron() {
    let core = schedule_script();
    let catalog = core.catalog(RuntimeCtx::default()).await;
    run(
        &catalog,
        "update_schedule",
        json!({"id": "s1", "every": " 5m ", "cron": "  "}),
    )
    .await;
    run(
        &catalog,
        "update_schedule",
        json!({"id": "s1", "every": "2h"}),
    )
    .await;
    run(
        &catalog,
        "update_schedule",
        json!({"id": "s1", "every": "24h", "name": null}),
    )
    .await;
    let updates = core.args_of("schedule_update");
    assert_eq!(
        updates[0],
        json!({"id": "s1", "cadence": {"type": "cron", "expression": "*/5 * * * *"}})
    );
    assert_eq!(
        updates[1]["cadence"],
        json!({"type": "cron", "expression": "0 */2 * * *"})
    );
    assert_eq!(
        updates[2],
        json!({"id": "s1", "name": null, "cadence": {"type": "cron", "expression": "0 0 * * *"}})
    );

    assert_eq!(
        error_text(
            &run(
                &catalog,
                "update_schedule",
                json!({"id": "s1", "every": "7m"})
            )
            .await
        ),
        "7m cannot be represented faithfully by five-field cron"
    );
    assert_eq!(
        error_text(
            &run(
                &catalog,
                "update_schedule",
                json!({"id": "s1", "every": "5m", "cron": "* * * * *"})
            )
            .await
        ),
        "Specify at most one of every or cron"
    );
    assert_eq!(
        error_text(
            &run(
                &catalog,
                "update_schedule",
                json!({"id": "s1", "every": "5m", "timezone": "UTC"})
            )
            .await
        ),
        "timezone can only be used with cron"
    );
    assert_eq!(
        error_text(&run(&catalog, "update_schedule", json!({"id": "s1", "every": 5})).await),
        "value.trim is not a function"
    );
}

#[tokio::test]
async fn update_schedule_cadence_expiry_and_agent_config() {
    let core = schedule_script();
    let catalog = core.catalog(RuntimeCtx::default()).await;
    run(&catalog, "update_schedule", json!({"id": "s1", "cron": "0 3 * * *", "timezone": "UTC", "prompt": "new", "maxRuns": null, "provider": "codex", "model": " gpt ", "mode": null, "cwd": "/new", "expiresIn": "30m"})).await;
    run(
        &catalog,
        "update_schedule",
        json!({"id": "s1", "clearExpires": true, "model": null}),
    )
    .await;
    run(
        &catalog,
        "update_schedule",
        json!({"id": "s1", "provider": "claude/opus"}),
    )
    .await;
    run(
        &catalog,
        "update_schedule",
        json!({"id": "s1", "provider": "claude/opus", "model": "opus"}),
    )
    .await;
    let updates = core.args_of("schedule_update");
    assert_eq!(
        serde_json::to_string(&updates[0]).unwrap(),
        r#"{"id":"s1","prompt":"new","cadence":{"type":"cron","expression":"0 3 * * *","timezone":"UTC"},"maxRuns":null,"expiresAt":"2026-02-01T12:30:00.000Z","newAgentConfig":{"provider":"codex","model":"gpt","modeId":null,"cwd":"/new"}}"#
    );
    assert_eq!(
        updates[1],
        json!({"id": "s1", "expiresAt": null, "newAgentConfig": {"model": null}})
    );
    assert_eq!(
        updates[2]["newAgentConfig"],
        json!({"provider": "claude", "model": "opus"})
    );
    assert_eq!(
        updates[3]["newAgentConfig"],
        json!({"provider": "claude", "model": "opus"})
    );

    for (args, message) in [
        (
            json!({"id": "s1", "provider": "claude/opus", "model": null}),
            "provider specifies a model but model is null",
        ),
        (
            json!({"id": "s1", "provider": "claude/opus", "model": "haiku"}),
            "Conflicting model values provided",
        ),
        (json!({"id": "s1", "model": "  "}), "MCP error -32602"),
        (
            json!({"id": "s1", "expiresIn": "1h", "clearExpires": true}),
            "Specify at most one of expiresIn or clearExpires",
        ),
        (
            json!({"id": "s1", "provider": "x/ "}),
            "provider must be <provider> or <provider>/<model>",
        ),
        (
            json!({"id": "s1", "timezone": "UTC"}),
            "timezone can only be used with cron",
        ),
    ] {
        let result = run(&catalog, "update_schedule", args.clone()).await;
        assert!(
            error_text(&result).starts_with(message),
            "{args}: {}",
            error_text(&result)
        );
    }
}

// ---- workspaces ---------------------------------------------------------------------------------

#[tokio::test]
async fn create_workspace_local_requires_an_existing_directory_and_provisioning() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap().to_owned();
    let core = Script::new();
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let missing = format!("{path}/missing");
    assert_eq!(
        error_text(
            &run(
                &catalog,
                "create_workspace",
                json!({"isolation": "local", "path": missing})
            )
            .await
        ),
        format!("Directory not found: {missing}")
    );
    assert_eq!(
        error_text(
            &run(
                &catalog,
                "create_workspace",
                json!({"isolation": "local", "path": path, "mode": "branch-off"})
            )
            .await
        ),
        "Worktree options require isolation worktree"
    );
    assert_eq!(
        error_text(
            &run(
                &catalog,
                "create_workspace",
                json!({"isolation": "local", "path": path})
            )
            .await
        ),
        "Workspace provisioning is not configured"
    );
    assert_eq!(
        error_text(&run(&catalog, "create_workspace", json!({"isolation": "local"})).await),
        "cwd is required"
    );

    let core = Script::new().caps(&[Capability::DirectoryWorkspaces]);
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(&catalog, "create_workspace", json!({"isolation": "local", "path": path, "title": " T ", "projectId": "prj_9", "background": true})).await;
    let out = structured(&result);
    assert_eq!(out["isolation"], "local");
    assert_eq!(out["background"], true);
    assert_eq!(
        core.args_of("create_directory_workspace")[0],
        json!([path, "T", "prj_9", {"background": true, "callerWorkspaceId": null}])
    );
}

#[tokio::test]
async fn create_workspace_worktree_modes() {
    let core = Script::new()
        .caps(&[Capability::ProjectRegistry])
        .with(|s| {
            s.created_workspace = Some({
                let mut w = workspace("wks_wt", "/wt", false, false);
                w.kind = WorkspaceKind::Worktree;
                w
            });
        });
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(&catalog, "create_workspace", json!({"isolation": "worktree", "path": "/repo", "worktreeSlug": "s", "branchName": "b", "baseBranch": "main", "title": "T"})).await;
    assert_eq!(structured(&result)["isolation"], "worktree");
    run(&catalog, "create_workspace", json!({"isolation": "worktree", "path": "/repo", "mode": "checkout-branch", "branch": "dev"})).await;
    run(&catalog, "create_workspace", json!({"isolation": "worktree", "path": "/repo", "mode": "checkout-pr", "prNumber": 7, "forge": "github"})).await;
    // A project without a path resolves its source checkout.
    run(
        &catalog,
        "create_workspace",
        json!({"isolation": "worktree", "projectId": "prj_1"}),
    )
    .await;
    let requests = core.args_of("create_paseo_worktree");
    assert_eq!(
        requests[0],
        json!({"cwd": "/repo", "projectId": null, "worktreeSlug": "s", "action": "branch-off", "branchName": "b", "refName": "main", "checkoutSource": null, "title": "T", "background": null, "callerWorkspaceId": null})
    );
    assert_eq!(requests[1]["action"], "checkout");
    assert_eq!(requests[1]["refName"], "dev");
    assert_eq!(
        requests[2]["checkoutSource"],
        json!({"kind": "change_request", "forge": "github", "number": 7})
    );
    assert_eq!(requests[3]["cwd"], "/projects/root");
    assert_eq!(requests[3]["projectId"], "prj_1");

    for (args, message) in [
        (
            json!({"isolation": "worktree", "path": "/r", "mode": "checkout-branch"}),
            "branch is required for checkout-branch mode",
        ),
        (
            json!({"isolation": "worktree", "path": "/r", "mode": "checkout-pr"}),
            "prNumber is required for checkout-pr mode",
        ),
        (
            json!({"isolation": "worktree", "path": "/r", "branch": "x"}),
            "branch, prNumber, and forge require a checkout mode",
        ),
        (
            json!({"isolation": "worktree", "path": "/r", "mode": "checkout-branch", "branch": "x", "baseBranch": "y"}),
            "branchName, baseBranch, prNumber, and forge are not valid for checkout-branch mode",
        ),
        (
            json!({"isolation": "worktree", "path": "/r", "mode": "checkout-pr", "prNumber": 1, "branch": "y"}),
            "branchName, baseBranch, and branch are not valid for checkout-pr mode",
        ),
    ] {
        assert_eq!(
            error_text(&run(&catalog, "create_workspace", args).await),
            message
        );
    }
    let core = Script::new();
    let catalog = core.catalog(RuntimeCtx::default()).await;
    assert_eq!(
        error_text(
            &run(
                &catalog,
                "create_workspace",
                json!({"isolation": "worktree", "projectId": "p"})
            )
            .await
        ),
        "Project registry is not configured"
    );
}

#[tokio::test]
async fn rename_workspace_rules() {
    let core = caller_script()
        .caps(&[Capability::WorkspaceRegistry, Capability::WorkspaceUpdates])
        .with(|s| {
            s.workspaces = vec![
                workspace("wks_caller", "/work/repo", false, false),
                workspace("wks_old", "/old", false, true),
            ];
        });
    let catalog = core.catalog(caller_runtime()).await;
    let result = run(&catalog, "rename_workspace", json!({"title": " Fresh "})).await;
    assert_eq!(
        structured(&result),
        &json!({"success": true, "workspaceId": "wks_caller", "title": "Fresh"})
    );
    assert_eq!(
        core.args_of("upsert_workspace")[0],
        json!(["wks_caller", "Fresh", "2026-02-01T12:00:00.000Z"])
    );
    assert_eq!(
        core.args_of("emit_workspace_updates")[0],
        json!(["wks_caller"])
    );
    assert_eq!(
        error_text(
            &run(
                &catalog,
                "rename_workspace",
                json!({"workspaceId": "wks_old", "title": "x"})
            )
            .await
        ),
        "Workspace wks_old is archived"
    );
    assert_eq!(
        error_text(
            &run(
                &catalog,
                "rename_workspace",
                json!({"workspaceId": "nope", "title": "x"})
            )
            .await
        ),
        "Workspace nope not found"
    );

    let anonymous = core.catalog(RuntimeCtx::default()).await;
    assert_eq!(
        error_text(&run(&anonymous, "rename_workspace", json!({"title": "x"})).await),
        "workspaceId is required outside an agent-scoped session"
    );
    let bare = Script::new().caps(&[Capability::WorkspaceRegistry]);
    let catalog = bare.catalog(RuntimeCtx::default()).await;
    assert_eq!(
        error_text(&run(&catalog, "rename_workspace", json!({"title": "x"})).await),
        "Workspace update emitter is required to rename workspaces"
    );
    let none = Script::new();
    let catalog = none.catalog(RuntimeCtx::default()).await;
    assert_eq!(
        error_text(&run(&catalog, "rename_workspace", json!({"title": "x"})).await),
        "Workspace registry is required to rename workspaces"
    );
}

#[tokio::test]
async fn archive_workspace_and_scripts() {
    let core = Script::new();
    let catalog = core.catalog(RuntimeCtx::default()).await;
    assert_eq!(
        error_text(&run(&catalog, "archive_workspace", json!({"workspaceId": "w"})).await),
        "Active workspace lister is required to archive workspaces"
    );
    assert_eq!(
        error_text(
            &run(
                &catalog,
                "list_workspace_scripts",
                json!({"workspaceId": "w"})
            )
            .await
        ),
        "Workspace script management is not configured"
    );

    let core = Script::new()
        .caps(&[Capability::ActiveWorkspaces, Capability::WorkspaceScripts])
        .with(|s| {
            s.active.push(ActiveWorkspaceRef {
                workspace_id: "w".into(),
                cwd: "/w".into(),
            });
        });
    let catalog = core.catalog(RuntimeCtx::default()).await;
    assert_eq!(
        error_text(&run(&catalog, "archive_workspace", json!({"workspaceId": "x"})).await),
        "Workspace not found: x"
    );
    assert_eq!(
        structured(&run(&catalog, "archive_workspace", json!({"workspaceId": "w"})).await),
        &json!({"workspaceId": "w", "archivedAgentIds": ["a1"], "removedDirectory": true})
    );
    assert_eq!(
        structured(
            &run(
                &catalog,
                "list_workspace_scripts",
                json!({"workspaceId": "w"})
            )
            .await
        ),
        &json!({"scripts": [{"scriptName": "dev"}]})
    );
    assert_eq!(
        structured(
            &run(
                &catalog,
                "start_workspace_script",
                json!({"workspaceId": "w", "scriptName": "dev"})
            )
            .await
        )["script"]["lifecycle"],
        "running"
    );
    assert_eq!(
        structured(
            &run(
                &catalog,
                "stop_workspace_script",
                json!({"workspaceId": "w", "scriptName": "dev"})
            )
            .await
        )["script"]["lifecycle"],
        "stopped"
    );
}

// ---- terminals ---------------------------------------------------------------------------------

#[tokio::test]
async fn terminals_use_the_callers_cwd_and_workspace() {
    let core = caller_script().caps(&[Capability::Terminals]).with(|s| {
        s.terminals = vec![TerminalSummary {
            id: "t1".into(),
            name: "main".into(),
            cwd: "/work/repo".into(),
        }];
    });
    let catalog = core.catalog(caller_runtime()).await;
    assert_eq!(
        structured(&run(&catalog, "list_terminals", json!({})).await)["terminals"][0]["id"],
        "t1"
    );
    run(&catalog, "create_terminal", json!({"name": " build "})).await;
    assert_eq!(
        core.args_of("create_terminal")[0],
        json!(["/work/repo", "wks_caller", "build"])
    );
    // A custom cwd resolves against the caller's.
    run(&catalog, "create_terminal", json!({"cwd": "sub"})).await;
    assert_eq!(
        core.args_of("create_terminal")[1],
        json!(["/work/repo/sub", "wks_caller", null])
    );

    // No workspace on the caller: minting, or the TS messages.
    let core = Script::new().caps(&[Capability::Terminals]).with(|s| {
        s.agents
            .insert(CALLER.into(), agent(CALLER, "claude", "/w", None));
    });
    let catalog = core.catalog(caller_runtime()).await;
    assert_eq!(
        error_text(&run(&catalog, "create_terminal", json!({})).await),
        format!("Caller agent {CALLER} has no workspace and workspace minting is not configured")
    );
    let core = core.caps(&[Capability::WorkspaceMinting]);
    run(
        &core.catalog(caller_runtime()).await,
        "create_terminal",
        json!({}),
    )
    .await;
    assert_eq!(
        core.args_of("ensure_workspace_for_create")[0],
        json!(["/w", null, null])
    );

    let none = Script::new();
    let catalog = none.catalog(RuntimeCtx::default()).await;
    assert_eq!(
        error_text(&run(&catalog, "kill_terminal", json!({"terminalId": "t"})).await),
        "Terminal manager is not configured"
    );
}

// ---- providers --------------------------------------------------------------------------------

fn provider(name: &str, enabled: bool, status: &str, error: Option<&str>) -> ProviderEntry {
    ProviderEntry {
        provider: name.into(),
        enabled,
        status: status.into(),
        error: error.map(str::to_owned),
        modes: Some(vec![json!({"id": "default"})]),
        ..ProviderEntry::default()
    }
}

#[tokio::test]
async fn inspect_provider_builds_the_draft_request() {
    let core = Script::new().with(|s| {
        s.providers = vec![
            provider("claude", true, "ready", None),
            provider("off", false, "ready", None),
            provider("down", true, "unavailable", Some("no binary")),
            provider("down2", true, "unavailable", None),
        ];
    });
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(&catalog, "inspect_provider", json!({"provider": " claude/opus ", "cwd": "/w", "settings": {"modeId": "plan", "model": "haiku", "thinkingOptionId": "low", "features": {"f": 1}}})).await;
    let out = structured(&result);
    assert_eq!(out["selectedModel"], "haiku");
    assert_eq!(out["label"], "claude");
    assert_eq!(out["description"], "");
    assert_eq!(
        core.args_of("list_draft_features")[0],
        json!({"provider": "claude", "cwd": "/w", "modeId": "plan", "model": "haiku", "thinkingOptionId": "low", "featureValues": {"f": 1}})
    );
    // The model of provider/model is the default selection.
    run(
        &catalog,
        "inspect_provider",
        json!({"provider": "claude/opus", "cwd": "/w"}),
    )
    .await;
    assert_eq!(
        core.args_of("list_draft_features")[1],
        json!({"provider": "claude", "cwd": "/w", "model": "opus"})
    );

    assert_eq!(
        error_text(
            &run(
                &catalog,
                "inspect_provider",
                json!({"provider": "off", "cwd": "/w"})
            )
            .await
        ),
        "Provider 'off' is disabled"
    );
    assert_eq!(
        error_text(
            &run(
                &catalog,
                "inspect_provider",
                json!({"provider": "down", "cwd": "/w"})
            )
            .await
        ),
        "no binary"
    );
    assert_eq!(
        error_text(
            &run(
                &catalog,
                "inspect_provider",
                json!({"provider": "down2", "cwd": "/w"})
            )
            .await
        ),
        "Provider 'down2' is unavailable"
    );
    assert_eq!(
        error_text(
            &run(
                &catalog,
                "inspect_provider",
                json!({"provider": "ghost", "cwd": "/w"})
            )
            .await
        ),
        "Provider ghost is not configured"
    );
    assert_eq!(
        error_text(&run(&catalog, "inspect_provider", json!({"provider": "claude"})).await),
        "cwd is required"
    );
    let bad = run(
        &catalog,
        "inspect_provider",
        json!({"provider": "a/", "cwd": "/w"}),
    )
    .await;
    assert!(error_text(&bad)
        .contains("provider must be provider or provider/model, for example codex/gpt-5.4"));
}

#[tokio::test]
async fn list_providers_summaries() {
    let core = Script::new().with(|s| {
        let mut errored = provider("codex", true, "error", Some("boom"));
        errored.modes = None;
        errored.label = Some("Codex".into());
        s.providers = vec![provider("claude", true, "ready", None), errored];
    });
    let catalog = core.catalog(RuntimeCtx::default()).await;
    let result = run(&catalog, "list_providers", json!({})).await;
    assert_eq!(
        serde_json::to_string(&structured(&result)["providers"]).unwrap(),
        r#"[{"id":"claude","label":"claude","description":"","enabled":true,"modes":[{"id":"default"}],"status":"available"},{"id":"codex","label":"Codex","description":"","enabled":true,"modes":[],"status":"error","error":"boom"}]"#
    );
}

// ---- speak, policy, voice ------------------------------------------------------------------------

#[tokio::test]
async fn speak_is_gated_and_scoped_to_agent_sessions() {
    let core = caller_script().with(|s| s.speak_handler = false);
    // Not registered unless voice is on.
    assert!(core
        .catalog(caller_runtime())
        .await
        .get_tool("speak")
        .is_none());
    let voice = RuntimeCtx {
        enable_voice: true,
        ..caller_runtime()
    };
    let catalog = core.catalog(voice.clone()).await;
    assert_eq!(
        error_text(&run(&catalog, "speak", json!({"text": "hi"})).await),
        format!("No speak handler registered for your session '{CALLER}'")
    );
    core.with(|s| s.speak_handler = true);
    structured(&run(&catalog, "speak", json!({"text": "hi"})).await);
    assert_eq!(core.args_of("speak")[0], json!([CALLER, "hi"]));
    // Without a caller agent.
    let anonymous = core
        .catalog(RuntimeCtx {
            voice_only: true,
            ..RuntimeCtx::default()
        })
        .await;
    assert_eq!(
        error_text(&run(&anonymous, "speak", json!({"text": "hi"})).await),
        "speak is only available to agent-scoped tool sessions"
    );
}

#[tokio::test]
async fn policy_gates_tools_and_browser_needs_a_broker() {
    let core = Script::new();
    let policy = |p: PaseoToolPolicy| RuntimeCtx {
        policy: Some(p),
        ..RuntimeCtx::default()
    };
    let catalog = core
        .catalog(policy(PaseoToolPolicy {
            enabled: None,
            disabled_tools: vec!["list_agents".into(), "speak".into()],
        }))
        .await;
    assert!(catalog.get_tool("list_agents").is_none());
    assert!(catalog.get_tool("list_models").is_some());
    // `speak` ignores the disabled list; a disabled policy keeps only it.
    let all_off = core
        .catalog(RuntimeCtx {
            enable_voice: true,
            ..policy(PaseoToolPolicy::disabled())
        })
        .await;
    assert_eq!(all_off.names(), ["speak"]);
    assert_eq!(
        error_text(&run(&all_off, "list_providers", json!({})).await),
        "MCP error -32602: Tool list_providers not found"
    );
    // Direct execution of a hidden tool.
    let err = all_off
        .execute_tool("list_providers", Some(&json!({})), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "Paseo tool not found: list_providers");

    // Browser tools: only with the broker capability, and still subject to the policy.
    assert!(core
        .catalog(RuntimeCtx::default())
        .await
        .get_tool("browser_snapshot")
        .is_none());
    let browser = Script::new().caps(&[Capability::BrowserBroker]);
    assert!(browser
        .catalog(RuntimeCtx::default())
        .await
        .get_tool("browser_snapshot")
        .is_some());
    let trimmed = browser
        .catalog(policy(PaseoToolPolicy {
            enabled: None,
            disabled_tools: vec!["browser_snapshot".into()],
        }))
        .await;
    assert!(trimmed.get_tool("browser_snapshot").is_none());
    assert!(trimmed.get_tool("browser_click").is_some());
}

// ---- browser --------------------------------------------------------------------------------------

const TAB: &str = "00000000-0000-4000-8000-000000000000";

#[tokio::test]
async fn browser_results_render_text_and_structured_content() {
    let core = Script::new().caps(&[Capability::BrowserBroker]).with(|s| {
        s.agents.insert(
            CALLER.into(),
            agent(CALLER, "claude", "/work/repo", Some("wks_caller")),
        );
    });
    let catalog = core.catalog(caller_runtime()).await;
    let exec = |payload: Value| core.with(|s| s.browser_payload = payload);

    exec(
        json!({"ok": true, "result": {"command": "screenshot", "dataBase64": "AAAA", "mimeType": "image/png", "width": 10, "height": 20}}),
    );
    let result = run(&catalog, "browser_screenshot", json!({"browserId": TAB})).await;
    assert_eq!(
        result["content"][0],
        json!({"type": "text", "text": "Captured browser screenshot (10x20)."})
    );
    assert_eq!(
        result["content"][1],
        json!({"type": "image", "data": "AAAA", "mimeType": "image/png"})
    );
    let result_structured = structured(&result);
    assert!(result_structured["result"].get("dataBase64").is_none());
    assert_eq!(
        result_structured["context"],
        json!({"agentId": CALLER, "cwd": "/work/repo", "workspaceId": "wks_caller", "browserId": TAB})
    );
    assert_eq!(
        core.args_of("browser_execute")[0],
        json!({"agentId": CALLER, "cwd": "/work/repo", "workspaceId": "wks_caller", "command": {"command": "screenshot", "args": {"browserId": TAB, "fullPage": false}}})
    );

    exec(
        json!({"ok": true, "result": {"command": "list_tabs", "tabs": [{"browserId": TAB, "title": "", "url": "http://a", "isActive": true}, {"browserId": "t2", "title": "Hi \"x\"", "url": "http://b"}]}, "dialogs": [{"action": "accept", "type": "alert", "message": "hey"}]}),
    );
    let result = run(&catalog, "browser_list_tabs", json!({})).await;
    assert_eq!(
        result["content"][0]["text"],
        format!("Found 2 Paseo browser tabs. Use these browserId values for tab-scoped browser tools.\n- browserId={TAB} active title=\"Untitled\" url=http://a\n- browserId=t2 title=\"Hi \\\"x\\\"\" url=http://b\nHandled browser dialog: accept alert \"hey\".")
    );
    assert_eq!(structured(&result)["dialogs"][0]["message"], "hey");

    exec(
        json!({"ok": true, "result": {"command": "list_tabs", "tabs": []}, "dialogs": [{"action": "accept", "type": "alert", "message": "hey"}]}),
    );
    let result = run(&catalog, "browser_list_tabs", json!({})).await;
    assert_eq!(
        result["content"][0]["text"],
        "No Paseo browser tabs are open. Call browser_new_tab to create one."
    );

    exec(
        json!({"ok": true, "result": {"command": "snapshot", "title": "", "url": "http://a", "snapshot": "- root\n  - @e1", "stats": {"nodeCount": 1, "refCount": 2}}}),
    );
    let result = run(&catalog, "browser_snapshot", json!({"browserId": TAB})).await;
    assert_eq!(
        result["content"][0]["text"],
        "Snapshot captured 1 node with 2 refs.\nTitle: Untitled\nURL: http://a\n\n- root\n  - @e1"
    );

    exec(
        json!({"ok": true, "result": {"command": "evaluate", "resultJson": "42", "truncated": true}}),
    );
    let result = run(
        &catalog,
        "browser_evaluate",
        json!({"browserId": TAB, "function": "() => 42"}),
    )
    .await;
    assert_eq!(
        result["content"][0]["text"],
        "Browser evaluate returned:\n42\nResult was truncated."
    );

    for (payload, text) in [
        (
            json!({"code": "browser_disabled", "message": "x"}),
            "Browser tools are disabled. Enable browser tools on the host, then try again.",
        ),
        (
            json!({"code": "browser_timeout", "message": "x"}),
            "The browser did not respond before the timeout. Try again or check the browser host.",
        ),
        (
            json!({"code": "browser_stale_ref", "message": "x"}),
            "That browser element reference is stale. Take a new browser snapshot and try again.",
        ),
        (
            json!({"code": "browser_no_host", "message": "No host connected"}),
            "No host connected",
        ),
        (json!({"code": "other", "message": "custom"}), "custom"),
    ] {
        exec(json!({"ok": false, "error": payload, "requestId": "r"}));
        let result = run(&catalog, "browser_back", json!({"browserId": TAB})).await;
        assert_eq!(result["content"][0]["text"], text);
        assert_eq!(structured(&result)["ok"], false);
    }
}

#[tokio::test]
async fn browser_commands_and_workspace_requirement() {
    let core = Script::new().caps(&[Capability::BrowserBroker]);
    let catalog = core.catalog(RuntimeCtx::default()).await;
    // Without a caller workspace the list/new tools answer without asking the broker.
    for tool in ["browser_list_tabs", "browser_new_tab"] {
        let result = run(&catalog, tool, json!({})).await;
        assert_eq!(structured(&result)["error"]["code"], "browser_denied");
        assert_eq!(structured(&result)["context"], json!({}));
    }
    assert!(core.args_of("browser_execute").is_empty());

    // Tab-scoped tools run without a workspace.
    run(
        &catalog,
        "browser_wait",
        json!({"browserId": TAB, "text": "hi", "timeoutMs": 2000}),
    )
    .await;
    run(
        &catalog,
        "browser_click",
        json!({"browserId": TAB, "ref": "@e3"}),
    )
    .await;
    run(
        &catalog,
        "browser_navigate",
        json!({"browserId": TAB, "url": "example.com"}),
    )
    .await;
    run(
        &catalog,
        "browser_scroll",
        json!({"browserId": TAB, "deltaX": 0, "deltaY": 120}),
    )
    .await;
    run(&catalog, "browser_logs", json!({"browserId": TAB})).await;
    let calls = core.args_of("browser_execute");
    assert_eq!(
        calls[0],
        json!({"timeoutMs": 3000, "command": {"command": "wait", "args": {"browserId": TAB, "text": "hi", "timeoutMs": 2000}}})
    );
    assert_eq!(
        calls[1]["command"]["args"],
        json!({"browserId": TAB, "ref": "@e3", "button": "left", "doubleClick": false, "modifiers": []})
    );
    assert_eq!(
        calls[2]["command"]["args"],
        json!({"browserId": TAB, "url": "http://example.com"})
    );
    assert_eq!(
        calls[3]["command"]["args"],
        json!({"browserId": TAB, "deltaX": 0, "deltaY": 120})
    );
    assert_eq!(
        calls[4]["command"]["args"],
        json!({"browserId": TAB, "maxEntries": 50})
    );
}
