//! Port of `terminal-capture.ts`: plain-text capture of scrollback + grid with
//! the same line-index semantics and `strip-ansi` 7.1.2 behavior.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Value};

// ansi-regex 6.2.2 (the version strip-ansi 7.1.2 pulls in), translated for the
// `regex` crate: ASCII digit classes and escaped nested brackets.
const ANSI_PATTERN: &str = concat!(
    r"(?:\x{1B}\][\s\S]*?(?:\x{07}|\x{1B}\x{5C}|\x{9C}))",
    "|",
    r"[\x{1B}\x{9B}][\[\]()#;?]*(?:[0-9]{1,4}(?:[;:][0-9]{0,4})*)?[0-9A-PR-TZcf-nq-uy=><~]"
);

pub fn strip_ansi(text: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(ANSI_PATTERN).expect("ansi pattern is valid"));
    re.replace_all(text, "").into_owned()
}

/// JS `String.prototype.trimEnd` whitespace set (differs from `char::is_whitespace`:
/// includes U+FEFF, excludes U+0085).
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{D}'
            | ' '
            | '\u{A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

pub fn trim_end_js(text: &str) -> &str {
    text.trim_end_matches(is_js_whitespace)
}

fn resolve_index(line: Option<i64>, total: i64, fallback_start: bool) -> i64 {
    if total == 0 {
        return if fallback_start { 0 } else { -1 };
    }
    let default_index = if fallback_start { 0 } else { total - 1 };
    let Some(line) = line else {
        return default_index;
    };
    let resolved = if line < 0 { total + line } else { line };
    resolved.clamp(0, total - 1)
}

/// `rows` are the raw (untrimmed) row texts, scrollback first.
pub fn capture_lines(
    rows: Vec<String>,
    start: Option<i64>,
    end: Option<i64>,
    strip: bool,
) -> Value {
    let all: Vec<String> = rows
        .iter()
        .map(|row| {
            let trimmed = trim_end_js(row);
            if strip {
                strip_ansi(trimmed)
            } else {
                trimmed.to_owned()
            }
        })
        .collect();
    let total = all.len() as i64;
    let start_index = resolve_index(start, total, true);
    let end_index = resolve_index(end, total, false);
    if total == 0 || start_index > end_index {
        return json!({ "lines": [], "totalLines": total });
    }
    let lines = &all[start_index as usize..=end_index as usize];
    json!({ "lines": lines, "totalLines": total })
}
