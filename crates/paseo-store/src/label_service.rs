//! Workspace label operations (`workspace-labels/internal/service.ts`). Operations run one at a
//! time; the catalog commit and the journal live in [`crate::labels`].

use crate::error::{LabelErrorCode, Result, StoreError};
use crate::label_sequence::{
    ChangeListener, LabelSubscription, SequencedChange, WorkspaceLabelChange, WorkspaceLabelCursor,
    WorkspaceLabelSequence, WorkspaceLabelSync,
};
use crate::labels::{LabelColor, LabelDefinition, LabelMutation, WorkspaceLabelCatalogStore};
use crate::registry::RecordMap;
use crate::time::now_iso;
use crate::workspace_registry::WorkspaceRecord;
use std::sync::Arc;

/// JS `\s` (ECMAScript WhiteSpace + LineTerminator), which differs from `char::is_whitespace`
/// (it has U+FEFF, lacks U+0085).
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `normalizeWorkspaceLabelName`: collapse whitespace runs to one space and trim.
pub fn normalize_workspace_label_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut in_space = false;
    for c in name.chars() {
        if is_js_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out.trim_matches(is_js_space).to_string()
}

/// `workspaceLabelKey`: the case-insensitive identity of a label.
pub fn workspace_label_key(name: &str) -> String {
    normalize_workspace_label_name(name).to_lowercase()
}

fn label_error(code: LabelErrorCode, message: &str) -> StoreError {
    StoreError::Label {
        code,
        message: message.to_string(),
    }
}

fn require_name(raw: &str) -> Result<String> {
    let name = normalize_workspace_label_name(raw);
    if name.is_empty() {
        return Err(label_error(
            LabelErrorCode::LabelNameEmpty,
            "Label name cannot be empty",
        ));
    }
    Ok(name)
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssignmentResult {
    pub label: LabelDefinition,
    pub workspace_labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateResult {
    pub label: LabelDefinition,
    pub affected_workspace_count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct LabelUpdate {
    pub name: String,
    pub new_name: Option<String>,
    pub color: Option<LabelColor>,
}

pub struct WorkspaceLabelService {
    catalog: WorkspaceLabelCatalogStore,
    sequence: WorkspaceLabelSequence,
    operations: tokio::sync::Mutex<()>,
}

impl WorkspaceLabelService {
    pub fn new(
        catalog: WorkspaceLabelCatalogStore,
        sequence: Option<WorkspaceLabelSequence>,
    ) -> Self {
        WorkspaceLabelService {
            catalog,
            sequence: sequence.unwrap_or_default(),
            operations: tokio::sync::Mutex::new(()),
        }
    }

    pub async fn initialize(&self) -> Result<()> {
        self.catalog.initialize().await
    }

    pub async fn list(&self, cursor: Option<&WorkspaceLabelCursor>) -> Result<WorkspaceLabelSync> {
        let _op = self.operations.lock().await;
        let labels = self.catalog.list().await?;
        Ok(self.sequence.synchronize(&labels, cursor))
    }

    /// Registers `on_change` and returns the snapshot it must be read against, atomically.
    pub async fn subscribe(
        &self,
        cursor: Option<&WorkspaceLabelCursor>,
        on_change: Arc<dyn Fn(&SequencedChange) + Send + Sync>,
    ) -> Result<(WorkspaceLabelSync, LabelSubscription)> {
        let _op = self.operations.lock().await;
        let listener: ChangeListener = on_change;
        let subscription = self.sequence.subscribe(listener);
        match self.catalog.list().await {
            Ok(labels) => Ok((self.sequence.synchronize(&labels, cursor), subscription)),
            Err(error) => {
                subscription.unsubscribe();
                Err(error)
            }
        }
    }

    pub async fn set_assignment(
        &self,
        workspace_id: &str,
        label: &LabelDefinition,
        assigned: bool,
    ) -> Result<AssignmentResult> {
        let _op = self.operations.lock().await;
        let name = require_name(&label.name)?;
        let color = label.color;
        let workspace_id = workspace_id.to_string();
        struct Commit {
            definition: LabelDefinition,
            workspace_labels: Vec<String>,
            catalog_changed: bool,
        }
        let committed = self
            .catalog
            .commit::<Commit, _>(move |catalog, workspaces| {
                let workspace = match workspaces.get(&workspace_id) {
                    Some(w) if w.archived_at.is_none() => w,
                    _ => {
                        return Err(label_error(
                            LabelErrorCode::WorkspaceNotFound,
                            "Workspace not found",
                        ))
                    }
                };
                let key = workspace_label_key(&name);
                let existing = catalog.iter().find(|l| workspace_label_key(&l.name) == key);
                let definition = existing
                    .cloned()
                    .unwrap_or_else(|| LabelDefinition::new(&name, color));
                let current = workspace.labels.clone().unwrap_or_default();
                let updated = update_assignment_labels(&current, &key, &definition.name, assigned);
                let catalog_changed = existing.is_none() && assigned;
                let workspace_labels = updated.clone().unwrap_or_else(|| current.clone());
                let workspace_updates = match updated {
                    Some(labels) => {
                        let mut next = workspace.clone();
                        next.labels = if labels.is_empty() {
                            None
                        } else {
                            Some(labels)
                        };
                        next.updated_at = now_iso();
                        vec![next]
                    }
                    None => Vec::new(),
                };
                let mut next_catalog = catalog.to_vec();
                if catalog_changed {
                    next_catalog.push(definition.clone());
                }
                Ok(LabelMutation {
                    labels: next_catalog,
                    workspace_updates,
                    result: Commit {
                        definition,
                        workspace_labels,
                        catalog_changed,
                    },
                })
            })
            .await?;
        if committed.catalog_changed {
            self.sequence.publish(WorkspaceLabelChange::Upsert {
                label: committed.definition.clone(),
                previous_name: None,
            });
        }
        Ok(AssignmentResult {
            label: committed.definition,
            workspace_labels: committed.workspace_labels,
        })
    }

    /// Edit a label: a new name, a new colour, or both, in one catalog commit. An omitted field
    /// is left alone. Renaming onto an existing name is rejected, not merged, and the check
    /// runs inside the planner so a rejected collision applies neither field.
    pub async fn update(&self, input: LabelUpdate) -> Result<UpdateResult> {
        let _op = self.operations.lock().await;
        let from_key = workspace_label_key(&require_name(&input.name)?);
        let new_name = match &input.new_name {
            Some(n) => Some(require_name(n)?),
            None => None,
        };
        let color = input.color;
        struct Commit {
            label: LabelDefinition,
            affected: usize,
            changed: bool,
            previous_name: Option<String>,
        }
        let committed = self
            .catalog
            .commit::<Commit, _>(move |catalog, workspaces| {
                let Some(position) = catalog
                    .iter()
                    .position(|l| workspace_label_key(&l.name) == from_key)
                else {
                    return Err(label_error(
                        LabelErrorCode::LabelNotFound,
                        "Label not found",
                    ));
                };
                let existing = &catalog[position];
                // A label keeping its own key is not colliding with itself, so case-only
                // edits pass.
                if let Some(new_name) = &new_name {
                    let to_key = workspace_label_key(new_name);
                    if to_key != from_key
                        && catalog
                            .iter()
                            .any(|l| workspace_label_key(&l.name) == to_key)
                    {
                        return Err(label_error(
                            LabelErrorCode::LabelNameTaken,
                            "A label with that name already exists",
                        ));
                    }
                }
                let name_changed = new_name.as_ref().is_some_and(|n| *n != existing.name);
                let color_changed = color.is_some_and(|c| c != existing.color);
                if !name_changed && !color_changed {
                    return Ok(LabelMutation {
                        labels: catalog.to_vec(),
                        workspace_updates: Vec::new(),
                        result: Commit {
                            label: existing.clone(),
                            affected: 0,
                            changed: false,
                            previous_name: None,
                        },
                    });
                }
                let mut label = existing.clone();
                if let (true, Some(n)) = (name_changed, &new_name) {
                    label.name = n.clone();
                }
                if let Some(c) = color {
                    label.color = c;
                }
                // Workspaces store names, so only a rename rewrites assignments.
                let workspace_updates = if name_changed {
                    rewrite_assignments(workspaces, &from_key, Some(&label.name))
                } else {
                    Vec::new()
                };
                let mut next_catalog = catalog.to_vec();
                next_catalog[position] = label.clone();
                Ok(LabelMutation {
                    labels: next_catalog,
                    result: Commit {
                        label,
                        affected: workspace_updates.len(),
                        changed: true,
                        previous_name: name_changed.then(|| existing.name.clone()),
                    },
                    workspace_updates,
                })
            })
            .await?;
        if committed.changed {
            self.sequence.publish(WorkspaceLabelChange::Upsert {
                label: committed.label.clone(),
                previous_name: committed.previous_name,
            });
        }
        Ok(UpdateResult {
            label: committed.label,
            affected_workspace_count: committed.affected,
        })
    }

    pub async fn delete(&self, name: &str) -> Result<usize> {
        let _op = self.operations.lock().await;
        let key = workspace_label_key(&require_name(name)?);
        struct Commit {
            affected: usize,
            deleted_name: Option<String>,
        }
        let committed = self
            .catalog
            .commit::<Commit, _>(move |catalog, workspaces| {
                let Some(position) = catalog
                    .iter()
                    .position(|l| workspace_label_key(&l.name) == key)
                else {
                    return Ok(LabelMutation {
                        labels: catalog.to_vec(),
                        workspace_updates: Vec::new(),
                        result: Commit {
                            affected: 0,
                            deleted_name: None,
                        },
                    });
                };
                let workspace_updates = rewrite_assignments(workspaces, &key, None);
                let mut next_catalog = catalog.to_vec();
                let removed = next_catalog.remove(position);
                Ok(LabelMutation {
                    labels: next_catalog,
                    result: Commit {
                        affected: workspace_updates.len(),
                        deleted_name: Some(removed.name),
                    },
                    workspace_updates,
                })
            })
            .await?;
        if let Some(name) = committed.deleted_name {
            self.sequence.publish(WorkspaceLabelChange::Remove { name });
        }
        Ok(committed.affected)
    }

    /// Counts the same (active and archived) workspaces a delete would rewrite.
    pub async fn count_affected_workspaces(&self, name: &str) -> Result<usize> {
        let _op = self.operations.lock().await;
        let key = workspace_label_key(&require_name(name)?);
        self.catalog
            .commit::<usize, _>(move |catalog, workspaces| {
                Ok(LabelMutation {
                    labels: catalog.to_vec(),
                    workspace_updates: Vec::new(),
                    result: workspaces
                        .values()
                        .filter(|w| workspace_has_label(w, &key))
                        .count(),
                })
            })
            .await
    }
}

/// `None` when the assignment already is what was asked for (TS returns the same array).
fn update_assignment_labels(
    current: &[String],
    key: &str,
    display_name: &str,
    assigned: bool,
) -> Option<Vec<String>> {
    let index = current.iter().position(|l| workspace_label_key(l) == key);
    if assigned {
        return match index {
            Some(_) => None,
            None => {
                let mut next = current.to_vec();
                next.push(display_name.to_string());
                Some(next)
            }
        };
    }
    index.map(|i| {
        current
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, l)| l.clone())
            .collect()
    })
}

fn workspace_has_label(workspace: &WorkspaceRecord, key: &str) -> bool {
    workspace
        .labels
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .any(|l| workspace_label_key(l) == key)
}

fn rewrite_assignments(
    workspaces: &RecordMap<WorkspaceRecord>,
    from_key: &str,
    to: Option<&str>,
) -> Vec<WorkspaceRecord> {
    let now = now_iso();
    workspaces
        .values()
        .filter(|w| workspace_has_label(w, from_key))
        .map(|workspace| {
            let labels = workspace.labels.as_deref().unwrap_or(&[]);
            let next: Vec<String> = labels
                .iter()
                .filter_map(|l| {
                    if workspace_label_key(l) != from_key {
                        Some(l.clone())
                    } else {
                        to.map(str::to_string)
                    }
                })
                .collect();
            let mut updated = workspace.clone();
            updated.labels = if next.is_empty() { None } else { Some(next) };
            updated.updated_at = now.clone();
            updated
        })
        .collect()
}
