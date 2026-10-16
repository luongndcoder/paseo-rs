//! Port of `tool-call-detail-primitives.ts`.
//!
//! The TS file builds zod schemas. Here each schema is a function returning `Result<T, ()>`
//! with the same accept/reject behavior:
//! - `z.object({...})` rejects non-objects; optional keys accept *absent* but not `null`.
//! - `z.union` returns the first option that parses.
//! - `.passthrough()` objects accept any extra keys.
//! - `z.intersection(path, X)` also fails when the input carries a `filePath` key that
//!   disagrees with the path the left side picked (zod "unmergable intersection").
//! - A transform that yields `undefined` still counts as a union success.

use crate::gutter::{
    flatten_read_content, strip_read_line_number_gutter, truncate_diff_text, DEFAULT_DIFF_MAX_CHARS,
};
use crate::js::{is_safe_integer, js_trim};
use crate::shell_output::extract_codex_shell_output;
use paseo_agent_types::{
    Map, Num, SearchDetail, SearchMode, SearchToolName, ToolCallDetail, WebResult,
};
use serde_json::Value;

pub type P<T> = Result<T, ()>;
pub type NormalizePath<'a> = Option<&'a dyn Fn(&str) -> Option<String>>;

// ---- low-level schema helpers -------------------------------------------------------------

pub(crate) fn obj(v: &Value) -> P<&Map> {
    v.as_object().ok_or(())
}

/// `z.string().optional()`
pub(crate) fn opt_s<'a>(m: &'a Map, k: &str) -> P<Option<&'a str>> {
    match m.get(k) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        _ => Err(()),
    }
}

pub(crate) fn req_s<'a>(m: &'a Map, k: &str) -> P<&'a str> {
    match m.get(k) {
        Some(Value::String(s)) => Ok(s),
        _ => Err(()),
    }
}

/// `z.number().finite().optional()`
pub(crate) fn opt_n(m: &Map, k: &str) -> P<Option<Num>> {
    match m.get(k) {
        None => Ok(None),
        Some(Value::Number(n)) => Ok(Some(n.clone())),
        _ => Err(()),
    }
}

pub(crate) fn req_n(m: &Map, k: &str) -> P<Num> {
    match m.get(k) {
        Some(Value::Number(n)) => Ok(n.clone()),
        _ => Err(()),
    }
}

/// `z.number().finite().nullable().optional()`
fn opt_nullable_n(m: &Map, k: &str) -> P<Option<Option<Num>>> {
    match m.get(k) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::Number(n)) => Ok(Some(Some(n.clone()))),
        _ => Err(()),
    }
}

/// `z.number().int().nonnegative()` (zod 4 `int()` is a safe-integer check)
fn req_nonneg_int(m: &Map, k: &str) -> P<Num> {
    let n = req_n(m, k)?;
    if is_safe_integer(&n) && n.as_f64().is_some_and(|f| f >= 0.0) {
        Ok(n)
    } else {
        Err(())
    }
}

fn opt_nonneg_int(m: &Map, k: &str) -> P<Option<Num>> {
    if m.contains_key(k) {
        req_nonneg_int(m, k).map(Some)
    } else {
        Ok(None)
    }
}

fn req_str_array(m: &Map, k: &str) -> P<Vec<String>> {
    match m.get(k) {
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().map(String::from).ok_or(()))
            .collect(),
        _ => Err(()),
    }
}

/// `z.nullable(schema)` where a missing value (`undefined`) is *not* null.
pub(crate) fn nullable<T>(v: Option<&Value>, f: impl FnOnce(&Value) -> P<T>) -> P<Option<T>> {
    match v {
        Some(Value::Null) => Ok(None),
        Some(x) => f(x).map(Some),
        None => Err(()),
    }
}

fn nes(s: Option<&str>) -> Option<String> {
    s.filter(|x| !x.is_empty()).map(String::from)
}

fn first_some<T>(items: impl IntoIterator<Item = Option<T>>) -> Option<T> {
    items.into_iter().flatten().next()
}

// ---- shell --------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ShellIn {
    pub command: Option<String>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ShellOut {
    pub command: Option<String>,
    pub output: Option<String>,
    pub exit_code: Option<Num>,
}

/// `CommandValueSchema = z.union([z.string(), z.array(z.string())])`
fn command_value(v: &Value) -> P<&Value> {
    match v {
        Value::String(_) => Ok(v),
        Value::Array(a) if a.iter().all(Value::is_string) => Ok(v),
        _ => Err(()),
    }
}

pub fn parse_shell_input(v: &Value) -> P<ShellIn> {
    let m = obj(v)?;
    let key = if m.get("command").is_some_and(|c| command_value(c).is_ok()) {
        "command"
    } else if m.get("cmd").is_some_and(|c| command_value(c).is_ok()) {
        "cmd"
    } else {
        return Err(());
    };
    let cwd = opt_s(m, "cwd")?;
    let directory = opt_s(m, "directory")?;
    let raw = if m.contains_key("command") {
        m.get("command")
    } else {
        m.get(key)
    };
    let command = match raw.and_then(|c| command_value(c).ok()) {
        Some(Value::String(s)) => nes(Some(s)),
        Some(Value::Array(a)) => {
            let joined = a
                .iter()
                .filter_map(Value::as_str)
                .map(js_trim)
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    };
    Ok(ShellIn {
        command,
        cwd: nes(cwd).or_else(|| nes(directory)),
    })
}

fn text_triplet(m: &Map) -> P<()> {
    opt_s(m, "output")?;
    opt_s(m, "text")?;
    opt_s(m, "content")?;
    Ok(())
}

pub fn parse_shell_output(v: &Value) -> P<ShellOut> {
    if let Value::String(s) = v {
        return Ok(ShellOut {
            command: None,
            output: extract_codex_shell_output(Some(s)),
            exit_code: None,
        });
    }
    let m = obj(v)?;
    opt_s(m, "command")?;
    text_triplet(m)?;
    opt_s(m, "aggregated_output")?;
    opt_s(m, "aggregatedOutput")?;
    let exit_code = opt_nullable_n(m, "exitCode")?;
    let exit_code_snake = opt_nullable_n(m, "exit_code")?;
    let mut meta_exit = None;
    let mut meta_exit_snake = None;
    if let Some(meta) = m.get("metadata") {
        let mm = obj(meta)?;
        meta_exit = opt_nullable_n(mm, "exitCode")?;
        meta_exit_snake = opt_nullable_n(mm, "exit_code")?;
    }
    let mut sc = None;
    for key in ["structuredContent", "structured_content"] {
        let part = match m.get(key) {
            None => None,
            Some(x) => {
                let sm = obj(x)?;
                text_triplet(sm)?;
                Some(sm)
            }
        };
        if key == "structuredContent" {
            sc = part;
        }
    }
    let structured_snake = m.get("structured_content").and_then(Value::as_object);
    let result = match m.get("result") {
        None => None,
        Some(x) => {
            let rm = obj(x)?;
            opt_s(rm, "command")?;
            text_triplet(rm)?;
            Some(rm)
        }
    };

    fn s<'a>(mm: Option<&'a Map>, k: &str) -> Option<&'a str> {
        mm.and_then(|x| x.get(k)).and_then(Value::as_str)
    }
    let raw_text = first_some([
        nes(s(Some(m), "output")),
        nes(s(Some(m), "text")),
        nes(s(Some(m), "content")),
        nes(s(Some(m), "aggregated_output")),
        nes(s(Some(m), "aggregatedOutput")),
        nes(s(sc, "output")),
        nes(s(sc, "text")),
        nes(s(sc, "content")),
        nes(s(structured_snake, "output")),
        nes(s(structured_snake, "text")),
        nes(s(structured_snake, "content")),
        nes(s(result, "output")),
        nes(s(result, "text")),
        nes(s(result, "content")),
    ]);
    // `a ?? b ?? ...` skips null as well as undefined, so the final value is never null.
    let exit = [exit_code, exit_code_snake, meta_exit, meta_exit_snake]
        .into_iter()
        .flatten()
        .flatten()
        .next();
    Ok(ShellOut {
        command: nes(s(Some(m), "command")).or_else(|| nes(s(result, "command"))),
        output: extract_codex_shell_output(raw_text.as_deref()),
        exit_code: exit,
    })
}

// ---- path input (shared by read/write/edit) -------------------------------------------------

const PATH_KEYS: [&str; 3] = ["path", "file_path", "filePath"];

/// `ToolPathInputSchema` first matching key, plus the intersection conflict rule.
fn path_key(m: &Map) -> P<String> {
    for k in PATH_KEYS {
        if let Some(Value::String(s)) = m.get(k) {
            return Ok(s.clone());
        }
    }
    Err(())
}

fn intersect_path(m: &Map) -> P<String> {
    let chosen = path_key(m)?;
    if let Some(existing) = m.get("filePath") {
        if existing != &Value::String(chosen.clone()) {
            return Err(());
        }
    }
    Ok(chosen)
}

// ---- read -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ReadIn {
    pub file_path: Option<String>,
    pub offset: Option<Num>,
    pub limit: Option<Num>,
}

#[derive(Debug, Clone, Default)]
pub struct ReadOut {
    pub file_path: Option<String>,
    pub content: Option<String>,
}

pub fn parse_read_input(v: &Value) -> P<ReadIn> {
    let m = obj(v)?;
    for k in PATH_KEYS {
        let attempt = (|| -> P<ReadIn> {
            Ok(ReadIn {
                file_path: Some(req_s(m, k)?.to_string()),
                offset: opt_n(m, "offset")?,
                limit: opt_n(m, "limit")?,
            })
        })();
        if attempt.is_ok() {
            return attempt;
        }
    }
    Err(())
}

fn read_chunk_ok(v: &Value) -> bool {
    let Some(m) = v.as_object() else { return false };
    let mut any = false;
    for k in ["text", "content", "output"] {
        match m.get(k) {
            None => {}
            Some(Value::String(_)) => any = true,
            _ => return false,
        }
    }
    any
}

/// `ToolReadContentSchema = string | chunk | chunk[]`
fn read_content_ok(v: &Value) -> bool {
    match v {
        Value::String(_) => true,
        Value::Array(a) => a.iter().all(read_chunk_ok),
        other => read_chunk_ok(other),
    }
}

/// `ToolReadPayloadSchema`: every present key valid, at least one present.
fn read_payload_ok(v: &Value) -> bool {
    let Some(m) = v.as_object() else { return false };
    let mut any = false;
    for k in ["content", "text", "output"] {
        if let Some(x) = m.get(k) {
            if !read_content_ok(x) {
                return false;
            }
            any = true;
        }
    }
    any
}

fn payload_content(m: &Map) -> Option<String> {
    flatten_read_content(m.get("content"))
        .or_else(|| flatten_read_content(m.get("text")))
        .or_else(|| flatten_read_content(m.get("output")))
}

fn read_xml_tag<'a>(value: &'a str, tag: &str) -> Option<&'a str> {
    let lower = value.to_ascii_lowercase();
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = lower.find(&open)? + open.len();
    let end = start + lower[start..].find(&close)?;
    Some(&value[start..end])
}

fn decode_xml_text(value: &str) -> String {
    let mut out = String::new();
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < value.len() {
        if bytes[i] == b'&' {
            if let Some((decoded, len)) = decode_entity(&value[i..]) {
                out.push_str(&decoded);
                i += len;
                continue;
            }
        }
        let c = value[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// `&(?:#(\d+)|#x([0-9a-fA-F]+)|amp|lt|gt|quot|apos);`
///
/// PARITY-QUIRK(xml-codepoint): TS `String.fromCodePoint` throws for values above U+10FFFF and
/// yields a lone surrogate for U+D800..U+DFFF. Rust leaves out-of-range entities untouched and
/// maps surrogates to U+FFFD.
fn decode_entity(s: &str) -> Option<(String, usize)> {
    let rest = &s[1..];
    let semi = rest.find(';')?;
    let body = &rest[..semi];
    let total = semi + 2;
    let ch = |cp: u32| -> Option<String> {
        if cp > 0x10FFFF {
            return None;
        }
        Some(char::from_u32(cp).unwrap_or('\u{fffd}').to_string())
    };
    if let Some(d) = body.strip_prefix('#') {
        if let Some(h) = d.strip_prefix('x') {
            if !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()) {
                let cp = u32::from_str_radix(h, 16).ok()?;
                return ch(cp).map(|c| (c, total));
            }
            return None;
        }
        if !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()) {
            let cp: u32 = d.parse().ok()?;
            return ch(cp).map(|c| (c, total));
        }
        return None;
    }
    let named = match body {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        _ => return None,
    };
    Some((named.into(), total))
}

fn trim_outer_line_breaks(v: &str) -> String {
    let a = v
        .strip_prefix("\r\n")
        .or_else(|| v.strip_prefix('\n'))
        .unwrap_or(v);
    a.strip_suffix("\r\n")
        .or_else(|| a.strip_suffix('\n'))
        .unwrap_or(a)
        .to_string()
}

fn parse_xml_read_output(value: &str) -> Option<ReadOut> {
    let trimmed = js_trim(value);
    if !trimmed.starts_with("<path>") || !trimmed.to_ascii_lowercase().contains("<content>") {
        return None;
    }
    let file_path = read_xml_tag(trimmed, "path")
        .map(js_trim)
        .filter(|s| !s.is_empty());
    let content = read_xml_tag(trimmed, "content");
    if file_path.is_none() && content.is_none() {
        return None;
    }
    Some(ReadOut {
        file_path: file_path.map(decode_xml_text),
        content: content.map(|c| decode_xml_text(&trim_outer_line_breaks(c))),
    })
}

/// `ToolReadOutputSchema` (content-only union).
pub fn parse_read_output(v: &Value) -> P<ReadOut> {
    if let Value::String(s) = v {
        if let Some(x) = parse_xml_read_output(s) {
            return Ok(x);
        }
        return Ok(ReadOut {
            file_path: None,
            content: nes(Some(s)),
        });
    }
    if read_chunk_ok(v) || matches!(v, Value::Array(a) if a.iter().all(read_chunk_ok)) {
        return Ok(ReadOut {
            file_path: None,
            content: flatten_read_content(Some(v)),
        });
    }
    if read_payload_ok(v) {
        return Ok(ReadOut {
            file_path: None,
            content: payload_content(v.as_object().unwrap()),
        });
    }
    if let Some(m) = v.as_object() {
        for k in ["data", "structuredContent", "structured_content"] {
            if let Some(inner) = m.get(k) {
                if read_payload_ok(inner) {
                    return Ok(ReadOut {
                        file_path: None,
                        content: payload_content(inner.as_object().unwrap()),
                    });
                }
            }
        }
    }
    Err(())
}

/// `ToolReadOutputWithPathSchema` (content union, then the path object).
pub fn parse_read_output_with_path(v: &Value) -> P<ReadOut> {
    if let Ok(x) = parse_read_output(v) {
        return Ok(x);
    }
    let m = obj(v)?;
    for k in PATH_KEYS {
        if let Some(Value::String(p)) = m.get(k) {
            let all_ok = ["content", "text", "output"]
                .iter()
                .all(|ck| m.get(*ck).is_none_or(read_content_ok));
            if all_ok {
                return Ok(ReadOut {
                    file_path: Some(p.clone()),
                    content: payload_content(m),
                });
            }
        }
    }
    Err(())
}

// ---- write ----------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct WriteVal {
    pub file_path: Option<String>,
    pub content: Option<String>,
}

fn write_content(m: &Map) -> P<Option<String>> {
    let c = opt_s(m, "content")?;
    let n1 = opt_s(m, "new_content")?;
    let n2 = opt_s(m, "newContent")?;
    Ok(nes(c).or_else(|| nes(n1)).or_else(|| nes(n2)))
}

pub fn parse_write_input(v: &Value) -> P<WriteVal> {
    let m = obj(v)?;
    let file_path = intersect_path(m)?;
    let content = write_content(m)?;
    Ok(WriteVal {
        file_path: Some(file_path),
        content,
    })
}

pub fn parse_write_output(v: &Value) -> P<WriteVal> {
    if v.is_string() {
        return Ok(WriteVal::default());
    }
    let m = obj(v)?;
    let content = write_content(m)?;
    if let Ok(p) = intersect_path(m) {
        return Ok(WriteVal {
            file_path: Some(p),
            content,
        });
    }
    Ok(WriteVal {
        file_path: None,
        content,
    })
}

// ---- edit -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct EditVal {
    pub file_path: Option<String>,
    pub old_string: Option<String>,
    pub new_string: Option<String>,
    pub unified_diff: Option<String>,
}

const EDIT_TEXT_KEYS: [&str; 13] = [
    "old_string",
    "old_str",
    "oldContent",
    "old_content",
    "new_string",
    "new_str",
    "newContent",
    "new_content",
    "content",
    "patch",
    "diff",
    "unified_diff",
    "unifiedDiff",
];

fn edit_text_ok(m: &Map) -> P<()> {
    for k in EDIT_TEXT_KEYS {
        opt_s(m, k)?;
    }
    Ok(())
}

fn s_of<'a>(m: &'a Map, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str)
}

fn diff_of(m: &Map) -> Option<String> {
    let raw = first_some([
        nes(s_of(m, "patch")),
        nes(s_of(m, "diff")),
        nes(s_of(m, "unified_diff")),
        nes(s_of(m, "unifiedDiff")),
    ]);
    truncate_diff_text(raw.as_deref(), DEFAULT_DIFF_MAX_CHARS)
}

fn new_content_of(m: &Map) -> Option<String> {
    first_some([
        nes(s_of(m, "newContent")),
        nes(s_of(m, "new_content")),
        nes(s_of(m, "content")),
    ])
}

pub fn parse_edit_input(v: &Value) -> P<EditVal> {
    let m = obj(v)?;
    let file_path = intersect_path(m)?;
    edit_text_ok(m)?;
    Ok(EditVal {
        file_path: Some(file_path),
        old_string: first_some([
            nes(s_of(m, "old_string")),
            nes(s_of(m, "old_str")),
            nes(s_of(m, "oldContent")),
            nes(s_of(m, "old_content")),
        ]),
        new_string: first_some([
            nes(s_of(m, "new_string")),
            nes(s_of(m, "new_str")),
            nes(s_of(m, "newContent")),
            nes(s_of(m, "new_content")),
            nes(s_of(m, "content")),
        ]),
        unified_diff: diff_of(m),
    })
}

fn edit_output_file(v: &Value) -> P<(String, Option<String>)> {
    let m = obj(v)?;
    for k in PATH_KEYS {
        let attempt = (|| -> P<(String, Option<String>)> {
            let p = req_s(m, k)?.to_string();
            for dk in ["patch", "diff", "unified_diff", "unifiedDiff"] {
                opt_s(m, dk)?;
            }
            Ok((p, diff_of(m)))
        })();
        if attempt.is_ok() {
            return attempt;
        }
    }
    Err(())
}

pub fn parse_edit_output(v: &Value) -> P<EditVal> {
    let m = obj(v)?;
    // 1. intersection(path, text)
    if let Ok(p) = intersect_path(m) {
        if edit_text_ok(m).is_ok() {
            return Ok(EditVal {
                file_path: Some(p),
                old_string: None,
                new_string: new_content_of(m),
                unified_diff: diff_of(m),
            });
        }
    }
    // 2. { files: [...] } (min 1)
    if let Some(Value::Array(files)) = m.get("files") {
        if !files.is_empty() {
            let parsed: P<Vec<_>> = files.iter().map(edit_output_file).collect();
            if let Ok(parsed) = parsed {
                let first = &parsed[0];
                return Ok(EditVal {
                    file_path: Some(first.0.clone()),
                    old_string: None,
                    new_string: None,
                    unified_diff: first.1.clone(),
                });
            }
        }
    }
    // 3. text-only object
    edit_text_ok(m)?;
    Ok(EditVal {
        file_path: None,
        old_string: None,
        new_string: new_content_of(m),
        unified_diff: diff_of(m),
    })
}

// ---- search / glob / grep / web ---------------------------------------------------------------

pub fn parse_search_input(v: &Value) -> P<String> {
    let m = obj(v)?;
    for k in ["query", "q", "pattern"] {
        if let Some(Value::String(s)) = m.get(k) {
            return Ok(s.clone());
        }
    }
    Err(())
}

#[derive(Debug, Clone)]
pub struct GlobOut {
    pub duration_ms: Num,
    pub num_files: Num,
    pub filenames: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct GrepOut {
    pub mode: Option<SearchMode>,
    pub num_files: Num,
    pub filenames: Vec<String>,
    pub content: Option<String>,
    pub num_matches: Option<Num>,
}

#[derive(Debug, Clone)]
pub enum WebSearchEntry {
    Hits(Vec<WebResult>),
    Text(String),
}

#[derive(Debug, Clone)]
pub struct WebSearchOut {
    pub results: Vec<WebSearchEntry>,
    pub duration_seconds: Num,
}

#[derive(Debug, Clone)]
pub enum SearchOut {
    Grep(GrepOut),
    Glob(GlobOut),
    Web(WebSearchOut),
}

pub fn parse_glob_output(v: &Value) -> P<GlobOut> {
    let m = obj(v)?;
    let truncated = match m.get("truncated") {
        Some(Value::Bool(b)) => *b,
        _ => return Err(()),
    };
    Ok(GlobOut {
        duration_ms: req_n(m, "durationMs")?,
        num_files: req_nonneg_int(m, "numFiles")?,
        filenames: req_str_array(m, "filenames")?,
        truncated,
    })
}

pub fn parse_grep_output(v: &Value) -> P<GrepOut> {
    let m = obj(v)?;
    let mode = match opt_s(m, "mode")? {
        None => None,
        Some(s) => Some(SearchMode::parse(s).ok_or(())?),
    };
    let num_files = req_nonneg_int(m, "numFiles")?;
    let filenames = req_str_array(m, "filenames")?;
    let content = opt_s(m, "content")?.map(String::from);
    opt_nonneg_int(m, "numLines")?;
    let num_matches = opt_nonneg_int(m, "numMatches")?;
    opt_nonneg_int(m, "appliedLimit")?;
    opt_nonneg_int(m, "appliedOffset")?;
    Ok(GrepOut {
        mode,
        num_files,
        filenames,
        content,
        num_matches,
    })
}

/// Grep output that is really just text (Claude `{output}` wrapper, OpenCode string).
pub fn grep_from_text(output: &str) -> GrepOut {
    GrepOut {
        mode: None,
        num_files: Num::from(0),
        filenames: vec![],
        content: Some(output.to_string()),
        num_matches: None,
    }
}

pub fn parse_web_search_output(v: &Value) -> P<WebSearchOut> {
    let m = obj(v)?;
    req_s(m, "query")?;
    let results = match m.get("results") {
        Some(Value::Array(a)) => a,
        _ => return Err(()),
    };
    let mut entries = Vec::new();
    for e in results {
        match e {
            Value::Object(em) => {
                req_s(em, "tool_use_id")?;
                let hits = match em.get("content") {
                    Some(Value::Array(h)) => h,
                    _ => return Err(()),
                };
                let parsed: Result<Vec<_>, _> = hits
                    .iter()
                    .map(|h| WebResult::from_value(h).map_err(|_| ()))
                    .collect();
                entries.push(WebSearchEntry::Hits(parsed?));
            }
            Value::String(s) => entries.push(WebSearchEntry::Text(s.clone())),
            _ => return Err(()),
        }
    }
    Ok(WebSearchOut {
        results: entries,
        duration_seconds: req_n(m, "durationSeconds")?,
    })
}

#[derive(Debug, Clone)]
pub struct WebFetchIn {
    pub url: String,
    pub prompt: String,
}

#[derive(Debug, Clone)]
pub struct WebFetchOut {
    pub bytes: Num,
    pub code: Num,
    pub code_text: String,
    pub result: String,
    pub duration_ms: Num,
    pub url: String,
}

pub fn parse_web_fetch_input(v: &Value) -> P<WebFetchIn> {
    let m = obj(v)?;
    Ok(WebFetchIn {
        url: req_s(m, "url")?.into(),
        prompt: req_s(m, "prompt")?.into(),
    })
}

pub fn parse_web_fetch_output(v: &Value) -> P<WebFetchOut> {
    let m = obj(v)?;
    let bytes = req_nonneg_int(m, "bytes")?;
    let code = req_n(m, "code")?;
    if !is_safe_integer(&code) {
        return Err(());
    }
    Ok(WebFetchOut {
        bytes,
        code,
        code_text: req_s(m, "codeText")?.into(),
        result: req_s(m, "result")?.into(),
        duration_ms: req_n(m, "durationMs")?,
        url: req_s(m, "url")?.into(),
    })
}

// ---- detail builders ---------------------------------------------------------------------------

fn normalize_detail_path(file_path: Option<&str>, normalize: NormalizePath) -> Option<String> {
    let trimmed = js_trim(file_path?);
    if trimmed.is_empty() {
        return None;
    }
    match normalize {
        Some(f) => f(trimmed),
        None => Some(trimmed.to_string()),
    }
}

fn pick<'a>(a: Option<&'a str>, b: Option<&'a str>) -> Option<&'a str> {
    a.or(b)
}

pub fn to_shell_tool_detail(
    input: Option<&ShellIn>,
    output: Option<&ShellOut>,
) -> Option<ToolCallDetail> {
    let command = input
        .and_then(|i| i.command.clone())
        .or_else(|| output.and_then(|o| o.command.clone()))
        .filter(|c| !c.is_empty())?;
    Some(ToolCallDetail::Shell {
        command,
        cwd: input.and_then(|i| i.cwd.clone()).filter(|c| !c.is_empty()),
        output: output
            .and_then(|o| o.output.clone())
            .filter(|c| !c.is_empty()),
        exit_code: output.and_then(|o| o.exit_code.clone()).map(Some),
    })
}

pub fn to_read_tool_detail(
    input: Option<&ReadIn>,
    output: Option<&ReadOut>,
    normalize: NormalizePath,
) -> Option<ToolCallDetail> {
    let file_path = normalize_detail_path(
        pick(
            input.and_then(|i| i.file_path.as_deref()),
            output.and_then(|o| o.file_path.as_deref()),
        ),
        normalize,
    )?;
    let out_content = output
        .and_then(|o| o.content.clone())
        .filter(|c| !c.is_empty());
    let stripped = strip_read_line_number_gutter(out_content.as_deref());
    let content = stripped
        .as_ref()
        .map(|s| s.content.clone())
        .or(out_content)
        .filter(|c| !c.is_empty());
    let offset = input.and_then(|i| i.offset.clone()).or_else(|| {
        stripped
            .as_ref()
            .and_then(|s| s.start_line)
            .map(crate::num_from_f64)
    });
    Some(ToolCallDetail::Read {
        file_path,
        content,
        offset,
        limit: input.and_then(|i| i.limit.clone()),
    })
}

pub fn to_write_tool_detail(
    input: Option<&WriteVal>,
    output: Option<&WriteVal>,
    normalize: NormalizePath,
) -> Option<ToolCallDetail> {
    let file_path = normalize_detail_path(
        pick(
            input.and_then(|i| i.file_path.as_deref()),
            output.and_then(|o| o.file_path.as_deref()),
        ),
        normalize,
    )?;
    // `input?.content ?? output?.content` then truthiness
    let content = input
        .and_then(|i| i.content.clone())
        .or_else(|| output.and_then(|o| o.content.clone()))
        .filter(|c| !c.is_empty());
    Some(ToolCallDetail::Write { file_path, content })
}

pub fn to_edit_tool_detail(
    input: Option<&EditVal>,
    output: Option<&EditVal>,
    normalize: NormalizePath,
) -> Option<ToolCallDetail> {
    let file_path = normalize_detail_path(
        pick(
            input.and_then(|i| i.file_path.as_deref()),
            output.and_then(|o| o.file_path.as_deref()),
        ),
        normalize,
    )?;
    let new_string = input
        .and_then(|i| i.new_string.clone())
        .or_else(|| output.and_then(|o| o.new_string.clone()))
        .filter(|c| !c.is_empty());
    let unified_diff = input
        .and_then(|i| i.unified_diff.clone())
        .or_else(|| output.and_then(|o| o.unified_diff.clone()))
        .filter(|c| !c.is_empty());
    Some(ToolCallDetail::Edit {
        file_path,
        old_string: input
            .and_then(|i| i.old_string.clone())
            .filter(|c| !c.is_empty()),
        new_string,
        unified_diff,
    })
}

pub fn to_search_tool_detail(
    query: Option<&str>,
    output: Option<&SearchOut>,
    tool_name: Option<SearchToolName>,
) -> Option<ToolCallDetail> {
    let query = query.filter(|q| !q.is_empty())?;
    let mut d = SearchDetail {
        query: query.to_string(),
        tool_name,
        ..Default::default()
    };
    match output {
        Some(SearchOut::Grep(g)) => {
            d.content = g.content.clone().filter(|c| !c.is_empty());
            d.file_paths = (!g.filenames.is_empty()).then(|| g.filenames.clone());
            d.num_files = Some(g.num_files.clone());
            d.num_matches = g.num_matches.clone();
            d.mode = g.mode;
        }
        Some(SearchOut::Glob(g)) => {
            d.file_paths = (!g.filenames.is_empty()).then(|| g.filenames.clone());
            d.num_files = Some(g.num_files.clone());
            d.duration_ms = Some(g.duration_ms.clone());
            d.truncated = Some(g.truncated);
        }
        Some(SearchOut::Web(w)) => {
            let hits: Vec<WebResult> = w
                .results
                .iter()
                .flat_map(|e| match e {
                    WebSearchEntry::Hits(h) => h.clone(),
                    WebSearchEntry::Text(_) => vec![],
                })
                .collect();
            let notes: Vec<String> = w
                .results
                .iter()
                .filter_map(|e| match e {
                    WebSearchEntry::Text(t) => Some(t.clone()),
                    _ => None,
                })
                .collect();
            d.web_results = (!hits.is_empty()).then_some(hits);
            d.annotations = (!notes.is_empty()).then_some(notes);
            d.duration_seconds = Some(w.duration_seconds.clone());
        }
        None => {}
    }
    Some(ToolCallDetail::Search(d))
}

pub fn to_fetch_tool_detail(
    input: Option<&WebFetchIn>,
    output: Option<&WebFetchOut>,
) -> Option<ToolCallDetail> {
    let url = input
        .map(|i| i.url.clone())
        .or_else(|| output.map(|o| o.url.clone()))
        .filter(|u| !u.is_empty())?;
    Some(ToolCallDetail::Fetch {
        url,
        prompt: input.map(|i| i.prompt.clone()).filter(|p| !p.is_empty()),
        result: output.map(|o| o.result.clone()).filter(|p| !p.is_empty()),
        code: output.map(|o| o.code.clone()),
        code_text: output
            .map(|o| o.code_text.clone())
            .filter(|p| !p.is_empty()),
        bytes: output.map(|o| o.bytes.clone()),
        duration_ms: output.map(|o| o.duration_ms.clone()),
    })
}
