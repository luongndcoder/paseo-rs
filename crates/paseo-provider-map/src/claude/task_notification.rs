//! Background task notification tool calls (`claude/task-notification-tool-call.ts`).

use regex::Regex;
use serde_json::{json, Map, Value};

use paseo_tool_detail::js::js_trim;

const MARKER: &str = "<task-notification>";

/// Parsed notification envelope.
#[derive(Debug, Default, Clone)]
pub struct Envelope {
    message_id: Option<String>,
    task_id: Option<String>,
    tool_use_id: Option<String>,
    status: Option<String>,
    summary: Option<String>,
    output_file: Option<String>,
    raw_text: Option<String>,
}

fn non_empty(v: Option<&Value>) -> Option<String> {
    let t = js_trim(v?.as_str()?);
    (!t.is_empty()).then(|| t.to_owned())
}

fn non_empty_str(s: &str) -> Option<String> {
    let t = js_trim(s);
    (!t.is_empty()).then(|| t.to_owned())
}

/// `extractUserContentText`; `None` when the zod union rejects the content or no parts remain.
fn extract_user_content_text(content: &Value) -> Option<String> {
    let mut parts = Vec::new();
    match content {
        Value::String(s) => parts.extend(non_empty_str(s)),
        Value::Array(blocks) => {
            for block in blocks {
                let o = block.as_object()?;
                for key in ["text", "input"] {
                    if let Some(v) = o.get(key) {
                        if !v.is_string() {
                            return None;
                        }
                    }
                }
                parts.extend(non_empty(o.get("text")));
                parts.extend(non_empty(o.get("input")));
            }
        }
        _ => return None,
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn tag_value(text: &str, tag: &str) -> Option<String> {
    let pattern = format!(r"(?is)<{t}>\s*(.*?)\s*</{t}>", t = regex::escape(tag));
    let re = Regex::new(&pattern).ok()?;
    non_empty_str(re.captures(text)?.get(1)?.as_str())
}

fn tag_either(text: &str, a: &str, b: &str) -> Option<String> {
    tag_value(text, a).or_else(|| tag_value(text, b))
}

/// `isTaskNotificationUserContent`.
pub fn is_task_notification_user_content(content: &Value) -> bool {
    extract_user_content_text(content).is_some_and(|t| t.contains(MARKER))
}

fn parse_from_user_content(content: &Value, message_id: Option<&Value>) -> Option<Envelope> {
    if let Some(m) = message_id {
        if !m.is_string() && !m.is_null() {
            return None;
        }
    }
    let raw = extract_user_content_text(content).filter(|t| t.contains(MARKER))?;
    Some(Envelope {
        message_id: non_empty(message_id),
        task_id: tag_value(&raw, "task-id"),
        tool_use_id: tag_either(&raw, "tool-use-id", "tool_use_id"),
        status: tag_value(&raw, "status"),
        summary: tag_value(&raw, "summary"),
        output_file: tag_either(&raw, "output-file", "output_file"),
        raw_text: Some(raw),
    })
}

/// zod `TaskNotificationHistoryRecordSchema`: every named field is an optional string.
fn history_record_valid(r: &Map<String, Value>) -> bool {
    const STRINGS: [&str; 10] = [
        "type",
        "subtype",
        "uuid",
        "message_id",
        "task_id",
        "tool_use_id",
        "status",
        "summary",
        "output_file",
        "content",
    ];
    STRINGS
        .iter()
        .all(|k| r.get(*k).is_none_or(Value::is_string))
        && r.get("message").is_none_or(Value::is_object)
}

fn parse_from_system_record(record: &Value) -> Option<Envelope> {
    let r = record.as_object()?;
    if !history_record_valid(r) {
        return None;
    }
    let ty = r.get("type").and_then(Value::as_str);
    let is_system = ty == Some("system")
        && r.get("subtype").and_then(Value::as_str) == Some("task_notification");
    let is_queued = ty == Some("queue-operation")
        && r.get("content")
            .is_some_and(is_task_notification_user_content);
    if !is_system && !is_queued {
        return None;
    }
    let raw = non_empty(r.get("content"));
    let from_raw = |a: &str, b: Option<&str>| -> Option<String> {
        let raw = raw.as_deref()?;
        match b {
            Some(b) => tag_either(raw, a, b),
            None => tag_value(raw, a),
        }
    };
    Some(Envelope {
        message_id: non_empty(r.get("uuid")).or_else(|| non_empty(r.get("message_id"))),
        task_id: non_empty(r.get("task_id")).or_else(|| from_raw("task-id", None)),
        tool_use_id: non_empty(r.get("tool_use_id"))
            .or_else(|| from_raw("tool-use-id", Some("tool_use_id"))),
        status: non_empty(r.get("status")).or_else(|| from_raw("status", None)),
        summary: non_empty(r.get("summary")).or_else(|| from_raw("summary", None)),
        output_file: non_empty(r.get("output_file"))
            .or_else(|| from_raw("output-file", Some("output_file"))),
        raw_text: raw.clone(),
    })
}

fn call_id_segment(segment: &str) -> Option<String> {
    let mut out = String::new();
    let mut in_run = false;
    for c in js_trim(segment).chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-') {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('_');
            in_run = true;
        }
    }
    (!out.is_empty()).then_some(out)
}

fn build_call_id(e: &Envelope) -> String {
    if let Some(seg) = e.message_id.as_deref().and_then(call_id_segment) {
        return format!("task_notification_{seg}");
    }
    if let Some(seg) = e.task_id.as_deref().and_then(call_id_segment) {
        return format!("task_notification_{seg}");
    }
    let seed = [&e.status, &e.summary, &e.output_file, &e.raw_text]
        .iter()
        .filter_map(|v| v.as_deref())
        .collect::<Vec<_>>()
        .join("|");
    let seed = if seed.is_empty() {
        "task_notification".to_owned()
    } else {
        seed
    };
    let digest = sha1_hex(seed.as_bytes());
    format!("task_notification_{}", &digest[..12])
}

fn to_tool_call(e: &Envelope) -> Value {
    let status_lower = e.status.as_deref().map(str::to_lowercase);
    let (status, error) = match status_lower.as_deref() {
        Some("failed" | "error") => (
            "failed",
            json!({"message": e.summary.clone().unwrap_or_else(|| "Background task failed".into())}),
        ),
        Some("canceled" | "cancelled") => ("canceled", Value::Null),
        _ => ("completed", Value::Null),
    };
    let label = match (&e.summary, &e.status) {
        (Some(s), _) => s.clone(),
        (None, Some(st)) => format!("Background task {}", st.to_lowercase()),
        _ => "Background task notification".into(),
    };
    let detail_text = e.raw_text.clone().or_else(|| e.summary.clone());
    let mut metadata = Map::new();
    metadata.insert("synthetic".into(), json!(true));
    metadata.insert("source".into(), json!("claude_task_notification"));
    for (k, v) in [
        ("taskId", &e.task_id),
        ("toolUseId", &e.tool_use_id),
        ("status", &e.status),
        ("outputFile", &e.output_file),
    ] {
        if let Some(v) = v {
            metadata.insert(k.into(), json!(v));
        }
    }
    let mut detail = Map::new();
    detail.insert("type".into(), json!("plain_text"));
    detail.insert("label".into(), json!(label));
    detail.insert("icon".into(), json!("wrench"));
    if let Some(t) = detail_text.filter(|t| !t.is_empty()) {
        detail.insert("text".into(), json!(t));
    }
    json!({
        "type": "tool_call",
        "callId": build_call_id(e),
        "name": "task_notification",
        "detail": Value::Object(detail),
        "metadata": Value::Object(metadata),
        "status": status,
        "error": error,
    })
}

/// `mapTaskNotificationUserContentToToolCall`.
pub fn map_user_content(content: &Value, message_id: Option<&str>) -> Option<Value> {
    let id = message_id.map(|m| Value::String(m.to_owned()));
    parse_from_user_content(content, id.as_ref()).map(|e| to_tool_call(&e))
}

/// `mapTaskNotificationSystemRecordToToolCall`.
pub fn map_system_record(record: &Value) -> Option<Value> {
    parse_from_system_record(record).map(|e| to_tool_call(&e))
}

// ---- SHA-1 (only for the notification call-id fallback) ----------------------------------------

fn sha1_hex(data: &[u8]) -> String {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h = [
            h[0].wrapping_add(a),
            h[1].wrapping_add(b),
            h[2].wrapping_add(c),
            h[3].wrapping_add(d),
            h[4].wrapping_add(e),
        ];
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_known_digests() {
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn user_content_notification_maps_to_a_completed_call() {
        let content = Value::String(
            "<task-notification><task-id> t1 </task-id><status>completed</status><summary>Done</summary></task-notification>".into(),
        );
        let item = map_user_content(&content, Some("u-1")).unwrap();
        assert_eq!(item["callId"], "task_notification_u-1");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["detail"]["label"], "Done");
        assert_eq!(item["metadata"]["taskId"], "t1");
    }
}
