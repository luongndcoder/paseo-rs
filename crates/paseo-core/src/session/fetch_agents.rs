//! `fetch_agents_request`: the agent directory listing and its sequenced sync form
//! (`session.ts` `listFetchAgentsEntries`, `readAgentDirectorySync`, `SortablePager`).
//!
//! DEVIATION(pager-collation): the pager breaks ties with `String.prototype.localeCompare`
//! (ICU). Here ids and lower-cased titles compare bytewise, which agrees with ICU for the ASCII
//! ids and titles the app produces (digits < lowercase letters, `-` first) and differs only for
//! mixed-script titles.

use std::collections::HashMap;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use paseo_directory::DirectorySyncCursor;
use paseo_store::time::parse_js_date;
use serde_json::{json, Map, Value};

use crate::agent_updates::{matches_agent_updates_filter, AgentUpdatesFilter};

use super::env::{ListOptions, SessionServices};

/// `SessionRequestError`: a failure with a wire `code`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRequestError {
    pub code: &'static str,
    pub message: String,
}

impl SessionRequestError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortKey {
    StatusPriority,
    CreatedAt,
    UpdatedAt,
    Title,
}

impl SortKey {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "status_priority" => Self::StatusPriority,
            "created_at" => Self::CreatedAt,
            "updated_at" => Self::UpdatedAt,
            "title" => Self::Title,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::StatusPriority => "status_priority",
            Self::CreatedAt => "created_at",
            Self::UpdatedAt => "updated_at",
            Self::Title => "title",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SortSpec {
    key: SortKey,
    desc: bool,
}

impl SortSpec {
    fn direction(self) -> &'static str {
        if self.desc {
            "desc"
        } else {
            "asc"
        }
    }
}

const DEFAULT_SORT: [SortSpec; 1] = [SortSpec {
    key: SortKey::UpdatedAt,
    desc: true,
}];

#[derive(Debug, Clone, PartialEq)]
enum SortValue {
    Null,
    Num(f64),
    Str(String),
}

fn compare_values(left: &SortValue, right: &SortValue) -> i32 {
    match (left, right) {
        (SortValue::Null, SortValue::Null) => 0,
        (SortValue::Null, _) => -1,
        (_, SortValue::Null) => 1,
        (SortValue::Num(l), SortValue::Num(r)) => {
            if l == r {
                0
            } else if l < r {
                -1
            } else {
                1
            }
        }
        (SortValue::Str(l), SortValue::Str(r)) => match l.cmp(r) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        },
        (l, r) => compare_values(&SortValue::Str(display(l)), &SortValue::Str(display(r))),
    }
}

fn display(v: &SortValue) -> String {
    match v {
        SortValue::Null => "null".into(),
        SortValue::Num(n) => n.to_string(),
        SortValue::Str(s) => s.clone(),
    }
}

/// `getAgentStatusPriority`.
fn status_priority(agent: &Value) -> f64 {
    let pending = agent
        .get("pendingPermissions")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let reason = agent.get("attentionReason").and_then(Value::as_str);
    let status = agent
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if pending > 0 || reason == Some("permission") {
        0.0
    } else if status == "error" || reason == Some("error") {
        1.0
    } else if status == "running" {
        2.0
    } else if status == "initializing" {
        3.0
    } else {
        4.0
    }
}

fn sort_value(agent: &Value, key: SortKey) -> SortValue {
    let date = |field: &str| {
        SortValue::Num(
            agent
                .get(field)
                .and_then(Value::as_str)
                .and_then(parse_js_date)
                .map_or(f64::NAN, |ms| ms as f64),
        )
    };
    match key {
        SortKey::StatusPriority => SortValue::Num(status_priority(agent)),
        SortKey::CreatedAt => date("createdAt"),
        SortKey::UpdatedAt => date("updatedAt"),
        SortKey::Title => SortValue::Str(
            agent
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_lowercase(),
        ),
    }
}

fn agent_id(agent: &Value) -> &str {
    agent.get("id").and_then(Value::as_str).unwrap_or_default()
}

fn compare(left: &Value, right: &Value, sort: &[SortSpec]) -> i32 {
    for spec in sort {
        let base = compare_values(&sort_value(left, spec.key), &sort_value(right, spec.key));
        if base != 0 {
            return if spec.desc { -base } else { base };
        }
    }
    compare_values(
        &SortValue::Str(agent_id(left).to_owned()),
        &SortValue::Str(agent_id(right).to_owned()),
    )
}

struct Cursor {
    values: Map<String, Value>,
    id: String,
}

fn json_sort_value(value: &Value) -> SortValue {
    match value {
        Value::Number(n) => SortValue::Num(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => SortValue::Str(s.clone()),
        _ => SortValue::Null,
    }
}

fn compare_with_cursor(agent: &Value, cursor: &Cursor, sort: &[SortSpec]) -> i32 {
    for spec in sort {
        let right = cursor
            .values
            .get(spec.key.as_str())
            .map_or(SortValue::Null, json_sort_value);
        let base = compare_values(&sort_value(agent, spec.key), &right);
        if base != 0 {
            return if spec.desc { -base } else { base };
        }
    }
    compare_values(
        &SortValue::Str(agent_id(agent).to_owned()),
        &SortValue::Str(cursor.id.clone()),
    )
}

fn encode_cursor(agent: &Value, sort: &[SortSpec]) -> String {
    let mut values = Map::new();
    for spec in sort {
        values.insert(
            spec.key.as_str().into(),
            match sort_value(agent, spec.key) {
                SortValue::Null => Value::Null,
                SortValue::Num(n) if n.is_finite() => {
                    serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
                }
                SortValue::Num(_) => Value::Null,
                SortValue::Str(s) => Value::String(s),
            },
        );
    }
    let payload = json!({
        "sort": sort.iter().map(|s| json!({"key": s.key.as_str(), "direction": s.direction()})).collect::<Vec<_>>(),
        "values": Value::Object(values),
        "id": agent_id(agent),
    });
    URL_SAFE_NO_PAD.encode(payload.to_string())
}

fn decode_cursor(token: &str, sort: &[SortSpec]) -> Result<Cursor, SessionRequestError> {
    let invalid = || SessionRequestError::new("invalid_cursor", "Invalid fetch_agents cursor");
    let bytes = URL_SAFE_NO_PAD.decode(token).map_err(|_| invalid())?;
    let parsed: Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let obj = parsed.as_object().ok_or_else(invalid)?;
    let raw_sort = obj
        .get("sort")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let id = obj.get("id").and_then(Value::as_str).ok_or_else(invalid)?;
    let values = obj
        .get("values")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    let mut cursor_sort = Vec::new();
    for item in raw_sort {
        let item = item.as_object().ok_or_else(invalid)?;
        let key = item
            .get("key")
            .and_then(Value::as_str)
            .and_then(SortKey::parse)
            .ok_or_else(invalid)?;
        let desc = match item.get("direction").and_then(Value::as_str) {
            Some("asc") => false,
            Some("desc") => true,
            _ => return Err(invalid()),
        };
        cursor_sort.push(SortSpec { key, desc });
    }
    if cursor_sort != sort {
        return Err(SessionRequestError::new(
            "invalid_cursor",
            "fetch_agents cursor does not match current sort",
        ));
    }
    Ok(Cursor {
        values: values.clone(),
        id: id.to_owned(),
    })
}

fn normalize_sort(raw: Option<&Vec<Value>>) -> Vec<SortSpec> {
    let mut out: Vec<SortSpec> = Vec::new();
    for entry in raw.into_iter().flatten() {
        let key = entry
            .get("key")
            .and_then(Value::as_str)
            .and_then(SortKey::parse);
        let Some(key) = key else {
            continue;
        };
        if out.iter().any(|s| s.key == key) {
            continue;
        }
        out.push(SortSpec {
            key,
            desc: entry.get("direction").and_then(Value::as_str) == Some("desc"),
        });
    }
    if out.is_empty() {
        DEFAULT_SORT.to_vec()
    } else {
        out
    }
}

/// `fetch_agents_request` fields the handler reads.
#[derive(Debug, Clone)]
pub struct FetchAgentsRequest {
    pub request_id: String,
    pub scope_active: bool,
    pub include_background: bool,
    pub filter: Option<AgentUpdatesFilter>,
    /// The filter was present on the wire (an empty object counts).
    pub has_filter: bool,
    pub sort: Option<Vec<Value>>,
    pub page_limit: Option<usize>,
    pub page_cursor: Option<String>,
    pub subscribe: Option<SubscribeRequest>,
    pub sync: Option<DirectorySyncCursor>,
}

#[derive(Debug, Clone, Default)]
pub struct SubscribeRequest {
    pub subscription_id: Option<String>,
}

impl FetchAgentsRequest {
    pub fn from_message(message: &Value) -> Self {
        let page = message.get("page");
        Self {
            request_id: message
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            scope_active: message.get("scope").and_then(Value::as_str) == Some("active"),
            include_background: message
                .get("includeBackground")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            has_filter: message.get("filter").is_some_and(|f| !f.is_null()),
            filter: message.get("filter").map(AgentUpdatesFilter::from_value),
            sort: message.get("sort").and_then(Value::as_array).cloned(),
            page_limit: page
                .and_then(|p| p.get("limit"))
                .and_then(Value::as_u64)
                .map(|n| n as usize),
            page_cursor: page
                .and_then(|p| p.get("cursor"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            subscribe: message.get("subscribe").filter(|s| s.is_object()).map(|s| {
                SubscribeRequest {
                    subscription_id: s
                        .get("subscriptionId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                }
            }),
            sync: message
                .get("sync")
                .filter(|s| s.is_object())
                .map(|s| DirectorySyncCursor {
                    generation: s
                        .get("generation")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    after_seq: s.get("afterSeq").and_then(Value::as_i64),
                }),
        }
    }
}

/// A listed page: `entries` are `{agent, project}`.
#[derive(Debug, Clone)]
pub struct AgentPage {
    pub entries: Vec<Value>,
    pub page_info: Value,
}

/// `listFetchAgentsEntries` for `fetch_agents_request`.
pub async fn list_fetch_agents_entries(
    services: &SessionServices,
    request: &FetchAgentsRequest,
    provider_visible: &(dyn Fn(&str) -> bool + Send + Sync),
    include_background: bool,
    limit: usize,
) -> Result<AgentPage, SessionRequestError> {
    let filter = request.filter.as_ref();
    let sort = normalize_sort(request.sort.as_ref());
    let mut agents = services
        .list_agent_payloads(
            &ListOptions {
                labels: filter.and_then(|f| f.labels.clone()),
                include_archived: filter.and_then(|f| f.include_archived).unwrap_or(false),
                include_background: filter.and_then(|f| f.include_background).unwrap_or(false)
                    || include_background,
                include_unavailable_persisted: false,
            },
            provider_visible,
        )
        .await
        .map_err(|e| SessionRequestError::new("fetch_agents_failed", e))?;
    let active_placements: Option<HashMap<String, Value>> = if request.scope_active {
        Some(services.active_placements().await)
    } else {
        None
    };
    if let Some(placements) = &active_placements {
        agents.retain(|a| {
            a.get("archivedAt").is_none_or(|v| v.is_null() || v == "")
                && a.get("workspaceId")
                    .and_then(Value::as_str)
                    .is_some_and(|w| placements.contains_key(w))
        });
    }
    agents.sort_by(|a, b| compare(a, b, &sort).cmp(&0));
    if let Some(token) = &request.page_cursor {
        let cursor = decode_cursor(token, &sort)?;
        agents.retain(|a| compare_with_cursor(a, &cursor, &sort) > 0);
    }
    let mut matched: Vec<(Value, Value)> = Vec::new();
    let mut placement_cache: HashMap<String, Option<Value>> = HashMap::new();
    for agent in agents {
        if matched.len() > limit {
            break;
        }
        let Some(workspace_id) = agent
            .get("workspaceId")
            .and_then(Value::as_str)
            .filter(|w| !w.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        let project = match &active_placements {
            Some(placements) => placements.get(&workspace_id).cloned(),
            None => match placement_cache.get(&workspace_id) {
                Some(cached) => cached.clone(),
                None => {
                    let built = services.project_placement(&workspace_id).await;
                    placement_cache.insert(workspace_id.clone(), built.clone());
                    built
                }
            },
        };
        let Some(project) = project else {
            continue;
        };
        if !matches_agent_updates_filter(&agent, &project, filter) {
            continue;
        }
        matched.push((agent, project));
    }
    let has_more = matched.len() > limit;
    let paged: Vec<(Value, Value)> = matched.into_iter().take(limit).collect();
    let next_cursor = if has_more {
        paged.last().map(|(agent, _)| encode_cursor(agent, &sort))
    } else {
        None
    };
    Ok(AgentPage {
        page_info: json!({
            "nextCursor": next_cursor,
            "prevCursor": request.page_cursor,
            "hasMore": has_more,
        }),
        entries: paged
            .into_iter()
            .map(|(agent, project)| json!({ "agent": agent, "project": project }))
            .collect(),
    })
}

/// `readAgentDirectorySync`: the sequenced listing. Requires `scope: "active"` and no filter.
/// Returns the response payload fields (`entries`, `pageInfo`, `sync`).
pub async fn read_agent_directory_sync(
    services: &SessionServices,
    request: &FetchAgentsRequest,
    provider_visible: &(dyn Fn(&str) -> bool + Send + Sync),
) -> Result<Map<String, Value>, SessionRequestError> {
    if !request.scope_active || request.has_filter {
        return Err(SessionRequestError::new(
            "invalid_request",
            "Sequenced agent directory reads require scope=active and no filter.",
        ));
    }
    let snapshot =
        list_fetch_agents_entries(services, request, provider_visible, true, usize::MAX >> 11)
            .await?;
    let cursor = request.sync.clone().unwrap_or_default();
    let result = services.directory.synchronize_agents(
        snapshot.entries,
        &cursor,
        request.include_background,
    );
    let mut entries = Vec::new();
    for entry in result.entries {
        let included = match entry.get("agent") {
            Some(agent) => {
                services
                    .directory_includes(agent, request.include_background)
                    .await
            }
            None => false,
        };
        if included {
            entries.push(entry);
        }
    }
    let mut payload = Map::new();
    payload.insert("entries".into(), Value::Array(entries));
    payload.insert(
        "pageInfo".into(),
        serde_json::to_value(&result.page_info).unwrap_or(Value::Null),
    );
    payload.insert(
        "sync".into(),
        serde_json::to_value(&result.sync).unwrap_or(Value::Null),
    );
    Ok(payload)
}
