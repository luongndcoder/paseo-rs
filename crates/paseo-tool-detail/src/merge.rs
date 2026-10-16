//! `mergeToolCallItems` from `timeline-projection.ts`.

use paseo_agent_types::{Map, ToolCallDetail, ToolCallItem, ToolCallStatus};
use serde_json::Value;

fn merge_detail(existing: &ToolCallDetail, incoming: &ToolCallDetail) -> ToolCallDetail {
    if existing.is_unknown() && !incoming.is_unknown() {
        return incoming.clone();
    }
    if incoming.is_unknown() && !existing.is_unknown() {
        return existing.clone();
    }
    incoming.clone()
}

/// Fold a later update for the same tool call into the one it follows:
/// `{...existing, ...incoming, detail, metadata}` plus the status/error rules.
pub fn merge_tool_call_items(existing: &ToolCallItem, incoming: &ToolCallItem) -> ToolCallItem {
    let metadata = match (&existing.metadata, &incoming.metadata) {
        (None, None) => None,
        (a, b) => {
            let mut m = Map::new();
            for src in [a, b].into_iter().flatten() {
                for (k, v) in src {
                    m.insert(k.clone(), v.clone());
                }
            }
            Some(m)
        }
    };

    let mut extra = existing.extra.clone();
    for (k, v) in &incoming.extra {
        extra.insert(k.clone(), v.clone());
    }

    // `...incoming` only overrides keys that are present on it.
    let mut error = incoming.error.clone().or_else(|| existing.error.clone());
    match incoming.status {
        ToolCallStatus::Failed => error = incoming.error.clone(),
        ToolCallStatus::Completed | ToolCallStatus::Canceled => error = Some(Value::Null),
        ToolCallStatus::Running => {
            if incoming.error.is_some() {
                error = incoming.error.clone();
            }
        }
    }

    ToolCallItem {
        call_id: incoming.call_id.clone(),
        name: incoming.name.clone(),
        status: incoming.status,
        detail: merge_detail(&existing.detail, &incoming.detail),
        error,
        metadata,
        agent_message: incoming
            .agent_message
            .clone()
            .or_else(|| existing.agent_message.clone()),
        extra,
    }
}
