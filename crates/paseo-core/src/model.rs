//! Public data model of the manager: the managed-agent read view and run options.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use indexmap::IndexMap;
use paseo_agent_types::{
    AgentMode, AgentRuntimeInfo, AgentUsage, AttentionReason, PermissionRequest, PersistenceHandle,
};
use paseo_provider_api::{AgentSessionConfig, JsonObject, RunOptions, SteerOptions};
pub use paseo_store::AgentOwner;
use serde_json::Value;

/// `AgentLifecycleStatus`.
pub type Lifecycle = paseo_store::AgentLifecycle;

pub const PARENT_AGENT_ID_LABEL: &str = "paseo.parent-agent-id";

/// TS `AttentionState`. Attention is an edge-triggered unread signal, set in `emit_state`.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum AttentionState {
    #[default]
    None,
    Required {
        reason: AttentionReason,
        timestamp_ms: i64,
    },
}

impl AttentionState {
    pub fn requires_attention(&self) -> bool {
        matches!(self, Self::Required { .. })
    }
}

/// Start state of the foreground run slot (`PendingForegroundRun.start`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunStart {
    Pending,
    Started { turn_id: String },
    Failed { error: String },
}

/// Snapshot of a managed agent: what TS exposes as `ManagedAgent` minus the live handles
/// (session, waiters, finalized turn ids). Emitted in `agent_state` and published by the actor
/// after every handled message.
#[derive(Debug, Clone, PartialEq)]
pub struct ManagedAgentView {
    pub id: String,
    pub provider: String,
    pub cwd: String,
    pub workspace_id: Option<String>,
    pub owner: Option<AgentOwner>,
    pub capabilities: JsonObject,
    pub config: AgentSessionConfig,
    pub runtime_info: Option<AgentRuntimeInfo>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub available_modes: Vec<AgentMode>,
    pub features: Vec<Value>,
    pub current_mode_id: Option<String>,
    /// Insertion ordered, like the TS `Map`.
    pub pending_permissions: IndexMap<String, PermissionRequest>,
    pub pending_replacement: bool,
    pub persistence: Option<PersistenceHandle>,
    pub history_primed: bool,
    pub last_user_message_at_ms: Option<i64>,
    pub active_turn_id: Option<String>,
    pub active_turn_started_at_ms: Option<i64>,
    pub last_usage: Option<AgentUsage>,
    pub last_error: Option<String>,
    pub attention: AttentionState,
    pub lifecycle: Lifecycle,
    pub active_foreground_turn_id: Option<String>,
    pub internal: bool,
    pub labels: IndexMap<String, String>,
    /// A run slot is occupied (`runs.hasRun`).
    pub has_run: bool,
    /// Start state of a foreground run slot, `None` for no run or an autonomous run.
    pub pending_run_start: Option<RunStart>,
}

impl ManagedAgentView {
    /// `hasInFlightRun`: lifecycle running, a foreground turn, or any run slot.
    pub fn has_in_flight_run(&self) -> bool {
        self.lifecycle == Lifecycle::Running
            || self.active_foreground_turn_id.is_some()
            || self.has_run
    }

    /// `isAgentBusy`.
    pub fn is_busy(&self) -> bool {
        matches!(self.lifecycle, Lifecycle::Initializing | Lifecycle::Running)
    }

    /// `getParentAgentIdFromLabels`: the label, trimmed, when non-blank.
    pub fn parent_agent_id(&self) -> Option<&str> {
        self.labels
            .get(PARENT_AGENT_ID_LABEL)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
    }

    /// `isDelegatedAgent`.
    pub fn is_delegated(&self) -> bool {
        self.parent_agent_id().is_some()
    }

    /// First pending permission (`peekPendingPermission`).
    pub fn peek_pending_permission(&self) -> Option<&PermissionRequest> {
        self.pending_permissions.values().next()
    }

    pub fn pending_start_started(&self) -> bool {
        matches!(self.pending_run_start, Some(RunStart::Started { .. }))
    }
}

#[cfg(any(test, feature = "testing"))]
impl ManagedAgentView {
    /// A minimal idle agent, for tests.
    pub fn for_test(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            provider: "codex".into(),
            cwd: "/tmp".into(),
            workspace_id: None,
            owner: None,
            capabilities: JsonObject::new(),
            config: AgentSessionConfig::new("codex", "/tmp"),
            runtime_info: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            available_modes: Vec::new(),
            features: Vec::new(),
            current_mode_id: None,
            pending_permissions: IndexMap::new(),
            pending_replacement: false,
            persistence: None,
            history_primed: true,
            last_user_message_at_ms: None,
            active_turn_id: None,
            active_turn_started_at_ms: None,
            last_usage: None,
            last_error: None,
            attention: AttentionState::None,
            lifecycle: Lifecycle::Idle,
            active_foreground_turn_id: None,
            internal: false,
            labels: IndexMap::new(),
            has_run: false,
            pending_run_start: None,
        }
    }
}

/// `AgentRunOptions` as the manager sees them: the one key it reads plus everything else the
/// provider receives untouched (`outputSchema`, `resumeFrom`, `maxThinkingTokens`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunOpts {
    pub client_message_id: Option<String>,
    pub extra: JsonObject,
}

impl RunOpts {
    pub fn with_client_message_id(id: impl Into<String>) -> Self {
        Self {
            client_message_id: Some(id.into()),
            extra: JsonObject::new(),
        }
    }

    pub(crate) fn to_provider(&self) -> Option<RunOptions> {
        if self.client_message_id.is_none() && self.extra.is_empty() {
            return None;
        }
        let mut options = self.extra.clone();
        if let Some(id) = &self.client_message_id {
            options.insert("clientMessageId".into(), Value::String(id.clone()));
        }
        Some(options)
    }
}

/// `AgentSteerOptions`: run options plus `clearPendingPermissions`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SteerRunOpts {
    pub run: RunOpts,
    pub clear_pending_permissions: bool,
}

impl SteerRunOpts {
    pub(crate) fn to_provider(&self, expected_turn_id: &str) -> SteerOptions {
        SteerOptions {
            expected_turn_id: expected_turn_id.to_owned(),
            clear_pending_permissions: self.clear_pending_permissions.then_some(true),
            run: self.run.to_provider().unwrap_or_default(),
        }
    }
}

/// `AgentRunCancellationResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    NotRunning,
    Settled,
    Refused,
}

/// `Subscribe`-side `agent_state` helper: the id of a subscriber target.
pub type AgentId = String;

/// `AgentLabelPatch`: a `None` value deletes the label (`null` in TS).
pub type LabelPatch = IndexMap<String, Option<String>>;

/// `applyLabelPatch`.
pub fn apply_label_patch(
    labels: &IndexMap<String, String>,
    patch: &LabelPatch,
) -> IndexMap<String, String> {
    let mut next = labels.clone();
    for (key, value) in patch {
        match value {
            None => {
                next.shift_remove(key);
            }
            Some(value) => {
                next.insert(key.clone(), value.clone());
            }
        }
    }
    next
}

/// A label patch that sets every entry of `labels`.
pub fn label_patch_of(labels: &IndexMap<String, String>) -> LabelPatch {
    labels
        .iter()
        .map(|(k, v)| (k.clone(), Some(v.clone())))
        .collect()
}

/// `broadcast?: boolean | (() => boolean)` of a history hydration. The deferred form is read
/// when the hydration applies, so a second concurrent loader can upgrade it meanwhile.
#[derive(Debug, Clone, Default)]
pub enum Broadcast {
    #[default]
    Off,
    On,
    Deferred(Arc<AtomicBool>),
}

impl Broadcast {
    pub fn now(&self) -> bool {
        match self {
            Self::Off => false,
            Self::On => true,
            Self::Deferred(flag) => flag.load(Ordering::SeqCst),
        }
    }

    pub(crate) fn is_deferred(&self) -> bool {
        matches!(self, Self::Deferred(_))
    }
}

impl From<bool> for Broadcast {
    fn from(value: bool) -> Self {
        if value {
            Self::On
        } else {
            Self::Off
        }
    }
}

/// `HydrateTimelineOptions`.
#[derive(Debug, Clone, Default)]
pub struct HydrateOpts {
    pub force: bool,
    pub broadcast: Broadcast,
    /// Defaults to `broadcast`.
    pub broadcast_timeline: Option<Broadcast>,
}

/// `CreateAgentOptions`. `workspace_id: None` is an explicit decision: the agent never appears
/// in the sidebar.
#[derive(Debug, Clone, Default)]
pub struct CreateAgentOptions {
    pub labels: Option<IndexMap<String, String>>,
    pub env: Option<BTreeMap<String, String>>,
    pub persist_session: Option<bool>,
    pub initial_title: Option<String>,
    pub workspace_id: Option<String>,
    pub owner: Option<AgentOwner>,
}

/// The `options` of `resumeAgentFromPersistence`.
#[derive(Debug, Clone, Default)]
pub struct ResumeOptions {
    pub created_at_ms: Option<i64>,
    pub updated_at_ms: Option<i64>,
    pub last_user_message_at_ms: Option<i64>,
    pub labels: Option<IndexMap<String, String>>,
    pub workspace_id: Option<String>,
    pub owner: Option<AgentOwner>,
    pub attention: Option<AttentionState>,
}

/// `importProviderSession` input.
#[derive(Debug, Clone)]
pub struct ImportInput {
    pub provider: String,
    pub provider_handle_id: String,
    pub cwd: String,
    pub workspace_id: String,
    pub labels: Option<IndexMap<String, String>>,
}

/// `RewindMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindMode {
    Conversation,
    Files,
    Both,
}

impl RewindMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Files => "files",
            Self::Both => "both",
        }
    }
}

/// Options of `reloadAgentSession`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReloadOptions {
    /// Wipe the in-memory timeline (new epoch) and re-stream provider history.
    pub rehydrate_from_disk: bool,
}

/// Result of `detachAgent`.
#[derive(Debug, Clone, PartialEq)]
pub struct DetachResult {
    pub record: paseo_store::StoredAgentRecord,
    pub live: bool,
    pub previous_parent_agent_id: Option<String>,
}

/// Updates of `unarchiveSnapshot`.
#[derive(Debug, Clone, Default)]
pub struct UnarchiveUpdates {
    pub workspace_id: Option<String>,
    pub labels: Option<LabelPatch>,
}
