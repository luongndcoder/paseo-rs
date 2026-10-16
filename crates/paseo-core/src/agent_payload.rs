//! `AgentSnapshotPayload` builders (`agent-projections.ts`): the wire form of a live agent
//! (`toAgentPayload`), of a stored-only agent (`buildStoredAgentPayload`) and the store
//! enrichment of both (`enrichAgentPayload`).
//!
//! Key order follows the TS object literals so frames compare equal to TS output. MCP server
//! credentials never reach the payload: they live in `config.mcpServers` and in persistence
//! metadata, and `projectPersistenceHandleForWire` strips the latter.

use paseo_agent_types::{AgentRuntimeInfo, PersistenceHandle};
use paseo_store::agent_record::PersistenceHandle as StoredHandle;
use paseo_store::time::parse_js_date;
use paseo_store::{AgentLifecycle, Nullable, StoredAgentRecord};
use serde_json::{json, Map, Value};

use crate::model::{AttentionState, ManagedAgentView};
use crate::projection::{resolve_stored_agent_updated_at, sanitize_json, sanitize_metadata};
use crate::time::iso8601_from_millis;

pub fn lifecycle_str(status: AgentLifecycle) -> &'static str {
    match status {
        AgentLifecycle::Initializing => "initializing",
        AgentLifecycle::Idle => "idle",
        AgentLifecycle::Running => "running",
        AgentLifecycle::Error => "error",
        AgentLifecycle::Closed => "closed",
    }
}

fn normalize_thinking(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// `resolveEffectiveThinkingOptionId`. A runtime info that carries the `thinkingOptionId` key
/// (even as null) wins over the configured value.
pub fn resolve_effective_thinking_option_id(
    runtime_thinking: Option<&Option<String>>,
    configured: Option<&str>,
) -> Option<String> {
    match runtime_thinking {
        Some(runtime) => normalize_thinking(runtime.as_deref()),
        None => normalize_thinking(configured),
    }
}

fn opt_string(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |s| Value::String(s.to_owned()))
}

fn sanitize_runtime_info(info: &AgentRuntimeInfo) -> Value {
    let mut out = info.clone();
    out.extra = info.extra.as_ref().and_then(sanitize_metadata);
    out.to_value()
}

fn drop_mcp_servers(metadata: &mut Map<String, Value>) -> bool {
    metadata.shift_remove("mcpServers");
    metadata.is_empty()
}

/// `projectPersistenceHandleForWire(sanitizePersistenceHandle(handle))`.
fn wire_handle(
    provider: &str,
    session_id: &str,
    native_handle: Option<Value>,
    metadata: Option<&Map<String, Value>>,
) -> Value {
    let mut out = Map::new();
    out.insert("provider".into(), provider.into());
    out.insert("sessionId".into(), session_id.into());
    if let Some(native) = native_handle {
        out.insert("nativeHandle".into(), native);
    }
    if let Some(mut metadata) = metadata.and_then(sanitize_metadata) {
        if !drop_mcp_servers(&mut metadata) {
            out.insert("metadata".into(), Value::Object(metadata));
        }
    }
    Value::Object(out)
}

fn persistence_for_wire(handle: Option<&PersistenceHandle>) -> Value {
    match handle {
        None => Value::Null,
        Some(h) => wire_handle(
            &h.provider,
            &h.session_id,
            h.native_handle.clone().map(Value::String),
            h.metadata.as_ref(),
        ),
    }
}

fn sanitize_usage(usage: &paseo_agent_types::AgentUsage) -> Option<Value> {
    let value = usage.to_value();
    value
        .as_object()
        .filter(|o| !o.is_empty())
        .map(|_| value.clone())
}

/// `sanitizePendingPermissions`: a request with `input`, `suggestions` and `metadata` reduced
/// to non-empty sanitized JSON (empty ones disappear, as `undefined` does in TS).
fn sanitize_permission(request: &paseo_agent_types::PermissionRequest) -> Value {
    let mut value = request.to_value();
    let Some(map) = value.as_object_mut() else {
        return value;
    };
    for key in ["input", "metadata"] {
        if let Some(Value::Object(inner)) = map.get(key) {
            match sanitize_metadata(inner) {
                Some(clean) => {
                    map.insert(key.into(), Value::Object(clean));
                }
                None => {
                    map.shift_remove(key);
                }
            }
        }
    }
    if let Some(Value::Array(items)) = map.get("suggestions") {
        let clean: Vec<Value> = items
            .iter()
            .filter_map(|item| match item {
                Value::Object(o) => sanitize_metadata(o).map(Value::Object),
                _ => None,
            })
            .collect();
        if clean.is_empty() {
            map.shift_remove("suggestions");
        } else {
            map.insert("suggestions".into(), Value::Array(clean));
        }
    }
    value
}

fn iso(ms: i64) -> String {
    iso8601_from_millis(ms)
}

/// `toAgentPayload`. Title and `archivedAt` are filled by [`enrich_agent_payload`].
pub fn to_agent_payload(agent: &ManagedAgentView) -> Value {
    let runtime_info = agent.runtime_info.as_ref();
    let thinking_option_id = agent.config.thinking_option_id.as_deref();
    let effective = resolve_effective_thinking_option_id(
        runtime_info.and_then(|r| r.thinking_option_id.as_ref()),
        thinking_option_id,
    );
    let mut p = Map::new();
    p.insert("id".into(), agent.id.clone().into());
    p.insert("provider".into(), agent.provider.clone().into());
    p.insert("cwd".into(), agent.cwd.clone().into());
    if let Some(workspace_id) = agent.workspace_id.as_ref().filter(|s| !s.is_empty()) {
        p.insert("workspaceId".into(), workspace_id.clone().into());
    }
    p.insert("model".into(), opt_string(agent.config.model.as_deref()));
    p.insert("thinkingOptionId".into(), opt_string(thinking_option_id));
    p.insert(
        "effectiveThinkingOptionId".into(),
        opt_string(effective.as_deref()),
    );
    if let Some(info) = runtime_info {
        p.insert("runtimeInfo".into(), sanitize_runtime_info(info));
    }
    p.insert("createdAt".into(), iso(agent.created_at_ms).into());
    p.insert("updatedAt".into(), iso(agent.updated_at_ms).into());
    p.insert(
        "lastUserMessageAt".into(),
        agent
            .last_user_message_at_ms
            .map_or(Value::Null, |ms| iso(ms).into()),
    );
    p.insert("status".into(), lifecycle_str(agent.lifecycle).into());
    p.insert(
        "activeTurn".into(),
        match &agent.active_turn_id {
            Some(turn_id) => json!({
                "turnId": turn_id,
                "startedAt": agent.active_turn_started_at_ms.map_or(Value::Null, |ms| iso(ms).into()),
            }),
            None => Value::Null,
        },
    );
    p.insert(
        "capabilities".into(),
        Value::Object(agent.capabilities.clone()),
    );
    p.insert(
        "currentModeId".into(),
        opt_string(agent.current_mode_id.as_deref()),
    );
    p.insert(
        "availableModes".into(),
        Value::Array(agent.available_modes.iter().map(|m| m.to_value()).collect()),
    );
    p.insert("features".into(), Value::Array(agent.features.clone()));
    p.insert(
        "pendingPermissions".into(),
        Value::Array(
            agent
                .pending_permissions
                .values()
                .map(sanitize_permission)
                .collect(),
        ),
    );
    p.insert(
        "persistence".into(),
        persistence_for_wire(agent.persistence.as_ref()),
    );
    p.insert("title".into(), Value::Null);
    p.insert(
        "labels".into(),
        Value::Object(
            agent
                .labels
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        ),
    );
    if agent.internal {
        p.insert("internal".into(), true.into());
    }
    if let Some(usage) = agent.last_usage.as_ref().and_then(sanitize_usage) {
        p.insert("lastUsage".into(), usage);
    }
    if let Some(error) = &agent.last_error {
        p.insert("lastError".into(), error.clone().into());
    }
    match &agent.attention {
        AttentionState::Required {
            reason,
            timestamp_ms,
        } => {
            p.insert("requiresAttention".into(), true.into());
            p.insert("attentionReason".into(), reason.as_str().into());
            p.insert("attentionTimestamp".into(), iso(*timestamp_ms).into());
        }
        AttentionState::None => {
            p.insert("requiresAttention".into(), false.into());
            p.insert("attentionReason".into(), Value::Null);
            p.insert("attentionTimestamp".into(), Value::Null);
        }
    }
    Value::Object(p)
}

/// `enrichAgentPayload`: title and archive state come from the stored record.
pub fn enrich_agent_payload(payload: &mut Value, record: Option<&StoredAgentRecord>) {
    let Some(map) = payload.as_object_mut() else {
        return;
    };
    map.insert(
        "title".into(),
        record
            .and_then(|r| r.title.value())
            .map_or(Value::Null, |t| t.clone().into()),
    );
    map.insert(
        "archivedAt".into(),
        record
            .and_then(|r| r.archived_at.value())
            .map_or(Value::Null, |t| t.clone().into()),
    );
}

fn stored_runtime_info(record: &StoredAgentRecord) -> Option<(Value, Option<Option<String>>)> {
    let info = record.runtime_info.as_ref()?;
    let mut out = Map::new();
    out.insert("provider".into(), info.provider.clone().into());
    out.insert("sessionId".into(), opt_string(info.session_id.as_deref()));
    let nullable =
        |out: &mut Map<String, Value>, key: &str, n: &Nullable<String>| -> Option<Option<String>> {
            if n.is_present() {
                let value = n.value().cloned();
                out.insert(key.into(), opt_string(value.as_deref()));
                Some(value)
            } else {
                None
            }
        };
    nullable(&mut out, "model", &info.model);
    let thinking = nullable(&mut out, "thinkingOptionId", &info.thinking_option_id);
    nullable(&mut out, "modeId", &info.mode_id);
    if let Some(extra) = &info.extra_info {
        out.insert("extra".into(), Value::Object(extra.clone()));
    }
    Some((Value::Object(out), thinking))
}

/// `buildStoredPersistenceHandle` + `projectPersistenceHandleForWire`.
fn stored_persistence_for_wire(handle: &StoredHandle, registered: &dyn Fn(&str) -> bool) -> Value {
    // `toAgentPersistenceHandle`: unregistered provider or empty session id is no handle.
    if !registered(&handle.provider) || handle.session_id.is_empty() {
        return Value::Null;
    }
    let native = match &handle.native_handle {
        Nullable::Missing => None,
        Nullable::Null => Some(Value::Null),
        Nullable::Value(v) => Some(v.clone()),
    };
    wire_handle(
        &handle.provider,
        &handle.session_id,
        native,
        handle.metadata.as_ref(),
    )
}

fn normalized_date(raw: &str) -> String {
    parse_js_date(raw).map_or_else(|| raw.to_owned(), iso)
}

/// `buildStoredAgentPayload`. `registered` is `isProviderRegistered` for the provider registry.
pub fn build_stored_agent_payload(
    record: &StoredAgentRecord,
    registered: &dyn Fn(&str) -> bool,
) -> Value {
    let provider_available = registered(&record.provider);
    let runtime = stored_runtime_info(record);
    let configured_thinking = record
        .config
        .value()
        .and_then(|c| c.thinking_option_id.value())
        .map(String::as_str);
    let effective = resolve_effective_thinking_option_id(
        runtime.as_ref().and_then(|(_, t)| t.as_ref()),
        configured_thinking,
    );
    let persistence = match record.persistence.value() {
        Some(handle) if provider_available => stored_persistence_for_wire(handle, registered),
        _ => Value::Null,
    };
    let mut p = Map::new();
    p.insert("id".into(), record.id.clone().into());
    p.insert("provider".into(), record.provider.clone().into());
    p.insert("cwd".into(), record.cwd.clone().into());
    if let Some(workspace_id) = record.workspace_id.as_ref().filter(|s| !s.is_empty()) {
        p.insert("workspaceId".into(), workspace_id.clone().into());
    }
    p.insert(
        "model".into(),
        opt_string(
            record
                .config
                .value()
                .and_then(|c| c.model.value())
                .map(String::as_str),
        ),
    );
    p.insert("thinkingOptionId".into(), opt_string(configured_thinking));
    p.insert(
        "effectiveThinkingOptionId".into(),
        opt_string(effective.as_deref()),
    );
    if let Some((info, _)) = &runtime {
        p.insert("runtimeInfo".into(), info.clone());
    }
    p.insert(
        "createdAt".into(),
        normalized_date(&record.created_at).into(),
    );
    p.insert(
        "updatedAt".into(),
        normalized_date(&resolve_stored_agent_updated_at(record)).into(),
    );
    p.insert(
        "lastUserMessageAt".into(),
        record
            .last_user_message_at
            .value()
            .map_or(Value::Null, |s| normalized_date(s).into()),
    );
    p.insert("status".into(), lifecycle_str(record.last_status).into());
    p.insert(
        "capabilities".into(),
        Value::Object(crate::projection::stored_agent_capabilities()),
    );
    p.insert(
        "currentModeId".into(),
        opt_string(record.last_mode_id.value().map(String::as_str)),
    );
    p.insert("availableModes".into(), json!([]));
    p.insert("pendingPermissions".into(), json!([]));
    p.insert("persistence".into(), persistence);
    p.insert(
        "title".into(),
        opt_string(record.title.value().map(String::as_str)),
    );
    p.insert(
        "requiresAttention".into(),
        record.requires_attention.unwrap_or(false).into(),
    );
    p.insert(
        "attentionReason".into(),
        record.attention_reason.value().map_or(Value::Null, |r| {
            serde_json::to_value(r).unwrap_or(Value::Null)
        }),
    );
    p.insert(
        "attentionTimestamp".into(),
        opt_string(record.attention_timestamp.value().map(String::as_str)),
    );
    p.insert(
        "archivedAt".into(),
        opt_string(record.archived_at.value().map(String::as_str)),
    );
    if record.internal == Some(true) {
        p.insert("internal".into(), true.into());
    }
    p.insert(
        "labels".into(),
        Value::Object(
            record
                .labels
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        ),
    );
    if let Some(error) = record.last_error.value() {
        p.insert("lastError".into(), error.clone().into());
    }
    if !provider_available {
        p.insert("providerUnavailable".into(), true.into());
    }
    Value::Object(p)
}

/// `sanitizeOptionalJson`-based check used by callers that need to know a JSON blob would
/// vanish from a payload.
pub fn is_empty_after_sanitize(value: &Value) -> bool {
    sanitize_json(value).is_none_or(|v| v.as_object().is_some_and(Map::is_empty))
}
