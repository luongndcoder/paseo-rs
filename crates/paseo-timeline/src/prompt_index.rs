//! Prompt index (`timeline-prompt-index.ts`): one preview per user message.

use paseo_agent_types::AgentTimelineItem;
use paseo_tool_detail::js::{is_js_whitespace, js_trim, utf16_len, utf16_prefix};
use serde_json::{json, Value};

use crate::model::{RowView, Seq};

const PROMPT_PREVIEW_MAX_LENGTH: usize = 120;

#[derive(Debug, Clone, PartialEq)]
pub struct PromptIndexEntry {
    pub seq: Seq,
    pub timestamp: String,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PromptIndex {
    pub epoch: String,
    pub prompts: Vec<PromptIndexEntry>,
}

impl PromptIndex {
    pub fn to_value(&self) -> Value {
        json!({
            "epoch": self.epoch,
            "prompts": self.prompts.iter().map(|p| json!({
                "seq": p.seq,
                "timestamp": p.timestamp,
                "preview": p.preview,
            })).collect::<Vec<_>>(),
        })
    }
}

/// `text.replace(/\s+/g, " ").trim()`, then cut to 119 UTF-16 units plus an ellipsis when longer
/// than 120.
///
/// PARITY-QUIRK(surrogate-truncation): the cut can leave a lone high surrogate before the `…`.
fn prompt_preview(text: &str) -> String {
    let mut collapsed = String::with_capacity(text.len());
    let mut in_ws = false;
    for c in text.chars() {
        if is_js_whitespace(c) {
            if !in_ws {
                collapsed.push(' ');
            }
            in_ws = true;
        } else {
            collapsed.push(c);
            in_ws = false;
        }
    }
    let collapsed = js_trim(&collapsed);
    if utf16_len(collapsed) <= PROMPT_PREVIEW_MAX_LENGTH {
        return collapsed.to_string();
    }
    format!(
        "{}…",
        utf16_prefix(collapsed, PROMPT_PREVIEW_MAX_LENGTH - 1)
    )
}

pub fn build_timeline_prompt_index<R: RowView>(epoch: &str, rows: &[R]) -> PromptIndex {
    PromptIndex {
        epoch: epoch.to_string(),
        prompts: rows
            .iter()
            .filter_map(|row| match row.item() {
                AgentTimelineItem::UserMessage { text, .. } => Some(PromptIndexEntry {
                    seq: row.seq(),
                    timestamp: row.timestamp().to_string(),
                    preview: prompt_preview(text),
                }),
                _ => None,
            })
            .collect(),
    }
}
