//! Pure ports of the TS tool-call detail pipeline: status/id helpers, read gutter, Codex shell
//! envelope, per-provider detail parsers, `merge_tool_call_items`, `limit_item_content` and the
//! shared display model. No IO, no async.
//!
//! Parity notes (`PARITY-QUIRK(...)` markers in code): UTF-16 lengths/truncation, JS `trim`/`\s`,
//! `JSON.stringify` number formatting.

pub mod detail;
pub mod display;
pub mod gutter;
pub mod ids;
pub mod js;
pub mod limits;
pub mod merge;
pub mod names;
pub mod shell_output;
pub mod status;

pub use display::{build_tool_call_display_model, ToolCallDisplayInput, ToolCallDisplayModel};
pub use ids::{coerce_tool_call_id, coerce_tool_call_id_parts};
pub use limits::{
    assert_plugin_timeline_data_size, limit_item_content, PLUGIN_TIMELINE_DATA_MAX_BYTES,
    TOOL_CALL_CONTENT_MAX_LENGTH,
};
pub use merge::merge_tool_call_items;
pub use status::{normalize_tool_call_status, NormalizedToolCallStatus};

/// JS number (f64) to a JSON number, integral values as integers.
pub fn num_from_f64(f: f64) -> paseo_agent_types::Num {
    if f.fract() == 0.0 && f.abs() <= 9007199254740991.0 {
        paseo_agent_types::Num::from(f as i64)
    } else {
        paseo_agent_types::Num::from_f64(f).unwrap_or_else(|| paseo_agent_types::Num::from(0))
    }
}
