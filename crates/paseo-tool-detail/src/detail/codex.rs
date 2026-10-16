//! `codex/tool-call-detail-parser.ts`. The Codex mapper (`tool-call-mapper.ts`) is WP10.

use super::primitives::*;
use super::{branch, branch_in, speak_detail, unknown_detail};
use crate::names::strip_cwd_prefix;
use paseo_agent_types::{SearchToolName, ToolCallDetail};
use serde_json::Value;

/// `normalizeCodexFilePath`.
pub fn normalize_codex_file_path(file_path: &str, cwd: Option<&str>) -> Option<String> {
    if file_path.is_empty() {
        return None;
    }
    match cwd.filter(|c| !c.is_empty()) {
        Some(c) => Some(strip_cwd_prefix(file_path, Some(c))),
        None => Some(file_path.to_string()),
    }
}

fn pass2(
    name: &str,
    input: Option<&Value>,
    output: Option<&Value>,
    cwd: Option<&str>,
) -> Option<Option<ToolCallDetail>> {
    let norm = |p: &str| normalize_codex_file_path(p, cwd);
    match name {
        "Bash" | "shell" | "bash" | "exec" | "exec_command" | "command" => branch(
            input,
            output,
            parse_shell_input,
            parse_shell_output,
            to_shell_tool_detail,
        ),
        "read" | "read_file" => branch_in(input, parse_read_input, |i| {
            let parsed = output.and_then(|o| parse_read_output_with_path(o).ok());
            to_read_tool_detail(i, parsed.as_ref(), Some(&norm))
        }),
        "write" | "write_file" | "create_file" => branch(
            input,
            output,
            parse_write_input,
            parse_write_output,
            |i, o| to_write_tool_detail(i, o, Some(&norm)),
        ),
        "edit" | "apply_patch" | "apply_diff" => branch_in(input, parse_edit_input, |i| {
            // CodexLooseEditOutputSchema: never fails, null when it does not parse
            let parsed = match output {
                Some(Value::Null) | None => None,
                Some(o) => parse_edit_output(o).ok(),
            };
            to_edit_tool_detail(i, parsed.as_ref(), Some(&norm))
        }),
        "search" => branch_in(input, parse_search_input, |i| {
            to_search_tool_detail(i.map(String::as_str), None, Some(SearchToolName::Search))
        }),
        "web_search" => branch_in(input, parse_search_input, |i| {
            to_search_tool_detail(i.map(String::as_str), None, Some(SearchToolName::WebSearch))
        }),
        "speak" => speak_detail(input),
        _ => None,
    }
}

/// `deriveCodexToolDetail({name, input, output, cwd})`.
pub fn derive_codex_tool_detail(
    name: &str,
    input: Option<&Value>,
    output: Option<&Value>,
    cwd: Option<&str>,
) -> ToolCallDetail {
    let null = Value::Null;
    let input = Some(input.unwrap_or(&null));
    let output = Some(output.unwrap_or(&null));
    if name.is_empty() {
        return unknown_detail(input, output);
    }
    match pass2(name, input, output, cwd) {
        Some(Some(d)) => d,
        _ => unknown_detail(input, output),
    }
}
