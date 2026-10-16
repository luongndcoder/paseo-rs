//! Read-content helpers from `tool-call-mapper-utils.ts`: `nonEmptyString`, `flattenReadContent`,
//! `stripReadLineNumberGutter`, `truncateDiffText`.

use crate::js::{is_js_whitespace, utf16_len, utf16_prefix};
use serde_json::Value;

pub fn non_empty_string(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn chunk_text(chunk: &Value) -> Option<String> {
    let o = chunk.as_object()?;
    non_empty_string(o.get("text"))
        .or_else(|| non_empty_string(o.get("content")))
        .or_else(|| non_empty_string(o.get("output")))
}

/// `flattenReadContent`: a string, a chunk object, or an array of chunk objects.
pub fn flatten_read_content(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => (!s.is_empty()).then(|| s.clone()),
        Value::Array(a) => {
            let parts: Vec<String> = a.iter().filter_map(chunk_text).collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        other => chunk_text(other),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrippedReadContent {
    pub content: String,
    /// JS number (`Number.parseInt`), kept as f64.
    pub start_line: Option<f64>,
}

/// `/^\s*(\d+)\t(.*)$/` on one line (no `\n` inside). `.` excludes `\r`, U+2028, U+2029.
fn match_gutter_line(line: &str) -> Option<(f64, &str)> {
    let rest = line.trim_start_matches(is_js_whitespace);
    let digits_end = rest
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    if digits_end == 0 {
        return None;
    }
    let after = &rest[digits_end..];
    let body = after.strip_prefix('\t')?;
    if body
        .chars()
        .any(|c| matches!(c, '\r' | '\u{2028}' | '\u{2029}'))
    {
        return None;
    }
    let n: f64 = rest[..digits_end].parse().ok()?;
    Some((n, body))
}

/// Claude's Read tool returns `cat -n` content. Strip the gutter and report the first line
/// number. Guarded so real source is never mistaken for a gutter.
pub fn strip_read_line_number_gutter(content: Option<&str>) -> Option<StrippedReadContent> {
    let text = content.filter(|s| !s.is_empty())?;
    let normalized = text.replace("\r\n", "\n");
    let mut stripped: Vec<&str> = Vec::new();
    let mut non_empty = 0usize;
    let mut matched = 0usize;
    let mut start_line: Option<f64> = None;
    let mut prev: Option<f64> = None;
    let mut sequential = true;
    let mut first_non_empty_matched = false;
    let mut saw_non_empty = false;

    for line in normalized.split('\n') {
        if line.is_empty() {
            stripped.push(line);
            continue;
        }
        non_empty += 1;
        match match_gutter_line(line) {
            None => {
                if !saw_non_empty {
                    return None;
                }
                stripped.push(line);
                saw_non_empty = true;
            }
            Some((n, body)) => {
                if !saw_non_empty {
                    first_non_empty_matched = true;
                }
                saw_non_empty = true;
                matched += 1;
                if start_line.is_none() {
                    start_line = Some(n);
                }
                if let Some(p) = prev {
                    if n != p + 1.0 {
                        sequential = false;
                    }
                }
                prev = Some(n);
                stripped.push(body);
            }
        }
    }

    if !first_non_empty_matched || !sequential || non_empty == 0 {
        return None;
    }
    if (matched as f64) / (non_empty as f64) < 0.5 {
        return None;
    }
    Some(StrippedReadContent {
        content: stripped.join("\n"),
        start_line,
    })
}

/// `truncateDiffText(text, maxChars = 12_000)`; lengths in UTF-16 code units.
pub fn truncate_diff_text(text: Option<&str>, max_chars: usize) -> Option<String> {
    let text = text?;
    let len = utf16_len(text);
    if len <= max_chars {
        return Some(text.to_string());
    }
    let truncated = len - max_chars;
    Some(format!(
        "{}\n...[truncated {truncated} chars]",
        utf16_prefix(text, max_chars)
    ))
}

pub const DEFAULT_DIFF_MAX_CHARS: usize = 12_000;
