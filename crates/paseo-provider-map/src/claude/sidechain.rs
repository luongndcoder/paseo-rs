//! `ClaudeSidechainTracker` (`claude/sidechain-tracker.ts`) and `subagent-handback.ts`.
//!
//! Frames carrying `parent_tool_use_id` come from inside a subagent. The tracker turns them into
//! `provider_subagent` timeline events for that child, plus the parent's synthetic Task card
//! enriched with the child's action log.

use std::collections::{HashMap, HashSet};

use paseo_tool_detail::detail::claude::{
    map_claude_completed_tool_call, map_claude_failed_tool_call, map_claude_running_tool_call,
    ClaudeToolCallParams,
};
use paseo_tool_detail::js::{js_to_string, js_trim};
use paseo_tool_detail::{build_tool_call_display_model, ToolCallDisplayInput};
use serde_json::{json, Map, Value};

use super::subagents::provider_subagent_event;
use super::text::{js_truthy, obj_get, read_trimmed, str_of};

const MAX_SUB_AGENT_LOG_ENTRIES: usize = 200;
const MAX_SUB_AGENT_SUMMARY_CHARS: usize = 160;
const SUBAGENT_HANDBACK_TOOL_NAME: &str = "SubagentHandback";

pub(super) fn is_handback_tool_name(name: Option<&str>) -> bool {
    name == Some(SUBAGENT_HANDBACK_TOOL_NAME)
}

/// `readClaudeSubagentHandback`: `(callId, report)` for a handback `tool_use` block. The report is
/// `None` while the call is still streaming its input.
fn read_handback(block: &Value) -> Option<(String, Option<String>)> {
    if str_of(block.get("type")) != Some("tool_use")
        || !is_handback_tool_name(str_of(block.get("name")))
    {
        return None;
    }
    let call_id = str_of(block.get("id")).filter(|id| !id.is_empty())?;
    let report = block
        .get("input")
        .and_then(|i| str_of(i.get("message")))
        .unwrap_or("");
    let report = (!js_trim(report).is_empty()).then(|| report.to_owned());
    Some((call_id.to_owned(), report))
}

#[derive(Debug, Clone)]
struct Action {
    tool_name: String,
    input: Option<Value>,
    summary: Option<String>,
}

#[derive(Debug, Default)]
struct SidechainState {
    name: Option<String>,
    sub_agent_type: Option<String>,
    description: Option<String>,
    actions: Vec<Action>,
    action_keys: Vec<String>,
    action_index_by_key: HashMap<String, usize>,
    completed_action_keys: HashSet<String>,
}

struct Candidate {
    key: String,
    tool_name: String,
    input: Option<Value>,
}

/// What the tracker reads from the session around one frame.
pub(super) struct SidechainCtx<'a> {
    /// The parent's Task tool input.
    pub tool_input: Option<&'a Value>,
    /// The task protocol owns subagent descriptors, so the tracker only maps timelines.
    pub descriptor_owned_elsewhere: bool,
    /// The parent's tool call does not already own its card.
    pub needs_parent_card: bool,
}

#[derive(Debug, Default)]
pub(super) struct SidechainTracker {
    /// Insertion ordered (`finishAll` walks them in order).
    active: Vec<(String, SidechainState)>,
}

fn is_tool_use_type(t: Option<&str>) -> bool {
    matches!(t, Some("tool_use" | "mcp_tool_use" | "server_tool_use"))
}

fn is_chunk(v: &Value) -> bool {
    v.as_object()
        .is_some_and(|o| o.get("type").is_some_and(Value::is_string))
}

/// `Claude input ?? null`.
fn non_null(v: Option<&Value>) -> Option<Value> {
    v.filter(|v| !v.is_null()).cloned()
}

fn running(
    name: &str,
    call_id: &str,
    input: Option<&Value>,
) -> Option<paseo_agent_types::ToolCallItem> {
    map_claude_running_tool_call(&ClaudeToolCallParams {
        call_id: Some(call_id),
        name,
        input,
        output: None,
        metadata: None,
    })
}

/// `normalizeSubAgentText`.
fn normalize_text(v: Option<&Value>) -> Option<String> {
    normalize_str(&read_trimmed(v)?)
}

fn normalize_str(s: &str) -> Option<String> {
    let trimmed = js_trim(s);
    if trimmed.is_empty() {
        return None;
    }
    let collapsed = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut units = 0;
    let mut kept = String::new();
    for c in collapsed.chars() {
        units += c.len_utf16();
        if units > MAX_SUB_AGENT_SUMMARY_CHARS {
            return Some(format!("{kept}..."));
        }
        kept.push(c);
    }
    Some(collapsed)
}

fn upsert(id: &str, state: &SidechainState, status: &str) -> Value {
    provider_subagent_event(json!({
        "type": "upsert",
        "id": id,
        "title": state.name.clone().or_else(|| state.sub_agent_type.clone())
            .unwrap_or_else(|| "Claude subagent".to_owned()),
        "description": state.description.clone().map(Value::String).unwrap_or(Value::Null),
        "status": status,
        "toolCallId": id,
    }))
}

impl SidechainTracker {
    fn position(&self, id: &str) -> Option<usize> {
        self.active.iter().position(|(k, _)| k == id)
    }

    pub fn clear(&mut self) {
        self.active.clear();
    }

    pub fn finish_all(&mut self, status: &str, owned_elsewhere: bool) -> Vec<Value> {
        let states = std::mem::take(&mut self.active);
        if owned_elsewhere {
            return Vec::new();
        }
        states.iter().map(|(id, s)| upsert(id, s, status)).collect()
    }

    pub fn finish(&mut self, id: &str, status: &str, owned_elsewhere: bool) -> Vec<Value> {
        let Some(pos) = self.position(id) else {
            return Vec::new();
        };
        let (_, state) = self.active.remove(pos);
        if owned_elsewhere {
            return Vec::new();
        }
        vec![upsert(id, &state, status)]
    }

    pub fn handle_message(
        &mut self,
        message: &Value,
        parent_id: &str,
        ctx: &SidechainCtx<'_>,
    ) -> Vec<Value> {
        let pos = match self.position(parent_id) {
            Some(p) => p,
            None => {
                self.active
                    .push((parent_id.to_owned(), SidechainState::default()));
                self.active.len() - 1
            }
        };
        let state = &mut self.active[pos].1;

        let context_updated = update_context_from_task_input(state, ctx.tool_input);
        let candidates = extract_action_candidates(message)
            .into_iter()
            .filter(|c| !is_handback_tool_name(Some(&c.tool_name)))
            .collect::<Vec<_>>();
        let mut child_items = extract_timeline_items(message, state);
        child_items.extend(extract_tool_results(message, state));
        let mut action_updated = false;
        for action in candidates {
            if state.completed_action_keys.contains(&action.key) {
                continue;
            }
            if append_action(state, &action) {
                action_updated = true;
                if let Some(item) = running(&action.tool_name, &action.key, action.input.as_ref()) {
                    child_items.push(item.to_value());
                }
            }
        }
        if !context_updated && !action_updated && child_items.is_empty() {
            return Vec::new();
        }

        let Some(tool_call) = running("Task", parent_id, None) else {
            return Vec::new();
        };
        let mut detail = Map::new();
        detail.insert("type".into(), json!("sub_agent"));
        if let Some(t) = &state.sub_agent_type {
            detail.insert("subAgentType".into(), json!(t));
        }
        if let Some(d) = &state.description {
            detail.insert("description".into(), json!(d));
        }
        let log = state
            .actions
            .iter()
            .map(|a| match &a.summary {
                Some(s) => format!("[{}] {s}", a.tool_name),
                None => format!("[{}]", a.tool_name),
            })
            .collect::<Vec<_>>()
            .join("\n");
        detail.insert("log".into(), json!(log));
        detail.insert("actions".into(), json!([]));

        let mut events = Vec::new();
        if !ctx.descriptor_owned_elsewhere {
            events.push(provider_subagent_event(json!({
                "type": "upsert",
                "id": parent_id,
                "title": state.name.clone().or_else(|| state.sub_agent_type.clone())
                    .unwrap_or_else(|| "Claude subagent".to_owned()),
                "description": state.description.clone().map(Value::String).unwrap_or(Value::Null),
                "status": "running",
                "toolCallId": parent_id,
            })));
        }
        for item in child_items {
            events.push(provider_subagent_event(
                json!({"type": "timeline", "id": parent_id, "item": item}),
            ));
        }
        if ctx.needs_parent_card {
            let mut card = tool_call.to_value();
            if let Some(o) = card.as_object_mut() {
                o.insert("detail".into(), Value::Object(detail));
            }
            events.push(json!({"type": "timeline", "item": card, "provider": "claude"}));
        }
        events
    }
}

fn update_context_from_task_input(state: &mut SidechainState, task_input: Option<&Value>) -> bool {
    let get = |key: &str| normalize_text(task_input.and_then(|i| i.get(key)));
    let mut changed = false;
    let slots: [(&mut Option<String>, Option<String>); 3] = [
        (&mut state.name, get("name")),
        (&mut state.sub_agent_type, get("subagent_type")),
        (&mut state.description, get("description")),
    ];
    for (slot, next) in slots {
        if let Some(next) = next {
            if slot.as_ref() != Some(&next) {
                *slot = Some(next);
                changed = true;
            }
        }
    }
    changed
}

fn message_content(message: &Value) -> Option<&Vec<Value>> {
    obj_get(message, "message")?.get("content")?.as_array()
}

fn extract_timeline_items(message: &Value, state: &mut SidechainState) -> Vec<Value> {
    if str_of(message.get("type")) != Some("assistant") {
        return Vec::new();
    }
    let Some(content) = message_content(message) else {
        return Vec::new();
    };
    let message_id = read_trimmed(obj_get(message, "message").and_then(|m| m.get("id")));
    let with_id = |text: &str| {
        let mut item = Map::new();
        item.insert("type".into(), json!("assistant_message"));
        item.insert("text".into(), json!(text));
        if let Some(id) = &message_id {
            item.insert("messageId".into(), json!(id));
        }
        Value::Object(item)
    };
    let mut items = Vec::new();
    for block in content {
        if !is_chunk(block) {
            continue;
        }
        match str_of(block.get("type")) {
            Some("text") => {
                if let Some(text) = read_trimmed(block.get("text")) {
                    items.push(with_id(&text));
                }
            }
            Some("thinking") => {
                if let Some(text) = read_trimmed(block.get("thinking")) {
                    items.push(json!({"type": "reasoning", "text": text}));
                }
            }
            _ => {
                let Some((call_id, report)) = read_handback(block) else {
                    continue;
                };
                // The handback's result is only an acknowledgement, never a tool card.
                state.completed_action_keys.insert(call_id);
                if let Some(report) = report {
                    items.push(with_id(&report));
                }
            }
        }
    }
    items
}

fn extract_tool_results(message: &Value, state: &mut SidechainState) -> Vec<Value> {
    let Some(content) = message_content(message) else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for block in content {
        if !is_chunk(block)
            || !str_of(block.get("type")).is_some_and(|t| t.ends_with("tool_result"))
        {
            continue;
        }
        let Some(call_id) = read_trimmed(block.get("tool_use_id")) else {
            continue;
        };
        if state.completed_action_keys.contains(&call_id) {
            continue;
        }
        let action = state
            .action_index_by_key
            .get(&call_id)
            .and_then(|i| state.actions.get(*i));
        let Some(tool_name) = action
            .map(|a| a.tool_name.clone())
            .or_else(|| read_trimmed(block.get("tool_name")))
        else {
            continue;
        };
        let input = action.and_then(|a| a.input.clone());
        let output = non_null(block.get("content"));
        let params = ClaudeToolCallParams {
            call_id: Some(&call_id),
            name: &tool_name,
            input: input.as_ref(),
            output: output.as_ref(),
            metadata: None,
        };
        let is_error = block.get("is_error").is_some_and(js_truthy);
        let tool_call = if is_error {
            map_claude_failed_tool_call(&params, block)
        } else {
            map_claude_completed_tool_call(&params)
        };
        if let Some(tool_call) = tool_call {
            state.completed_action_keys.insert(call_id);
            items.push(tool_call.to_value());
        }
    }
    items
}

fn candidate_from_block(block: &Value, key: String) -> Option<Candidate> {
    Some(Candidate {
        key,
        tool_name: str_of(block.get("name"))?.to_owned(),
        input: non_null(block.get("input")),
    })
}

fn extract_action_candidates(message: &Value) -> Vec<Candidate> {
    match str_of(message.get("type")) {
        Some("assistant") => {
            let Some(content) = message_content(message) else {
                return Vec::new();
            };
            let mut actions: Vec<Candidate> = Vec::new();
            for block in content {
                if !is_chunk(block)
                    || !is_tool_use_type(str_of(block.get("type")))
                    || str_of(block.get("name")).is_none()
                {
                    continue;
                }
                let name = str_of(block.get("name")).unwrap_or_default();
                let key = read_trimmed(block.get("id"))
                    .unwrap_or_else(|| format!("assistant:{name}:{}", actions.len()));
                actions.extend(candidate_from_block(block, key));
            }
            actions
        }
        Some("stream_event") => {
            let Some(event) = message.get("event") else {
                return Vec::new();
            };
            if str_of(event.get("type")) != Some("content_block_start") {
                return Vec::new();
            }
            let Some(block) = event.get("content_block").filter(|b| is_chunk(b)) else {
                return Vec::new();
            };
            if !is_tool_use_type(str_of(block.get("type"))) || str_of(block.get("name")).is_none() {
                return Vec::new();
            }
            let name = str_of(block.get("name")).unwrap_or_default();
            let index = match event.get("index") {
                Some(v) if v.is_number() => js_to_string(v),
                _ => "0".to_owned(),
            };
            let key =
                read_trimmed(block.get("id")).unwrap_or_else(|| format!("stream:{name}:{index}"));
            candidate_from_block(block, key).into_iter().collect()
        }
        Some("tool_progress") => {
            let Some(tool_name) = read_trimmed(message.get("tool_name")) else {
                return Vec::new();
            };
            let key = read_trimmed(message.get("tool_use_id"))
                .unwrap_or_else(|| format!("progress:{tool_name}"));
            vec![Candidate {
                key,
                tool_name,
                input: None,
            }]
        }
        _ => Vec::new(),
    }
}

fn derive_summary(tool_name: &str, input: Option<&Value>) -> Option<String> {
    let call_id = format!("sub-agent-summary-{tool_name}");
    let item = running(tool_name, &call_id, input)?;
    let display = build_tool_call_display_model(&ToolCallDisplayInput::from_item(&item, None));
    normalize_str(display.summary.as_deref()?)
}

fn append_action(state: &mut SidechainState, candidate: &Candidate) -> bool {
    let tool_name = js_trim(&candidate.tool_name);
    if tool_name.is_empty() {
        return false;
    }
    let summary = derive_summary(tool_name, candidate.input.as_ref());
    if let Some(&idx) = state.action_index_by_key.get(&candidate.key) {
        let Some(existing) = state.actions.get(idx) else {
            return false;
        };
        let next_summary = existing.summary.clone().or(summary);
        if existing.tool_name == tool_name && existing.summary == next_summary {
            return false;
        }
        let input = existing.input.clone().or_else(|| candidate.input.clone());
        state.actions[idx] = Action {
            tool_name: tool_name.to_owned(),
            input,
            summary: next_summary,
        };
        return true;
    }
    state.actions.push(Action {
        tool_name: tool_name.to_owned(),
        input: candidate.input.clone(),
        summary,
    });
    state.action_keys.push(candidate.key.clone());
    while state.actions.len() > MAX_SUB_AGENT_LOG_ENTRIES {
        state.actions.remove(0);
        let removed = state.action_keys.remove(0);
        state.completed_action_keys.remove(&removed);
    }
    state.action_index_by_key.clear();
    for (i, key) in state.action_keys.iter().enumerate() {
        state.action_index_by_key.insert(key.clone(), i);
    }
    true
}
