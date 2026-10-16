//! Paseo daemon core (WP8): the agent actor and manager, the Rust port of TS `AgentManager`.
//!
//! Step A: [`AgentActor`] with the run slot state machine, event/lifecycle/foreground lanes and
//! the shutdown barrier, event routing (steer barrier, pending-run staging, event lane), turn
//! identity, attention edge detection in `emit_state`, the [`ManagerBus`] with atomic
//! subscribe-with-replay, published read views, [`ManagerHooks`] and the [`ProviderBridge`]
//! seam (with `MockSession` and `MockClient` behind the `testing` feature).
//!
//! Step B: the lifecycle side on top of it: create / import / resume / reload / close, archive,
//! unarchive and the parent cascade, detach / labels / metadata, hydration and rewind,
//! `ensure_agent_loaded`, config preparation and the provider registry, importable sessions,
//! retired internal agents, the persistence projection ([`StorePersister`]) and the schedule
//! host ([`CoreScheduleHost`]).
//!
//! Behavior is the TS daemon's; quirks are reproduced and marked `PARITY-QUIRK(<name>)` (listed
//! in `docs/p4-core-spec.md` section 17), the few intentional differences `DEVIATION(<name>)`.

pub mod abort;
pub mod activity;
pub mod agent_payload;
pub mod agent_updates;
pub mod background;
pub mod bridge;
pub mod config;
pub mod delivery;
pub mod error;
pub mod events;
pub mod fanout;
pub mod hooks;
pub mod ids;
pub mod lifecycle;
pub mod manager;
pub mod model;
pub(crate) mod persist_chain;
pub mod persistence_sub;
pub mod projection;
pub mod provider_subagents;
pub mod schedule_host;
pub mod session;
pub mod time;

pub(crate) mod agent_actor;

#[cfg(any(test, feature = "testing"))]
pub mod mock;

pub use abort::AbortSignal;
pub use agent_actor::timeline_record::command_may_have_changed_external_state;
pub use agent_actor::ActorHandle;
pub use bridge::{ProviderBridge, SessionRef};
pub use error::{CoreError, Result};
pub use events::{ManagerEvent, SharedEvent, SubscribeOpts, Subscription};
pub use hooks::{
    AttentionNotice, ManagerHooks, NoopHooks, PersistOptions, SnapshotPersister,
    PLUGIN_BEFORE_HOOK_TIMEOUT,
};
pub use lifecycle::archive::{has_open_agent_tab, is_open_agent_tab_label};
pub use lifecycle::create::{derive_initial_agent_title, resolve_create_agent_titles};
pub use lifecycle::detach_labels::{detached_agent_label_patch, MetadataUpdates};
pub use lifecycle::loader::LoadOptions;
pub use lifecycle::register::RegisterOptions;
pub use lifecycle::reload::RELOAD_SESSION_CLOSE_TIMEOUT_ERROR;
pub use manager::bus::BusTx as ManagerBus;
pub use manager::config_prep::{
    is_paseo_tool_policy_enabled, PaseoToolPolicy, PaseoToolsOptions, ResumePurpose,
};
pub use manager::importable::{
    ImportableQuery, ImportableSessionProviderError, ImportableSessionsResult,
};
pub use manager::prompt::{PromptDisposition, StartRunOptions};
pub use manager::providers::{
    ApplyToolPolicy, ProviderAvailability, ProviderDefinition, ProviderRegistryUpdate,
};
pub use manager::retired::RetiredInternalAgent;
pub use manager::runs::{AgentRunOutcome, RunStream};
pub use manager::steer::SteerDispatch;
pub use manager::waits::{WaitForAgentOptions, WaitForAgentResult};
pub use manager::{AgentManager, ManagerOptions};
pub use model::{
    apply_label_patch, label_patch_of, AgentOwner, AttentionState, Broadcast, CancelOutcome,
    CreateAgentOptions, DetachResult, HydrateOpts, ImportInput, LabelPatch, Lifecycle,
    ManagedAgentView, ReloadOptions, ResumeOptions, RewindMode, RunOpts, RunStart, SteerRunOpts,
    UnarchiveUpdates, PARENT_AGENT_ID_LABEL,
};
pub use persistence_sub::StorePersister;
pub use schedule_host::{CoreScheduleHost, ScheduleWorkspaces};
