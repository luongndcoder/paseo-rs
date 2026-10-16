//! `limitAgentTimelineItemContent` and plugin data size check from `agent-timeline-content.ts`.
//!
//! All lengths are UTF-16 code units, as in TS (`String.length` / `slice`). See
//! `js::utf16_prefix` for the lone-surrogate caveat (PARITY-QUIRK(surrogate-truncation)).

use crate::js::{json_stringify, utf16_len, utf16_prefix};
use paseo_agent_types::js_str::escape_carriers;
use paseo_agent_types::{AgentTimelineItem, ToolCallDetail, ToolCallStatus};
use serde_json::Value;

pub const TOOL_CALL_CONTENT_MAX_LENGTH: usize = 64 * 1024;
pub const PLUGIN_TIMELINE_DATA_MAX_BYTES: usize = 64 * 1024;
const TRUNCATION_NOTICE: &str = "\n\n[Content truncated]";

/// `assertPluginTimelineDataSize`: `Buffer.byteLength(JSON.stringify(data), "utf8")`.
pub fn assert_plugin_timeline_data_size(data: &Value) -> Result<(), String> {
    let bytes = escape_carriers(&json_stringify(data)).len();
    if bytes > PLUGIN_TIMELINE_DATA_MAX_BYTES {
        return Err(format!(
            "Plugin timeline item data exceeds {PLUGIN_TIMELINE_DATA_MAX_BYTES} bytes"
        ));
    }
    Ok(())
}

pub fn limit_item_content(mut item: AgentTimelineItem) -> AgentTimelineItem {
    let AgentTimelineItem::ToolCall(t) = &mut item else {
        return item;
    };

    // limitFailedShellError: failed shell whose error is an object with a long string `content`.
    if t.status == ToolCallStatus::Failed && matches!(t.detail, ToolCallDetail::Shell { .. }) {
        if let Some(Value::Object(err)) = &mut t.error {
            if let Some(Value::String(content)) = err.get("content") {
                if utf16_len(content) > TOOL_CALL_CONTENT_MAX_LENGTH {
                    let cut = utf16_prefix(content, TOOL_CALL_CONTENT_MAX_LENGTH);
                    err.insert("content".into(), Value::String(cut));
                }
            }
        }
    }

    // limitPlainText
    if let ToolCallDetail::PlainText {
        text: Some(text), ..
    } = &mut t.detail
    {
        if utf16_len(text) > TOOL_CALL_CONTENT_MAX_LENGTH {
            let keep = TOOL_CALL_CONTENT_MAX_LENGTH - utf16_len(TRUNCATION_NOTICE);
            *text = format!("{}{}", utf16_prefix(text, keep), TRUNCATION_NOTICE);
        }
    }

    // shell output
    if let ToolCallDetail::Shell {
        output: Some(output),
        ..
    } = &mut t.detail
    {
        if utf16_len(output) > TOOL_CALL_CONTENT_MAX_LENGTH {
            *output = utf16_prefix(output, TOOL_CALL_CONTENT_MAX_LENGTH);
        }
    }
    item
}
