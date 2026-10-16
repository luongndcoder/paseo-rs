//! OpenCode mapper: port of `translateOpenCodeEvent` (live SSE), the session-history conversion
//! (`buildOpenCodeSessionTimeline` and friends) and the sub-agent presentation folding from
//! `providers/opencode-agent.ts`, `opencode/subagent-presentation.ts` and
//! `opencode/tool-call-mapper.ts` (the tool-call part lives in `paseo-tool-detail`).
//!
//! Input shape (`ProviderMapper::feed`): one row of the recorded tee log
//! (`crates/paseo-opencode/fixtures/*.jsonl`). Rows with `dir == "sse"` carry the global event
//! (`event.payload`); a bare global event or payload is accepted too. The session id is taken from
//! the first `session.created` event or `POST /session` response, like the TS capture driver.
//!
//! Seam: this is the update-translation seam only. `turn_started`, prompt submission events,
//! the session-level child translation (`getChildTranslationState`) and the child assistant
//! presentation upserts belong to the session controller and are not part of this mapper.

use std::collections::{HashMap, HashSet};

use paseo_agent_types::{
    AgentStreamEvent, AgentTaskItem, AgentTimelineItem, AgentUsage, CompactionStatus,
    CompactionTrigger, Map, PermissionAction, PermissionBehavior, PermissionKind,
    PermissionRequest, ProviderSubagentInputEvent, ProviderSubagentStatus, TaskStatus,
    ToolCallDetail, ToolCallItem, ToolCallStatus,
};
use paseo_tool_detail::detail::opencode::{map_opencode_tool_call, OpencodeToolCallParams};
use paseo_tool_detail::js::{js_number_to_string, js_to_string, js_trim, json_stringify};
use paseo_tool_detail::num_from_f64;
use serde_json::{json, Value};

use crate::{MapEnv, ProviderMapper};

const PROVIDER: &str = "opencode";

// ---- small JS-semantics helpers ------------------------------------------------------------------

fn record(v: Option<&Value>) -> Option<&Map> {
    match v {
        Some(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// `readNonEmptyString`.
fn non_empty(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !js_trim(s).is_empty() => Some(js_trim(s).to_string()),
        _ => None,
    }
}

/// A string field that is truthy in JS (`""` is falsy).
fn truthy_str(v: Option<&Value>) -> Option<&str> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s),
        _ => None,
    }
}

fn str_of(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
}

/// JS truthiness of a JSON value (`undefined` is `None`).
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `readPositiveFiniteNumber`.
fn positive_finite(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite() && *f > 0.0),
        _ => None,
    }
}

fn timeline(item: AgentTimelineItem) -> AgentStreamEvent {
    AgentStreamEvent::Timeline {
        item: Box::new(item),
        provider: PROVIDER.into(),
        turn_id: None,
        timestamp: None,
    }
}

fn some_str(v: &Option<String>) -> Option<Option<String>> {
    v.clone().map(Some)
}

// ---- sub-agent presentation (`opencode/subagent-presentation.ts`) --------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubagentFacts {
    pub agent_name: Option<String>,
    pub model_id: Option<String>,
    pub variant: Option<String>,
    pub total_tokens: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubagentPresentation {
    pub facts: SubagentFacts,
    pub last_subtitle: Option<String>,
    pub title_emitted: bool,
    pub linked_tool_call_id: Option<String>,
    pub title_from_link: bool,
    pub description_from_link: bool,
}

fn read_part(v: Option<&str>) -> Option<String> {
    let t = js_trim(v?);
    (!t.is_empty()).then(|| t.to_string())
}

fn format_variant(variant: Option<&str>) -> Option<String> {
    let normalized = read_part(variant)?;
    if normalized == "xhigh" {
        return Some("Extra High".into());
    }
    let parts: Vec<String> = normalized
        .split(|c: char| c == '-' || c == '_' || paseo_tool_detail::js::is_js_whitespace(c))
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut chars = p.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    Some(parts.join(" "))
}

fn format_tokens(total: Option<f64>) -> Option<String> {
    let total = total.filter(|t| *t > 0.0)?;
    if total < 1000.0 {
        return Some(format!(
            "{} tokens",
            js_number_to_string((total + 0.5).floor())
        ));
    }
    let k = (total / 100.0 + 0.5).floor() / 10.0;
    Some(format!("{}k tokens", js_number_to_string(k)))
}

fn build_subtitle(facts: &SubagentFacts) -> Option<String> {
    let parts: Vec<String> = [
        read_part(facts.agent_name.as_deref()),
        read_part(facts.model_id.as_deref()),
        format_variant(facts.variant.as_deref()),
        format_tokens(facts.total_tokens),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join(" \u{b7} "))
}

/// `foldOpenCodeSubagentPresentation`: the subtitle only when it changed.
pub fn fold_presentation(state: &mut SubagentPresentation, patch: SubagentFacts) -> Option<String> {
    if patch.agent_name.is_some() {
        state.facts.agent_name = patch.agent_name;
    }
    if patch.model_id.is_some() {
        state.facts.model_id = patch.model_id;
    }
    if patch.variant.is_some() {
        state.facts.variant = patch.variant;
    }
    if patch.total_tokens.is_some() {
        state.facts.total_tokens = patch.total_tokens;
    }
    let subtitle = build_subtitle(&state.facts)?;
    if state.last_subtitle.as_deref() == Some(subtitle.as_str()) {
        return None;
    }
    state.last_subtitle = Some(subtitle.clone());
    Some(subtitle)
}

/// `claimOpenCodeSubagentFallbackTitle`.
pub fn claim_fallback_title(
    state: &mut SubagentPresentation,
    agent_name: Option<&str>,
) -> Option<String> {
    let title = read_part(agent_name)?;
    if state.title_from_link || state.title_emitted {
        return None;
    }
    state.title_emitted = true;
    Some(title)
}

// ---- translation state -----------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Materialized {
    message_id: String,
    emitted_text: String,
    closed: bool,
}

#[derive(Debug, Clone)]
struct SubAgentActivity {
    tool_call: ToolCallItem,
    child_session_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PendingSteer {
    pub provider_message_id: String,
    pub text: String,
    pub client_message_id: Option<String>,
}

/// Mirrors `OpenCodeEventTranslationState`. Fields TS leaves `undefined` for a bare replay are
/// `Option`s that stay `None` here (the golden driver never primes them).
#[derive(Debug, Default)]
pub struct TranslationState {
    pub session_id: String,
    pub message_roles: HashMap<String, String>,
    pub pending_user_message_text: Option<String>,
    pub pending_client_message_id: Option<String>,
    pub pending_steer_submissions: Option<Vec<PendingSteer>>,
    pub emitted_user_message_ids: Option<HashSet<String>>,
    pub accumulated_usage: AgentUsage,
    pub session_total_cost_usd: Option<f64>,
    materialized_parts: HashMap<String, Materialized>,
    pub emitted_structured_message_ids: HashSet<String>,
    pub compaction_summary_message_ids: HashSet<String>,
    pub emitted_compaction_part_ids: HashSet<String>,
    /// Fingerprint is `JSON.stringify` of the hydrated message / part.
    pub hydrated_message_fingerprints: Option<HashMap<String, String>>,
    pub hydrated_part_fingerprints: Option<HashMap<String, String>>,
    /// `suppressAssistantMessagesUntilIdle?.active`; `None` is the absent object.
    pub suppress_assistant_messages_until_idle: Option<bool>,
    pub part_types: HashMap<String, String>,
    sub_agents_by_call_id: HashMap<String, SubAgentActivity>,
    sub_agent_call_id_by_child_session_id: HashMap<String, String>,
    known_child_session_ids: HashSet<String>,
    subagent_presentation_by_child_id: HashMap<String, SubagentPresentation>,
    pub model_context_windows_by_model_key: Option<HashMap<String, f64>>,
    /// Stand-in for `onAssistantModelContextWindowResolved`: values it would have been called with.
    pub resolved_context_windows: Vec<f64>,
    /// Stand-in for `onMaterializationMismatch`: `(partId, messageId, "text" | "reasoning")`.
    pub materialization_mismatches: Vec<(String, String, &'static str)>,
}

impl TranslationState {
    pub fn new(session_id: impl Into<String>) -> Self {
        TranslationState {
            session_id: session_id.into(),
            ..Default::default()
        }
    }

    fn is_tracked(&self, session_id: &str) -> bool {
        session_id == self.session_id
            || self.known_child_session_ids.contains(session_id)
            || self
                .sub_agent_call_id_by_child_session_id
                .contains_key(session_id)
    }

    fn presentation(&mut self, child: &str) -> &mut SubagentPresentation {
        self.subagent_presentation_by_child_id
            .entry(child.to_string())
            .or_default()
    }

    fn reset_turn_tracking(&mut self) {
        self.part_types.clear();
        self.compaction_summary_message_ids.clear();
        self.emitted_compaction_part_ids.clear();
        if let Some(active) = self.suppress_assistant_messages_until_idle.as_mut() {
            *active = false;
        }
    }
}

/// `matchesHydratedFingerprint`: consumes the fingerprint whether or not it matches.
fn matches_hydrated(fps: &mut Option<HashMap<String, String>>, id: &str, value: &Value) -> bool {
    let Some(map) = fps.as_mut() else {
        return false;
    };
    match map.remove(id) {
        Some(fp) if !fp.is_empty() => fp == json_stringify(value),
        _ => false,
    }
}

// ---- tool part parsing (`OpencodeToolPartSchema` union) --------------------------------------------

struct ToolPart<'a> {
    tool: &'a str,
    call_id: Option<&'a str>,
    status: Option<&'a Value>,
    input: Option<&'a Value>,
    output: Option<&'a Value>,
    error: Option<&'a Value>,
    metadata: Option<&'a Map>,
}

fn trimmed_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
        .map(js_trim)
        .filter(|s| !s.is_empty())
}

fn parse_tool_part(part: &Value) -> Option<ToolPart<'_>> {
    let tool = js_trim(part.get("tool")?.as_str()?);
    if tool.is_empty() {
        return None;
    }
    let state = match part.get("state") {
        None => None,
        Some(Value::Object(m)) => Some(m),
        Some(_) => return None,
    };
    let status = match state.and_then(|s| s.get("status")) {
        None => None,
        Some(v @ Value::String(_)) => Some(v),
        Some(_) => return None,
    };
    let metadata = match state.and_then(|s| s.get("metadata")) {
        None => None,
        Some(Value::Object(m)) => Some(m),
        Some(_) => return None,
    };
    let id = part.get("id");
    let call = part.get("callID");
    let id_ok_or_absent = matches!(id, None | Some(Value::String(_)));
    let call_ok_or_absent = matches!(call, None | Some(Value::String(_)));
    let call_id = if let (Some(c), true) = (trimmed_str(call), id_ok_or_absent) {
        Some(c)
    } else if let (Some(i), true) = (trimmed_str(id), call_ok_or_absent) {
        Some(i)
    } else if id_ok_or_absent && call_ok_or_absent {
        None
    } else {
        return None;
    };
    Some(ToolPart {
        tool,
        call_id,
        status,
        input: state.and_then(|s| s.get("input")),
        output: state.and_then(|s| s.get("output")),
        error: state.and_then(|s| s.get("error")),
        metadata,
    })
}

/// `OpencodeToolPartToTimelineItemSchema` (`None` when the part does not parse or has no call id).
pub fn tool_part_to_item(part: &Value) -> Option<ToolCallItem> {
    let p = parse_tool_part(part)?;
    map_opencode_tool_call(&OpencodeToolCallParams {
        tool_name: p.tool,
        call_id: p.call_id,
        status: p.status,
        input: p.input,
        output: p.output,
        error: p.error,
        metadata: p.metadata,
    })
}

fn is_todo_write_part(part: &Value) -> bool {
    str_of(part.get("type")) == Some("tool")
        && str_of(part.get("tool")).is_some_and(|t| js_trim(t).to_lowercase() == "todowrite")
}

// ---- todo ------------------------------------------------------------------------------------

type TodoRaw = (Option<String>, Option<String>);

/// `readOpenCodeTodoItems`.
fn read_todo_items(v: Option<&Value>) -> Option<Vec<TodoRaw>> {
    match v {
        Some(Value::String(s)) => {
            let parsed: Value = serde_json::from_str(s).ok()?;
            read_todo_items(Some(&parsed))
        }
        Some(Value::Array(a)) => Some(
            a.iter()
                .filter_map(|e| {
                    let r = record(Some(e))?;
                    let content = non_empty(r.get("content"))?;
                    Some((Some(content), non_empty(r.get("status"))))
                })
                .collect(),
        ),
        Some(Value::Object(r)) => read_todo_items(r.get("todos")),
        _ => None,
    }
}

fn read_todo_items_from_tool_part(part: &Value) -> Option<Vec<TodoRaw>> {
    let state = record(part.get("state"));
    read_todo_items(state.and_then(|s| s.get("input")))
        .or_else(|| read_todo_items(state.and_then(|s| s.get("output"))))
        .or_else(|| read_todo_items(state.and_then(|s| s.get("metadata"))))
}

fn todos_to_item(todos: &[TodoRaw]) -> AgentTimelineItem {
    AgentTimelineItem::Todo {
        items: todos
            .iter()
            .filter_map(|(content, status)| {
                let text = non_empty(content.as_ref().map(|c| Value::String(c.clone())).as_ref())?;
                let status_str = status.as_deref();
                Some(AgentTaskItem {
                    text,
                    completed: status_str == Some("completed"),
                    id: None,
                    status: Some(match status_str {
                        Some("completed") => TaskStatus::Completed,
                        Some("in_progress") | Some("inProgress") => TaskStatus::InProgress,
                        _ => TaskStatus::Pending,
                    }),
                    active_form: None,
                })
            })
            .collect(),
    }
}

fn compaction_item(
    status: CompactionStatus,
    trigger: Option<CompactionTrigger>,
) -> AgentTimelineItem {
    AgentTimelineItem::Compaction {
        status,
        trigger,
        pre_tokens: None,
        extra: Map::new(),
    }
}

// ---- permission ------------------------------------------------------------------------------

const PERMISSION_COMMAND_KEYS: [&str; 3] = ["command", "cmd", "shellCommand"];
const PERMISSION_CWD_KEYS: [&str; 4] = ["cwd", "directory", "path", "workdir"];
const PERMISSION_REASON_KEYS: [&str; 4] = ["reason", "purpose", "description", "message"];

fn permission_title(permission: &str) -> String {
    let mapped = match permission {
        "external_directory" => Some("Access external directory"),
        "bash" => Some("Run shell command"),
        "read" | "read_file" => Some("Read files"),
        "write" | "write_file" | "create_file" => Some("Write files"),
        "edit" | "apply_patch" | "apply_diff" => Some("Edit files"),
        _ => None,
    };
    if let Some(m) = mapped {
        return m.into();
    }
    let normalized = permission
        .split(|c: char| c == '_' || c == '-' || paseo_tool_detail::js::is_js_whitespace(c))
        .map(js_trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut chars = p.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty() {
        "Permission request".into()
    } else {
        normalized
    }
}

fn first_string(record: Option<&Map>, keys: &[&str]) -> Option<String> {
    let record = record?;
    keys.iter().find_map(|k| non_empty(record.get(*k)))
}

fn permission_field(metadata: Option<&Map>, keys: &[&str]) -> Option<String> {
    first_string(metadata, keys)
        .or_else(|| first_string(record(metadata.and_then(|m| m.get("input"))), keys))
}

fn permission_actions() -> Vec<PermissionAction> {
    use paseo_agent_types::{ActionIntent, ActionVariant};
    vec![
        PermissionAction {
            id: "deny".into(),
            label: "Deny".into(),
            behavior: PermissionBehavior::Deny,
            variant: Some(ActionVariant::Danger),
            intent: Some(ActionIntent::Dismiss),
        },
        PermissionAction {
            id: "allow_always".into(),
            label: "Allow always".into(),
            behavior: PermissionBehavior::Allow,
            variant: Some(ActionVariant::Secondary),
            intent: None,
        },
        PermissionAction {
            id: "allow_once".into(),
            label: "Allow once".into(),
            behavior: PermissionBehavior::Allow,
            variant: Some(ActionVariant::Primary),
            intent: None,
        },
    ]
}

// ---- `translateOpenCodeEvent` ----------------------------------------------------------------

/// `translateOpenCodeEvent` over one global-event payload (`{type, properties}`).
pub fn translate_event(event: &Value, state: &mut TranslationState) -> Vec<AgentStreamEvent> {
    let mut events = Vec::new();
    let props = event.get("properties");
    let session_matches = |state: &TranslationState| {
        str_of(props.and_then(|p| p.get("sessionID"))) == Some(&state.session_id)
    };
    match str_of(event.get("type")).unwrap_or("") {
        "session.created" | "session.updated" => {
            session_created_or_updated(props, state, &mut events)
        }
        "session.deleted" => session_deleted(props, state, &mut events),
        "message.updated" => message_updated(props, state, &mut events),
        "message.part.updated" => message_part_updated(props, state, &mut events),
        "message.part.delta" => message_part_delta(props, state, &mut events),
        "permission.asked" => permission_asked(props, state, &mut events),
        "question.asked" => question_asked(props, state, &mut events),
        "todo.updated" => {
            if session_matches(state) {
                let todos: Vec<TodoRaw> = props
                    .and_then(|p| p.get("todos"))
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .map(|t| {
                                (
                                    non_empty(t.get("content")),
                                    t.get("status").and_then(Value::as_str).map(str::to_string),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                events.push(timeline(todos_to_item(&todos)));
            }
        }
        "session.compacted" => {
            if session_matches(state) {
                events.push(timeline(compaction_item(CompactionStatus::Completed, None)));
            }
        }
        "session.idle" => {
            if session_matches(state) {
                state.reset_turn_tracking();
                events.push(turn_completed());
            }
        }
        "session.error" => session_error(props, state, &mut events),
        "session.status" => session_status(props, state, &mut events),
        _ => {}
    }
    events
}

fn turn_completed() -> AgentStreamEvent {
    AgentStreamEvent::TurnCompleted {
        provider: PROVIDER.into(),
        usage: None,
        turn_id: None,
    }
}

#[derive(Default)]
struct UpsertFields {
    title: Option<String>,
    description: Option<String>,
    status: Option<ProviderSubagentStatus>,
    tool_call_id: Option<String>,
    cwd: Option<String>,
    subtitle: Option<String>,
    parent: Option<Option<String>>,
}

fn upsert(id: &str, f: UpsertFields) -> AgentStreamEvent {
    AgentStreamEvent::ProviderSubagent {
        provider: PROVIDER.into(),
        event: Box::new(ProviderSubagentInputEvent::Upsert {
            id: id.to_string(),
            title: some_str(&f.title),
            description: some_str(&f.description),
            status: f.status,
            tool_call_id: some_str(&f.tool_call_id),
            cwd: some_str(&f.cwd),
            subtitle: some_str(&f.subtitle),
            parent_subagent_id: f.parent,
            timestamp: None,
        }),
    }
}

struct ChildInfo {
    id: String,
    parent: String,
    title: Option<String>,
    directory: Option<String>,
    agent: Option<String>,
    model: Option<(String, Option<String>)>,
}

/// `readOpenCodeChildSessionInfo` (the `revert` field is not used by the translator).
fn read_child_info(info: &Map) -> Option<ChildInfo> {
    let id = non_empty(info.get("id"))?;
    let parent = non_empty(info.get("parentID")).or_else(|| non_empty(info.get("parentId")))?;
    let model = record(info.get("model")).and_then(|m| {
        let id = non_empty(m.get("id"))?;
        Some((id, non_empty(m.get("variant"))))
    });
    Some(ChildInfo {
        id,
        parent,
        title: non_empty(info.get("title")),
        directory: non_empty(info.get("directory")),
        agent: non_empty(info.get("agent")),
        model,
    })
}

fn child_detected(
    child: &ChildInfo,
    state: &mut TranslationState,
    events: &mut Vec<AgentStreamEvent>,
    status: Option<ProviderSubagentStatus>,
) -> bool {
    if child.id == state.session_id || !state.is_tracked(&child.parent) {
        return false;
    }
    if state.known_child_session_ids.contains(&child.id) {
        return false;
    }
    state.known_child_session_ids.insert(child.id.clone());
    let presentation = state.presentation(&child.id);
    let subtitle = fold_presentation(
        presentation,
        SubagentFacts {
            agent_name: child.agent.clone(),
            model_id: child.model.as_ref().map(|m| m.0.clone()),
            variant: child.model.as_ref().and_then(|m| m.1.clone()),
            total_tokens: None,
        },
    );
    let title = claim_fallback_title(presentation, child.agent.as_deref());
    let description = child
        .title
        .clone()
        .filter(|_| !presentation.description_from_link);
    let parent = if child.parent == state.session_id {
        None
    } else {
        Some(child.parent.clone())
    };
    events.push(upsert(
        &child.id,
        UpsertFields {
            title,
            description,
            status,
            cwd: child.directory.clone(),
            subtitle,
            parent: Some(parent),
            ..Default::default()
        },
    ));
    true
}

fn link_child_session(
    state: &mut TranslationState,
    call_id: &str,
    child_session_id: &str,
    events: &mut Vec<AgentStreamEvent>,
) {
    let Some(activity) = state.sub_agents_by_call_id.get_mut(call_id) else {
        return;
    };
    activity.child_session_id = Some(child_session_id.to_string());
    let tool_call = activity.tool_call.clone();
    state
        .sub_agent_call_id_by_child_session_id
        .insert(child_session_id.to_string(), call_id.to_string());
    link_presentation(state, &tool_call, child_session_id, events);
}

fn link_presentation(
    state: &mut TranslationState,
    tool_call: &ToolCallItem,
    child_session_id: &str,
    events: &mut Vec<AgentStreamEvent>,
) {
    let ToolCallDetail::SubAgent {
        sub_agent_type,
        description,
        ..
    } = &tool_call.detail
    else {
        return;
    };
    let presentation = state.presentation(child_session_id);
    if presentation.linked_tool_call_id.as_deref() == Some(tool_call.call_id.as_str()) {
        return;
    }
    presentation.linked_tool_call_id = Some(tool_call.call_id.clone());
    let sub_agent_type = non_empty(sub_agent_type.clone().map(Value::String).as_ref());
    let description = non_empty(description.clone().map(Value::String).as_ref());
    if sub_agent_type.is_some() {
        presentation.title_from_link = true;
        presentation.title_emitted = true;
    }
    if description.is_some() {
        presentation.description_from_link = true;
    }
    let subtitle = fold_presentation(
        presentation,
        SubagentFacts {
            agent_name: sub_agent_type.clone(),
            ..Default::default()
        },
    );
    events.push(upsert(
        child_session_id,
        UpsertFields {
            title: sub_agent_type,
            description,
            tool_call_id: Some(tool_call.call_id.clone()),
            subtitle,
            ..Default::default()
        },
    ));
}

fn sub_agent_timeline_item(activity: &SubAgentActivity) -> ToolCallItem {
    let mut item = activity.tool_call.clone();
    if let ToolCallDetail::SubAgent {
        child_session_id, ..
    } = &mut item.detail
    {
        let merged = activity
            .child_session_id
            .clone()
            .or_else(|| child_session_id.clone());
        if merged.is_some() {
            *child_session_id = merged;
        }
    }
    item
}

fn register_sub_agent_tool_call(
    item: ToolCallItem,
    state: &mut TranslationState,
    events: &mut Vec<AgentStreamEvent>,
) -> ToolCallItem {
    let ToolCallDetail::SubAgent {
        child_session_id, ..
    } = &item.detail
    else {
        return item;
    };
    let link_to = child_session_id.clone();
    let call_id = item.call_id.clone();
    state
        .sub_agents_by_call_id
        .entry(call_id.clone())
        .and_modify(|a| a.tool_call = item.clone())
        .or_insert_with(|| SubAgentActivity {
            tool_call: item.clone(),
            child_session_id: None,
        });
    if let Some(child) = link_to {
        link_child_session(state, &call_id, &child, events);
    }
    sub_agent_timeline_item(&state.sub_agents_by_call_id[&call_id])
}

fn session_created_or_updated(
    props: Option<&Value>,
    state: &mut TranslationState,
    events: &mut Vec<AgentStreamEvent>,
) {
    let Some(info) = record(props.and_then(|p| p.get("info"))) else {
        return;
    };
    let Some(info_id) = str_of(info.get("id")) else {
        return;
    };
    if info_id == state.session_id {
        if let Some(cost) = positive_finite(info.get("cost")) {
            let total = state.session_total_cost_usd.map_or(cost, |c| c.max(cost));
            state.session_total_cost_usd = Some(total);
            state.accumulated_usage.total_cost_usd = Some(num_from_f64(total));
        }
        events.push(AgentStreamEvent::ThreadStarted {
            session_id: state.session_id.clone(),
            provider: PROVIDER.into(),
        });
        return;
    }
    let parent = non_empty(info.get("parentID")).or_else(|| non_empty(info.get("parentId")));
    let Some(parent) = parent else {
        return;
    };
    let mut merged = info.clone();
    merged.insert("id".into(), Value::String(info_id.to_string()));
    merged.insert("parentID".into(), Value::String(parent.clone()));
    if let Some(child) = read_child_info(&merged) {
        child_detected(&child, state, events, Some(ProviderSubagentStatus::Running));
    }
    if parent == state.session_id {
        // `appendOpenCodeSubAgentChildSessionLinked`: only when exactly one running sub_agent
        // tool call is still waiting for its child.
        let candidates: Vec<String> = state
            .sub_agents_by_call_id
            .iter()
            .filter(|(_, a)| {
                a.tool_call.status == ToolCallStatus::Running
                    && matches!(a.tool_call.detail, ToolCallDetail::SubAgent { .. })
                    && a.child_session_id.as_deref().is_none_or(str::is_empty)
            })
            .map(|(k, _)| k.clone())
            .collect();
        if let [call_id] = candidates.as_slice() {
            link_child_session(state, call_id, info_id, events);
            let item = sub_agent_timeline_item(&state.sub_agents_by_call_id[call_id]);
            events.push(timeline(AgentTimelineItem::tool_call(item)));
        }
    }
}

fn session_deleted(
    props: Option<&Value>,
    state: &mut TranslationState,
    events: &mut Vec<AgentStreamEvent>,
) {
    let Some(session_id) = str_of(props.and_then(|p| p.get("sessionID"))) else {
        return;
    };
    if !state.is_tracked(session_id) {
        return;
    }
    state.known_child_session_ids.remove(session_id);
    state
        .sub_agent_call_id_by_child_session_id
        .remove(session_id);
    state.subagent_presentation_by_child_id.remove(session_id);
    events.push(AgentStreamEvent::ProviderSubagent {
        provider: PROVIDER.into(),
        event: Box::new(ProviderSubagentInputEvent::Remove {
            id: session_id.to_string(),
        }),
    });
}

/// `stringifyStructuredAssistantMessage`: `None` is `undefined`.
fn stringify_structured(v: Option<&Value>) -> Option<String> {
    match v {
        None => None,
        Some(Value::String(s)) => {
            let t = js_trim(s);
            (!t.is_empty()).then(|| t.to_string())
        }
        Some(other) => Some(json_stringify(other)),
    }
}

fn is_compaction_summary_message(info: &Value) -> bool {
    str_of(info.get("role")) == Some("assistant")
        && (info.get("summary") == Some(&Value::Bool(true))
            || str_of(info.get("agent")) == Some("compaction")
            || str_of(info.get("mode")) == Some("compaction"))
}

fn message_updated(
    props: Option<&Value>,
    state: &mut TranslationState,
    events: &mut Vec<AgentStreamEvent>,
) {
    let Some(info) = props.and_then(|p| p.get("info")) else {
        return;
    };
    if str_of(info.get("sessionID")) != Some(state.session_id.as_str()) {
        return;
    }
    let Some(id) = str_of(info.get("id")) else {
        return;
    };
    let role = str_of(info.get("role")).unwrap_or("");
    state.message_roles.insert(id.to_string(), role.to_string());
    if matches_hydrated(&mut state.hydrated_message_fingerprints, id, info) {
        return;
    }
    if role == "user" {
        user_message_updated(id, state, events);
        return;
    }
    if role != "assistant" {
        return;
    }
    if state.suppress_assistant_messages_until_idle == Some(true) {
        state.compaction_summary_message_ids.insert(id.to_string());
        return;
    }
    if is_compaction_summary_message(info) {
        state.compaction_summary_message_ids.insert(id.to_string());
        return;
    }
    if let (Some(p), Some(m)) = (
        truthy_str(info.get("providerID")),
        truthy_str(info.get("modelID")),
    ) {
        let key = format!("{p}/{m}");
        if let Some(window) = state
            .model_context_windows_by_model_key
            .as_ref()
            .and_then(|w| w.get(&key))
        {
            state.resolved_context_windows.push(*window);
        }
    }
    let completed_absent = record(info.get("time")).is_none_or(|t| !t.contains_key("completed"));
    if state.emitted_structured_message_ids.contains(id) || completed_absent {
        return;
    }
    let Some(text) = stringify_structured(info.get("structured")) else {
        return;
    };
    state.emitted_structured_message_ids.insert(id.to_string());
    events.push(timeline(AgentTimelineItem::AssistantMessage {
        text,
        message_id: Some(id.to_string()),
    }));
}

fn user_message_updated(
    id: &str,
    state: &mut TranslationState,
    events: &mut Vec<AgentStreamEvent>,
) {
    let steer = state.pending_steer_submissions.as_mut().and_then(|subs| {
        let idx = subs.iter().position(|s| s.provider_message_id == id)?;
        Some(subs.remove(idx))
    });
    let text = steer
        .as_ref()
        .map(|s| s.text.clone())
        .or_else(|| state.pending_user_message_text.clone());
    let Some(text) = text.filter(|t| !js_trim(t).is_empty()) else {
        return;
    };
    if let Some(ids) = state.emitted_user_message_ids.as_mut() {
        if !ids.insert(id.to_string()) {
            return;
        }
    }
    let client_message_id = steer
        .and_then(|s| s.client_message_id)
        .or_else(|| state.pending_client_message_id.clone())
        .filter(|c| !c.is_empty());
    events.push(timeline(AgentTimelineItem::UserMessage {
        text,
        message_id: Some(id.to_string()),
        client_message_id,
    }));
}

fn message_part_updated(
    props: Option<&Value>,
    state: &mut TranslationState,
    events: &mut Vec<AgentStreamEvent>,
) {
    let Some(part) = props.and_then(|p| p.get("part")) else {
        return;
    };
    if is_todo_write_part(part) {
        return;
    }
    if str_of(part.get("sessionID")) != Some(state.session_id.as_str()) {
        return;
    }
    let part_id = str_of(part.get("id")).unwrap_or("");
    if matches_hydrated(&mut state.hydrated_part_fingerprints, part_id, part) {
        return;
    }
    let message_id = str_of(part.get("messageID")).unwrap_or("").to_string();
    let part_type = str_of(part.get("type")).unwrap_or("").to_string();
    let role = state.message_roles.get(&message_id).cloned();
    state
        .part_types
        .insert(part_id.to_string(), part_type.clone());

    if state.compaction_summary_message_ids.contains(&message_id) {
        return;
    }
    if state.suppress_assistant_messages_until_idle == Some(true)
        && part_type == "text"
        && role.as_deref() != Some("user")
    {
        state.compaction_summary_message_ids.insert(message_id);
        return;
    }
    match part_type.as_str() {
        "text" => text_part(part, role.as_deref(), state, events),
        "reasoning" => reasoning_part(part, state, events),
        "tool" => {
            if let Some(item) = tool_part_to_item(part) {
                let item = register_sub_agent_tool_call(item, state, events);
                events.push(timeline(AgentTimelineItem::tool_call(item)));
            }
        }
        "compaction" => {
            if !state
                .emitted_compaction_part_ids
                .insert(part_id.to_string())
            {
                return;
            }
            let trigger = if truthy(part.get("auto")) {
                CompactionTrigger::Auto
            } else {
                CompactionTrigger::Manual
            };
            events.push(timeline(compaction_item(
                CompactionStatus::Loading,
                Some(trigger),
            )));
        }
        "step-finish" => step_finish(part, state, events),
        _ => {}
    }
}

fn step_finish(part: &Value, state: &mut TranslationState, events: &mut Vec<AgentStreamEvent>) {
    if let Some(cost) = positive_finite(part.get("cost")) {
        state.session_total_cost_usd = Some(state.session_total_cost_usd.unwrap_or(0.0) + cost);
    }
    // `mergeOpenCodeStepFinishUsage`
    let tokens = record(part.get("tokens"));
    let tok = |k: &str| positive_finite(tokens.and_then(|t| t.get(k)));
    let cache = record(tokens.and_then(|t| t.get("cache")));
    let cache_tok = |k: &str| positive_finite(cache.and_then(|c| c.get(k)));
    let (input, output, reasoning) = (tok("input"), tok("output"), tok("reasoning"));
    let (cache_read, cache_write) = (cache_tok("read"), cache_tok("write"));
    let total = input.unwrap_or(0.0)
        + output.unwrap_or(0.0)
        + reasoning.unwrap_or(0.0)
        + cache_read.unwrap_or(0.0)
        + cache_write.unwrap_or(0.0);
    let usage = &mut state.accumulated_usage;
    if let Some(v) = input {
        usage.input_tokens = Some(num_from_f64(v));
    }
    if let Some(v) = cache_read {
        usage.cached_input_tokens = Some(num_from_f64(v));
    }
    if let Some(v) = output {
        usage.output_tokens = Some(num_from_f64(v));
    }
    if total > 0.0 {
        usage.context_window_used_tokens = Some(num_from_f64(total));
    }
    if let Some(cost) = positive_finite(part.get("cost")) {
        let value = match state.session_total_cost_usd {
            Some(t) => t,
            None => {
                usage
                    .total_cost_usd
                    .as_ref()
                    .and_then(|n| n.as_f64())
                    .unwrap_or(0.0)
                    + cost
            }
        };
        usage.total_cost_usd = Some(num_from_f64(value));
    }
    let has = [
        &usage.input_tokens,
        &usage.cached_input_tokens,
        &usage.output_tokens,
        &usage.total_cost_usd,
        &usage.context_window_max_tokens,
        &usage.context_window_used_tokens,
    ]
    .iter()
    .any(|v| v.is_some());
    if has {
        events.push(AgentStreamEvent::UsageUpdated {
            provider: PROVIDER.into(),
            usage: usage.clone(),
            turn_id: None,
        });
    }
}

fn text_part(
    part: &Value,
    role: Option<&str>,
    state: &mut TranslationState,
    events: &mut Vec<AgentStreamEvent>,
) {
    let message_id = str_of(part.get("messageID")).unwrap_or("").to_string();
    let part_id = str_of(part.get("id")).unwrap_or("").to_string();
    let text = str_of(part.get("text")).unwrap_or("");
    if role == Some("user") {
        // `synthetic` text is OpenCode's own addition to the user message.
        if text.is_empty() || part.get("synthetic") == Some(&Value::Bool(true)) {
            return;
        }
        if let Some(ids) = state.emitted_user_message_ids.as_mut() {
            if !ids.insert(message_id.clone()) {
                return;
            }
        }
        events.push(timeline(AgentTimelineItem::UserMessage {
            text: text.to_string(),
            message_id: Some(message_id),
            client_message_id: None,
        }));
        return;
    }
    if !truthy(record(part.get("time")).and_then(|t| t.get("end"))) {
        return;
    }
    if let Some(suffix) = materialize_part(state, &part_id, &message_id, text, "text") {
        events.push(timeline(AgentTimelineItem::AssistantMessage {
            text: suffix,
            message_id: Some(message_id),
        }));
    }
}

fn reasoning_part(part: &Value, state: &mut TranslationState, events: &mut Vec<AgentStreamEvent>) {
    let message_id = str_of(part.get("messageID")).unwrap_or("").to_string();
    let part_id = str_of(part.get("id")).unwrap_or("").to_string();
    let text = str_of(part.get("text")).unwrap_or("");
    if !truthy(record(part.get("time")).and_then(|t| t.get("end"))) {
        return;
    }
    if let Some(suffix) = materialize_part(state, &part_id, &message_id, text, "reasoning") {
        events.push(timeline(AgentTimelineItem::Reasoning { text: suffix }));
    }
}

/// Shared tail of the text/reasoning completed-part path: emit only the unseen suffix.
fn materialize_part(
    state: &mut TranslationState,
    part_id: &str,
    message_id: &str,
    text: &str,
    kind: &'static str,
) -> Option<String> {
    let prev = state.materialized_parts.get(part_id);
    if prev.is_some_and(|m| m.closed) {
        return None;
    }
    let emitted = match prev {
        Some(m) if m.message_id == message_id => m.emitted_text.clone(),
        _ => String::new(),
    };
    if !text.starts_with(&emitted) {
        state
            .materialization_mismatches
            .push((part_id.to_string(), message_id.to_string(), kind));
        return None;
    }
    let suffix = text[emitted.len()..].to_string();
    state.materialized_parts.insert(
        part_id.to_string(),
        Materialized {
            message_id: message_id.to_string(),
            emitted_text: text.to_string(),
            closed: true,
        },
    );
    (!suffix.is_empty()).then_some(suffix)
}

fn message_part_delta(
    props: Option<&Value>,
    state: &mut TranslationState,
    events: &mut Vec<AgentStreamEvent>,
) {
    let get = |k: &str| props.and_then(|p| p.get(k));
    if str_of(get("sessionID")) != Some(state.session_id.as_str()) {
        return;
    }
    let (Some(delta), Some(field)) = (truthy_str(get("delta")), truthy_str(get("field"))) else {
        return;
    };
    let message_id = truthy_str(get("messageID"));
    let part_id = truthy_str(get("partID"));
    let role = message_id.and_then(|m| state.message_roles.get(m)).cloned();
    let known_type = part_id.and_then(|p| state.part_types.get(p)).cloned();
    let is_reasoning = known_type.as_deref() == Some("reasoning") || field == "reasoning";

    if message_id.is_some_and(|m| state.compaction_summary_message_ids.contains(m)) {
        return;
    }
    if is_reasoning {
        if !materialized_delta(state, part_id, message_id.unwrap_or(""), delta) {
            return;
        }
        events.push(timeline(AgentTimelineItem::Reasoning {
            text: delta.to_string(),
        }));
        return;
    }
    if field != "text" || role.as_deref() == Some("user") {
        return;
    }
    let Some(assistant_message_id) = message_id.or(part_id) else {
        return;
    };
    if state.suppress_assistant_messages_until_idle == Some(true) {
        state
            .compaction_summary_message_ids
            .insert(assistant_message_id.to_string());
        return;
    }
    if !materialized_delta(state, part_id, assistant_message_id, delta) {
        return;
    }
    events.push(timeline(AgentTimelineItem::AssistantMessage {
        text: delta.to_string(),
        message_id: Some(assistant_message_id.to_string()),
    }));
}

fn materialized_delta(
    state: &mut TranslationState,
    part_id: Option<&str>,
    message_id: &str,
    delta: &str,
) -> bool {
    let Some(part_id) = part_id else {
        return true;
    };
    let prev = state.materialized_parts.get(part_id);
    if prev.is_some_and(|m| m.closed) {
        return false;
    }
    let emitted = format!("{}{delta}", prev.map_or("", |m| m.emitted_text.as_str()));
    state.materialized_parts.insert(
        part_id.to_string(),
        Materialized {
            message_id: message_id.to_string(),
            emitted_text: emitted,
            closed: false,
        },
    );
    true
}

fn permission_asked(
    props: Option<&Value>,
    state: &mut TranslationState,
    events: &mut Vec<AgentStreamEvent>,
) {
    let get = |k: &str| props.and_then(|p| p.get(k));
    let Some(session_id) = str_of(get("sessionID")) else {
        return;
    };
    if !state.is_tracked(session_id) {
        return;
    }
    let (Some(id), Some(permission)) = (str_of(get("id")), str_of(get("permission"))) else {
        return;
    };
    let metadata = record(get("metadata"));
    let tool = record(get("tool"));
    let patterns: Vec<String> = get("patterns")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let command = permission_field(metadata, &PERMISSION_COMMAND_KEYS);
    let cwd = permission_field(metadata, &PERMISSION_CWD_KEYS);
    let reason = permission_field(metadata, &PERMISSION_REASON_KEYS);

    let mut input = Map::new();
    if !patterns.is_empty() {
        input.insert("patterns".into(), json!(patterns));
    }
    if let Some(m) = metadata {
        input.insert("metadata".into(), Value::Object(m.clone()));
    }
    if let Some(t) = tool {
        input.insert("tool".into(), Value::Object(t.clone()));
    }
    if let Some(c) = &command {
        input.insert("command".into(), Value::String(c.clone()));
    }
    let detail = match &command {
        Some(c) => ToolCallDetail::Shell {
            command: c.clone(),
            cwd: cwd.clone(),
            output: None,
            exit_code: None,
        },
        None => {
            let mut merged = Map::new();
            merged.insert("permission".into(), Value::String(permission.to_string()));
            for (k, v) in &input {
                merged.insert(k.clone(), v.clone());
            }
            ToolCallDetail::Unknown {
                input: Value::Object(merged),
                output: Value::Null,
            }
        }
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(r) = reason {
        parts.push(r);
    }
    if !patterns.is_empty() {
        parts.push(format!("Scope: {}", patterns.join(", ")));
    }
    let description = (!parts.is_empty()).then(|| parts.join(" - "));
    events.push(AgentStreamEvent::PermissionRequested {
        provider: PROVIDER.into(),
        request: Box::new(PermissionRequest {
            id: id.to_string(),
            provider: PROVIDER.into(),
            name: permission.to_string(),
            kind: PermissionKind::Tool,
            title: Some(permission_title(permission)),
            description,
            input: Some(input),
            detail: Some(detail),
            suggestions: None,
            actions: Some(permission_actions()),
            metadata: None,
        }),
        turn_id: None,
    });
}

fn question_asked(
    props: Option<&Value>,
    state: &mut TranslationState,
    events: &mut Vec<AgentStreamEvent>,
) {
    let get = |k: &str| props.and_then(|p| p.get(k));
    if str_of(get("sessionID")) != Some(state.session_id.as_str()) {
        return;
    }
    let Some(id) = str_of(get("id")) else {
        return;
    };
    let questions: Vec<Value> = get("questions")
        .and_then(Value::as_array)
        .map(|qs| {
            qs.iter()
                .filter_map(|q| {
                    let question = truthy_str(q.get("question"))?;
                    let header = truthy_str(q.get("header"))?;
                    let options: Vec<Value> = q
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|os| {
                            os.iter()
                                .map(|o| {
                                    let mut m = Map::new();
                                    if let Some(l) = o.get("label") {
                                        m.insert("label".into(), l.clone());
                                    }
                                    if let Some(d) = truthy_str(o.get("description")) {
                                        m.insert("description".into(), Value::String(d.into()));
                                    }
                                    Value::Object(m)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let mut m = Map::new();
                    m.insert("question".into(), Value::String(question.into()));
                    m.insert("header".into(), Value::String(header.into()));
                    m.insert("options".into(), Value::Array(options));
                    if q.get("multiple") == Some(&Value::Bool(true)) {
                        m.insert("multiSelect".into(), Value::Bool(true));
                    }
                    m.insert("allowOther".into(), Value::Bool(true));
                    Some(Value::Object(m))
                })
                .collect()
        })
        .unwrap_or_default();
    if questions.is_empty() {
        return;
    }
    let mut input = Map::new();
    input.insert("questions".into(), Value::Array(questions));
    let mut metadata = Map::new();
    metadata.insert("source".into(), Value::String("opencode_question".into()));
    if let Some(tool) = record(get("tool")) {
        for (k, v) in tool {
            metadata.insert(k.clone(), v.clone());
        }
    }
    events.push(AgentStreamEvent::PermissionRequested {
        provider: PROVIDER.into(),
        request: Box::new(PermissionRequest {
            id: id.to_string(),
            provider: PROVIDER.into(),
            name: "question".into(),
            kind: PermissionKind::Question,
            title: Some("Question".into()),
            description: None,
            input: Some(input),
            detail: None,
            suggestions: None,
            actions: None,
            metadata: Some(metadata),
        }),
        turn_id: None,
    });
}

/// `toDiagnosticErrorMessage` for a decoded JSON value (never an `Error` instance).
pub fn diagnostic_error_message(error: Option<&Value>) -> String {
    match error {
        Some(Value::String(s)) => {
            let t = js_trim(s);
            if t.is_empty() {
                "Unknown error".into()
            } else {
                t.into()
            }
        }
        None | Some(Value::Null) => "Unknown error".into(),
        Some(other) => {
            let serialized = json_stringify(other);
            if serialized != "{}" && serialized != "\"\"" {
                return serialized;
            }
            let s = js_to_string(other);
            if !s.is_empty() && s != "[object Object]" {
                s
            } else {
                "Unknown error".into()
            }
        }
    }
}

fn session_error(
    props: Option<&Value>,
    state: &mut TranslationState,
    events: &mut Vec<AgentStreamEvent>,
) {
    if str_of(props.and_then(|p| p.get("sessionID"))) != Some(state.session_id.as_str()) {
        return;
    }
    state.reset_turn_tracking();
    let error = props.and_then(|p| p.get("error"));
    let aborted = record(error)
        .and_then(|e| e.get("name"))
        .and_then(Value::as_str)
        == Some("MessageAbortedError");
    if aborted {
        events.push(AgentStreamEvent::TurnCanceled {
            provider: PROVIDER.into(),
            reason: "interrupted".into(),
            turn_id: None,
        });
    } else {
        events.push(AgentStreamEvent::TurnFailed {
            provider: PROVIDER.into(),
            error: diagnostic_error_message(error),
            code: None,
            diagnostic: None,
            turn_id: None,
        });
    }
}

fn session_status(
    props: Option<&Value>,
    state: &mut TranslationState,
    events: &mut Vec<AgentStreamEvent>,
) {
    if str_of(props.and_then(|p| p.get("sessionID"))) != Some(state.session_id.as_str()) {
        return;
    }
    let status = props.and_then(|p| p.get("status"));
    match str_of(status.and_then(|s| s.get("type"))) {
        Some("idle") => {
            state.reset_turn_tracking();
            events.push(turn_completed());
        }
        Some("retry") => {
            // opencode retries forever; show the attempts as visible (non-terminal) activity.
            let message = str_of(status.and_then(|s| s.get("message"))).map_or("", js_trim);
            let attempt = status
                .and_then(|s| s.get("attempt"))
                .map_or_else(|| "undefined".to_string(), js_to_string);
            let text = if message.is_empty() {
                format!("Provider retry (attempt {attempt})")
            } else {
                format!("Provider retry (attempt {attempt}): {message}")
            };
            events.push(timeline(AgentTimelineItem::Error { message: text }));
        }
        _ => {}
    }
}

// ---- mapper ----------------------------------------------------------------------------------

/// Live mapper over tee-log rows. `Default` is an empty mapper that learns its session id.
#[derive(Debug, Default)]
pub struct OpencodeMapper {
    state: Option<TranslationState>,
}

impl OpencodeMapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mapper with a known session id (skips discovery).
    pub fn with_session(session_id: impl Into<String>) -> Self {
        OpencodeMapper {
            state: Some(TranslationState::new(session_id)),
        }
    }

    pub fn state(&self) -> Option<&TranslationState> {
        self.state.as_ref()
    }

    pub fn state_mut(&mut self) -> Option<&mut TranslationState> {
        self.state.as_mut()
    }

    /// The capture driver takes the id from the first `session.created`, else the first
    /// `POST /session` style response body with a `ses_` id.
    fn discover_session(&mut self, row: &Value, payload: Option<&Value>) {
        if self.state.is_some() {
            return;
        }
        let from_created = payload
            .filter(|p| str_of(p.get("type")) == Some("session.created"))
            .and_then(|p| p.get("properties")?.get("info")?.get("id")?.as_str());
        let from_res = (str_of(row.get("dir")) == Some("res"))
            .then(|| row.get("body")?.get("id")?.as_str())
            .flatten()
            .filter(|id| id.starts_with("ses_"));
        if let Some(id) = from_created.or(from_res) {
            self.state = Some(TranslationState::new(id));
        }
    }
}

fn row_payload(row: &Value) -> Option<&Value> {
    if str_of(row.get("dir")).is_some() {
        if row.get("dir")? != "sse" {
            return None;
        }
        return row.get("event")?.get("payload");
    }
    // A bare global event (`{payload}` or `{type, properties}`).
    match row.get("payload") {
        Some(p) if p.get("type").is_some_and(Value::is_string) => Some(p),
        _ if row.get("type").is_some_and(Value::is_string) => Some(row),
        _ => None,
    }
}

impl ProviderMapper for OpencodeMapper {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn feed(&mut self, raw: &Value, _env: &mut dyn MapEnv) -> Vec<AgentStreamEvent> {
        let payload = row_payload(raw);
        self.discover_session(raw, payload);
        let Some(payload) = payload else {
            return Vec::new();
        };
        let Some(ty) = str_of(payload.get("type")) else {
            return Vec::new();
        };
        if ty == "sync" || ty == "server.heartbeat" {
            return Vec::new();
        }
        match self.state.as_mut() {
            Some(state) => translate_event(payload, state),
            None => Vec::new(),
        }
    }
}

// ---- history conversion ----------------------------------------------------------------------

/// `normalizeProviderReplayTimestamp`.
///
/// PARITY-QUIRK(date-parse): TS accepts any string `Date.parse` understands and returns it
/// untouched; this port accepts ISO-8601 date/date-time strings only. OpenCode reports epoch
/// milliseconds, so strings do not occur in practice.
pub fn normalize_replay_timestamp(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => {
            let t = js_trim(s);
            looks_like_iso(t).then(|| t.to_string())
        }
        Some(Value::Number(n)) => {
            let f = n.as_f64().filter(|f| f.is_finite())?;
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

fn looks_like_iso(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 10
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
        && (b.len() == 10 || b[10] == b'T' || b[10] == b' ')
}

/// `new Date(ms).toISOString()`; `None` for the invalid range (`|ms| > 8.64e15`).
fn iso_from_ms(ms: f64) -> Option<String> {
    if ms.abs() > 8.64e15 {
        return None;
    }
    let ms = ms.trunc() as i64;
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    // civil_from_days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let ys = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{:06}", -year)
    } else {
        format!("+{year:06}")
    };
    Some(format!(
        "{ys}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        (rem / 60_000) % 60,
        (rem / 1000) % 60,
        rem % 1000
    ))
}

fn replay_timestamp(message_info: &Value, part: Option<&Value>) -> Option<String> {
    // `part.time.start ?? part.time.end ?? message.time.created ?? message.time.completed`
    let nn = |v: Option<&Value>| v.filter(|x| !x.is_null()).cloned();
    let ptime = part.and_then(|p| record(p.get("time")));
    let mtime = record(message_info.get("time"));
    let ts = nn(ptime.and_then(|t| t.get("start")))
        .or_else(|| nn(ptime.and_then(|t| t.get("end"))))
        .or_else(|| nn(mtime.and_then(|t| t.get("created"))))
        .or_else(|| nn(mtime.and_then(|t| t.get("completed"))));
    normalize_replay_timestamp(ts.as_ref())
}

fn replay_event(item: AgentTimelineItem, info: &Value, part: Option<&Value>) -> AgentStreamEvent {
    AgentStreamEvent::Timeline {
        item: Box::new(item),
        provider: PROVIDER.into(),
        turn_id: None,
        timestamp: replay_timestamp(info, part),
    }
}

fn replay_part_event(part: &Value, info: &Value) -> Option<AgentStreamEvent> {
    let message_id = str_of(info.get("id")).map(str::to_string);
    let ty = str_of(part.get("type"));
    if ty == Some("text") && truthy(part.get("text")) {
        return Some(replay_event(
            AgentTimelineItem::AssistantMessage {
                text: str_of(part.get("text")).unwrap_or("").to_string(),
                message_id,
            },
            info,
            Some(part),
        ));
    }
    if ty == Some("reasoning") && truthy(part.get("text")) {
        return Some(replay_event(
            AgentTimelineItem::Reasoning {
                text: str_of(part.get("text")).unwrap_or("").to_string(),
            },
            info,
            Some(part),
        ));
    }
    if ty != Some("tool") {
        return None;
    }
    if is_todo_write_part(part) {
        let todos = read_todo_items_from_tool_part(part)?;
        return Some(replay_event(todos_to_item(&todos), info, Some(part)));
    }
    let item = tool_part_to_item(part)?;
    Some(replay_event(
        AgentTimelineItem::tool_call(item),
        info,
        Some(part),
    ))
}

/// `buildOpenCodeReplayTimelineEvents` for one `{info, parts}` message.
pub fn replay_timeline_events(message: &Value) -> Vec<AgentStreamEvent> {
    let Some(info) = message.get("info") else {
        return Vec::new();
    };
    let parts: &[Value] = message
        .get("parts")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice);
    if is_compaction_summary_message(info) {
        return Vec::new();
    }
    if str_of(info.get("role")) == Some("user") {
        let text: String = parts
            .iter()
            .filter(|p| {
                str_of(p.get("type")) == Some("text")
                    && p.get("synthetic") != Some(&Value::Bool(true))
            })
            .map(|p| str_of(p.get("text")).unwrap_or(""))
            .collect();
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![replay_event(
                AgentTimelineItem::UserMessage {
                    text,
                    message_id: str_of(info.get("id")).map(str::to_string),
                    client_message_id: None,
                },
                info,
                None,
            )]
        };
    }
    let mut events = Vec::new();
    let mut emitted_text = false;
    for part in parts {
        if str_of(part.get("type")) == Some("text") && truthy(part.get("text")) {
            emitted_text = true;
        }
        if let Some(e) = replay_part_event(part, info) {
            events.push(e);
        }
    }
    if !emitted_text {
        if let Some(text) = stringify_structured(info.get("structured")) {
            events.push(replay_event(
                AgentTimelineItem::AssistantMessage {
                    text,
                    message_id: str_of(info.get("id")).map(str::to_string),
                },
                info,
                None,
            ));
        }
    }
    events
}

/// `filterOpenCodeRevertedMessages`.
pub fn filter_reverted_messages(messages: &[Value], revert: Option<&Value>) -> Vec<Value> {
    let Some(revert) = record(revert) else {
        return messages.to_vec();
    };
    if !truthy(revert.get("messageID")) || truthy(revert.get("partID")) {
        return messages.to_vec();
    }
    let target = revert.get("messageID").and_then(Value::as_str);
    match messages.iter().position(|m| {
        m.get("info")
            .and_then(|i| i.get("id"))
            .and_then(Value::as_str)
            == target
    }) {
        Some(idx) => messages[..idx].to_vec(),
        None => messages.to_vec(),
    }
}

/// `buildOpenCodeSessionTimeline`.
pub fn build_session_timeline(messages: &[Value]) -> Vec<AgentTimelineItem> {
    let mut out = Vec::new();
    let mut hide_next_assistant = false;
    for message in messages {
        let info = message.get("info");
        let role = str_of(info.and_then(|i| i.get("role")));
        let compaction = message
            .get("parts")
            .and_then(Value::as_array)
            .and_then(|ps| {
                ps.iter()
                    .find(|p| str_of(p.get("type")) == Some("compaction"))
            });
        if role == Some("assistant") && hide_next_assistant {
            hide_next_assistant = false;
            continue;
        }
        if role == Some("user") && compaction.is_none() {
            hide_next_assistant = false;
        }
        out.extend(
            replay_timeline_events(message)
                .into_iter()
                .filter_map(|e| match e {
                    AgentStreamEvent::Timeline { item, .. } => Some(*item),
                    _ => None,
                }),
        );
        if let (Some("user"), Some(c)) = (role, compaction) {
            let trigger = if truthy(c.get("auto")) {
                CompactionTrigger::Auto
            } else {
                CompactionTrigger::Manual
            };
            out.push(compaction_item(CompactionStatus::Completed, Some(trigger)));
            hide_next_assistant = true;
        }
    }
    out
}
