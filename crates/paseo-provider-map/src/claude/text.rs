//! Text and JSON helpers shared by the Claude mapper: transcript noise filters, tool-result
//! coercion, JS-compatible number normalisation. Ports of the free functions in `claude/agent.ts`.

use paseo_tool_detail::js::{js_trim, json_stringify};
use serde_json::{Map, Value};

pub type Obj = Map<String, Value>;

pub const INTERRUPT_TOOL_USE_PLACEHOLDER: &str = "[Request interrupted by user for tool use]";
const NO_RESPONSE_REQUESTED_PLACEHOLDER: &str = "No response requested.";

pub fn obj_get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.as_object().and_then(|o| o.get(key))
}

pub fn str_of(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
}

/// `readTrimmedString`: a string trimmed with JS `trim`, `None` when empty or not a string.
pub fn read_trimmed(v: Option<&Value>) -> Option<String> {
    let s = v?.as_str()?;
    let t = js_trim(s);
    (!t.is_empty()).then(|| t.to_owned())
}

/// First non-empty trimmed string among the candidates (`firstTrimmedString`).
pub fn first_trimmed(candidates: &[Option<&Value>]) -> Option<String> {
    candidates.iter().find_map(|c| read_trimmed(*c))
}

/// JS truthiness of a JSON value.
pub fn js_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn normalize_transcript_text(v: &Value) -> Option<&str> {
    let t = js_trim(v.as_str()?);
    (!t.is_empty()).then_some(t)
}

fn is_interrupt_placeholder(text: &str) -> bool {
    // /^\[Request interrupted by user(?:[^\]]*)\]$/
    const PREFIX: &str = "[Request interrupted by user";
    let Some(rest) = text.strip_prefix(PREFIX) else {
        return false;
    };
    match rest.strip_suffix(']') {
        Some(middle) => !middle.contains(']'),
        None => false,
    }
}

fn is_local_command_stdout(text: &str) -> bool {
    const OPEN: &str = "<local-command-stdout>";
    const CLOSE: &str = "</local-command-stdout>";
    text.len() >= OPEN.len() + CLOSE.len() && text.starts_with(OPEN) && text.ends_with(CLOSE)
}

pub fn is_transcript_noise_str(text: &str) -> bool {
    let t = js_trim(text);
    if t.is_empty() {
        return false;
    }
    is_interrupt_placeholder(t)
        || t == NO_RESPONSE_REQUESTED_PLACEHOLDER
        || is_local_command_stdout(t)
}

fn collect_text_content_parts(content: &Value) -> Vec<String> {
    match content {
        Value::String(_) => normalize_transcript_text(content)
            .map(|s| vec![s.to_owned()])
            .unwrap_or_default(),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                let Some(o) = block.as_object() else {
                    continue;
                };
                if let Some(t) = o.get("text").and_then(normalize_transcript_text) {
                    parts.push(t.to_owned());
                    continue;
                }
                if let Some(i) = o.get("input").and_then(normalize_transcript_text) {
                    parts.push(i.to_owned());
                }
            }
            parts
        }
        _ => Vec::new(),
    }
}

pub fn is_transcript_noise_content(content: &Value) -> bool {
    let parts = collect_text_content_parts(content);
    !parts.is_empty() && parts.iter().all(|p| is_transcript_noise_str(p))
}

fn tag_value<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    // /<tag>([\s\S]*?)<\/tag>/ (first match, lazy)
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(&text[start..end])
}

fn command_prompt_name(text: &str) -> Option<String> {
    if let Some(name) = tag_value(text, "command-name").map(js_trim) {
        if !name.is_empty() {
            return Some(if name.starts_with('/') {
                name.to_owned()
            } else {
                format!("/{name}")
            });
        }
    }
    let message = js_trim(tag_value(text, "command-message")?);
    if message.is_empty() {
        return None;
    }
    Some(if message.starts_with('/') {
        message.to_owned()
    } else {
        format!("/{message}")
    })
}

/// `normalizeClaudeUserPromptText`.
pub fn normalize_user_prompt_text(text: &str) -> Option<String> {
    let normalized = js_trim(text);
    if tag_value(normalized, "command-message").is_none() {
        return (!normalized.is_empty()).then(|| normalized.to_owned());
    }
    let command = command_prompt_name(normalized)?;
    match tag_value(normalized, "command-args").map(js_trim) {
        Some(args) if !args.is_empty() => Some(format!("{command} {args}")),
        _ => Some(command),
    }
}

/// `extractUserMessageText`.
pub fn extract_user_message_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => {
            let normalized = js_trim(s);
            if normalized.is_empty() || is_transcript_noise_str(normalized) {
                return None;
            }
            normalize_user_prompt_text(normalized)
        }
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                let Some(o) = block.as_object() else {
                    continue;
                };
                for key in ["text", "input"] {
                    let Some(text) = o.get(key).and_then(Value::as_str) else {
                        continue;
                    };
                    let trimmed = js_trim(text);
                    if trimmed.is_empty() {
                        continue;
                    }
                    if !is_transcript_noise_str(trimmed) {
                        if let Some(n) = normalize_user_prompt_text(trimmed) {
                            parts.push(n);
                        }
                    }
                    break;
                }
            }
            if parts.is_empty() {
                return None;
            }
            let combined = parts.join("\n\n");
            let combined = js_trim(&combined);
            (!combined.is_empty()).then(|| combined.to_owned())
        }
        _ => None,
    }
}

// ---- tool-result coercion ----------------------------------------------------------------------

fn is_tool_result_text_block(v: &Value) -> bool {
    v.as_object().is_some_and(|o| {
        o.get("type").and_then(Value::as_str) == Some("text")
            && o.get("text").is_some_and(Value::is_string)
    })
}

fn normalize_for_deterministic_string(v: &Value) -> Value {
    match v {
        Value::Array(a) => Value::Array(a.iter().map(normalize_for_deterministic_string).collect()),
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), normalize_for_deterministic_string(&o[k]));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// `deterministicStringify`: object keys sorted by UTF-16 code units, then `JSON.stringify`.
pub fn deterministic_stringify(v: Option<&Value>) -> String {
    let Some(v) = v else {
        return String::new();
    };
    match normalize_for_deterministic_string(v) {
        Value::String(s) => s,
        other => json_stringify(&other),
    }
}

/// `coerceToolResultContentToString`.
pub fn coerce_tool_result_content_to_string(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) if a.iter().all(is_tool_result_text_block) => a
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        other => deterministic_stringify(other),
    }
}

/// A base64 image block pulled out of a tool result.
#[derive(Debug, Clone)]
pub struct ImageOutput {
    pub data: String,
    pub mime_type: Option<String>,
}

fn base64_image_output(block: &Value) -> Option<ImageOutput> {
    let o = block.as_object()?;
    if o.get("type").and_then(Value::as_str) != Some("image") {
        return None;
    }
    let source = o.get("source")?.as_object()?;
    if source.get("type").and_then(Value::as_str) != Some("base64") {
        return None;
    }
    let data = source.get("data")?.as_str()?.to_owned();
    Some(ImageOutput {
        data,
        mime_type: source
            .get("media_type")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// `splitClaudeToolResultImages`: images become a `[image]` text block.
pub fn split_tool_result_images(content: Option<&Value>) -> (Vec<ImageOutput>, Option<Value>) {
    let Some(Value::Array(blocks)) = content else {
        return (Vec::new(), content.cloned());
    };
    let mut images = Vec::new();
    let text: Vec<Value> = blocks
        .iter()
        .map(|b| match base64_image_output(b) {
            Some(img) => {
                images.push(img);
                serde_json::json!({"type": "text", "text": "[image]"})
            }
            None => b.clone(),
        })
        .collect();
    (images, Some(Value::Array(text)))
}

/// Re-types numbers as JS would after `JSON.parse`: integral doubles become integers, integers
/// beyond 2^53 round through f64.
pub fn normalize_js_numbers(v: Value) -> Value {
    match v {
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if n.is_f64() || f.abs() > 9_007_199_254_740_991.0 {
                    return Value::Number(paseo_tool_detail::num_from_f64(f));
                }
            }
            Value::Number(n)
        }
        Value::Array(a) => Value::Array(a.into_iter().map(normalize_js_numbers).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, v)| (k, normalize_js_numbers(v)))
                .collect(),
        ),
        other => other,
    }
}

/// UTF-16 `slice(start)`.
pub fn utf16_slice_from(s: &str, start: usize) -> String {
    let units = paseo_agent_types::js_str::utf16_units(s);
    if start >= units.len() {
        return String::new();
    }
    units_to_string(&units[start..])
}

fn units_to_string(units: &[u16]) -> String {
    let mut out = String::new();
    for r in char::decode_utf16(units.iter().copied()) {
        match r {
            Ok(c) => out.push(c),
            Err(e) => {
                out.push(
                    paseo_agent_types::js_str::lone_high_surrogate(e.unpaired_surrogate())
                        .unwrap_or('\u{fffd}'),
                );
            }
        }
    }
    out
}

pub fn utf16_len(s: &str) -> usize {
    paseo_agent_types::js_str::utf16_len(s)
}
