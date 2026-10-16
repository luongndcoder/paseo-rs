//! `daemon-executions.test.ts` and the control cases of `execution-session.websocket.test.ts`
//! against `DaemonExecutions` over the in-memory backend.

mod support;

use std::sync::Arc;

use paseo_hub::{
    ControlAction, DaemonExecutions, ExecError, HubExecutionAgentCreateInput, HubExecutionAgents,
    HubExecutionControlInput, OwnedAgentEvent,
};
use paseo_store::{AgentOwner, StoredAgentRecord};
use serde_json::{json, Map};
use support::{settle, wait_until, FakeBackend, Operation};

const DAEMON: &str = "daemon-1";

fn executions(backend: &Arc<FakeBackend>) -> Arc<DaemonExecutions> {
    Arc::new(DaemonExecutions::new(DAEMON, backend.clone()))
}

fn input(execution_id: &str) -> HubExecutionAgentCreateInput {
    HubExecutionAgentCreateInput {
        execution_id: execution_id.into(),
        provider: "codex".into(),
        cwd: "/workspace".into(),
        prompt: "run".into(),
        ..Default::default()
    }
}

fn control(
    execution_id: &str,
    request_id: &str,
    action: ControlAction,
) -> HubExecutionControlInput {
    HubExecutionControlInput {
        request_id: request_id.into(),
        execution_id: execution_id.into(),
        action,
    }
}

fn servers(value: serde_json::Value) -> Option<Map<String, serde_json::Value>> {
    Some(value.as_object().unwrap().clone())
}

fn branch_off_worktree() -> Option<paseo_hub::ExecutionWorktree> {
    Some(paseo_hub::ExecutionWorktree {
        action: paseo_hub::WorktreeAction::BranchOff,
        worktree_name: Some("failed-hub-create".into()),
        base_branch: None,
        ref_name: None,
        github_pr_number: None,
    })
}

fn other_daemon_record(id: &str, daemon: &str, execution: &str) -> StoredAgentRecord {
    let mut record = StoredAgentRecord::new(id, "codex", "/workspace", "2026-07-13T00:00:00.000Z");
    record.workspace_id = Some("workspace-foreign".into());
    record.owner = Some(AgentOwner {
        kind: paseo_store::agent_record::OwnerKind::Daemon,
        daemon_id: daemon.into(),
        execution_id: execution.into(),
        extra: Default::default(),
    });
    record
}

// ---- create ------------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_creates_of_one_execution_make_one_agent() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);

    let (first, duplicate) = tokio::join!(
        agents.create(input("execution-1")),
        agents.create(input("execution-1"))
    );

    let (first, duplicate) = (first.unwrap(), duplicate.unwrap());
    assert_eq!(first.execution_id, "execution-1");
    assert_eq!(duplicate.agent["id"], first.agent["id"]);
    assert_eq!(backend.creation_attempts(), 1);
    assert_eq!(backend.durable_agent_ids(), vec!["agent-1"]);
}

#[tokio::test]
async fn sequential_replay_after_reconstruction_keeps_one_durable_owned_agent() {
    let backend = FakeBackend::new();
    let created = executions(&backend)
        .create(input("execution-1"))
        .await
        .unwrap();
    assert_eq!(created.agent["status"], "running");

    backend.restart();
    let reconstructed = executions(&backend);
    let replay = reconstructed.create(input("execution-1")).await.unwrap();

    assert_eq!(replay.agent["id"], created.agent["id"]);
    assert_eq!(
        replay.agent["status"], "closed",
        "a restarted daemon resolves the stored record"
    );
    assert_eq!(backend.creation_attempts(), 1);
    assert_eq!(backend.durable_agent_ids().len(), 1);
}

#[tokio::test]
async fn a_live_replay_returns_the_live_snapshot_without_creating_again() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);
    agents.create(input("execution-1")).await.unwrap();

    let replay = agents.create(input("execution-1")).await.unwrap();

    assert_eq!(replay.agent["status"], "idle");
    assert_eq!(backend.creation_attempts(), 1);
}

#[tokio::test]
async fn the_create_request_reaches_the_backend_with_the_ts_command_fields() {
    let backend = FakeBackend::new();
    let mut create = input("execution-1");
    create.model = Some("gpt-5".into());
    create.mode_id = Some("full-access".into());
    create.thinking_option_id = Some("high".into());
    create.env = Some(json!({ "A": "1" }));
    create.mcp_servers =
        servers(json!({ "hub": { "type": "http", "url": "https://hub.test/mcp" } }));
    create.tool_policy = Some(
        json!({ "preapproved": [{ "kind": "mcp", "server": "hub", "tool": "finish_execution" }] }),
    );
    create.provider_options = Some(json!({ "sandbox_mode": "workspace-write" }));

    executions(&backend).create(create).await.unwrap();

    let request = backend.requests().remove(0);
    assert_eq!(request.provider, "codex/gpt-5");
    assert_eq!(request.title, "run");
    assert_eq!(request.initial_prompt, "run");
    assert_eq!(request.cwd, "/workspace");
    assert_eq!(request.mode.as_deref(), Some("full-access"));
    assert_eq!(request.thinking.as_deref(), Some("high"));
    assert_eq!(request.owner.daemon_id, DAEMON);
    assert_eq!(request.owner.execution_id, "execution-1");
    assert!(request.mcp_servers.unwrap().contains_key("hub"));
    assert_eq!(
        request.provider_options,
        Some(json!({ "sandbox_mode": "workspace-write" }))
    );
}

#[tokio::test]
async fn new_hub_executions_cannot_override_the_daemon_owned_paseo_mcp_server() {
    let backend = FakeBackend::new();
    let mut create = input("reserved-mcp-execution");
    create.mcp_servers =
        servers(json!({ "paseo": { "type": "http", "url": "https://hub.test/replace-paseo" } }));

    let error = executions(&backend).create(create).await.unwrap_err();

    assert_eq!(
        error,
        ExecError::other("Hub execution MCP server name \"paseo\" is reserved by the daemon")
    );
    assert_eq!(backend.creation_attempts(), 0);
    assert!(backend.live_agent_ids().is_empty());
    assert!(backend.durable_agent_ids().is_empty());
}

#[tokio::test]
async fn reserved_paseo_mcp_input_does_not_invalidate_replay_of_an_owned_execution() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);
    let original = agents.create(input("replayed-execution")).await.unwrap();

    let mut replay = input("replayed-execution");
    replay.mcp_servers =
        servers(json!({ "paseo": { "type": "http", "url": "https://hub.test/replace-paseo" } }));
    let replay = agents.create(replay).await.unwrap();

    assert_eq!(replay.agent["id"], original.agent["id"]);
    assert_eq!(backend.creation_attempts(), 1);
    assert_eq!(backend.durable_agent_ids(), vec!["agent-1"]);
}

#[tokio::test]
async fn hub_can_preapprove_only_tools_on_mcp_servers_injected_in_the_same_request() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);
    let mut foreign = input("foreign-grant-execution");
    foreign.mcp_servers =
        servers(json!({ "hub": { "type": "http", "url": "https://hub.test/mcp" } }));
    foreign.tool_policy = Some(
        json!({ "preapproved": [{ "kind": "mcp", "server": "unrelated", "tool": "dangerous_tool" }] }),
    );

    let error = agents.create(foreign).await.unwrap_err();

    assert_eq!(
        error,
        ExecError::other(
            "Hub tool preapproval 'unrelated.dangerous_tool' requires MCP server 'unrelated' in the same create request"
        )
    );
    assert_eq!(backend.creation_attempts(), 0);

    let mut ok = input("granted");
    ok.mcp_servers = servers(json!({ "hub": { "type": "http", "url": "https://hub.test/mcp" } }));
    ok.tool_policy = Some(
        json!({ "preapproved": [{ "kind": "mcp", "server": "hub", "tool": "finish_execution" }] }),
    );
    agents.create(ok).await.unwrap();
    assert_eq!(backend.creation_attempts(), 1);

    let mut no_servers = input("no-servers");
    no_servers.tool_policy =
        Some(json!({ "preapproved": [{ "kind": "mcp", "server": "hub", "tool": "x" }] }));
    assert!(agents.create(no_servers).await.is_err());
}

// ---- failed create cleanup ---------------------------------------------------------------

fn worktree_op() -> Operation {
    Operation::CleanupFailedCreate(Some(
        json!({ "path": "/worktrees/agent-1", "created": true }),
    ))
}

#[tokio::test]
async fn a_failed_hub_create_cleans_every_step_in_order_and_rethrows_the_original() {
    let backend = FakeBackend::new();
    backend.fail_create_with(ExecError::other("prompt start failed"));
    let mut create = input("failed-prompt-execution");
    create.worktree = branch_off_worktree();

    let error = executions(&backend).create(create).await.unwrap_err();

    assert_eq!(error, ExecError::other("prompt start failed"));
    assert_eq!(
        backend.operations(),
        vec![
            Operation::CloseAgent("agent-1".into()),
            Operation::DeleteAgentState("agent-1".into()),
            worktree_op(),
            Operation::RemoveRecord("agent-1".into()),
        ]
    );
    assert!(backend.live_agent_ids().is_empty());
    assert!(backend.durable_agent_ids().is_empty());
}

#[tokio::test]
async fn a_failed_create_cleans_durable_state_when_provider_close_rejects() {
    let backend = FakeBackend::new();
    backend.fail_create_with(ExecError::other("prompt start failed"));
    backend.fail_close();
    let mut create = input("failed-close-execution");
    create.worktree = branch_off_worktree();

    let error = executions(&backend).create(create).await.unwrap_err();

    // PARITY-QUIRK(cleanup-error-masks-create-error): the close error replaces the create error.
    assert_eq!(error, ExecError::other("provider close rejected"));
    assert_eq!(
        backend.operations(),
        vec![
            Operation::CloseAgent("agent-1".into()),
            Operation::DeleteAgentState("agent-1".into()),
            worktree_op(),
            Operation::RemoveRecord("agent-1".into()),
        ],
        "every step still runs"
    );
    assert!(backend.live_agent_ids().is_empty());
    assert!(backend.durable_agent_ids().is_empty());
}

#[tokio::test]
async fn a_failing_worktree_cleanup_still_removes_the_record_and_the_later_error_wins() {
    let backend = FakeBackend::new();
    backend.fail_create_with(ExecError::other("prompt start failed"));
    backend.fail_cleanup();
    let mut create = input("failed-cleanup-execution");
    create.worktree = branch_off_worktree();

    let error = executions(&backend).create(create).await.unwrap_err();

    assert_eq!(error, ExecError::other("worktree cleanup failed"));
    assert!(backend
        .operations()
        .contains(&Operation::RemoveRecord("agent-1".into())));
    assert!(backend.durable_agent_ids().is_empty());

    // The record removal is the last `finally`: its error replaces the cleanup error.
    let backend = FakeBackend::new();
    backend.fail_create_with(ExecError::other("prompt start failed"));
    backend.fail_cleanup();
    backend.fail_remove();
    let error = executions(&backend)
        .create(input("both"))
        .await
        .unwrap_err();
    assert_eq!(error, ExecError::other("record removal failed"));
}

#[tokio::test]
async fn failed_create_never_archives_a_reused_worktree() {
    let backend = FakeBackend::new();
    backend.reuse_worktree();
    backend.fail_create_with(ExecError::other("Fail the reused worktree create"));
    let mut create = input("reused-execution");
    create.worktree = branch_off_worktree();

    executions(&backend).create(create).await.unwrap_err();

    assert!(backend
        .operations()
        .contains(&Operation::CleanupFailedCreate(None)));
    assert!(!backend.operations().contains(&worktree_op()));
}

#[tokio::test]
async fn a_create_that_fails_before_the_agent_exists_only_runs_the_worktree_cleanup() {
    let backend = FakeBackend::new();
    backend.fail_create_early_with(ExecError::other("provider unavailable"));
    let mut create = input("early-failure");
    create.worktree = branch_off_worktree();

    let error = executions(&backend).create(create).await.unwrap_err();

    assert_eq!(error, ExecError::other("provider unavailable"));
    assert_eq!(
        backend.operations(),
        vec![Operation::CleanupFailedCreate(None)]
    );
}

#[tokio::test]
async fn structured_create_errors_survive_the_cleanup() {
    let backend = FakeBackend::new();
    backend.fail_create_with(ExecError::ProviderOptionsInvalid {
        provider: "codex".into(),
        issues: vec![json!({ "path": ["sandbox_workspace_write", "writable_roots", 1], "message": "Expected string" })],
        message: "Invalid providerOptions".into(),
    });

    let error = executions(&backend)
        .create(input("invalid-options"))
        .await
        .unwrap_err();

    assert!(matches!(error, ExecError::ProviderOptionsInvalid { .. }));
}

#[tokio::test]
async fn an_agent_without_a_workspace_id_fails_the_create_and_is_cleaned_up() {
    let backend = FakeBackend::new();
    backend.without_workspace();

    let error = executions(&backend)
        .create(input("no-workspace"))
        .await
        .unwrap_err();

    assert_eq!(
        error,
        ExecError::other("Hub execution agent agent-1 has no workspaceId")
    );
    assert!(backend.durable_agent_ids().is_empty());
    assert!(backend.live_agent_ids().is_empty());
}

#[tokio::test]
async fn a_stored_record_without_a_workspace_id_cannot_be_resolved() {
    let backend = FakeBackend::new();
    let mut record = other_daemon_record("agent-old", DAEMON, "execution-old");
    record.workspace_id = None;
    backend.insert_record(record);

    let error = executions(&backend)
        .create(input("execution-old"))
        .await
        .unwrap_err();

    assert_eq!(
        error,
        ExecError::other("Hub execution agent agent-old has no workspaceId")
    );
    assert_eq!(backend.creation_attempts(), 0);
}

#[tokio::test]
async fn a_failed_create_can_be_retried_with_a_fresh_attempt() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);
    backend.fail_create_with(ExecError::other("flaky"));
    agents.create(input("execution-1")).await.unwrap_err();

    let retried = agents.create(input("execution-1")).await.unwrap();

    assert_eq!(retried.execution_id, "execution-1");
    assert_eq!(backend.creation_attempts(), 2);
    assert_eq!(
        backend.durable_agent_ids().len(),
        1,
        "only the second agent is durable"
    );
}

// ---- authority ---------------------------------------------------------------------------

#[tokio::test]
async fn authority_invalidation_rejects_new_work_and_fences_in_flight_creates() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);
    backend.hold_creation();
    let create = {
        let agents = agents.clone();
        tokio::spawn(async move { agents.create(input("execution-1")).await })
    };
    wait_until("creation", || backend.creation_attempts() == 1).await;

    let drained = agents.invalidate_authority();
    assert_eq!(
        agents.create(input("execution-2")).await.unwrap_err(),
        ExecError::other("Hub relationship authority is no longer active")
    );
    assert_eq!(
        agents
            .control(control("execution-1", "r", ControlAction::Archive))
            .await
            .unwrap_err(),
        ExecError::other("Hub relationship authority is no longer active")
    );
    backend.finish_creation();
    drained.await;

    assert_eq!(
        create.await.unwrap().unwrap_err(),
        ExecError::other("Hub relationship authority ended during agent creation")
    );
    assert!(
        backend.durable_agent_ids().is_empty(),
        "the fenced create cleaned up after itself"
    );
    assert!(backend.live_agent_ids().is_empty());
}

#[tokio::test]
async fn authority_invalidation_fences_a_pending_control_before_it_acts() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);
    agents.create(input("execution-1")).await.unwrap();
    backend.hold_interrupt();
    let interrupt = {
        let agents = agents.clone();
        tokio::spawn(async move {
            agents
                .control(control("execution-1", "r1", ControlAction::Interrupt))
                .await
        })
    };
    settle().await; // the interrupt is now blocked inside the backend; the archive queues behind it
    let archive = {
        let agents = agents.clone();
        tokio::spawn(async move {
            agents
                .control(control("execution-1", "r2", ControlAction::Archive))
                .await
        })
    };
    settle().await;

    let drained = agents.invalidate_authority();
    backend.release_interrupt();
    drained.await;

    interrupt.await.unwrap().unwrap();
    assert_eq!(
        archive.await.unwrap().unwrap_err(),
        ExecError::other("Hub relationship authority ended during execution control")
    );
    assert!(!backend
        .operations()
        .iter()
        .any(|op| matches!(op, Operation::ArchiveWorkspace(..))));
}

// ---- control -----------------------------------------------------------------------------

#[tokio::test]
async fn interrupt_calls_the_backend_for_a_live_owned_agent() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);
    agents.create(input("execution-interrupt")).await.unwrap();

    agents
        .control(control(
            "execution-interrupt",
            "interrupt-first",
            ControlAction::Interrupt,
        ))
        .await
        .unwrap();
    agents
        .control(control(
            "execution-interrupt",
            "interrupt-duplicate",
            ControlAction::Interrupt,
        ))
        .await
        .unwrap();

    assert_eq!(
        backend.operations(),
        vec![
            Operation::Interrupt("agent-1".into()),
            Operation::Interrupt("agent-1".into())
        ]
    );
}

#[tokio::test]
async fn concurrent_identical_controls_share_one_operation() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);
    agents.create(input("execution-1")).await.unwrap();
    backend.hold_interrupt();

    let first = {
        let agents = agents.clone();
        tokio::spawn(async move {
            agents
                .control(control("execution-1", "r1", ControlAction::Interrupt))
                .await
        })
    };
    settle().await;
    let duplicate = {
        let agents = agents.clone();
        tokio::spawn(async move {
            agents
                .control(control("execution-1", "r2", ControlAction::Interrupt))
                .await
        })
    };
    settle().await;
    backend.release_interrupt();

    first.await.unwrap().unwrap();
    duplicate.await.unwrap().unwrap();
    assert_eq!(
        backend.operations(),
        vec![Operation::Interrupt("agent-1".into())]
    );
}

#[tokio::test]
async fn control_waits_for_an_in_flight_create_of_the_same_execution() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);
    backend.hold_creation();
    let create = {
        let agents = agents.clone();
        tokio::spawn(async move { agents.create(input("execution-pending-control")).await })
    };
    wait_until("creation", || backend.creation_attempts() == 1).await;
    let archive = {
        let agents = agents.clone();
        tokio::spawn(async move {
            agents
                .control(control(
                    "execution-pending-control",
                    "pending-control-archive",
                    ControlAction::Archive,
                ))
                .await
        })
    };
    settle().await;
    assert!(
        backend.operations().is_empty(),
        "the archive has not run before the create settled"
    );

    backend.finish_creation();
    let created = create.await.unwrap().unwrap();
    archive.await.unwrap().unwrap();

    assert_eq!(created.agent["workspaceId"], "workspace-1");
    assert_eq!(
        backend.operations(),
        vec![Operation::ArchiveWorkspace(
            "workspace-1".into(),
            "pending-control-archive".into()
        )]
    );
    assert!(backend
        .record("agent-1")
        .unwrap()
        .archived_at
        .value()
        .is_some());
}

#[tokio::test]
async fn a_failed_create_does_not_block_the_control_queued_behind_it() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);
    backend.hold_creation();
    backend.fail_create_with(ExecError::other("create failed"));
    let create = {
        let agents = agents.clone();
        tokio::spawn(async move { agents.create(input("execution-1")).await })
    };
    wait_until("creation", || backend.creation_attempts() == 1).await;
    let archive = {
        let agents = agents.clone();
        tokio::spawn(async move {
            agents
                .control(control("execution-1", "r", ControlAction::Archive))
                .await
        })
    };
    settle().await;
    backend.finish_creation();

    create.await.unwrap().unwrap_err();
    archive.await.unwrap().unwrap();
    assert!(
        !backend
            .operations()
            .iter()
            .any(|op| matches!(op, Operation::ArchiveWorkspace(..))),
        "nothing left to archive"
    );
}

#[tokio::test]
async fn controls_of_one_execution_run_in_order_and_a_failed_one_does_not_block_the_next() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);
    agents.create(input("execution-1")).await.unwrap();
    backend.hold_interrupt();
    backend.fail_interrupt();
    let interrupt = {
        let agents = agents.clone();
        tokio::spawn(async move {
            agents
                .control(control("execution-1", "r1", ControlAction::Interrupt))
                .await
        })
    };
    settle().await;
    let archive = {
        let agents = agents.clone();
        tokio::spawn(async move {
            agents
                .control(control("execution-1", "r2", ControlAction::Archive))
                .await
        })
    };
    settle().await;
    assert_eq!(
        backend.operations(),
        Vec::<Operation>::new(),
        "the archive waits behind the interrupt"
    );

    backend.release_interrupt();

    assert_eq!(
        interrupt.await.unwrap().unwrap_err(),
        ExecError::other("interrupt failed")
    );
    archive.await.unwrap().unwrap();
    assert_eq!(
        backend.operations(),
        vec![
            Operation::Interrupt("agent-1".into()),
            Operation::ArchiveWorkspace("workspace-1".into(), "r2".into())
        ]
    );
}

#[tokio::test]
async fn hub_archives_the_execution_workspace_and_a_duplicate_archive_is_harmless() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);
    agents.create(input("execution-local")).await.unwrap();

    agents
        .control(control(
            "execution-local",
            "archive-local",
            ControlAction::Archive,
        ))
        .await
        .unwrap();
    agents
        .control(control(
            "execution-local",
            "archive-local-duplicate",
            ControlAction::Archive,
        ))
        .await
        .unwrap();

    assert_eq!(
        backend.operations(),
        vec![
            Operation::ArchiveWorkspace("workspace-1".into(), "archive-local".into()),
            Operation::ArchiveWorkspace("workspace-1".into(), "archive-local-duplicate".into()),
        ]
    );
    assert!(backend.live_agent_ids().is_empty());
}

#[tokio::test]
async fn interrupt_skips_archived_and_stopped_agents() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);
    agents.create(input("execution-1")).await.unwrap();
    agents
        .control(control("execution-1", "a", ControlAction::Archive))
        .await
        .unwrap();
    backend.restart();

    agents
        .control(control("execution-1", "i", ControlAction::Interrupt))
        .await
        .unwrap();

    assert!(!backend
        .operations()
        .iter()
        .any(|op| matches!(op, Operation::Interrupt(_))));
}

#[tokio::test]
async fn control_resolves_persisted_execution_ownership_after_daemon_restart() {
    let backend = FakeBackend::new();
    executions(&backend)
        .create(input("execution-restart"))
        .await
        .unwrap();
    backend.restart();

    executions(&backend)
        .control(control(
            "execution-restart",
            "archive-after-restart",
            ControlAction::Archive,
        ))
        .await
        .unwrap();

    assert_eq!(
        backend.operations(),
        vec![Operation::ArchiveWorkspace(
            "workspace-1".into(),
            "archive-after-restart".into()
        )]
    );
}

#[tokio::test]
async fn missing_and_foreign_executions_are_already_controlled_without_exposing_ownership() {
    let backend = FakeBackend::new();
    backend.insert_record(other_daemon_record(
        "agent-foreign",
        "daemon-other",
        "execution-foreign",
    ));
    let agents = executions(&backend);

    for (execution, action) in [
        ("execution-missing", ControlAction::Interrupt),
        ("execution-missing", ControlAction::Archive),
        ("execution-foreign", ControlAction::Interrupt),
        ("execution-foreign", ControlAction::Archive),
    ] {
        agents
            .control(control(execution, "r", action))
            .await
            .unwrap();
    }

    assert!(backend.operations().is_empty());
    assert!(backend.record("agent-foreign").is_some());
}

#[tokio::test]
async fn removing_a_daemon_owned_agent_removes_its_execution_association() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);
    let created = agents.create(input("execution-1")).await.unwrap();
    let owner = AgentOwner {
        kind: paseo_store::agent_record::OwnerKind::Daemon,
        daemon_id: DAEMON.into(),
        execution_id: "execution-1".into(),
        extra: Default::default(),
    };
    use paseo_hub::ExecutionBackend as _;
    assert!(backend
        .find_by_daemon_execution(&owner)
        .await
        .unwrap()
        .is_some());

    backend
        .remove_agent_record(created.agent["id"].as_str().unwrap())
        .await
        .unwrap();

    assert!(backend
        .find_by_daemon_execution(&owner)
        .await
        .unwrap()
        .is_none());
    assert!(backend.durable_agent_ids().is_empty());
}

// ---- observation -------------------------------------------------------------------------

#[tokio::test]
async fn subscribe_observes_through_the_backend_until_unsubscribed() {
    let backend = FakeBackend::new();
    let agents = executions(&backend);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    let unsubscribe = agents.subscribe(Arc::new(move |event| sink.lock().unwrap().push(event)));
    assert_eq!(backend.listener_count(), 1);

    backend.emit(OwnedAgentEvent::Update {
        execution_id: "e".into(),
        agent: json!({ "id": "a" }),
    });
    unsubscribe.call();
    backend.emit(OwnedAgentEvent::Update {
        execution_id: "e".into(),
        agent: json!({ "id": "b" }),
    });

    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(backend.listener_count(), 0);
}
