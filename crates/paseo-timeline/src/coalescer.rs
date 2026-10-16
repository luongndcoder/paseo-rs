//! Agent stream coalescer (`agent-stream-coalescer.ts`) as a pure state machine.
//!
//! The caller owns the clock and the timer: pass `now` to every call, sleep until
//! [`Coalescer::next_deadline`], then call [`Coalescer::tick`]. Flushes come back as values; the
//! caller assigns seq, records the row, then dispatches.
//!
//! 60 ms leading edge plus trailing flush. Coalescable: assistant_message, reasoning, tool_call.
//! Empty text is swallowed. A terminal tool status flushes immediately. Tool entries merge by
//! callId in place; text concatenates only when buffer-adjacent with the same kind, assistant
//! messageId, provider and turnId. Never shrink the window for smoothing (client concern).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem, ToolCallItem, ToolCallStatus};
use paseo_tool_detail::merge_tool_call_items;

pub const DEFAULT_WINDOW: Duration = Duration::from_millis(60);

/// One coalesced timeline event ready to record and dispatch.
#[derive(Debug, Clone, PartialEq)]
pub struct Flush {
    pub agent_id: String,
    pub item: AgentTimelineItem,
    pub provider: String,
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone)]
enum Pending {
    Text {
        /// First chunk of the run: `messageId` and kind come from it.
        item: AgentTimelineItem,
        text: String,
        provider: String,
        turn_id: Option<String>,
    },
    Tool {
        item: Box<ToolCallItem>,
        provider: String,
        turn_id: Option<String>,
    },
}

#[derive(Debug, Default)]
struct Buffer {
    entries: Vec<Pending>,
    tool_call_indexes: HashMap<String, usize>,
    /// Trailing-flush deadline and its schedule order (FIFO on equal deadlines).
    deadline: Option<(Instant, u64)>,
    last_flush_at: Option<Instant>,
}

#[derive(Debug)]
pub struct Coalescer {
    window: Duration,
    buffers: IndexMap<String, Buffer>,
    schedule_counter: u64,
}

impl Default for Coalescer {
    fn default() -> Self {
        Self::new(DEFAULT_WINDOW)
    }
}

/// True for the events [`Coalescer::handle`] buffers (TS `handle` returning `true`). Everything
/// else must be dispatched by the caller.
pub fn is_coalescable(event: &AgentStreamEvent) -> bool {
    matches!(
        event,
        AgentStreamEvent::Timeline { item, .. } if matches!(
            **item,
            AgentTimelineItem::AssistantMessage { .. }
                | AgentTimelineItem::Reasoning { .. }
                | AgentTimelineItem::ToolCall(_)
        )
    )
}

fn is_terminal(status: ToolCallStatus) -> bool {
    matches!(
        status,
        ToolCallStatus::Completed | ToolCallStatus::Failed | ToolCallStatus::Canceled
    )
}

fn is_same_text_stream(prev: &AgentTimelineItem, next: &AgentTimelineItem) -> bool {
    match (prev, next) {
        (
            AgentTimelineItem::AssistantMessage { message_id: a, .. },
            AgentTimelineItem::AssistantMessage { message_id: b, .. },
        ) => a == b,
        (AgentTimelineItem::Reasoning { .. }, AgentTimelineItem::Reasoning { .. }) => true,
        _ => false,
    }
}

fn with_text(item: &AgentTimelineItem, text: &str) -> AgentTimelineItem {
    match item {
        AgentTimelineItem::AssistantMessage { message_id, .. } => {
            AgentTimelineItem::AssistantMessage {
                text: text.to_string(),
                message_id: message_id.clone(),
            }
        }
        AgentTimelineItem::Reasoning { .. } => AgentTimelineItem::Reasoning {
            text: text.to_string(),
        },
        other => other.clone(),
    }
}

impl Coalescer {
    pub fn new(window: Duration) -> Self {
        Coalescer {
            window,
            buffers: IndexMap::new(),
            schedule_counter: 0,
        }
    }

    /// Feed one event. Returns flushes due now (leading edge or terminal tool status). A
    /// non-coalescable event returns nothing; check [`is_coalescable`] first.
    pub fn handle(&mut self, agent_id: &str, event: &AgentStreamEvent, now: Instant) -> Vec<Flush> {
        let AgentStreamEvent::Timeline {
            item,
            provider,
            turn_id,
            ..
        } = event
        else {
            return Vec::new();
        };
        if !is_coalescable(event) {
            return Vec::new();
        }
        if let AgentTimelineItem::AssistantMessage { text, .. }
        | AgentTimelineItem::Reasoning { text } = &**item
        {
            if text.is_empty() {
                return Vec::new();
            }
        }

        let buffer = self.buffers.entry(agent_id.to_string()).or_default();
        append_to_buffer(buffer, item, provider, turn_id);

        let terminal = matches!(&**item, AgentTimelineItem::ToolCall(t) if is_terminal(t.status));
        if terminal {
            return self.flush_buffer(agent_id, now);
        }

        // Leading edge: the first event after an idle window flushes at once so the first token
        // of a turn is not delayed a full window. Sustained bursts fall through to the timer.
        if buffer.deadline.is_none() {
            let elapsed_ok = buffer
                .last_flush_at
                .is_none_or(|last| now.saturating_duration_since(last) >= self.window);
            if elapsed_ok {
                return self.flush_buffer(agent_id, now);
            }
            self.schedule_counter += 1;
            buffer.deadline = Some((now + self.window, self.schedule_counter));
        }
        Vec::new()
    }

    /// Earliest trailing-flush deadline across agents.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.buffers
            .values()
            .filter_map(|b| b.deadline.map(|(at, _)| at))
            .min()
    }

    /// Fire every trailing flush due at `now` (earliest first, FIFO on ties).
    pub fn tick(&mut self, now: Instant) -> Vec<Flush> {
        let mut due: Vec<(Instant, u64, String)> = self
            .buffers
            .iter()
            .filter_map(|(id, b)| {
                b.deadline
                    .filter(|(at, _)| *at <= now)
                    .map(|(at, seq)| (at, seq, id.clone()))
            })
            .collect();
        due.sort_by_key(|d| (d.0, d.1));
        due.into_iter()
            .flat_map(|(_, _, id)| self.flush_buffer(&id, now))
            .collect()
    }

    pub fn flush_for(&mut self, agent_id: &str, now: Instant) -> Vec<Flush> {
        self.flush_buffer(agent_id, now)
    }

    pub fn flush_all(&mut self, now: Instant) -> Vec<Flush> {
        let ids: Vec<String> = self.buffers.keys().cloned().collect();
        ids.iter()
            .flat_map(|id| self.flush_buffer(id, now))
            .collect()
    }

    /// Flush, then forget the agent (the next event is a leading edge). Use on close/delete.
    pub fn flush_and_discard(&mut self, agent_id: &str, now: Instant) -> Vec<Flush> {
        let flushes = self.flush_buffer(agent_id, now);
        self.buffers.shift_remove(agent_id);
        flushes
    }

    /// Number of armed trailing timers.
    pub fn pending_timers(&self) -> usize {
        self.buffers
            .values()
            .filter(|b| b.deadline.is_some())
            .count()
    }

    fn flush_buffer(&mut self, agent_id: &str, now: Instant) -> Vec<Flush> {
        let Some(buffer) = self.buffers.get_mut(agent_id) else {
            return Vec::new();
        };
        buffer.deadline = None;
        if buffer.entries.is_empty() {
            return Vec::new();
        }
        let entries = std::mem::take(&mut buffer.entries);
        buffer.tool_call_indexes.clear();
        buffer.last_flush_at = Some(now);
        collapse_entries(entries)
            .into_iter()
            .map(|entry| match entry {
                Pending::Text {
                    item,
                    text,
                    provider,
                    turn_id,
                } => Flush {
                    agent_id: agent_id.to_string(),
                    item: with_text(&item, &text),
                    provider,
                    turn_id,
                },
                Pending::Tool {
                    item,
                    provider,
                    turn_id,
                } => Flush {
                    agent_id: agent_id.to_string(),
                    item: AgentTimelineItem::ToolCall(item),
                    provider,
                    turn_id,
                },
            })
            .collect()
    }
}

fn append_to_buffer(
    buffer: &mut Buffer,
    item: &AgentTimelineItem,
    provider: &str,
    turn_id: &Option<String>,
) {
    match item {
        AgentTimelineItem::AssistantMessage { text, .. }
        | AgentTimelineItem::Reasoning { text } => {
            buffer.entries.push(Pending::Text {
                item: item.clone(),
                text: text.clone(),
                provider: provider.to_string(),
                turn_id: turn_id.clone(),
            });
        }
        AgentTimelineItem::ToolCall(incoming) => {
            let existing_index = buffer.tool_call_indexes.get(&incoming.call_id).copied();
            // Merge rather than replace, so an update that carries less (an unknown detail after
            // a labeled one) cannot erase what the timeline would have kept had both flushed.
            let merged = match existing_index.map(|i| &buffer.entries[i]) {
                Some(Pending::Tool { item: existing, .. }) => {
                    merge_tool_call_items(existing, incoming)
                }
                _ => (**incoming).clone(),
            };
            let entry = Pending::Tool {
                item: Box::new(merged),
                provider: provider.to_string(),
                turn_id: turn_id.clone(),
            };
            match existing_index {
                Some(i) => buffer.entries[i] = entry,
                None => {
                    buffer
                        .tool_call_indexes
                        .insert(incoming.call_id.clone(), buffer.entries.len());
                    buffer.entries.push(entry);
                }
            }
        }
        _ => {}
    }
}

fn collapse_entries(entries: Vec<Pending>) -> Vec<Pending> {
    let mut collapsed: Vec<Pending> = Vec::new();
    for entry in entries {
        if let (
            Some(Pending::Text {
                item: prev_item,
                text: prev_text,
                provider: prev_provider,
                turn_id: prev_turn,
            }),
            Pending::Text {
                item,
                text,
                provider,
                turn_id,
            },
        ) = (collapsed.last_mut(), &entry)
        {
            if is_same_text_stream(prev_item, item)
                && prev_provider == provider
                && prev_turn == turn_id
            {
                prev_text.push_str(text);
                continue;
            }
        }
        collapsed.push(entry);
    }
    collapsed
}
