//! Crash-safe selection save: capture what a plan can mutate, converge, then commit or
//! roll back. Ports `transaction.ts`.
//!
//! The on-disk format is byte-compatible with TS so a TS crash is recoverable by Rust and
//! the reverse: a `.paseo-skills-transaction-<uuid>` directory beside the skill roots with a
//! `transaction.json` (`owner: "paseo-skills-transaction"`, `version: 1`, phase
//! `capturing|converging`). A directory without a readable manifest of ours is never
//! touched: the prefix alone is a guess, and guessing here deletes someone's files.

use crate::error::{failed, Result};
use crate::fsx::{
    base_name, copy_entry, dir_name, is_directory, join_rel, lstat, mode_bits, path_string,
    read_dir_names, rm_rf,
};
use crate::operations::{list_managed_skill_names, OpKind, SkillOp, SkillTargets};
use crate::selection::SkillSelection;
use crate::sync::{list_files_recursive, manifest_text, MANAGED_FILES_MANIFEST};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const MANIFEST_OWNER: &str = "paseo-skills-transaction";
const MANIFEST_FILENAME: &str = "transaction.json";
pub const TRANSACTION_PREFIX: &str = ".paseo-skills-transaction-";
pub const RECOVERED_PREFIX: &str = ".paseo-skills-recovered-";
const BACKUP_DIRNAME: &str = "backup";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    /// Convergence never started, so there is nothing to undo.
    Capturing,
    Converging,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct CapturedDirectory {
    live_path: String,
    kind: OpKind,
    /// Relative to the manifest directory, or an absolute same-filesystem delete stage.
    backup_path: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TransactionManifest {
    owner: &'static str,
    version: u8,
    phase: Phase,
    previous_selection: SkillSelection,
    next_selection: SkillSelection,
    entries: Vec<CapturedDirectory>,
}

impl TransactionManifest {
    fn new(
        phase: Phase,
        previous: &SkillSelection,
        next: &SkillSelection,
        entries: &[CapturedDirectory],
    ) -> Self {
        Self {
            owner: MANIFEST_OWNER,
            version: 1,
            phase,
            previous_selection: previous.clone(),
            next_selection: next.clone(),
            entries: entries.to_vec(),
        }
    }
}

/// A started save. `commit` drops the staged copies; `rollback` puts every captured
/// directory back as it was, then drops the staging.
pub struct SkillsTransaction {
    targets: SkillTargets,
    transaction_dir: PathBuf,
    previous: SkillSelection,
    next: SkillSelection,
    entries: Vec<CapturedDirectory>,
}

impl SkillsTransaction {
    pub fn commit(self) -> Result<()> {
        discard_transaction(&self.transaction_dir, &self.entries)
    }

    pub fn rollback(self) -> Result<()> {
        let manifest =
            TransactionManifest::new(Phase::Converging, &self.previous, &self.next, &self.entries);
        restore(&self.targets, &self.transaction_dir, &manifest)?;
        discard_transaction(&self.transaction_dir, &self.entries)
    }
}

fn parse_selection(value: &Value) -> Option<SkillSelection> {
    let map = value.as_object()?;
    match map.get("mode")?.as_str()? {
        "all" => Some(SkillSelection::All),
        "custom" => {
            let skills = map.get("skills")?.as_array()?;
            let names: Option<Vec<String>> = skills
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect();
            Some(SkillSelection::Custom { skills: names? })
        }
        _ => None,
    }
}

fn parse_entry(value: &Value) -> Option<CapturedDirectory> {
    let map = value.as_object()?;
    let live_path = map.get("livePath")?.as_str()?.to_string();
    let kind = match map.get("kind")?.as_str()? {
        "add" => OpKind::Add,
        "update" => OpKind::Update,
        "delete" => OpKind::Delete,
        _ => return None,
    };
    let backup_path = match map.get("backupPath")? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        _ => return None,
    };
    Some(CapturedDirectory {
        live_path,
        kind,
        backup_path,
    })
}

/// `None` unless the manifest is readable and ours.
fn read_manifest(transaction_dir: &Path) -> Option<TransactionManifest> {
    let raw = fs::read_to_string(transaction_dir.join(MANIFEST_FILENAME)).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    let map = parsed.as_object()?;
    if map.get("owner")?.as_str()? != MANIFEST_OWNER {
        return None;
    }
    if map.get("version")?.as_f64()? != 1.0 {
        return None;
    }
    let phase = match map.get("phase")?.as_str()? {
        "capturing" => Phase::Capturing,
        "converging" => Phase::Converging,
        _ => return None,
    };
    let previous = parse_selection(map.get("previousSelection")?)?;
    let next = parse_selection(map.get("nextSelection")?)?;
    let entries: Option<Vec<CapturedDirectory>> = map
        .get("entries")?
        .as_array()?
        .iter()
        .map(parse_entry)
        .collect();
    Some(TransactionManifest {
        owner: MANIFEST_OWNER,
        version: 1,
        phase,
        previous_selection: previous,
        next_selection: next,
        entries: entries?,
    })
}

// PARITY-QUIRK(skills-manifest-plain-write): TS writes `transaction.json` with a plain
// `writeFile`, not atomically; a torn manifest parses as invalid and is treated as not ours.
/// Plain write, not atomic: a torn manifest is unreadable and therefore not ours.
fn write_manifest(transaction_dir: &Path, manifest: &TransactionManifest) -> Result<()> {
    let mut text = serde_json::to_string_pretty(manifest).map_err(std::io::Error::other)?;
    text.push('\n');
    fs::write(transaction_dir.join(MANIFEST_FILENAME), text)?;
    Ok(())
}

/// Owned by this transaction only if every entry is a managed name directly under one of
/// the three roots, with a backup path that is ours.
fn validate_entries(
    targets: &SkillTargets,
    transaction_dir: &Path,
    entries: &[CapturedDirectory],
) -> bool {
    let names: HashSet<String> = list_managed_skill_names(&targets.source_dir)
        .into_iter()
        .collect();
    let roots = targets.roots();
    entries.iter().all(|entry| {
        let live = Path::new(&entry.live_path);
        let parent = live.parent();
        let Some(root_index) = roots.iter().position(|root| Some(*root) == parent) else {
            return false;
        };
        let name = base_name(live);
        if !names.contains(&name) {
            return false;
        }
        let Some(backup_path) = &entry.backup_path else {
            return true;
        };
        let expected = Path::new(BACKUP_DIRNAME)
            .join(root_index.to_string())
            .join(&name);
        let same_filesystem_stage = roots[root_index]
            .join(base_name(transaction_dir))
            .join(&name);
        if entry.kind == OpKind::Delete && Path::new(backup_path) == same_filesystem_stage {
            return true;
        }
        Path::new(backup_path) == expected
            && transaction_dir
                .join(backup_path)
                .starts_with(transaction_dir)
    })
}

fn resolve_backup_path(transaction_dir: &Path, backup_path: Option<&str>) -> Option<PathBuf> {
    backup_path.map(|b| {
        let p = Path::new(b);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            transaction_dir.join(p)
        }
    })
}

fn discard_transaction(transaction_dir: &Path, entries: &[CapturedDirectory]) -> Result<()> {
    let mut staging_dirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        if let Some(backup) = &entry.backup_path {
            let p = Path::new(backup);
            if p.is_absolute() {
                let dir = dir_name(p);
                if !staging_dirs.contains(&dir) {
                    staging_dirs.push(dir);
                }
            }
        }
    }
    for dir in staging_dirs {
        rm_rf(&dir)?;
    }
    rm_rf(transaction_dir)?;
    Ok(())
}

/// Every regular file under `root` (relative, `/`-separated); unreadable dirs are empty.
fn list_all_files(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
        let Ok(read_dir) = fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = read_dir.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                walk(&entry.path(), &rel, out);
            } else if file_type.is_file() {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, "", &mut out);
    out
}

/// What a sync of bundled skill `name` writes: its files plus the managed-files manifest.
fn expected_synced_files(source_dir: &Path, name: &str) -> Result<HashMap<String, Vec<u8>>> {
    let skill_dir = source_dir.join(name);
    let mut files = HashMap::new();
    let mut hashes: Vec<(String, String)> = Vec::new();
    for rel in list_files_recursive(&skill_dir)? {
        let contents = fs::read(skill_dir.join(&rel))?;
        hashes.push((rel.clone(), crate::fsx::sha256_hex(&contents)));
        files.insert(rel, contents);
    }
    files.insert(
        MANAGED_FILES_MANIFEST.to_string(),
        manifest_text(&hashes).into_bytes(),
    );
    Ok(files)
}

/// Whether a file on disk is exactly what a sync would have written. The managed-files
/// manifest is compared as JSON: a TS-written one lists files in readdir order, ours in
/// sorted order, and both mean the same thing.
fn matches_expected(rel: &str, current: &[u8], expected: &[u8]) -> bool {
    if current == expected {
        return true;
    }
    if rel != MANAGED_FILES_MANIFEST {
        return false;
    }
    match (
        serde_json::from_slice::<Value>(current),
        serde_json::from_slice::<Value>(expected),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn copy_staged_entry(source: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    copy_entry(source, target)?;
    Ok(())
}

fn is_plain_dir(meta: &fs::Metadata) -> bool {
    meta.is_dir() && !meta.file_type().is_symlink()
}

/// Copies into `live_path` whatever the backup has that the live tree lacks. Returns true
/// when both sides have something different at the same path (a conflict the caller must
/// quarantine rather than overwrite).
fn merge_backup_directory(live_path: &Path, backup_path: Option<&Path>) -> Result<bool> {
    let Some(backup_path) = backup_path else {
        return Ok(false);
    };
    let destination = match lstat(live_path) {
        Some(m) if m.file_type().is_symlink() => fs::canonicalize(live_path)?,
        _ => live_path.to_path_buf(),
    };
    fs::create_dir_all(&destination)?;
    let mut has_conflict = false;

    fn merge_missing(
        source_dir: &Path,
        destination_dir: &Path,
        has_conflict: &mut bool,
    ) -> Result<()> {
        let mut names: Vec<_> = fs::read_dir(source_dir)?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .map(|e| e.file_name())
            .collect();
        names.sort();
        for name in names {
            let source = source_dir.join(&name);
            let target = destination_dir.join(&name);
            let source_info = fs::symlink_metadata(&source)?;
            let Some(target_info) = lstat(&target) else {
                copy_staged_entry(&source, &target)?;
                continue;
            };
            if is_plain_dir(&source_info) && is_plain_dir(&target_info) {
                if mode_bits(&source_info) != mode_bits(&target_info) {
                    *has_conflict = true;
                }
                merge_missing(&source, &target, has_conflict)?;
                continue;
            }
            if source_info.is_file() && target_info.is_file() {
                if fs::read(&source)? != fs::read(&target)?
                    || mode_bits(&source_info) != mode_bits(&target_info)
                {
                    *has_conflict = true;
                }
                continue;
            }
            if source_info.file_type().is_symlink() && target_info.file_type().is_symlink() {
                if fs::read_link(&source)? != fs::read_link(&target)? {
                    *has_conflict = true;
                }
                continue;
            }
            *has_conflict = true;
        }
        Ok(())
    }

    merge_missing(backup_path, &destination, &mut has_conflict)?;
    Ok(has_conflict)
}

fn recovery_base(transaction_dir: &Path, live_path: &Path, backup_path: &Path) -> PathBuf {
    let tx_name = base_name(transaction_dir);
    let transaction_id = tx_name.replacen(TRANSACTION_PREFIX, "", 1);
    // Update backups live inside the central transaction directory; delete stages live
    // beside their target root. Keep quarantine on the backup's filesystem so preserving
    // it stays an atomic rename across mounted agent homes.
    let recovery_parent = if backup_path.starts_with(transaction_dir) {
        dir_name(transaction_dir)
    } else {
        dir_name(live_path)
    };
    recovery_parent.join(format!(
        "{RECOVERED_PREFIX}{}-{transaction_id}",
        base_name(live_path)
    ))
}

fn find_quarantined_entry(
    transaction_dir: &Path,
    live_path: &Path,
    backup_path: &Path,
    claimed: &HashSet<PathBuf>,
) -> Option<PathBuf> {
    let base = recovery_base(transaction_dir, live_path, backup_path);
    let name = base_name(&base);
    let parent = dir_name(&base);
    let prefix = format!("{name}-");
    read_dir_names(&parent)
        .into_iter()
        .filter(|entry| *entry == name || entry.starts_with(&prefix))
        .map(|entry| parent.join(entry))
        .find(|entry| !claimed.contains(entry))
}

fn quarantine_staged_entry(
    transaction_dir: &Path,
    live_path: &Path,
    backup_path: &Path,
) -> Result<()> {
    let base = recovery_base(transaction_dir, live_path, backup_path);
    let mut destination = base.clone();
    let mut suffix = 1;
    while lstat(&destination).is_some() {
        destination = PathBuf::from(format!("{}-{suffix}", base.display()));
        suffix += 1;
    }
    fs::rename(backup_path, destination)?;
    Ok(())
}

fn restore_deleted_directory(
    transaction_dir: &Path,
    live_path: &Path,
    backup_path: Option<&Path>,
) -> Result<()> {
    let Some(backup_path) = backup_path else {
        return Ok(());
    };
    if lstat(live_path).is_none() {
        if let Some(parent) = live_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(backup_path, live_path)?;
        return Ok(());
    }
    let backup_info = fs::symlink_metadata(backup_path)?;
    if is_plain_dir(&backup_info) && is_directory(live_path) {
        let has_conflict = merge_backup_directory(live_path, Some(backup_path))?;
        if has_conflict {
            quarantine_staged_entry(transaction_dir, live_path, backup_path)?;
        }
        return Ok(());
    }
    // A concurrent writer recreated the path with an incompatible type. Keep its entry
    // live and move the staged original outside transaction cleanup so neither side is
    // overwritten and later controller operations can proceed.
    quarantine_staged_entry(transaction_dir, live_path, backup_path)
}

fn restore_staged_entry(live_root: &Path, backup_root: Option<&Path>, rel: &str) -> Result<()> {
    let live = join_rel(live_root, rel);
    let backup = backup_root.map(|b| join_rel(b, rel));
    rm_rf(&live)?;
    let Some(backup) = backup else {
        return Ok(());
    };
    if lstat(&backup).is_none() {
        return Ok(());
    }
    copy_staged_entry(&backup, &live)
}

fn undo_synced_directory(
    transaction_dir: &Path,
    live_path: &Path,
    backup_path: Option<&Path>,
    expected_files: &HashMap<String, Vec<u8>>,
) -> Result<()> {
    for rel in list_all_files(live_path) {
        let target = live_path.join(&rel);
        let Ok(current) = fs::read(&target) else {
            continue;
        };
        let Some(expected) = expected_files.get(&rel) else {
            continue;
        };
        if !matches_expected(&rel, &current, expected) {
            continue;
        }
        restore_staged_entry(live_path, backup_path, &rel)?;
    }

    // Sync can prune files that an older managed-files manifest owned. Restore missing
    // staged entries with their type and metadata, but never overwrite a path another
    // writer recreated.
    let has_conflict = merge_backup_directory(live_path, backup_path)?;
    if has_conflict {
        if let Some(backup) = backup_path {
            quarantine_staged_entry(transaction_dir, live_path, backup)?;
        }
    }
    // PARITY-QUIRK(skills-undo-removes-symlinked-dir): when the emptied live skill dir is a
    // symlink, TS `rm(livePath, {recursive})` removes the link itself, not just its contents.
    if fs::read_dir(live_path)
        .map(|mut rd| rd.next().is_none())
        .unwrap_or(true)
    {
        rm_rf(live_path)?;
    }
    Ok(())
}

fn restore(
    targets: &SkillTargets,
    transaction_dir: &Path,
    manifest: &TransactionManifest,
) -> Result<()> {
    // Nothing was converged yet, so the live tree is already the pre-save state.
    if manifest.phase == Phase::Capturing {
        return Ok(());
    }
    let mut expected_by_name: HashMap<String, HashMap<String, Vec<u8>>> = HashMap::new();
    let mut claimed_quarantines: HashSet<PathBuf> = HashSet::new();
    for entry in &manifest.entries {
        let live = Path::new(&entry.live_path);
        let backup = resolve_backup_path(transaction_dir, entry.backup_path.as_deref());
        if let Some(b) = &backup {
            if lstat(b).is_none() {
                // Atomic rename leaves either the live path or its stage. If neither is
                // present, an external actor deleted it; there is nothing this
                // transaction can restore. Other captured entries still need rollback.
                if entry.kind == OpKind::Delete {
                    continue;
                }
                if let Some(quarantined) =
                    find_quarantined_entry(transaction_dir, live, b, &claimed_quarantines)
                {
                    claimed_quarantines.insert(quarantined);
                    continue;
                }
                return Err(failed(format!(
                    "Skills transaction backup is missing: {}",
                    b.display()
                )));
            }
        }
        if entry.kind == OpKind::Delete {
            restore_deleted_directory(transaction_dir, live, backup.as_deref())?;
            continue;
        }
        let live_info = lstat(live);
        if live_info.is_some() && !is_directory(live) {
            // Add/update rollback has the same conflict state as delete rollback. A
            // recreated file or file symlink belongs to the external writer; preserve it,
            // and preserve any captured original outside transaction cleanup.
            if let Some(b) = &backup {
                quarantine_staged_entry(transaction_dir, live, b)?;
            }
            continue;
        }
        let backup_info = backup.as_deref().and_then(lstat);
        if let (Some(info), Some(b)) = (&backup_info, &backup) {
            if !info.is_dir() {
                if live_info.is_some() && is_directory(live) {
                    // The captured file was replaced by a directory after the transaction
                    // started. Preserve the external tree and quarantine the captured
                    // entry instead of recursively deleting the replacement.
                    quarantine_staged_entry(transaction_dir, live, b)?;
                    continue;
                }
                restore_staged_entry(live, Some(b), ".")?;
                continue;
            }
        }
        let name = base_name(live);
        if !expected_by_name.contains_key(&name) {
            let expected = expected_synced_files(&targets.source_dir, &name)?;
            expected_by_name.insert(name.clone(), expected);
        }
        let expected = &expected_by_name[&name];
        undo_synced_directory(transaction_dir, live, backup.as_deref(), expected)?;
    }
    Ok(())
}

fn transaction_parents(targets: &SkillTargets) -> Vec<PathBuf> {
    let mut parents: Vec<PathBuf> = Vec::new();
    for root in targets.roots() {
        let parent = dir_name(root);
        if !parents.contains(&parent) {
            parents.push(parent);
        }
    }
    parents
}

/// Finishes what an interrupted save started. A transaction directory only exists while a
/// save is mid-flight, so finding one means the process died between mutating the
/// directories and committing the selection; the staged copies are the only place the
/// user's files still exist. Restore them, then drop the transaction. Anything without our
/// manifest is left where it is.
pub fn recover_interrupted_skill_transactions(
    targets: &SkillTargets,
    committed_selection: &SkillSelection,
) -> Result<()> {
    for parent in transaction_parents(targets) {
        for entry in read_dir_names(&parent) {
            if !entry.starts_with(TRANSACTION_PREFIX) {
                continue;
            }
            let transaction_dir = parent.join(&entry);
            let Some(manifest) = read_manifest(&transaction_dir) else {
                continue;
            };
            if !validate_entries(targets, &transaction_dir, &manifest.entries) {
                continue;
            }
            if *committed_selection == manifest.next_selection {
                discard_transaction(&transaction_dir, &manifest.entries)?;
                continue;
            }
            if *committed_selection != manifest.previous_selection {
                return Err(failed(format!(
                    "Cannot safely recover interrupted skills transaction at {}",
                    transaction_dir.display()
                )));
            }
            // PARITY-QUIRK(skills-recovery-aborts-on-first-failure): the first failing
            // transaction aborts recovery of the remaining ones, as in TS.
            restore(targets, &transaction_dir, &manifest)?;
            discard_transaction(&transaction_dir, &manifest.entries)?;
        }
    }
    Ok(())
}

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Captures the managed skill directories so a failed save can put the machine back
/// exactly as it was, including files the user added inside them, which re-running an
/// install cannot reconstruct. Deletions are staged (renamed) before this returns.
pub fn begin_skills_transaction(
    targets: &SkillTargets,
    previous: &SkillSelection,
    next: &SkillSelection,
    ops: &[SkillOp],
) -> Result<SkillsTransaction> {
    let roots = targets.roots();
    let mut seen_roots: HashSet<PathBuf> = HashSet::new();
    let mut unique_roots: Vec<(&Path, usize)> = Vec::new();
    for (root_index, root) in roots.iter().enumerate() {
        let identity = fs::canonicalize(root).unwrap_or_else(|_| absolute(root));
        if !seen_roots.insert(identity) {
            continue;
        }
        unique_roots.push((root, root_index));
    }
    // Staged next to the skills tree rather than inside it: same filesystem, so a restore
    // is a local copy, and never a directory a skill scan walks into.
    let transaction_dir =
        dir_name(roots[0]).join(format!("{TRANSACTION_PREFIX}{}", uuid::Uuid::new_v4()));
    let mut entries: Vec<CapturedDirectory> = Vec::new();

    fs::create_dir_all(&transaction_dir)?;
    write_manifest(
        &transaction_dir,
        &TransactionManifest::new(Phase::Capturing, previous, next, &[]),
    )?;

    let capture = (|| -> Result<()> {
        for (root, root_index) in &unique_roots {
            for op in ops {
                let live_path = root.join(&op.name);
                let Some(live_info) = lstat(&live_path) else {
                    entries.push(CapturedDirectory {
                        live_path: path_string(&live_path),
                        kind: op.kind,
                        backup_path: None,
                    });
                    continue;
                };
                let backup_path = if op.kind == OpKind::Delete {
                    root.join(base_name(&transaction_dir)).join(&op.name)
                } else {
                    Path::new(BACKUP_DIRNAME)
                        .join(root_index.to_string())
                        .join(&op.name)
                };
                if op.kind != OpKind::Delete {
                    let capture_source =
                        if live_info.file_type().is_symlink() && is_directory(&live_path) {
                            fs::canonicalize(&live_path)?
                        } else {
                            live_path.clone()
                        };
                    copy_entry(&capture_source, &transaction_dir.join(&backup_path))?;
                }
                entries.push(CapturedDirectory {
                    live_path: path_string(&live_path),
                    kind: op.kind,
                    backup_path: Some(path_string(&backup_path)),
                });
            }
        }
        write_manifest(
            &transaction_dir,
            &TransactionManifest::new(Phase::Converging, previous, next, &entries),
        )
    })();
    if let Err(error) = capture {
        // Nothing has been converged yet, so failing to stage means failing the save
        // before it touches anything.
        discard_transaction(&transaction_dir, &entries)?;
        return Err(error);
    }

    // Each delete is staged inside its own skills root, so rename stays atomic even when
    // the agent homes are symlinks or mounts on different filesystems. Any writer holding
    // the old directory keeps writing into the staged inode; any writer recreating the old
    // path remains live and is merged on rollback. The later convergence plan
    // deliberately omits deletes.
    let stage = (|| -> Result<()> {
        for entry in &entries {
            if entry.kind != OpKind::Delete {
                continue;
            }
            let Some(backup) = resolve_backup_path(&transaction_dir, entry.backup_path.as_deref())
            else {
                continue;
            };
            if let Some(parent) = backup.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&entry.live_path, &backup)?;
        }
        Ok(())
    })();
    if let Err(error) = stage {
        restore(
            targets,
            &transaction_dir,
            &TransactionManifest::new(Phase::Converging, previous, next, &entries),
        )?;
        discard_transaction(&transaction_dir, &entries)?;
        return Err(error);
    }

    Ok(SkillsTransaction {
        targets: targets.clone(),
        transaction_dir,
        previous: previous.clone(),
        next: next.clone(),
        entries,
    })
}
