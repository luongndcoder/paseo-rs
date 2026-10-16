//! Port of `codex/tool-call-mapper.ts` plus the exec/patch notification mappers of
//! `codex-app-server-agent.ts` (`mapCodexExecNotificationToToolCall`,
//! `mapCodexPatchNotificationToToolCall`, ...).
//!
//! Everything works on `serde_json::Value` envelopes (`input`/`output` are `unknown` in TS) and
//! ends in `derive_codex_tool_detail`. `Value::Null` stands for both JS `null` and `undefined`
//! wherever the TS code folds them with `??`.

use paseo_agent_types::{AgentTimelineItem, Map, ToolCallDetail, ToolCallItem, ToolCallStatus};
use paseo_tool_detail::detail::codex::{derive_codex_tool_detail, normalize_codex_file_path};
use paseo_tool_detail::gutter::truncate_diff_text;
use paseo_tool_detail::js::{is_js_whitespace, js_trim};
use paseo_tool_detail::names::is_speak_tool_name;
use paseo_tool_detail::shell_output::extract_codex_shell_output;
use paseo_tool_detail::status::normalize_tool_call_status;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;

const DIFF_MAX: usize = 12_000;

/// JS `\s` as a regex class (the `regex` crate's `\s` is the Unicode White_Space set, which
/// differs from the ECMAScript set).
const WS: &str = r"[\t\n\x0B\f\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]";

fn re_unix() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"^(?:(?:/[^/\s]+)*/)?(?:zsh|bash|sh){WS}+-(?:lc|c){WS}+((?s:.)+)$"
        ))
        .expect("unix shell regex")
    })
}

fn re_windows() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r#"(?i)^(?:"[^"]*\\)?(?:pwsh|powershell|cmd)(?:\.exe)?"?(?:{WS}+-[A-Za-z]+(?:{WS}+[^-\s][^\s]*)?)*{WS}+(?:-Command|-c|/c){WS}+((?s:.)+)$"#
        ))
        .expect("windows shell regex")
    })
}

fn re_windows_program() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:^|\\)(?:pwsh|powershell|cmd)(?:\.exe)?$")
            .expect("windows program regex")
    })
}

fn re_windows_flag() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^(-command|-c|/c)$").expect("windows flag regex"))
}

// ---------------------------------------------------------------------------
// small JS-flavoured helpers
// ---------------------------------------------------------------------------

pub(crate) fn as_record(v: &Value) -> Option<&Map> {
    v.as_object()
}

fn non_blank(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !js_trim(s).is_empty() => Some(js_trim(s).to_string()),
        _ => None,
    }
}

fn non_empty_str(v: Option<&Value>) -> Option<&str> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s),
        _ => None,
    }
}

fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn nullable_object(m: Map) -> Value {
    if m.is_empty() {
        Value::Null
    } else {
        Value::Object(m)
    }
}

fn trim_start(s: &str) -> &str {
    s.trim_start_matches(is_js_whitespace)
}

fn split_lines_crlf(s: &str) -> Vec<&str> {
    // `split(/\r?\n/)`
    s.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

// ---------------------------------------------------------------------------
// shell command normalisation
// ---------------------------------------------------------------------------

fn strip_matching_edge_quotes(value: &str) -> &str {
    let quoted = (value.starts_with('"') && value.ends_with('"'))
        || (value.starts_with('\'') && value.ends_with('\''));
    if !quoted {
        return value;
    }
    if value.len() < 2 {
        return "";
    }
    &value[1..value.len() - 1]
}

fn unwrap_shell_command(command: &str) -> String {
    let trimmed = js_trim(command);
    if let Some(c) = re_unix().captures(trimmed) {
        return strip_matching_edge_quotes(js_trim(&c[1])).to_string();
    }
    if let Some(c) = re_windows().captures(trimmed) {
        return strip_matching_edge_quotes(js_trim(&c[1])).to_string();
    }
    trimmed.to_string()
}

fn is_windows_shell_command(command: &str) -> bool {
    let mut normalized = command;
    if normalized.starts_with('"') || normalized.starts_with('\'') {
        normalized = &normalized[1..];
    }
    if normalized.ends_with('"') || normalized.ends_with('\'') {
        normalized = &normalized[..normalized.len() - 1];
    }
    re_windows_program().is_match(normalized)
}

/// `normalizeCommandExecutionCommand`.
pub fn normalize_command_execution_command(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) => {
            let n = unwrap_shell_command(s);
            (!n.is_empty()).then_some(n)
        }
        Some(Value::Array(a)) => {
            let parts: Vec<&str> = a
                .iter()
                .filter_map(Value::as_str)
                .map(js_trim)
                .filter(|s| !s.is_empty())
                .collect();
            if parts.is_empty() {
                return None;
            }
            if parts.len() >= 3 && (parts[1] == "-lc" || parts[1] == "-c") {
                let u = js_trim(parts[2]);
                return (!u.is_empty()).then(|| u.to_string());
            }
            if parts.len() >= 3
                && is_windows_shell_command(parts[0])
                && re_windows_flag().is_match(parts[1])
            {
                let joined = parts[2..].join(" ");
                let u = js_trim(&joined);
                return (!u.is_empty()).then(|| strip_matching_edge_quotes(u).to_string());
            }
            Some(parts.join(" "))
        }
        _ => None,
    }
}

/// `normalizeCodexCommandValue` (adapter): `string | string[] | null`; the array arm yields the
/// parts, which every caller only tests for presence before it goes into `input.command`.
#[derive(Debug, Clone, PartialEq)]
pub enum CodexCommand {
    Text(String),
    Parts(Vec<String>),
}

impl CodexCommand {
    pub fn to_value(&self) -> Value {
        match self {
            Self::Text(s) => Value::String(s.clone()),
            Self::Parts(p) => Value::Array(p.iter().cloned().map(Value::String).collect()),
        }
    }
}

fn re_exec_wrapper() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"^(?:/bin/)?(?:zsh|bash|sh){WS}+-(?:lc|c){WS}+((?s:.)+)$"
        ))
        .expect("exec wrapper regex")
    })
}

pub fn normalize_codex_command_value(value: Option<&Value>) -> Option<CodexCommand> {
    match value {
        Some(Value::String(s)) => {
            let trimmed = js_trim(s);
            if trimmed.is_empty() {
                return None;
            }
            let Some(c) = re_exec_wrapper().captures(trimmed) else {
                return Some(CodexCommand::Text(trimmed.to_string()));
            };
            let candidate = js_trim(&c[1]);
            if candidate.is_empty() {
                return Some(CodexCommand::Text(trimmed.to_string()));
            }
            let quoted = (candidate.starts_with('"') && candidate.ends_with('"'))
                || (candidate.starts_with('\'') && candidate.ends_with('\''));
            if quoted {
                let inner = if candidate.len() < 2 {
                    ""
                } else {
                    &candidate[1..candidate.len() - 1]
                };
                return Some(CodexCommand::Text(inner.to_string()));
            }
            Some(CodexCommand::Text(candidate.to_string()))
        }
        Some(Value::Array(a)) => {
            let parts: Vec<String> = a
                .iter()
                .filter_map(Value::as_str)
                .map(|s| js_trim(s).to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if parts.is_empty() {
                return None;
            }
            if parts.len() >= 3 && (parts[1] == "-lc" || parts[1] == "-c") {
                return Some(CodexCommand::Text(parts[2].clone()));
            }
            Some(CodexCommand::Parts(parts))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// apply_patch / diff handling
// ---------------------------------------------------------------------------

fn looks_like_unified_diff(text: &str) -> bool {
    let n = trim_start(text);
    !n.is_empty()
        && (n.starts_with("diff --git")
            || n.starts_with("@@")
            || n.starts_with("--- ")
            || n.starts_with("+++ "))
}

#[derive(Clone, Copy, PartialEq)]
enum DirectiveKind {
    Add,
    Update,
    Delete,
}

fn parse_directive(line: &str) -> Option<(DirectiveKind, String)> {
    let t = js_trim(line);
    for (prefix, kind) in [
        ("*** Add File:", DirectiveKind::Add),
        ("*** Update File:", DirectiveKind::Update),
        ("*** Delete File:", DirectiveKind::Delete),
    ] {
        if t.starts_with(prefix) {
            // JS `replace` with a string pattern replaces the first occurrence only.
            let rest = t.replacen(prefix, "", 1);
            return Some((kind, js_trim(&rest).to_string()));
        }
    }
    None
}

fn extract_patch_primary_file_path(patch: &str) -> Option<String> {
    for line in split_lines_crlf(patch) {
        if let Some((_, path)) = parse_directive(line) {
            if !path.is_empty() {
                return Some(path);
            }
        }
    }
    None
}

fn looks_like_codex_apply_patch(text: &str) -> bool {
    let n = trim_start(text);
    if n.is_empty() {
        return false;
    }
    if n.starts_with("*** Begin Patch") {
        return true;
    }
    split_lines_crlf(text)
        .into_iter()
        .any(|l| parse_directive(l).is_some())
}

fn normalize_diff_header_path(raw: &str) -> String {
    let t = js_trim(raw);
    // `replace(/^["']+|["']+$/g, "")`
    let t = t.trim_start_matches(['"', '\'']);
    t.trim_end_matches(['"', '\'']).to_string()
}

fn codex_apply_patch_to_unified_diff(text: &str) -> String {
    let normalized_text = text.replace("\r\n", "\n");
    let mut output: Vec<String> = Vec::new();
    let mut saw = false;
    for line in normalized_text.split('\n') {
        if let Some((kind, raw)) = parse_directive(line) {
            let path = normalize_diff_header_path(&raw);
            if !path.is_empty() {
                if !output.is_empty() && output.last().map(String::as_str) != Some("") {
                    output.push(String::new());
                }
                let left = if kind == DirectiveKind::Add {
                    "/dev/null".to_string()
                } else {
                    format!("a/{path}")
                };
                let right = if kind == DirectiveKind::Delete {
                    "/dev/null".to_string()
                } else {
                    format!("b/{path}")
                };
                output.push(format!("diff --git a/{path} b/{path}"));
                output.push(format!("--- {left}"));
                output.push(format!("+++ {right}"));
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
            output.push(line.to_string());
            saw = true;
        }
    }
    if !saw {
        return text.to_string();
    }
    let joined = output.join("\n");
    let n = js_trim(&joined);
    if n.is_empty() {
        text.to_string()
    } else {
        n.to_string()
    }
}

/// `contentToDeletionDiff`.
///
/// PARITY-QUIRK(codex-deletion-diff-empty-lines): TS filters with
/// `lines.indexOf(l) < lines.length - 1`, so an empty line is kept unless the FIRST empty line
/// is the last element; one earlier empty line keeps every empty line, the trailing one included.
fn content_to_deletion_diff(file_path: &str, content: &str) -> String {
    let normalized = content.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let first_empty = lines.iter().position(|l| l.is_empty());
    let non_empty: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| !l.is_empty() || first_empty.is_some_and(|i| i < lines.len() - 1))
        .collect();
    let mut out = vec![
        format!("diff --git a/{file_path} b/{file_path}"),
        format!("--- a/{file_path}"),
        "+++ /dev/null".to_string(),
    ];
    if !non_empty.is_empty() {
        out.push(format!("@@ -1,{} +0,0 @@", non_empty.len()));
        for l in non_empty {
            out.push(format!("-{l}"));
        }
    }
    out.join("\n")
}

/// `classifyDiffLikeText`: `(is_diff, text)`.
fn classify_diff_like_text(text: &str) -> (bool, String) {
    if looks_like_unified_diff(text) {
        return (true, text.to_string());
    }
    if looks_like_codex_apply_patch(text) {
        return (true, codex_apply_patch_to_unified_diff(text));
    }
    (false, text.to_string())
}

#[derive(Default)]
struct EditTextFields {
    unified_diff: Option<String>,
    new_string: Option<String>,
}

impl EditTextFields {
    fn is_empty(&self) -> bool {
        self.unified_diff.is_none() && self.new_string.is_none()
    }
}

fn as_edit_text_fields(text: Option<&str>) -> EditTextFields {
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return EditTextFields::default();
    };
    let (is_diff, t) = classify_diff_like_text(text);
    if is_diff {
        EditTextFields {
            unified_diff: truncate_diff_text(Some(&t), DIFF_MAX),
            new_string: None,
        }
    } else {
        EditTextFields {
            unified_diff: None,
            new_string: Some(text.to_string()),
        }
    }
}

fn first_patch_text(input: &Map) -> Option<String> {
    for k in ["patch", "diff", "unified_diff", "unifiedDiff", "content"] {
        if let Some(s) = non_empty_str(input.get(k)) {
            return Some(s.to_string());
        }
    }
    None
}

fn find_edit_input_path(input: &Map, patch_text: &str) -> Option<String> {
    for k in ["path", "file_path", "filePath"] {
        if let Some(Value::String(s)) = input.get(k) {
            if !js_trim(s).is_empty() {
                return Some(s.clone());
            }
        }
    }
    extract_patch_primary_file_path(patch_text)
}

fn normalize_edit_record_input(input: &Map) -> Value {
    let Some(candidate) = first_patch_text(input) else {
        return Value::Object(input.clone());
    };
    let fields = as_edit_text_fields(Some(&candidate));
    let raw_path = find_edit_input_path(input, &candidate);
    let mut normalized = Map::new();
    for (k, v) in input {
        if matches!(
            k.as_str(),
            "patch" | "diff" | "unified_diff" | "unifiedDiff"
        ) {
            continue;
        }
        normalized.insert(k.clone(), v.clone());
    }
    if let Some(p) = raw_path.filter(|p| !p.is_empty()) {
        normalized.insert("path".into(), Value::String(p));
    }
    if let Some(d) = fields.unified_diff.as_ref().filter(|d| !d.is_empty()) {
        normalized.insert("patch".into(), Value::String(d.clone()));
    }
    if let Some(n) = fields.new_string.as_ref().filter(|n| !n.is_empty()) {
        normalized.insert("content".into(), Value::String(n.clone()));
    }
    if fields.unified_diff.as_ref().is_some_and(|d| !d.is_empty()) {
        normalized.remove("content");
    }
    Value::Object(normalized)
}

fn normalize_edit_input(input: &Value) -> Value {
    match input {
        Value::String(s) => {
            let fields = as_edit_text_fields(Some(s));
            let path = extract_patch_primary_file_path(s);
            let mut m = Map::new();
            if let Some(p) = path {
                m.insert("path".into(), Value::String(p));
            }
            if let Some(d) = fields.unified_diff.filter(|d| !d.is_empty()) {
                m.insert("patch".into(), Value::String(d));
            }
            if let Some(n) = fields.new_string.filter(|n| !n.is_empty()) {
                m.insert("content".into(), Value::String(n));
            }
            Value::Object(m)
        }
        Value::Object(m) => normalize_edit_record_input(m),
        other => other.clone(),
    }
}

/// `asEditFileOutputFields`: `(patch, content)`.
fn as_edit_file_output_fields(text: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return (None, None);
    };
    let (is_diff, t) = classify_diff_like_text(text);
    if is_diff {
        (truncate_diff_text(Some(&t), DIFF_MAX), None)
    } else {
        (None, Some(text.to_string()))
    }
}

fn has_renderable_edit_detail(detail: &ToolCallDetail) -> bool {
    match detail {
        ToolCallDetail::Edit {
            old_string,
            new_string,
            unified_diff,
            ..
        } => [unified_diff, new_string, old_string]
            .iter()
            .any(|s| s.as_deref().is_some_and(|s| !js_trim(s).is_empty())),
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// envelope -> ToolCallItem
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum ToolKind {
    Shell,
    Read,
    Write,
    Edit,
    Search,
    Speak,
    Unknown,
}

fn resolve_tool_kind(name: &str) -> ToolKind {
    match name {
        "Bash" | "shell" | "bash" | "exec" | "exec_command" | "command" => ToolKind::Shell,
        "read" | "read_file" => ToolKind::Read,
        "write" | "write_file" | "create_file" => ToolKind::Write,
        "edit" | "apply_patch" | "apply_diff" => ToolKind::Edit,
        "search" | "web_search" => ToolKind::Search,
        _ if is_speak_tool_name(name) => ToolKind::Speak,
        _ => ToolKind::Unknown,
    }
}

#[derive(Debug, Clone)]
pub struct Envelope {
    pub call_id: String,
    pub name: String,
    pub input: Value,
    pub output: Value,
    pub status: Option<ToolCallStatus>,
    pub error: Value,
    pub metadata: Option<Map>,
    pub cwd: Option<String>,
}

fn to_tool_call_item(
    env: &Envelope,
    name: &str,
    kind: ToolKind,
    status: ToolCallStatus,
) -> ToolCallItem {
    let name = if kind == ToolKind::Speak {
        "speak"
    } else {
        name
    };
    let (input, output) = (&env.input, &env.output);
    let parsed = derive_codex_tool_detail(name, Some(input), Some(output), env.cwd.as_deref());
    let detail = if kind == ToolKind::Edit
        && status != ToolCallStatus::Running
        && !has_renderable_edit_detail(&parsed)
    {
        ToolCallDetail::Unknown {
            input: input.clone(),
            output: output.clone(),
        }
    } else {
        parsed
    };
    let mut item = if status == ToolCallStatus::Failed {
        let err = if env.error.is_null() {
            json!({"message": "Tool call failed"})
        } else {
            env.error.clone()
        };
        ToolCallItem::failed(env.call_id.clone(), name, detail, err)
    } else {
        ToolCallItem::new(env.call_id.clone(), name, status, detail)
    };
    item.metadata = env.metadata.clone();
    item
}

pub fn to_tool_call_from_envelope(env: Envelope) -> Option<ToolCallItem> {
    let status = env.status?;
    if env.call_id.is_empty() || env.name.is_empty() {
        return None;
    }
    let trimmed = js_trim(&env.name);
    if trimmed.is_empty() {
        return None;
    }
    Some(to_tool_call_item(
        &env,
        trimmed,
        resolve_tool_kind(trimmed),
        status,
    ))
}

// ---------------------------------------------------------------------------
// thread-item parsing (zod schemas)
// ---------------------------------------------------------------------------

fn opt_string_field(m: &Map, k: &str) -> Result<Option<String>, ()> {
    match m.get(k) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(()),
    }
}

fn required_id(m: &Map) -> Result<String, ()> {
    match m.get("id") {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        _ => Err(()),
    }
}

fn error_or_null(m: &Map) -> Value {
    m.get("error").cloned().unwrap_or(Value::Null)
}

fn map_command_execution_item(m: &Map) -> Result<Envelope, ()> {
    let id = required_id(m)?;
    let status = opt_string_field(m, "status")?;
    let command_v = match m.get("command") {
        None => None,
        Some(Value::String(_)) => m.get("command"),
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => m.get("command"),
        Some(_) => return Err(()),
    };
    let cwd = opt_string_field(m, "cwd")?;
    let aggregated = match m.get("aggregatedOutput") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(()),
    };
    let exit_code = match m.get("exitCode") {
        None => None,
        Some(Value::Null) => Some(Value::Null),
        Some(v @ Value::Number(_)) => Some(v.clone()),
        Some(_) => return Err(()),
    };
    let command = normalize_command_execution_command(command_v);
    let parsed_output = extract_codex_shell_output(aggregated.as_deref());
    let mut input = Map::new();
    if let Some(c) = &command {
        input.insert("command".into(), Value::String(c.clone()));
    }
    if let Some(c) = &cwd {
        input.insert("cwd".into(), Value::String(c.clone()));
    }
    let output = if parsed_output.is_some() || exit_code.is_some() {
        let mut o = Map::new();
        if let Some(c) = &command {
            o.insert("command".into(), Value::String(c.clone()));
        }
        if let Some(p) = parsed_output {
            o.insert("output".into(), Value::String(p));
        }
        if let Some(e) = exit_code {
            o.insert("exitCode".into(), e);
        }
        Value::Object(o)
    } else {
        Value::Null
    };
    let error = error_or_null(m);
    let status = normalize_tool_call_status(status.as_deref(), Some(&error), Some(&output));
    Ok(Envelope {
        call_id: id,
        name: "shell".into(),
        input: nullable_object(input),
        output,
        status: Some(status),
        error,
        metadata: None,
        cwd,
    })
}

struct FileChangeEntry {
    path: String,
    kind: Option<String>,
    diff: Option<String>,
}

fn parse_file_change_path(
    entry: &Map,
    cwd: Option<&str>,
    fallback: Option<&str>,
) -> Option<String> {
    let raw = non_blank(entry.get("path"))
        .or_else(|| non_blank(entry.get("file_path")))
        .or_else(|| non_blank(entry.get("filePath")))
        .or_else(|| {
            fallback.and_then(|f| (!js_trim(f).is_empty()).then(|| js_trim(f).to_string()))
        })?;
    normalize_codex_file_path(&raw, cwd)
}

fn parse_file_change_kind(entry: &Map) -> Option<String> {
    non_empty_str(entry.get("kind"))
        .or_else(|| non_empty_str(entry.get("type")))
        .map(str::to_string)
}

fn pick_first_patch_like(entry: &Map, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| non_empty_str(entry.get(*k)))
        .map(str::to_string)
}

fn to_file_change_entry(
    entry: &Map,
    cwd: Option<&str>,
    fallback: Option<&str>,
) -> Option<FileChangeEntry> {
    let path = parse_file_change_path(entry, cwd, fallback)?;
    Some(FileChangeEntry {
        path,
        kind: parse_file_change_kind(entry),
        diff: pick_first_patch_like(
            entry,
            &[
                "diff",
                "patch",
                "unified_diff",
                "unifiedDiff",
                "content",
                "newString",
            ],
        ),
    })
}

fn parse_file_change_entries(changes: &Value, cwd: Option<&str>) -> Vec<FileChangeEntry> {
    if !js_truthy(changes) {
        return Vec::new();
    }
    if let Value::Array(a) = changes {
        return a
            .iter()
            .filter_map(|e| as_record(e).and_then(|r| to_file_change_entry(r, cwd, None)))
            .collect();
    }
    let Some(rec) = as_record(changes) else {
        return Vec::new();
    };
    if let Some(files @ Value::Array(_)) = rec.get("files") {
        return parse_file_change_entries(files, cwd);
    }
    if let Some(single) = to_file_change_entry(rec, cwd, None) {
        return vec![single];
    }
    rec.iter()
        .filter_map(|(path, value)| match value {
            Value::Object(r) => to_file_change_entry(r, cwd, Some(path)),
            Value::String(s) => {
                let p = normalize_codex_file_path(js_trim(path), cwd)?;
                Some(FileChangeEntry {
                    path: p,
                    kind: None,
                    diff: Some(s.clone()),
                })
            }
            _ => None,
        })
        .collect()
}

fn resolve_file_change_text_fields(file: Option<&FileChangeEntry>) -> EditTextFields {
    let Some(file) = file else {
        return EditTextFields::default();
    };
    let is_delete = file.kind.as_deref() == Some("delete");
    if is_delete {
        if let Some(diff) = file.diff.as_deref().filter(|d| !d.is_empty()) {
            let (is_diff, t) = classify_diff_like_text(diff);
            let text = if is_diff {
                t
            } else {
                content_to_deletion_diff(&file.path, diff)
            };
            return EditTextFields {
                unified_diff: truncate_diff_text(Some(&text), DIFF_MAX),
                new_string: None,
            };
        }
        return EditTextFields {
            unified_diff: Some(content_to_deletion_diff(&file.path, "")),
            new_string: None,
        };
    }
    as_edit_text_fields(file.diff.as_deref())
}

fn map_file_change_item(m: &Map, cwd: Option<&str>) -> Result<Envelope, ()> {
    let id = required_id(m)?;
    let status = opt_string_field(m, "status")?;
    let files = parse_file_change_entries(m.get("changes").unwrap_or(&Value::Null), cwd);
    let file_entry = |f: &FileChangeEntry, with_text: bool| {
        let mut e = Map::new();
        e.insert("path".into(), Value::String(f.path.clone()));
        if let Some(k) = &f.kind {
            e.insert("kind".into(), Value::String(k.clone()));
        }
        if with_text {
            if f.kind.as_deref() == Some("delete") {
                if let Some(p) = resolve_file_change_text_fields(Some(f)).unified_diff {
                    e.insert("patch".into(), Value::String(p));
                }
            } else {
                let (patch, content) = as_edit_file_output_fields(f.diff.as_deref());
                if let Some(p) = patch {
                    e.insert("patch".into(), Value::String(p));
                }
                if let Some(c) = content {
                    e.insert("content".into(), Value::String(c));
                }
            }
        }
        Value::Object(e)
    };
    let mut input_base = Map::new();
    let mut output = Map::new();
    if !files.is_empty() {
        input_base.insert(
            "files".into(),
            Value::Array(files.iter().map(|f| file_entry(f, false)).collect()),
        );
        output.insert(
            "files".into(),
            Value::Array(files.iter().map(|f| file_entry(f, true)).collect()),
        );
    }
    let output = nullable_object(output);
    let error = error_or_null(m);
    let status = normalize_tool_call_status(status.as_deref(), Some(&error), Some(&output));
    let first = files.first();
    let first_text = resolve_file_change_text_fields(first);
    let has_first = !first_text.is_empty();
    let mut input = input_base;
    if let Some(f) = first {
        if !f.path.is_empty() && has_first {
            input.insert("path".into(), Value::String(f.path.clone()));
        }
    }
    if has_first {
        if let Some(d) = first_text.unified_diff.filter(|d| !d.is_empty()) {
            input.insert("patch".into(), Value::String(d));
        }
        if let Some(n) = first_text.new_string.filter(|n| !n.is_empty()) {
            input.insert("content".into(), Value::String(n));
        }
    }
    Ok(Envelope {
        call_id: id,
        name: "apply_patch".into(),
        input: nullable_object(input),
        output,
        status: Some(status),
        error,
        metadata: None,
        cwd: cwd.map(str::to_string),
    })
}

fn build_mcp_tool_name(server: Option<&str>, tool: &str) -> String {
    let t = js_trim(tool);
    if t.is_empty() {
        return "tool".into();
    }
    match server.map(js_trim).filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{t}"),
        None => t.to_string(),
    }
}

/// `splitCodexMcpToolResultImages`: `(images, output)`.
pub fn split_codex_mcp_tool_result_images(result: &Value) -> (Vec<(String, String)>, Value) {
    let Some(rec) = as_record(result) else {
        return (Vec::new(), result.clone());
    };
    let Some(Value::Array(content)) = rec.get("content") else {
        return (Vec::new(), result.clone());
    };
    let mut images = Vec::new();
    let blocks: Vec<Value> = content
        .iter()
        .map(|b| {
            let image = as_record(b).and_then(|r| {
                if r.get("type").and_then(Value::as_str) != Some("image") {
                    return None;
                }
                Some((
                    r.get("data").and_then(Value::as_str)?.to_string(),
                    r.get("mimeType").and_then(Value::as_str)?.to_string(),
                ))
            });
            match image {
                Some(i) => {
                    images.push(i);
                    json!({"type": "text", "text": "[image]"})
                }
                None => b.clone(),
            }
        })
        .collect();
    if images.is_empty() {
        return (images, result.clone());
    }
    let mut out = rec.clone();
    out.insert("content".into(), Value::Array(blocks));
    (images, Value::Object(out))
}

fn map_mcp_tool_call_item(m: &Map, cwd: Option<&str>) -> Result<Option<Envelope>, ()> {
    let id = required_id(m)?;
    let status = opt_string_field(m, "status")?;
    let tool = match m.get("tool") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => return Err(()),
    };
    let server = opt_string_field(m, "server")?;
    let tool_trimmed = js_trim(&tool);
    if tool_trimmed.is_empty() {
        return Ok(None);
    }
    let name = build_mcp_tool_name(server.as_deref(), tool_trimmed);
    let input = m.get("arguments").cloned().unwrap_or(Value::Null);
    let (_, output) = split_codex_mcp_tool_result_images(m.get("result").unwrap_or(&Value::Null));
    let error = error_or_null(m);
    let status = normalize_tool_call_status(status.as_deref(), Some(&error), Some(&output));
    Ok(Some(Envelope {
        call_id: id,
        name,
        input,
        output,
        status: Some(status),
        error,
        metadata: None,
        cwd: cwd.map(str::to_string),
    }))
}

fn map_web_search_item(m: &Map) -> Result<Envelope, ()> {
    let id = required_id(m)?;
    let status = opt_string_field(m, "status")?;
    let query = opt_string_field(m, "query")?;
    let input = match query {
        Some(q) => json!({"query": q}),
        None => Value::Null,
    };
    let output = m.get("action").cloned().unwrap_or(Value::Null);
    let error = error_or_null(m);
    let status = normalize_tool_call_status(
        Some(status.as_deref().unwrap_or("completed")),
        Some(&error),
        Some(&output),
    );
    Ok(Envelope {
        call_id: id,
        name: "web_search".into(),
        input,
        output,
        status: Some(status),
        error,
        metadata: None,
        cwd: None,
    })
}

fn normalize_collab_child_status(status: &str) -> ToolCallStatus {
    match js_trim(status).to_lowercase().as_str() {
        "error" | "errored" => ToolCallStatus::Running,
        "shutdown" => ToolCallStatus::Canceled,
        "notfound" => ToolCallStatus::Failed,
        _ => normalize_tool_call_status(Some(status), None, None),
    }
}

fn map_collab_agent_tool_call_item(m: &Map) -> Result<ToolCallItem, ()> {
    let id = required_id(m)?;
    let status_raw = opt_string_field(m, "status")?;
    let prompt = opt_string_field(m, "prompt")?;
    opt_string_field(m, "tool")?;
    match m.get("receiverThreadIds") {
        None => {}
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => {}
        Some(_) => return Err(()),
    }
    let states = match m.get("agentsStates") {
        None => None,
        Some(Value::Object(o)) => Some(o),
        Some(_) => return Err(()),
    };
    let error = error_or_null(m);
    let status = if !error.is_null() {
        ToolCallStatus::Failed
    } else {
        let parent = normalize_tool_call_status(status_raw.as_deref(), None, None);
        if parent == ToolCallStatus::Failed {
            ToolCallStatus::Failed
        } else {
            let child: Vec<ToolCallStatus> = states
                .map(|s| {
                    s.values()
                        .filter_map(|v| as_record(v)?.get("status")?.as_str())
                        .filter(|s| !js_trim(s).is_empty())
                        .map(normalize_collab_child_status)
                        .collect()
                })
                .unwrap_or_default();
            if child.contains(&ToolCallStatus::Failed) {
                ToolCallStatus::Failed
            } else if child.contains(&ToolCallStatus::Canceled) {
                ToolCallStatus::Canceled
            } else if !child.is_empty() {
                if child.iter().all(|s| *s == ToolCallStatus::Completed) {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Running
                }
            } else {
                parent
            }
        }
    };
    let detail = ToolCallDetail::SubAgent {
        sub_agent_type: Some("Sub-agent".into()),
        description: prompt.filter(|p| !p.is_empty()),
        child_session_id: None,
        log: String::new(),
        actions: Some(Vec::new()),
    };
    Ok(if status == ToolCallStatus::Failed {
        let err = if error.is_null() {
            json!({"message": "Sub-agent failed"})
        } else {
            error
        };
        ToolCallItem::failed(id, "Sub-agent", detail, err)
    } else {
        ToolCallItem::new(id, "Sub-agent", status, detail)
    })
}

fn map_sub_agent_activity_item(m: &Map) -> Result<ToolCallItem, ()> {
    let id = required_id(m)?;
    let kind = match m.get("kind").and_then(Value::as_str) {
        Some(k @ ("started" | "interacted" | "interrupted" | "completed")) => k,
        _ => return Err(()),
    };
    match m.get("agentThreadId") {
        Some(Value::String(s)) if !s.is_empty() => {}
        _ => return Err(()),
    }
    let agent_path = match m.get("agentPath") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err(()),
    };
    let mut native = agent_path.clone();
    if native == "/root" {
        native = String::new();
    } else if let Some(rest) = native.strip_prefix("/root/") {
        native = rest.to_string();
    } else if native.contains(['/', '\\']) {
        let idx = native.rfind(['/', '\\']).map(|i| i + 1).unwrap_or(0);
        native = native[idx..].to_string();
    }
    let description = native.clone();
    let sep = Regex::new(r"[_-]+").expect("sep regex");
    let mut label = native
        .split('/')
        .map(|seg| js_trim(&sep.replace_all(seg, " ")).to_string())
        .filter(|s| !s.is_empty())
        .map(|seg| {
            let mut c = seg.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" / ");
    if label.is_empty() {
        label = "Sub-agent".into();
    }
    let status = match kind {
        "interrupted" => ToolCallStatus::Canceled,
        "completed" => ToolCallStatus::Completed,
        _ => ToolCallStatus::Running,
    };
    Ok(ToolCallItem::new(
        id,
        "Sub-agent",
        status,
        ToolCallDetail::SubAgent {
            sub_agent_type: Some(label),
            description: Some(description),
            child_session_id: None,
            log: String::new(),
            actions: Some(Vec::new()),
        },
    ))
}

/// `mapCodexToolCallFromThreadItem(item, {cwd})`.
pub fn map_codex_tool_call_from_thread_item(
    item: &Value,
    cwd: Option<&str>,
) -> Option<ToolCallItem> {
    let m = as_record(item)?;
    let envelope = match m.get("type").and_then(Value::as_str)? {
        "commandExecution" => map_command_execution_item(m).ok()?,
        "fileChange" => map_file_change_item(m, cwd).ok()?,
        "mcpToolCall" => map_mcp_tool_call_item(m, cwd).ok()??,
        "webSearch" => map_web_search_item(m).ok()?,
        "collabAgentToolCall" => return map_collab_agent_tool_call_item(m).ok(),
        "subAgentActivity" => return map_sub_agent_activity_item(m).ok(),
        _ => return None,
    };
    to_tool_call_from_envelope(envelope)
}

/// Parameters of `mapCodexToolCallEnvelope`.
#[derive(Debug, Clone, Default)]
pub struct EnvelopeParams {
    pub call_id: Option<String>,
    pub name: String,
    pub input: Value,
    pub output: Value,
    pub error: Value,
    pub cwd: Option<String>,
}

/// `mapCodexToolCallEnvelope`.
pub fn map_codex_tool_call_envelope(p: &EnvelopeParams) -> Option<ToolCallItem> {
    if p.name.is_empty() {
        return None;
    }
    let name = js_trim(&p.name);
    if name.is_empty() {
        return None;
    }
    let call_id = js_trim(p.call_id.as_deref().unwrap_or("")).to_string();
    if call_id.is_empty() {
        return None;
    }
    let input = if name == "apply_patch" || name == "apply_diff" {
        normalize_edit_input(&p.input)
    } else {
        p.input.clone()
    };
    let status = normalize_tool_call_status(Some("completed"), Some(&p.error), Some(&p.output));
    to_tool_call_from_envelope(Envelope {
        call_id,
        name: name.to_string(),
        input,
        output: p.output.clone(),
        status: Some(status),
        error: p.error.clone(),
        metadata: None,
        cwd: p.cwd.clone(),
    })
}

/// JSON entry for the golden vectors: `{callId, name, input, output, error, cwd}`.
pub fn map_codex_tool_call_envelope_value(params: &Value) -> Option<ToolCallItem> {
    let m = as_record(params)?;
    let name = m.get("name")?.as_str()?.to_string();
    let call_id = match m.get("callId") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return None,
    };
    let cwd = match m.get("cwd") {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    map_codex_tool_call_envelope(&EnvelopeParams {
        call_id,
        name,
        input: m.get("input").cloned().unwrap_or(Value::Null),
        output: m.get("output").cloned().unwrap_or(Value::Null),
        error: m.get("error").cloned().unwrap_or(Value::Null),
        cwd,
    })
}

// ---------------------------------------------------------------------------
// notification-driven mappers (adapter)
// ---------------------------------------------------------------------------

pub fn to_running_tool_call(mut item: ToolCallItem) -> ToolCallItem {
    item.status = ToolCallStatus::Running;
    item.error = Some(Value::Null);
    item
}

pub struct ExecParams<'a> {
    pub call_id: Option<&'a str>,
    pub command: Option<&'a Value>,
    pub cwd: Option<&'a str>,
    pub output: Option<&'a str>,
    pub exit_code: Option<&'a Value>,
    pub success: Option<bool>,
    pub stderr: Option<&'a str>,
    pub running: bool,
}

/// `mapCodexExecNotificationToToolCall`.
pub fn map_codex_exec_notification_to_tool_call(p: &ExecParams) -> Option<ToolCallItem> {
    let command = normalize_codex_command_value(p.command)?;
    let exit_nonzero = p
        .exit_code
        .and_then(Value::as_f64)
        .is_some_and(|n| n != 0.0);
    let is_failure = if p.running {
        false
    } else {
        p.success == Some(false) || exit_nonzero
    };
    let output = if p.running {
        Value::Null
    } else {
        let mut o = Map::new();
        o.insert("command".into(), command.to_value());
        if let Some(out) = p.output {
            o.insert("output".into(), Value::String(out.to_string()));
        }
        if let Some(e) = p.exit_code.filter(|e| !e.is_null()) {
            o.insert("exitCode".into(), e.clone());
        }
        Value::Object(o)
    };
    let mut input = Map::new();
    input.insert("command".into(), command.to_value());
    if let Some(c) = p.cwd.filter(|c| !c.is_empty()) {
        input.insert("cwd".into(), Value::String(c.to_string()));
    }
    let error = if is_failure {
        let msg = p.stderr.map(js_trim).filter(|s| !s.is_empty());
        json!({"message": msg.unwrap_or("Command failed")})
    } else {
        Value::Null
    };
    let mapped = map_codex_tool_call_envelope(&EnvelopeParams {
        call_id: p.call_id.map(str::to_string),
        name: "shell".into(),
        input: Value::Object(input),
        output,
        error,
        cwd: p.cwd.map(str::to_string),
    })?;
    Some(if p.running {
        to_running_tool_call(mapped)
    } else {
        mapped
    })
}

pub struct PatchFileChange {
    pub path: String,
    pub kind: Option<String>,
    pub content: Option<String>,
}

fn extract_patch_like_text(v: &Value) -> Option<String> {
    let r = as_record(v)?;
    pick_first_patch_like(
        r,
        &[
            "diff",
            "patch",
            "unified_diff",
            "unifiedDiff",
            "content",
            "newString",
        ],
    )
}

fn record_path(r: &Map) -> String {
    for k in ["path", "file_path", "filePath"] {
        if let Some(Value::String(s)) = r.get(k) {
            if !js_trim(s).is_empty() {
                return js_trim(s).to_string();
            }
        }
    }
    String::new()
}

fn record_kind(r: &Map) -> Option<String> {
    non_empty_str(r.get("kind"))
        .or_else(|| non_empty_str(r.get("type")))
        .map(str::to_string)
}

/// `parseCodexPatchChanges`.
pub fn parse_codex_patch_changes(changes: &Value) -> Vec<PatchFileChange> {
    match changes {
        Value::Array(a) => a
            .iter()
            .filter_map(|e| {
                let r = as_record(e)?;
                let path = record_path(r);
                if path.is_empty() {
                    return None;
                }
                Some(PatchFileChange {
                    path,
                    kind: record_kind(r),
                    content: extract_patch_like_text(e),
                })
            })
            .collect(),
        Value::Object(r) => {
            let direct = record_path(r);
            if !direct.is_empty() {
                return vec![PatchFileChange {
                    path: direct,
                    kind: record_kind(r),
                    content: extract_patch_like_text(changes),
                }];
            }
            r.iter()
                .filter_map(|(k, v)| {
                    let path = js_trim(k);
                    if path.is_empty() {
                        return None;
                    }
                    Some(PatchFileChange {
                        path: path.to_string(),
                        kind: as_record(v)
                            .and_then(|o| o.get("type"))
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        content: extract_patch_like_text(v),
                    })
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// `codexPatchTextFields`: `(patch, content)`.
fn codex_patch_text_fields(text: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(text) = text else {
        return (None, None);
    };
    if looks_like_unified_diff(text) {
        (Some(text.to_string()), None)
    } else {
        (None, Some(text.to_string()))
    }
}

pub struct PatchParams<'a> {
    pub call_id: Option<&'a str>,
    pub changes: &'a Value,
    pub cwd: Option<&'a str>,
    pub stdout: Option<&'a str>,
    pub stderr: Option<&'a str>,
    pub success: Option<bool>,
    pub running: bool,
}

/// `mapCodexPatchNotificationToToolCall`.
pub fn map_codex_patch_notification_to_tool_call(p: &PatchParams) -> Option<ToolCallItem> {
    let files = parse_codex_patch_changes(p.changes);
    let first_path = files.first().map(|f| f.path.clone());
    let patch_text = files
        .iter()
        .filter_map(|f| f.content.as_deref().map(js_trim))
        .find(|v| !v.is_empty())
        .map(str::to_string);
    let (patch, content) = codex_patch_text_fields(patch_text.as_deref());
    let mut input = Map::new();
    if let Some(fp) = &first_path {
        input.insert("path".into(), Value::String(fp.clone()));
        if let Some(pa) = &patch {
            input.insert("patch".into(), Value::String(pa.clone()));
        }
        if let Some(c) = &content {
            input.insert("content".into(), Value::String(c.clone()));
        }
        input.insert(
            "files".into(),
            Value::Array(
                files
                    .iter()
                    .map(|f| {
                        let mut e = Map::new();
                        e.insert("path".into(), Value::String(f.path.clone()));
                        if let Some(k) = &f.kind {
                            e.insert("kind".into(), Value::String(k.clone()));
                        }
                        Value::Object(e)
                    })
                    .collect(),
            ),
        );
    } else {
        input.insert("changes".into(), p.changes.clone());
        if let Some(pa) = &patch {
            input.insert("patch".into(), Value::String(pa.clone()));
        }
        if let Some(c) = &content {
            input.insert("content".into(), Value::String(c.clone()));
        }
    }
    let output = if p.running {
        Value::Null
    } else {
        let mut o = Map::new();
        if !files.is_empty() {
            o.insert(
                "files".into(),
                Value::Array(
                    files
                        .iter()
                        .map(|f| {
                            let mut e = Map::new();
                            e.insert("path".into(), Value::String(f.path.clone()));
                            if let Some(k) = &f.kind {
                                e.insert("kind".into(), Value::String(k.clone()));
                            }
                            let (pa, co) = codex_patch_text_fields(
                                f.content.as_deref().or(patch_text.as_deref()),
                            );
                            if let Some(pa) = pa {
                                e.insert("patch".into(), Value::String(pa));
                            }
                            if let Some(co) = co {
                                e.insert("content".into(), Value::String(co));
                            }
                            Value::Object(e)
                        })
                        .collect(),
                ),
            );
        }
        if let Some(s) = p.stdout.filter(|s| !s.is_empty()) {
            o.insert("stdout".into(), Value::String(s.to_string()));
        }
        if let Some(s) = p.stderr.filter(|s| !s.is_empty()) {
            o.insert("stderr".into(), Value::String(s.to_string()));
        }
        if let Some(s) = p.success {
            o.insert("success".into(), Value::Bool(s));
        }
        Value::Object(o)
    };
    let error = if p.running || p.success != Some(false) {
        Value::Null
    } else {
        let msg = p.stderr.map(js_trim).filter(|s| !s.is_empty());
        json!({"message": msg.unwrap_or("Patch apply failed")})
    };
    let mapped = map_codex_tool_call_envelope(&EnvelopeParams {
        call_id: p.call_id.map(str::to_string),
        name: "apply_patch".into(),
        input: Value::Object(input),
        output,
        error,
        cwd: p.cwd.map(str::to_string),
    })?;
    Some(if p.running {
        to_running_tool_call(mapped)
    } else {
        mapped
    })
}

/// `mapCodexTerminalInteractionToToolCall`.
pub fn map_codex_terminal_interaction_to_tool_call(
    call_id: &str,
    process_id: Option<&str>,
    command: Option<&str>,
    stdin: Option<&str>,
) -> ToolCallItem {
    let process_id = process_id.filter(|s| !s.is_empty());
    let label = command.filter(|s| !s.is_empty());
    let mut item = ToolCallItem::new(
        call_id,
        "terminal",
        ToolCallStatus::Completed,
        ToolCallDetail::PlainText {
            label: label.map(str::to_string),
            text: stdin.map(str::to_string),
            icon: Some(paseo_agent_types::ToolCallIcon::SquareTerminal),
        },
    );
    if let Some(p) = process_id {
        let mut m = Map::new();
        m.insert("processId".into(), Value::String(p.to_string()));
        item.metadata = Some(m);
    }
    item
}

/// `isEditToolCallWithoutContent` (used by the TS warn-only path; kept for callers that log).
pub fn is_edit_tool_call_without_content(item: &AgentTimelineItem) -> bool {
    match item.as_tool_call().map(|t| &t.detail) {
        Some(ToolCallDetail::Edit {
            new_string,
            unified_diff,
            ..
        }) => ![unified_diff, new_string]
            .iter()
            .any(|s| s.as_deref().is_some_and(|s| !js_trim(s).is_empty())),
        _ => false,
    }
}
