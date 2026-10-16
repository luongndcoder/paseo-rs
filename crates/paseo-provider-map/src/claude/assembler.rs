//! `TimelineAssembler` (`claude/agent.ts`): reconciles streamed deltas and absolute assistant
//! messages into non-duplicating `assistant_message` / `reasoning` timeline items, keyed by
//! message id.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use super::text::{
    is_transcript_noise_str, obj_get, read_trimmed, utf16_len, utf16_slice_from,
    INTERRUPT_TOOL_USE_PLACEHOLDER,
};

#[derive(Debug, Default)]
struct MessageState {
    id: String,
    assistant_text: String,
    reasoning_text: String,
    emitted_assistant_len: usize,
    emitted_reasoning_len: usize,
}

#[derive(Debug, Default)]
pub struct TimelineAssembler {
    messages: HashMap<String, MessageState>,
    finalized: HashSet<String>,
    active_by_run: HashMap<String, String>,
    synthetic_counter: u64,
}

enum Fragment {
    Assistant(String),
    Reasoning(String),
}

fn extract_fragments(content: &Value) -> Vec<Fragment> {
    if let Value::String(s) = content {
        return if s.is_empty() {
            Vec::new()
        } else {
            vec![Fragment::Assistant(s.clone())]
        };
    }
    let blocks: Vec<&Value> = match content {
        Value::Array(a) => a.iter().collect(),
        other => vec![other],
    };
    let mut out = Vec::new();
    for block in blocks {
        let Some(o) = block.as_object() else {
            continue;
        };
        let Some(ty) = o.get("type").and_then(Value::as_str) else {
            continue;
        };
        if matches!(ty, "text" | "text_delta") {
            if let Some(t) = o
                .get("text")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                out.push(Fragment::Assistant(t.to_owned()));
            }
        }
        if matches!(ty, "thinking" | "thinking_delta") {
            if let Some(t) = o
                .get("thinking")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                out.push(Fragment::Reasoning(t.to_owned()));
            }
        }
    }
    out
}

fn stream_event_message_id(event: &Value) -> Option<String> {
    read_trimmed(event.get("message_id"))
        .or_else(|| read_trimmed(obj_get(event, "message").and_then(|m| m.get("id"))))
}

impl TimelineAssembler {
    /// `consume`: `message` is the SDK message, `hint` the identifiers' message id.
    pub fn consume(
        &mut self,
        message: &Value,
        run_id: Option<&str>,
        hint: Option<&str>,
    ) -> Vec<Value> {
        match message.get("type").and_then(Value::as_str) {
            Some("assistant") => self.consume_assistant(message, run_id, hint),
            Some("stream_event") => self.consume_stream_event(message, run_id, hint),
            _ => Vec::new(),
        }
    }

    fn consume_assistant(
        &mut self,
        message: &Value,
        run_id: Option<&str>,
        hint: Option<&str>,
    ) -> Vec<Value> {
        let own_id = read_trimmed(message.get("message_id"))
            .or_else(|| read_trimmed(obj_get(message, "message").and_then(|m| m.get("id"))));
        let message_id = own_id
            .or_else(|| hint.map(str::to_owned))
            .or_else(|| self.resolve_message_id(run_id, true, None));
        let Some(message_id) = message_id else {
            return Vec::new();
        };
        if self.finalized.contains(&message_id) {
            return Vec::new();
        }
        self.ensure_state(&message_id, run_id);
        let content = obj_get(message, "message").and_then(|m| m.get("content"));
        let fragments = content.map(extract_fragments).unwrap_or_default();
        let state = self.messages.get_mut(&message_id).expect("ensured");
        Self::apply_absolute(state, fragments)
    }

    fn consume_stream_event(
        &mut self,
        message: &Value,
        run_id: Option<&str>,
        hint: Option<&str>,
    ) -> Vec<Value> {
        let empty = json!({});
        let event = message
            .get("event")
            .filter(|e| e.is_object())
            .unwrap_or(&empty);
        let event_type = read_trimmed(event.get("type"));
        let msg_id = stream_event_message_id(event).or_else(|| hint.map(str::to_owned));
        match event_type.as_deref() {
            Some("message_start") => {
                if let Some(id) = self.resolve_message_id(run_id, true, msg_id.as_deref()) {
                    self.ensure_state(&id, run_id);
                }
                Vec::new()
            }
            Some("message_stop") => match self.resolve_message_id(run_id, false, msg_id.as_deref())
            {
                Some(id) => self.finalize(&id, run_id),
                None => Vec::new(),
            },
            Some("content_block_start") => {
                self.consume_delta(event.get("content_block"), run_id, msg_id.as_deref())
            }
            Some("content_block_delta") => {
                self.consume_delta(event.get("delta"), run_id, msg_id.as_deref())
            }
            _ => Vec::new(),
        }
    }

    fn consume_delta(
        &mut self,
        content: Option<&Value>,
        run_id: Option<&str>,
        hint: Option<&str>,
    ) -> Vec<Value> {
        let fragments = content.map(extract_fragments).unwrap_or_default();
        if fragments.is_empty() {
            return Vec::new();
        }
        let Some(id) = self.resolve_message_id(run_id, true, hint) else {
            return Vec::new();
        };
        self.ensure_state(&id, run_id);
        let state = self.messages.get_mut(&id).expect("ensured");
        for f in fragments {
            match f {
                Fragment::Assistant(t) => state.assistant_text.push_str(&t),
                Fragment::Reasoning(t) => state.reasoning_text.push_str(&t),
            }
        }
        Self::emit_new_content(state)
    }

    fn apply_absolute(state: &mut MessageState, fragments: Vec<Fragment>) -> Vec<Value> {
        let mut assistant = String::new();
        let mut reasoning = String::new();
        for f in &fragments {
            match f {
                Fragment::Assistant(t) => assistant.push_str(t),
                Fragment::Reasoning(t) => reasoning.push_str(t),
            }
        }
        if !assistant.is_empty() {
            if !assistant.starts_with(&state.assistant_text) {
                state.emitted_assistant_len = 0;
            }
            state.assistant_text = assistant;
        }
        if !reasoning.is_empty() {
            if !reasoning.starts_with(&state.reasoning_text) {
                state.emitted_reasoning_len = 0;
            }
            state.reasoning_text = reasoning;
        }
        Self::emit_new_content(state)
    }

    fn finalize(&mut self, id: &str, run_id: Option<&str>) -> Vec<Value> {
        let Some(state) = self.messages.get_mut(id) else {
            return Vec::new();
        };
        let items = Self::emit_new_content(state);
        if let Some(run) = run_id {
            if self.active_by_run.get(run).map(String::as_str) == Some(id) {
                self.active_by_run.remove(run);
            }
        }
        self.finalized.insert(id.to_owned());
        self.messages.remove(id);
        items
    }

    fn emit_new_content(state: &mut MessageState) -> Vec<Value> {
        let mut items = Vec::new();
        let next_assistant = utf16_slice_from(&state.assistant_text, state.emitted_assistant_len);
        if !next_assistant.is_empty()
            && next_assistant != INTERRUPT_TOOL_USE_PLACEHOLDER
            && !is_transcript_noise_str(&next_assistant)
        {
            state.emitted_assistant_len = utf16_len(&state.assistant_text);
            items.push(json!({
                "type": "assistant_message",
                "text": next_assistant,
                "messageId": state.id,
            }));
        }
        let next_reasoning = utf16_slice_from(&state.reasoning_text, state.emitted_reasoning_len);
        if !next_reasoning.is_empty() {
            state.emitted_reasoning_len = utf16_len(&state.reasoning_text);
            items.push(json!({"type": "reasoning", "text": next_reasoning}));
        }
        items
    }

    fn ensure_state(&mut self, id: &str, run_id: Option<&str>) {
        self.messages
            .entry(id.to_owned())
            .or_insert_with(|| MessageState {
                id: id.to_owned(),
                ..MessageState::default()
            });
        if let Some(run) = run_id {
            self.active_by_run.insert(run.to_owned(), id.to_owned());
        }
    }

    fn resolve_message_id(
        &mut self,
        run_id: Option<&str>,
        create: bool,
        message_id: Option<&str>,
    ) -> Option<String> {
        if let Some(id) = message_id {
            return Some(id.to_owned());
        }
        if let Some(run) = run_id {
            if let Some(active) = self.active_by_run.get(run) {
                return Some(active.clone());
            }
        }
        if !create {
            return None;
        }
        self.synthetic_counter += 1;
        let synthetic = format!("synthetic-message-{}", self.synthetic_counter);
        if let Some(run) = run_id {
            self.active_by_run.insert(run.to_owned(), synthetic.clone());
        }
        Some(synthetic)
    }
}
