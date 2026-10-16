//! Small ports used by several tools: path helpers (`path-utils.ts`), provider/model parsing
//! (`mcp-shared.ts`), duration parsing, terminal key tokens.

use paseo_store::paths::resolve_path;

use crate::error::ToolError;

fn has_home_prefix(value: &str) -> bool {
    value == "~" || value.starts_with("~/")
}

fn home_dir() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/".to_owned())
}

/// `path.isAbsolute` for POSIX paths and Windows drive/UNC forms.
fn is_absolute(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with('\\')
        || (value.len() >= 2
            && value.as_bytes()[0].is_ascii_alphabetic()
            && value.as_bytes()[1] == b':')
}

/// `path.resolve(path)` against `process_cwd` (the host's `process.cwd()`).
fn resolve_in(process_cwd: &str, path: &str) -> String {
    if is_absolute(path) {
        resolve_path(path)
    } else {
        resolve_path(&format!("{process_cwd}/{path}"))
    }
}

/// `expandUserPath`: trim, expand `~`, make absolute against `process_cwd`.
pub fn expand_user_path(value: &str, process_cwd: &str) -> String {
    let trimmed = js_trim(value);
    if has_home_prefix(trimmed) {
        let rest = trimmed.get(2..).unwrap_or("");
        return resolve_in(process_cwd, &format!("{}/{}", home_dir(), rest));
    }
    resolve_in(process_cwd, trimmed)
}

/// `resolvePathFromBase`.
pub fn resolve_path_from_base(base_cwd: &str, requested_path: &str, process_cwd: &str) -> String {
    let trimmed = js_trim(requested_path);
    if has_home_prefix(trimmed) || is_absolute(trimmed) {
        return expand_user_path(trimmed, process_cwd);
    }
    resolve_in(process_cwd, &format!("{base_cwd}/{trimmed}"))
}

/// A JS number as JSON: integral values are integers (`2`, not `2.0`), like `JSON.stringify`.
pub fn json_number(value: f64) -> serde_json::Value {
    if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        serde_json::json!(value as i64)
    } else {
        serde_json::json!(value)
    }
}

/// `isSameOrDescendantPath(base, candidate)`.
pub fn is_same_or_descendant_path(base_path: &str, candidate_path: &str) -> bool {
    fn norm(p: &str) -> String {
        let replaced = p.replace('\\', "/");
        replaced
            .strip_suffix('/')
            .map_or(replaced.clone(), str::to_owned)
    }
    fn has_drive(p: &str) -> bool {
        let b = p.as_bytes();
        b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/'
    }
    let mut base = norm(base_path);
    let mut candidate = norm(candidate_path);
    if has_drive(&base) || has_drive(&candidate) {
        base = base.to_lowercase();
        candidate = candidate.to_lowercase();
    }
    candidate == base || candidate.starts_with(&format!("{base}/"))
}

/// `String.prototype.trim`.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

const PROVIDER_MODEL_USAGE: &str = "provider must be provider/model, for example codex/gpt-5.4";

/// `resolveRequiredProviderModel`: `provider/model`, both parts non-empty after trimming.
pub fn resolve_required_provider_model(value: &str) -> Result<(String, String), ToolError> {
    let input = js_trim(value);
    let Some(slash) = input.find('/') else {
        return Err(ToolError::new(PROVIDER_MODEL_USAGE));
    };
    if slash == 0 || slash == input.len() - 1 {
        return Err(ToolError::new(PROVIDER_MODEL_USAGE));
    }
    let provider = js_trim(&input[..slash]);
    let model = js_trim(&input[slash + 1..]);
    if provider.is_empty() || model.is_empty() {
        return Err(ToolError::new(PROVIDER_MODEL_USAGE));
    }
    Ok((provider.to_owned(), model.to_owned()))
}

/// `resolveScheduleProviderAndModel`.
pub fn resolve_schedule_provider_and_model(
    provider: Option<&str>,
    default_provider: &str,
) -> Result<(String, Option<String>), ToolError> {
    let trimmed = provider.map(js_trim).filter(|p| !p.is_empty());
    let input = trimmed.unwrap_or(default_provider);
    let Some(slash) = input.find('/') else {
        return Ok((input.to_owned(), None));
    };
    let provider = js_trim(&input[..slash]);
    let model = js_trim(&input[slash + 1..]);
    if provider.is_empty() || model.is_empty() {
        return Err(ToolError::new(
            "provider must be <provider> or <provider>/<model>",
        ));
    }
    Ok((provider.to_owned(), Some(model.to_owned())))
}

/// `parseDurationString` (a JS number of milliseconds). Unanchored like the TS regex: any
/// `<digits><s|m|h>` pair counts.
pub fn parse_duration_string(input: &str) -> Result<f64, ToolError> {
    let trimmed = js_trim(input);
    if !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(trimmed.parse::<f64>().unwrap_or(f64::INFINITY) * 1000.0);
    }
    let bytes = trimmed.as_bytes();
    let mut total = 0.0;
    let mut matched = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i < bytes.len() && matches!(bytes[i], b's' | b'm' | b'h') {
                matched = true;
                let value = trimmed[start..i].parse::<f64>().unwrap_or(f64::INFINITY);
                total += match bytes[i] {
                    b's' => value * 1000.0,
                    b'm' => value * 60.0 * 1000.0,
                    _ => value * 60.0 * 60.0 * 1000.0,
                };
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    if !matched {
        return Err(ToolError::new(format!(
            "Invalid duration format: {input}. Use formats like: 5m, 30s, 1h, 2h30m"
        )));
    }
    Ok(total)
}

/// `new Date(now + ms).toISOString()`; a result outside the JS date range is a `RangeError`.
pub fn iso_after(now_ms: i64, add_ms: f64) -> Result<String, ToolError> {
    let target = now_ms as f64 + add_ms;
    if !target.is_finite() || target.abs() > 8.64e15 {
        return Err(ToolError::new("Invalid time value"));
    }
    Ok(paseo_core::time::iso8601_from_millis(target as i64))
}

/// `resolveTerminalKeyToken`.
pub fn resolve_terminal_key_token(key: &str, literal: bool) -> String {
    if literal {
        return key.to_owned();
    }
    match key {
        "Enter" => "\r",
        "Tab" => "\t",
        "Escape" => "\u{1b}",
        "Space" => " ",
        "BSpace" => "\u{7f}",
        "C-c" => "\u{3}",
        "C-d" => "\u{4}",
        "C-z" => "\u{1a}",
        "C-l" => "\u{c}",
        "C-a" => "\u{1}",
        "C-e" => "\u{5}",
        other => other,
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration_string("5m").unwrap(), 300_000.0);
        assert_eq!(parse_duration_string("2h30m").unwrap(), 9_000_000.0);
        assert_eq!(parse_duration_string("45").unwrap(), 45_000.0);
        assert_eq!(parse_duration_string("x1s").unwrap(), 1000.0);
        assert!(parse_duration_string("soon").is_err());
        assert_eq!(
            parse_duration_string("soon").unwrap_err().0,
            "Invalid duration format: soon. Use formats like: 5m, 30s, 1h, 2h30m"
        );
    }

    #[test]
    fn provider_model() {
        assert_eq!(
            resolve_required_provider_model(" claude / opus ").unwrap(),
            ("claude".into(), "opus".into())
        );
        assert!(resolve_required_provider_model("claude").is_err());
        assert!(resolve_required_provider_model("/m").is_err());
        assert!(resolve_required_provider_model("c/").is_err());
        assert_eq!(
            resolve_schedule_provider_and_model(Some("codex"), "x").unwrap(),
            ("codex".into(), None)
        );
        assert_eq!(
            resolve_schedule_provider_and_model(None, "claude/m").unwrap(),
            ("claude".into(), Some("m".into()))
        );
        assert!(resolve_schedule_provider_and_model(Some("a/ "), "x").is_err());
    }

    #[test]
    fn descendant_paths() {
        assert!(is_same_or_descendant_path("/work/repo", "/work/repo"));
        assert!(is_same_or_descendant_path("/work/repo/", "/work/repo/sub"));
        assert!(!is_same_or_descendant_path("/work/repo", "/work/repo2"));
        assert!(is_same_or_descendant_path("C:\\Work", "c:/work/x"));
    }

    #[test]
    fn paths_resolve() {
        assert_eq!(resolve_path_from_base("/a/b", "../c", "/p"), "/a/c");
        assert_eq!(resolve_path_from_base("/a/b", "/x/y/", "/p"), "/x/y");
        assert_eq!(expand_user_path(" /x/./y ", "/p"), "/x/y");
        assert_eq!(expand_user_path("rel/../z", "/p/q"), "/p/q/z");
        assert_eq!(resolve_path_from_base("rel", "c", "/p"), "/p/rel/c");
        assert_eq!(json_number(2.0), serde_json::json!(2));
        assert_eq!(json_number(2.5), serde_json::json!(2.5));
    }
}
