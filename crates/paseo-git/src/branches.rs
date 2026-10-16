//! Branch operations: suggestions, resolution, checkout, create, rename and
//! their wire payloads.

use std::cmp::Ordering;
use std::collections::HashMap;

use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};

use crate::args;
use crate::error::{to_checkout_error, GitError, Result};
use crate::repo::{
    get_current_branch, is_working_tree_dirty, local_branch_exists, require_git_repo,
    require_git_worktree_root, ro, LONG_TIMEOUT_MS,
};
use crate::run::{run_git, GitOptions};
use crate::worktree_meta::rebind_change_request_hint;

pub fn normalize_branch_suggestion_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut n = trimmed;
    if let Some(r) = n.strip_prefix("refs/heads/") {
        n = r;
    } else if let Some(r) = n.strip_prefix("refs/remotes/") {
        n = r;
    }
    if let Some(r) = n.strip_prefix("origin/") {
        n = r;
    }
    if n.is_empty() || n == "HEAD" || n == "origin" {
        return None;
    }
    Some(n.to_string())
}

struct GitRef {
    name: String,
    committer_date: i64,
    oid: String,
}

async fn list_git_refs(cwd: &str, prefix: &str) -> Result<Vec<GitRef>> {
    let out = run_git(
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)%09%(committerdate:unix)%09%(objectname)",
            prefix,
        ],
        &ro(cwd),
    )
    .await?;
    Ok(out
        .stdout
        .split('\n')
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let mut p = trimmed.split('\t');
            let name = p.next()?;
            let date = p.next().unwrap_or("");
            let oid = p.next()?;
            if name.is_empty() || oid.is_empty() {
                return None;
            }
            Some(GitRef {
                name: name.into(),
                committer_date: date.parse().unwrap_or(0),
                oid: oid.into(),
            })
        })
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSuggestion {
    pub name: String,
    pub committer_date: i64,
    pub has_local: bool,
    pub has_remote: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_ahead: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_behind: Option<u64>,
}

#[derive(Default, Clone)]
struct Meta {
    committer_date: i64,
    has_local: bool,
    has_remote: bool,
    local_oid: Option<String>,
    remote_oid: Option<String>,
}

/// Approximation of ICU root collation for `String.prototype.localeCompare`:
/// punctuation < digits < letters, case-insensitive first, lowercase before
/// uppercase as the tie-break.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    const PUNCT: &str = " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    fn primary(c: char) -> (u8, u32) {
        if let Some(i) = PUNCT.find(c) {
            return (0, i as u32);
        }
        if c.is_ascii_digit() {
            return (1, c as u32);
        }
        let lower = c.to_lowercase().next().unwrap_or(c);
        (2, lower as u32)
    }
    let ka: Vec<_> = a.chars().map(primary).collect();
    let kb: Vec<_> = b.chars().map(primary).collect();
    ka.cmp(&kb).then_with(|| {
        for (x, y) in a.chars().zip(b.chars()) {
            if x != y {
                // lowercase sorts before its uppercase form
                return if x.is_lowercase() {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
        }
        Ordering::Equal
    })
}

fn sort_branch_suggestions(names: &mut [String], meta: &HashMap<String, Meta>, query: &str) {
    let q = query.trim().to_lowercase();
    let has_query = !q.is_empty();
    names.sort_by(|a, b| {
        if has_query {
            let ap = a.to_lowercase().starts_with(&q);
            let bp = b.to_lowercase().starts_with(&q);
            if ap != bp {
                return if ap {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
        }
        let ad = meta.get(a).map_or(0, |m| m.committer_date);
        let bd = meta.get(b).map_or(0, |m| m.committer_date);
        if ad != bd {
            return bd.cmp(&ad);
        }
        locale_compare(a, b)
    });
}

pub async fn list_branch_suggestions(
    cwd: &str,
    query: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<BranchSuggestion>> {
    require_git_repo(cwd).await?;
    let limit = limit.unwrap_or(50).clamp(1, 200);
    let raw_query = query.map(|q| q.trim().to_lowercase()).unwrap_or_default();
    let query = normalize_branch_suggestion_name(&raw_query).unwrap_or(raw_query);

    let (local_refs, remote_refs) = tokio::join!(
        list_git_refs(cwd, "refs/heads"),
        list_git_refs(cwd, "refs/remotes/origin")
    );
    let (local_refs, remote_refs) = (local_refs?, remote_refs?);

    let mut meta: HashMap<String, Meta> = HashMap::new();
    for r in &local_refs {
        let Some(n) = normalize_branch_suggestion_name(&r.name) else {
            continue;
        };
        let e = meta.entry(n).or_default();
        e.has_local = true;
        e.local_oid = Some(r.oid.clone());
        e.committer_date = e.committer_date.max(r.committer_date);
    }
    for r in &remote_refs {
        let Some(n) = normalize_branch_suggestion_name(&r.name) else {
            continue;
        };
        let fresh = !meta.contains_key(&n);
        let e = meta.entry(n).or_default();
        e.has_remote = true;
        e.remote_oid = Some(r.oid.clone());
        e.committer_date = if fresh {
            r.committer_date
        } else {
            e.committer_date.max(r.committer_date)
        };
    }
    let mut names: Vec<String> = meta
        .keys()
        .filter(|n| query.is_empty() || n.to_lowercase().contains(&query))
        .cloned()
        .collect();
    if names.is_empty() {
        return Ok(vec![]);
    }
    // Deterministic start so equal sort keys stay stable.
    names.sort();
    sort_branch_suggestions(&mut names, &meta, &query);

    let mut result = Vec::new();
    for name in names.into_iter().take(limit) {
        let m = meta.get(&name).cloned().unwrap_or_default();
        let mut s = BranchSuggestion {
            name: name.clone(),
            committer_date: m.committer_date,
            has_local: m.has_local,
            has_remote: m.has_remote,
            local_ahead: None,
            local_behind: None,
        };
        if s.has_local && s.has_remote {
            if m.local_oid.is_some() && m.local_oid == m.remote_oid {
                s.local_ahead = Some(0);
                s.local_behind = Some(0);
            } else if let Ok(out) = run_git(
                &args![
                    "rev-list",
                    "--left-right",
                    "--count",
                    format!("refs/heads/{name}...refs/remotes/origin/{name}")
                ],
                &ro(cwd),
            )
            .await
            {
                // A ref may vanish between listing and comparison; keep the branch without metadata.
                let mut it = out.stdout.split_whitespace();
                if let (Some(Ok(a)), Some(Ok(b))) = (
                    it.next().map(str::parse::<u64>),
                    it.next().map(str::parse::<u64>),
                ) {
                    s.local_ahead = Some(a);
                    s.local_behind = Some(b);
                }
            }
        }
        result.push(s);
    }
    Ok(result)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchCheckoutResolution {
    Local { name: String },
    RemoteOnly { name: String, remote_ref: String },
    NotFound,
}

pub async fn resolve_branch_checkout(cwd: &str, name: &str) -> Result<BranchCheckoutResolution> {
    require_git_repo(cwd).await?;
    let Some(normalized) = normalize_branch_suggestion_name(name) else {
        return Ok(BranchCheckoutResolution::NotFound);
    };
    let local = run_git(
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{normalized}"),
        ],
        &ro(cwd).accept(&[0, 1]),
    )
    .await?;
    if local.exit_code == Some(0) {
        return Ok(BranchCheckoutResolution::Local { name: normalized });
    }
    let remote_ref = format!("origin/{normalized}");
    let remote = run_git(
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/{remote_ref}"),
        ],
        &ro(cwd).accept(&[0, 1]),
    )
    .await?;
    if remote.exit_code == Some(0) {
        return Ok(BranchCheckoutResolution::RemoteOnly {
            name: normalized,
            remote_ref,
        });
    }
    Ok(BranchCheckoutResolution::NotFound)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BranchCheckoutSource {
    Local,
    Remote,
}

pub async fn checkout_resolved_branch(
    cwd: &str,
    resolution: &BranchCheckoutResolution,
    requested_branch: Option<&str>,
) -> Result<BranchCheckoutSource> {
    match resolution {
        BranchCheckoutResolution::Local { name } => {
            let out = run_git(
                &["rev-parse", "--abbrev-ref", "HEAD"],
                &GitOptions::new(cwd),
            )
            .await?;
            if out.stdout.trim() == name {
                return Ok(BranchCheckoutSource::Local);
            }
            run_git(&["checkout", name], &GitOptions::new(cwd)).await?;
            Ok(BranchCheckoutSource::Local)
        }
        BranchCheckoutResolution::RemoteOnly { name, remote_ref } => {
            run_git(
                &["checkout", "-b", name, "--track", remote_ref],
                &GitOptions::new(cwd),
            )
            .await?;
            Ok(BranchCheckoutSource::Remote)
        }
        BranchCheckoutResolution::NotFound => Err(GitError::other(format!(
            "Branch not found: {}",
            requested_branch.unwrap_or("unknown")
        ))),
    }
}

pub fn assert_safe_git_ref(r: &str, label: &str) -> Result<()> {
    let ok = !r.is_empty()
        && r.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
        && !r.contains("..")
        && !r.contains("@{");
    if ok {
        Ok(())
    } else {
        Err(GitError::other(format!("Invalid {label}: {r}")))
    }
}

async fn ensure_clean_working_tree(cwd: &str) -> Result<()> {
    let dirty = is_working_tree_dirty(cwd)
        .await
        .map_err(|e| GitError::other(format!("Unable to inspect git status for {cwd}: {e}")))?;
    if dirty {
        return Err(GitError::other(
            "Working directory has uncommitted changes. Commit or stash before switching branches.",
        ));
    }
    Ok(())
}

/// `checkout_switch_branch_request` without `create`.
pub async fn checkout_existing_branch(cwd: &str, branch: &str) -> Result<BranchCheckoutSource> {
    assert_safe_git_ref(branch, "branch")?;
    let resolution = resolve_branch_checkout(cwd, branch).await?;
    if resolution == BranchCheckoutResolution::NotFound {
        return Err(GitError::other(format!("Branch not found: {branch}")));
    }
    ensure_clean_working_tree(cwd).await?;
    checkout_resolved_branch(cwd, &resolution, None).await
}

/// `checkout_switch_branch_request` with `create`.
pub async fn checkout_new_branch(cwd: &str, new_branch_name: &str) -> Result<BranchCheckoutSource> {
    assert_safe_git_ref(new_branch_name, "new branch")?;
    if local_branch_exists(cwd, new_branch_name).await? {
        return Err(GitError::other(format!(
            "Branch already exists: {new_branch_name}"
        )));
    }
    run_git(
        &["checkout", "-b", new_branch_name],
        &GitOptions::new(cwd).timeout_ms(LONG_TIMEOUT_MS),
    )
    .await?;
    Ok(BranchCheckoutSource::Local)
}

/// Create a branch from `base_branch` without inheriting its upstream
/// (`--no-track`): a branch we create is not a continuation of its base.
pub async fn create_branch_from_base(
    cwd: &str,
    base_branch: &str,
    new_branch_name: &str,
) -> Result<()> {
    assert_safe_git_ref(base_branch, "base branch")?;
    assert_safe_git_ref(new_branch_name, "new branch")?;
    if resolve_branch_checkout(cwd, base_branch).await? == BranchCheckoutResolution::NotFound {
        return Err(GitError::other(format!(
            "Base branch not found: {base_branch}"
        )));
    }
    if local_branch_exists(cwd, new_branch_name).await? {
        return Err(GitError::other(format!(
            "Branch already exists: {new_branch_name}"
        )));
    }
    ensure_clean_working_tree(cwd).await?;
    run_git(
        &["checkout", "-b", new_branch_name, "--no-track", base_branch],
        &GitOptions::new(cwd).timeout_ms(LONG_TIMEOUT_MS),
    )
    .await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameResult {
    pub previous_branch: Option<String>,
    pub current_branch: Option<String>,
}

pub async fn rename_current_branch(cwd: &str, new_name: &str) -> Result<RenameResult> {
    let worktree_root = require_git_worktree_root(cwd).await?;
    let previous = get_current_branch(cwd).await;
    let Some(previous_branch) = previous.filter(|b| b != "HEAD") else {
        return Err(GitError::other(
            "Cannot rename branch in detached HEAD state",
        ));
    };
    run_git(
        &["branch", "-m", new_name],
        &GitOptions::new(cwd).timeout_ms(LONG_TIMEOUT_MS),
    )
    .await?;
    let current_branch = get_current_branch(cwd).await;
    if let Some(current) = &current_branch {
        rebind_change_request_hint(&worktree_root, &previous_branch, current)?;
    }
    Ok(RenameResult {
        previous_branch: Some(previous_branch),
        current_branch,
    })
}

/// `validateBranchSlug` from the protocol package (used for rename requests).
pub fn validate_branch_slug(slug: &str) -> std::result::Result<(), String> {
    if slug.is_empty() {
        return Err("Branch name cannot be empty".into());
    }
    if slug.chars().count() > 100 {
        return Err("Branch name too long (max 100 characters)".into());
    }
    let valid = Regex::new(r"^[a-z0-9\-/]+$").unwrap();
    if !valid.is_match(slug) {
        return Err("Branch name must contain only lowercase letters, numbers, hyphens, and forward slashes".into());
    }
    if slug.starts_with('-') || slug.ends_with('-') {
        return Err("Branch name cannot start or end with a hyphen".into());
    }
    if slug.contains("--") {
        return Err("Branch name cannot have consecutive hyphens".into());
    }
    Ok(())
}

// ---- wire payloads -----------------------------------------------------------

pub fn build_branch_suggestions_payload(
    request_id: &str,
    result: &Result<Vec<BranchSuggestion>>,
) -> Value {
    match result {
        Ok(details) => json!({
            "branches": details.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
            "branchDetails": details, "error": null, "requestId": request_id,
        }),
        Err(e) => {
            json!({"branches": [], "branchDetails": [], "error": e.to_string(), "requestId": request_id})
        }
    }
}

/// `validate_branch_response` payload (the handler also runs `assert_safe_git_ref`).
pub fn build_validate_branch_payload(
    request_id: &str,
    result: &Result<BranchCheckoutResolution>,
) -> Value {
    match result {
        Ok(BranchCheckoutResolution::Local { name }) => {
            json!({"exists": true, "resolvedRef": name, "isRemote": false, "error": null, "requestId": request_id})
        }
        Ok(BranchCheckoutResolution::RemoteOnly { remote_ref, .. }) => {
            json!({"exists": true, "resolvedRef": remote_ref, "isRemote": true, "error": null, "requestId": request_id})
        }
        Ok(BranchCheckoutResolution::NotFound) => {
            json!({"exists": false, "resolvedRef": null, "isRemote": false, "error": null, "requestId": request_id})
        }
        Err(e) => {
            json!({"exists": false, "resolvedRef": null, "isRemote": false, "error": e.to_string(), "requestId": request_id})
        }
    }
}

pub fn build_switch_branch_payload(
    cwd: &str,
    branch: &str,
    request_id: &str,
    result: &Result<BranchCheckoutSource>,
) -> Value {
    match result {
        Ok(source) => {
            json!({"cwd": cwd, "success": true, "branch": branch, "source": source, "error": null, "requestId": request_id})
        }
        Err(e) => {
            json!({"cwd": cwd, "success": false, "branch": branch, "error": to_checkout_error(e), "requestId": request_id})
        }
    }
}

pub fn build_rename_branch_payload(
    cwd: &str,
    request_id: &str,
    result: &Result<RenameResult>,
) -> Value {
    match result {
        Ok(r) => {
            json!({"cwd": cwd, "success": true, "currentBranch": r.current_branch, "error": null, "requestId": request_id})
        }
        Err(e) => {
            json!({"cwd": cwd, "success": false, "currentBranch": null, "error": to_checkout_error(e), "requestId": request_id})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_suggestion_names() {
        assert_eq!(
            normalize_branch_suggestion_name("refs/heads/feat/x").as_deref(),
            Some("feat/x")
        );
        assert_eq!(
            normalize_branch_suggestion_name("refs/remotes/origin/main").as_deref(),
            Some("main")
        );
        assert_eq!(normalize_branch_suggestion_name("origin/HEAD"), None);
        assert_eq!(normalize_branch_suggestion_name("origin"), None);
        assert_eq!(normalize_branch_suggestion_name("  "), None);
    }

    #[test]
    fn safe_ref_rules() {
        assert!(assert_safe_git_ref("feature/a-b_c.1", "branch").is_ok());
        for bad in ["", "a b", "a..b", "a@{1}", "a;rm", "-x y"] {
            assert!(assert_safe_git_ref(bad, "branch").is_err(), "{bad}");
        }
        assert_eq!(
            assert_safe_git_ref("a..b", "commit")
                .unwrap_err()
                .to_string(),
            "Invalid commit: a..b"
        );
    }

    #[test]
    fn slug_validation_messages() {
        assert!(validate_branch_slug("feat/ok-1").is_ok());
        assert_eq!(
            validate_branch_slug("").unwrap_err(),
            "Branch name cannot be empty"
        );
        assert!(validate_branch_slug("Bad").is_err());
        assert_eq!(
            validate_branch_slug("-a").unwrap_err(),
            "Branch name cannot start or end with a hyphen"
        );
        assert_eq!(
            validate_branch_slug("a--b").unwrap_err(),
            "Branch name cannot have consecutive hyphens"
        );
    }

    #[test]
    fn locale_compare_orders_like_icu_for_ascii() {
        assert_eq!(locale_compare("a", "B"), Ordering::Less);
        assert_eq!(locale_compare("a", "A"), Ordering::Less);
        assert_eq!(locale_compare("feature-a", "feature/a"), Ordering::Less);
        assert_eq!(locale_compare("1", "a"), Ordering::Less);
        assert_eq!(locale_compare("x", "x"), Ordering::Equal);
    }
}
