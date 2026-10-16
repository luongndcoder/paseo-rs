//! `matchesAgentUpdatesFilter`: the pure predicate shared by the live subscription stream and the
//! snapshot pager. Does an agent (with its resolved project placement) satisfy a `fetch_agents`
//! filter?

use std::collections::HashSet;

use indexmap::IndexMap;
use serde_json::Value;

use crate::agent_payload::resolve_effective_thinking_option_id;

/// `AgentDirectoryFilterSchema` (`fetch_agents_request.filter`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentUpdatesFilter {
    pub labels: Option<IndexMap<String, String>>,
    pub project_keys: Option<Vec<String>>,
    pub statuses: Option<Vec<String>>,
    pub include_archived: Option<bool>,
    pub requires_attention: Option<bool>,
    /// `thinkingOptionId?: string | null`: outer `None` is "do not filter".
    pub thinking_option_id: Option<Option<String>>,
    pub include_internal: Option<bool>,
    pub include_background: Option<bool>,
}

impl AgentUpdatesFilter {
    /// Reads the wire object leniently; fields of the wrong type count as absent.
    pub fn from_value(value: &Value) -> Self {
        let strings = |key: &str| -> Option<Vec<String>> {
            value.get(key)?.as_array().map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
        };
        Self {
            labels: value.get("labels").and_then(Value::as_object).map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            }),
            project_keys: strings("projectKeys"),
            statuses: strings("statuses"),
            include_archived: value.get("includeArchived").and_then(Value::as_bool),
            requires_attention: value.get("requiresAttention").and_then(Value::as_bool),
            thinking_option_id: match value.get("thinkingOptionId") {
                None => None,
                Some(Value::Null) => Some(None),
                Some(Value::String(s)) => Some(Some(s.clone())),
                Some(_) => None,
            },
            include_internal: value.get("includeInternal").and_then(Value::as_bool),
            include_background: value.get("includeBackground").and_then(Value::as_bool),
        }
    }

    pub fn with_include_background(mut self, include: bool) -> Self {
        if include {
            self.include_background = Some(true);
        }
        self
    }
}

fn str_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `agentThinkingOptionMatchesFilter`.
fn thinking_option_matches(agent: &Value, filter: &AgentUpdatesFilter) -> bool {
    let Some(expected) = &filter.thinking_option_id else {
        return true;
    };
    let expected = resolve_effective_thinking_option_id(None, expected.as_deref());
    // `agent.effectiveThinkingOptionId ?? resolve({runtimeInfo, configured})`
    let resolved = match str_at(agent, "effectiveThinkingOptionId") {
        Some(effective) => Some(effective.to_owned()),
        None => {
            let runtime_thinking = agent
                .get("runtimeInfo")
                .and_then(Value::as_object)
                .and_then(|info| info.get("thinkingOptionId"))
                .map(|v| v.as_str().map(str::to_owned));
            resolve_effective_thinking_option_id(
                runtime_thinking.as_ref(),
                str_at(agent, "thinkingOptionId"),
            )
        }
    };
    resolved == expected
}

/// `matchesAgentStructuralFilter`.
fn structural_matches(agent: &Value, project: &Value, filter: &AgentUpdatesFilter) -> bool {
    if let Some(statuses) = filter.statuses.as_ref().filter(|s| !s.is_empty()) {
        let status = str_at(agent, "status").unwrap_or_default();
        if !statuses.iter().any(|s| s == status) {
            return false;
        }
    }
    if let Some(expected) = filter.requires_attention {
        let actual = agent
            .get("requiresAttention")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if actual != expected {
            return false;
        }
    }
    if let Some(keys) = filter.project_keys.as_ref().filter(|k| !k.is_empty()) {
        let keys: HashSet<&str> = keys
            .iter()
            .map(String::as_str)
            .filter(|k| !k.trim().is_empty())
            .collect();
        if !keys.is_empty() && !keys.contains(str_at(project, "projectKey").unwrap_or_default()) {
            return false;
        }
    }
    true
}

pub fn matches_agent_updates_filter(
    agent: &Value,
    project: &Value,
    filter: Option<&AgentUpdatesFilter>,
) -> bool {
    if let Some(labels) = filter.and_then(|f| f.labels.as_ref()) {
        let agent_labels = agent.get("labels").and_then(Value::as_object);
        let all = labels.iter().all(|(key, value)| {
            agent_labels
                .and_then(|l| l.get(key))
                .and_then(Value::as_str)
                == Some(value.as_str())
        });
        if !all {
            return false;
        }
    }
    let include_archived = filter.and_then(|f| f.include_archived).unwrap_or(false);
    let archived = str_at(agent, "archivedAt").is_some_and(|s| !s.is_empty());
    if !include_archived && archived {
        return false;
    }
    let internal = agent
        .get("internal")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if internal && filter.and_then(|f| f.include_internal) != Some(true) {
        return false;
    }
    if let Some(filter) = filter {
        if !thinking_option_matches(agent, filter) || !structural_matches(agent, project, filter) {
            return false;
        }
    }
    true
}
