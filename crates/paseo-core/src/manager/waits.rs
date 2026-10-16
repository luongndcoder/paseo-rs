//! `wait_for_agent_run_start` and `wait_for_agent_event`.
//!
//! Both subscribe (agent scoped) and judge each `agent_state` event by the snapshot it carries,
//! which is exactly the live state TS reads inside its synchronous callback.

use std::sync::Arc;

use paseo_agent_types::{AgentStreamEvent, PermissionRequest};

use super::AgentManager;
use crate::abort::AbortSignal;
use crate::error::{CoreError, Result};
use crate::events::{ManagerEvent, SubscribeOpts};
use crate::model::{Lifecycle, ManagedAgentView, RunStart};

#[derive(Debug, Clone, Default)]
pub struct WaitForAgentOptions {
    pub signal: Option<AbortSignal>,
    pub wait_for_active: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WaitForAgentResult {
    pub status: Lifecycle,
    pub permission: Option<PermissionRequest>,
    pub last_message: Option<String>,
}

fn aborted_error(signal: &AbortSignal, fallback: &str) -> CoreError {
    CoreError::Aborted(signal.message(fallback))
}

/// Verdict of one `checkCurrentState` pass of `wait_for_agent_run_start`.
enum StartCheck {
    Started,
    Failed(CoreError),
    Waiting,
}

fn check_run_start(agent_id: &str, current: Option<&ManagedAgentView>) -> StartCheck {
    let Some(current) = current.filter(|v| v.lifecycle != Lifecycle::Closed) else {
        return StartCheck::Failed(CoreError::msg(format!("Agent {agent_id} not found")));
    };
    if (current.lifecycle == Lifecycle::Running || current.pending_start_started())
        && !current.pending_replacement
    {
        return StartCheck::Started;
    }
    if let Some(RunStart::Failed { error }) = &current.pending_run_start {
        return StartCheck::Failed(CoreError::msg(error.clone()));
    }
    if current.lifecycle == Lifecycle::Error && current.pending_run_start.is_none() {
        let message = current
            .last_error
            .clone()
            .unwrap_or_else(|| format!("Agent {agent_id} failed to start"));
        return StartCheck::Failed(CoreError::Message(message));
    }
    if current.pending_run_start.is_none()
        && current.active_foreground_turn_id.is_none()
        && !current.pending_replacement
    {
        return StartCheck::Failed(CoreError::msg(format!(
            "Agent {agent_id} run finished before starting"
        )));
    }
    StartCheck::Waiting
}

/// Read the agent's state and register a subscription in ONE actor step: the snapshot is the live
/// state at registration time and the subscription sees every event emitted after it, with no
/// gap (TS gets this from running `subscribe` and the state read in one synchronous stretch).
async fn snapshot_and_subscribe(
    handle: &crate::agent_actor::ActorHandle,
    opts: SubscribeOpts,
) -> Result<(ManagedAgentView, crate::events::Subscription)> {
    handle
        .ask(move |a| {
            let snapshot = a.snapshot();
            a.ctx
                .bus
                .register(opts)
                .map(|(subscription, _)| (snapshot, subscription))
        })
        .await?
}

impl AgentManager {
    /// `waitForAgentRunStart`.
    pub async fn wait_for_agent_run_start(
        &self,
        agent_id: &str,
        signal: Option<AbortSignal>,
    ) -> Result<()> {
        let Some(handle) = self.inner.registry.get(agent_id) else {
            return Err(CoreError::msg(format!("Agent {agent_id} not found")));
        };
        let (snapshot, mut subscription) =
            snapshot_and_subscribe(&handle, SubscribeOpts::for_agent(agent_id).without_replay())
                .await?;
        if (snapshot.lifecycle == Lifecycle::Running || snapshot.pending_start_started())
            && !snapshot.pending_replacement
        {
            return Ok(());
        }
        if snapshot.active_foreground_turn_id.is_none()
            && snapshot.pending_run_start.is_none()
            && !snapshot.pending_replacement
        {
            return Err(CoreError::msg(format!(
                "Agent {agent_id} has no pending run"
            )));
        }
        if let Some(signal) = signal.as_ref().filter(|s| s.is_aborted()) {
            return Err(aborted_error(signal, "wait_for_agent_start aborted"));
        }
        // `checkCurrentState()` right after subscribing.
        match check_run_start(agent_id, Some(&snapshot)) {
            StartCheck::Started => return Ok(()),
            StartCheck::Failed(error) => return Err(error),
            StartCheck::Waiting => {}
        }
        let signal = signal.unwrap_or_default();
        loop {
            tokio::select! {
                () = signal.aborted() => {
                    return Err(aborted_error(&signal, "wait_for_agent_start aborted"));
                }
                event = subscription.next() => {
                    let Some(event) = event else {
                        return Err(CoreError::msg("agent subscription closed"));
                    };
                    let ManagerEvent::AgentState { agent } = &*event else { continue };
                    if agent.id != agent_id {
                        continue;
                    }
                    match check_run_start(agent_id, Some(agent)) {
                        StartCheck::Started => return Ok(()),
                        StartCheck::Failed(error) => return Err(error),
                        StartCheck::Waiting => {}
                    }
                }
            }
        }
    }

    /// `waitForAgentEvent`: resolve on a permission request, or when the agent stops being busy.
    pub async fn wait_for_agent_event(
        &self,
        agent_id: &str,
        options: WaitForAgentOptions,
    ) -> Result<WaitForAgentResult> {
        let Some(handle) = self.inner.registry.get(agent_id) else {
            return Err(CoreError::msg(format!("Agent {agent_id} not found")));
        };
        let (snapshot, mut subscription) =
            snapshot_and_subscribe(&handle, SubscribeOpts::for_agent(agent_id).without_replay())
                .await?;
        let has_foreground_turn =
            snapshot.active_foreground_turn_id.is_some() || snapshot.pending_run_start.is_some();
        if let Some(permission) = snapshot.peek_pending_permission() {
            return Ok(WaitForAgentResult {
                status: snapshot.lifecycle,
                permission: Some(permission.clone()),
                last_message: self.get_last_assistant_message(agent_id).await,
            });
        }
        let initial_status = snapshot.lifecycle;
        let initial_busy = snapshot.is_busy() || has_foreground_turn;
        if !initial_busy {
            return Ok(WaitForAgentResult {
                status: initial_status,
                permission: None,
                last_message: self.get_last_assistant_message(agent_id).await,
            });
        }
        if let Some(signal) = options.signal.as_ref().filter(|s| s.is_aborted()) {
            return Err(aborted_error(signal, "wait_for_agent aborted"));
        }

        let mut status = initial_status;
        let mut has_started = snapshot.is_busy()
            || snapshot.active_foreground_turn_id.is_some()
            || snapshot.pending_start_started();
        let mut terminal_override: Option<Lifecycle> = None;
        let signal = options.signal.unwrap_or_default();
        // `subscribe(..., { replayState: true })`: the state at registration is replayed first.
        let replay = Some(Arc::new(ManagerEvent::AgentState {
            agent: Arc::new(snapshot),
        }));
        let mut replay = replay;
        loop {
            let permission = tokio::select! {
                () = signal.aborted() => {
                    return Err(aborted_error(&signal, "wait_for_agent aborted"));
                }
                event = async {
                    match replay.take() {
                        Some(event) => Some(event),
                        None => subscription.next().await,
                    }
                } => {
                    let Some(event) = event else {
                        return Err(CoreError::msg("agent subscription closed"));
                    };
                    match &*event {
                        ManagerEvent::AgentState { agent } => {
                            status = agent.lifecycle;
                            if let Some(pending) = agent.peek_pending_permission() {
                                Some(Some(pending.clone()))
                            } else if agent.is_busy() {
                                has_started = true;
                                None
                            } else if !options.wait_for_active || has_started {
                                if let Some(forced) = terminal_override {
                                    status = forced;
                                }
                                Some(None)
                            } else {
                                None
                            }
                        }
                        ManagerEvent::AgentStream { event, .. } => match event {
                            AgentStreamEvent::PermissionRequested { request, .. } => {
                                Some(Some((**request).clone()))
                            }
                            AgentStreamEvent::TurnFailed { .. } => {
                                has_started = true;
                                terminal_override = Some(Lifecycle::Error);
                                None
                            }
                            AgentStreamEvent::TurnCompleted { .. }
                            | AgentStreamEvent::TurnCanceled { .. } => {
                                has_started = true;
                                None
                            }
                            _ => None,
                        },
                        _ => None,
                    }
                }
            };
            if let Some(permission) = permission {
                drop(subscription);
                return Ok(WaitForAgentResult {
                    status,
                    permission,
                    last_message: self.get_last_assistant_message(agent_id).await,
                });
            }
        }
    }
}
