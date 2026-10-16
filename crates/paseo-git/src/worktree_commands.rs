//! Worktree command helpers (`server/worktree/commands.ts`): resolving which
//! worktree path an archive request targets.

use std::path::Path;

use crate::error::{GitError, Result};
use crate::worktree_meta::{get_paseo_worktrees_root, WorktreeRootOptions};

#[derive(Debug, Clone, Default)]
pub struct ArchiveCommandInput {
    pub request_id: String,
    pub repo_root: Option<String>,
    pub worktree_path: Option<String>,
    pub worktree_slug: Option<String>,
    pub branch_name: Option<String>,
    pub workspace_id: Option<String>,
    pub scope: Option<ArchiveScopeKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveScopeKind {
    Workspace,
    Worktree,
}

/// One entry of the paseo worktree listing the caller already holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeListing {
    pub path: String,
    pub branch_name: Option<String>,
}

/// Resolve the worktree path an archive request targets: explicit path, then
/// `repoRoot` + slug, then `repoRoot` + branch (looked up in `worktrees`).
pub async fn resolve_archive_target(
    input: &ArchiveCommandInput,
    worktrees: &[WorktreeListing],
    options: &WorktreeRootOptions,
) -> Result<String> {
    let repo_root = input.repo_root.as_deref().filter(|r| !r.is_empty());
    if let Some(p) = input.worktree_path.as_deref().filter(|p| !p.is_empty()) {
        return Ok(p.to_string());
    }
    if let Some(slug) = input.worktree_slug.as_deref().filter(|s| !s.is_empty()) {
        let Some(repo_root) = repo_root else {
            return Err(GitError::other(
                "repoRoot is required when worktreeSlug is supplied",
            ));
        };
        let root = get_paseo_worktrees_root(repo_root, options).await;
        return Ok(Path::new(&root).join(slug).to_string_lossy().into_owned());
    }
    if let (Some(_), Some(branch)) = (
        repo_root,
        input.branch_name.as_deref().filter(|b| !b.is_empty()),
    ) {
        return worktrees
            .iter()
            .find(|w| w.branch_name.as_deref() == Some(branch))
            .map(|w| w.path.clone())
            .ok_or_else(|| {
                GitError::other(format!("Paseo worktree not found for branch {branch}"))
            });
    }
    Err(GitError::other(
        "worktreePath, worktreeSlug, or repoRoot+branchName is required",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_archive_targets_in_priority_order() {
        let opts = WorktreeRootOptions {
            paseo_home: None,
            worktrees_root: Some("/wt-root".into()),
        };
        let list = [WorktreeListing {
            path: "/wt-root/h/feat".into(),
            branch_name: Some("feat".into()),
        }];
        let base = ArchiveCommandInput {
            request_id: "r".into(),
            ..Default::default()
        };

        let explicit = ArchiveCommandInput {
            worktree_path: Some("/x/y".into()),
            worktree_slug: Some("s".into()),
            ..base.clone()
        };
        assert_eq!(
            resolve_archive_target(&explicit, &list, &opts)
                .await
                .unwrap(),
            "/x/y"
        );

        let by_branch = ArchiveCommandInput {
            repo_root: Some("/repo".into()),
            branch_name: Some("feat".into()),
            ..base.clone()
        };
        assert_eq!(
            resolve_archive_target(&by_branch, &list, &opts)
                .await
                .unwrap(),
            "/wt-root/h/feat"
        );

        let missing = ArchiveCommandInput {
            repo_root: Some("/repo".into()),
            branch_name: Some("nope".into()),
            ..base.clone()
        };
        assert_eq!(
            resolve_archive_target(&missing, &list, &opts)
                .await
                .unwrap_err()
                .to_string(),
            "Paseo worktree not found for branch nope"
        );

        let slug_without_root = ArchiveCommandInput {
            worktree_slug: Some("s".into()),
            ..base.clone()
        };
        assert_eq!(
            resolve_archive_target(&slug_without_root, &list, &opts)
                .await
                .unwrap_err()
                .to_string(),
            "repoRoot is required when worktreeSlug is supplied"
        );

        assert_eq!(
            resolve_archive_target(&base, &list, &opts)
                .await
                .unwrap_err()
                .to_string(),
            "worktreePath, worktreeSlug, or repoRoot+branchName is required"
        );

        let by_slug = ArchiveCommandInput {
            repo_root: Some("/nonexistent-repo-root".into()),
            worktree_slug: Some("feat".into()),
            ..base
        };
        let p = resolve_archive_target(&by_slug, &list, &opts)
            .await
            .unwrap();
        assert!(p.starts_with("/wt-root/") && p.ends_with("/feat"), "{p}");
    }
}
