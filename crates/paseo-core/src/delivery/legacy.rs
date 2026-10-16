//! COMPAT(ownedSubscriptions) branches, isolated here so removing legacy clients deletes one file.
//!
//! COMPAT(ownedSubscriptions): added in v0.8.0, remove after 2027-03-09 once client floor >= v0.8.0.

use serde_json::Value;

/// `withSubscriptionId`: `payload.subscriptionId` for `{type,payload}` messages, a top-level key
/// otherwise.
pub(crate) fn with_subscription_id(mut message: Value, subscription_id: &str) -> Value {
    if let Some(obj) = message.as_object_mut() {
        if let Some(Value::Object(payload)) = obj.get_mut("payload") {
            payload.insert("subscriptionId".into(), subscription_id.into());
        } else if obj.contains_key("payload") {
            // A non-object payload is spread by TS into a fresh object holding only the id.
            let mut payload = serde_json::Map::new();
            payload.insert("subscriptionId".into(), subscription_id.into());
            obj.insert("payload".into(), Value::Object(payload));
        } else {
            obj.insert("subscriptionId".into(), subscription_id.into());
        }
    }
    message
}

/// `legacyMessage`: what a client that predates server-assigned subscription ids still parses.
/// `terminals_changed` loses `workspaceId`; the four listed responses lose `subscriptionId`.
pub(crate) fn legacy_message(mut message: Value) -> Value {
    let ty = message
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let strip = match ty.as_str() {
        "terminals_changed" => "workspaceId",
        "subscribe_terminal_response"
        | "session.events.set_subscription.response"
        | "agent.timeline.set_subscription.response"
        | "workspace.label.list.response" => "subscriptionId",
        _ => return message,
    };
    if let Some(Value::Object(payload)) = message.get_mut("payload") {
        payload.shift_remove(strip);
    }
    message
}

/// Message types that keep their `subscriptionId` on a legacy source.
pub(crate) fn legacy_keeps_subscription_id(ty: &str) -> bool {
    ty == "fs.file.update" || ty == "checkout_diff_update"
}
