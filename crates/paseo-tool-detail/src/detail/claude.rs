//! `claude/tool-call-detail-parser.ts` and `claude/tool-call-mapper.ts`.

use super::primitives::*;
use super::{branch, branch_in, skill_output, speak_detail, unknown_detail};
use crate::js::js_trim;
use crate::names::is_speak_tool_name;
use paseo_agent_types::{
    Map, SearchToolName, ToolCallDetail, ToolCallIcon, ToolCallItem, ToolCallStatus,
};
use serde_json::{json, Value};

fn parse_claude_grep(v: &Value) -> P<SearchOut> {
    if let Ok(g) = parse_grep_output(v) {
        return Ok(SearchOut::Grep(g));
    }
    let m = obj(v)?;
    Ok(SearchOut::Grep(grep_from_text(req_s(m, "output")?)))
}

fn read_branch(input: Option<&Value>, output: Option<&Value>) -> Option<Option<ToolCallDetail>> {
    branch_in(input, parse_read_input, |i| {
        let parsed = output.and_then(|o| parse_read_output(o).ok());
        to_read_tool_detail(i, parsed.as_ref(), None)
    })
}

fn pass2(
    name: &str,
    input: Option<&Value>,
    output: Option<&Value>,
) -> Option<Option<ToolCallDetail>> {
    match name {
        "Bash" | "bash" | "shell" | "exec_command" => branch(
            input,
            output,
            parse_shell_input,
            parse_shell_output,
            to_shell_tool_detail,
        ),
        "Read" | "read" | "read_file" | "view_file" => read_branch(input, output),
        "Write" | "write" | "write_file" | "create_file" => branch(
            input,
            output,
            parse_write_input,
            parse_write_output,
            |i, o| to_write_tool_detail(i, o, None),
        ),
        "Edit" | "MultiEdit" | "multi_edit" | "edit" | "apply_patch" | "apply_diff"
        | "str_replace_editor" => branch(
            input,
            output,
            parse_edit_input,
            parse_edit_output,
            |i, o| to_edit_tool_detail(i, o, None),
        ),
        "WebSearch" | "web_search" => branch(
            input,
            output,
            parse_search_input,
            |v| parse_web_search_output(v).map(SearchOut::Web),
            |i, o| to_search_tool_detail(i.map(String::as_str), o, Some(SearchToolName::WebSearch)),
        ),
        "search" => branch_in(input, parse_search_input, |i| {
            to_search_tool_detail(i.map(String::as_str), None, Some(SearchToolName::Search))
        }),
        "Grep" | "grep" => branch(
            input,
            output,
            parse_search_input,
            parse_claude_grep,
            |i, o| to_search_tool_detail(i.map(String::as_str), o, Some(SearchToolName::Grep)),
        ),
        "Glob" | "glob" => branch(
            input,
            output,
            parse_search_input,
            |v| parse_glob_output(v).map(SearchOut::Glob),
            |i, o| to_search_tool_detail(i.map(String::as_str), o, Some(SearchToolName::Glob)),
        ),
        "WebFetch" | "web_fetch" | "WebFetchTool" | "web_fetch_tool" | "webfetch" => branch(
            input,
            output,
            parse_web_fetch_input,
            parse_web_fetch_output,
            to_fetch_tool_detail,
        ),
        "Skill" => branch(
            input,
            output,
            |v| {
                let m = obj(v)?;
                Ok(req_s(m, "skill")?.to_string())
            },
            skill_output,
            |i, o| {
                let skill = i.filter(|s| !s.is_empty())?;
                Some(ToolCallDetail::PlainText {
                    label: Some(skill.clone()),
                    text: o.cloned().filter(|t| !t.is_empty()),
                    icon: Some(ToolCallIcon::Sparkles),
                })
            },
        ),
        "speak" => speak_detail(input),
        _ => None,
    }
}

/// `deriveClaudeToolDetail(name, input, output)`; `None` is JS `undefined`.
pub fn derive_claude_tool_detail(
    name: &str,
    input: Option<&Value>,
    output: Option<&Value>,
) -> ToolCallDetail {
    let null = Value::Null;
    let input = Some(input.unwrap_or(&null));
    let output = Some(output.unwrap_or(&null));
    if name.is_empty() {
        return unknown_detail(input, output);
    }
    match pass2(name, input, output) {
        Some(Some(d)) => d,
        _ => unknown_detail(input, output),
    }
}

// ---- mapper ------------------------------------------------------------------------------------

pub struct ClaudeToolCallParams<'a> {
    pub call_id: Option<&'a str>,
    pub name: &'a str,
    pub input: Option<&'a Value>,
    pub output: Option<&'a Value>,
    pub metadata: Option<&'a Map>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Shell,
    Read,
    Write,
    Edit,
    Search,
    Fetch,
    Speak,
    Unknown,
}

fn resolve_kind(name: &str) -> Kind {
    match name {
        "Bash" | "bash" | "shell" | "exec_command" => Kind::Shell,
        "Read" | "read" | "read_file" | "view_file" => Kind::Read,
        "Write" | "write" | "write_file" | "create_file" => Kind::Write,
        "Edit" | "MultiEdit" | "multi_edit" | "edit" | "apply_patch" | "apply_diff"
        | "str_replace_editor" => Kind::Edit,
        "WebSearch" | "web_search" | "search" | "Grep" | "grep" | "Glob" | "glob" => Kind::Search,
        "WebFetch" | "web_fetch" | "WebFetchTool" | "web_fetch_tool" | "webfetch" => Kind::Fetch,
        _ if is_speak_tool_name(name) => Kind::Speak,
        _ => Kind::Unknown,
    }
}

fn detail_name<'a>(kind: Kind, name: &'a str) -> &'a str {
    match kind {
        Kind::Shell => "shell",
        Kind::Read => "read_file",
        Kind::Write => "write_file",
        Kind::Edit => "apply_patch",
        Kind::Speak => "speak",
        _ => name,
    }
}

fn map_tool_call(
    p: &ClaudeToolCallParams,
    status: ToolCallStatus,
    error: Option<&Value>,
) -> Option<ToolCallItem> {
    if p.name.is_empty() {
        return None;
    }
    let call_id = p.call_id.filter(|c| !js_trim(c).is_empty())?.to_string();
    let trimmed = js_trim(p.name);

    if trimmed == "ExitPlanMode" {
        if let Some(Value::Object(im)) = p.input {
            if let Some(Value::String(plan)) = im.get("plan") {
                let mut metadata = p.metadata.cloned().unwrap_or_default();
                if matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed) {
                    metadata.insert(
                        "approved".into(),
                        Value::Bool(status == ToolCallStatus::Completed),
                    );
                }
                let mut item = ToolCallItem::new(
                    call_id,
                    if status == ToolCallStatus::Running {
                        trimmed.to_string()
                    } else {
                        "plan_approval".to_string()
                    },
                    if status == ToolCallStatus::Failed {
                        ToolCallStatus::Completed
                    } else {
                        status
                    },
                    ToolCallDetail::Plan { text: plan.clone() },
                );
                item.metadata = Some(metadata);
                return Some(item);
            }
        }
    }

    let kind = resolve_kind(trimmed);
    let name = if kind == Kind::Speak {
        "speak"
    } else {
        trimmed
    };
    let detail = derive_claude_tool_detail(detail_name(kind, name), p.input, p.output);

    let mut item = if status == ToolCallStatus::Failed {
        let err = match error {
            Some(e) if !e.is_null() => e.clone(),
            _ => json!({"message": "Tool call failed"}),
        };
        ToolCallItem::failed(call_id, name, detail, err)
    } else {
        ToolCallItem::new(call_id, name, status, detail)
    };
    item.metadata = p.metadata.cloned();
    Some(item)
}

pub fn map_claude_running_tool_call(p: &ClaudeToolCallParams) -> Option<ToolCallItem> {
    map_tool_call(p, ToolCallStatus::Running, None)
}

pub fn map_claude_completed_tool_call(p: &ClaudeToolCallParams) -> Option<ToolCallItem> {
    map_tool_call(p, ToolCallStatus::Completed, None)
}

pub fn map_claude_failed_tool_call(
    p: &ClaudeToolCallParams,
    error: &Value,
) -> Option<ToolCallItem> {
    map_tool_call(p, ToolCallStatus::Failed, Some(error))
}

pub fn map_claude_canceled_tool_call(p: &ClaudeToolCallParams) -> Option<ToolCallItem> {
    map_tool_call(p, ToolCallStatus::Canceled, None)
}
