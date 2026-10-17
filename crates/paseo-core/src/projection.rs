//! Projections between the live agent view and the stored record (`agent-projections.ts`,
//! `persistence-hooks.ts`): `toStoredAgentRecord`, the stored-only `agent_state` view,
//! resume overrides and persistence handles.

use indexmap::IndexMap;
use paseo_agent_types::{AgentRuntimeInfo, AttentionReason, PersistenceHandle};
use paseo_provider_api::{AgentSessionConfig, JsonObject};
use paseo_store::agent_record::{
    AgentFeature, JsonMap, McpKind, RuntimeInfo, SerializableConfig, ToolGrant, ToolPolicy,
};
use paseo_store::time::parse_js_date;
use paseo_store::{AgentLifecycle, Nullable, StoredAgentRecord};
use serde_json::Value;

use crate::config::strip_internal_paseo_mcp_server;
use crate::model::{AttentionState, ManagedAgentView};
use crate::time::iso8601_from_millis;

/// `sanitizeOptionalJson` for values that came from JSON: objects drop keys that sanitize to
/// nothing and become nothing themselves when empty; arrays keep their (sanitized) items.
pub(crate) fn sanitize_json(value: &Value) -> Option<Value> {
    match value {
        Value::Array(items) => Some(Value::Array(
            items.iter().filter_map(sanitize_json).collect(),
        )),
        Value::Object(map) => {
            let cleaned: JsonMap = map
                .iter()
                .filter_map(|(k, v)| sanitize_json(v).map(|v| (k.clone(), v)))
                .collect();
            (!cleaned.is_empty()).then_some(Value::Object(cleaned))
        }
        other => Some(other.clone()),
    }
}

/// `sanitizeMetadata`: a non-empty sanitized object, else nothing.
pub(crate) fn sanitize_metadata(map: &JsonMap) -> Option<JsonMap> {
    match sanitize_json(&Value::Object(map.clone())) {
        Some(Value::Object(cleaned)) => Some(cleaned),
        _ => None,
    }
}

fn nullable_from_double(value: &Option<Option<String>>) -> Nullable<String> {
    match value {
        None => Nullable::Missing,
        Some(None) => Nullable::Null,
        Some(Some(s)) => Nullable::Value(s.clone()),
    }
}

/// `buildSerializableConfig`.
fn build_serializable_config(config: &AgentSessionConfig) -> Option<SerializableConfig> {
    let mut out = SerializableConfig::default();
    let mut any = false;
    let mut set = |present: bool| any |= present;
    if let Some(mode) = config.mode_id.as_ref().filter(|s| !s.is_empty()) {
        out.mode_id = Nullable::Value(mode.clone());
        set(true);
    }
    if let Some(model) = config.model.as_ref().filter(|s| !s.is_empty()) {
        out.model = Nullable::Value(model.clone());
        set(true);
    }
    if let Some(thinking) = config.thinking_option_id.as_ref().filter(|s| !s.is_empty()) {
        out.thinking_option_id = Nullable::Value(thinking.clone());
        set(true);
    }
    if let Some(values) = &config.feature_values {
        if let Some(Value::Object(cleaned)) = sanitize_json(&Value::Object(values.clone())) {
            out.feature_values = Nullable::Value(cleaned);
            set(true);
        }
    }
    if let Some(options) = &config.provider_options {
        if let Some(Value::Object(cleaned)) = sanitize_json(options) {
            out.provider_options = Nullable::Value(cleaned);
            set(true);
        }
    }
    if let Some(policy) = config.tool_policy.as_ref().and_then(tool_policy_from_value) {
        out.tool_policy = Nullable::Value(policy);
        set(true);
    }
    if let Some(prompt) = config.system_prompt.as_ref().filter(|s| !s.is_empty()) {
        out.system_prompt = Nullable::Value(prompt.clone());
        set(true);
    }
    if let Some(Value::Object(servers)) = &config.mcp_servers {
        out.mcp_servers = Nullable::Value(servers.clone());
        set(true);
    }
    any.then_some(out)
}

fn tool_policy_from_value(value: &Value) -> Option<ToolPolicy> {
    let grants = value.get("preapproved")?.as_array()?;
    let preapproved = grants
        .iter()
        .map(|g| serde_json::from_value::<ToolGrant>(g.clone()).ok())
        .collect::<Option<Vec<_>>>()?;
    Some(ToolPolicy { preapproved })
}

fn tool_policy_to_value(policy: &ToolPolicy) -> Value {
    serde_json::json!({
        "preapproved": policy.preapproved.iter().map(|g| serde_json::json!({
            "kind": match g.kind { McpKind::Mcp => "mcp" },
            "server": g.server,
            "tool": g.tool,
        })).collect::<Vec<_>>()
    })
}

/// `sanitizeRuntimeInfo`.
fn sanitize_runtime_info(info: &AgentRuntimeInfo) -> RuntimeInfo {
    let nullable = |v: &Option<Option<String>>| nullable_from_double(v);
    RuntimeInfo {
        provider: info.provider.clone(),
        session_id: info.session_id.clone(),
        model: nullable(&info.model),
        thinking_option_id: nullable(&info.thinking_option_id),
        mode_id: nullable(&info.mode_id),
        extra_info: info.extra.as_ref().and_then(sanitize_metadata),
        extra: JsonMap::new(),
    }
}

/// `sanitizePersistenceHandle` into the stored shape.
pub(crate) fn to_stored_persistence(
    handle: &PersistenceHandle,
) -> paseo_store::agent_record::PersistenceHandle {
    paseo_store::agent_record::PersistenceHandle {
        provider: handle.provider.clone(),
        session_id: handle.session_id.clone(),
        native_handle: match &handle.native_handle {
            Some(native) => Nullable::Value(Value::String(native.clone())),
            None => Nullable::Missing,
        },
        metadata: handle.metadata.as_ref().and_then(sanitize_metadata),
        extra: JsonMap::new(),
    }
}

/// Stored handle to the provider-facing one. `nativeHandle` is a string in the TS type, so a
/// stored non-string value is dropped.
pub(crate) fn from_stored_persistence(
    handle: &paseo_store::agent_record::PersistenceHandle,
) -> PersistenceHandle {
    PersistenceHandle {
        provider: handle.provider.clone(),
        session_id: handle.session_id.clone(),
        native_handle: handle
            .native_handle
            .value()
            .and_then(Value::as_str)
            .map(str::to_owned),
        metadata: handle.metadata.clone(),
    }
}

/// `toAgentPersistenceHandle`: `None` for an unregistered provider or an empty session id.
pub fn to_agent_persistence_handle(
    registered: Option<&[String]>,
    handle: &Nullable<paseo_store::agent_record::PersistenceHandle>,
) -> Option<PersistenceHandle> {
    let handle = handle.value()?;
    if let Some(registered) = registered {
        if !registered.contains(&handle.provider) {
            return None;
        }
    }
    if handle.session_id.is_empty() {
        return None;
    }
    Some(from_stored_persistence(handle))
}

fn lifecycle_of(view: &ManagedAgentView) -> AgentLifecycle {
    view.lifecycle
}

/// `toStoredAgentRecord`. `created_at` is the existing record's, which wins over the agent's.
pub fn to_stored_agent_record(
    view: &ManagedAgentView,
    title: Option<String>,
    created_at: Option<String>,
    internal: Option<bool>,
) -> StoredAgentRecord {
    let updated_at = iso8601_from_millis(view.updated_at_ms);
    let config = build_serializable_config(&view.config);
    let mut record = StoredAgentRecord::new(
        &view.id,
        &view.provider,
        &view.cwd,
        &created_at
            .clone()
            .unwrap_or_else(|| iso8601_from_millis(view.created_at_ms)),
    );
    record.workspace_id = view.workspace_id.clone();
    record.updated_at = updated_at.clone();
    record.last_activity_at = Some(updated_at);
    record.last_user_message_at =
        Nullable::from(view.last_user_message_at_ms.map(iso8601_from_millis));
    record.title = Nullable::from(title);
    record.labels = view.labels.clone();
    record.last_status = lifecycle_of(view);
    record.last_mode_id = Nullable::from(
        view.current_mode_id
            .clone()
            .or_else(|| config.as_ref().and_then(|c| c.mode_id.value().cloned())),
    );
    record.config = match config {
        Some(c) => Nullable::Value(c),
        None => Nullable::Null,
    };
    record.runtime_info = view.runtime_info.as_ref().map(sanitize_runtime_info);
    record.features = Some(
        view.features
            .iter()
            .map(|f| AgentFeature(f.clone()))
            .collect(),
    );
    record.persistence = match &view.persistence {
        Some(handle) => Nullable::Value(to_stored_persistence(handle)),
        None => Nullable::Null,
    };
    record.last_error = match &view.last_error {
        Some(error) => Nullable::Value(error.clone()),
        None => Nullable::Missing,
    };
    match &view.attention {
        AttentionState::None => {
            record.requires_attention = Some(false);
            record.attention_reason = Nullable::Null;
            record.attention_timestamp = Nullable::Null;
        }
        AttentionState::Required {
            reason,
            timestamp_ms,
        } => {
            record.requires_attention = Some(true);
            record.attention_reason = Nullable::Value(to_store_reason(*reason));
            record.attention_timestamp = Nullable::Value(iso8601_from_millis(*timestamp_ms));
        }
    }
    record.internal = internal;
    record.owner = view.owner.clone();
    record
}

fn to_store_reason(reason: AttentionReason) -> paseo_store::agent_record::AttentionReason {
    use paseo_store::agent_record::AttentionReason as S;
    match reason {
        AttentionReason::Finished => S::Finished,
        AttentionReason::Error => S::Error,
        AttentionReason::Permission => S::Permission,
    }
}

fn from_store_reason(reason: paseo_store::agent_record::AttentionReason) -> AttentionReason {
    use paseo_store::agent_record::AttentionReason as S;
    match reason {
        S::Finished => AttentionReason::Finished,
        S::Error => AttentionReason::Error,
        S::Permission => AttentionReason::Permission,
    }
}

/// `extractAttention`: unread state survives a resume.
pub fn extract_attention(record: &StoredAgentRecord) -> AttentionState {
    if record.requires_attention != Some(true) {
        return AttentionState::None;
    }
    let (Some(reason), Some(timestamp)) = (
        record.attention_reason.value().copied(),
        record.attention_timestamp.value(),
    ) else {
        return AttentionState::None;
    };
    AttentionState::Required {
        reason: from_store_reason(reason),
        timestamp_ms: parse_js_date(timestamp).unwrap_or(0),
    }
}

/// `resolveStoredAgentUpdatedAt`: the later of `updatedAt` and `lastActivityAt`.
pub fn resolve_stored_agent_updated_at(record: &StoredAgentRecord) -> String {
    let mut candidates: Vec<(i64, &String)> =
        [Some(&record.updated_at), record.last_activity_at.as_ref()]
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .filter_map(|s| parse_js_date(s).map(|ms| (ms, s)))
            .collect();
    if candidates.is_empty() {
        return record.updated_at.clone();
    }
    // Stable sort by descending time: ties keep `updatedAt` first.
    candidates.sort_by_key(|c| std::cmp::Reverse(c.0));
    candidates[0].1.clone()
}

/// Everything `extractTimestamps` carries into a resume.
pub struct StoredTimestamps {
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub last_user_message_at_ms: Option<i64>,
}

pub fn extract_timestamps(record: &StoredAgentRecord) -> StoredTimestamps {
    StoredTimestamps {
        created_at_ms: parse_js_date(&record.created_at).unwrap_or(0),
        updated_at_ms: parse_js_date(&resolve_stored_agent_updated_at(record)).unwrap_or(0),
        last_user_message_at_ms: record
            .last_user_message_at
            .value()
            .and_then(|s| parse_js_date(s)),
    }
}

fn config_value<T>(value: &Nullable<T>, f: impl FnOnce(&T) -> Value) -> Value {
    value.value().map(f).unwrap_or(Value::Null)
}

/// `buildConfigOverrides`: every key is present; `null` stands for the JS `undefined` that
/// shadows a same-named key of the persisted metadata when the resume config is merged.
pub fn build_config_overrides(record: &StoredAgentRecord) -> JsonObject {
    let config = record.config.value();
    let mut out = JsonObject::new();
    out.insert("provider".into(), Value::String(record.provider.clone()));
    out.insert("cwd".into(), Value::String(record.cwd.clone()));
    let mode = record
        .last_mode_id
        .value()
        .cloned()
        .or_else(|| config.and_then(|c| c.mode_id.value().cloned()));
    out.insert("modeId".into(), mode.map_or(Value::Null, Value::String));
    let pick = |f: fn(&SerializableConfig) -> Value| config.map(f).unwrap_or(Value::Null);
    out.insert(
        "model".into(),
        pick(|c| config_value(&c.model, |s| Value::String(s.clone()))),
    );
    out.insert(
        "thinkingOptionId".into(),
        pick(|c| config_value(&c.thinking_option_id, |s| Value::String(s.clone()))),
    );
    out.insert(
        "featureValues".into(),
        pick(|c| config_value(&c.feature_values, |m| Value::Object(m.clone()))),
    );
    out.insert(
        "providerOptions".into(),
        pick(|c| config_value(&c.provider_options, |m| Value::Object(m.clone()))),
    );
    out.insert(
        "toolPolicy".into(),
        pick(|c| config_value(&c.tool_policy, tool_policy_to_value)),
    );
    out.insert(
        "systemPrompt".into(),
        pick(|c| config_value(&c.system_prompt, |s| Value::String(s.clone()))),
    );
    out.insert(
        "mcpServers".into(),
        pick(|c| config_value(&c.mcp_servers, |m| Value::Object(m.clone()))),
    );
    strip_internal_paseo_mcp_from_overrides(out)
}

/// `stripInternalPaseoMcpServer` over an overrides object (a `null` mcpServers is "undefined").
fn strip_internal_paseo_mcp_from_overrides(mut overrides: JsonObject) -> JsonObject {
    let mut probe = AgentSessionConfig::new("", "");
    probe.mcp_servers = overrides
        .get("mcpServers")
        .filter(|v| !v.is_null())
        .cloned();
    let had = probe.mcp_servers.is_some();
    let stripped = strip_internal_paseo_mcp_server(probe);
    match (had, stripped.mcp_servers) {
        (true, Some(servers)) => {
            overrides.insert("mcpServers".into(), servers);
        }
        (true, None) => {
            overrides.shift_remove("mcpServers");
        }
        _ => {}
    }
    overrides
}

/// `buildSessionConfig`: the config of a record that has no provider handle to resume.
pub fn build_session_config(
    record: &StoredAgentRecord,
    valid_providers: Option<&[String]>,
) -> Option<AgentSessionConfig> {
    if let Some(valid) = valid_providers {
        if !valid.contains(&record.provider) {
            return None;
        }
    }
    let overrides = build_config_overrides(record);
    let mut object = JsonObject::new();
    for (key, value) in overrides {
        if !value.is_null() {
            object.insert(key, value);
        }
    }
    serde_json::from_value(Value::Object(object)).ok()
}

/// `buildStoredAgentConfig`: the config of a stored-only agent's `agent_state`.
pub fn build_stored_agent_config(record: &StoredAgentRecord) -> AgentSessionConfig {
    let mut config = AgentSessionConfig::new(record.provider.clone(), record.cwd.clone());
    let stored = record.config.value();
    config.mode_id = record
        .last_mode_id
        .value()
        .cloned()
        .or_else(|| stored.and_then(|c| c.mode_id.value().cloned()));
    if let Some(stored) = stored {
        config.model = stored.model.value().cloned();
        config.thinking_option_id = stored.thinking_option_id.value().cloned();
        config.feature_values = stored.feature_values.value().cloned();
        config.provider_options = stored.provider_options.value().cloned().map(Value::Object);
        config.tool_policy = stored.tool_policy.value().map(tool_policy_to_value);
        config.system_prompt = stored.system_prompt.value().cloned();
        config.mcp_servers = stored.mcp_servers.value().cloned().map(Value::Object);
    }
    strip_internal_paseo_mcp_server(config)
}

/// `STORED_AGENT_CAPABILITIES`.
pub fn stored_agent_capabilities() -> JsonObject {
    let mut caps = JsonObject::new();
    for (key, value) in [
        ("supportsStreaming", false),
        ("supportsSessionPersistence", true),
        ("supportsDynamicModes", false),
        ("supportsMcpServers", false),
        ("supportsReasoningStream", false),
        ("supportsToolInvocations", true),
        ("supportsRewindConversation", false),
        ("supportsRewindFiles", false),
        ("supportsRewindBoth", false),
    ] {
        caps.insert(key.into(), Value::Bool(value));
    }
    caps
}

/// The `agent_state` payload of a stored-only agent (`dispatchStoredAgentState`).
pub fn view_from_stored_record(record: &StoredAgentRecord) -> ManagedAgentView {
    ManagedAgentView {
        id: record.id.clone(),
        provider: record.provider.clone(),
        cwd: record.cwd.clone(),
        workspace_id: record.workspace_id.clone(),
        owner: record.owner.clone(),
        capabilities: stored_agent_capabilities(),
        config: build_stored_agent_config(record),
        runtime_info: None,
        created_at_ms: parse_js_date(&record.created_at).unwrap_or(0),
        updated_at_ms: parse_js_date(&record.updated_at).unwrap_or(0),
        available_modes: Vec::new(),
        features: record
            .features
            .as_ref()
            .map(|f| f.iter().map(|f| f.0.clone()).collect())
            .unwrap_or_default(),
        current_mode_id: record.last_mode_id.value().cloned(),
        pending_permissions: IndexMap::new(),
        pending_replacement: false,
        persistence: record.persistence.value().map(from_stored_persistence),
        history_primed: true,
        last_user_message_at_ms: record
            .last_user_message_at
            .value()
            .and_then(|s| parse_js_date(s)),
        active_turn_id: None,
        active_turn_started_at_ms: None,
        last_usage: None,
        last_error: record.last_error.value().cloned(),
        attention: extract_attention(record),
        lifecycle: AgentLifecycle::Closed,
        active_foreground_turn_id: None,
        internal: record.internal.unwrap_or(false),
        labels: record.labels.clone(),
        has_run: false,
        pending_run_start: None,
    }
}

/// `buildArchivedAgentRecord`: archived, `running`/`initializing` normalized to `idle`,
/// attention cleared.
pub fn build_archived_agent_record(
    record: &StoredAgentRecord,
    archived_at: &str,
    updated_at: Option<&str>,
) -> StoredAgentRecord {
    let mut archived = record.clone();
    archived.archived_at = Nullable::Value(archived_at.to_owned());
    if let Some(updated_at) = updated_at {
        archived.updated_at = updated_at.to_owned();
    }
    archived.last_status = match record.last_status {
        AgentLifecycle::Running | AgentLifecycle::Initializing => AgentLifecycle::Idle,
        other => other,
    };
    archived.requires_attention = Some(false);
    archived.attention_reason = Nullable::Null;
    archived.attention_timestamp = Nullable::Null;
    archived
}

#[cfg(test)]
mod tests {
    use super::*;
    use paseo_store::AgentLifecycle as L;
    use serde_json::json;

    fn view() -> ManagedAgentView {
        let mut v = ManagedAgentView::for_test("00000000-0000-4000-8000-000000000001");
        v.created_at_ms = 1_700_000_000_000;
        v.updated_at_ms = 1_700_000_001_000;
        v
    }

    #[test]
    fn record_projection_writes_the_ts_key_set() {
        let mut v = view();
        v.lifecycle = L::Idle;
        v.config.model = Some("m".into());
        let record = to_stored_agent_record(&v, None, None, Some(false));
        let json = serde_json::to_value(&record).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "id",
                "provider",
                "cwd",
                "createdAt",
                "updatedAt",
                "lastActivityAt",
                "lastUserMessageAt",
                "title",
                "labels",
                "lastStatus",
                "lastModeId",
                "config",
                "features",
                "persistence",
                "requiresAttention",
                "attentionReason",
                "attentionTimestamp",
                "internal"
            ]
        );
        assert_eq!(json["title"], Value::Null);
        assert_eq!(json["features"], json!([]));
        assert_eq!(json["config"], json!({"model": "m"}));
        assert_eq!(json["internal"], json!(false));
    }

    #[test]
    fn empty_config_is_null_and_mode_falls_back_to_config() {
        let mut v = view();
        assert_eq!(
            to_stored_agent_record(&v, None, None, None).config,
            Nullable::Null
        );
        v.config.mode_id = Some("plan".into());
        let record = to_stored_agent_record(&v, Some("T".into()), Some("created".into()), None);
        assert_eq!(record.last_mode_id, Nullable::Value("plan".into()));
        assert_eq!(record.created_at, "created");
        assert_eq!(record.title, Nullable::Value("T".into()));
    }

    #[test]
    fn attention_round_trips_through_the_record() {
        let mut v = view();
        v.attention = AttentionState::Required {
            reason: AttentionReason::Finished,
            timestamp_ms: 1_700_000_002_000,
        };
        let record = to_stored_agent_record(&v, None, None, None);
        assert_eq!(record.requires_attention, Some(true));
        assert_eq!(extract_attention(&record), v.attention);
        let archived = build_archived_agent_record(&record, "2026-01-01T00:00:00.000Z", None);
        assert_eq!(extract_attention(&archived), AttentionState::None);
        assert_eq!(archived.attention_reason, Nullable::Null);
    }

    #[test]
    fn archived_status_normalizes_busy_to_idle() {
        let mut record = StoredAgentRecord::new("a", "codex", "/x", "2026-01-01T00:00:00.000Z");
        record.last_status = L::Running;
        let archived = build_archived_agent_record(&record, "A", Some("U"));
        assert_eq!(archived.last_status, L::Idle);
        assert_eq!(archived.updated_at, "U");
        record.last_status = L::Closed;
        assert_eq!(
            build_archived_agent_record(&record, "A", None).last_status,
            L::Closed
        );
    }

    #[test]
    fn overrides_carry_undefined_as_null() {
        let record = StoredAgentRecord::new("a", "codex", "/x", "2026-01-01T00:00:00.000Z");
        let overrides = build_config_overrides(&record);
        assert_eq!(overrides["model"], Value::Null);
        assert_eq!(overrides["cwd"], json!("/x"));
        assert!(overrides.contains_key("mcpServers"));
    }

    #[test]
    fn persistence_handle_needs_a_registered_provider_and_session_id() {
        let handle = Nullable::Value(paseo_store::agent_record::PersistenceHandle {
            provider: "codex".into(),
            session_id: "s".into(),
            native_handle: Nullable::Value(json!("n")),
            metadata: None,
            extra: JsonMap::new(),
        });
        let registered = vec!["codex".to_owned()];
        let got = to_agent_persistence_handle(Some(&registered), &handle).unwrap();
        assert_eq!(got.native_handle.as_deref(), Some("n"));
        assert!(to_agent_persistence_handle(Some(&["claude".to_owned()]), &handle).is_none());
        assert!(to_agent_persistence_handle(None, &Nullable::Missing).is_none());
    }

    #[test]
    fn updated_at_resolution_takes_the_later_timestamp() {
        let mut record = StoredAgentRecord::new("a", "codex", "/x", "2026-01-01T00:00:00.000Z");
        record.updated_at = "2026-01-01T00:00:00.000Z".into();
        record.last_activity_at = Some("2026-02-01T00:00:00.000Z".into());
        assert_eq!(
            resolve_stored_agent_updated_at(&record),
            "2026-02-01T00:00:00.000Z"
        );
    }

    #[test]
    fn stored_state_view_is_closed_with_stored_capabilities() {
        let mut record = StoredAgentRecord::new("a", "codex", "/x", "2026-01-01T00:00:00.000Z");
        record.internal = Some(true);
        record.last_mode_id = Nullable::Value("m".into());
        let view = view_from_stored_record(&record);
        assert_eq!(view.lifecycle, L::Closed);
        assert!(view.internal);
        assert_eq!(view.current_mode_id.as_deref(), Some("m"));
        assert_eq!(view.capabilities["supportsToolInvocations"], json!(true));
        assert!(view.history_primed);
    }

    // ---- agent-archive.test.ts ----

    fn base_record() -> StoredAgentRecord {
        let mut record = StoredAgentRecord::new(
            "agent-1",
            "codex",
            "/workspace/project",
            "2025-01-01T00:00:00.000Z",
        );
        record.updated_at = "2025-01-02T00:00:00.000Z".into();
        record.last_status = L::Idle;
        record
    }

    #[test]
    fn archives_a_stored_agent_without_changing_terminal_statuses() {
        for status in [L::Idle, L::Error, L::Closed] {
            let mut record = base_record();
            record.last_status = status;
            let archived = build_archived_agent_record(&record, "2025-01-03T00:00:00.000Z", None);
            assert_eq!(archived.last_status, status);
            assert_eq!(
                archived.archived_at,
                Nullable::Value("2025-01-03T00:00:00.000Z".into())
            );
            assert_eq!(archived.updated_at, base_record().updated_at);
        }
    }

    #[test]
    fn archives_busy_stored_agents_as_idle() {
        for status in [L::Initializing, L::Running] {
            let mut record = base_record();
            record.last_status = status;
            let archived = build_archived_agent_record(&record, "2025-01-03T00:00:00.000Z", None);
            assert_eq!(archived.last_status, L::Idle);
        }
    }

    #[test]
    fn clears_persisted_attention_when_archiving() {
        let mut record = base_record();
        record.requires_attention = Some(true);
        record.attention_reason =
            Nullable::Value(paseo_store::agent_record::AttentionReason::Finished);
        record.attention_timestamp = Nullable::Value("2025-01-02T12:00:00.000Z".into());
        let archived = build_archived_agent_record(&record, "2025-01-03T00:00:00.000Z", None);
        assert_eq!(archived.requires_attention, Some(false));
        assert_eq!(archived.attention_reason, Nullable::Null);
        assert_eq!(archived.attention_timestamp, Nullable::Null);
    }

    #[test]
    fn can_stamp_updated_at_to_the_archive_timestamp() {
        let archived = build_archived_agent_record(
            &base_record(),
            "2025-01-03T00:00:00.000Z",
            Some("2025-01-03T00:00:00.000Z"),
        );
        assert_eq!(archived.updated_at, "2025-01-03T00:00:00.000Z");
    }
}
