//! `ClaudeTaskProtocolSource` (`claude/subagents/live-source.ts`), the observation fold
//! (`subagents/observation.ts`), the subtitle builder (`subagents/presentation.ts`) and the
//! workflow result reader (`subagents/workflow-output.ts`).
//!
//! Claude Code announces subagent lifecycle on the SDK stream (`task_started`, `task_updated`,
//! `task_notification`, `task_progress`). The source reads those announcements, remembers the
//! task id to subagent id routing for the session and emits provider-neutral observations.

use std::collections::{HashMap, HashSet};

use paseo_tool_detail::js::{js_to_string, json_stringify_pretty, utf16_len};
use serde_json::{json, Map, Value};

use super::models;
use super::text::{obj_get, read_trimmed, str_of};

/// The parent's Task tool input by tool_use id (`getToolInput`): the Task call may carry an
/// explicit `name` that wins over the announced subagent type.
pub(super) type ToolInput<'a> = dyn Fn(&str) -> Option<Value> + 'a;

const SUBAGENT_TASK_TYPE: &str = "local_agent";
const WORKFLOW_TASK_TYPE: &str = "local_workflow";

const MAX_WORKFLOW_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_WORKFLOW_RESULT_CHARS: usize = 100_000;

/// `SubagentObservation`.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Observation {
    Declared {
        id: String,
        title: Option<String>,
        description: Option<String>,
        tool_call_id: Option<String>,
        parent_subagent_id: Option<String>,
    },
    Status {
        id: String,
        status: &'static str,
    },
    Subtitle {
        id: String,
        subtitle: String,
    },
    Timeline {
        id: String,
        item: Value,
    },
}

/// `foldSubagentObservations` wrapped as `provider_subagent` stream events.
pub(super) fn fold_observations(observations: &[Observation]) -> Vec<Value> {
    observations
        .iter()
        .map(|o| provider_subagent_event(fold_one(o)))
        .collect()
}

pub(super) fn provider_subagent_event(event: Value) -> Value {
    json!({"type": "provider_subagent", "provider": "claude", "event": event})
}

fn fold_one(observation: &Observation) -> Value {
    match observation {
        Observation::Timeline { id, item } => {
            json!({"type": "timeline", "id": id, "item": item})
        }
        Observation::Subtitle { id, subtitle } => {
            json!({"type": "upsert", "id": id, "subtitle": subtitle})
        }
        Observation::Status { id, status } => {
            json!({"type": "upsert", "id": id, "status": status})
        }
        Observation::Declared {
            id,
            title,
            description,
            tool_call_id,
            parent_subagent_id,
        } => {
            let mut m = Map::new();
            m.insert("type".into(), json!("upsert"));
            m.insert("id".into(), json!(id));
            m.insert("status".into(), json!("running"));
            if let Some(t) = title {
                m.insert("title".into(), json!(t));
            }
            if let Some(d) = description {
                m.insert("description".into(), json!(d));
            }
            if let Some(t) = tool_call_id {
                m.insert("toolCallId".into(), json!(t));
            }
            if let Some(p) = parent_subagent_id {
                m.insert("parentSubagentId".into(), json!(p));
            }
            Value::Object(m)
        }
    }
}

// ---- presentation ----------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct Presentation {
    title: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    total_tokens: Option<f64>,
}

#[derive(Default)]
struct PresentationPatch {
    model: Option<String>,
    effort: Option<String>,
    total_tokens: Option<f64>,
}

fn format_effort(effort: &str) -> String {
    if effort == "xhigh" {
        return "Extra High".to_owned();
    }
    effort
        .split(|c: char| c == '-' || c == '_' || c.is_whitespace())
        .filter(|p| !p.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_tokens(total: f64) -> Option<String> {
    if total <= 0.0 {
        return None;
    }
    if total < 1000.0 {
        return Some(format!("{} tokens", (total + 0.5).floor() as i64));
    }
    let tenths = ((total / 100.0) + 0.5).floor() / 10.0;
    let text = if tenths.fract() == 0.0 {
        format!("{}", tenths as i64)
    } else {
        format!("{tenths}")
    };
    Some(format!("{text}k tokens"))
}

/// `buildClaudeSubagentSubtitle`.
fn build_subtitle(facts: &Presentation) -> Option<String> {
    let trimmed = |v: &Option<String>| {
        v.as_deref()
            .map(|s| paseo_tool_detail::js::js_trim(s).to_owned())
            .filter(|s| !s.is_empty())
    };
    let mut parts: Vec<String> = Vec::new();
    parts.extend(trimmed(&facts.title));
    if let Some(model) = trimmed(&facts.model) {
        parts.push(
            models::find_model_label(&model)
                .map(str::to_owned)
                .unwrap_or(model),
        );
    }
    if let Some(effort) = trimmed(&facts.effort) {
        parts.push(format_effort(&effort));
    }
    if let Some(tokens) = facts.total_tokens.and_then(format_tokens) {
        parts.push(tokens);
    }
    (!parts.is_empty()).then(|| parts.join(" \u{b7} "))
}

// ---- task status ------------------------------------------------------------------------------

/// `mapTaskStatus`. `killed` is a cancellation, not a failure.
fn map_task_status(status: Option<&str>) -> Option<&'static str> {
    match status? {
        "pending" | "running" | "paused" => Some("running"),
        "completed" => Some("completed"),
        "failed" => Some("failed"),
        "killed" | "stopped" => Some("canceled"),
        _ => None,
    }
}

/// `isProviderSubagentTask`.
fn is_provider_subagent_task(message: &Value) -> bool {
    if let Some(task_type) = str_of(message.get("task_type")).filter(|t| !t.is_empty()) {
        return task_type == SUBAGENT_TASK_TYPE || task_type == WORKFLOW_TASK_TYPE;
    }
    read_trimmed(message.get("subagent_type")).is_some()
}

fn usage_total_tokens(usage: Option<&Value>) -> Option<f64> {
    let total = usage?.get("total_tokens")?;
    total.is_number().then(|| total.as_f64()).flatten()
}

// ---- the source -------------------------------------------------------------------------------

#[derive(Debug, Default)]
pub(super) struct TaskProtocolSource {
    subagent_id_by_task_id: HashMap<String, String>,
    canonical_id_by_tool_use_id: HashMap<String, String>,
    owner_by_tool_use_id: HashMap<String, String>,
    owner_by_task_id: HashMap<String, String>,
    declared_ids: HashSet<String>,
    /// Insertion order of `declared_ids` (a JS `Set` iterates in insertion order).
    declared_order: Vec<String>,
    workflow_task_ids: HashSet<String>,
    last_workflow_result: HashMap<String, String>,
    ids_with_parent_card: HashSet<String>,
    backgrounded_ids: HashSet<String>,
    last_status: HashMap<String, &'static str>,
    presentation: HashMap<String, Presentation>,
    last_subtitle: HashMap<String, String>,
    saw_task_started: bool,
    saw_any_task: bool,
}

impl TaskProtocolSource {
    /// `isActive`: this session announced at least one subagent task.
    pub fn is_active(&self) -> bool {
        self.saw_task_started
    }

    /// `announcesTasks`: this session announced any task at all, including filtered ones.
    pub fn announces_tasks(&self) -> bool {
        self.saw_any_task
    }

    pub fn resolve_subagent_id(&self, tool_use_id: &str) -> Option<String> {
        let canonical = self.canonical_id_by_tool_use_id.get(tool_use_id)?;
        self.declared_ids
            .contains(canonical)
            .then(|| canonical.clone())
    }

    pub fn is_declared_task(&self, task_id: &str) -> bool {
        self.subagent_id_by_task_id
            .get(task_id)
            .is_some_and(|id| self.declared_ids.contains(id))
    }

    pub fn resolve_task_owner(&self, task_id: &str, tool_use_id: Option<&str>) -> Option<String> {
        self.owner_by_task_id
            .get(task_id)
            .or_else(|| tool_use_id.and_then(|t| self.owner_by_tool_use_id.get(t)))
            .cloned()
    }

    pub fn needs_synthetic_parent_tool_card(&self, subagent_id: &str) -> bool {
        !self.ids_with_parent_card.contains(subagent_id)
    }

    pub fn observe(&mut self, message: &Value, tool_input: &ToolInput<'_>) -> Vec<Observation> {
        if str_of(message.get("type")) != Some("system") {
            return Vec::new();
        }
        match str_of(message.get("subtype")) {
            Some("task_started") => self.observe_task_started(message, tool_input),
            Some("task_updated") => self.observe_task_updated(message),
            Some("task_notification") => self.observe_task_notification(message),
            Some("task_progress") => {
                let Some(task_id) = str_of(message.get("task_id")) else {
                    return Vec::new();
                };
                self.observe_usage(task_id, message.get("usage"))
            }
            _ => Vec::new(),
        }
    }

    /// `reset`: session teardown only. A turn ending is not a reason to forget the session.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// `cancelRunningForegroundTasks`.
    pub fn cancel_running_foreground_tasks(&mut self) -> Vec<Observation> {
        let mut out = Vec::new();
        for id in self.declared_order.clone() {
            if self.backgrounded_ids.contains(&id) {
                continue;
            }
            if self.last_status.get(&id) != Some(&"running") {
                continue;
            }
            self.last_status.insert(id.clone(), "canceled");
            out.push(Observation::Status {
                id,
                status: "canceled",
            });
        }
        out
    }

    /// `failRunningTasks`: a lost Claude process terminates every task it owned.
    #[allow(dead_code)]
    pub fn fail_running_tasks(&mut self) -> Vec<Observation> {
        let mut out = Vec::new();
        for id in self.declared_order.clone() {
            if self.last_status.get(&id) != Some(&"running") {
                continue;
            }
            self.last_status.insert(id.clone(), "failed");
            out.push(Observation::Status {
                id,
                status: "failed",
            });
        }
        out
    }

    fn record_backgrounded(&mut self, id: &str, backgrounded: Option<&Value>) {
        match backgrounded {
            Some(Value::Bool(true)) => {
                self.backgrounded_ids.insert(id.to_owned());
            }
            Some(Value::Bool(false)) => {
                self.backgrounded_ids.remove(id);
            }
            _ => {}
        }
    }

    fn observe_task_started(
        &mut self,
        message: &Value,
        tool_input: &ToolInput<'_>,
    ) -> Vec<Observation> {
        self.saw_any_task = true;
        let task_id = str_of(message.get("task_id"))
            .unwrap_or("undefined")
            .to_owned();
        let id = read_trimmed(message.get("tool_use_id"));
        let parent_subagent_id = id
            .as_ref()
            .and_then(|id| self.owner_by_tool_use_id.get(id).cloned());
        if let Some(parent) = &parent_subagent_id {
            self.owner_by_task_id
                .insert(task_id.clone(), parent.clone());
        }
        let Some(id) = id else {
            return Vec::new();
        };
        if message.get("skip_transcript") == Some(&Value::Bool(true))
            || !is_provider_subagent_task(message)
        {
            return Vec::new();
        }
        self.saw_task_started = true;
        let existing = self.subagent_id_by_task_id.get(&task_id).cloned();
        let backgrounded_id = existing.clone().unwrap_or_else(|| id.clone());
        self.record_backgrounded(&backgrounded_id, message.get("is_backgrounded"));
        match existing {
            Some(existing) => self.observe_existing_task_start(message, id, existing),
            None => {
                self.observe_new_task_start(message, &task_id, id, parent_subagent_id, tool_input)
            }
        }
    }

    fn observe_existing_task_start(
        &mut self,
        message: &Value,
        tool_use_id: String,
        existing_id: String,
    ) -> Vec<Observation> {
        self.canonical_id_by_tool_use_id
            .insert(tool_use_id, existing_id.clone());
        let mut out = Vec::new();
        if self.last_status.get(&existing_id) != Some(&"running") {
            self.last_status.insert(existing_id.clone(), "running");
            out.push(Observation::Status {
                id: existing_id.clone(),
                status: "running",
            });
        }
        let prompt = if str_of(message.get("task_type")) == Some(WORKFLOW_TASK_TYPE) {
            read_trimmed(message.get("description"))
        } else {
            read_trimmed(message.get("prompt"))
        };
        if let Some(text) = prompt {
            out.push(Observation::Timeline {
                id: existing_id,
                item: json!({"type": "user_message", "text": text}),
            });
        }
        out
    }

    fn observe_new_task_start(
        &mut self,
        message: &Value,
        task_id: &str,
        id: String,
        parent_subagent_id: Option<String>,
        tool_input: &ToolInput<'_>,
    ) -> Vec<Observation> {
        self.subagent_id_by_task_id
            .insert(task_id.to_owned(), id.clone());
        self.canonical_id_by_tool_use_id
            .insert(id.clone(), id.clone());
        if self.declared_ids.insert(id.clone()) {
            self.declared_order.push(id.clone());
        }
        self.last_status.insert(id.clone(), "running");

        let is_workflow = str_of(message.get("task_type")) == Some(WORKFLOW_TASK_TYPE);
        if is_workflow || parent_subagent_id.is_some() {
            self.ids_with_parent_card.insert(id.clone());
        }
        if is_workflow {
            self.workflow_task_ids.insert(task_id.to_owned());
        }
        let title = if is_workflow {
            Some("Workflow".to_owned())
        } else {
            read_trimmed(tool_input(&id).as_ref().and_then(|v| v.get("name")))
                .or_else(|| read_trimmed(message.get("subagent_type")))
        };
        let description = read_trimmed(message.get("description"));
        let mut out = vec![Observation::Declared {
            id: id.clone(),
            title: title.clone(),
            description: description.clone(),
            tool_call_id: Some(id.clone()),
            parent_subagent_id,
        }];
        let initial = Presentation {
            title: title.clone(),
            ..Default::default()
        };
        if let Some(subtitle) = build_subtitle(&initial) {
            self.last_subtitle.insert(id.clone(), subtitle);
        }
        self.presentation.insert(id.clone(), initial);

        let prompt = if is_workflow {
            description
        } else {
            read_trimmed(message.get("prompt"))
        };
        if let Some(text) = prompt {
            out.push(Observation::Timeline {
                id,
                item: json!({"type": "user_message", "text": text}),
            });
        }
        out
    }

    fn observe_task_updated(&mut self, message: &Value) -> Vec<Observation> {
        let Some(task_id) = str_of(message.get("task_id")) else {
            return Vec::new();
        };
        let patch = message.get("patch");
        if let Some(id) = self.subagent_id_by_task_id.get(task_id).cloned() {
            self.record_backgrounded(&id, patch.and_then(|p| p.get("is_backgrounded")));
        }
        self.observe_status(task_id, patch.and_then(|p| str_of(p.get("status"))))
    }

    fn observe_task_notification(&mut self, message: &Value) -> Vec<Observation> {
        let Some(task_id) = str_of(message.get("task_id")) else {
            return Vec::new();
        };
        let mut out = self.observe_workflow_result(task_id, message);
        out.extend(self.observe_usage(task_id, message.get("usage")));
        out.extend(self.observe_status(task_id, str_of(message.get("status"))));
        out
    }

    fn observe_workflow_result(&mut self, task_id: &str, message: &Value) -> Vec<Observation> {
        if !self.workflow_task_ids.contains(task_id) {
            return Vec::new();
        }
        let id = self.subagent_id_by_task_id.get(task_id).cloned();
        let output_file = read_trimmed(message.get("output_file"));
        let (Some(id), Some(output_file)) = (id, output_file) else {
            return Vec::new();
        };
        let Some(text) = read_workflow_result_file(&output_file) else {
            return Vec::new();
        };
        if self.last_workflow_result.get(task_id) == Some(&text) {
            return Vec::new();
        }
        self.last_workflow_result
            .insert(task_id.to_owned(), text.clone());
        vec![Observation::Timeline {
            id,
            item: json!({"type": "assistant_message", "text": text}),
        }]
    }

    fn observe_usage(&mut self, task_id: &str, usage: Option<&Value>) -> Vec<Observation> {
        let id = self.subagent_id_by_task_id.get(task_id).cloned();
        let tokens = usage_total_tokens(usage);
        let (Some(id), Some(total_tokens)) = (id, tokens) else {
            return Vec::new();
        };
        self.update_presentation(
            &id,
            PresentationPatch {
                total_tokens: Some(total_tokens),
                ..Default::default()
            },
        )
    }

    /// `observeHook`: effort as reported by a hook firing inside a subagent. The golden driver
    /// has no hooks; the entry point exists for the live session wiring.
    #[allow(dead_code)]
    pub fn observe_hook(
        &mut self,
        agent_id: Option<&str>,
        effort: Option<&str>,
    ) -> Vec<Observation> {
        let task_id = agent_id
            .map(paseo_tool_detail::js::js_trim)
            .filter(|s| !s.is_empty());
        let effort = effort
            .map(paseo_tool_detail::js::js_trim)
            .filter(|s| !s.is_empty());
        let (Some(task_id), Some(effort)) = (task_id, effort) else {
            return Vec::new();
        };
        let Some(id) = self.subagent_id_by_task_id.get(task_id).cloned() else {
            return Vec::new();
        };
        self.update_presentation(
            &id,
            PresentationPatch {
                effort: Some(effort.to_owned()),
                ..Default::default()
            },
        )
    }

    /// `observeSidechainFrame`: records which child owns the tool calls it makes and reads the
    /// model off its own assistant frames.
    pub fn observe_sidechain_frame(
        &mut self,
        message: &Value,
        subagent_id: &str,
    ) -> Vec<Observation> {
        if str_of(message.get("type")) != Some("assistant")
            || !self.declared_ids.contains(subagent_id)
        {
            return Vec::new();
        }
        let inner = obj_get(message, "message");
        if let Some(Value::Array(content)) = inner.and_then(|m| m.get("content")) {
            for block in content {
                if !block.is_object() {
                    continue;
                }
                if str_of(block.get("type")) == Some("tool_use") {
                    if let Some(tool_id) = str_of(block.get("id")) {
                        self.owner_by_tool_use_id
                            .insert(tool_id.to_owned(), subagent_id.to_owned());
                    }
                }
            }
        }
        let Some(model) = models::resolve_observed_model_id(
            inner.and_then(|m| m.get("model")).and_then(Value::as_str),
        ) else {
            return Vec::new();
        };
        self.update_presentation(
            subagent_id,
            PresentationPatch {
                model: Some(model),
                ..Default::default()
            },
        )
    }

    fn update_presentation(&mut self, id: &str, patch: PresentationPatch) -> Vec<Observation> {
        let mut next = self.presentation.get(id).cloned().unwrap_or_default();
        if patch.model.is_some() {
            next.model = patch.model;
        }
        if patch.effort.is_some() {
            next.effort = patch.effort;
        }
        if patch.total_tokens.is_some() {
            next.total_tokens = patch.total_tokens;
        }
        let subtitle = build_subtitle(&next);
        self.presentation.insert(id.to_owned(), next);
        let Some(subtitle) = subtitle else {
            return Vec::new();
        };
        if self.last_subtitle.get(id) == Some(&subtitle) {
            return Vec::new();
        }
        self.last_subtitle.insert(id.to_owned(), subtitle.clone());
        vec![Observation::Subtitle {
            id: id.to_owned(),
            subtitle,
        }]
    }

    /// Status is routed only through a task this source declared: a status without a
    /// declaration describes nothing.
    fn observe_status(&mut self, task_id: &str, raw: Option<&str>) -> Vec<Observation> {
        let Some(id) = self.subagent_id_by_task_id.get(task_id).cloned() else {
            return Vec::new();
        };
        let Some(status) = map_task_status(raw) else {
            return Vec::new();
        };
        if self.last_status.get(&id) == Some(&status) {
            return Vec::new();
        }
        self.last_status.insert(id.clone(), status);
        vec![Observation::Status { id, status }]
    }
}

// ---- workflow result ----------------------------------------------------------------------------

/// `readClaudeWorkflowResultFile`.
fn read_workflow_result_file(path: &str) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_WORKFLOW_OUTPUT_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() > MAX_WORKFLOW_OUTPUT_BYTES {
        return None;
    }
    parse_workflow_result(&String::from_utf8_lossy(&buf))
}

/// `parseClaudeWorkflowResult`.
pub(super) fn parse_workflow_result(contents: &str) -> Option<String> {
    if contents.len() > MAX_WORKFLOW_OUTPUT_BYTES {
        return None;
    }
    let parsed: Value = serde_json::from_str(contents).ok()?;
    let record = parsed.as_object()?;
    format_workflow_result(record.get("result")?)
}

fn unwrap_single_value(value: &Value) -> &Value {
    let mut current = value;
    for _ in 0..8 {
        match current.as_object() {
            Some(record) if record.len() == 1 => {
                current = record.values().next().expect("one key");
            }
            _ => return current,
        }
    }
    current
}

/// `formatClaudeWorkflowResult`.
fn format_workflow_result(result: &Value) -> Option<String> {
    let text = match unwrap_single_value(result) {
        Value::String(s) => paseo_tool_detail::js::js_trim(s).to_owned(),
        v @ (Value::Number(_) | Value::Bool(_)) => js_to_string(v),
        Value::Null => return None,
        other => format!("```json\n{}\n```", json_stringify_pretty(other)),
    };
    if text.is_empty() {
        return None;
    }
    if utf16_len(&text) <= MAX_WORKFLOW_RESULT_CHARS {
        return Some(text);
    }
    let mut kept = String::new();
    let mut units = 0;
    for c in text.chars() {
        units += c.len_utf16();
        if units > MAX_WORKFLOW_RESULT_CHARS {
            break;
        }
        kept.push(c);
    }
    Some(format!("{kept}\n\n\u{2026}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_result_unwraps_single_key_objects_and_formats_values() {
        assert_eq!(
            parse_workflow_result(r#"{"result": {"a": {"b": "  hi  "}}}"#).as_deref(),
            Some("hi")
        );
        assert_eq!(
            parse_workflow_result(r#"{"result": 3}"#).as_deref(),
            Some("3")
        );
        assert_eq!(
            parse_workflow_result(r#"{"result": {"a": 1, "b": 2}}"#).as_deref(),
            Some("```json\n{\n  \"a\": 1,\n  \"b\": 2\n}\n```")
        );
        assert_eq!(parse_workflow_result(r#"{"result": null}"#), None);
        assert_eq!(parse_workflow_result(r#"{"other": 1}"#), None);
        assert_eq!(parse_workflow_result("not json"), None);
    }

    #[test]
    fn subtitle_joins_title_model_effort_and_tokens() {
        let facts = Presentation {
            title: Some("Explore".into()),
            model: Some("claude-opus-4-7".into()),
            effort: Some("xhigh".into()),
            total_tokens: Some(1500.0),
        };
        assert_eq!(
            build_subtitle(&facts).as_deref(),
            Some("Explore \u{b7} Opus 4.7 \u{b7} Extra High \u{b7} 1.5k tokens")
        );
        assert_eq!(format_tokens(950.0).as_deref(), Some("950 tokens"));
        assert_eq!(format_tokens(2000.0).as_deref(), Some("2k tokens"));
        assert_eq!(format_effort("very-high"), "Very High");
    }
}
