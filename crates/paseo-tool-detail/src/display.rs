//! `buildToolCallDisplayModel` from `packages/protocol/src/tool-call-display.ts`
//! (+ `buildAgentMessageDisplay`).

use crate::js::{js_trim, json_stringify_pretty};
use crate::names::{get_paseo_tool_leaf_name, is_paseo_tool_name, strip_cwd_prefix};
use paseo_agent_types::{
    AgentMessage, AgentMessageEvent, Map, ToolCallDetail, ToolCallItem, ToolCallStatus,
};
use serde_json::Value;

pub struct ToolCallDisplayInput<'a> {
    pub name: &'a str,
    pub status: ToolCallStatus,
    pub error: Option<&'a Value>,
    pub metadata: Option<&'a Map>,
    pub detail: &'a ToolCallDetail,
    pub agent_message: Option<&'a AgentMessage>,
    pub cwd: Option<&'a str>,
}

impl<'a> ToolCallDisplayInput<'a> {
    pub fn from_item(item: &'a ToolCallItem, cwd: Option<&'a str>) -> Self {
        ToolCallDisplayInput {
            name: &item.name,
            status: item.status,
            error: item.error.as_ref(),
            metadata: item.metadata.as_ref(),
            detail: &item.detail,
            agent_message: item.agent_message.as_ref(),
            cwd,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ToolCallDisplayModel {
    pub display_name: String,
    pub summary: Option<String>,
    pub error_text: Option<String>,
}

#[derive(Default)]
struct DetailDisplay {
    display_name: Option<String>,
    summary: Option<String>,
}

fn read_string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn opt_non_empty(s: &Option<String>) -> Option<String> {
    s.clone().filter(|x| !x.is_empty())
}

/// `.replace(/^./, c => c.toUpperCase())`. `.` matches one UTF-16 unit and no line terminator,
/// so astral first characters (only half matched) are left alone.
fn upper_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if !matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}') && c.len_utf16() == 1 => {
            let mut out: String = c.to_uppercase().collect();
            out.push_str(chars.as_str());
            out
        }
        _ => s.to_string(),
    }
}

fn humanize_tool_name(name: &str) -> String {
    let trimmed = js_trim(name);
    if trimmed.is_empty() {
        return name.to_string();
    }
    if is_paseo_tool_name(trimmed) {
        if let Some(leaf) = get_paseo_tool_leaf_name(trimmed) {
            if !leaf.is_empty() {
                return humanize_tool_name(&leaf);
            }
        }
    }
    if trimmed.contains([':', '.', '/']) || trimmed.contains("__") {
        return trimmed.to_string();
    }
    // replace(/[._-]+/g, " ")
    let mut replaced = String::new();
    let mut in_run = false;
    for c in trimmed.chars() {
        if matches!(c, '.' | '_' | '-') {
            if !in_run {
                replaced.push(' ');
                in_run = true;
            }
        } else {
            replaced.push(c);
            in_run = false;
        }
    }
    let joined = replaced
        .split(' ')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    upper_first(&joined)
}

fn format_error_text(error: Option<&Value>) -> Option<String> {
    match error? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Object(o) if matches!(o.get("content"), Some(Value::String(_))) => {
            o.get("content").and_then(Value::as_str).map(String::from)
        }
        other => Some(json_stringify_pretty(other)),
    }
}

fn file_path_display(name: &str, file_path: &str, cwd: Option<&str>) -> DetailDisplay {
    DetailDisplay {
        display_name: Some(name.into()),
        summary: Some(strip_cwd_prefix(file_path, cwd)),
    }
}

fn canonical_detail_display(input: &ToolCallDisplayInput) -> DetailDisplay {
    match input.detail {
        ToolCallDetail::Shell { command, .. } => DetailDisplay {
            display_name: Some("Shell".into()),
            summary: Some(command.clone()),
        },
        ToolCallDetail::Read { file_path, .. } => file_path_display("Read", file_path, input.cwd),
        ToolCallDetail::Edit { file_path, .. } => file_path_display("Edit", file_path, input.cwd),
        ToolCallDetail::Write { file_path, .. } => file_path_display("Write", file_path, input.cwd),
        ToolCallDetail::Search(s) => DetailDisplay {
            display_name: Some("Search".into()),
            summary: Some(s.query.clone()),
        },
        ToolCallDetail::Fetch { url, .. } => DetailDisplay {
            display_name: Some("Fetch".into()),
            summary: Some(url.clone()),
        },
        ToolCallDetail::WorktreeSetup { branch_name, .. } => DetailDisplay {
            display_name: Some("Worktree setup".into()),
            summary: Some(branch_name.clone()),
        },
        ToolCallDetail::SubAgent {
            sub_agent_type,
            description,
            ..
        } => DetailDisplay {
            display_name: Some(opt_non_empty(sub_agent_type).unwrap_or_else(|| "Task".into())),
            summary: opt_non_empty(description),
        },
        ToolCallDetail::PlainText { label, .. } => DetailDisplay {
            display_name: None,
            summary: label.clone(),
        },
        ToolCallDetail::Plan { .. } => DetailDisplay {
            display_name: Some("Plan".into()),
            summary: None,
        },
        ToolCallDetail::Unknown { .. } => DetailDisplay::default(),
    }
}

fn unknown_detail_override(input: &ToolCallDisplayInput) -> DetailDisplay {
    let lower = js_trim(input.name).to_lowercase();
    let is_unknown = input.detail.is_unknown();
    if is_unknown && lower == "task" {
        return DetailDisplay {
            display_name: Some("Task".into()),
            summary: read_string(input.metadata.and_then(|m| m.get("subAgentActivity"))),
        };
    }
    if is_unknown && lower == "thinking" {
        return DetailDisplay {
            display_name: Some("Thinking".into()),
            summary: None,
        };
    }
    if lower == "terminal" {
        let summary = match input.detail {
            ToolCallDetail::PlainText { label, .. } => opt_non_empty(label),
            _ => None,
        };
        return DetailDisplay {
            display_name: Some("Terminal".into()),
            summary,
        };
    }
    DetailDisplay::default()
}

/// `buildAgentMessageDisplay(message).displayName`.
pub fn agent_message_display_name(m: &AgentMessage) -> String {
    let sender = m
        .sender_title
        .clone()
        .unwrap_or_else(|| m.sender_id.clone());
    match m.event {
        AgentMessageEvent::Message => format!("Message from {sender}"),
        AgentMessageEvent::Finished => format!("{sender} finished"),
        AgentMessageEvent::Errored => format!("{sender} errored"),
        AgentMessageEvent::PermissionRequired => format!("{sender} needs permission"),
        AgentMessageEvent::Closed => format!("{sender} closed"),
    }
}

pub fn build_tool_call_display_model(input: &ToolCallDisplayInput) -> ToolCallDisplayModel {
    if let Some(m) = input.agent_message {
        return ToolCallDisplayModel {
            display_name: agent_message_display_name(m),
            ..Default::default()
        };
    }
    let canonical = canonical_detail_display(input);
    let over = unknown_detail_override(input);
    let display_name = over
        .display_name
        .or(canonical.display_name)
        .unwrap_or_else(|| humanize_tool_name(input.name));
    let summary = over.summary.or(canonical.summary).filter(|s| !s.is_empty());
    let error_text = if input.status == ToolCallStatus::Failed {
        format_error_text(input.error).filter(|s| !s.is_empty())
    } else {
        None
    };
    ToolCallDisplayModel {
        display_name,
        summary,
        error_text,
    }
}
