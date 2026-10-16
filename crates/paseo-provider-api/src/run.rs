//! Shared turn runner: the Rust form of TS `runProviderTurn` (`providers/provider-runner.ts`).

use std::future::Future;

use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem, AgentUsage};

use crate::error::{ProviderError, Result};
use crate::events::EventStream;
use crate::types::{RunResult, TurnHandle};

/// Folds timeline items into the final text.
pub type FinalTextReducer = fn(current: &str, item: &AgentTimelineItem) -> String;

/// `replaceFinalTextWithAssistantMessage`: the last assistant message wins.
pub fn replace_with_assistant_message(current: &str, item: &AgentTimelineItem) -> String {
    match item {
        AgentTimelineItem::AssistantMessage { text, .. } => text.clone(),
        _ => current.to_owned(),
    }
}

/// `appendOrReplaceGrowingAssistantMessage`: a message that extends the text so far replaces
/// it, anything else appends.
pub fn append_or_replace_growing(current: &str, item: &AgentTimelineItem) -> String {
    let AgentTimelineItem::AssistantMessage { text, .. } = item else {
        return current.to_owned();
    };
    if current.is_empty() {
        return text.clone();
    }
    if text.starts_with(current) {
        text.clone()
    } else {
        format!("{current}{text}")
    }
}

enum Step {
    Continue,
    Completed(Option<AgentUsage>),
    Canceled,
    Failed(String),
}

struct Accumulator {
    reduce: FinalTextReducer,
    timeline: Vec<AgentTimelineItem>,
    final_text: String,
}

impl Accumulator {
    fn process(&mut self, turn_id: &str, event: AgentStreamEvent) -> Step {
        if let Some(event_turn) = event.turn_id() {
            if event_turn != turn_id {
                return Step::Continue;
            }
        }
        match event {
            AgentStreamEvent::Timeline { item, .. } => {
                self.final_text = (self.reduce)(&self.final_text, &item);
                self.timeline.push(*item);
                Step::Continue
            }
            AgentStreamEvent::TurnCompleted { usage, .. } => Step::Completed(usage),
            AgentStreamEvent::TurnFailed { error, .. } => Step::Failed(error),
            AgentStreamEvent::TurnCanceled { .. } => Step::Canceled,
            _ => Step::Continue,
        }
    }
}

/// Run one turn to its terminal event.
///
/// `events` must already be subscribed (take the stream before calling `start_turn` so no
/// event is missed). Events that arrive before `start_turn` resolves are buffered, then
/// replayed against the returned turn id; events for other turns are ignored.
/// `turn_failed` is an error, `turn_canceled` resolves with what was collected.
///
/// PARITY-QUIRK(run-canceled-unflagged): TS resolves a canceled turn without marking the
/// result canceled; the result here carries no flag either.
pub async fn run_provider_turn<S, I>(
    events: &mut EventStream,
    start_turn: S,
    session_id: I,
    reduce: FinalTextReducer,
) -> Result<RunResult>
where
    S: Future<Output = Result<TurnHandle>>,
    I: Future<Output = Result<String>>,
{
    let mut start_turn = std::pin::pin!(start_turn);
    let mut buffered: Vec<AgentStreamEvent> = Vec::new();
    let handle = loop {
        tokio::select! {
            started = &mut start_turn => break started?,
            event = events.next() => match event {
                Some(event) => buffered.push(event),
                None => return Err(ProviderError::StreamClosed),
            },
        }
    };
    let mut acc = Accumulator {
        reduce,
        timeline: Vec::new(),
        final_text: String::new(),
    };
    let mut outcome = None;
    for event in buffered {
        match acc.process(&handle.turn_id, event) {
            Step::Continue => {}
            step => {
                outcome = Some(step);
                break;
            }
        }
    }
    while outcome.is_none() {
        let Some(event) = events.next().await else {
            return Err(ProviderError::StreamClosed);
        };
        match acc.process(&handle.turn_id, event) {
            Step::Continue => {}
            step => outcome = Some(step),
        }
    }
    let usage = match outcome {
        Some(Step::Failed(error)) => return Err(ProviderError::TurnFailed(error)),
        Some(Step::Completed(usage)) => usage,
        _ => None,
    };
    Ok(RunResult {
        session_id: session_id.await?,
        final_text: acc.final_text,
        usage,
        timeline: acc.timeline,
    })
}

/// [`run_provider_turn`] over an [`crate::AgentSession`]: starts the turn and reads the id.
pub async fn run_session_turn(
    session: &dyn crate::AgentSession,
    events: &mut EventStream,
    prompt: crate::PromptInput,
    options: Option<crate::RunOptions>,
    reduce: FinalTextReducer,
) -> Result<RunResult> {
    run_provider_turn(
        events,
        session.start_turn(prompt, options),
        async {
            session
                .id()
                .map(str::to_owned)
                .ok_or_else(|| ProviderError::Protocol("session has no id".into()))
        },
        reduce,
    )
    .await
}
