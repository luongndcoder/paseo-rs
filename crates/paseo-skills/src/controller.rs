//! The single writer for installed skills. Ports `controller.ts`.
//!
//! `SkillsController` is the synchronous body; [`crate::actor`] runs it on one thread so
//! startup convergence and every settings command go through one queue.

use crate::error::{failed, Result};
use crate::operations::{
    auto_update_installed_skills, get_skills_status, install_skills, uninstall_skills,
    update_skills, OpKind, SkillTargets, SkillsStatus,
};
use crate::selection::{
    coerce_skill_names, coerce_skill_selection, SelectionStore, SkillSelection,
};
use crate::transaction::{begin_skills_transaction, recover_interrupted_skill_transactions};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Everything the settings UI needs to render skills in one round trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillsSnapshot {
    #[serde(flatten)]
    pub status: SkillsStatus,
    pub selection: SkillSelection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillsConfirmation {
    pub removals: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillsSaveResult {
    #[serde(flatten)]
    pub snapshot: SkillsSnapshot,
    /// Non-null when the save did nothing because these directories would be deleted and
    /// the request had not confirmed them. Retry with the names.
    #[serde(rename = "confirmationRequired")]
    pub confirmation_required: Option<SkillsConfirmation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportLegacyResult {
    pub imported: bool,
    pub selection: SkillSelection,
}

/// A save request after coercion: the selection plus the removals the caller confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveRequest {
    pub selection: SkillSelection,
    pub confirmed_removals: BTreeSet<String>,
}

impl SaveRequest {
    /// Mirrors TS `save(request: unknown)`: both parts are coerced from raw JSON.
    pub fn from_value(request: &Value) -> Self {
        let confirmed = request
            .as_object()
            .and_then(|m| m.get("confirmedRemovals"))
            .map(coerce_skill_names)
            .unwrap_or_default();
        Self {
            selection: coerce_skill_selection(request),
            confirmed_removals: confirmed.into_iter().collect(),
        }
    }

    pub fn new(selection: SkillSelection, confirmed_removals: &[String]) -> Self {
        Self::from_value(&{
            let mut value = serde_json::to_value(&selection).unwrap_or(Value::Null);
            if let Some(map) = value.as_object_mut() {
                map.insert(
                    "confirmedRemovals".into(),
                    Value::from(confirmed_removals.to_vec()),
                );
            }
            value
        })
    }
}

type Converge = fn(&SkillTargets, &SkillSelection) -> Result<SkillsStatus>;
const MAX_SAVE_ATTEMPTS: usize = 3;

/// Resolved per operation, not at wiring time: the bundle path depends on how the app was
/// packaged, which is not knowable when the controller is built.
pub type ResolveTargets = Arc<dyn Fn() -> SkillTargets + Send + Sync>;

pub struct SkillsController {
    resolve_targets: ResolveTargets,
    store: SelectionStore,
}

impl SkillsController {
    pub fn new(resolve_targets: ResolveTargets, store: SelectionStore) -> Self {
        Self {
            resolve_targets,
            store,
        }
    }

    fn converge(&self, apply: Converge) -> Result<SkillsSnapshot> {
        let selection = self.store.get()?;
        recover_interrupted_skill_transactions(&(self.resolve_targets)(), &selection)?;
        let status = apply(&(self.resolve_targets)(), &selection)?;
        Ok(SkillsSnapshot { status, selection })
    }

    pub fn status(&self) -> Result<SkillsSnapshot> {
        self.converge(get_skills_status)
    }

    pub fn install(&self) -> Result<SkillsSnapshot> {
        self.converge(update_skills)
    }

    pub fn update(&self) -> Result<SkillsSnapshot> {
        self.converge(update_skills)
    }

    pub fn uninstall(&self) -> Result<SkillsSnapshot> {
        self.converge(uninstall_skills)
    }

    pub fn auto_update(&self) -> Result<SkillsSnapshot> {
        self.converge(auto_update_installed_skills)
    }

    pub fn save(&self, request: &SaveRequest) -> Result<SkillsSaveResult> {
        let targets = (self.resolve_targets)();
        let next = &request.selection;
        let previous = self.store.get()?;
        recover_interrupted_skill_transactions(&targets, &previous)?;
        let confirmed = &request.confirmed_removals;

        // The plan comes from a scan taken here, inside the queue, and is the exact plan
        // applied below. Deciding what will be deleted from an older snapshot leaves a
        // window for a directory to appear and be deleted unannounced.
        let mut plan = get_skills_status(&targets, next)?;
        for _ in 0..MAX_SAVE_ATTEMPTS {
            let removals: Vec<String> = plan
                .ops
                .iter()
                .filter(|op| op.kind == OpKind::Delete)
                .map(|op| op.name.clone())
                .collect();
            if removals.iter().any(|name| !confirmed.contains(name)) {
                return Ok(SkillsSaveResult {
                    snapshot: SkillsSnapshot {
                        status: get_skills_status(&targets, &previous)?,
                        selection: previous,
                    },
                    confirmation_required: Some(SkillsConfirmation { removals }),
                });
            }

            // Capture only paths this frozen plan can mutate. Capturing every managed path
            // would make rollback delete a new external directory that appeared after the
            // scan merely because it was absent from the snapshot.
            let transaction = begin_skills_transaction(&targets, &previous, next, &plan.ops)?;
            // beginSkillsTransaction atomically staged the deletes. Running removeSkill
            // afterward would delete a path another writer recreated.
            let frozen = SkillsStatus {
                ops: plan
                    .ops
                    .iter()
                    .filter(|op| op.kind != OpKind::Delete)
                    .cloned()
                    .collect(),
                ..plan.clone()
            };
            let status = match install_skills(&targets, next, Some(frozen)) {
                Ok(status) => status,
                Err(error) => {
                    // PARITY-QUIRK(skills-rollback-error-masks-cause): a failing rollback
                    // replaces the convergence error, as `await rollback(); throw error` does.
                    transaction.rollback()?;
                    return Err(error);
                }
            };

            // An empty custom selection converges to the intentional not-installed state;
            // zero remaining operations is the invariant, not the label.
            if status.ops.is_empty() {
                if let Err(error) = self.store.set_parsed(next) {
                    transaction.rollback()?;
                    return Err(error);
                }
                if let Err(error) = transaction.commit() {
                    // Selection and disk convergence are already durable. The owned
                    // manifest stays behind so the next serialized operation retries
                    // cleanup; reporting this save as failed would misstate its outcome.
                    tracing::warn!(%error, "skills transaction cleanup deferred");
                }
                return Ok(SkillsSaveResult {
                    snapshot: SkillsSnapshot {
                        status,
                        selection: next.clone(),
                    },
                    confirmation_required: None,
                });
            }

            // The frozen plan became incomplete while it was being applied. Put only its
            // mutations back, rescan, and either request confirmation for a new deletion
            // or retry harmless additions/updates from the fresh plan.
            transaction.rollback()?;
            plan = get_skills_status(&targets, next)?;
        }

        Err(failed(
            "Skills changed repeatedly while the selection was being saved",
        ))
    }

    pub fn import_legacy_selection_if_unset(
        &self,
        selection: &Value,
    ) -> Result<ImportLegacyResult> {
        if self.store.is_set()? {
            return Ok(ImportLegacyResult {
                imported: false,
                selection: self.store.get()?,
            });
        }
        let imported = self.store.set(selection)?;
        Ok(ImportLegacyResult {
            imported: true,
            selection: imported,
        })
    }
}
