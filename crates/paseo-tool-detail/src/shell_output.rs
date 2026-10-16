//! Codex shell envelope helpers from `tool-call-mapper-utils.ts`.

use crate::js::{ascii_lower, is_js_whitespace, js_trim};

const HEADER_PREFIXES: [&str; 4] = [
    "chunk id:",
    "wall time:",
    "process exited with code",
    "original token count:",
];

fn is_header_line(line: &str) -> bool {
    let n = js_trim(line).to_lowercase();
    HEADER_PREFIXES.iter().any(|p| n.starts_with(p))
}

fn looks_like_envelope(lines: &[&str]) -> bool {
    let Some(first) = lines.first() else {
        return false;
    };
    if !js_trim(first).to_lowercase().starts_with("chunk id:") {
        return false;
    }
    let window: Vec<String> = lines
        .iter()
        .take(8)
        .map(|l| js_trim(l).to_lowercase())
        .collect();
    window.iter().any(|l| l.starts_with("wall time:"))
        && window
            .iter()
            .any(|l| l.starts_with("process exited with code"))
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

pub fn extract_codex_shell_output(value: Option<&str>) -> Option<String> {
    let text = value.filter(|s| !s.is_empty())?;
    let normalized = text.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    if !looks_like_envelope(&lines) {
        return Some(text.to_string());
    }
    if let Some(i) = lines.iter().position(|l| js_trim(l) == "Output:") {
        return non_empty(lines[i + 1..].join("\n"));
    }
    let first_body = (1..lines.len()).find(|&i| !is_header_line(lines[i]))?;
    non_empty(lines[first_body..].join("\n"))
}

/// `/process running with session id\s+([A-Za-z0-9._:-]+)/i`
pub fn extract_codex_terminal_session_id(value: Option<&str>) -> Option<String> {
    let text = value.filter(|s| !s.is_empty())?;
    let lower = ascii_lower(text);
    const NEEDLE: &str = "process running with session id";
    let mut from = 0;
    while let Some(rel) = lower[from..].find(NEEDLE) {
        let after_start = from + rel + NEEDLE.len();
        let after = &text[after_start..];
        let rest = after.trim_start_matches(is_js_whitespace);
        if rest.len() < after.len() {
            let id: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
                .collect();
            if !id.is_empty() {
                return Some(id);
            }
        }
        from = from + rel + 1;
    }
    None
}
