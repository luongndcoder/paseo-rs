//! `normalizeToolCallStatus` from `tool-call-mapper-utils.ts`.

use crate::js::js_trim;
use paseo_agent_types::ToolCallStatus;
use serde_json::Value;

pub type NormalizedToolCallStatus = ToolCallStatus;

const FAILED: [&str; 7] = [
    "failed", "failure", "error", "errored", "rejected", "denied", "declined",
];
const CANCELED: [&str; 4] = ["canceled", "cancelled", "interrupted", "aborted"];
const COMPLETED: [&str; 5] = ["completed", "complete", "done", "success", "succeeded"];

/// `None` models JS `undefined`; `Some(Value::Null)` is `null`. Both count as "no value".
pub fn is_present(v: Option<&Value>) -> bool {
    matches!(v, Some(x) if !x.is_null())
}

pub fn normalize_tool_call_status(
    raw_status: Option<&str>,
    error: Option<&Value>,
    output: Option<&Value>,
) -> NormalizedToolCallStatus {
    if is_present(error) {
        return ToolCallStatus::Failed;
    }
    if let Some(raw) = raw_status {
        let normalized = js_trim(raw).to_lowercase();
        if !normalized.is_empty() {
            if FAILED.contains(&normalized.as_str()) {
                return ToolCallStatus::Failed;
            }
            if CANCELED.contains(&normalized.as_str()) {
                return ToolCallStatus::Canceled;
            }
            if COMPLETED.contains(&normalized.as_str()) {
                return ToolCallStatus::Completed;
            }
            return ToolCallStatus::Running;
        }
    }
    if is_present(output) {
        ToolCallStatus::Completed
    } else {
        ToolCallStatus::Running
    }
}
