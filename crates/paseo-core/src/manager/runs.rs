//! `stream_agent` / `run_agent`: admission, the lazy foreground stream and turn start.
//!
//! Each function is a TS async method: a sequence of actor commands (one per synchronous
//! stretch) with provider awaits between them.

use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem, AgentUsage};
use paseo_provider_api::PromptInput;
use tokio::sync::{mpsc, watch};

use super::AgentManager;
use crate::agent_actor::events::is_terminal;
use crate::agent_actor::timeline_record::format_turn_failed_message;
use crate::agent_actor::ActorHandle;
use crate::error::{is_closed_provider_session_message, CoreError, Result};
use crate::manager::reads::last_assistant_message;
use crate::model::RunOpts;

/// `AgentRunResult` of `run_agent`.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRunOutcome {
    pub session_id: String,
    pub final_text: String,
    pub usage: Option<AgentUsage>,
    pub timeline: Vec<AgentTimelineItem>,
    pub canceled: bool,
}

enum StreamState {
    /// Admitted; the provider turn starts on the first `next()`.
    Unpolled {
        prompt: PromptInput,
        opts: RunOpts,
        is_replacement: bool,
        settled: watch::Receiver<bool>,
    },
    Streaming {
        events: mpsc::UnboundedReceiver<AgentStreamEvent>,
        /// The accepted `turn_started`, yielded before anything the waiter delivers.
        first: Option<AgentStreamEvent>,
        saw_terminal: bool,
    },
    Done,
}

/// The foreground stream of one turn (TS `AsyncGenerator<AgentStreamEvent>`).
///
/// Admission is atomic at `stream_agent`; the provider turn starts on the first `next()`.
///
/// DEVIATION(stream-drop-settles-run): TS leaks the pending run of a generator that is never
/// polled. Here dropping an unpolled (or abandoned) stream settles its run through the actor,
/// so a forgotten stream cannot lock the agent. This is the only intentional deviation in the
/// run path (decision Q18).
pub struct RunStream {
    manager: AgentManager,
    handle: ActorHandle,
    token: String,
    state: StreamState,
    /// The waiter this stream registered, once the turn was accepted.
    waiter_id: Option<u64>,
    /// The `finally` ran (or the actor already settled the run); `Drop` has nothing to do.
    cleaned: bool,
}

impl RunStream {
    /// Next event; `Ok(None)` when the stream is finished. A start failure is the first and only
    /// `Err`. Dropping the returned future (or the stream) at any await point leaves the run to
    /// be settled by `Drop`.
    pub async fn next(&mut self) -> Result<Option<AgentStreamEvent>> {
        loop {
            match &mut self.state {
                StreamState::Done => return Ok(None),
                StreamState::Unpolled {
                    prompt,
                    opts,
                    is_replacement,
                    settled,
                } => {
                    let (prompt, opts, is_replacement, settled) = (
                        prompt.clone(),
                        opts.clone(),
                        *is_replacement,
                        settled.clone(),
                    );
                    let started = self
                        .manager
                        .start_pending_foreground_turn(
                            &self.handle,
                            &self.token,
                            &prompt,
                            &opts,
                            is_replacement,
                            &settled,
                        )
                        .await;
                    let accepted = match started {
                        Ok(accepted) => accepted,
                        Err(error) => {
                            // The start path already settled the run.
                            self.state = StreamState::Done;
                            self.cleaned = true;
                            return Err(error);
                        }
                    };
                    let first = AgentStreamEvent::TurnStarted {
                        provider: self.handle.view().provider.clone(),
                        turn_id: Some(accepted.turn_id.clone()),
                    };
                    self.waiter_id = Some(accepted.waiter_id);
                    self.state = StreamState::Streaming {
                        events: accepted.events,
                        first: Some(first),
                        saw_terminal: false,
                    };
                }
                StreamState::Streaming {
                    first,
                    events,
                    saw_terminal,
                } => {
                    if let Some(event) = first.take() {
                        return Ok(Some(event));
                    }
                    if !*saw_terminal {
                        if let Some(event) = events.recv().await {
                            *saw_terminal = is_terminal(&event);
                            return Ok(Some(event));
                        }
                    }
                    self.finish().await;
                    return Ok(None);
                }
            }
        }
    }

    /// Drive the stream to its end, discarding events.
    pub async fn drain(&mut self) -> Result<()> {
        while self.next().await?.is_some() {}
        Ok(())
    }

    /// The `finally` of the TS generator.
    async fn finish(&mut self) {
        let token = self.token.clone();
        let waiter_id = self.waiter_id;
        let no_foreground_turn = self
            .handle
            .ask(move |a| {
                a.end_stream(waiter_id, &token);
                a.view.active_foreground_turn_id.is_none()
            })
            .await;
        self.state = StreamState::Done;
        self.cleaned = true;
        if matches!(no_foreground_turn, Ok(true)) {
            self.manager.refresh_runtime_info(&self.handle, true).await;
        }
    }
}

impl Drop for RunStream {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        let (token, waiter_id) = (self.token.clone(), self.waiter_id);
        self.handle.tell(move |a| a.end_stream(waiter_id, &token));
    }
}

impl AgentManager {
    /// `streamAgent`: admission is atomic; the turn starts when the returned stream is polled.
    pub async fn stream_agent(
        &self,
        agent_id: &str,
        prompt: PromptInput,
        opts: RunOpts,
    ) -> Result<RunStream> {
        let handle = self.require_agent(agent_id)?;
        let admission = handle.ask(|a| a.admit_stream()).await??;
        Ok(RunStream {
            manager: self.clone(),
            token: admission.ticket.token.clone(),
            handle,
            waiter_id: None,
            cleaned: false,
            state: StreamState::Unpolled {
                prompt,
                opts,
                is_replacement: admission.is_replacement,
                settled: admission.ticket.settled,
            },
        })
    }

    /// `startPendingForegroundTurn` plus the accept block.
    async fn start_pending_foreground_turn(
        &self,
        handle: &ActorHandle,
        token: &str,
        prompt: &PromptInput,
        opts: &RunOpts,
        is_replacement: bool,
        settled: &watch::Receiver<bool>,
    ) -> Result<AcceptedTurn> {
        let agent_id = handle.id.clone();
        let reloaded = self
            .inner
            .reloaded_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&handle.generation);
        let started = if reloaded {
            let session_id = persistence_session_id(handle)
                .await
                .unwrap_or(agent_id.clone());
            Err(CoreError::StaleProviderSession { session_id })
        } else {
            handle
                .session
                .start_turn(prompt.clone(), opts.to_provider())
                .await
                .map_err(CoreError::from)
        };
        let turn = match started {
            Ok(turn) if *settled.borrow() => {
                let _ = turn;
                return Err(CoreError::msg(format!(
                    "Agent {agent_id} run was canceled before its turn started"
                )));
            }
            Ok(turn) => turn,
            Err(error) => {
                return Err(self
                    .handle_start_failure(handle, token, prompt, opts, settled, error)
                    .await)
            }
        };
        let (token_owned, prompt_owned) = (token.to_owned(), prompt.clone());
        let client_message_id = opts.client_message_id.clone();
        let turn_id = turn.turn_id.clone();
        let accepted = handle
            .ask(move |a| {
                a.accept_turn(
                    &token_owned,
                    &turn.turn_id,
                    &prompt_owned,
                    client_message_id.as_deref(),
                    is_replacement,
                )
            })
            .await?;
        Ok(AcceptedTurn {
            turn_id,
            waiter_id: accepted.waiter_id,
            events: accepted.events,
        })
    }

    async fn handle_start_failure(
        &self,
        handle: &ActorHandle,
        token: &str,
        prompt: &PromptInput,
        opts: &RunOpts,
        settled: &watch::Receiver<bool>,
        error: CoreError,
    ) -> CoreError {
        if *settled.borrow() {
            return error;
        }
        let reloaded = self
            .inner
            .reloaded_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&handle.generation);
        let persistence = persistence_session_id(handle).await;
        let stale = if error.is_stale_provider_session() {
            Some(error.to_string())
        } else if is_closed_provider_session_message(&error.to_string())
            && (persistence.is_some() || reloaded)
        {
            Some(
                CoreError::StaleProviderSession {
                    session_id: persistence.clone().unwrap_or_else(|| handle.id.clone()),
                }
                .to_string(),
            )
        } else {
            None
        };
        let token = token.to_owned();
        if let Some(message) = stale {
            let _ = handle
                .ask(move |a| a.start_failed_stale(&token, &message))
                .await;
            return CoreError::StaleProviderSession {
                session_id: persistence.unwrap_or_else(|| handle.id.clone()),
            };
        }
        let message = error.to_string();
        let (prompt, client_message_id) = (prompt.clone(), opts.client_message_id.clone());
        let _ = handle
            .ask(move |a| a.start_failed(&token, &message, &prompt, client_message_id.as_deref()))
            .await;
        error
    }

    /// `runAgent`: drain a stream into a result.
    pub async fn run_agent(
        &self,
        agent_id: &str,
        prompt: PromptInput,
        opts: RunOpts,
    ) -> Result<AgentRunOutcome> {
        let mut stream = self.stream_agent(agent_id, prompt, opts).await?;
        let mut timeline = Vec::new();
        let mut usage = None;
        let mut canceled = false;
        while let Some(event) = stream.next().await? {
            match event {
                AgentStreamEvent::Timeline { item, .. } => timeline.push(*item),
                AgentStreamEvent::TurnCompleted { usage: u, .. } => usage = u,
                AgentStreamEvent::TurnFailed {
                    error,
                    code,
                    diagnostic,
                    ..
                } => {
                    return Err(CoreError::Message(format_turn_failed_message(
                        &error,
                        code.as_deref(),
                        diagnostic.as_deref(),
                    )));
                }
                AgentStreamEvent::TurnCanceled { .. } => canceled = true,
                _ => {}
            }
        }
        let final_text = last_assistant_message(&timeline).unwrap_or_default();
        let handle = self.require_agent(agent_id)?;
        let session_id = persistence_session_id(&handle).await.ok_or_else(|| {
            CoreError::msg(format!(
                "Agent {agent_id} has no persistence.sessionId after run completed"
            ))
        })?;
        Ok(AgentRunOutcome {
            session_id,
            final_text,
            usage,
            timeline,
            canceled,
        })
    }
}

struct AcceptedTurn {
    turn_id: String,
    waiter_id: u64,
    events: mpsc::UnboundedReceiver<AgentStreamEvent>,
}

async fn persistence_session_id(handle: &ActorHandle) -> Option<String> {
    handle
        .ask(|a| a.view.persistence.as_ref().map(|p| p.session_id.clone()))
        .await
        .ok()
        .flatten()
}
