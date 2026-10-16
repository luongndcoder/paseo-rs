//! `tool-name-normalization.ts` and `path-utils.ts` from `packages/protocol/src`.

use crate::js::js_trim;

pub fn normalize_tool_name(name: &str) -> String {
    js_trim(name).to_lowercase()
}

/// `name.toLowerCase().match(/[a-z0-9]+/g)`
pub fn tokenize_tool_name(name: &str) -> Vec<String> {
    let n = normalize_tool_name(name);
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in n.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn get_tool_leaf_name(name: &str) -> Option<String> {
    tokenize_tool_name(name).pop()
}

pub fn is_speak_tool_name(name: &str) -> bool {
    get_tool_leaf_name(name).as_deref() == Some("speak")
}

fn segments(normalized: &str, sep: &str) -> Vec<String> {
    normalized
        .split(sep)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn is_paseo_ns(first: &str) -> bool {
    first == "paseo" || first.starts_with("paseo_")
}

pub fn is_paseo_tool_name(name: &str) -> bool {
    let n = normalize_tool_name(name);
    if is_speak_tool_name(&n) {
        return false;
    }
    if n.contains("__") {
        let s = segments(&n, "__");
        return s.len() >= 3 && s[0] == "mcp" && is_paseo_ns(&s[1]);
    }
    if n.contains('.') {
        let first = n.split('.').next().unwrap_or("");
        return is_paseo_ns(first);
    }
    false
}

pub fn get_paseo_tool_leaf_name(name: &str) -> Option<String> {
    let n = normalize_tool_name(name);
    if n.contains("__") {
        let s = segments(&n, "__");
        if s.len() >= 3 && s[0] == "mcp" && is_paseo_ns(&s[1]) {
            return Some(s[2..].join("__"));
        }
        return None;
    }
    if n.contains('.') {
        let mut parts = n.split('.');
        let first = parts.next().unwrap_or("");
        if is_paseo_ns(first) {
            return Some(parts.collect::<Vec<_>>().join("."));
        }
        return None;
    }
    None
}

pub fn is_likely_namespaced_tool_name(name: &str) -> bool {
    let n = normalize_tool_name(name);
    if n.contains(['.', ':', '/']) {
        return true;
    }
    if !n.contains("__") {
        return false;
    }
    let s = segments(&n, "__");
    s.len() >= 3 || (s.len() == 2 && s[1].contains('_'))
}

pub fn is_likely_external_tool_name(name: &str) -> bool {
    let n = normalize_tool_name(name);
    if n.is_empty() {
        return false;
    }
    is_speak_tool_name(&n) || is_likely_namespaced_tool_name(&n)
}

/// `stripCwdPrefix(filePath, cwd?)`.
pub fn strip_cwd_prefix(file_path: &str, cwd: Option<&str>) -> String {
    let Some(cwd) = cwd.filter(|c| !c.is_empty()) else {
        return file_path.to_string();
    };
    if file_path.is_empty() {
        return file_path.to_string();
    }
    let normalized_cwd = cwd.replace('\\', "/");
    let normalized_cwd = normalized_cwd.trim_end_matches('/');
    let normalized_path = file_path.replace('\\', "/");
    let prefix = format!("{normalized_cwd}/");
    if let Some(rest) = normalized_path.strip_prefix(&prefix) {
        return rest.to_string();
    }
    if normalized_path == normalized_cwd {
        return ".".into();
    }
    file_path.to_string()
}
