//! `agent.skills.*` response payloads, key-for-key what `session.ts` emits:
//! `{requestId, ...status}` and `{requestId, ...saveResult}`.

use crate::controller::{ImportLegacyResult, SkillsSaveResult, SkillsSnapshot};
use serde::Serialize;
use serde_json::{Map, Value};

fn with_request_id<T: Serialize>(request_id: &str, body: &T) -> Value {
    let mut out = Map::new();
    out.insert("requestId".into(), Value::String(request_id.to_string()));
    if let Ok(Value::Object(fields)) = serde_json::to_value(body) {
        out.extend(fields);
    }
    Value::Object(out)
}

fn envelope(kind: &str, payload: Value) -> Value {
    let mut out = Map::new();
    out.insert("type".into(), Value::String(kind.to_string()));
    out.insert("payload".into(), payload);
    Value::Object(out)
}

/// `agent.skills.{get_status,reconcile,uninstall}.response`.
pub fn status_response(kind: &str, request_id: &str, snapshot: &SkillsSnapshot) -> Value {
    envelope(kind, with_request_id(request_id, snapshot))
}

pub fn save_selection_response(request_id: &str, result: &SkillsSaveResult) -> Value {
    envelope(
        "agent.skills.save_selection.response",
        with_request_id(request_id, result),
    )
}

pub fn import_legacy_selection_response(request_id: &str, result: &ImportLegacyResult) -> Value {
    envelope(
        "agent.skills.import_legacy_selection.response",
        with_request_id(request_id, result),
    )
}
