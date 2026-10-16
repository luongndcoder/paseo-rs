//! Client capabilities and the COMPAT predicates built on them
//! (`protocol/src/client-capabilities.ts`, `session.ts` helpers).

use std::collections::BTreeSet;

use serde_json::Value;

pub mod cap {
    pub const HELLO_REJECTION: &str = "hello_rejection";
    // COMPAT(ownedSubscriptions): added in v0.8.0, remove legacy ownership after 2027-03-09.
    pub const OWNED_SUBSCRIPTIONS: &str = "owned_subscriptions";
    // COMPAT(explicitEventSubscriptions): added in v0.8.0, remove legacy broadcasts after 2027-03-08.
    pub const EXPLICIT_EVENT_SUBSCRIPTIONS: &str = "explicit_event_subscriptions";
    pub const ALL_PROVIDERS: &str = "all_providers";
    // COMPAT(selectiveAgentTimeline): added in v0.1.106, remove after 2027-01-12.
    pub const SELECTIVE_AGENT_TIMELINE: &str = "selective_agent_timeline";
    pub const REASONING_MERGE_ENUM: &str = "reasoning_merge_enum";
    pub const CUSTOM_MODE_ICONS: &str = "custom_mode_icons";
    pub const TERMINAL_REFLOWABLE_SNAPSHOT: &str = "terminal_reflowable_snapshot";
    pub const PROVIDER_SUBAGENTS: &str = "provider_subagents";
    pub const PROJECTED_SUBAGENT_TIMELINE: &str = "projected_subagent_timeline";
    // COMPAT(projectUpdates): added in v0.1.109, remove gate after 2027-01-15.
    pub const PROJECT_UPDATES: &str = "project_updates";
    pub const COMPACT_PROVIDER_SNAPSHOTS: &str = "compact_provider_snapshots";
    pub const PROVIDER_SNAPSHOT_REFERENCES: &str = "provider_snapshot_references";
    // COMPAT(timelineReplacementInvalidation): added in v0.5.0, remove after 2027-02-21.
    pub const TIMELINE_REPLACEMENT_INVALIDATION: &str = "timeline_replacement_invalidation";
    // COMPAT(timelineNotifications): added in v0.7.2, remove after 2027-03-03.
    pub const TIMELINE_NOTIFICATIONS: &str = "timeline_notifications";
    // COMPAT(pluginTimelineItems): added in v0.8.0, remove after 2027-03-07.
    pub const PLUGIN_TIMELINE_ITEMS: &str = "plugin_timeline_items";
    pub const WORKSPACE_SETUP_BLOCKED: &str = "workspace_setup_blocked";
    pub const BROWSER_HOST: &str = "browser_host";

    pub const ALL: [&str; 18] = [
        HELLO_REJECTION,
        OWNED_SUBSCRIPTIONS,
        EXPLICIT_EVENT_SUBSCRIPTIONS,
        ALL_PROVIDERS,
        SELECTIVE_AGENT_TIMELINE,
        REASONING_MERGE_ENUM,
        CUSTOM_MODE_ICONS,
        TERMINAL_REFLOWABLE_SNAPSHOT,
        PROVIDER_SUBAGENTS,
        PROJECTED_SUBAGENT_TIMELINE,
        PROJECT_UPDATES,
        COMPACT_PROVIDER_SNAPSHOTS,
        PROVIDER_SNAPSHOT_REFERENCES,
        TIMELINE_REPLACEMENT_INVALIDATION,
        TIMELINE_NOTIFICATIONS,
        PLUGIN_TIMELINE_ITEMS,
        WORKSPACE_SETUP_BLOCKED,
        BROWSER_HOST,
    ];
}

/// The capabilities a client advertised that the daemon knows (`parseClientCapabilities`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientCaps(BTreeSet<String>);

impl ClientCaps {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Reads `hello.capabilities`: only keys the daemon knows whose value is exactly `true`.
    pub fn from_hello(capabilities: Option<&Value>) -> Self {
        let Some(map) = capabilities.and_then(Value::as_object) else {
            return Self::empty();
        };
        Self(
            map.iter()
                .filter(|(key, value)| {
                    **value == Value::Bool(true) && cap::ALL.contains(&key.as_str())
                })
                .map(|(key, _)| key.clone())
                .collect(),
        )
    }

    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        Self(names.into_iter().map(str::to_owned).collect())
    }

    pub fn has(&self, capability: &str) -> bool {
        self.0.contains(capability)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

const MIN_VERSION_ALL_PROVIDERS: &str = "0.1.45";

/// `isAppVersionAtLeast`: prerelease suffix stripped, missing parts are 0.
pub fn is_app_version_at_least(app_version: Option<&str>, min_version: &str) -> bool {
    let Some(app_version) = app_version else {
        return false;
    };
    let base = app_version.split('-').next().unwrap_or_default();
    let parse = |s: &str| -> Vec<f64> {
        s.split('.')
            .map(|p| p.parse::<f64>().unwrap_or(f64::NAN))
            .collect()
    };
    let parts = parse(base);
    let min_parts = parse(min_version);
    for (i, b) in min_parts.iter().enumerate() {
        let a = parts.get(i).copied().unwrap_or(0.0);
        // JS comparisons with NaN are false, so an unparsable part falls through.
        if a > *b {
            return true;
        }
        if a < *b {
            return false;
        }
    }
    true
}

pub fn client_supports_all_providers(app_version: Option<&str>) -> bool {
    is_app_version_at_least(app_version, MIN_VERSION_ALL_PROVIDERS)
}

/// Clients before 0.1.45 validate providers with a closed enum and reject the whole message on
/// an unknown one.
pub const LEGACY_PROVIDER_IDS: [&str; 3] = ["claude", "codex", "opencode"];

/// `isProviderVisibleToClient`.
pub fn is_provider_visible(caps: &ClientCaps, app_version: Option<&str>, provider: &str) -> bool {
    caps.has(cap::ALL_PROVIDERS)
        || client_supports_all_providers(app_version)
        || LEGACY_PROVIDER_IDS.contains(&provider)
}

/// `sessionEventCategory`: the `session.events.set_subscription` category of an outbound
/// message, if it has one.
pub fn session_event_category(message: &Value) -> Option<&'static str> {
    let ty = message.get("type")?.as_str()?;
    Some(match ty {
        "project.update" => "project.update",
        "providers_snapshot_update" => "providers_snapshot_update",
        "agent_attention_required" => "agent_attention_required",
        "agent_permission_request" => "agent_permission_request",
        "agent_permission_resolved" => "agent_permission_resolved",
        "checkout_status_update" => "checkout_status_update",
        "script_status_update" => "script_status_update",
        "workspace_setup_progress" => "workspace_setup_progress",
        "agent.provider_subagents.update" => "agent.provider_subagents.update",
        "terminal_attention_required" => "terminal_attention_required",
        "activity_log" => "activity_log",
        "hub.execution.agent.update" => "hub.execution.agent.update",
        "hub.execution.agent.stream" => "hub.execution.agent.stream",
        "status" => match message.get("payload")?.get("status")?.as_str()? {
            "server_info" => "status.server_info",
            "daemon_config_changed" => "status.daemon_config_changed",
            "plugin_catalog_changed" => "status.plugin_catalog_changed",
            "plugin_settings_changed" => "status.plugin_settings_changed",
            _ => return None,
        },
        _ => return None,
    })
}

/// `legacyWantsEvent`. COMPAT(ownedSubscriptions): remove implicit event delivery after 2027-03-09.
pub fn legacy_wants_event(event: &str, caps: &ClientCaps) -> bool {
    if event == "project.update" && !caps.has(cap::PROJECT_UPDATES) {
        return false;
    }
    match event {
        "project.update"
        | "providers_snapshot_update"
        | "agent_attention_required"
        | "agent_permission_request"
        | "agent_permission_resolved" => !caps.has(cap::EXPLICIT_EVENT_SUBSCRIPTIONS),
        "agent.provider_subagents.update" => caps.has(cap::PROVIDER_SUBAGENTS),
        _ => true,
    }
}

/// Every value `SessionEventSubscription` accepts.
pub const SESSION_EVENT_SUBSCRIPTIONS: [&str; 17] = [
    "project.update",
    "providers_snapshot_update",
    "agent_attention_required",
    "agent_permission_request",
    "agent_permission_resolved",
    "checkout_status_update",
    "script_status_update",
    "workspace_setup_progress",
    "agent.provider_subagents.update",
    "terminal_attention_required",
    "status.server_info",
    "status.daemon_config_changed",
    "status.plugin_catalog_changed",
    "status.plugin_settings_changed",
    "activity_log",
    "hub.execution.agent.update",
    "hub.execution.agent.stream",
];
