//! Callbacks the manager makes into its host: the Rust form of the `AgentManagerOptions`
//! callbacks and plugin lifecycle (`PluginLifecycle`). In the strangler phase the plugin and
//! `on_archived` hooks are `internal.*` RPCs to the TS daemon (spec section 13).

use std::time::Duration;

use async_trait::async_trait;
use paseo_agent_types::{AgentStreamEvent, AttentionReason};
use paseo_provider_api::ToolSpec;
use paseo_timeline::TimelineState;
use serde_json::Value;

use crate::model::ManagedAgentView;

/// Time a plugin `before` hook may take before the create/resume/reload path gives up (decision
/// Q16): the hooks are a core -> TS request/response RPC (`internal.plugins.lifecycle.before`),
/// run in the same per-plugin id order as TS, and a failure or timeout aborts the operation.
pub const PLUGIN_BEFORE_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// `AgentAttentionCallback` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionNotice {
    pub agent_id: String,
    pub provider: String,
    pub reason: AttentionReason,
}

#[async_trait]
pub trait ManagerHooks: Send + Sync + 'static {
    /// Whether a plugin lifecycle exists (`pluginLifecycle` is set in TS). Only then do create
    /// and session open run the `before` hooks; the default is no plugins.
    fn has_plugin_lifecycle(&self) -> bool {
        false
    }

    /// Awaited `before` hook of `agent.create`. May rewrite the config/env carried in `input`;
    /// an `Err` aborts the create. The caller enforces [`PLUGIN_BEFORE_HOOK_TIMEOUT`].
    async fn plugin_before_create(&self, input: Value) -> Result<Value, String> {
        Ok(input)
    }

    /// Awaited `before` hook of `agent.session_open`.
    async fn plugin_before_session_open(&self, input: Value) -> Result<Value, String> {
        Ok(input)
    }

    /// Fire-and-forget plugin lifecycle emit (`agent.archived`, `agent.closed`, ...).
    fn plugin_emit(&self, _hook: &str, _payload: Value) {}

    /// `publishAgentStream`: called for non-timeline stream events of live, non-internal agents.
    fn publish_agent_stream(
        &self,
        _agent: &ManagedAgentView,
        _event: &AgentStreamEvent,
        _timeline: &TimelineState,
    ) {
    }

    /// Edge-triggered attention (`finished`, `error`, first pending `permission`). Never called
    /// for internal or delegated agents.
    fn on_attention(&self, _notice: AttentionNotice) {}

    /// Awaited inside archive (schedule completion). Errors are logged and swallowed (Q19).
    async fn on_archived(&self, _agent_id: &str) -> Result<(), String> {
        Ok(())
    }

    /// A completed shell tool call may have changed remote git state (`gh pr merge`, `git push`).
    fn on_workspace_state_may_have_changed(&self, _cwd: &str) {}

    async fn before_steer_unavailable_fallback(&self, _agent_id: &str, _expected_turn_id: &str) {}

    /// Internal Paseo tools to offer a new session; `None` for none.
    async fn paseo_tool_catalog(&self, _agent_id: &str) -> Option<Vec<ToolSpec>> {
        None
    }
}

/// Hooks that do nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopHooks;

#[async_trait]
impl ManagerHooks for NoopHooks {}

/// `AgentStorage.applySnapshot` options (`hasOwnProperty` semantics: `Some` means present).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersistOptions {
    pub title: Option<Option<String>>,
    pub internal: Option<Option<bool>>,
}

/// Writes a managed-agent snapshot to storage. The real implementation projects the view into a
/// `StoredAgentRecord` (`toStoredAgentRecord`) and calls `AgentStore::apply_snapshot`.
/// Internal agents never reach it.
#[async_trait]
pub trait SnapshotPersister: Send + Sync + 'static {
    async fn persist(
        &self,
        agent: &ManagedAgentView,
        options: PersistOptions,
    ) -> Result<(), String>;

    /// The stored title of an existing record: `None` when there is no record,
    /// `Some(title)` (possibly `None`) when there is (`resolveInitialPersistedTitle`).
    async fn stored_title(&self, _agent_id: &str) -> Result<Option<Option<String>>, String> {
        Ok(None)
    }
}
