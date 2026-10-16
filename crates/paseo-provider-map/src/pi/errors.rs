//! Terminal error formatting (`formatPiErrorMessage` and friends).

use paseo_tool_detail::js::{json_stringify, utf16_prefix};
use serde_json::Value;

use super::util::truthy_str;

/// `piAssistantText`: text and thinking parts joined with a blank line, trimmed.
pub(super) fn assistant_text(message: &Value) -> Option<String> {
    let parts: Vec<&str> = message
        .get("content")
        .and_then(Value::as_array)
        .map(|content| {
            content
                .iter()
                .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                    Some("text") => p.get("text").and_then(Value::as_str),
                    Some("thinking") => p.get("thinking").and_then(Value::as_str),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let joined = parts.join("\n\n");
    let text = paseo_tool_detail::js::js_trim(&joined);
    (!text.is_empty()).then(|| text.to_string())
}

/// `formatPiErrorMessage`
pub(super) fn format_error_message(message: &Value) -> String {
    let headline = message
        .get("errorMessage")
        .and_then(Value::as_str)
        .map(paseo_tool_detail::js::js_trim)
        .filter(|h| !h.is_empty())
        .unwrap_or("Pi turn failed");
    let s = |k: &str| truthy_str(message.get(k));
    let mut details = Vec::new();
    if let Some(r) = s("stopReason") {
        details.push(format!("stopReason={r}"));
    }
    if let (Some(p), Some(m)) = (s("provider"), s("model")) {
        details.push(format!("model={p}/{m}"));
    }
    if let Some(r) = s("responseModel") {
        details.push(format!("responseModel={r}"));
    }
    if let Some(r) = s("responseId") {
        details.push(format!("responseId={r}"));
    }
    if let Some(partial) = assistant_text(message) {
        details.push(format!(
            "partial={}",
            json_stringify(&Value::String(utf16_prefix(&partial, 500)))
        ));
    }
    if details.is_empty() {
        headline.to_string()
    } else {
        format!("{headline} ({})", details.join(", "))
    }
}

pub(super) fn latest_assistant(messages: &[Value]) -> Option<&Value> {
    messages
        .iter()
        .rfind(|m| m.get("role").and_then(Value::as_str) == Some("assistant"))
}

/// `latestPiErrorMessage`
pub(super) fn latest_error_message(messages: &[Value]) -> Option<String> {
    let assistant = latest_assistant(messages)?;
    let err = assistant.get("errorMessage").and_then(Value::as_str)?;
    if paseo_tool_detail::js::js_trim(err).is_empty() {
        return None;
    }
    Some(format_error_message(assistant))
}

/// `isPiAbortedTerminalResponse`
pub(super) fn is_aborted_terminal_response(messages: &[Value]) -> bool {
    latest_assistant(messages)
        .and_then(|m| m.get("stopReason"))
        .and_then(Value::as_str)
        .is_some_and(|r| r.to_lowercase() == "aborted")
}
