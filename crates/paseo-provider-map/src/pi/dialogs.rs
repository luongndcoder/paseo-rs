//! Extension UI requests: dialogs as question permissions, permission answers as UI responses,
//! and the `ctx.ui.notify` marker payloads Paseo's Pi integration extension sends.

use paseo_agent_types::Map;
use serde_json::{json, Value};

use super::util::*;

const QUESTION_RESPONSE_HEADER: &str = "Response";

fn question_permission(
    event: &Value,
    provider: &str,
    label: &str,
    question: String,
    options: Vec<String>,
    placeholder: Option<&str>,
    allow_empty: bool,
) -> Value {
    let method = event.get("method").and_then(Value::as_str).unwrap_or("");
    let mut q = json!({
        "question": question,
        "header": QUESTION_RESPONSE_HEADER,
        "options": options.iter().map(|l| json!({"label": l})).collect::<Vec<_>>(),
        "multiSelect": false,
    });
    if let Some(p) = placeholder.filter(|p| !p.is_empty()) {
        q["placeholder"] = json!(p);
    }
    if allow_empty {
        q["allowEmpty"] = json!(true);
        q["dismissLabel"] = json!("Skip");
    }
    json!({
        "id": event.get("id").cloned().unwrap_or(Value::Null),
        "provider": provider,
        "name": format!("{label} {method}"),
        "kind": "question",
        "title": question,
        "input": {"questions": [q]},
        "metadata": {"extensionUiMethod": method, "answerHeader": QUESTION_RESPONSE_HEADER},
    })
}

/// `mapExtensionUiRequestToPermission`
pub(super) fn map_ui_request_to_permission(
    event: &Value,
    provider: &str,
    label: &str,
) -> Option<Value> {
    let title = optional_string(event.get("title"));
    match event.get("method").and_then(Value::as_str)? {
        "select" => {
            let options = event
                .get("options")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Some(question_permission(
                event,
                provider,
                label,
                title.unwrap_or("Select an option").to_string(),
                options,
                None,
                false,
            ))
        }
        "input" => {
            let placeholder = optional_string(event.get("placeholder"));
            let allow_empty = is_optional_input_placeholder(placeholder);
            let question = if !allow_empty {
                title.unwrap_or("Enter a value").to_string()
            } else {
                let probe = format!("{}\n{}", title.unwrap_or(""), placeholder.unwrap_or(""));
                if has_word_ci(&probe, "comment") {
                    "Optional comment".to_string()
                } else {
                    "Optional response".to_string()
                }
            };
            Some(question_permission(
                event,
                provider,
                label,
                question,
                Vec::new(),
                placeholder,
                allow_empty,
            ))
        }
        "editor" => Some(question_permission(
            event,
            provider,
            label,
            title.unwrap_or("Edit text").to_string(),
            Vec::new(),
            None,
            false,
        )),
        "confirm" => {
            let question = [title, optional_string(event.get("message"))]
                .into_iter()
                .flatten()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            Some(question_permission(
                event,
                provider,
                label,
                question,
                vec!["Yes".into(), "No".into()],
                None,
                false,
            ))
        }
        _ => None,
    }
}

/// `firstPermissionAnswer`
pub(super) fn first_permission_answer(input: Option<&Value>) -> Option<&str> {
    input?
        .get("answers")?
        .as_object()?
        .values()
        .find_map(Value::as_str)
}

/// `buildExtensionUiResponse`
pub(super) fn build_ui_response(request: &Value, response: &Value) -> Value {
    if response.get("behavior").and_then(Value::as_str) == Some("deny") {
        return json!({"cancelled": true});
    }
    let method = request
        .get("metadata")
        .and_then(|m| m.get("extensionUiMethod"))
        .and_then(Value::as_str);
    let Some(answer) = first_permission_answer(response.get("updatedInput")) else {
        return json!({"cancelled": true});
    };
    if method == Some("confirm") {
        let t = paseo_tool_detail::js::js_trim(answer);
        return json!({"confirmed": t.eq_ignore_ascii_case("yes")});
    }
    json!({"value": answer})
}

/// `parseExtensionMarkerPayload`
pub(super) fn parse_marker_payload(message: &str, marker: &str) -> Option<Map> {
    let rest = message.strip_prefix(marker)?.strip_prefix(' ')?;
    match serde_json::from_str::<Value>(rest).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

/// `PiCapturedEntry`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiCapturedEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub text: String,
}

/// `parseCapturedEntries`
pub(super) fn parse_captured_entries(value: Option<&Value>) -> Vec<PiCapturedEntry> {
    let Some(Value::Array(entries)) = value else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|e| {
            let m = e.as_object()?;
            let id = optional_string(m.get("id")).map(paseo_tool_detail::js::js_trim)?;
            let text = optional_string(m.get("text"))?;
            if id.is_empty() {
                return None;
            }
            let parent_id = match m.get("parentId") {
                Some(Value::Null) => None,
                other => optional_string(other)
                    .map(paseo_tool_detail::js::js_trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string),
            };
            Some(PiCapturedEntry {
                id: id.to_string(),
                parent_id,
                text: text.to_string(),
            })
        })
        .collect()
}
