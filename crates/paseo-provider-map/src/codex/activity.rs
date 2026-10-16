//! `curateAgentActivity` with `labelAssistantMessages` (the sub-agent `log` text).
//!
//! Private copy of `paseo-core::activity` restricted to what the Codex sub-agent log needs;
//! `paseo-core` is not a dependency of this crate. Fold both into one crate when the curator
//! moves below `paseo-core`.

use paseo_agent_types::{AgentTimelineItem, ToolCallDetail, ToolCallItem};
use paseo_timeline::{project_rows, ProjectionMode, Row};
use paseo_tool_detail::names::is_likely_external_tool_name;
use paseo_tool_detail::{build_tool_call_display_model, ToolCallDisplayInput};
use serde_json::Value;

const MAX_TOOL_INPUT_CHARS: usize = 400;
const MAX_TOOL_SUMMARY_CHARS: usize = 200;

fn slice_units(text: &str, units: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for ch in text.chars() {
        used += ch.len_utf16();
        if used > units {
            break;
        }
        out.push(ch);
    }
    out
}

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

fn format_tool_input_json(input: &Value) -> Option<String> {
    let encoded = serde_json::to_string(input).ok()?;
    if encoded.is_empty() {
        return None;
    }
    if utf16_len(&encoded) <= MAX_TOOL_INPUT_CHARS {
        return Some(encoded);
    }
    Some(format!(
        "{}...",
        slice_units(&encoded, MAX_TOOL_INPUT_CHARS)
    ))
}

fn format_tool_summary(summary: Option<&str>) -> Option<String> {
    let normalized = summary?.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    if utf16_len(&normalized) <= MAX_TOOL_SUMMARY_CHARS {
        return Some(normalized);
    }
    Some(format!(
        "{}...",
        slice_units(&normalized, MAX_TOOL_SUMMARY_CHARS - 3)
    ))
}

fn format_tool_call_entry(item: &ToolCallItem) -> String {
    // PARITY-QUIRK(activity-null-input): a non-`unknown` detail has no input, and
    // `JSON.stringify(null)` is the non-empty string "null", so an external tool prints it.
    let input = match &item.detail {
        ToolCallDetail::Unknown { input, .. } => input.clone(),
        _ => Value::Null,
    };
    let input_json = format_tool_input_json(&input);
    let display = build_tool_call_display_model(&ToolCallDisplayInput {
        name: &item.name,
        status: item.status,
        error: item.error.as_ref(),
        metadata: item.metadata.as_ref(),
        detail: &item.detail,
        agent_message: None,
        cwd: None,
    });
    let summary = format_tool_summary(display.summary.as_deref());
    if is_likely_external_tool_name(&item.name) {
        if let Some(json) = input_json {
            return format!("[{}] {json}", display.display_name);
        }
    }
    match summary {
        Some(s) => format!("[{}] {s}", display.display_name),
        None => format!("[{}]", display.display_name),
    }
}

fn curate_entries(items: &[AgentTimelineItem]) -> Vec<String> {
    let mut entries = Vec::new();
    for item in items {
        match item {
            AgentTimelineItem::UserMessage { text, .. } => {
                entries.push(format!("[User] {}", text.trim()));
            }
            AgentTimelineItem::AssistantMessage { text, .. }
            | AgentTimelineItem::Reasoning { text } => {
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                let prefix = if matches!(item, AgentTimelineItem::Reasoning { .. }) {
                    "[Thought] "
                } else {
                    "[Assistant] "
                };
                entries.push(format!("{prefix}{text}"));
            }
            AgentTimelineItem::ToolCall(call) => {
                entries.push(format_tool_call_entry(call));
                if let ToolCallDetail::SubAgent { log, .. } = &call.detail {
                    if !log.trim().is_empty() {
                        entries.push(log.trim().to_owned());
                    }
                }
            }
            AgentTimelineItem::Todo { items } => {
                entries.push("[Tasks]".to_owned());
                for task in items {
                    let checkbox = if task.completed { "[x]" } else { "[ ]" };
                    entries.push(format!("- {checkbox} {}", task.text));
                }
            }
            AgentTimelineItem::Error { message } => entries.push(format!("[Error] {message}")),
            AgentTimelineItem::Compaction { .. } => entries.push("[Compacted]".to_owned()),
            AgentTimelineItem::Notification { .. } | AgentTimelineItem::Plugin { .. } => {}
        }
    }
    entries
}

/// `curateAgentActivity(timeline, {labelAssistantMessages: true})`.
pub fn curate_labeled_activity(timeline: &[AgentTimelineItem]) -> String {
    let rows: Vec<Row> = timeline
        .iter()
        .enumerate()
        .map(|(i, item)| Row::new(i as i64 + 1, "", item.clone()))
        .collect();
    let projected: Vec<AgentTimelineItem> = project_rows(&rows, ProjectionMode::Projected)
        .into_iter()
        .map(|e| e.item)
        .collect();
    let entries = curate_entries(&projected);
    if entries.is_empty() {
        "No activity to display.".to_owned()
    } else {
        entries.join("\n")
    }
}
