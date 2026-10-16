//! Paseo worktree helpers: `worktree.json` metadata, ownership checks and the
//! worktree directory layout (`utils/worktree.ts`, `utils/worktree-metadata.ts`).

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::{GitError, Result};
use crate::paths::{
    branch_name_from_ref, expand_tilde, node_resolve, parse_git_rev_parse_path,
    resolve_git_rev_parse_path,
};
use crate::run::{run_git, GitOptions};

const WORKTREE_PROJECT_HASH_LENGTH: usize = 8;

#[derive(Debug, Clone, Default)]
pub struct WorktreeRootOptions {
    pub paseo_home: Option<String>,
    pub worktrees_root: Option<String>,
}

/// `PASEO_HOME` (default `~/.paseo`), resolved to an absolute path.
pub fn resolve_paseo_home() -> String {
    let raw = std::env::var("PASEO_HOME").unwrap_or_else(|_| "~/.paseo".to_string());
    node_resolve(
        &std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        &expand_tilde(&raw),
    )
}

pub fn resolve_paseo_worktrees_base_root(options: &WorktreeRootOptions) -> String {
    let home = || match &options.paseo_home {
        Some(h) => node_resolve("/", h),
        None => resolve_paseo_home(),
    };
    if let Some(root) = &options.worktrees_root {
        let expanded = expand_tilde(root);
        if Path::new(&expanded).is_absolute() {
            return node_resolve("/", &expanded);
        }
        return node_resolve(&home(), &expanded);
    }
    Path::new(&home())
        .join("worktrees")
        .to_string_lossy()
        .into_owned()
}

/// Whether a path is under Paseo's worktree root.
pub fn is_paseo_worktree_path(p: &str, options: Option<&WorktreeRootOptions>) -> bool {
    if let Some(o) = options.filter(|o| o.worktrees_root.is_some() || o.paseo_home.is_some()) {
        return crate::paths::is_descendant_path(p, &resolve_paseo_worktrees_base_root(o));
    }
    let normalized = p.replace('\\', "/");
    normalized.contains("/.paseo/worktrees/")
}

/// First 8 base36 chars of the sha256-derived id (`deriveShortAlphanumericHash`).
pub fn derive_short_alphanumeric_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut n: u64 = 0;
    for b in digest.iter().take(8) {
        n = (n << 8) | u64::from(*b);
    }
    let mut digits = Vec::new();
    let mut v = n;
    if v == 0 {
        digits.push(b'0');
    }
    while v > 0 {
        let d = (v % 36) as u8;
        digits.push(if d < 10 { b'0' + d } else { b'a' + d - 10 });
        v /= 36;
    }
    digits.reverse();
    let mut s = String::from_utf8(digits).unwrap();
    while s.len() < 13 {
        s.insert(0, '0');
    }
    s[..WORKTREE_PROJECT_HASH_LENGTH].to_string()
}

fn normalize_for_ownership(input: &str) -> String {
    fs::canonicalize(input)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| node_resolve("/", input))
}

pub async fn get_git_common_dir(cwd: &str) -> Result<String> {
    let out = run_git(
        &["rev-parse", "--git-common-dir"],
        &GitOptions::new(cwd).env("GIT_OPTIONAL_LOCKS", "0"),
    )
    .await?;
    resolve_git_rev_parse_path(cwd, &out.stdout)
        .ok_or_else(|| GitError::other("Not in a git repository"))
}

pub async fn derive_worktree_project_hash(cwd: &str) -> String {
    match get_git_common_dir(cwd).await {
        Ok(common) => {
            let normalized = normalize_for_ownership(&common);
            let root = if Path::new(&normalized)
                .file_name()
                .is_some_and(|n| n == ".git")
            {
                Path::new(&normalized)
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or(normalized.clone())
            } else {
                normalized
            };
            derive_short_alphanumeric_hash(&root)
        }
        Err(_) => derive_short_alphanumeric_hash(&normalize_for_ownership(cwd)),
    }
}

pub async fn get_paseo_worktrees_root(cwd: &str, options: &WorktreeRootOptions) -> String {
    let base = resolve_paseo_worktrees_base_root(options);
    let hash = derive_worktree_project_hash(cwd).await;
    Path::new(&base).join(hash).to_string_lossy().into_owned()
}

pub async fn compute_worktree_path(cwd: &str, slug: &str, options: &WorktreeRootOptions) -> String {
    let root = get_paseo_worktrees_root(cwd, options).await;
    Path::new(&root).join(slug).to_string_lossy().into_owned()
}

/// Path of `candidate` relative to `root`, comparing raw and realpath variants.
pub fn realpath_aware_relative_path(root: &str, candidate: &str) -> Option<PathBuf> {
    fn variants(value: &str) -> Vec<PathBuf> {
        let mut out = vec![PathBuf::from(node_resolve("/", value))];
        // Canonicalize the longest existing ancestor and re-append the rest.
        let mut existing = PathBuf::from(value);
        let mut rest: Vec<std::ffi::OsString> = Vec::new();
        loop {
            if let Ok(real) = fs::canonicalize(&existing) {
                let mut full = real;
                for part in rest.iter().rev() {
                    full.push(part);
                }
                out.push(full);
                break;
            }
            match (
                existing.file_name().map(|n| n.to_os_string()),
                existing.parent().map(Path::to_path_buf),
            ) {
                (Some(name), Some(parent)) => {
                    rest.push(name);
                    existing = parent;
                }
                _ => break,
            }
        }
        out
    }
    for r in variants(root) {
        for c in variants(candidate) {
            if let Ok(rel) = c.strip_prefix(&r) {
                return Some(rel.to_path_buf());
            }
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaseoWorktreeOwnership {
    pub allowed: bool,
    pub worktree_root: Option<String>,
    pub worktree_path: String,
}

/// Ownership is defined by the path living under `<worktrees-root>/<hash>/<slug>[/...]`.
pub fn is_paseo_owned_worktree_cwd(
    cwd: &str,
    options: &WorktreeRootOptions,
) -> PaseoWorktreeOwnership {
    let resolved = normalize_for_ownership(cwd);
    let base = resolve_paseo_worktrees_base_root(options);
    let denied = |p: String| PaseoWorktreeOwnership {
        allowed: false,
        worktree_root: None,
        worktree_path: p,
    };
    let Some(relative) = realpath_aware_relative_path(&base, &resolved) else {
        return denied(resolved);
    };
    let parts: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() < 2 {
        return denied(resolved);
    }
    let root = Path::new(&base).join(&parts[0]);
    PaseoWorktreeOwnership {
        allowed: true,
        worktree_path: root.join(&parts[1]).to_string_lossy().into_owned(),
        worktree_root: Some(root.to_string_lossy().into_owned()),
    }
}

// ---- worktree.json ---------------------------------------------------------

fn git_dir_for_worktree_root(worktree_root: &str) -> Result<PathBuf> {
    let git_path = Path::new(worktree_root).join(".git");
    if !git_path.exists() {
        return Err(GitError::other(format!(
            "Not a git repository: {worktree_root}"
        )));
    }
    if let Ok(content) = fs::read_to_string(&git_path) {
        if let Some(rest) = content.split("gitdir:").nth(1) {
            let raw = rest.lines().next().unwrap_or("").trim();
            if !raw.is_empty() {
                return Ok(if Path::new(raw).is_absolute() {
                    PathBuf::from(raw)
                } else {
                    PathBuf::from(node_resolve(worktree_root, raw))
                });
            }
        }
    }
    Ok(git_path)
}

pub fn paseo_worktree_metadata_path(worktree_root: &str) -> Result<PathBuf> {
    Ok(git_dir_for_worktree_root(worktree_root)?
        .join("paseo")
        .join("worktree.json"))
}

/// Parsed `worktree.json`. The raw document is retained so unrelated fields
/// (runtime port, auto-name state, change-request hint) survive rewrites.
#[derive(Debug, Clone, PartialEq)]
pub struct PaseoWorktreeMetadata {
    pub version: u8,
    pub base_ref_name: String,
    pub base_ref: Option<String>,
    pub raw: Value,
}

impl PaseoWorktreeMetadata {
    /// Worktrees written before `baseRef` existed only stored the stripped
    /// name; it resolves local-first, which is the base they were cut from.
    pub fn stored_base_ref(&self) -> Option<&str> {
        self.base_ref
            .as_deref()
            .or(Some(self.base_ref_name.as_str()))
    }
}

pub fn stored_base_ref_from_metadata(metadata: Option<&PaseoWorktreeMetadata>) -> Option<String> {
    metadata.and_then(|m| m.stored_base_ref().map(str::to_string))
}

pub fn read_paseo_worktree_metadata(worktree_root: &str) -> Result<Option<PaseoWorktreeMetadata>> {
    let path = paseo_worktree_metadata_path(worktree_root)?;
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    let raw: Value = serde_json::from_str(&text).map_err(|e| GitError::other(e.to_string()))?;
    let version = raw.get("version").and_then(Value::as_u64);
    let base_ref_name = raw
        .get("baseRefName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let (Some(version @ (1 | 2)), Some(base_ref_name)) = (version, base_ref_name) else {
        return Err(GitError::other(format!(
            "Invalid Paseo worktree metadata: {}",
            path.display()
        )));
    };
    let base_ref = raw
        .get("baseRef")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(Some(PaseoWorktreeMetadata {
        version: version as u8,
        base_ref_name: base_ref_name.to_string(),
        base_ref,
        raw,
    }))
}

fn assert_valid_base_ref(value: &str) -> Result<()> {
    if value == "HEAD" {
        return Err(GitError::other("Base branch cannot be HEAD"));
    }
    if value.contains("..") || value.contains("@{") {
        return Err(GitError::other(format!("Invalid base branch: {value}")));
    }
    Ok(())
}

fn write_metadata_file(worktree_root: &str, doc: &Value) -> Result<()> {
    let path = paseo_worktree_metadata_path(worktree_root)?;
    let dir = path.parent().unwrap();
    fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(
        &tmp,
        format!(
            "{}\n",
            serde_json::to_string_pretty(doc).map_err(|e| GitError::other(e.to_string()))?
        ),
    )?;
    fs::rename(tmp, path)?;
    Ok(())
}

/// Write a version-1 `worktree.json` (`writePaseoWorktreeMetadata`).
pub fn write_paseo_worktree_metadata(
    worktree_root: &str,
    base_ref_name: &str,
    base_ref: Option<&str>,
) -> Result<()> {
    let name = base_ref_name.trim();
    if name.is_empty() {
        return Err(GitError::other("Base branch is required"));
    }
    let name = branch_name_from_ref(name);
    assert_valid_base_ref(&name)?;
    let mut doc = json!({"version": 1, "baseRefName": name});
    if let Some(r) = base_ref.map(str::trim).filter(|r| !r.is_empty()) {
        assert_valid_base_ref(r)?;
        doc["baseRef"] = json!(r);
    }
    write_metadata_file(worktree_root, &doc)
}

/// After a branch rename keep the change-request hint bound to the new local
/// branch (`rebindPaseoWorktreeChangeRequestHint`). Returns whether it wrote.
pub fn rebind_change_request_hint(
    worktree_root: &str,
    previous_branch: &str,
    current_branch: &str,
) -> Result<bool> {
    let Some(meta) = read_paseo_worktree_metadata(worktree_root)? else {
        return Ok(false);
    };
    let Some(target) = hint_for_branch(&meta.raw, previous_branch) else {
        return Ok(false);
    };
    let mut next_target = target.clone();
    let head_ref = target.get("headRef").and_then(Value::as_str);
    if head_ref == Some(previous_branch)
        && target.get("headRepositoryOwner").is_none()
        && target.get("changeRequestNumber").is_none()
    {
        next_target["headRef"] = json!(current_branch);
    }
    next_target["localBranchName"] = json!(current_branch);
    let mut doc = meta.raw.clone();
    doc["changeRequestLookupTarget"] = next_target;
    write_metadata_file(worktree_root, &doc)?;
    Ok(true)
}

fn hint_for_branch(raw: &Value, current_branch: &str) -> Option<Value> {
    let target = raw.get("changeRequestLookupTarget")?;
    if let Some(local) = target.get("localBranchName").and_then(Value::as_str) {
        return (local == current_branch).then(|| target.clone());
    }
    // COMPAT(change-request-local-branch): metadata before v0.2.5 omitted the local binding.
    let head_ref = target.get("headRef").and_then(Value::as_str)?;
    let mut canonical = vec![];
    if let Some(owner) = target.get("headRepositoryOwner").and_then(Value::as_str) {
        canonical.push(format!("{owner}/{head_ref}"));
        let lower = owner.trim().to_lowercase();
        if !lower.is_empty()
            && lower
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            canonical.push(format!("{lower}/{head_ref}"));
        }
    } else {
        canonical.push(head_ref.to_string());
    }
    canonical
        .iter()
        .any(|c| c == current_branch)
        .then(|| target.clone())
}

/// Used by callers that need the raw rev-parse parsing next to metadata.
pub fn parse_rev_parse(stdout: &str) -> Option<String> {
    parse_git_rev_parse_path(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_hash_is_stable_and_eight_chars() {
        let a = derive_short_alphanumeric_hash("/repo");
        assert_eq!(a.len(), 8);
        assert_eq!(a, derive_short_alphanumeric_hash("/repo"));
        assert_ne!(a, derive_short_alphanumeric_hash("/other"));
        assert!(a
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()));
    }

    #[test]
    fn project_hash_matches_values_computed_by_node() {
        // node: BigInt("0x" + sha256(path).hex.slice(0, 16)).toString(36).padStart(13, "0").slice(0, 8)
        assert_eq!(derive_short_alphanumeric_hash("/repo"), "1yv09vyl");
        assert_eq!(derive_short_alphanumeric_hash("/other"), "2wo061tx");
        assert_eq!(derive_short_alphanumeric_hash("/Users/x/proj"), "0kp6qjjl");
    }

    #[test]
    fn project_hash_matches_node_bigint_base36() {
        // node: BigInt("0x"+sha256("/repo").hex.slice(0,16)).toString(36).padStart(13,"0").slice(0,8)
        // sha256("/repo") starts with the bytes hashed below; verified by recomputing the first 8 bytes.
        let digest = Sha256::digest(b"/repo");
        let n = u64::from_be_bytes(digest[..8].try_into().unwrap());
        let mut s = String::new();
        let mut v = n;
        while v > 0 {
            let d = (v % 36) as u32;
            s.insert(0, char::from_digit(d, 36).unwrap());
            v /= 36;
        }
        let padded = format!("{s:0>13}");
        assert_eq!(derive_short_alphanumeric_hash("/repo"), padded[..8]);
    }

    #[test]
    fn worktree_path_classification() {
        let opts = WorktreeRootOptions {
            paseo_home: Some("/home/u/.paseo".into()),
            worktrees_root: None,
        };
        assert!(is_paseo_worktree_path(
            "/home/u/.paseo/worktrees/abc/feature",
            Some(&opts)
        ));
        assert!(!is_paseo_worktree_path(
            "/home/u/.paseo/worktrees",
            Some(&opts)
        ));
        assert!(is_paseo_worktree_path("/x/.paseo/worktrees/abc/f", None));
        assert!(!is_paseo_worktree_path("/x/other/worktrees/abc/f", None));
    }

    #[test]
    fn ownership_requires_hash_and_slug_segments() {
        let opts = WorktreeRootOptions {
            paseo_home: None,
            worktrees_root: Some("/nonexistent-wt-root".into()),
        };
        assert!(is_paseo_owned_worktree_cwd("/nonexistent-wt-root/hash/slug", &opts).allowed);
        let nested = is_paseo_owned_worktree_cwd("/nonexistent-wt-root/hash/slug/sub/dir", &opts);
        assert!(nested.allowed);
        assert_eq!(nested.worktree_path, "/nonexistent-wt-root/hash/slug");
        assert!(!is_paseo_owned_worktree_cwd("/nonexistent-wt-root/hash", &opts).allowed);
        assert!(!is_paseo_owned_worktree_cwd("/elsewhere/hash/slug", &opts).allowed);
    }
}
