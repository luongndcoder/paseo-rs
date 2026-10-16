//! `HubExecutionController` (TS `execution-controller.ts`): the per-session handler of the
//! `hub.execution.*` messages. It validates input, calls [`HubExecutionAgents`], and sends the
//! responses and observed agent events through `send`. `cleanup` fences everything in flight
//! so a dead session never receives a late response.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::sync::{Notify, OnceCell};

use crate::backend::{ExecError, ExecutionWorktree, WorktreeAction};
use crate::executions::{
    ControlAction, HubExecutionAgentCreateInput, HubExecutionAgents, HubExecutionControlInput,
    OwnedAgentEvent, OwnedEventListener, Unsubscribe,
};

/// `hub.execution.agent.create.request`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubExecutionAgentCreateRequest {
    pub request_id: String,
    pub execution_id: String,
    pub provider: String,
    pub cwd: String,
    pub prompt: String,
    pub model: Option<String>,
    pub mode_id: Option<String>,
    pub thinking_option_id: Option<String>,
    pub feature_values: Option<Value>,
    pub provider_options: Option<Value>,
    pub tool_policy: Option<Value>,
    pub env: Option<Value>,
    pub mcp_servers: Option<Map<String, Value>>,
    pub worktree: Option<WorktreeTarget>,
}

/// `CreateAgentWorktreeTarget`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode")]
pub enum WorktreeTarget {
    #[serde(rename = "branch-off", rename_all = "camelCase")]
    BranchOff {
        new_branch: String,
        base: Option<String>,
    },
    #[serde(rename = "checkout-branch")]
    CheckoutBranch { branch: String },
    #[serde(rename = "checkout-pr", rename_all = "camelCase")]
    CheckoutPr { pr_number: i64 },
}

impl WorktreeTarget {
    /// `toCreateAgentWorktree`.
    fn into_execution_worktree(self) -> ExecutionWorktree {
        let empty = ExecutionWorktree {
            action: WorktreeAction::Checkout,
            worktree_name: None,
            base_branch: None,
            ref_name: None,
            github_pr_number: None,
        };
        match self {
            Self::BranchOff { new_branch, base } => ExecutionWorktree {
                action: WorktreeAction::BranchOff,
                worktree_name: Some(new_branch),
                base_branch: base,
                ..empty
            },
            Self::CheckoutBranch { branch } => ExecutionWorktree {
                ref_name: Some(branch),
                ..empty
            },
            Self::CheckoutPr { pr_number } => ExecutionWorktree {
                github_pr_number: Some(pr_number),
                ..empty
            },
        }
    }
}

/// `hub.execution.agent.validate.request`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubExecutionAgentValidateRequest {
    pub request_id: String,
    pub provider: String,
    pub model: Option<String>,
    pub mode_id: Option<String>,
    pub thinking_option_id: Option<String>,
    pub provider_options: Option<Value>,
}

/// `hub.execution.control.request`. `action` is echoed back verbatim.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubExecutionControlRequest {
    pub request_id: String,
    pub execution_id: String,
    pub action: String,
}

#[derive(Debug, Clone)]
pub struct ValidateInput {
    pub provider: String,
    pub model: Option<String>,
    pub mode_id: Option<String>,
    pub thinking_option_id: Option<String>,
    pub provider_options: Option<Value>,
}

/// `validateAgentConfiguration`: the issues found, or a message when validation itself failed.
#[async_trait]
pub trait ExecutionValidator: Send + Sync + 'static {
    async fn validate(&self, input: ValidateInput) -> Result<Vec<Value>, String>;
}

pub type SendMessage = Arc<dyn Fn(Value) + Send + Sync>;

#[derive(Default)]
struct InFlight {
    count: AtomicUsize,
    idle: Notify,
}

struct InFlightGuard(Arc<InFlight>);

impl InFlight {
    fn enter(self: &Arc<Self>) -> InFlightGuard {
        self.count.fetch_add(1, Ordering::SeqCst);
        InFlightGuard(self.clone())
    }

    async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.count.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

struct Inner {
    agents: Arc<dyn HubExecutionAgents>,
    validator: Arc<dyn ExecutionValidator>,
    send: SendMessage,
    unsubscribe: Mutex<Option<Unsubscribe>>,
    in_flight: Arc<InFlight>,
    cleanup: OnceCell<()>,
    closed: AtomicBool,
}

#[derive(Clone)]
pub struct HubExecutionController {
    inner: Arc<Inner>,
}

impl HubExecutionController {
    pub fn new(
        agents: Arc<dyn HubExecutionAgents>,
        validator: Arc<dyn ExecutionValidator>,
        send: SendMessage,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                agents,
                validator,
                send,
                unsubscribe: Mutex::new(None),
                in_flight: Arc::new(InFlight::default()),
                cleanup: OnceCell::new(),
                closed: AtomicBool::new(false),
            }),
        }
    }

    pub fn is_observing(&self) -> bool {
        self.inner.unsubscribe().is_some()
    }

    /// Acquires or releases the owned-agent observation. Idempotent; never closes the
    /// controller.
    pub fn set_observing(&self, observing: bool) {
        let inner = &self.inner;
        if observing && !inner.is_closed() {
            let mut slot = inner.unsubscribe();
            if slot.is_none() {
                let weak = Arc::downgrade(inner);
                let listener: OwnedEventListener = Arc::new(move |event| {
                    if let Some(inner) = weak.upgrade() {
                        inner.send_owned_event(event);
                    }
                });
                *slot = Some(inner.agents.subscribe(listener));
            }
        } else if let Some(unsubscribe) = inner.unsubscribe().take() {
            unsubscribe.call();
        }
    }

    /// Closes the controller: stops observing and waits for every in-flight request. Calling
    /// it again waits for the same cleanup.
    pub async fn cleanup(&self) {
        self.inner
            .cleanup
            .get_or_init(|| async {
                self.inner.closed.store(true, Ordering::SeqCst);
                self.set_observing(false);
                self.inner.in_flight.wait_idle().await;
            })
            .await;
    }

    pub async fn validate_agent(&self, message: HubExecutionAgentValidateRequest) {
        if self.inner.is_closed() {
            return;
        }
        let _guard = self.inner.in_flight.enter();
        let input = ValidateInput {
            provider: message.provider,
            model: message.model,
            mode_id: message.mode_id,
            thinking_option_id: message.thinking_option_id,
            provider_options: message.provider_options,
        };
        let (issues, error) = match self.inner.validator.validate(input).await {
            Ok(issues) => (issues, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        if self.inner.is_closed() {
            return;
        }
        let valid = error.is_none() && issues.is_empty();
        self.inner.send(json!({
            "type": "hub.execution.agent.validate.response",
            "payload": {
                "requestId": message.request_id,
                "valid": valid,
                "issues": issues,
                "error": error,
            },
        }));
    }

    pub async fn control_execution(&self, message: HubExecutionControlRequest) {
        if self.inner.is_closed() {
            return;
        }
        let _guard = self.inner.in_flight.enter();
        let result = async {
            require_non_blank("executionId", &message.execution_id)?;
            self.inner
                .agents
                .control(HubExecutionControlInput {
                    request_id: message.request_id.clone(),
                    execution_id: message.execution_id.clone(),
                    action: if message.action == "interrupt" {
                        ControlAction::Interrupt
                    } else {
                        ControlAction::Archive
                    },
                })
                .await
        }
        .await;
        if self.inner.is_closed() {
            return;
        }
        let error = result.err().map(|e| e.to_string());
        self.inner.send(json!({
            "type": "hub.execution.control.response",
            "payload": {
                "requestId": message.request_id,
                "executionId": message.execution_id,
                "action": message.action,
                "success": error.is_none(),
                "error": error,
            },
        }));
    }

    pub async fn create_agent(&self, message: HubExecutionAgentCreateRequest) {
        if self.inner.is_closed() {
            return;
        }
        let _guard = self.inner.in_flight.enter();
        let request_id = message.request_id.clone();
        let execution_id = message.execution_id.clone();
        let tool_policy_requested = message.tool_policy.is_some();
        let result = self.create_inner(message).await;
        if self.inner.is_closed() {
            return;
        }
        let payload = match result {
            Ok(snapshot) => {
                let agent_id = snapshot.agent.get("id").cloned().unwrap_or(Value::Null);
                let mut payload = Map::new();
                payload.insert("requestId".into(), request_id.into());
                payload.insert("executionId".into(), execution_id.into());
                payload.insert("agentId".into(), agent_id);
                payload.insert("agent".into(), snapshot.agent);
                payload.insert("success".into(), true.into());
                if tool_policy_requested {
                    payload.insert("toolPolicyApplied".into(), true.into());
                }
                payload.insert("error".into(), Value::Null);
                Value::Object(payload)
            }
            Err(error) => json!({
                "requestId": request_id,
                "executionId": execution_id,
                "agentId": null,
                "agent": null,
                "success": false,
                "error": to_hub_create_error(&error),
            }),
        };
        self.inner.send(json!({
            "type": "hub.execution.agent.create.response",
            "payload": payload,
        }));
    }

    async fn create_inner(
        &self,
        message: HubExecutionAgentCreateRequest,
    ) -> Result<crate::executions::OwnedAgentSnapshot, ExecError> {
        require_non_blank("executionId", &message.execution_id)?;
        require_non_blank("prompt", &message.prompt)?;
        require_non_blank("cwd", &message.cwd)?;
        if !std::path::Path::new(&message.cwd).is_absolute() {
            return Err(ExecError::other("Hub agent cwd must be absolute"));
        }
        self.inner
            .agents
            .create(HubExecutionAgentCreateInput {
                execution_id: message.execution_id,
                provider: message.provider,
                cwd: message.cwd,
                prompt: message.prompt,
                model: message.model,
                mode_id: message.mode_id,
                thinking_option_id: message.thinking_option_id,
                feature_values: message.feature_values,
                provider_options: message.provider_options,
                tool_policy: message.tool_policy,
                env: message.env,
                mcp_servers: message.mcp_servers,
                worktree: message
                    .worktree
                    .map(WorktreeTarget::into_execution_worktree),
            })
            .await
    }
}

impl Inner {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn unsubscribe(&self) -> std::sync::MutexGuard<'_, Option<Unsubscribe>> {
        self.unsubscribe.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn send(&self, message: Value) {
        (self.send)(message);
    }

    fn send_owned_event(&self, event: OwnedAgentEvent) {
        if self.is_closed() {
            return;
        }
        match event {
            OwnedAgentEvent::Update {
                execution_id,
                agent,
            } => {
                let agent_id = agent.get("id").cloned().unwrap_or(Value::Null);
                self.send(json!({
                    "type": "hub.execution.agent.update",
                    "payload": {
                        "executionId": execution_id,
                        "agentId": agent_id,
                        "agent": agent,
                    },
                }));
            }
            OwnedAgentEvent::Stream {
                execution_id,
                agent_id,
                event,
            } => self.send(json!({
                "type": "hub.execution.agent.stream",
                "payload": {
                    "executionId": execution_id,
                    "agentId": agent_id,
                    "event": event,
                },
            })),
        }
    }
}

/// `toHubCreateError`.
fn to_hub_create_error(error: &ExecError) -> Value {
    match error {
        ExecError::ProviderOptionsInvalid {
            provider,
            issues,
            message,
        } => json!({
            "code": "provider_options_invalid",
            "provider": provider,
            "issues": issues,
            "message": message,
        }),
        ExecError::ToolPolicyUnsupported { provider, message } => json!({
            "code": "tool_policy_unsupported",
            "provider": provider,
            "message": message,
        }),
        ExecError::Other(message) => json!({ "code": "create_failed", "message": message }),
    }
}

/// JS `String.prototype.trim` whitespace (differs from Rust's: it has U+FEFF, lacks U+0085).
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn require_non_blank(field: &str, value: &str) -> Result<(), ExecError> {
    if value.chars().all(is_js_whitespace) {
        return Err(ExecError::other(format!(
            "Hub agent {field} cannot be blank"
        )));
    }
    Ok(())
}
