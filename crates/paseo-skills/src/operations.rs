//! Scan, plan and apply: turns a selection plus the bundle plus what is on disk into
//! add/update/delete ops. Ports `operations.ts`.

use crate::error::Result;
use crate::fsx::{self, js_cmp, sha256_hex};
use crate::selection::SkillSelection;
use crate::sync::{
    is_legacy_codex_copy, list_files_recursive, read_managed_files_manifest, remove_skill,
    sync_skills, RemoveSkillTargets, SkillSyncOptions,
};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkillsState {
    NotInstalled,
    UpToDate,
    Drift,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OpKind {
    Add,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillOp {
    pub kind: OpKind,
    pub name: String,
}

impl SkillOp {
    pub fn new(kind: OpKind, name: impl Into<String>) -> Self {
        Self {
            kind,
            name: name.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillsStatus {
    pub state: SkillsState,
    pub ops: Vec<SkillOp>,
    /// Every skill the bundle currently ships, sorted. The selectable catalog.
    pub available: Vec<String>,
    /// Managed skills with a directory in at least one agent home, sorted. An `add` op
    /// means "missing from at least one target", so it cannot answer whether there is
    /// anything on disk to delete; this can.
    pub installed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillTargets {
    pub source_dir: PathBuf,
    pub agents_dir: PathBuf,
    pub claude_dir: PathBuf,
    pub codex_dir: PathBuf,
}

impl SkillTargets {
    pub fn roots(&self) -> [&Path; 3] {
        [&self.agents_dir, &self.claude_dir, &self.codex_dir]
    }

    pub fn remove_targets(&self) -> RemoveSkillTargets<'_> {
        RemoveSkillTargets {
            agents_dir: &self.agents_dir,
            claude_dir: &self.claude_dir,
            codex_dir: &self.codex_dir,
        }
    }
}

/// Names the bundle used to ship. They are never selectable, but every scan still covers
/// them so an older install's copies get cleaned up.
pub const LEGACY_SKILL_NAMES: [&str; 4] = [
    "paseo-chat",
    "paseo-epic",
    "paseo-orchestrate",
    "paseo-orchestrator",
];

type SkillFiles = HashMap<String, String>;
type TargetSkills = HashMap<String, SkillFiles>;

/// The bundle directory is the catalog. Reading it instead of a hardcoded list is what
/// makes `all` pick up skills added in a later release with no code change. Symlinked
/// entries are not directories here (Node `Dirent`), so they are not skills.
fn list_bundled_skills(source_dir: &Path) -> Vec<String> {
    let Ok(read_dir) = fs::read_dir(source_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read_dir
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_by(|a, b| js_cmp(a, b));
    names
}

/// Every name Paseo owns on disk: what it ships now plus what it used to ship.
fn managed_skill_names(available: &[String]) -> Vec<String> {
    let mut set: Vec<String> = available.to_vec();
    set.extend(LEGACY_SKILL_NAMES.iter().map(|s| s.to_string()));
    set.sort_by(|a, b| js_cmp(a, b));
    set.dedup();
    set
}

/// The names a convergence may create, replace, or delete.
pub fn list_managed_skill_names(source_dir: &Path) -> Vec<String> {
    managed_skill_names(&list_bundled_skills(source_dir))
}

fn resolve_desired_skills(selection: &SkillSelection, available: &[String]) -> HashSet<String> {
    match selection {
        SkillSelection::All => available.iter().cloned().collect(),
        SkillSelection::Custom { skills } => {
            let chosen: HashSet<&String> = skills.iter().collect();
            available
                .iter()
                .filter(|n| chosen.contains(n))
                .cloned()
                .collect()
        }
    }
}

fn hash_skill_dir(skill_dir: &Path) -> Result<Option<SkillFiles>> {
    if !fsx::is_directory(skill_dir) {
        return Ok(None);
    }
    let mut files = SkillFiles::new();
    for rel in list_files_recursive(skill_dir)? {
        let bytes = fs::read(skill_dir.join(&rel))?;
        files.insert(rel, sha256_hex(&bytes));
    }
    Ok(Some(files))
}

fn hash_skills(root: &Path, names: &[String]) -> Result<TargetSkills> {
    let mut out = TargetSkills::new();
    for name in names {
        if let Some(files) = hash_skill_dir(&root.join(name))? {
            out.insert(name.clone(), files);
        }
    }
    Ok(out)
}

fn bundle_files_match(bundle: &SkillFiles, disk: &SkillFiles) -> bool {
    bundle.iter().all(|(rel, sha)| disk.get(rel) == Some(sha))
}

fn diff(
    bundle: &TargetSkills,
    disks: &[&TargetSkills],
    names: &[String],
    desired: &HashSet<String>,
) -> Vec<SkillOp> {
    let mut ops = Vec::new();
    for name in names {
        let b = if desired.contains(name) {
            bundle.get(name)
        } else {
            None
        };
        let installed: Vec<&SkillFiles> = disks.iter().filter_map(|d| d.get(name)).collect();
        if let Some(b) = b {
            if installed.len() < disks.len() {
                ops.push(SkillOp::new(OpKind::Add, name));
            } else if installed.iter().any(|files| !bundle_files_match(b, files)) {
                ops.push(SkillOp::new(OpKind::Update, name));
            }
        } else if !installed.is_empty() {
            ops.push(SkillOp::new(OpKind::Delete, name));
        }
    }
    ops.sort_by(|a, b| js_cmp(&a.name, &b.name));
    ops
}

pub fn get_skills_status(
    targets: &SkillTargets,
    selection: &SkillSelection,
) -> Result<SkillsStatus> {
    let available = list_bundled_skills(&targets.source_dir);
    let names = managed_skill_names(&available);
    let bundle = hash_skills(&targets.source_dir, &available)?;
    let agents_disk = hash_skills(&targets.agents_dir, &names)?;
    let claude_disk = hash_skills(&targets.claude_dir, &names)?;
    let codex_disk = hash_skills(&targets.codex_dir, &names)?;
    let desired = resolve_desired_skills(selection, &available);
    let mut ops = diff(&bundle, &[&agents_disk, &claude_disk], &names, &desired);
    let remove_targets = targets.remove_targets();
    // Map iteration order in TS is insertion order of `names` (sorted), so walk `names`.
    for name in names.iter().filter(|n| codex_disk.contains_key(*n)) {
        if ops.iter().any(|op| &op.name == name) {
            continue;
        }
        if !is_legacy_codex_copy(name, &remove_targets) {
            continue;
        }
        if !desired.contains(name) {
            ops.push(SkillOp::new(OpKind::Delete, name));
        } else if read_managed_files_manifest(&targets.codex_dir.join(name)).is_some() {
            ops.push(SkillOp::new(OpKind::Update, name));
        }
    }
    ops.sort_by(|a, b| js_cmp(&a.name, &b.name));
    let all_disks = [&agents_disk, &claude_disk, &codex_disk];
    let installed: Vec<String> = names
        .iter()
        .filter(|n| all_disks.iter().any(|d| d.contains_key(*n)))
        .cloned()
        .collect();

    let state = if all_disks.iter().all(|d| d.is_empty()) {
        SkillsState::NotInstalled
    } else if ops.is_empty() {
        SkillsState::UpToDate
    } else {
        SkillsState::Drift
    };
    Ok(SkillsStatus {
        state,
        ops,
        available,
        installed,
    })
}

fn apply_skills(
    targets: &SkillTargets,
    selection: &SkillSelection,
    initial: Option<SkillsStatus>,
) -> Result<SkillsStatus> {
    let status = match initial {
        Some(status) => status,
        None => get_skills_status(targets, selection)?,
    };
    let writes: Vec<String> = status
        .ops
        .iter()
        .filter(|op| matches!(op.kind, OpKind::Add | OpKind::Update))
        .map(|op| op.name.clone())
        .collect();
    if !writes.is_empty() {
        sync_skills(SkillSyncOptions {
            source_dir: &targets.source_dir,
            agents_dir: &targets.agents_dir,
            claude_dir: &targets.claude_dir,
            codex_dir: &targets.codex_dir,
            skill_names: &writes,
            on_skill_error: None,
        })?;
    }
    for op in status.ops.iter().filter(|op| op.kind == OpKind::Delete) {
        remove_skill(&op.name, &targets.remove_targets())?;
    }
    get_skills_status(targets, selection)
}

/// Applies `plan` as given (no rescan) so a confirmed plan is the applied plan.
pub fn install_skills(
    targets: &SkillTargets,
    selection: &SkillSelection,
    plan: Option<SkillsStatus>,
) -> Result<SkillsStatus> {
    apply_skills(targets, selection, plan)
}

fn non_destructive_plan(status: SkillsStatus) -> SkillsStatus {
    SkillsStatus {
        ops: status
            .ops
            .into_iter()
            .filter(|op| op.kind != OpKind::Delete)
            .collect(),
        ..status
    }
}

pub fn update_skills(targets: &SkillTargets, selection: &SkillSelection) -> Result<SkillsStatus> {
    let status = get_skills_status(targets, selection)?;
    apply_skills(targets, selection, Some(non_destructive_plan(status)))
}

pub fn auto_update_installed_skills(
    targets: &SkillTargets,
    selection: &SkillSelection,
) -> Result<SkillsStatus> {
    let status = get_skills_status(targets, selection)?;
    if status.state != SkillsState::Drift {
        return Ok(status);
    }
    // Automatic maintenance may repair selected skills, but removal is an interactive
    // operation because managed directories can contain user files.
    apply_skills(targets, selection, Some(non_destructive_plan(status)))
}

pub fn uninstall_skills(
    targets: &SkillTargets,
    selection: &SkillSelection,
) -> Result<SkillsStatus> {
    let available = list_bundled_skills(&targets.source_dir);
    for name in managed_skill_names(&available) {
        remove_skill(&name, &targets.remove_targets())?;
    }
    get_skills_status(targets, selection)
}
