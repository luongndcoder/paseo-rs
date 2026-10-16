//! `opencode/tool-call-detail-parser.ts` and `opencode/tool-call-mapper.ts`.

use super::primitives::*;
use super::{branch, branch_in, skill_output, unknown_detail};
use crate::gutter::{truncate_diff_text, DEFAULT_DIFF_MAX_CHARS};
use crate::js::{is_js_whitespace, js_trim, json_stringify_pretty};
use crate::status::normalize_tool_call_status;
use paseo_agent_types::{
    Map, SearchToolName, ToolCallDetail, ToolCallIcon, ToolCallItem, ToolCallStatus,
};
use serde_json::{json, Value};

fn nes_str(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// `nonEmptyString(value)?.trim().replace(/\s+/g, " ")`. May return `Some("")`.
fn normalize_sub_agent_text(v: Option<&Value>) -> Option<String> {
    let s = nes_str(v)?;
    let trimmed = js_trim(&s);
    let mut out = String::new();
    let mut in_ws = false;
    for c in trimmed.chars() {
        if is_js_whitespace(c) {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    Some(out)
}

fn read_output_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            let t = js_trim(s);
            (!t.is_empty()).then(|| t.to_string())
        }
        Value::Object(m) => ["output", "text", "content", "result"]
            .iter()
            .find_map(|k| m.get(*k).and_then(read_output_text)),
        _ => None,
    }
}

struct Directive {
    kind: &'static str,
    path: String,
}

fn parse_directive(line: &str) -> Option<Directive> {
    let t = js_trim(line);
    for (prefix, kind) in [
        ("*** Add File:", "add"),
        ("*** Update File:", "update"),
        ("*** Delete File:", "delete"),
    ] {
        if let Some(rest) = t.strip_prefix(prefix) {
            return Some(Directive {
                kind,
                path: js_trim(rest).to_string(),
            });
        }
    }
    None
}

fn extract_primary_file_path(patch: &str) -> Option<String> {
    patch
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter_map(parse_directive)
        .find(|d| !d.path.is_empty())
        .map(|d| d.path)
}

/// `rawPath.trim().replace(/^["']+|["']+$/g, "")`
fn normalize_diff_header_path(raw: &str) -> String {
    js_trim(raw)
        .trim_start_matches(['"', '\''])
        .trim_end_matches(['"', '\''])
        .to_string()
}

fn apply_patch_to_unified_diff(patch: &str) -> String {
    let normalized = patch.replace("\r\n", "\n");
    let mut out: Vec<String> = Vec::new();
    let mut saw = false;
    for line in normalized.split('\n') {
        if let Some(d) = parse_directive(line) {
            let file = normalize_diff_header_path(&d.path);
            if !file.is_empty() {
                if out.last().is_some_and(|l| !l.is_empty()) {
                    out.push(String::new());
                }
                let left = if d.kind == "add" {
                    "/dev/null".to_string()
                } else {
                    format!("a/{file}")
                };
                let right = if d.kind == "delete" {
                    "/dev/null".to_string()
                } else {
                    format!("b/{file}")
                };
                out.push(format!("diff --git a/{file} b/{file}"));
                out.push(format!("--- {left}"));
                out.push(format!("+++ {right}"));
                saw = true;
            }
            continue;
        }
        let t = js_trim(line);
        if t == "*** Begin Patch"
            || t == "*** End Patch"
            || t == "*** End of File"
            || t.starts_with("*** Move to:")
        {
            continue;
        }
        if line.starts_with("@@")
            || line.starts_with('+')
            || line.starts_with('-')
            || line.starts_with(' ')
            || line.starts_with("\\ No newline at end of file")
        {
            out.push(line.to_string());
            saw = true;
        }
    }
    if !saw {
        return patch.to_string();
    }
    let joined = out.join("\n");
    let t = js_trim(&joined);
    if t.is_empty() {
        patch.to_string()
    } else {
        t.to_string()
    }
}

fn parse_patch_text_input(v: &Value) -> P<EditVal> {
    let m = obj(v)?;
    let patch = req_s(m, "patchText")?;
    let file_path = extract_primary_file_path(patch).unwrap_or_default();
    Ok(EditVal {
        file_path: Some(file_path),
        old_string: None,
        new_string: None,
        unified_diff: truncate_diff_text(
            Some(&apply_patch_to_unified_diff(patch)),
            DEFAULT_DIFF_MAX_CHARS,
        ),
    })
}

fn parse_opencode_grep(v: &Value) -> P<SearchOut> {
    if let Ok(g) = parse_grep_output(v) {
        return Ok(SearchOut::Grep(g));
    }
    match v {
        Value::String(s) => Ok(SearchOut::Grep(grep_from_text(s))),
        _ => Err(()),
    }
}

fn nes(s: Option<&str>) -> Option<String> {
    s.filter(|x| !x.is_empty()).map(String::from)
}

fn parse_opencode_edit_input(v: &Value) -> P<EditVal> {
    let m = obj(v)?;
    // 1. {filePath: string, oldString?: string, newString?: string}
    if let Ok(fp) = req_s(m, "filePath") {
        if let (Ok(o), Ok(n)) = (opt_s(m, "oldString"), opt_s(m, "newString")) {
            return Ok(EditVal {
                file_path: Some(fp.to_string()),
                old_string: nes(o),
                new_string: nes(n),
                unified_diff: None,
            });
        }
    }
    // 2. v2: {path, oldString, newString} all required strings
    if let (Ok(p), Ok(o), Ok(n)) = (
        req_s(m, "path"),
        req_s(m, "oldString"),
        req_s(m, "newString"),
    ) {
        return Ok(EditVal {
            file_path: Some(p.to_string()),
            old_string: nes(Some(o)),
            new_string: nes(Some(n)),
            unified_diff: None,
        });
    }
    // 3. generic edit input
    parse_edit_input(v)
}

/// `z.union([z.string().transform(() => null), ToolEditOutputSchema])`; `Ok(None)` is the null.
fn parse_opencode_edit_output(v: &Value) -> P<Option<EditVal>> {
    if v.is_string() {
        return Ok(None);
    }
    parse_edit_output(v).map(Some)
}

fn format_log_entry(v: Option<&Value>) -> Option<String> {
    let v = v?;
    if let Some(t) = read_output_text(v) {
        return Some(t);
    }
    if v.is_null() {
        return None;
    }
    Some(json_stringify_pretty(v))
}

fn extract_task_session_id(v: Option<&Value>) -> Option<String> {
    let v = v?;
    if let Some(text) = read_output_text(v) {
        if let Some(id) = match_task_id(&text) {
            return Some(id);
        }
    }
    let m = v.as_object()?;
    // `a ?? b ?? ...` over possibly-empty normalized strings
    for k in ["task_id", "taskId", "sessionID", "sessionId"] {
        if let Some(s) = normalize_sub_agent_text(m.get(k)) {
            return Some(s);
        }
    }
    for k in ["output", "text", "content", "result"] {
        if let Some(s) = extract_task_session_id(m.get(k)) {
            return Some(s);
        }
    }
    None
}

/// `/\btask_id:\s*(ses_[A-Za-z0-9]+)/i`
fn match_task_id(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find("task_id:") {
        let start = from + rel;
        // \b: previous char must be a non-word char (or start)
        let boundary = text[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
        if boundary {
            let after = text[start + "task_id:".len()..].trim_start_matches(is_js_whitespace);
            if after
                .get(..4)
                .is_some_and(|p| p.eq_ignore_ascii_case("ses_"))
            {
                let id: String = after[4..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                if !id.is_empty() {
                    return Some(format!("{}{}", &after[..4], id));
                }
            }
        }
        from = start + 1;
    }
    None
}

fn nullish_or<'a>(a: Option<&'a Value>, b: Option<&'a Value>) -> Option<&'a Value> {
    match a {
        Some(Value::Null) | None => b,
        x => x,
    }
}

fn derive_task_detail(
    input: Option<&Value>,
    output: Option<&Value>,
    error: Option<&Value>,
    metadata: Option<&Map>,
) -> Option<ToolCallDetail> {
    let m = input?.as_object()?;
    let sub_agent_type =
        normalize_sub_agent_text(nullish_or(m.get("subagent_type"), m.get("subAgentType")));
    let description = normalize_sub_agent_text(m.get("description"));
    if sub_agent_type.is_none() && description.is_none() {
        return None;
    }
    let log = [format_log_entry(output), format_log_entry(error)]
        .into_iter()
        .flatten()
        .filter(|e| !e.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let child = normalize_sub_agent_text(metadata.and_then(|md| md.get("sessionId")))
        .or_else(|| extract_task_session_id(output));
    Some(ToolCallDetail::SubAgent {
        sub_agent_type: sub_agent_type.filter(|s| !s.is_empty()),
        description: description.filter(|s| !s.is_empty()),
        child_session_id: child.filter(|s| !s.is_empty()),
        log,
        actions: Some(vec![]),
    })
}

fn pass2(
    tool_name: &str,
    input: Option<&Value>,
    output: Option<&Value>,
) -> Option<Option<ToolCallDetail>> {
    match tool_name {
        "shell" | "bash" | "exec_command" => branch(
            input,
            output,
            parse_shell_input,
            parse_shell_output,
            to_shell_tool_detail,
        ),
        "read" | "read_file" => branch_in(input, parse_read_input, |i| {
            let parsed = output.and_then(|o| parse_read_output(o).ok());
            to_read_tool_detail(i, parsed.as_ref(), None)
        }),
        "write" | "write_file" | "create_file" => branch(
            input,
            output,
            parse_write_input,
            parse_write_output,
            |i, o| to_write_tool_detail(i, o, None),
        ),
        "edit" | "apply_diff" => edit_branch(input, output),
        "apply_patch" => edit_branch(input, output).or_else(|| patch_text_branch(input)),
        "patch" => patch_text_branch(input),
        "search" => branch_in(input, parse_search_input, |i| {
            to_search_tool_detail(i.map(String::as_str), None, Some(SearchToolName::Search))
        }),
        "grep" => branch(
            input,
            output,
            parse_search_input,
            parse_opencode_grep,
            |i, o| to_search_tool_detail(i.map(String::as_str), o, Some(SearchToolName::Grep)),
        ),
        "glob" => branch_in(input, parse_search_input, |i| {
            let parsed = output
                .and_then(|o| parse_glob_output(o).ok())
                .map(SearchOut::Glob);
            to_search_tool_detail(
                i.map(String::as_str),
                parsed.as_ref(),
                Some(SearchToolName::Glob),
            )
        }),
        "web_search" => branch_in(input, parse_search_input, |i| {
            to_search_tool_detail(i.map(String::as_str), None, Some(SearchToolName::WebSearch))
        }),
        "skill" => branch(
            input,
            output,
            |v| {
                let m = obj(v)?;
                Ok(req_s(m, "name")?.to_string())
            },
            skill_output,
            |i, o| {
                let name = js_trim(i?).to_string();
                if name.is_empty() {
                    return None;
                }
                Some(ToolCallDetail::PlainText {
                    label: Some(name),
                    text: o.cloned().filter(|t| !t.is_empty()),
                    icon: Some(ToolCallIcon::Sparkles),
                })
            },
        ),
        _ => None,
    }
}

fn edit_branch(input: Option<&Value>, output: Option<&Value>) -> Option<Option<ToolCallDetail>> {
    let i = nullable(input, parse_opencode_edit_input).ok()?;
    let o = nullable(output, parse_opencode_edit_output).ok()?.flatten();
    Some(to_edit_tool_detail(i.as_ref(), o.as_ref(), None))
}

fn patch_text_branch(input: Option<&Value>) -> Option<Option<ToolCallDetail>> {
    branch_in(input, parse_patch_text_input, |i| {
        to_edit_tool_detail(i, None, None)
    })
}

/// `deriveOpencodeToolDetail`. `None` is JS `undefined` (distinct from `Some(Null)`).
pub fn derive_opencode_tool_detail(
    tool_name: &str,
    input: Option<&Value>,
    output: Option<&Value>,
    error: Option<&Value>,
    metadata: Option<&Map>,
) -> ToolCallDetail {
    if js_trim(tool_name).to_lowercase() == "task" {
        if let Some(d) = derive_task_detail(input, output, error, metadata) {
            return d;
        }
    }
    if let Some(Some(d)) = pass2(tool_name, input, output) {
        return d;
    }
    unknown_detail(input, output)
}

// ---- mapper ------------------------------------------------------------------------------------

pub struct OpencodeToolCallParams<'a> {
    pub tool_name: &'a str,
    pub call_id: Option<&'a str>,
    pub status: Option<&'a Value>,
    pub input: Option<&'a Value>,
    pub output: Option<&'a Value>,
    pub error: Option<&'a Value>,
    pub metadata: Option<&'a Map>,
}

pub fn map_opencode_tool_call(p: &OpencodeToolCallParams) -> Option<ToolCallItem> {
    if p.tool_name.is_empty() {
        return None;
    }
    let call_id = p
        .call_id
        .map(js_trim)
        .filter(|c| !c.is_empty())?
        .to_string();
    let name = js_trim(p.tool_name).to_string();
    let null = Value::Null;
    let input = Some(p.input.unwrap_or(&null));
    let output = Some(p.output.unwrap_or(&null));
    let error = Some(p.error.unwrap_or(&null));
    let raw_status = p.status.and_then(Value::as_str);
    let status = normalize_tool_call_status(raw_status, error, output);
    let detail = derive_opencode_tool_detail(&name, input, output, error, p.metadata);

    let mut item = if status == ToolCallStatus::Failed {
        let e = error.filter(|e| !e.is_null()).cloned();
        ToolCallItem::failed(
            call_id,
            name,
            detail,
            e.unwrap_or_else(|| json!({"message": "Tool call failed"})),
        )
    } else {
        ToolCallItem::new(call_id, name, status, detail)
    };
    item.metadata = p.metadata.cloned();
    Some(item)
}

#[cfg(test)]
mod task_id_boundary_tests {
    use super::match_task_id;

    #[test]
    fn non_ascii_after_task_id_does_not_panic() {
        assert_eq!(match_task_id("task_id: abcé"), None);
        assert_eq!(
            match_task_id("task_id: ses_ab1"),
            Some("ses_ab1".to_string())
        );
    }
}
