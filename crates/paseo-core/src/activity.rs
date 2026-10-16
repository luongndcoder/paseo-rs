//! `curateAgentActivity` (`agent/activity-curator.ts`): a timeline as concise text. Used for
//! the schedule run output; the fork-context builder of the same TS file is not needed here.

use paseo_agent_types::{AgentTimelineItem, ToolCallDetail, ToolCallItem};
use paseo_timeline::{project_rows, ProjectionMode, Row};
use paseo_tool_detail::names::is_likely_external_tool_name;
use paseo_tool_detail::{build_tool_call_display_model, ToolCallDisplayInput};
use serde_json::Value;

const MAX_TOOL_INPUT_CHARS: usize = 400;
const MAX_TOOL_SUMMARY_CHARS: usize = 200;

/// `ActivityCuratorOptions`.
#[derive(Debug, Clone, Default)]
pub struct ActivityOptions {
    pub max_items: usize,
    pub label_assistant_messages: bool,
    pub include_kinds: Option<Vec<&'static str>>,
    /// Defaults to true when `None`.
    pub include_external_tool_input: Option<bool>,
}

/// Slice a string to its first `units` UTF-16 code units, like `String.prototype.slice(0, n)`.
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

fn format_tool_call_entry(item: &ToolCallItem, options: &ActivityOptions) -> String {
    // PARITY-QUIRK(activity-null-input): a detail other than `unknown` has no input, and its
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
    if options.include_external_tool_input.unwrap_or(true)
        && is_likely_external_tool_name(&item.name)
    {
        if let Some(json) = input_json {
            return format!("[{}] {json}", display.display_name);
        }
    }
    match summary {
        Some(summary) => format!("[{}] {summary}", display.display_name),
        None => format!("[{}]", display.display_name),
    }
}

fn included(item: &AgentTimelineItem, options: &ActivityOptions) -> bool {
    options
        .include_kinds
        .as_ref()
        .is_none_or(|kinds| kinds.contains(&item.type_name()))
}

fn curate_entries(items: &[AgentTimelineItem], options: &ActivityOptions) -> Vec<String> {
    let recent = if options.max_items > 0 && items.len() > options.max_items {
        &items[items.len() - options.max_items..]
    } else {
        items
    };
    let mut entries = Vec::new();
    for item in recent.iter().filter(|i| included(i, options)) {
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
                } else if options.label_assistant_messages {
                    "[Assistant] "
                } else {
                    ""
                };
                entries.push(format!("{prefix}{text}"));
            }
            AgentTimelineItem::ToolCall(call) => {
                entries.push(format_tool_call_entry(call, options));
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

/// `curateAgentActivity`.
pub fn curate_agent_activity(timeline: &[AgentTimelineItem], options: &ActivityOptions) -> String {
    let rows: Vec<Row> = timeline
        .iter()
        .enumerate()
        .map(|(index, item)| Row::new(index as i64 + 1, "", item.clone()))
        .collect();
    let projected: Vec<AgentTimelineItem> = project_rows(&rows, ProjectionMode::Projected)
        .into_iter()
        .map(|entry| entry.item)
        .collect();
    let entries = curate_entries(&projected, options);
    if entries.is_empty() {
        "No activity to display.".to_owned()
    } else {
        entries.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paseo_agent_types::{ToolCallItem, ToolCallStatus};
    use serde_json::json;

    fn text(kind: &str, text: &str) -> AgentTimelineItem {
        match kind {
            "user" => AgentTimelineItem::UserMessage {
                text: text.into(),
                message_id: None,
                client_message_id: None,
            },
            "reasoning" => AgentTimelineItem::Reasoning { text: text.into() },
            _ => AgentTimelineItem::AssistantMessage {
                text: text.into(),
                message_id: None,
            },
        }
    }

    #[test]
    fn labels_and_empty_timeline() {
        assert_eq!(
            curate_agent_activity(&[], &ActivityOptions::default()),
            "No activity to display."
        );
        let out = curate_agent_activity(
            &[
                text("user", " hi "),
                text("reasoning", "hmm"),
                text("assistant", "  "),
            ],
            &ActivityOptions::default(),
        );
        assert_eq!(out, "[User] hi\n[Thought] hmm");
    }

    #[test]
    fn assistant_chunks_collapse_before_curation() {
        let out = curate_agent_activity(
            &[text("assistant", "foo "), text("assistant", "bar")],
            &ActivityOptions::default(),
        );
        assert_eq!(out, "foo bar");
    }

    #[test]
    fn external_tool_prints_its_input_unknown_detail() {
        let call = ToolCallItem::new(
            "c1",
            "mcp__server__tool",
            ToolCallStatus::Completed,
            ToolCallDetail::unknown(json!({"a": 1}), Value::Null),
        );
        let out = curate_agent_activity(
            &[AgentTimelineItem::tool_call(call)],
            &ActivityOptions::default(),
        );
        assert!(out.ends_with(r#"{"a":1}"#), "{out}");
    }

    #[test]
    fn max_items_keeps_the_tail_and_kinds_filter() {
        let items = [text("user", "a"), text("assistant", "b"), text("user", "c")];
        let out = curate_agent_activity(
            &items,
            &ActivityOptions {
                max_items: 2,
                include_kinds: Some(vec!["user_message"]),
                ..ActivityOptions::default()
            },
        );
        assert_eq!(out, "[User] c");
    }

    #[test]
    fn slicing_counts_utf16_units() {
        assert_eq!(slice_units("a\u{1F600}b", 2), "a");
        assert_eq!(slice_units("a\u{1F600}b", 3), "a\u{1F600}");
    }
}
