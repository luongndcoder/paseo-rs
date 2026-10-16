//! Thread-item -> timeline conversion shared by live notifications and persisted history
//! (`threadItemToTimeline`, `loadCodexThreadHistoryTimeline` and friends in
//! `codex-app-server-agent.ts`, plus `provider-image-output.ts` and
//! `provider-history-timestamps.ts`).

use paseo_agent_types::{AgentTimelineItem, Map, ToolCallDetail, ToolCallItem, ToolCallStatus};
use paseo_tool_detail::js::js_trim;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;

use super::questions::async_question_to_timeline;
use super::tool_call::{
    as_record, map_codex_tool_call_envelope, map_codex_tool_call_from_thread_item,
    split_codex_mcp_tool_result_images, EnvelopeParams,
};

pub const CONTEXT_COMPACTION_TYPE: &str = "contextCompaction";

/// `normalizeCodexThreadItemType`.
pub fn normalize_thread_item_type(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    Some(
        match raw {
            "UserMessage" => "userMessage",
            "AgentMessage" => "agentMessage",
            "Reasoning" => "reasoning",
            "Plan" => "plan",
            "CommandExecution" => "commandExecution",
            "FileChange" => "fileChange",
            "McpToolCall" => "mcpToolCall",
            "WebSearch" => "webSearch",
            "CollabAgentToolCall" => "collabAgentToolCall",
            "SubAgentActivity" => "subAgentActivity",
            "ImageView" => "imageView",
            "ImageGeneration" => "imageGeneration",
            other => other,
        }
        .to_string(),
    )
}

pub fn item_type(item: &Value) -> Option<String> {
    normalize_thread_item_type(as_record(item)?.get("type")?.as_str())
}

fn is_tool_thread_item_type(t: &str) -> bool {
    matches!(
        t,
        "commandExecution"
            | "fileChange"
            | "mcpToolCall"
            | "webSearch"
            | "collabAgentToolCall"
            | "subAgentActivity"
    )
}

fn non_empty_string(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// plan helpers
// ---------------------------------------------------------------------------

fn trim_end_ws(s: &str) -> &str {
    s.trim_end_matches(paseo_tool_detail::js::is_js_whitespace)
}

/// `normalizePlanMarkdown`.
pub fn normalize_plan_markdown(text: &str) -> String {
    let joined = text
        .split('\n')
        .map(trim_end_ws)
        .collect::<Vec<_>>()
        .join("\n");
    js_trim(&joined).to_string()
}

fn re_plan_bullet() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(#{1,6}\s|[-*+]\s|\d+\.\s)").expect("plan bullet regex"))
}

/// `planStepsToMarkdown`.
pub fn plan_steps_to_markdown(steps: &[(String, String)]) -> String {
    let lines: Vec<String> = steps
        .iter()
        .map(|(step, _)| js_trim(step))
        .filter(|s| !s.is_empty())
        .map(|s| {
            if re_plan_bullet().is_match(s) {
                s.to_string()
            } else {
                format!("- {s}")
            }
        })
        .collect();
    normalize_plan_markdown(&lines.join("\n"))
}

/// `mapCodexPlanToToolCall`.
pub fn map_codex_plan_to_tool_call(call_id: &str, text: &str) -> Option<ToolCallItem> {
    let text = normalize_plan_markdown(text);
    if text.is_empty() {
        return None;
    }
    Some(ToolCallItem::new(
        call_id,
        "plan",
        ToolCallStatus::Completed,
        ToolCallDetail::Plan { text },
    ))
}

fn normalize_task_status(status: Option<&str>) -> paseo_agent_types::TaskStatus {
    use paseo_agent_types::TaskStatus;
    match status {
        Some("completed") => TaskStatus::Completed,
        Some("inProgress" | "in_progress") => TaskStatus::InProgress,
        _ => TaskStatus::Pending,
    }
}

/// `mapCodexPlanUpdateToTodo`.
pub fn map_codex_plan_update_to_todo(
    steps: &[(Option<String>, Option<String>)],
) -> AgentTimelineItem {
    let mut items = Vec::new();
    for (index, (step, status)) in steps.iter().enumerate() {
        let text = step.as_deref().map(js_trim).unwrap_or("");
        if text.is_empty() {
            continue;
        }
        let status = normalize_task_status(status.as_deref());
        items.push(paseo_agent_types::AgentTaskItem {
            text: text.to_string(),
            completed: status == paseo_agent_types::TaskStatus::Completed,
            id: Some(index.to_string()),
            status: Some(status),
            active_form: None,
        });
    }
    AgentTimelineItem::Todo { items }
}

// ---------------------------------------------------------------------------
// image output
// ---------------------------------------------------------------------------

/// Materializes base64 image data to a file and returns its path. Injected because the mapper
/// itself does no IO.
///
/// DEVIATION(codex-image-materialize-injected): TS writes a content-hashed file under a private
/// temp dir inside the mapper. Rust leaves that to the caller; without a materializer a `data`
/// image renders the TS failure text ("Image output was omitted ...").
pub type ImageMaterializer = dyn Fn(&str, Option<&str>) -> Option<String> + Send + Sync;

#[derive(Default, Clone)]
pub struct ProviderImage {
    pub path: Option<String>,
    pub url: Option<String>,
    pub data: Option<String>,
    pub mime_type: Option<String>,
}

fn nonblank_opt(v: Option<&str>) -> Option<String> {
    v.map(js_trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn is_data_image_source(s: &str) -> bool {
    js_trim(s).to_lowercase().starts_with("data:image/")
}

fn escape_alt(v: &str) -> String {
    v.replace('\\', "\\\\").replace(']', "\\]")
}

/// `encodeURIComponent`.
fn encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn encode_file_path(v: &str) -> String {
    v.split('/')
        .map(encode_uri_component)
        .collect::<Vec<_>>()
        .join("/")
}

fn windows_file_uri(value: &str) -> Option<String> {
    let is_network = value.starts_with("\\\\");
    let mut n = value.replace('\\', "/");
    let lower = n.to_lowercase();
    if lower.starts_with("//?/unc/") {
        n = format!("//{}", &n[8..]);
    } else if n.len() >= 7
        && n.starts_with("//?/")
        && n.as_bytes()[4].is_ascii_alphabetic()
        && n.as_bytes()[5] == b':'
        && n.as_bytes()[6] == b'/'
    {
        n = n[4..].to_string();
    }
    let b = n.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/' {
        let drive = &n[..2];
        return Some(format!("file:///{drive}{}", encode_file_path(&n[2..])));
    }
    if is_network && n.starts_with("//") {
        return Some(format!("file:{}", encode_file_path(&n)));
    }
    None
}

fn markdown_image_source(value: &str) -> String {
    if let Some(u) = windows_file_uri(value) {
        return u;
    }
    if value.starts_with('/') {
        return format!("file://{}", encode_file_path(value));
    }
    value.to_string()
}

fn escape_source(value: &str) -> String {
    markdown_image_source(value)
        .replace('\\', "\\\\")
        .replace(')', "\\)")
}

/// `renderProviderImageOutputAsAssistantMarkdown`.
pub fn render_provider_image(
    image: &ProviderImage,
    materialize: Option<&ImageMaterializer>,
) -> Option<AgentTimelineItem> {
    let source = nonblank_opt(image.path.as_deref()).or_else(|| nonblank_opt(image.url.as_deref()));
    if let Some(src) = source.as_deref().filter(|s| !is_data_image_source(s)) {
        return Some(AgentTimelineItem::AssistantMessage {
            text: format!("![{}]({})", escape_alt("Image"), escape_source(src)),
            message_id: None,
        });
    }
    let data = nonblank_opt(image.data.as_deref()).or_else(|| {
        source
            .as_deref()
            .filter(|s| is_data_image_source(s))
            .map(str::to_string)
    })?;
    let mime = nonblank_opt(image.mime_type.as_deref());
    let materialized = materialize.and_then(|f| f(&data, mime.as_deref()));
    match materialized {
        Some(p) if !p.is_empty() && !is_data_image_source(&p) => {
            Some(AgentTimelineItem::AssistantMessage {
                text: format!("![{}]({})", escape_alt("Image"), escape_source(&p)),
                message_id: None,
            })
        }
        _ => Some(AgentTimelineItem::AssistantMessage {
            text: "Image output was omitted because it was not available as a file path or URL."
                .into(),
            message_id: None,
        }),
    }
}

fn first_string_field(r: &Map, fields: &[&str]) -> Option<String> {
    fields
        .iter()
        .find_map(|f| r.get(*f).and_then(Value::as_str))
        .map(str::to_string)
}

fn re_base64() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9+/]+={0,2}$").expect("base64 regex"))
}

fn image_output_from_result(result: &Value) -> Option<ProviderImage> {
    if let Value::String(s) = result {
        let t = js_trim(s);
        let is_data = t.to_lowercase().starts_with("data:image/")
            || (re_base64().is_match(t) && paseo_tool_detail::js::utf16_len(t) > 64);
        return Some(if is_data {
            ProviderImage {
                data: Some(t.to_string()),
                ..Default::default()
            }
        } else {
            ProviderImage {
                url: Some(t.to_string()),
                ..Default::default()
            }
        });
    }
    let r = as_record(result)?;
    Some(ProviderImage {
        path: first_string_field(r, &["path", "savedPath", "saved_path"]),
        url: first_string_field(r, &["url"]),
        data: first_string_field(r, &["data"]),
        mime_type: first_string_field(r, &["mimeType", "mime_type"]),
    })
}

fn map_thread_image_item(
    kind: &str,
    item: &Map,
    materialize: Option<&ImageMaterializer>,
) -> Option<AgentTimelineItem> {
    if kind == "imageView" {
        return render_provider_image(
            &ProviderImage {
                path: first_string_field(item, &["path"]),
                ..Default::default()
            },
            None,
        );
    }
    if item.get("status").and_then(Value::as_str) == Some("failed") {
        let prompt = first_string_field(item, &["revisedPrompt", "revised_prompt"])
            .map(Value::String)
            .unwrap_or(Value::Null);
        let error = match item.get("failure") {
            Some(v) if !v.is_null() => v.clone(),
            _ => json!({"message": "Image generation failed"}),
        };
        return map_codex_tool_call_envelope(&EnvelopeParams {
            call_id: first_string_field(item, &["id"]),
            name: "image_generation".into(),
            input: json!({ "prompt": prompt }),
            output: Value::Null,
            error,
            cwd: None,
        })
        .map(AgentTimelineItem::tool_call);
    }
    let saved = first_string_field(item, &["savedPath", "saved_path"]);
    let result = image_output_from_result(item.get("result").unwrap_or(&Value::Null));
    render_provider_image(
        &ProviderImage {
            path: saved.or_else(|| result.as_ref().and_then(|r| r.path.clone())),
            url: result.as_ref().and_then(|r| r.url.clone()),
            data: result.as_ref().and_then(|r| r.data.clone()),
            mime_type: result.as_ref().and_then(|r| r.mime_type.clone()),
        },
        materialize,
    )
}

/// `mcpToolResultImagesToTimeline`.
pub fn mcp_tool_result_images_to_timeline(
    item: &Value,
    materialize: Option<&ImageMaterializer>,
) -> Vec<AgentTimelineItem> {
    if item_type(item).as_deref() != Some("mcpToolCall") {
        return Vec::new();
    }
    let Some(rec) = as_record(item) else {
        return Vec::new();
    };
    let (images, _) = split_codex_mcp_tool_result_images(rec.get("result").unwrap_or(&Value::Null));
    images
        .into_iter()
        .filter_map(|(data, mime)| {
            render_provider_image(
                &ProviderImage {
                    data: Some(data),
                    mime_type: Some(mime),
                    ..Default::default()
                },
                materialize,
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// thread item -> timeline
// ---------------------------------------------------------------------------

/// `extractUserText`.
pub fn extract_user_text(content: Option<&Value>) -> Option<String> {
    let Some(Value::Array(a)) = content else {
        return None;
    };
    let parts: Vec<&str> = a
        .iter()
        .filter_map(|i| {
            let r = as_record(i)?;
            if r.get("type").and_then(Value::as_str) == Some("text") {
                r.get("text").and_then(Value::as_str)
            } else {
                None
            }
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn join_lines(v: Option<&Value>) -> String {
    match v {
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::Null => String::new(),
                Value::String(s) => s.clone(),
                other => paseo_tool_detail::js::js_to_string(other),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn map_user_message_item(item: &Map, include_user_message: bool) -> Option<AgentTimelineItem> {
    if !include_user_message {
        return None;
    }
    let text = extract_user_text(item.get("content")).unwrap_or_default();
    let message_id = non_empty_string(item.get("id"));
    let client_message_id = non_empty_string(item.get("clientId"))
        .or_else(|| non_empty_string(item.get("client_id")))
        .or_else(|| non_empty_string(item.get("clientUserMessageId")));
    Some(AgentTimelineItem::UserMessage {
        text,
        message_id,
        client_message_id,
    })
}

fn map_agent_message(item: &Map) -> AgentTimelineItem {
    if let Some(q) = async_question_to_timeline(&Value::Object(item.clone())) {
        return AgentTimelineItem::tool_call(q);
    }
    AgentTimelineItem::AssistantMessage {
        text: item
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        message_id: non_empty_string(item.get("id")),
    }
}

pub struct ThreadItemOptions<'a> {
    pub include_user_message: bool,
    pub cwd: Option<&'a str>,
    pub materialize: Option<&'a ImageMaterializer>,
}

/// `threadItemToTimeline`.
pub fn thread_item_to_timeline(
    item: &Value,
    opts: &ThreadItemOptions,
) -> Option<AgentTimelineItem> {
    let rec = as_record(item)?;
    let kind = normalize_thread_item_type(rec.get("type").and_then(Value::as_str));
    let normalized: Value = match (&kind, rec.get("type").and_then(Value::as_str)) {
        (Some(k), raw) if Some(k.as_str()) != raw => {
            let mut m = rec.clone();
            m.insert("type".into(), Value::String(k.clone()));
            Value::Object(m)
        }
        _ => item.clone(),
    };
    let nrec = as_record(&normalized)?;
    let kind_str = kind.as_deref();
    if matches!(kind_str, Some("imageView" | "imageGeneration")) {
        return map_thread_image_item(kind_str.unwrap_or_default(), nrec, opts.materialize);
    }
    if kind_str.is_some_and(is_tool_thread_item_type) {
        return map_codex_tool_call_from_thread_item(&normalized, opts.cwd)
            .map(AgentTimelineItem::tool_call);
    }
    match kind_str {
        Some("userMessage") => map_user_message_item(nrec, opts.include_user_message),
        Some("agentMessage") => Some(map_agent_message(nrec)),
        Some("plan") => {
            let text = nrec.get("text").and_then(Value::as_str).unwrap_or("");
            let call_id = non_empty_string(nrec.get("id").or_else(|| nrec.get("itemId")))
                .unwrap_or_else(|| format!("plan:{}", normalize_plan_markdown(text)));
            map_codex_plan_to_tool_call(&call_id, text).map(AgentTimelineItem::tool_call)
        }
        Some("reasoning") => {
            let summary = join_lines(nrec.get("summary"));
            let content = join_lines(nrec.get("content"));
            let text = if !summary.is_empty() {
                summary
            } else {
                content
            };
            (!text.is_empty()).then_some(AgentTimelineItem::Reasoning { text })
        }
        Some(CONTEXT_COMPACTION_TYPE) => Some(AgentTimelineItem::Compaction {
            status: paseo_agent_types::CompactionStatus::Completed,
            trigger: None,
            pre_tokens: None,
            extra: Map::new(),
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// sub-agent activity helpers (history + live)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentKind {
    Started,
    Interacted,
    Interrupted,
    Completed,
}

#[derive(Debug, Clone)]
pub struct SubAgentActivity {
    pub id: Option<String>,
    pub agent_thread_id: String,
    pub kind: SubAgentKind,
}

/// `readCodexSubAgentActivity`.
pub fn read_sub_agent_activity(item: &Value) -> Option<SubAgentActivity> {
    let rec = as_record(item)?;
    if item_type(item).as_deref() != Some("subAgentActivity") {
        return None;
    }
    let thread = rec.get("agentThreadId")?.as_str()?.to_string();
    let kind = match rec.get("kind")?.as_str()? {
        "started" => SubAgentKind::Started,
        "interacted" => SubAgentKind::Interacted,
        "interrupted" => SubAgentKind::Interrupted,
        "completed" => SubAgentKind::Completed,
        _ => return None,
    };
    Some(SubAgentActivity {
        id: non_empty_string(rec.get("id")),
        agent_thread_id: thread,
        kind,
    })
}

/// `settleHistoricalSubAgentActivity`.
pub fn settle_historical_sub_agent_activity(
    mut item: ToolCallItem,
    kind: SubAgentKind,
) -> ToolCallItem {
    item.status = if kind == SubAgentKind::Interrupted {
        ToolCallStatus::Canceled
    } else {
        ToolCallStatus::Completed
    };
    item.error = Some(Value::Null);
    item
}

fn read_historical_sub_agent_thread_ids(item: &Value) -> Vec<String> {
    if let Some(a) = read_sub_agent_activity(item) {
        return if a.kind == SubAgentKind::Started {
            vec![a.agent_thread_id]
        } else {
            Vec::new()
        };
    }
    let Some(rec) = as_record(item) else {
        return Vec::new();
    };
    if item_type(item).as_deref() != Some("collabAgentToolCall")
        || rec.get("tool").and_then(Value::as_str) != Some("spawnAgent")
    {
        return Vec::new();
    }
    match rec.get("receiverThreadIds") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// replay timestamps
// ---------------------------------------------------------------------------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `new Date(ms).toISOString()` for years 0..=9999.
pub fn iso_from_ms(ms: f64) -> Option<String> {
    if !ms.is_finite() || ms.abs() > 8.64e15 {
        return None;
    }
    let ms_i = ms.trunc() as i64;
    let days = ms_i.div_euclid(86_400_000);
    let rem = ms_i.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    if !(0..=9999).contains(&y) {
        return None;
    }
    Some(format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        (rem / 60_000) % 60,
        (rem / 1000) % 60,
        rem % 1000
    ))
}

fn re_iso() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^(\d{4})-(\d{2})(?:-(\d{2}))?(?:T(\d{2}):(\d{2})(?::(\d{2})(?:\.\d{1,9})?)?(?:Z|[+-]\d{2}:\d{2})?)?$",
        )
        .expect("iso regex")
    })
}

/// DEVIATION(codex-history-timestamp-iso-only): `Date.parse` accepts many legacy formats; the
/// Rust check accepts the ISO-8601 subset only. Codex history carries numeric epoch seconds, so
/// the legacy formats are not expected.
fn parses_as_date(s: &str) -> bool {
    let Some(c) = re_iso().captures(s) else {
        return false;
    };
    let num = |i: usize| c.get(i).map(|m| m.as_str().parse::<i64>().unwrap_or(0));
    let (y, mo) = (num(1).unwrap_or(0), num(2).unwrap_or(0));
    let d = num(3).unwrap_or(1);
    if !(1..=12).contains(&mo) || d < 1 {
        return false;
    }
    let (ny, nm) = if mo == 12 { (y + 1, 1) } else { (y, mo + 1) };
    let dim = days_from_civil(ny, nm, 1) - days_from_civil(y, mo, 1);
    if d > dim {
        return false;
    }
    let h = num(4).unwrap_or(0);
    let mi = num(5).unwrap_or(0);
    let se = num(6).unwrap_or(0);
    (h < 24 || (h == 24 && mi == 0 && se == 0)) && mi < 60 && se < 60
}

/// `normalizeProviderReplayTimestamp`.
pub fn normalize_replay_timestamp(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => {
            let t = js_trim(s);
            (!t.is_empty() && parses_as_date(t)).then(|| t.to_string())
        }
        Value::Number(n) => {
            let f = n.as_f64()?;
            let ms = if f > 1_000_000_000_000.0 {
                f
            } else {
                f * 1000.0
            };
            iso_from_ms(ms)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PersistedTimelineEntry {
    pub item: AgentTimelineItem,
    pub timestamp: Option<String>,
    pub provider_turn_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PersistedSubAgentRoute {
    pub child_thread_id: String,
    pub tool_call: ToolCallItem,
}

#[derive(Debug, Clone, Default)]
pub struct HistoryProjection {
    pub timeline: Vec<PersistedTimelineEntry>,
    pub sub_agent_routes: Vec<PersistedSubAgentRoute>,
}

fn history_item_timestamp(item: &Value) -> Option<String> {
    let r = as_record(item)?;
    normalize_replay_timestamp(r.get("timestamp"))
        .or_else(|| normalize_replay_timestamp(r.get("createdAt")))
        .or_else(|| normalize_replay_timestamp(r.get("created_at")))
}

fn turn_history_timestamp(turn: &Map, item: &AgentTimelineItem) -> Option<String> {
    let started = normalize_replay_timestamp(turn.get("startedAt"))
        .or_else(|| normalize_replay_timestamp(turn.get("started_at")));
    let completed = normalize_replay_timestamp(turn.get("completedAt"))
        .or_else(|| normalize_replay_timestamp(turn.get("completed_at")));
    if matches!(item, AgentTimelineItem::UserMessage { .. }) {
        started.or(completed)
    } else {
        completed.or(started)
    }
}

fn update_historical_sub_agent_activity(
    timeline: &mut [PersistedTimelineEntry],
    index: usize,
    kind: SubAgentKind,
    sub_agent_type: Option<String>,
) {
    let AgentTimelineItem::ToolCall(existing) = &timeline[index].item else {
        return;
    };
    let mut settled = settle_historical_sub_agent_activity((**existing).clone(), kind);
    if let (Some(t), ToolCallDetail::SubAgent { sub_agent_type, .. }) = (
        sub_agent_type.filter(|t| !t.is_empty()),
        &mut settled.detail,
    ) {
        *sub_agent_type = Some(t);
    }
    timeline[index].item = AgentTimelineItem::tool_call(settled);
}

/// `threadItemToTimelineEntries`: the item plus the images of an MCP tool result.
fn thread_item_to_timeline_entries(
    item: &Value,
    cwd: Option<&str>,
    materialize: Option<&ImageMaterializer>,
) -> Vec<AgentTimelineItem> {
    let opts = ThreadItemOptions {
        include_user_message: true,
        cwd,
        materialize,
    };
    let Some(first) = thread_item_to_timeline(item, &opts) else {
        return Vec::new();
    };
    let mut out = vec![first];
    out.extend(mcp_tool_result_images_to_timeline(item, materialize));
    out
}

/// `loadCodexThreadHistoryTimeline` over an already-fetched `thread/read` result. `None` when
/// the response does not parse (TS throws).
pub fn project_thread_history(
    response: &Value,
    cwd: Option<&str>,
    materialize: Option<&ImageMaterializer>,
) -> Option<HistoryProjection> {
    let root = as_record(response)?;
    let turns: Vec<&Value> = match root.get("thread") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Object(t)) => match t.get("turns") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a.iter().collect(),
            Some(_) => return None,
        },
        Some(_) => return None,
    };
    let mut timeline: Vec<PersistedTimelineEntry> = Vec::new();
    let mut sub_agent_index: Vec<(String, usize)> = Vec::new();
    let set_index = |thread: String, idx: usize, v: &mut Vec<(String, usize)>| {
        if let Some(e) = v.iter_mut().find(|(t, _)| *t == thread) {
            e.1 = idx;
        } else {
            v.push((thread, idx));
        }
    };
    for turn_v in turns {
        let turn = as_record(turn_v)?;
        let items: Vec<&Value> = match turn.get("items") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(a)) => a.iter().collect(),
            Some(_) => return None,
        };
        for item in items {
            let activity = read_sub_agent_activity(item);
            if let Some(a) = &activity {
                if let Some((_, idx)) = sub_agent_index
                    .iter()
                    .find(|(t, _)| *t == a.agent_thread_id)
                {
                    let idx = *idx;
                    let activity_item = thread_item_to_timeline(
                        item,
                        &ThreadItemOptions {
                            include_user_message: true,
                            cwd,
                            materialize,
                        },
                    );
                    let sub_type = match activity_item {
                        Some(AgentTimelineItem::ToolCall(t)) => match t.detail {
                            ToolCallDetail::SubAgent { sub_agent_type, .. } => sub_agent_type,
                            _ => None,
                        },
                        _ => None,
                    };
                    update_historical_sub_agent_activity(&mut timeline, idx, a.kind, sub_type);
                    continue;
                }
            }
            for entry in thread_item_to_timeline_entries(item, cwd, materialize) {
                let timestamp =
                    history_item_timestamp(item).or_else(|| turn_history_timestamp(turn, &entry));
                let settled = match &entry {
                    AgentTimelineItem::ToolCall(t)
                        if activity.is_some()
                            || (matches!(t.detail, ToolCallDetail::SubAgent { .. })
                                && t.status == ToolCallStatus::Running) =>
                    {
                        AgentTimelineItem::tool_call(settle_historical_sub_agent_activity(
                            (**t).clone(),
                            activity
                                .as_ref()
                                .map(|a| a.kind)
                                .unwrap_or(SubAgentKind::Completed),
                        ))
                    }
                    other => other.clone(),
                };
                let provider_turn_id = match (&entry, turn.get("id")) {
                    (AgentTimelineItem::UserMessage { .. }, Some(Value::String(id))) => {
                        Some(id.clone())
                    }
                    _ => None,
                };
                timeline.push(PersistedTimelineEntry {
                    item: settled,
                    timestamp,
                    provider_turn_id,
                });
                for child in read_historical_sub_agent_thread_ids(item) {
                    set_index(child, timeline.len() - 1, &mut sub_agent_index);
                }
            }
        }
    }
    let sub_agent_routes = sub_agent_index
        .into_iter()
        .filter_map(|(child_thread_id, idx)| match &timeline[idx].item {
            AgentTimelineItem::ToolCall(t)
                if matches!(t.detail, ToolCallDetail::SubAgent { .. }) =>
            {
                Some(PersistedSubAgentRoute {
                    child_thread_id,
                    tool_call: (**t).clone(),
                })
            }
            _ => None,
        })
        .collect();
    Some(HistoryProjection {
        timeline,
        sub_agent_routes,
    })
}
