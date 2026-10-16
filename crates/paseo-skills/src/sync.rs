//! File-level sync of one bundled skill into the agent skill roots. Ports `sync.ts`.
//!
//! `.paseo-managed-files.json` (`{version:1, files:{rel: sha256}}`) records which files
//! Paseo wrote, so a later sync prunes only files it owns and keeps user additions.

use crate::error::{failed, Result, SkillsError};
use crate::fsx::{self, lstat, rm_file_force, sha256_hex};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const MANAGED_FILES_MANIFEST: &str = ".paseo-managed-files.json";

/// The three roots a skill removal or legacy check touches.
#[derive(Debug, Clone, Copy)]
pub struct RemoveSkillTargets<'a> {
    pub agents_dir: &'a Path,
    pub claude_dir: &'a Path,
    pub codex_dir: &'a Path,
}

pub type SkillErrorCallback<'a> = dyn FnMut(&str, &SkillsError) + 'a;

pub struct SkillSyncOptions<'a> {
    pub source_dir: &'a Path,
    pub agents_dir: &'a Path,
    pub claude_dir: &'a Path,
    pub codex_dir: &'a Path,
    pub skill_names: &'a [String],
    /// When set, a failing skill is reported here and the loop continues; otherwise the
    /// first error aborts the sync.
    pub on_skill_error: Option<&'a mut SkillErrorCallback<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SkillSyncResult {
    pub changed_files: usize,
    pub processed_skills: usize,
}

/// Parsed `.paseo-managed-files.json`. Values stay raw JSON: a non-string hash never
/// matches, exactly as `currentHash !== previousHash` behaves in TS.
#[derive(Debug, Clone, PartialEq)]
pub struct ManagedFilesManifest {
    pub files: Vec<(String, Value)>,
}

impl ManagedFilesManifest {
    pub fn keys(&self) -> Vec<String> {
        self.files.iter().map(|(k, _)| k.clone()).collect()
    }
}

fn write_file_if_changed(src: &Path, dst: &Path) -> io::Result<bool> {
    let src_bytes = fs::read(src)?;
    if let Ok(dst_bytes) = fs::read(dst) {
        if dst_bytes == src_bytes {
            return Ok(false);
        }
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(dst, src_bytes)?;
    Ok(true)
}

/// Regular files under `root` as `/`-separated relative paths, sorted. Symlinks are
/// neither files nor directories here (Node `Dirent`), so they are skipped. The top-level
/// managed-files manifest is excluded.
pub fn list_files_recursive(root: &Path) -> io::Result<Vec<String>> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) -> io::Result<()> {
        let mut entries = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if rel == MANAGED_FILES_MANIFEST {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                walk(&entry.path(), &rel, out)?;
            } else if file_type.is_file() {
                out.push(rel);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, "", &mut out)?;
    Ok(out)
}

/// `readManagedFilesManifest`: any unreadable or malformed manifest is `None`.
pub fn read_managed_files_manifest(dst_dir: &Path) -> Option<ManagedFilesManifest> {
    let raw = fs::read_to_string(dst_dir.join(MANAGED_FILES_MANIFEST)).ok()?;
    if raw.is_empty() {
        return None;
    }
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    let object = parsed.as_object()?;
    if object.get("version").and_then(Value::as_f64) != Some(1.0) {
        return None;
    }
    // PARITY-QUIRK(skills-manifest-array-files): TS accepts `files: []` because
    // `typeof [] === "object"`; the array is treated as an object with index keys.
    let files = match object.get("files")? {
        Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        // `typeof [] === "object"` in TS, so an array passes with index keys.
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, v)| (i.to_string(), v.clone()))
            .collect(),
        _ => return None,
    };
    Some(ManagedFilesManifest { files })
}

fn hash_file(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|b| sha256_hex(&b))
}

pub(crate) fn manifest_text(files: &[(String, String)]) -> String {
    let mut map = Map::new();
    for (rel, hash) in files {
        map.insert(rel.clone(), Value::String(hash.clone()));
    }
    let mut root = Map::new();
    root.insert("version".into(), Value::from(1));
    root.insert("files".into(), Value::Object(map));
    let mut text = serde_json::to_string_pretty(&Value::Object(root)).unwrap_or_default();
    text.push('\n');
    text
}

fn write_manifest_if_changed(dst_dir: &Path, files: &[(String, String)]) -> io::Result<bool> {
    let next = manifest_text(files);
    let manifest_path = dst_dir.join(MANAGED_FILES_MANIFEST);
    if fs::read_to_string(&manifest_path).ok().as_deref() == Some(next.as_str()) {
        return Ok(false);
    }
    fs::create_dir_all(dst_dir)?;
    fs::write(manifest_path, next)?;
    Ok(true)
}

fn prune_empty_parent_dirs(root: &Path, rels: &[String]) {
    let mut dirs: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for rel in rels {
        let mut dir = Path::new(rel).parent().map(Path::to_path_buf);
        while let Some(d) = dir {
            if d.as_os_str().is_empty() {
                break;
            }
            let key = d.to_string_lossy().into_owned();
            if seen.insert(key.clone()) {
                dirs.push(key);
            }
            dir = d.parent().map(Path::to_path_buf);
        }
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.split('/').count()));
    for rel in dirs {
        let _ = fs::remove_dir(root.join(rel));
    }
}

/// JS `path.normalize` for a relative path.
fn normalize_rel(rel: &str) -> String {
    let absolute = rel.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if matches!(stack.last(), Some(last) if *last != "..") {
                    stack.pop();
                } else if !absolute {
                    stack.push("..");
                }
            }
            other => stack.push(other),
        }
    }
    let joined = stack.join("/");
    match (absolute, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

// PARITY-QUIRK(skills-root-symlink-unchecked): TS only lstat-checks path components below the
// skill directory, never the skill directory itself, so a symlinked skill dir is written through.
/// Rejects managed paths that escape the skill directory or pass through a symlink.
/// Only a missing path component ends the walk quietly; any other lstat error propagates.
pub(crate) fn assert_managed_paths_stay_inside_skill(root: &Path, rels: &[String]) -> Result<()> {
    for rel in rels {
        let normalized = normalize_rel(rel);
        if normalized.starts_with('/') || normalized == ".." || normalized.starts_with("../") {
            return Err(failed(format!(
                "Cannot sync managed path outside skill directory: {rel}"
            )));
        }
        let mut current: PathBuf = root.to_path_buf();
        for segment in normalized.split('/') {
            current.push(segment);
            match fs::symlink_metadata(&current) {
                Ok(meta) => {
                    if meta.file_type().is_symlink() {
                        return Err(failed(format!(
                            "Cannot sync through symbolic link: {}",
                            current.display()
                        )));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

fn sync_directory_files(src_dir: &Path, dst_dir: &Path) -> Result<usize> {
    let files = list_files_recursive(src_dir)?;
    let src_set: HashSet<&str> = files.iter().map(String::as_str).collect();
    let mut src_hashes: Vec<(String, String)> = Vec::new();
    for rel in &files {
        let bytes = fs::read(src_dir.join(rel))?;
        src_hashes.push((rel.clone(), sha256_hex(&bytes)));
    }
    assert_managed_paths_stay_inside_skill(dst_dir, &[MANAGED_FILES_MANIFEST.to_string()])?;
    let previous = read_managed_files_manifest(dst_dir);
    let mut checked = vec![MANAGED_FILES_MANIFEST.to_string()];
    checked.extend(files.iter().cloned());
    if let Some(prev) = &previous {
        checked.extend(prev.keys());
    }
    assert_managed_paths_stay_inside_skill(dst_dir, &checked)?;

    let mut changed = 0;
    for rel in &files {
        if write_file_if_changed(&src_dir.join(rel), &dst_dir.join(rel))? {
            changed += 1;
        }
    }

    let mut deleted: Vec<String> = Vec::new();
    if let Some(prev) = &previous {
        for (rel, previous_hash) in &prev.files {
            if src_set.contains(rel.as_str()) {
                continue;
            }
            let dst_path = dst_dir.join(rel);
            let current = hash_file(&dst_path);
            if current.as_deref() != previous_hash.as_str() {
                continue;
            }
            rm_file_force(&dst_path)?;
            deleted.push(rel.clone());
            changed += 1;
        }
    }
    prune_empty_parent_dirs(dst_dir, &deleted);
    if write_manifest_if_changed(dst_dir, &src_hashes)? {
        changed += 1;
    }
    Ok(changed)
}

/// `fs.rm(p, {recursive, force})` over the three roots; stops at the first hard error.
pub fn remove_skill(skill_name: &str, targets: &RemoveSkillTargets<'_>) -> Result<()> {
    for root in [targets.agents_dir, targets.claude_dir, targets.codex_dir] {
        fsx::rm_rf(&root.join(skill_name))?;
    }
    Ok(())
}

fn is_same_directory(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ra), Ok(rb)) => ra == rb,
        _ => false,
    }
}

/// Codex also discovers `.agents/skills`, so its dedicated copy is a leftover. When the
/// Codex skill directory resolves to the shared or Claude copy, it is that copy, and
/// retiring it would delete the install.
pub fn is_legacy_codex_copy(skill_name: &str, targets: &RemoveSkillTargets<'_>) -> bool {
    let codex_skill_dir = targets.codex_dir.join(skill_name);
    for root in [targets.agents_dir, targets.claude_dir] {
        if is_same_directory(&codex_skill_dir, &root.join(skill_name)) {
            return false;
        }
    }
    true
}

/// Removes the files a managed Codex copy still owns (unchanged hashes), keeps personal
/// files, then the manifest and, if empty, the directory.
fn retire_managed_skill(dst_dir: &Path) -> Result<usize> {
    let Some(info) = lstat(dst_dir) else {
        return Ok(0);
    };
    if !info.is_dir() {
        return Ok(0);
    }
    let Some(manifest) = read_managed_files_manifest(dst_dir) else {
        return Ok(0);
    };
    let mut to_check = vec![MANAGED_FILES_MANIFEST.to_string()];
    to_check.extend(manifest.keys());
    assert_managed_paths_stay_inside_skill(dst_dir, &to_check)?;
    let mut removed: Vec<String> = Vec::new();
    for (rel, previous_hash) in &manifest.files {
        let file_path = dst_dir.join(rel);
        if hash_file(&file_path).as_deref() != previous_hash.as_str() {
            continue;
        }
        fs::remove_file(&file_path)?;
        removed.push(rel.clone());
    }
    prune_empty_parent_dirs(dst_dir, &removed);
    fs::remove_file(dst_dir.join(MANAGED_FILES_MANIFEST))?;
    match fs::remove_dir(dst_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) if is_not_empty(&e) => {}
        Err(e) => return Err(e.into()),
    }
    Ok(removed.len() + 1)
}

fn is_not_empty(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::DirectoryNotEmpty
}

/// Syncs each named bundled skill into the agents and Claude roots and retires an
/// unchanged legacy Codex copy. Names missing from the bundle are skipped.
pub fn sync_skills(options: SkillSyncOptions<'_>) -> Result<SkillSyncResult> {
    let SkillSyncOptions {
        source_dir,
        agents_dir,
        claude_dir,
        codex_dir,
        skill_names,
        mut on_skill_error,
    } = options;
    let remove_targets = RemoveSkillTargets {
        agents_dir,
        claude_dir,
        codex_dir,
    };
    let mut result = SkillSyncResult::default();
    for skill_name in skill_names {
        let bundle_skill_dir = source_dir.join(skill_name);
        if !fsx::is_directory(&bundle_skill_dir) {
            continue;
        }
        // Counts accumulate as each step lands, so a skill that fails midway still
        // contributes the files it changed (TS `changedFiles +=` per step).
        let outcome = (|| -> Result<()> {
            result.changed_files +=
                sync_directory_files(&bundle_skill_dir, &agents_dir.join(skill_name))?;
            result.changed_files +=
                sync_directory_files(&bundle_skill_dir, &claude_dir.join(skill_name))?;
            // Retain user edits to the old dedicated Codex copy.
            if is_legacy_codex_copy(skill_name, &remove_targets) {
                result.changed_files += retire_managed_skill(&codex_dir.join(skill_name))?;
            }
            Ok(())
        })();
        match outcome {
            Ok(()) => result.processed_skills += 1,
            Err(error) => match on_skill_error.as_mut() {
                Some(callback) => callback(skill_name, &error),
                None => return Err(error),
            },
        }
    }
    Ok(result)
}
