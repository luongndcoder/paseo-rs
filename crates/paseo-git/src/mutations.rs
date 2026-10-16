//! Git writes: commit, stage/unstage/discard, merge to/from base, pull, push.

use std::path::Path;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{to_checkout_error, GitError, Result};
use crate::paths::node_resolve;
use crate::repo::{
    get_current_branch, get_git_config_value, get_worktree_root, has_origin_remote,
    require_git_repo, require_git_worktree_root, resolve_absolute_git_dir,
    resolve_base_ref_for_cwd, resolve_merge_target_branch, resolve_most_ahead_base_ref,
    resolve_operation_base_ref, ro, CheckoutContext, LONG_TIMEOUT_MS,
};
use crate::run::{run_git, GitOptions};
use crate::{args, argv};

const DISCARD_CHANGES_TIMEOUT_MS: u64 = 120_000;

fn long(cwd: &str) -> GitOptions {
    GitOptions::new(cwd).timeout_ms(LONG_TIMEOUT_MS)
}

#[derive(Debug, Clone, Default)]
pub struct CommitOptions {
    pub message: String,
    pub add_all: Option<bool>,
    pub paths: Option<Vec<String>>,
    pub amend: bool,
}

pub async fn commit_changes(cwd: &str, options: &CommitOptions) -> Result<()> {
    require_git_repo(cwd).await?;
    match &options.paths {
        Some(paths) if !paths.is_empty() => {
            let mut a = argv!["--literal-pathspecs", "add", "-A", "--"];
            a.extend(paths.iter().cloned());
            run_git(&a, &long(cwd)).await?;
        }
        _ if options.add_all.unwrap_or(true) => {
            run_git(&["add", "-A"], &long(cwd)).await?;
        }
        _ => {}
    }
    let commit_args = if options.amend {
        argv!["commit", "--amend", "-m", options.message.as_str()]
    } else {
        argv!["commit", "-m", options.message.as_str()]
    };
    run_git(&commit_args, &long(cwd)).await?;
    Ok(())
}

pub async fn commit_all(cwd: &str, message: &str) -> Result<()> {
    commit_changes(
        cwd,
        &CommitOptions {
            message: message.into(),
            add_all: Some(true),
            ..Default::default()
        },
    )
    .await
}

pub async fn stage_changes(cwd: &str, pathspecs: &[String]) -> Result<()> {
    require_git_repo(cwd).await?;
    if pathspecs.is_empty() {
        return Ok(());
    }
    let mut a = argv!["--literal-pathspecs", "add", "-A", "--"];
    a.extend(pathspecs.iter().cloned());
    run_git(
        &a,
        &GitOptions::new(cwd).timeout_ms(DISCARD_CHANGES_TIMEOUT_MS),
    )
    .await?;
    Ok(())
}

async fn unstage_paths(cwd: &str, pathspecs: &[String]) -> Result<()> {
    let opts = GitOptions::new(cwd).timeout_ms(DISCARD_CHANGES_TIMEOUT_MS);
    let mut a = argv!["--literal-pathspecs", "reset", "-q", "HEAD", "--"];
    a.extend(pathspecs.iter().cloned());
    if run_git(&a, &opts).await.is_ok() {
        return Ok(());
    }
    // Unborn HEAD has no commit for reset: drop the paths from the index directly.
    let mut a = argv![
        "--literal-pathspecs",
        "rm",
        "--cached",
        "-r",
        "-q",
        "--ignore-unmatch",
        "--"
    ];
    a.extend(pathspecs.iter().cloned());
    run_git(&a, &opts).await?;
    Ok(())
}

pub async fn unstage_changes(cwd: &str, pathspecs: &[String]) -> Result<()> {
    require_git_repo(cwd).await?;
    if pathspecs.is_empty() {
        return Ok(());
    }
    unstage_paths(cwd, pathspecs).await
}

pub async fn discard_changes(cwd: &str, pathspecs: &[String]) -> Result<()> {
    let root = require_git_worktree_root(cwd).await?;
    if pathspecs.is_empty() {
        return Ok(());
    }
    unstage_paths(&root, pathspecs).await?;
    // Everything is unstaged now: classify from porcelain so each path gets the
    // command that applies to it (restore from the index vs clean untracked).
    let opts = GitOptions::new(&root).timeout_ms(DISCARD_CHANGES_TIMEOUT_MS);
    let mut a = argv![
        "--literal-pathspecs",
        "status",
        "--porcelain=v1",
        "-z",
        "--"
    ];
    a.extend(pathspecs.iter().cloned());
    let status = run_git(&a, &opts).await?;
    let (mut tracked, mut untracked) = (Vec::<String>::new(), Vec::<String>::new());
    let tokens: Vec<&str> = status.stdout.split('\0').collect();
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        i += 1;
        if token.len() < 4 {
            continue;
        }
        let state = &token[..2];
        let Some(file_path) = token.get(3..) else {
            continue;
        };
        if state.starts_with('R') || state.starts_with('C') {
            // Rename/copy entries carry the source path as the next NUL token.
            i += 1;
        }
        if state == "??" {
            untracked.push(file_path.to_string());
        } else {
            tracked.push(file_path.to_string());
        }
    }
    if !tracked.is_empty() {
        let mut a = argv!["--literal-pathspecs", "checkout", "-q", "--"];
        a.extend(tracked);
        run_git(&a, &opts).await?;
    }
    if !untracked.is_empty() {
        let mut a = argv!["--literal-pathspecs", "clean", "-fd", "-q", "--"];
        a.extend(untracked);
        run_git(&a, &opts).await?;
    }
    Ok(())
}

/// `checkout.discard_changes.request` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiscardAction {
    Discard,
    Stage,
    Unstage,
}

pub async fn apply_discard_action(
    cwd: &str,
    paths: &[String],
    action: Option<DiscardAction>,
) -> Result<()> {
    match action {
        Some(DiscardAction::Stage) => stage_changes(cwd, paths).await,
        Some(DiscardAction::Unstage) => unstage_changes(cwd, paths).await,
        _ => discard_changes(cwd, paths).await,
    }
}

/// Wire payload shared by commit/merge/pull/push/discard/refresh responses.
pub fn build_success_payload(cwd: &str, request_id: &str, result: &Result<()>) -> Value {
    match result {
        Ok(()) => json!({"cwd": cwd, "success": true, "error": null, "requestId": request_id}),
        Err(e) => {
            json!({"cwd": cwd, "success": false, "error": to_checkout_error(e), "requestId": request_id})
        }
    }
}

// ---- merges ------------------------------------------------------------------

/// After a failed merge, look for unmerged paths. When conflicts exist the merge
/// is aborted and the conflicted files are returned. Detection failures are
/// ignored so the original error surfaces.
async fn detect_merge_conflict(cwd: &str, error: &GitError) -> Option<Vec<String>> {
    let details = format!("{}\n{}\n{}", error, error.stderr(), error.stdout());
    let o = GitOptions::new(cwd);
    let (unmerged, ls_files, status) = tokio::join!(
        run_git(&["diff", "--name-only", "--diff-filter=U"], &o),
        run_git(&["ls-files", "-u"], &o),
        run_git(&["status", "--porcelain"], &o),
    );
    let (unmerged, ls_files, status) = (unmerged.ok()?, ls_files.ok()?, status.ok()?);
    let lines = |s: &str| -> Vec<String> {
        s.split('\n')
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    };
    let is_conflict_status = |l: &str| {
        l.len() >= 3
            && matches!(&l[..2], "UU" | "AA" | "DD" | "AU" | "UA" | "UD" | "DU")
            && l.as_bytes()[2].is_ascii_whitespace()
    };
    let status_conflicts: Vec<String> = lines(&status.stdout)
        .into_iter()
        .filter(|l| is_conflict_status(l))
        .map(|l| l[3..].trim().to_string())
        .collect();
    let mut conflicts: Vec<String> = lines(&unmerged.stdout);
    conflicts.extend(
        lines(&ls_files.stdout)
            .into_iter()
            .map(|l| l.rsplit('\t').next().unwrap_or("").to_string()),
    );
    conflicts.extend(status_conflicts);
    conflicts.retain(|c| !c.is_empty());
    let detected = !conflicts.is_empty() || {
        let d = details.to_lowercase();
        d.contains("conflict") || d.contains("automatic merge failed")
    };
    if !detected {
        return None;
    }
    let _ = run_git(&["merge", "--abort"], &long(cwd)).await;
    Some(conflicts)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeMode {
    Merge,
    Squash,
}

#[derive(Debug, Clone, Default)]
pub struct MergeToBaseOptions {
    pub base_ref: Option<String>,
    pub mode: Option<MergeMode>,
    pub commit_message: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MergeFromBaseOptions {
    pub base_ref: Option<String>,
    pub require_clean_target: Option<bool>,
}

async fn get_worktree_path_for_branch(cwd: &str, branch_name: &str) -> Option<String> {
    let reference = if branch_name.starts_with("refs/heads/") {
        branch_name.to_string()
    } else {
        format!("refs/heads/{branch_name}")
    };
    let out = run_git(
        &args![
            "for-each-ref",
            "--format=%(refname)%00%(worktreepath)%00",
            reference.as_str()
        ],
        &ro(cwd),
    )
    .await
    .ok()?;
    for record in out.stdout.split("\0\n") {
        let mut parts = record.split('\0');
        let (name, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        if name == reference {
            return (!path.is_empty()).then(|| path.to_string());
        }
    }
    None
}

/// Merge the current branch into its base. Returns the checkout the merge ran in.
pub async fn merge_to_base(
    cwd: &str,
    options: &MergeToBaseOptions,
    ctx: &CheckoutContext,
) -> Result<String> {
    require_git_repo(cwd).await?;
    let current_branch = get_current_branch(cwd).await;
    let r = resolve_base_ref_for_cwd(cwd, ctx).await?;
    let base_ref = resolve_operation_base_ref(
        r.stored_base_ref.as_deref(),
        r.resolved_base_ref.as_deref(),
        options.base_ref.as_deref(),
    )?
    .ok_or_else(|| GitError::other("Unable to determine base branch for merge"))?;
    let current_branch = current_branch
        .ok_or_else(|| GitError::other("Unable to determine current branch for merge"))?;
    let normalized_base = resolve_merge_target_branch(&base_ref)?;
    let current_root = get_worktree_root(cwd)
        .await
        .unwrap_or_else(|| cwd.to_string());
    if normalized_base == current_branch {
        return Ok(current_root);
    }
    let base_worktree = get_worktree_path_for_branch(cwd, &normalized_base).await;
    let operation_cwd = base_worktree.unwrap_or_else(|| current_root.clone());
    let is_same_checkout = node_resolve("/", &operation_cwd) == node_resolve("/", &current_root);
    let original_branch = get_current_branch(&operation_cwd).await;
    let mode = options.mode.unwrap_or(MergeMode::Merge);

    let attempt: Result<()> = async {
        run_git(&["checkout", &normalized_base], &long(&operation_cwd)).await?;
        if mode == MergeMode::Squash {
            run_git(
                &["merge", "--squash", &current_branch],
                &long(&operation_cwd),
            )
            .await?;
            let message = options
                .commit_message
                .clone()
                .unwrap_or_else(|| format!("Squash merge {current_branch} into {normalized_base}"));
            run_git(&["commit", "-m", &message], &long(&operation_cwd)).await?;
        } else {
            run_git(&["merge", &current_branch], &long(&operation_cwd)).await?;
        }
        Ok(())
    }
    .await;

    let outcome = match attempt {
        Ok(()) => Ok(()),
        Err(error) => Err(match detect_merge_conflict(&operation_cwd, &error).await {
            Some(conflict_files) => GitError::MergeConflict {
                base_ref: normalized_base.clone(),
                current_branch: current_branch.clone(),
                conflict_files,
            },
            None => error,
        }),
    };
    if is_same_checkout {
        if let Some(original) = original_branch.filter(|o| *o != normalized_base) {
            let _ = run_git(&["checkout", &original], &long(&operation_cwd)).await;
        }
    }
    outcome?;
    Ok(operation_cwd)
}

pub async fn merge_from_base(
    cwd: &str,
    options: &MergeFromBaseOptions,
    ctx: &CheckoutContext,
) -> Result<()> {
    require_git_repo(cwd).await?;
    let current_branch = get_current_branch(cwd)
        .await
        .filter(|b| b != "HEAD")
        .ok_or_else(|| GitError::other("Unable to determine current branch for merge"))?;
    let r = resolve_base_ref_for_cwd(cwd, ctx).await?;
    let base_ref = resolve_operation_base_ref(
        r.stored_base_ref.as_deref(),
        r.resolved_base_ref.as_deref(),
        options.base_ref.as_deref(),
    )?
    .ok_or_else(|| GitError::other("Unable to determine base branch for merge"))?;
    if options.require_clean_target.unwrap_or(true) {
        let out = run_git(&["status", "--porcelain"], &ro(cwd)).await?;
        if !out.stdout.trim().is_empty() {
            return Err(GitError::other(
                "Working directory has uncommitted changes.",
            ));
        }
    }
    let best = resolve_most_ahead_base_ref(cwd, &base_ref).await?;
    if best == current_branch {
        return Ok(());
    }
    if let Err(error) = run_git(&["merge", &best], &long(cwd)).await {
        return Err(match detect_merge_conflict(cwd, &error).await {
            Some(conflict_files) => GitError::MergeFromBaseConflict {
                base_ref: best,
                current_branch,
                conflict_files,
            },
            None => error,
        });
    }
    Ok(())
}

// ---- pull / push -------------------------------------------------------------

async fn abort_git_pull_conflict_state(cwd: &str) {
    let Some(git_dir) = resolve_absolute_git_dir(cwd).await else {
        return;
    };
    let dir = Path::new(&git_dir);
    if dir.join("MERGE_HEAD").exists() {
        let _ = run_git(&["merge", "--abort"], &long(cwd)).await;
    }
    if dir.join("rebase-merge").exists() || dir.join("rebase-apply").exists() {
        let _ = run_git(&["rebase", "--abort"], &long(cwd)).await;
    }
}

pub async fn pull_current_branch(cwd: &str) -> Result<()> {
    require_git_repo(cwd).await?;
    if get_current_branch(cwd).await.is_none_or(|b| b == "HEAD") {
        return Err(GitError::other(
            "Unable to determine current branch for pull",
        ));
    }
    if !has_origin_remote(cwd).await {
        return Err(GitError::other("Remote 'origin' is not configured."));
    }
    if let Err(error) = run_git(&["pull"], &long(cwd)).await {
        abort_git_pull_conflict_state(cwd).await;
        return Err(error);
    }
    Ok(())
}

fn parse_branch_merge_head_ref(merge_ref: Option<&str>) -> Option<String> {
    let head = merge_ref?.strip_prefix("refs/heads/")?.trim();
    (!head.is_empty()).then(|| head.to_string())
}

fn parse_head_push_refspec(refspec: Option<&str>) -> Option<String> {
    let normalized = refspec?.trim();
    let normalized = normalized.strip_prefix('+').unwrap_or(normalized);
    let head = normalized.strip_prefix("HEAD:refs/heads/")?.trim();
    (!head.is_empty()).then(|| head.to_string())
}

struct PushTarget {
    remote_name: String,
    head_ref: String,
}

async fn configured_push_target(cwd: &str, branch: &str) -> Option<PushTarget> {
    let remote_name = get_git_config_value(cwd, &format!("branch.{branch}.pushRemote")).await?;
    let refspec = get_git_config_value(cwd, &format!("remote.{remote_name}.push")).await;
    let head_ref = parse_head_push_refspec(refspec.as_deref())?;
    get_git_config_value(cwd, &format!("remote.{remote_name}.url")).await?;
    Some(PushTarget {
        remote_name,
        head_ref,
    })
}

async fn upstream_push_target(cwd: &str, branch: &str) -> Option<PushTarget> {
    let remote_name = get_git_config_value(cwd, &format!("branch.{branch}.remote")).await?;
    let merge_ref = get_git_config_value(cwd, &format!("branch.{branch}.merge")).await;
    let head_ref = parse_branch_merge_head_ref(merge_ref.as_deref())?;
    get_git_config_value(cwd, &format!("remote.{remote_name}.url")).await?;
    Some(PushTarget {
        remote_name,
        head_ref,
    })
}

async fn remote_push_url(cwd: &str, remote: &str) -> Option<String> {
    let out = run_git(&["remote", "get-url", "--push", remote], &ro(cwd))
        .await
        .ok()?;
    let v = out.stdout.trim();
    (!v.is_empty()).then(|| v.to_string())
}

async fn update_remote_tracking_ref(cwd: &str, remote: &str, head_ref: &str) -> Result<bool> {
    let tracking = format!("refs/remotes/{remote}/{head_ref}");
    let check = run_git(
        &["check-ref-format", &tracking],
        &GitOptions::new(cwd).accept(&[0, 1]),
    )
    .await?;
    if check.exit_code != Some(0) {
        return Ok(false);
    }
    run_git(&["update-ref", &tracking, "HEAD"], &long(cwd)).await?;
    Ok(true)
}

async fn refresh_tracked_ref_after_push(
    cwd: &str,
    branch: &str,
    pushed: &PushTarget,
) -> Result<()> {
    let tracking_remote = get_git_config_value(cwd, &format!("branch.{branch}.remote")).await;
    let tracking_merge = get_git_config_value(cwd, &format!("branch.{branch}.merge")).await;
    let tracking_head = parse_branch_merge_head_ref(tracking_merge.as_deref());
    if tracking_remote.is_none() && tracking_merge.is_none() {
        if !update_remote_tracking_ref(cwd, &pushed.remote_name, &pushed.head_ref).await? {
            return Ok(());
        }
        run_git(
            &[
                "config",
                &format!("branch.{branch}.remote"),
                &pushed.remote_name,
            ],
            &GitOptions::new(cwd),
        )
        .await?;
        run_git(
            &[
                "config",
                &format!("branch.{branch}.merge"),
                &format!("refs/heads/{}", pushed.head_ref),
            ],
            &GitOptions::new(cwd),
        )
        .await?;
        return Ok(());
    }
    let Some(tracking_remote) = tracking_remote else {
        return Ok(());
    };
    if tracking_head.as_deref() != Some(pushed.head_ref.as_str()) {
        return Ok(());
    }
    let (a, b) = tokio::join!(
        remote_push_url(cwd, &tracking_remote),
        remote_push_url(cwd, &pushed.remote_name)
    );
    match a {
        Some(a) if Some(&a) == b.as_ref() => {
            update_remote_tracking_ref(cwd, &tracking_remote, &pushed.head_ref).await?;
        }
        _ => {}
    }
    Ok(())
}

pub async fn push_current_branch(cwd: &str) -> Result<()> {
    require_git_repo(cwd).await?;
    let branch = get_current_branch(cwd)
        .await
        .filter(|b| b != "HEAD")
        .ok_or_else(|| GitError::other("Unable to determine current branch for push"))?;
    if let Some(target) = configured_push_target(cwd, &branch).await {
        run_git(
            &args![
                "push",
                target.remote_name.as_str(),
                format!("HEAD:refs/heads/{}", target.head_ref)
            ],
            &long(cwd),
        )
        .await?;
        refresh_tracked_ref_after_push(cwd, &branch, &target).await?;
        return Ok(());
    }
    if let Some(target) = upstream_push_target(cwd, &branch).await {
        run_git(
            &args![
                "push",
                "-u",
                target.remote_name.as_str(),
                format!("HEAD:refs/heads/{}", target.head_ref)
            ],
            &long(cwd),
        )
        .await?;
        return Ok(());
    }
    if !has_origin_remote(cwd).await {
        return Err(GitError::other("Remote 'origin' is not configured."));
    }
    run_git(&["push", "-u", "origin", &branch], &long(cwd)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_refspec_parsing() {
        assert_eq!(
            parse_head_push_refspec(Some("HEAD:refs/heads/x")).as_deref(),
            Some("x")
        );
        assert_eq!(
            parse_head_push_refspec(Some("+HEAD:refs/heads/x ")).as_deref(),
            Some("x")
        );
        assert_eq!(
            parse_head_push_refspec(Some("refs/heads/*:refs/heads/*")),
            None
        );
        assert_eq!(parse_head_push_refspec(Some("HEAD:refs/heads/")), None);
        assert_eq!(parse_head_push_refspec(None), None);
        assert_eq!(
            parse_branch_merge_head_ref(Some("refs/heads/feat/a")).as_deref(),
            Some("feat/a")
        );
        assert_eq!(parse_branch_merge_head_ref(Some("refs/tags/v")), None);
    }

    #[test]
    fn discard_action_deserializes() {
        let a: DiscardAction = serde_json::from_str("\"unstage\"").unwrap();
        assert_eq!(a, DiscardAction::Unstage);
    }
}
