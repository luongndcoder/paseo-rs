//! `coerceToolCallId` from `tool-call-mapper-utils.ts`.

use crate::js::{hash_text, js_trim, json_stringify};
use serde_json::Value;

/// `input = None` is JS `undefined` (stringifies to the empty string); `Some(Null)` is `null`
/// (stringifies to `"null"`).
pub fn coerce_tool_call_id_parts(
    provider_prefix: &str,
    raw_call_id: Option<&str>,
    tool_name: &str,
    input: Option<&Value>,
) -> String {
    if let Some(id) = raw_call_id {
        if !js_trim(id).is_empty() {
            return id.to_string();
        }
    }
    let serialized = input.map(json_stringify).unwrap_or_default();
    format!(
        "{provider_prefix}-{}",
        hash_text(&format!("{tool_name}:{serialized}"))
    )
}

/// JSON form: `{providerPrefix, rawCallId?, toolName, input?}`. A missing `input` key is
/// `undefined`.
pub fn coerce_tool_call_id(params: &Value) -> String {
    let prefix = params
        .get("providerPrefix")
        .and_then(Value::as_str)
        .unwrap_or("");
    let raw = params.get("rawCallId").and_then(Value::as_str);
    let tool = params.get("toolName").and_then(Value::as_str).unwrap_or("");
    coerce_tool_call_id_parts(prefix, raw, tool, params.get("input"))
}
