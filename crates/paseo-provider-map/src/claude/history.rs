//! History conversion (`convertClaudeHistoryEntry`, `ingestPersistedHistory`): the persisted
//! transcript is replayed through the same mapper state as the live stream.
//!
//! Sidechain (subagent) transcripts are not replayed; see `DEVIATION(claude-subagent-frames-unmapped)`.

use serde_json::{json, Map, Value};

use super::images::is_provider_image_markdown;
use super::text::{
    extract_user_message_text, is_transcript_noise_content, js_truthy, obj_get, str_of,
};
use super::{task_notification, ClaudeMapper};

/// One replayed timeline item with its transcript timestamp.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEntry {
    pub item: Value,
    pub timestamp: Option<String>,
}

fn is_synthetic_user_entry(entry: &Value) -> bool {
    entry.as_object().is_some_and(|o| {
        o.get("isSynthetic") == Some(&Value::Bool(true))
            || o.get("isMeta") == Some(&Value::Bool(true))
            || o.get("toolUseResult").is_some_and(js_truthy)
    })
}

fn is_tool_result_user_entry(entry: &Value) -> bool {
    obj_get(entry, "message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .is_some_and(|c| {
            c.iter()
                .any(|b| str_of(b.get("type")) == Some("tool_result"))
        })
}

fn is_synthetic_history_user_entry(entry: &Value) -> bool {
    is_synthetic_user_entry(entry) && !is_tool_result_user_entry(entry)
}

fn normalize_history_blocks(content: &Value) -> Option<Vec<Value>> {
    let is_chunk = |v: &Value| {
        v.as_object()
            .is_some_and(|o| o.get("type").is_some_and(Value::is_string))
    };
    match content {
        Value::Array(a) => {
            let blocks: Vec<Value> = a.iter().filter(|e| is_chunk(e)).cloned().collect();
            (!blocks.is_empty()).then_some(blocks)
        }
        other if is_chunk(other) => Some(vec![other.clone()]),
        _ => None,
    }
}

fn has_tool_like_block(block: &Value) -> bool {
    block
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.to_lowercase().contains("tool"))
}

/// `normalizeProviderReplayTimestamp`.
pub(crate) fn normalize_replay_timestamp(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => {
            let t = paseo_tool_detail::js::js_trim(s);
            (!t.is_empty() && parse_iso_timestamp(t).is_some()).then(|| t.to_owned())
        }
        Value::Number(n) => {
            let f = n.as_f64().filter(|f| f.is_finite())?;
            let ms = if f > 1_000_000_000_000.0 {
                f
            } else {
                f * 1000.0
            };
            iso_from_epoch_ms(ms)
        }
        _ => None,
    }
}

/// Accepts the ISO-8601 forms `Date.parse` takes without locale heuristics:
/// `YYYY[-MM[-DD]][THH:mm[:ss[.sss]][Z|±HH:mm]]`.
fn parse_iso_timestamp(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let digits = |from: usize, n: usize| -> Option<u32> {
        let slice = b.get(from..from + n)?;
        slice.iter().all(u8::is_ascii_digit).then(|| {
            slice
                .iter()
                .fold(0u32, |acc, d| acc * 10 + u32::from(d - b'0'))
        })
    };
    let year = digits(0, 4)? as i64;
    let (mut month, mut day) = (1u32, 1u32);
    let mut i = 4;
    if b.get(i) == Some(&b'-') {
        month = digits(i + 1, 2)?;
        i += 3;
        if b.get(i) == Some(&b'-') {
            day = digits(i + 1, 2)?;
            i += 3;
        }
    }
    if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    let (mut hour, mut minute, mut second, mut millis) = (0u32, 0u32, 0u32, 0u32);
    let mut offset_minutes = 0i64;
    if matches!(b.get(i), Some(b'T' | b't' | b' ')) {
        hour = digits(i + 1, 2)?;
        if b.get(i + 3) != Some(&b':') {
            return None;
        }
        minute = digits(i + 4, 2)?;
        i += 6;
        if b.get(i) == Some(&b':') {
            second = digits(i + 1, 2)?;
            i += 3;
            if b.get(i) == Some(&b'.') {
                let start = i + 1;
                let mut end = start;
                while b.get(end).is_some_and(u8::is_ascii_digit) {
                    end += 1;
                }
                if end == start {
                    return None;
                }
                let frac = &s[start..end];
                let three: String = frac.chars().chain("00".chars()).take(3).collect();
                millis = three.parse().ok()?;
                i = end;
            }
        }
        match b.get(i) {
            Some(b'Z' | b'z') => i += 1,
            Some(b'+' | b'-') => {
                let sign = if b[i] == b'+' { 1 } else { -1 };
                let oh = digits(i + 1, 2)?;
                let (om, next) = if b.get(i + 3) == Some(&b':') {
                    (digits(i + 4, 2)?, i + 6)
                } else {
                    (digits(i + 3, 2)?, i + 5)
                };
                offset_minutes = sign * (i64::from(oh) * 60 + i64::from(om));
                i = next;
            }
            _ => {}
        }
        if hour > 24
            || minute > 59
            || second > 59
            || (hour == 24 && (minute | second | millis) != 0)
        {
            return None;
        }
    }
    if i != b.len() {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second)
        - offset_minutes * 60;
    Some(secs as f64 * 1000.0 + f64::from(millis))
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        _ => 28,
    }
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `new Date(ms).toISOString()`; `None` outside the JS Date range.
fn iso_from_epoch_ms(ms: f64) -> Option<String> {
    if !ms.is_finite() || ms.abs() > 8.64e15 {
        return None;
    }
    let ms = ms.trunc() as i64;
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let year = if (0..=9999).contains(&y) {
        format!("{y:04}")
    } else if y < 0 {
        format!("-{:06}", -y)
    } else {
        format!("+{y:06}")
    };
    Some(format!(
        "{year}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        (rem / 60_000) % 60,
        (rem / 1000) % 60,
        rem % 1000
    ))
}

impl ClaudeMapper {
    /// `convertClaudeHistoryEntry` over one transcript record.
    pub fn convert_history_entry(&mut self, entry: &Value) -> Vec<Value> {
        let ty = str_of(entry.get("type"));
        if ty == Some("system") && str_of(entry.get("subtype")) == Some("compact_boundary") {
            let meta = super::compaction_metadata(entry);
            let mut item = Map::new();
            item.insert("type".into(), json!("compaction"));
            item.insert("status".into(), json!("completed"));
            item.insert(
                "trigger".into(),
                json!(if meta.trigger.as_deref() == Some("manual") {
                    "manual"
                } else {
                    "auto"
                }),
            );
            if let Some(pre) = meta.pre_tokens {
                item.insert(
                    "preTokens".into(),
                    Value::Number(paseo_tool_detail::num_from_f64(pre)),
                );
            }
            return vec![Value::Object(item)];
        }
        if let Some(item) = task_notification::map_system_record(entry) {
            return vec![item];
        }
        if entry.get("isCompactSummary").is_some_and(js_truthy) {
            return Vec::new();
        }
        if ty == Some("user") && is_synthetic_history_user_entry(entry) {
            return Vec::new();
        }
        let Some(content) = obj_get(entry, "message").and_then(|m| m.get("content")) else {
            return Vec::new();
        };
        if matches!(ty, Some("user" | "assistant")) && is_transcript_noise_content(content) {
            return Vec::new();
        }

        let normalized_blocks = normalize_history_blocks(content);
        let has_tool_block = normalized_blocks
            .as_ref()
            .is_some_and(|b| b.iter().any(has_tool_like_block));
        let uuid = entry
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let user_message_id = (ty == Some("user")).then(|| uuid.clone()).flatten();

        if ty == Some("user") {
            if let Some(item) =
                task_notification::map_user_content(content, user_message_id.as_deref())
            {
                return vec![item];
            }
        }

        let mut timeline = Vec::new();
        if ty == Some("user") {
            if let Some(text) = extract_user_message_text(content) {
                let mut item = Map::new();
                item.insert("type".into(), json!("user_message"));
                item.insert("text".into(), json!(text));
                if let Some(id) = &user_message_id {
                    item.insert("messageId".into(), json!(id));
                }
                timeline.push(Value::Object(item));
            }
        }

        if has_tool_block {
            if let Some(blocks) = normalized_blocks {
                let mapped = self.map_blocks_to_timeline(
                    &Value::Array(blocks),
                    super::blocks::BlockOpts::assistant(false, false),
                );
                if ty == Some("user") {
                    let tool_items = mapped.into_iter().filter(|item| {
                        str_of(item.get("type")) == Some("tool_call")
                            || (str_of(item.get("type")) == Some("assistant_message")
                                && item
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .is_some_and(is_provider_image_markdown))
                    });
                    timeline.extend(tool_items);
                    return timeline;
                }
                return mapped;
            }
        }

        if ty == Some("assistant") {
            let content_value = match content {
                Value::String(s) if !s.is_empty() => Some(content.clone()),
                Value::String(_) => None,
                _ => normalized_blocks_value(content),
            };
            if let Some(cv) = content_value {
                let mut items = self
                    .map_blocks_to_timeline(&cv, super::blocks::BlockOpts::assistant(false, false));
                if let Some(id) = &uuid {
                    for item in &mut items {
                        if str_of(item.get("type")) == Some("assistant_message")
                            && item.get("messageId").is_none_or(|m| !js_truthy(m))
                        {
                            if let Some(o) = item.as_object_mut() {
                                o.insert("messageId".into(), json!(id));
                            }
                        }
                    }
                }
                return items;
            }
        }
        timeline
    }

    /// `ingestPersistedHistory` for a transcript's text: one timeline entry per replayed item
    /// (task snapshots included), in order. Sidechain records are skipped.
    pub fn ingest_history(&mut self, content: &str) -> Vec<HistoryEntry> {
        self.task_state.reset();
        let mut timeline = Vec::new();
        for line in content.split(['\n']) {
            let trimmed = paseo_tool_detail::js::js_trim(line.trim_end_matches('\r'));
            if trimmed.is_empty() {
                continue;
            }
            let Ok(entry) = paseo_agent_types::js_str::parse_js_json(trimmed) else {
                continue;
            };
            if !entry.is_object() || entry.get("isSidechain").is_some_and(js_truthy) {
                continue;
            }
            let timestamp = normalize_replay_timestamp(entry.get("timestamp"));
            let mut items = Vec::new();
            if let Some(snapshot) = self.task_state.observe(&entry) {
                items.push(snapshot);
            }
            items.extend(self.convert_history_entry(&entry));
            for item in items {
                timeline.push(HistoryEntry {
                    item,
                    timestamp: timestamp.clone(),
                });
            }
        }
        timeline
    }
}

/// `contentValue` for non-string content: `normalizeHistoryBlocks` as an array value.
fn normalized_blocks_value(content: &Value) -> Option<Value> {
    normalize_history_blocks(content).map(Value::Array)
}
