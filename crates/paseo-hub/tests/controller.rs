//! `execution-controller.test.ts` plus the Hub execution message flows of
//! `execution-session.websocket.test.ts` that do not need a whole daemon.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::future::BoxFuture;
use paseo_hub::{
    DaemonExecutions, ExecError, ExecutionValidator, HubExecutionAgentCreateInput,
    HubExecutionAgentCreateRequest, HubExecutionAgentValidateRequest, HubExecutionAgents,
    HubExecutionControlInput, HubExecutionControlRequest, HubExecutionController, OwnedAgentEvent,
    OwnedAgentSnapshot, OwnedEventListener, Unsubscribe, ValidateInput,
};
use serde_json::{json, Value};
use support::{settle, wait_until, FakeBackend, Operation};
use tokio::sync::Notify;

type Messages = Arc<Mutex<Vec<Value>>>;

fn sink() -> (Messages, paseo_hub::SendMessage) {
    let messages: Messages = Arc::default();
    let target = messages.clone();
    (messages, Arc::new(move |m| target.lock().unwrap().push(m)))
}

struct Validator {
    issues: Vec<Value>,
    error: Option<String>,
}

#[async_trait]
impl ExecutionValidator for Validator {
    async fn validate(&self, input: ValidateInput) -> Result<Vec<Value>, String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        // `validateAgentConfiguration` of the TS test: an unknown model is an issue.
        if input.model.as_deref() == Some("missing") {
            return Ok(vec![
                json!({ "path": ["model"], "message": "Model is unavailable" }),
            ]);
        }
        Ok(self.issues.clone())
    }
}

fn validator() -> Arc<Validator> {
    Arc::new(Validator {
        issues: vec![],
        error: None,
    })
}

/// TS `ControlledHubExecutionAgents`: one create that finishes when the test says so.
struct ControlledAgents {
    observers: Arc<AtomicUsize>,
    started: Notify,
    started_flag: std::sync::atomic::AtomicBool,
    finish: tokio::sync::watch::Sender<bool>,
    listener: Mutex<Option<OwnedEventListener>>,
}

impl ControlledAgents {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            observers: Arc::new(AtomicUsize::new(0)),
            started: Notify::new(),
            started_flag: std::sync::atomic::AtomicBool::new(false),
            finish: tokio::sync::watch::channel(false).0,
            listener: Mutex::new(None),
        })
    }

    async fn creation_started(&self) {
        wait_until("creation started", || {
            self.started_flag.load(Ordering::SeqCst)
        })
        .await;
    }

    fn finish_create(&self) {
        self.finish.send_replace(true);
    }
}

#[async_trait]
impl HubExecutionAgents for ControlledAgents {
    async fn create(
        &self,
        _input: HubExecutionAgentCreateInput,
    ) -> Result<OwnedAgentSnapshot, ExecError> {
        self.started_flag.store(true, Ordering::SeqCst);
        self.started.notify_waiters();
        let mut finish = self.finish.subscribe();
        let _ = finish.wait_for(|done| *done).await;
        Ok(OwnedAgentSnapshot {
            execution_id: "execution-shutdown".into(),
            agent: json!({ "id": "agent-shutdown", "status": "running" }),
        })
    }

    async fn control(&self, _input: HubExecutionControlInput) -> Result<(), ExecError> {
        Ok(())
    }

    fn subscribe(&self, listener: OwnedEventListener) -> Unsubscribe {
        self.observers.fetch_add(1, Ordering::SeqCst);
        *self.listener.lock().unwrap() = Some(listener);
        let observers = self.observers.clone();
        Unsubscribe::new(move || {
            observers.fetch_sub(1, Ordering::SeqCst);
        })
    }

    fn invalidate_authority(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

struct RejectingAgents(ExecError);

#[async_trait]
impl HubExecutionAgents for RejectingAgents {
    async fn create(
        &self,
        _input: HubExecutionAgentCreateInput,
    ) -> Result<OwnedAgentSnapshot, ExecError> {
        Err(self.0.clone())
    }

    async fn control(&self, _input: HubExecutionControlInput) -> Result<(), ExecError> {
        Ok(())
    }

    fn subscribe(&self, _listener: OwnedEventListener) -> Unsubscribe {
        Unsubscribe::new(|| {})
    }

    fn invalidate_authority(&self) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

fn create_request(value: Value) -> HubExecutionAgentCreateRequest {
    let mut base = json!({
        "type": "hub.execution.agent.create.request",
        "requestId": "create-1",
        "executionId": "execution-1",
        "provider": "codex",
        "cwd": "/tmp/paseo",
        "prompt": "run unattended"
    });
    base.as_object_mut()
        .unwrap()
        .extend(value.as_object().unwrap().clone());
    serde_json::from_value(base).unwrap()
}

fn control_request(
    execution_id: &str,
    request_id: &str,
    action: &str,
) -> HubExecutionControlRequest {
    serde_json::from_value(json!({
        "type": "hub.execution.control.request",
        "requestId": request_id,
        "executionId": execution_id,
        "action": action
    }))
    .unwrap()
}

// ---- execution-controller.test.ts ---------------------------------------------------------

#[tokio::test]
async fn validates_a_named_agent_through_the_daemon_provider_registry() {
    let (messages, send) = sink();
    let controller = HubExecutionController::new(ControlledAgents::new(), validator(), send);

    controller
        .validate_agent(
            serde_json::from_value::<HubExecutionAgentValidateRequest>(json!({
                "type": "hub.execution.agent.validate.request",
                "requestId": "validate-agent",
                "provider": "codex",
                "model": "missing"
            }))
            .unwrap(),
        )
        .await;

    assert_eq!(
        *messages.lock().unwrap(),
        vec![json!({
            "type": "hub.execution.agent.validate.response",
            "payload": {
                "requestId": "validate-agent",
                "valid": false,
                "issues": [{ "path": ["model"], "message": "Model is unavailable" }],
                "error": null
            }
        })]
    );
}

#[tokio::test]
async fn a_validator_failure_is_reported_as_an_error_not_as_issues() {
    let (messages, send) = sink();
    let failing = Arc::new(Validator {
        issues: vec![],
        error: Some("registry offline".into()),
    });
    let controller = HubExecutionController::new(ControlledAgents::new(), failing, send);

    controller
        .validate_agent(
            serde_json::from_value(json!({ "requestId": "v", "provider": "codex" })).unwrap(),
        )
        .await;

    assert_eq!(
        messages.lock().unwrap()[0]["payload"],
        json!({ "requestId": "v", "valid": false, "issues": [], "error": "registry offline" })
    );
}

#[tokio::test]
async fn cleanup_fences_in_flight_creates_before_the_dead_session_can_receive_a_response() {
    let agents = ControlledAgents::new();
    let (messages, send) = sink();
    let controller = HubExecutionController::new(agents.clone(), validator(), send);
    let create = {
        let controller = controller.clone();
        tokio::spawn(async move { controller.create_agent(create_request(json!({}))).await })
    };
    agents.creation_started().await;

    let cleanup = {
        let controller = controller.clone();
        tokio::spawn(async move { controller.cleanup().await })
    };
    settle().await;
    assert!(
        !cleanup.is_finished(),
        "cleanup waits for the in-flight create"
    );
    agents.finish_create();
    create.await.unwrap();
    cleanup.await.unwrap();

    assert!(messages.lock().unwrap().is_empty());
}

#[tokio::test]
async fn cleanup_is_idempotent_and_closes_the_controller() {
    let (messages, send) = sink();
    let controller = HubExecutionController::new(ControlledAgents::new(), validator(), send);

    controller.cleanup().await;
    controller.cleanup().await;
    controller.create_agent(create_request(json!({}))).await;
    controller
        .control_execution(control_request("execution-1", "c", "interrupt"))
        .await;
    controller
        .validate_agent(
            serde_json::from_value(json!({ "requestId": "v", "provider": "codex" })).unwrap(),
        )
        .await;
    controller.set_observing(true);

    assert!(messages.lock().unwrap().is_empty());
    assert!(!controller.is_observing());
}

#[tokio::test]
async fn acknowledges_successful_application_of_a_requested_tool_policy() {
    let agents = ControlledAgents::new();
    let (messages, send) = sink();
    let controller = HubExecutionController::new(agents.clone(), validator(), send);
    let create = {
        let controller = controller.clone();
        tokio::spawn(async move {
            controller
                .create_agent(create_request(json!({
                    "requestId": "tool-policy-create",
                    "provider": "hub-e2e",
                    "prompt": "finish",
                    "mcpServers": { "hub": { "type": "http", "url": "http://127.0.0.1/execution" } },
                    "toolPolicy": { "preapproved": [{ "kind": "mcp", "server": "hub", "tool": "finish_execution" }] }
                })))
                .await
        })
    };
    agents.creation_started().await;
    agents.finish_create();
    create.await.unwrap();

    let messages = messages.lock().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["type"], "hub.execution.agent.create.response");
    assert_eq!(messages[0]["payload"]["success"], true);
    assert_eq!(messages[0]["payload"]["toolPolicyApplied"], true);
    // Key order of the payload is part of the wire text.
    assert_eq!(
        serde_json::to_string(&messages[0]["payload"]).unwrap(),
        r#"{"requestId":"tool-policy-create","executionId":"execution-1","agentId":"agent-shutdown","agent":{"id":"agent-shutdown","status":"running"},"success":true,"toolPolicyApplied":true,"error":null}"#
    );
}

#[tokio::test]
async fn a_create_without_a_tool_policy_carries_no_acknowledgement() {
    let agents = ControlledAgents::new();
    agents.finish_create();
    let (messages, send) = sink();
    let controller = HubExecutionController::new(agents, validator(), send);

    controller.create_agent(create_request(json!({}))).await;

    assert!(messages.lock().unwrap()[0]["payload"]
        .get("toolPolicyApplied")
        .is_none());
}

#[tokio::test]
async fn returns_structured_create_feedback() {
    let cases = [
        (
            ExecError::ProviderOptionsInvalid {
                provider: "codex".into(),
                issues: vec![json!({ "path": ["sandbox_workspace_write", "writable_roots", 0], "message": "Expected string" })],
                message: "Invalid providerOptions for 'codex': sandbox_workspace_write.writable_roots[0]: Expected string".into(),
            },
            json!({
                "code": "provider_options_invalid",
                "provider": "codex",
                "issues": [{ "path": ["sandbox_workspace_write", "writable_roots", 0], "message": "Expected string" }],
                "message": "Invalid providerOptions for 'codex': sandbox_workspace_write.writable_roots[0]: Expected string"
            }),
        ),
        (
            ExecError::ToolPolicyUnsupported {
                provider: "pi".into(),
                message: "Provider 'pi' cannot preapprove exact MCP tools for unattended execution; select Claude, Codex, or OpenCode".into(),
            },
            json!({
                "code": "tool_policy_unsupported",
                "provider": "pi",
                "message": "Provider 'pi' cannot preapprove exact MCP tools for unattended execution; select Claude, Codex, or OpenCode"
            }),
        ),
        (ExecError::other("boom"), json!({ "code": "create_failed", "message": "boom" })),
    ];
    for (error, expected) in cases {
        let (messages, send) = sink();
        let controller =
            HubExecutionController::new(Arc::new(RejectingAgents(error)), validator(), send);

        controller
            .create_agent(create_request(
                json!({ "requestId": "rejected-create", "executionId": "rejected-execution" }),
            ))
            .await;

        assert_eq!(
            messages.lock().unwrap()[0],
            json!({
                "type": "hub.execution.agent.create.response",
                "payload": {
                    "requestId": "rejected-create",
                    "executionId": "rejected-execution",
                    "agentId": null,
                    "agent": null,
                    "success": false,
                    "error": expected
                }
            })
        );
    }
}

#[tokio::test]
async fn observer_count_follows_set_observing_exactly() {
    let agents = ControlledAgents::new();
    let (messages, send) = sink();
    let controller = HubExecutionController::new(agents.clone(), validator(), send);
    assert_eq!(agents.observers.load(Ordering::SeqCst), 0);
    controller.set_observing(true);
    assert_eq!(agents.observers.load(Ordering::SeqCst), 1);
    controller.set_observing(true);
    assert_eq!(agents.observers.load(Ordering::SeqCst), 1);
    controller.set_observing(false);
    assert_eq!(agents.observers.load(Ordering::SeqCst), 0);
    controller
        .validate_agent(
            serde_json::from_value(json!({ "requestId": "plain-validation", "provider": "codex" }))
                .unwrap(),
        )
        .await;
    assert_eq!(messages.lock().unwrap().len(), 1);
    assert_eq!(agents.observers.load(Ordering::SeqCst), 0);
    controller.cleanup().await;
}

#[tokio::test]
async fn hub_observation_is_acquired_and_released_explicitly_without_closing_its_rpc_controller() {
    let (backend, agents) = real_agents();
    let (messages, send) = sink();
    let controller = HubExecutionController::new(agents, validator(), send);
    assert_eq!(backend.listener_count(), 0);

    controller.set_observing(true);
    assert_eq!(backend.listener_count(), 1);
    controller.set_observing(true);
    assert_eq!(backend.listener_count(), 1);
    assert!(controller.is_observing());
    controller.set_observing(false);
    assert_eq!(backend.listener_count(), 0);
    assert!(!controller.is_observing());
    controller
        .validate_agent(
            serde_json::from_value(json!({ "requestId": "plain-validation", "provider": "codex" }))
                .unwrap(),
        )
        .await;

    assert_eq!(messages.lock().unwrap().len(), 1);
    assert_eq!(backend.listener_count(), 0);
    controller.cleanup().await;
}

fn real_agents() -> (Arc<FakeBackend>, Arc<DaemonExecutions>) {
    let backend = FakeBackend::new();
    let agents = Arc::new(DaemonExecutions::new("daemon-1", backend.clone()));
    (backend, agents)
}

// ---- observed events ---------------------------------------------------------------------

#[tokio::test]
async fn observed_events_become_hub_update_and_stream_messages() {
    let (backend, agents) = real_agents();
    let (messages, send) = sink();
    let controller = HubExecutionController::new(agents, validator(), send);
    controller.set_observing(true);

    backend.emit(OwnedAgentEvent::Update {
        execution_id: "execution-1".into(),
        agent: json!({ "id": "agent-1", "status": "running" }),
    });
    backend.emit(OwnedAgentEvent::Stream {
        execution_id: "execution-1".into(),
        agent_id: "agent-1".into(),
        event: json!({ "type": "turn_started", "provider": "codex" }),
    });

    assert_eq!(
        *messages.lock().unwrap(),
        vec![
            json!({
                "type": "hub.execution.agent.update",
                "payload": { "executionId": "execution-1", "agentId": "agent-1", "agent": { "id": "agent-1", "status": "running" } }
            }),
            json!({
                "type": "hub.execution.agent.stream",
                "payload": { "executionId": "execution-1", "agentId": "agent-1", "event": { "type": "turn_started", "provider": "codex" } }
            }),
        ]
    );

    controller.cleanup().await;
    backend.emit(OwnedAgentEvent::Update {
        execution_id: "e".into(),
        agent: json!({ "id": "x" }),
    });
    assert_eq!(
        messages.lock().unwrap().len(),
        2,
        "a closed controller sends nothing"
    );
}

// ---- input validation --------------------------------------------------------------------

#[tokio::test]
async fn create_input_is_validated_before_the_agents_are_called() {
    let cases = [
        (
            json!({ "executionId": "  " }),
            "Hub agent executionId cannot be blank",
        ),
        (
            json!({ "executionId": "\u{feff}" }),
            "Hub agent executionId cannot be blank",
        ),
        (
            json!({ "prompt": "\n\t " }),
            "Hub agent prompt cannot be blank",
        ),
        (json!({ "cwd": "" }), "Hub agent cwd cannot be blank"),
        (
            json!({ "cwd": "relative/dir" }),
            "Hub agent cwd must be absolute",
        ),
        (json!({ "cwd": "./here" }), "Hub agent cwd must be absolute"),
    ];
    for (patch, message) in cases {
        let (messages, send) = sink();
        let controller = HubExecutionController::new(
            Arc::new(RejectingAgents(ExecError::other("must not be called"))),
            validator(),
            send,
        );

        controller.create_agent(create_request(patch.clone())).await;

        assert_eq!(
            messages.lock().unwrap()[0]["payload"]["error"],
            json!({ "code": "create_failed", "message": message }),
            "{patch}"
        );
    }
}

#[tokio::test]
async fn js_trim_whitespace_is_what_counts_as_blank() {
    // U+0085 is not whitespace to `String.prototype.trim`; U+FEFF and U+00A0 are.
    let (messages, send) = sink();
    let agents = ControlledAgents::new();
    agents.finish_create();
    let controller = HubExecutionController::new(agents, validator(), send);

    controller
        .create_agent(create_request(json!({ "prompt": "\u{85}" })))
        .await;
    controller
        .create_agent(create_request(json!({ "prompt": "\u{a0}\u{2003}" })))
        .await;

    let messages = messages.lock().unwrap();
    assert_eq!(messages[0]["payload"]["success"], true);
    assert_eq!(
        messages[1]["payload"]["error"]["message"],
        "Hub agent prompt cannot be blank"
    );
}

#[tokio::test]
async fn control_input_is_validated_and_echoed_back() {
    let (messages, send) = sink();
    let controller = HubExecutionController::new(ControlledAgents::new(), validator(), send);

    controller
        .control_execution(control_request(" ", "blank", "interrupt"))
        .await;
    controller
        .control_execution(control_request("execution-1", "ok", "archive"))
        .await;

    assert_eq!(
        *messages.lock().unwrap(),
        vec![
            json!({
                "type": "hub.execution.control.response",
                "payload": { "requestId": "blank", "executionId": " ", "action": "interrupt", "success": false, "error": "Hub agent executionId cannot be blank" }
            }),
            json!({
                "type": "hub.execution.control.response",
                "payload": { "requestId": "ok", "executionId": "execution-1", "action": "archive", "success": true, "error": null }
            }),
        ]
    );
}

#[tokio::test]
async fn worktree_targets_map_to_the_create_command() {
    let cases = [
        (
            json!({ "mode": "branch-off", "newBranch": "hub-branch", "base": "main" }),
            (
                paseo_hub::WorktreeAction::BranchOff,
                Some("hub-branch"),
                Some("main"),
                None,
                None,
            ),
        ),
        (
            json!({ "mode": "branch-off", "newBranch": "hub-branch" }),
            (
                paseo_hub::WorktreeAction::BranchOff,
                Some("hub-branch"),
                None,
                None,
                None,
            ),
        ),
        (
            json!({ "mode": "checkout-branch", "branch": "existing" }),
            (
                paseo_hub::WorktreeAction::Checkout,
                None,
                None,
                Some("existing"),
                None,
            ),
        ),
        (
            json!({ "mode": "checkout-pr", "prNumber": 42 }),
            (
                paseo_hub::WorktreeAction::Checkout,
                None,
                None,
                None,
                Some(42),
            ),
        ),
    ];
    for (target, (action, name, base, reference, pr)) in cases {
        let (backend, agents) = real_agents();
        let (_messages, send) = sink();
        let controller = HubExecutionController::new(agents, validator(), send);

        controller
            .create_agent(create_request(json!({ "worktree": target })))
            .await;

        let worktree = backend.requests().remove(0).worktree.expect("worktree");
        assert_eq!(worktree.action, action, "{target}");
        assert_eq!(worktree.worktree_name.as_deref(), name);
        assert_eq!(worktree.base_branch.as_deref(), base);
        assert_eq!(worktree.ref_name.as_deref(), reference);
        assert_eq!(worktree.github_pr_number, pr);
    }
}

// ---- over DaemonExecutions (execution-session.websocket.test.ts) --------------------------

#[tokio::test]
async fn hub_retries_one_durable_daemon_execution_across_concurrency() {
    let (backend, agents) = real_agents();
    let (messages, send) = sink();
    let controller = HubExecutionController::new(agents, validator(), send);

    tokio::join!(
        controller.create_agent(create_request(json!({ "requestId": "first" }))),
        controller.create_agent(create_request(json!({ "requestId": "duplicate" }))),
    );

    let messages = messages.lock().unwrap();
    assert_eq!(messages.len(), 2);
    let ids: Vec<&Value> = messages.iter().map(|m| &m["payload"]["agentId"]).collect();
    assert_eq!(ids[0], ids[1]);
    assert_eq!(
        messages
            .iter()
            .filter(|m| m["payload"]["success"] == true)
            .count(),
        2
    );
    assert_eq!(backend.durable_agent_ids().len(), 1);
}

#[tokio::test]
async fn hub_interrupts_an_owned_running_execution_idempotently() {
    let (backend, agents) = real_agents();
    let (messages, send) = sink();
    let controller = HubExecutionController::new(agents, validator(), send);
    controller
        .create_agent(create_request(
            json!({ "executionId": "execution-interrupt" }),
        ))
        .await;

    controller
        .control_execution(control_request(
            "execution-interrupt",
            "interrupt-first",
            "interrupt",
        ))
        .await;
    controller
        .control_execution(control_request(
            "execution-interrupt",
            "interrupt-duplicate",
            "interrupt",
        ))
        .await;

    let messages = messages.lock().unwrap();
    assert_eq!(
        messages[1]["payload"],
        json!({ "requestId": "interrupt-first", "executionId": "execution-interrupt", "action": "interrupt", "success": true, "error": null })
    );
    assert_eq!(
        messages[2]["payload"],
        json!({ "requestId": "interrupt-duplicate", "executionId": "execution-interrupt", "action": "interrupt", "success": true, "error": null })
    );
    assert!(backend
        .operations()
        .contains(&Operation::Interrupt("agent-1".into())));
}

#[tokio::test]
async fn hub_control_waits_for_an_in_flight_create_of_the_same_execution() {
    let (backend, agents) = real_agents();
    let (messages, send) = sink();
    let controller = HubExecutionController::new(agents, validator(), send);
    backend.hold_creation();
    let create = {
        let controller = controller.clone();
        tokio::spawn(async move {
            controller
                .create_agent(create_request(json!({ "requestId": "pending-control-create", "executionId": "execution-pending-control" })))
                .await
        })
    };
    wait_until("creation", || backend.creation_attempts() == 1).await;
    let archive = {
        let controller = controller.clone();
        tokio::spawn(async move {
            controller
                .control_execution(control_request(
                    "execution-pending-control",
                    "pending-control-archive",
                    "archive",
                ))
                .await
        })
    };
    settle().await;
    backend.finish_creation();
    create.await.unwrap();
    archive.await.unwrap();

    let messages = messages.lock().unwrap();
    let created = messages
        .iter()
        .find(|m| m["type"] == "hub.execution.agent.create.response")
        .unwrap();
    let archived = messages
        .iter()
        .find(|m| m["type"] == "hub.execution.control.response")
        .unwrap();
    assert_eq!(created["payload"]["success"], true);
    assert!(created["payload"]["agent"]["workspaceId"].is_string());
    assert_eq!(archived["payload"]["success"], true);
    assert_eq!(archived["payload"]["action"], "archive");
    assert!(backend
        .record("agent-1")
        .unwrap()
        .archived_at
        .value()
        .is_some());
}
