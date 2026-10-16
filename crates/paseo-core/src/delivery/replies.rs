//! `isReply` (`owned-subscriptions/replies.ts`): does an outbound message answer the request a
//! handler is running for? Naming is a protocol convention, not proof, so every nonconforming
//! correlated output declares its originating request in the tables below. A test iterates every
//! inbound `*.request` type in the authz table (generated from the protocol union) to keep the
//! classification honest when a message is added.

use serde_json::Value;

use super::types::RequestInfo;

/// Outputs whose name does not follow `<request>` -> `<response>`.
const EXCEPTIONS: &[(&str, &[&str])] = &[
    ("pong", &["ping"]),
    ("agent_archived", &["archive_agent_request"]),
    ("agent_deleted", &["delete_agent_request"]),
    ("agent_permission_resolved", &["agent_permission_response"]),
    ("terminals_changed", &["subscribe_terminals_request"]),
    ("usage.list_reports.update", &["usage.list_reports.request"]),
    ("daemon.update.progress", &["daemon.update.request"]),
    // Transcription belongs to the explicitly retained voice operation, not a request ID.
    ("transcription_result", &[]),
    // This legacy requestId is a producer marker; checkout updates require event demand.
    ("checkout_status_update", &[]),
];

/// `status` payloads that answer a request.
const STATUSES: &[(&str, &[&str])] = &[
    ("agent_created", &["create_agent_request"]),
    (
        "agent_create_failed",
        &["create_agent_request", "import_agent_request"],
    ),
    (
        "agent_resumed",
        &["resume_agent_request", "import_agent_request"],
    ),
    ("agent_refreshed", &["refresh_agent_request"]),
    ("restart_requested", &["restart_server_request"]),
    ("shutdown_requested", &["shutdown_server_request"]),
];

fn declared<'a>(table: &'a [(&str, &[&str])], key: &str) -> Option<&'a [&'a str]> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// The conventional response type of a request type.
pub fn conventional_response_type(request_type: &str) -> String {
    if let Some(prefix) = request_type.strip_suffix(".request") {
        format!("{prefix}.response")
    } else if let Some(prefix) = request_type.strip_suffix("_request") {
        format!("{prefix}_response")
    } else if request_type.contains('/') {
        format!("{request_type}/response")
    } else {
        format!("{request_type}_response")
    }
}

/// `isReply(request, message)`. `message` is the unwrapped session message `{type, payload}`.
pub fn is_reply(request: &RequestInfo, message: &Value) -> bool {
    let Some(request_id) = request.request_id.as_deref() else {
        return false;
    };
    let Some(payload) = message.get("payload") else {
        return false;
    };
    if payload.get("requestId").and_then(Value::as_str) != Some(request_id) {
        return false;
    }
    let Some(message_type) = message.get("type").and_then(Value::as_str) else {
        return false;
    };
    if message_type == "rpc_error" {
        return payload.get("requestType").and_then(Value::as_str) == Some(request.ty.as_str());
    }
    let is_status = message_type == "status";
    let key = if is_status {
        payload
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
    } else {
        message_type
    };
    if let Some(list) = declared(if is_status { STATUSES } else { EXCEPTIONS }, key) {
        return list.contains(&request.ty.as_str());
    }
    if is_status {
        return false;
    }
    message_type == conventional_response_type(&request.ty)
}
