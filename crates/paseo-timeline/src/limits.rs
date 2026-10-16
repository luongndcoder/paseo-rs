//! Content limits (`agent-timeline-content.ts`). The implementation lives in
//! `paseo-tool-detail` (WP2); this module is the timeline-facing entry point.
//!
//! Applied on record, on dispatch and on hydration. 64 KiB in UTF-16 code units: shell
//! `detail.output` and failed-shell `error.content` are cut with no notice, `plain_text` detail is
//! cut to `65536 - 21` units plus the 21-unit notice. Plugin data is never truncated: JSON over
//! 64 KiB (UTF-8 bytes) is an error.
//!
//! PARITY-QUIRK(surrogate-truncation): a cut can leave a lone high surrogate. It is carried as a
//! private-use scalar (see `paseo_agent_types::js_str`), so every outbound JSON writer that can see
//! a truncated item must use `to_js_json_string`.

use paseo_agent_types::AgentTimelineItem;

pub use paseo_tool_detail::limits::{
    assert_plugin_timeline_data_size, limit_item_content, PLUGIN_TIMELINE_DATA_MAX_BYTES,
    TOOL_CALL_CONTENT_MAX_LENGTH,
};

/// Validate and limit an item before it is recorded: plugin data size check, then content limits.
pub fn prepare_item_for_record(item: AgentTimelineItem) -> Result<AgentTimelineItem, String> {
    if let AgentTimelineItem::Plugin { data, .. } = &item {
        assert_plugin_timeline_data_size(data)?;
    }
    Ok(limit_item_content(item))
}
