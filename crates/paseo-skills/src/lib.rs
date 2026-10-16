//! Skills sync engine and crash-safe selection transactions (spec WP12).
//!
//! Ports `packages/server/src/server/orchestration-skills`. The on-disk formats
//! (`transaction.json`, per-skill `.paseo-managed-files.json`) are byte-compatible with TS
//! so either runtime can recover the other's interrupted save. `skills.selection` stays in
//! the daemon config behind [`ConfigStore`] (spec Q10).
//!
//! Quirks reproduced from TS are marked `PARITY-QUIRK`.

pub mod actor;
pub mod controller;
pub mod error;
mod fsx;
pub mod operations;
pub mod paths;
pub mod selection;
pub mod sync;
pub mod transaction;
pub mod wire;

pub use actor::{SkillsActor, SkillsHandle};
pub use controller::{
    ImportLegacyResult, SaveRequest, SkillsConfirmation, SkillsController, SkillsSaveResult,
    SkillsSnapshot,
};
pub use error::{Result, SkillsError};
pub use operations::{
    OpKind, SkillOp, SkillTargets, SkillsState, SkillsStatus, LEGACY_SKILL_NAMES,
};
pub use selection::{ConfigStore, InMemoryConfigStore, SelectionStore, SkillSelection};

#[cfg(test)]
mod tests;
