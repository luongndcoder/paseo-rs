//! Repository facts and ref resolution: worktree roots, base refs, comparison
//! refs, ahead/behind counts (the first half of `checkout-git.ts`).

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;

use crate::args;
use crate::error::{GitError, Result};
use crate::paths::{
    branch_name_from_ref, is_descendant_path, is_qualified_branch_ref, parse_git_rev_parse_path,
    parse_worktree_list, resolve_git_rev_parse_path,
};
use crate::run::{run_git, GitOptions};
use crate::worktree_meta::{
    is_paseo_owned_worktree_cwd, is_paseo_worktree_path, read_paseo_worktree_metadata,
    stored_base_ref_from_metadata, WorktreeRootOptions,
};

pub(crate) const LONG_TIMEOUT_MS: u64 = 120_000;

#[derive(Debug, Clone, Default)]
pub struct CheckoutContext {
    pub paseo_home: Option<String>,
    pub worktrees_root: Option<String>,
    /// Pre-computed facts (from a workspace snapshot). When set, facts are not re-read.
    pub facts: Option<CheckoutSnapshotFacts>,
}

impl CheckoutContext {
    pub fn root_options(&self) -> WorktreeRootOptions {
        WorktreeRootOptions {
            paseo_home: self.paseo_home.clone(),
            worktrees_root: self.worktrees_root.clone(),
        }
    }

    fn git_facts(&self) -> Option<&GitFacts> {
        match &self.facts {
            Some(CheckoutSnapshotFacts::Git(f)) => Some(f),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AheadBehind {
    pub ahead: u64,
    pub behind: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamStatus {
    pub reference: String,
    pub ahead_behind: AheadBehind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaseoWorktreeForCwd {
    NotOwned,
    Owned { worktree_root: String },
}

impl PaseoWorktreeForCwd {
    pub fn is_owned(&self) -> bool {
        matches!(self, PaseoWorktreeForCwd::Owned { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitFacts {
    pub worktree_root: String,
    pub current_branch: Option<String>,
    pub remote_url: Option<String>,
    pub absolute_git_dir: Option<String>,
    pub git_common_dir: Option<String>,
    pub paseo_worktree: PaseoWorktreeForCwd,
    pub stored_base_ref: Option<String>,
    pub resolved_base_ref: Option<String>,
    pub main_repo_root: Option<String>,
    pub comparison_base_ref: Option<String>,
    pub branch_remote_name: Option<String>,
    pub branch_merge_ref: Option<String>,
    pub upstream_status: Option<UpstreamStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum CheckoutSnapshotFacts {
    NotGit,
    Git(GitFacts),
}

pub(crate) fn ro(cwd: &str) -> GitOptions {
    GitOptions::new(cwd).read_only()
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

pub fn is_not_git_repository_error(error: &GitError) -> bool {
    error
        .to_string()
        .to_lowercase()
        .contains("not a git repository")
}

pub async fn require_git_repo(cwd: &str) -> Result<()> {
    run_git(&["rev-parse", "--git-dir"], &ro(cwd))
        .await
        .map(|_| ())
        .map_err(|_| GitError::NotGitRepo {
            cwd: cwd.to_string(),
        })
}

pub async fn require_git_worktree_root(cwd: &str) -> Result<String> {
    let out = run_git(&["rev-parse", "--show-toplevel"], &ro(cwd))
        .await
        .map_err(|_| GitError::NotGitRepo {
            cwd: cwd.to_string(),
        })?;
    parse_git_rev_parse_path(&out.stdout).ok_or_else(|| GitError::NotGitRepo {
        cwd: cwd.to_string(),
    })
}

pub async fn get_current_branch(cwd: &str) -> Option<String> {
    let out = run_git(&["rev-parse", "--abbrev-ref", "HEAD"], &ro(cwd))
        .await
        .ok()?;
    let branch = out.stdout.trim();
    if branch == "HEAD" {
        return get_rebase_head_branch(cwd).await;
    }
    (!branch.is_empty()).then(|| branch.to_string())
}

async fn get_rebase_head_branch(cwd: &str) -> Option<String> {
    for path in ["rebase-merge/head-name", "rebase-apply/head-name"] {
        let Ok(out) = run_git(&["rev-parse", "--git-path", path], &ro(cwd)).await else {
            continue;
        };
        let resolved = crate::paths::node_resolve(cwd, out.stdout.trim());
        let Ok(text) = tokio::fs::read_to_string(resolved).await else {
            continue;
        };
        let head_name = text.trim();
        let name = head_name.strip_prefix("refs/heads/").unwrap_or(head_name);
        if !name.is_empty() {
            return Some(name.to_string());
        }
    }
    None
}

pub async fn get_worktree_root(cwd: &str) -> Option<String> {
    let out = run_git(&["rev-parse", "--show-toplevel"], &ro(cwd))
        .await
        .ok()?;
    parse_git_rev_parse_path(&out.stdout)
}

pub async fn get_main_repo_root(cwd: &str, ctx: &CheckoutContext) -> Result<String> {
    let out = run_git(&["rev-parse", "--git-common-dir"], &ro(cwd)).await?;
    get_main_repo_root_from_common_dir(cwd, resolve_git_rev_parse_path(cwd, &out.stdout), ctx).await
}

pub(crate) async fn get_main_repo_root_from_common_dir(
    cwd: &str,
    common_dir: Option<String>,
    ctx: &CheckoutContext,
) -> Result<String> {
    let common_dir = common_dir.ok_or_else(|| GitError::other("Not in a git repository"))?;
    let normalized = std::fs::canonicalize(&common_dir)?
        .to_string_lossy()
        .into_owned();
    let p = Path::new(&normalized);
    if p.file_name().is_some_and(|n| n == ".git") {
        return Ok(p
            .parent()
            .map(|x| x.to_string_lossy().into_owned())
            .unwrap_or(normalized));
    }
    let out = run_git(&["worktree", "list", "--porcelain"], &ro(cwd)).await?;
    let opts = ctx.root_options();
    let worktrees = parse_worktree_list(&out.stdout);
    let non_bare_non_paseo: Vec<_> = worktrees
        .into_iter()
        .filter(|wt| wt.is_bare != Some(true) && !is_paseo_worktree_path(&wt.path, Some(&opts)))
        .collect();
    let children: Vec<_> = non_bare_non_paseo
        .iter()
        .filter(|wt| is_descendant_path(&wt.path, &normalized))
        .collect();
    let main_child = children
        .iter()
        .find(|wt| Path::new(&wt.path).file_name().is_some_and(|n| n == "main"));
    Ok(main_child
        .map(|w| w.path.clone())
        .or_else(|| children.first().map(|w| w.path.clone()))
        .or_else(|| non_bare_non_paseo.first().map(|w| w.path.clone()))
        .unwrap_or(normalized))
}

pub async fn get_git_config_value(cwd: &str, key: &str) -> Option<String> {
    let out = run_git(&["config", "--get", key], &ro(cwd)).await.ok()?;
    non_empty(&out.stdout)
}

pub async fn get_origin_remote_url(cwd: &str) -> Option<String> {
    get_git_config_value(cwd, "remote.origin.url").await
}

pub async fn has_origin_remote(cwd: &str) -> bool {
    get_origin_remote_url(cwd).await.is_some()
}

pub async fn resolve_absolute_git_dir(cwd: &str) -> Option<String> {
    let out = run_git(&["rev-parse", "--absolute-git-dir"], &ro(cwd))
        .await
        .ok()?;
    non_empty(&out.stdout)
}

pub async fn resolve_git_common_dir(cwd: &str) -> Option<String> {
    let out = run_git(&["rev-parse", "--git-common-dir"], &ro(cwd))
        .await
        .ok()?;
    resolve_git_rev_parse_path(cwd, &out.stdout)
}

pub async fn is_working_tree_dirty(cwd: &str) -> Result<bool> {
    let out = run_git(&["status", "--porcelain"], &ro(cwd)).await?;
    Ok(!out.stdout.trim().is_empty())
}

pub async fn local_branch_exists(cwd: &str, branch_name: &str) -> Result<bool> {
    does_git_ref_exist(cwd, &format!("refs/heads/{branch_name}")).await
}

pub async fn does_git_ref_exist(cwd: &str, full_ref: &str) -> Result<bool> {
    let out = run_git(
        &["show-ref", "--verify", "--quiet", full_ref],
        &ro(cwd).accept(&[0, 1]),
    )
    .await?;
    Ok(out.exit_code == Some(0))
}

/// Default branch: `origin/HEAD` (preferring the local branch), else `main`, else `master`.
pub async fn resolve_repository_default_branch(repo_root: &str) -> Result<Option<String>> {
    if let Ok(out) = run_git(
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
        &ro(repo_root),
    )
    .await
    {
        let reference = out.stdout.trim().to_string();
        if !reference.is_empty() {
            let remote_short = reference
                .strip_prefix("refs/remotes/")
                .unwrap_or(&reference)
                .to_string();
            let local_name = remote_short
                .strip_prefix("origin/")
                .unwrap_or(&remote_short)
                .to_string();
            let attempt: Result<Option<String>> = async {
                if run_git(
                    &[
                        "show-ref",
                        "--verify",
                        "--quiet",
                        &format!("refs/heads/{local_name}"),
                    ],
                    &ro(repo_root),
                )
                .await
                .is_ok()
                {
                    return Ok(Some(local_name.clone()));
                }
                run_git(
                    &["show-ref", "--verify", "--quiet", &reference],
                    &ro(repo_root),
                )
                .await?;
                Ok(Some(remote_short.clone()))
            }
            .await;
            // A stale origin/HEAD falls through to the local branches.
            if let Ok(Some(found)) = attempt {
                return Ok(Some(found));
            }
        }
    }
    let out = run_git(&["branch", "--format=%(refname:short)"], &ro(repo_root)).await?;
    let branches: std::collections::HashSet<_> = out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if branches.contains("main") {
        return Ok(Some("main".into()));
    }
    if branches.contains("master") {
        return Ok(Some("master".into()));
    }
    Ok(None)
}

pub(crate) async fn resolve_base_ref(repo_root: &str) -> Result<Option<String>> {
    resolve_repository_default_branch(repo_root).await
}

pub(crate) fn resolve_merge_target_branch(base_ref: &str) -> Result<String> {
    let remote_prefix = "refs/remotes/";
    let origin_prefix = "refs/remotes/origin/";
    if base_ref.starts_with(remote_prefix) && !base_ref.starts_with(origin_prefix) {
        return Err(GitError::other(format!(
            "No local merge target is recorded for base ref {base_ref}"
        )));
    }
    Ok(branch_name_from_ref(base_ref))
}

pub(crate) async fn resolve_best_comparison_base_ref(cwd: &str, base_ref: &str) -> Result<String> {
    // A fully qualified branch ref names the exact commit stream to compare against.
    if is_qualified_branch_ref(base_ref) {
        if does_git_ref_exist(cwd, base_ref).await? {
            return Ok(base_ref.to_string());
        }
        return Err(GitError::other(format!("Base ref not found: {base_ref}")));
    }
    let local_name = branch_name_from_ref(base_ref);
    let (local_ref, origin_ref) = (
        format!("refs/heads/{local_name}"),
        format!("refs/remotes/origin/{local_name}"),
    );
    let (has_local, has_origin) = tokio::join!(
        does_git_ref_exist(cwd, &local_ref),
        does_git_ref_exist(cwd, &origin_ref)
    );
    let (has_local, has_origin) = (has_local?, has_origin?);
    if has_origin {
        return Ok(format!("origin/{local_name}"));
    }
    if has_local {
        return Ok(local_name);
    }
    let ref_name =
        if base_ref.starts_with("origin/") || base_ref.starts_with("refs/remotes/origin/") {
            format!("origin/{local_name}")
        } else {
            local_name
        };
    Err(GitError::other(format!(
        "Base branch not found locally or on origin: {ref_name}"
    )))
}

pub(crate) async fn resolve_most_ahead_base_ref(cwd: &str, base_ref: &str) -> Result<String> {
    if is_qualified_branch_ref(base_ref) {
        if does_git_ref_exist(cwd, base_ref).await? {
            return Ok(base_ref.to_string());
        }
        return Err(GitError::other(format!("Base ref not found: {base_ref}")));
    }
    let name = branch_name_from_ref(base_ref);
    let (local_ref, origin_ref) = (
        format!("refs/heads/{name}"),
        format!("refs/remotes/origin/{name}"),
    );
    let (has_local, has_origin) = tokio::join!(
        does_git_ref_exist(cwd, &local_ref),
        does_git_ref_exist(cwd, &origin_ref)
    );
    let (has_local, has_origin) = (has_local?, has_origin?);
    if has_local && !has_origin {
        return Ok(name);
    }
    if !has_local && has_origin {
        return Ok(format!("origin/{name}"));
    }
    if !has_local && !has_origin {
        return Err(GitError::other(format!(
            "Base branch not found locally or on origin: {name}"
        )));
    }
    let out = run_git(
        &args![
            "rev-list",
            "--left-right",
            "--count",
            format!("{name}...origin/{name}")
        ],
        &ro(cwd),
    )
    .await?;
    let mut parts = out.stdout.split_whitespace();
    let local_only = parts.next().and_then(|v| v.parse::<i64>().ok());
    let origin_only = parts.next().and_then(|v| v.parse::<i64>().ok());
    let (Some(local_only), Some(origin_only)) = (local_only, origin_only) else {
        return Ok(name);
    };
    if origin_only > local_only {
        return Ok(format!("origin/{name}"));
    }
    Ok(name)
}

pub(crate) async fn get_ahead_behind_for_comparison_ref(
    cwd: &str,
    comparison_ref: &str,
    current_branch: &str,
) -> Result<Option<AheadBehind>> {
    let out = run_git(
        &args![
            "rev-list",
            "--left-right",
            "--count",
            format!("{comparison_ref}...{current_branch}")
        ],
        &ro(cwd),
    )
    .await?;
    let mut parts = out.stdout.split_whitespace();
    let behind = parts.next().unwrap_or("0").parse::<u64>();
    let ahead = parts.next().unwrap_or("0").parse::<u64>();
    match (ahead, behind) {
        (Ok(ahead), Ok(behind)) => Ok(Some(AheadBehind { ahead, behind })),
        _ => Ok(None),
    }
}

pub async fn get_ahead_behind(
    cwd: &str,
    base_ref: &str,
    current_branch: &str,
    ctx: &CheckoutContext,
) -> Result<Option<AheadBehind>> {
    let normalized = branch_name_from_ref(base_ref);
    if normalized.is_empty() || current_branch.is_empty() || normalized == current_branch {
        return Ok(None);
    }
    let comparison = match ctx.git_facts() {
        Some(f) if f.resolved_base_ref.as_deref() == Some(base_ref) => {
            f.comparison_base_ref.clone()
        }
        _ => Some(resolve_best_comparison_base_ref(cwd, base_ref).await?),
    };
    match comparison {
        Some(c) => get_ahead_behind_for_comparison_ref(cwd, &c, current_branch).await,
        None => Ok(None),
    }
}

pub async fn get_upstream_status(cwd: &str, current_branch: &str) -> Option<UpstreamStatus> {
    static AHEAD: OnceLock<Regex> = OnceLock::new();
    static BEHIND: OnceLock<Regex> = OnceLock::new();
    let out = run_git(
        &args![
            "for-each-ref",
            "--format=%(upstream)%00%(upstream:track,nobracket)",
            format!("refs/heads/{current_branch}")
        ],
        &ro(cwd),
    )
    .await
    .ok()?;
    let trimmed = out.stdout.trim();
    let mut parts = trimmed.splitn(2, '\0');
    let reference = parts.next().unwrap_or("");
    let track = parts.next().unwrap_or("");
    if reference.is_empty() || track == "gone" {
        return None;
    }
    let grab = |cell: &'static OnceLock<Regex>, pat: &str| {
        cell.get_or_init(|| Regex::new(pat).unwrap())
            .captures(track)
            .and_then(|c| c[1].parse::<u64>().ok())
            .unwrap_or(0)
    };
    Some(UpstreamStatus {
        reference: reference.to_string(),
        ahead_behind: AheadBehind {
            ahead: grab(&AHEAD, r"ahead (\d+)"),
            behind: grab(&BEHIND, r"behind (\d+)"),
        },
    })
}

pub(crate) async fn get_paseo_worktree_for_cwd(
    cwd: &str,
    ctx: &CheckoutContext,
    known_worktree_root: Option<&str>,
) -> PaseoWorktreeForCwd {
    // Fast-path reject: non-worktree paths skip the ownership check.
    if !cwd.replace('\\', "/").contains("/worktrees/") {
        return PaseoWorktreeForCwd::NotOwned;
    }
    if !is_paseo_owned_worktree_cwd(cwd, &ctx.root_options()).allowed {
        return PaseoWorktreeForCwd::NotOwned;
    }
    let root = match known_worktree_root {
        Some(r) => r.to_string(),
        None => get_worktree_root(cwd)
            .await
            .unwrap_or_else(|| cwd.to_string()),
    };
    PaseoWorktreeForCwd::Owned {
        worktree_root: root,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BaseRefResolution {
    pub stored_base_ref: Option<String>,
    pub resolved_base_ref: Option<String>,
}

pub(crate) async fn resolve_base_ref_for_cwd(
    cwd: &str,
    ctx: &CheckoutContext,
) -> Result<BaseRefResolution> {
    if let Some(f) = ctx.git_facts() {
        return Ok(BaseRefResolution {
            stored_base_ref: f.stored_base_ref.clone(),
            resolved_base_ref: f.resolved_base_ref.clone(),
        });
    }
    let stored = match get_paseo_worktree_for_cwd(cwd, ctx, None).await {
        PaseoWorktreeForCwd::Owned { worktree_root } => {
            stored_base_ref_from_metadata(read_paseo_worktree_metadata(&worktree_root)?.as_ref())
        }
        PaseoWorktreeForCwd::NotOwned => None,
    };
    let resolved = match &stored {
        Some(s) => Some(s.clone()),
        None => resolve_base_ref(cwd).await?,
    };
    Ok(BaseRefResolution {
        stored_base_ref: stored,
        resolved_base_ref: resolved,
    })
}

/// The worktree stores the exact ref it was cut from while callers send its
/// display name; a different qualified ref keeps its own identity.
fn is_same_base_ref(stored: &str, requested: &str) -> bool {
    stored == requested || branch_name_from_ref(stored) == requested
}

pub(crate) fn resolve_operation_base_ref(
    stored: Option<&str>,
    resolved: Option<&str>,
    requested: Option<&str>,
) -> Result<Option<String>> {
    if let (Some(stored), Some(requested)) = (stored, requested) {
        if !is_same_base_ref(stored, requested) {
            return Err(GitError::other(format!(
                "Base ref mismatch: stored {stored}, requested {requested}"
            )));
        }
    }
    Ok(stored.or(requested).or(resolved).map(str::to_string))
}

pub(crate) async fn try_resolve_merge_base(cwd: &str, base_ref: &str) -> Option<String> {
    let out = run_git(&["merge-base", base_ref, "HEAD"], &ro(cwd))
        .await
        .ok()?;
    non_empty(&out.stdout)
}

/// Collect the facts a status snapshot is built from.
pub async fn get_checkout_snapshot_facts(
    cwd: &str,
    ctx: &CheckoutContext,
) -> Result<CheckoutSnapshotFacts> {
    if let Some(f) = &ctx.facts {
        return Ok(f.clone());
    }
    let Some(root) = get_worktree_root(cwd).await else {
        return Ok(CheckoutSnapshotFacts::NotGit);
    };
    let (current_branch, remote_url, absolute_git_dir, git_common_dir) = tokio::join!(
        get_current_branch(cwd),
        get_origin_remote_url(cwd),
        resolve_absolute_git_dir(cwd),
        resolve_git_common_dir(cwd),
    );
    let paseo_worktree = get_paseo_worktree_for_cwd(cwd, ctx, Some(&root)).await;
    let metadata = match &paseo_worktree {
        PaseoWorktreeForCwd::Owned { worktree_root } => {
            read_paseo_worktree_metadata(worktree_root)?
        }
        PaseoWorktreeForCwd::NotOwned => None,
    };
    let stored_base_ref = stored_base_ref_from_metadata(metadata.as_ref());
    let resolved_base_ref = match &stored_base_ref {
        Some(s) => Some(s.clone()),
        None => resolve_base_ref(cwd).await?,
    };
    let main_repo_root = get_main_repo_root_from_common_dir(cwd, git_common_dir.clone(), ctx)
        .await
        .ok();
    let mut comparison_base_ref = None;
    if let (Some(base), Some(branch)) = (&resolved_base_ref, &current_branch) {
        if &branch_name_from_ref(base) != branch {
            comparison_base_ref = resolve_best_comparison_base_ref(cwd, base).await.ok();
        }
    }
    let (mut branch_remote_name, mut branch_merge_ref) = (None, None);
    let mut upstream_status = None;
    if let Some(branch) = &current_branch {
        upstream_status = get_upstream_status(cwd, branch).await;
        branch_remote_name = get_git_config_value(cwd, &format!("branch.{branch}.remote")).await;
        if branch_remote_name.is_some() {
            branch_merge_ref = get_git_config_value(cwd, &format!("branch.{branch}.merge")).await;
        }
    }
    Ok(CheckoutSnapshotFacts::Git(GitFacts {
        worktree_root: root,
        current_branch,
        remote_url,
        absolute_git_dir,
        git_common_dir,
        paseo_worktree,
        stored_base_ref,
        resolved_base_ref,
        main_repo_root,
        comparison_base_ref,
        branch_remote_name,
        branch_merge_ref,
        upstream_status,
    }))
}
