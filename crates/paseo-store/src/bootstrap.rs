//! Registry bootstrap and the legacy `workspaceId` backfill
//! (`workspace-registry-bootstrap.ts`, `migrations/backfill-workspace-id.migration.ts`).
//!
//! Git/project-key classification belongs to other crates, so the host supplies it through
//! [`BootstrapHost`]. Everything else is ported as is.

use crate::agent_record::StoredAgentRecord;
use crate::agent_store::AgentStore;
use crate::error::Result;
use crate::paths::resolve_path;
use crate::project_registry::{generate_workspace_id, ProjectKind, ProjectRecord, ProjectRegistry};
use crate::time::{now_iso, parse_js_date};
use crate::workspace_registry::{WorkspaceKind, WorkspaceRecord, WorkspaceRegistry};
use async_trait::async_trait;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::path::Path;

/// `classifyDirectoryForProjectMembership` output for one directory.
#[derive(Debug, Clone)]
pub struct DirectoryMembership {
    pub workspace_directory_key: String,
    /// `membership.checkout.cwd`: the resolved cwd the workspace record is keyed on.
    pub workspace_cwd: String,
    pub workspace_kind: WorkspaceKind,
    pub workspace_display_name: String,
    pub project_id: String,
    pub project_key: String,
    pub project_name: String,
    pub project_root_path: String,
    pub project_kind: ProjectKind,
}

#[async_trait]
pub trait BootstrapHost: Send + Sync {
    /// `getCheckout(cwd)` + `classifyDirectoryForProjectMembership`. A failure aborts the
    /// bootstrap (a Git failure for an existing legacy directory propagates, as in TS).
    async fn classify_directory(&self, resolved_cwd: &str) -> Result<DirectoryMembership>;

    // COMPAT(worktree-branch-identity): added in v0.4.0 on 2026-08-15; remove after
    // 2027-02-15. Older worktrees did not pin branch-off/check-out branch identity.
    fn pin_worktree_branch_identity(&self, worktree_root: &str, branch: &str) -> Result<()>;
}

pub struct BootstrapOptions<'a> {
    pub paseo_home: &'a Path,
    pub agent_store: &'a AgentStore,
    pub project_registry: &'a ProjectRegistry,
    pub workspace_registry: &'a WorkspaceRegistry,
    pub host: &'a dyn BootstrapHost,
    /// Overrides `$HOME` for the backfill's "home never owns descendants" rule.
    pub user_home: Option<&'a str>,
}

fn truthy(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|s| !s.is_empty())
}

/// JS `Date.parse(left) <= Date.parse(right) ? left : right`; NaN compares false.
fn min_iso(left: Option<String>, right: Option<String>) -> Option<String> {
    pick_iso(left, right, |l, r| l <= r)
}

fn max_iso(left: Option<String>, right: Option<String>) -> Option<String> {
    pick_iso(left, right, |l, r| l >= r)
}

fn pick_iso(
    left: Option<String>,
    right: Option<String>,
    keep_left: impl Fn(i64, i64) -> bool,
) -> Option<String> {
    let left = left.filter(|s| !s.is_empty());
    let right = right.filter(|s| !s.is_empty());
    match (left, right) {
        (None, r) => r,
        (l, None) => l,
        (Some(l), Some(r)) => match (parse_js_date(&l), parse_js_date(&r)) {
            (Some(a), Some(b)) if keep_left(a, b) => Some(l),
            _ => Some(r),
        },
    }
}

fn epoch_iso() -> String {
    "1970-01-01T00:00:00.000Z".to_string()
}

fn agent_created_at(record: &StoredAgentRecord) -> String {
    [&record.created_at, &record.updated_at]
        .into_iter()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(epoch_iso)
}

fn agent_updated_at(record: &StoredAgentRecord) -> String {
    [
        record.last_activity_at.as_ref(),
        Some(&record.updated_at),
        Some(&record.created_at),
    ]
    .into_iter()
    .flatten()
    .find(|s| !s.is_empty())
    .cloned()
    .unwrap_or_else(epoch_iso)
}

fn is_archived(record: &StoredAgentRecord) -> bool {
    record.archived_at.value().is_some_and(|s| !s.is_empty())
}

/// Boot step "bootstrap registries": materialize `projects.json` / `workspaces.json` from
/// agent records only when either file is missing, then backfill `workspaceId` on every boot.
pub async fn bootstrap_registries(options: BootstrapOptions<'_>) -> Result<()> {
    let projects_exists = options.project_registry.exists_on_disk().await;
    let workspaces_exists = options.workspace_registry.exists_on_disk().await;
    options.project_registry.initialize().await;
    options.workspace_registry.initialize().await;

    for workspace in options.workspace_registry.list().await {
        if workspace.archived_at.is_some()
            || !workspace.is_paseo_owned_worktree
            || !truthy(&workspace.worktree_root)
            || !truthy(&workspace.branch)
        {
            continue;
        }
        let (root, branch) = (
            workspace.worktree_root.as_deref().unwrap_or_default(),
            workspace.branch.as_deref().unwrap_or_default(),
        );
        if let Err(error) = options.host.pin_worktree_branch_identity(root, branch) {
            tracing::warn!(
                workspace_id = %workspace.workspace_id,
                %error,
                "Failed to pin legacy worktree branch identity; PR association remains disabled"
            );
        }
    }

    if projects_exists && workspaces_exists {
        backfill_workspace_id_for_legacy_agents(
            options.agent_store,
            options.workspace_registry,
            options.user_home,
        )
        .await?;
        return Ok(());
    }

    let existing_ids_by_cwd: HashMap<String, String> = options
        .workspace_registry
        .list()
        .await
        .into_iter()
        .map(|w| (resolve_path(&w.cwd), w.workspace_id))
        .collect();
    let records = options.agent_store.list().await?;
    // A legacy agent can outlive its working directory. Reconciliation treats a missing
    // directory as absent rather than asking Git about it; bootstrap must do the same before
    // materializing its first workspace record.
    let active: Vec<&StoredAgentRecord> = records
        .iter()
        .filter(|r| !is_archived(r) && std::fs::metadata(&r.cwd).is_ok_and(|m| m.is_dir()))
        .collect();

    struct Group<'r> {
        membership: DirectoryMembership,
        records: Vec<&'r StoredAgentRecord>,
    }
    let mut groups: IndexMap<String, Group<'_>> = IndexMap::new();
    for record in active {
        let membership = options
            .host
            .classify_directory(&resolve_path(&record.cwd))
            .await?;
        let key = membership.workspace_directory_key.clone();
        groups
            .entry(key)
            .or_insert_with(|| Group {
                membership,
                records: Vec::new(),
            })
            .records
            .push(record);
    }

    struct UpsertInput {
        workspace_id: String,
        membership: DirectoryMembership,
        created_at: String,
        updated_at: String,
    }
    let mut project_ranges: IndexMap<String, (Option<String>, Option<String>)> = IndexMap::new();
    let mut inputs: Vec<UpsertInput> = Vec::new();
    for group in groups.values() {
        let membership = &group.membership;
        let (mut ws_created, mut ws_updated): (Option<String>, Option<String>) = (None, None);
        for record in &group.records {
            ws_created = min_iso(ws_created, Some(agent_created_at(record)));
            ws_updated = max_iso(ws_updated, Some(agent_updated_at(record)));
        }
        let created_at = ws_created.unwrap_or_else(now_iso);
        let updated_at = ws_updated.unwrap_or_else(|| created_at.clone());
        let range = project_ranges
            .entry(membership.project_key.clone())
            .or_insert((None, None));
        range.0 = min_iso(range.0.take(), Some(created_at.clone()));
        range.1 = max_iso(range.1.take(), Some(updated_at.clone()));
        inputs.push(UpsertInput {
            workspace_id: existing_ids_by_cwd
                .get(&membership.workspace_cwd)
                .cloned()
                .unwrap_or_else(generate_workspace_id),
            membership: membership.clone(),
            created_at,
            updated_at,
        });
    }

    for input in &inputs {
        let m = &input.membership;
        let range = project_ranges
            .get(&m.project_key)
            .cloned()
            .unwrap_or((None, None));
        options
            .workspace_registry
            .upsert(
                WorkspaceRecord::new(
                    &input.workspace_id,
                    &m.project_id,
                    &m.workspace_cwd,
                    m.workspace_kind,
                    &m.workspace_display_name,
                    &input.created_at,
                    &input.updated_at,
                ),
                None,
            )
            .await?;
        let mut project = ProjectRecord::new(
            &m.project_id,
            &m.project_root_path,
            m.project_kind,
            &m.project_name,
            range.0.as_deref().unwrap_or(&input.created_at),
            range.1.as_deref().unwrap_or(&input.updated_at),
        );
        project.project_key = Some(m.project_key.clone());
        options.project_registry.upsert(project).await?;
    }

    backfill_workspace_id_for_legacy_agents(
        options.agent_store,
        options.workspace_registry,
        options.user_home,
    )
    .await?;

    tracing::info!(
        projects_file = %options.project_registry.file_path().display(),
        workspaces_file = %options.workspace_registry.file_path().display(),
        materialized_projects = project_ranges.len(),
        materialized_workspaces = groups.len(),
        "Workspace registries bootstrapped from existing agent storage"
    );
    Ok(())
}

// COMPAT(workspaceIdBackfill): one-time legacy backfill, delete after 2026-12-16 once floor
// >= the release that always stamps workspaceId at create time.
//
// This is the only place that maps a cwd to a workspaceId. Everything else treats a record's
// `workspaceId` as authoritative ownership.

/// Picks the workspace that owned `cwd` for a legacy, unstamped agent record: an exact-cwd
/// workspace (oldest wins), else the deepest enclosing workspace directory, never letting the
/// home directory own descendants. Live records only consider live workspaces; archived
/// records can resolve to archived workspaces.
fn resolve_legacy_workspace_owner(
    cwd: &str,
    workspaces: &[WorkspaceRecord],
    user_home: &str,
    include_archived: bool,
) -> Option<String> {
    let normalized_cwd = resolve_path(cwd);
    let candidates: Vec<(&WorkspaceRecord, String)> = workspaces
        .iter()
        .filter(|w| include_archived || !truthy(&w.archived_at))
        .map(|w| (w, resolve_path(&w.cwd)))
        .collect();
    let exact: Vec<_> = candidates
        .iter()
        .filter(|(_, c)| *c == normalized_cwd)
        .collect();
    if !exact.is_empty() {
        return Some(
            oldest(exact.into_iter().map(|(w, _)| *w))
                .workspace_id
                .clone(),
        );
    }
    let prefix: Vec<_> = candidates
        .iter()
        .filter(|(_, c)| {
            if c == user_home {
                return false;
            }
            normalized_cwd == *c || normalized_cwd.starts_with(&format!("{c}/"))
        })
        .collect();
    let deepest = prefix.iter().map(|(_, c)| c.len()).max()?;
    Some(
        oldest(
            prefix
                .into_iter()
                .filter(|(_, c)| c.len() == deepest)
                .map(|(w, _)| *w),
        )
        .workspace_id
        .clone(),
    )
}

fn oldest<'a>(workspaces: impl Iterator<Item = &'a WorkspaceRecord>) -> &'a WorkspaceRecord {
    workspaces
        .reduce(|oldest, candidate| {
            if candidate.created_at < oldest.created_at {
                candidate
            } else {
                oldest
            }
        })
        .expect("non-empty candidate list")
}

/// Stamps `workspaceId` on legacy agent records. Returns how many were migrated.
pub async fn backfill_workspace_id_for_legacy_agents(
    agent_store: &AgentStore,
    workspace_registry: &WorkspaceRegistry,
    user_home: Option<&str>,
) -> Result<usize> {
    let home = match user_home {
        Some(home) => resolve_path(home),
        None => resolve_path(&std::env::var("HOME").unwrap_or_default()),
    };
    let workspaces = workspace_registry.list().await;
    let records = agent_store.list().await?;
    let mut migrated = 0usize;
    for record in records {
        if truthy(&record.workspace_id) {
            continue;
        }
        // `record.archivedAt != null`: any present value, even an empty string.
        let include_archived = record.archived_at.value().is_some();
        let Some(workspace_id) =
            resolve_legacy_workspace_owner(&record.cwd, &workspaces, &home, include_archived)
        else {
            continue;
        };
        let mut stamped = record;
        stamped.workspace_id = Some(workspace_id);
        agent_store.upsert(stamped).await?;
        migrated += 1;
    }
    if migrated > 0 {
        tracing::info!(migrated, "Backfilled workspaceId for legacy agent records");
    }
    Ok(migrated)
}
