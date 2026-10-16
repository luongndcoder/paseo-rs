//! File-backed persistence for the Paseo core: agent records, project/workspace registries,
//! workspace labels and registry bootstrap.
//!
//! Files are byte-compatible with the TS daemon's `$PASEO_HOME` (`agents/**`, `projects/*`).
//! During the strangler both runtimes read the same files, so records keep unknown keys
//! (decision Q3) and writes use the same `JSON.stringify(v, null, 2)` shape.

pub mod agent_record;
pub mod agent_store;
pub mod bootstrap;
pub mod error;
pub mod js_json;
pub mod label_sequence;
pub mod label_service;
pub mod labels;
pub mod nullable;
pub mod paths;
pub mod project_registry;
pub mod registry;
pub mod time;
pub mod workspace_registry;

pub use agent_record::{parse_stored_agent_record, AgentLifecycle, AgentOwner, StoredAgentRecord};
pub use agent_store::{AgentStore, SnapshotInput, SnapshotOptions};
pub use bootstrap::{
    backfill_workspace_id_for_legacy_agents, bootstrap_registries, BootstrapHost, BootstrapOptions,
    DirectoryMembership,
};
pub use error::{LabelErrorCode, Result, StoreError};
pub use label_sequence::WorkspaceLabelSequence;
pub use label_service::{
    normalize_workspace_label_name, workspace_label_key, WorkspaceLabelService,
};
pub use labels::{LabelDefinition, WorkspaceLabelCatalogStore};
pub use nullable::Nullable;
pub use paths::{are_equivalent_paths, project_dir_name_from_cwd};
pub use project_registry::{ProjectKind, ProjectRecord, ProjectRegistry};
pub use workspace_registry::{WorkspaceKind, WorkspaceRecord, WorkspaceRegistry};
