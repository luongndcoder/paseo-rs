mod common;

use common::*;
use paseo_git::error::GitError;
use paseo_git::mutations::*;
use paseo_git::repo::CheckoutContext;
use paseo_git::status::{get_checkout_status, CheckoutStatusResult};
use serde_json::json;

fn conflict_setup(f: &Fixture) {
    commit_file(&f.repo, "conflict.txt", "base\n", "add conflict file");
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(
        &f.repo,
        "conflict.txt",
        "feature change\n",
        "feature change",
    );
    git(&f.repo, &["checkout", "main"]);
    commit_file(&f.repo, "conflict.txt", "main change\n", "main change");
    git(&f.repo, &["checkout", "feature"]);
}

fn no_merge_in_progress(repo: &str) {
    assert_eq!(git_trim(repo, &["status", "--porcelain"]), "");
    assert!(!git_succeeds(
        repo,
        &["rev-parse", "-q", "--verify", "MERGE_HEAD"]
    ));
}

// ---- merge to base ------------------------------------------------------------------------

#[tokio::test]
async fn merges_the_current_branch_into_base_from_a_worktree_checkout() {
    let f = init_repo();
    let wt = create_paseo_worktree(&f, "merge", "merge-branch", "main").await;
    git(&wt, &["checkout", "-b", "feature"]);
    commit_file(&wt, "merge.txt", "feature\n", "feature commit");
    let feature_commit = git_trim(&wt, &["rev-parse", "HEAD"]);
    merge_to_base(
        &wt,
        &MergeToBaseOptions {
            base_ref: Some("main".into()),
            ..Default::default()
        },
        &f.ctx(),
    )
    .await
    .unwrap();
    assert!(git_succeeds(
        &f.repo,
        &["merge-base", "--is-ancestor", &feature_commit, "main"]
    ));
    let CheckoutStatusResult::Git(status) = get_checkout_status(&wt, &f.ctx()).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(status.ahead_behind.map_or(0, |a| a.ahead), 0);
    assert_eq!(
        git_trim(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "feature"
    );
}

#[tokio::test]
async fn reports_the_base_worktree_cwd_when_merge_to_base_mutates_a_separate_checkout() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "develop"]);
    commit_file(&f.repo, "develop.txt", "develop\n", "develop commit");
    git(&f.repo, &["checkout", "main"]);
    let base_wt = f.path("base-worktree");
    git(&f.repo, &["worktree", "add", &base_wt, "develop"]);
    let feature = create_paseo_worktree(&f, "feature-worktree", "feature", "develop").await;
    commit_file(&feature, "feature.txt", "feature\n", "feature commit");
    let mutated = merge_to_base(&feature, &MergeToBaseOptions::default(), &f.ctx())
        .await
        .unwrap();
    assert_eq!(real(&mutated), real(&base_wt));
    assert_ne!(mutated, feature);
}

#[tokio::test]
async fn merges_to_stored_base_ref_name_when_base_ref_is_not_provided() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "develop"]);
    commit_file(&f.repo, "file.txt", "develop\n", "develop change");
    git(&f.repo, &["checkout", "main"]);
    let wt = create_paseo_worktree(&f, "merge-to-develop", "feature", "develop").await;
    commit_file(&wt, "feature.txt", "feature\n", "feature commit");
    let feature_commit = git_trim(&wt, &["rev-parse", "HEAD"]);
    merge_to_base(&wt, &MergeToBaseOptions::default(), &f.ctx())
        .await
        .unwrap();
    assert!(git_succeeds(
        &f.repo,
        &["merge-base", "--is-ancestor", &feature_commit, "develop"]
    ));
    assert!(!git_succeeds(
        &f.repo,
        &["merge-base", "--is-ancestor", &feature_commit, "main"]
    ));
}

#[tokio::test]
async fn returns_typed_merge_conflict_error_on_merge_to_base_conflicts() {
    let f = init_repo();
    conflict_setup(&f);
    let err = merge_to_base(
        &f.repo,
        &MergeToBaseOptions {
            base_ref: Some("main".into()),
            ..Default::default()
        },
        &CheckoutContext::default(),
    )
    .await
    .unwrap_err();
    match &err {
        GitError::MergeConflict {
            base_ref,
            current_branch,
            conflict_files,
        } => {
            assert_eq!(
                (base_ref.as_str(), current_branch.as_str()),
                ("main", "feature")
            );
            assert!(
                conflict_files.iter().all(|f| f == "conflict.txt"),
                "{conflict_files:?}"
            );
            assert!(!conflict_files.is_empty());
        }
        other => panic!("expected MergeConflict, got {other:?}"),
    }
    assert_eq!(
        err.to_string(),
        "Merge conflict while merging feature into main"
    );
    assert_eq!(
        serde_json::to_value(paseo_git::to_checkout_error(&err)).unwrap(),
        json!({"code": "MERGE_CONFLICT", "message": "Merge conflict while merging feature into main"})
    );
    // The merge ran in the repo; it is aborted and the original branch restored.
    no_merge_in_progress(&f.repo);
    assert_eq!(git_trim(&f.repo, &["branch", "--show-current"]), "feature");
}

#[tokio::test]
async fn squash_merge_commits_with_a_default_message_and_honors_signing() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "feature.txt", "feature\n", "feature commit");
    let opts = MergeToBaseOptions {
        base_ref: Some("main".into()),
        mode: Some(MergeMode::Squash),
        commit_message: None,
    };
    merge_to_base(&f.repo, &opts, &CheckoutContext::default())
        .await
        .unwrap();
    assert_eq!(
        git_trim(&f.repo, &["log", "-1", "--pretty=%s", "main"]),
        "Squash merge feature into main"
    );
    assert_eq!(git_trim(&f.repo, &["branch", "--show-current"]), "feature");

    // signing failure surfaces from the squash commit
    let g = init_repo();
    git(&g.repo, &["checkout", "-b", "feature"]);
    commit_file(&g.repo, "feature.txt", "feature\n", "feature commit");
    git(&g.repo, &["config", "commit.gpgsign", "true"]);
    git(&g.repo, &["config", "gpg.format", "openpgp"]);
    git(&g.repo, &["config", "gpg.program", "/usr/bin/false"]);
    let err = merge_to_base(&g.repo, &opts, &CheckoutContext::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("failed to sign the data"), "{err}");
}

#[tokio::test]
async fn merge_to_base_on_the_base_branch_is_a_no_op() {
    let f = init_repo();
    let cwd = merge_to_base(
        &f.repo,
        &MergeToBaseOptions {
            base_ref: Some("main".into()),
            ..Default::default()
        },
        &CheckoutContext::default(),
    )
    .await
    .unwrap();
    assert_eq!(real(&cwd), real(&f.repo));
}

#[tokio::test]
async fn merge_to_base_refuses_non_origin_remote_refs() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature"]);
    let ctx = CheckoutContext::default();
    let err = merge_to_base(
        &f.repo,
        &MergeToBaseOptions {
            base_ref: Some("refs/remotes/upstream/main".into()),
            ..Default::default()
        },
        &ctx,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "No local merge target is recorded for base ref refs/remotes/upstream/main"
    );
}

// ---- merge from base -----------------------------------------------------------------------

#[tokio::test]
async fn merges_from_the_most_ahead_base_ref_when_origin_main_is_ahead() {
    let f = init_repo();
    let remote = add_bare_remote(&f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    let other = clone_remote(&f, &remote, "other-clone");
    commit_file(&other, "remote-only.txt", "remote\n", "remote only");
    let remote_commit = git_trim(&other, &["rev-parse", "HEAD"]);
    git(&other, &["push"]);
    git(&f.repo, &["fetch", "origin"]);
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "feature.txt", "feature\n", "feature commit");
    merge_from_base(
        &f.repo,
        &MergeFromBaseOptions {
            base_ref: Some("main".into()),
            require_clean_target: Some(true),
        },
        &CheckoutContext::default(),
    )
    .await
    .unwrap();
    assert!(git_succeeds(
        &f.repo,
        &["merge-base", "--is-ancestor", &remote_commit, "feature"]
    ));
}

#[tokio::test]
async fn merges_from_the_most_ahead_base_ref_when_local_main_is_ahead() {
    let f = init_repo();
    add_bare_remote(&f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    commit_file(&f.repo, "local-only.txt", "local\n", "local only");
    let local_commit = git_trim(&f.repo, &["rev-parse", "HEAD"]);
    git(
        &f.repo,
        &["checkout", "-b", "feature", &format!("{local_commit}~1")],
    );
    commit_file(&f.repo, "feature.txt", "feature\n", "feature commit");
    merge_from_base(
        &f.repo,
        &MergeFromBaseOptions {
            base_ref: Some("main".into()),
            require_clean_target: Some(true),
        },
        &CheckoutContext::default(),
    )
    .await
    .unwrap();
    assert!(git_succeeds(
        &f.repo,
        &["merge-base", "--is-ancestor", &local_commit, "feature"]
    ));
}

#[tokio::test]
async fn aborts_merge_from_base_on_conflicts_and_leaves_no_merge_in_progress() {
    let f = init_repo();
    conflict_setup(&f);
    let err = merge_from_base(
        &f.repo,
        &MergeFromBaseOptions {
            base_ref: Some("main".into()),
            require_clean_target: Some(true),
        },
        &CheckoutContext::default(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, GitError::MergeFromBaseConflict { .. }),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        "Merge conflict while merging main into feature. Please merge manually."
    );
    no_merge_in_progress(&f.repo);
}

#[tokio::test]
async fn merge_from_base_requires_a_clean_target_unless_told_otherwise() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature"]);
    write(format!("{}/file.txt", f.repo), "dirty\n");
    let err = merge_from_base(
        &f.repo,
        &MergeFromBaseOptions {
            base_ref: Some("main".into()),
            require_clean_target: None,
        },
        &CheckoutContext::default(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Working directory has uncommitted changes."
    );
    merge_from_base(
        &f.repo,
        &MergeFromBaseOptions {
            base_ref: Some("main".into()),
            require_clean_target: Some(false),
        },
        &CheckoutContext::default(),
    )
    .await
    .unwrap();
}

// ---- pull ------------------------------------------------------------------------------------

#[tokio::test]
async fn pulls_the_current_branch_from_origin() {
    let f = init_repo();
    let remote = add_bare_remote(&f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    let other = clone_remote(&f, &remote, "other-clone");
    commit_file(&other, "pulled.txt", "remote\n", "remote pull commit");
    let remote_commit = git_trim(&other, &["rev-parse", "HEAD"]);
    git(&other, &["push"]);
    pull_current_branch(&f.repo).await.unwrap();
    assert!(git_succeeds(
        &f.repo,
        &["merge-base", "--is-ancestor", &remote_commit, "HEAD"]
    ));
    assert_eq!(read(format!("{}/pulled.txt", f.repo)), "remote\n");
}

#[tokio::test]
async fn pull_requires_origin_and_a_branch() {
    let f = init_repo();
    assert_eq!(
        pull_current_branch(&f.repo).await.unwrap_err().to_string(),
        "Remote 'origin' is not configured."
    );
    git(&f.repo, &["checkout", "--detach"]);
    assert_eq!(
        pull_current_branch(&f.repo).await.unwrap_err().to_string(),
        "Unable to determine current branch for pull"
    );
}

async fn conflicting_pull_setup(f: &Fixture) {
    let remote = add_bare_remote(f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    commit_file(&f.repo, "conflict.txt", "local\n", "local conflict commit");
    let other = clone_remote(f, &remote, "other-clone");
    commit_file(&other, "conflict.txt", "remote\n", "remote conflict commit");
    git(&other, &["push"]);
}

#[tokio::test]
async fn aborts_pull_on_merge_conflicts_and_leaves_no_merge_in_progress() {
    let f = init_repo();
    conflicting_pull_setup(&f).await;
    assert!(pull_current_branch(&f.repo).await.is_err());
    no_merge_in_progress(&f.repo);
}

#[tokio::test]
async fn aborts_pull_on_rebase_conflicts_and_leaves_no_rebase_in_progress() {
    let f = init_repo();
    git(&f.repo, &["config", "pull.rebase", "true"]);
    conflicting_pull_setup(&f).await;
    assert!(pull_current_branch(&f.repo).await.is_err());
    let git_dir = git_trim(&f.repo, &["rev-parse", "--absolute-git-dir"]);
    assert_eq!(git_trim(&f.repo, &["status", "--porcelain"]), "");
    assert!(!exists(format!("{git_dir}/rebase-merge")));
    assert!(!exists(format!("{git_dir}/rebase-apply")));
}

// ---- push ---------------------------------------------------------------------------------------

#[tokio::test]
async fn pushes_the_current_branch_to_origin() {
    let f = init_repo();
    let remote = add_bare_remote(&f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "push.txt", "push\n", "push commit");
    push_current_branch(&f.repo).await.unwrap();
    git(
        &remote,
        &[
            "--git-dir",
            &remote,
            "show-ref",
            "--verify",
            "refs/heads/feature",
        ],
    );
    assert_eq!(
        git_trim(
            &f.repo,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]
        ),
        "origin/feature"
    );
}

#[tokio::test]
async fn push_without_origin_reports_the_missing_remote() {
    let f = init_repo();
    assert_eq!(
        push_current_branch(&f.repo).await.unwrap_err().to_string(),
        "Remote 'origin' is not configured."
    );
}

fn bare_clone(f: &Fixture, name: &str) -> String {
    let dir = f.path(name);
    git(&f.temp_dir, &["clone", "--bare", &f.repo, &dir]);
    dir
}

fn upstream_of(repo: &str) -> Option<String> {
    git_succeeds(
        repo,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .then(|| {
        git_trim(
            repo,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        )
    })
}

#[tokio::test]
async fn pushes_the_current_branch_to_its_configured_upstream() {
    let f = init_repo();
    let origin = bare_clone(&f, "origin.git");
    let pr_remote = bare_clone(&f, "pr-remote.git");
    git(&f.repo, &["remote", "add", "origin", &origin]);
    git(&f.repo, &["remote", "add", "paseo-pr-526", &pr_remote]);
    git(&f.repo, &["checkout", "-b", "therainisme/main"]);
    git(&f.repo, &["fetch", "paseo-pr-526", "main"]);
    git(
        &f.repo,
        &["config", "branch.therainisme/main.remote", "paseo-pr-526"],
    );
    git(
        &f.repo,
        &["config", "branch.therainisme/main.merge", "refs/heads/main"],
    );
    commit_file(&f.repo, "fork-pr.txt", "fork pr edit\n", "fork pr edit");
    let head = git_trim(&f.repo, &["rev-parse", "HEAD"]);
    push_current_branch(&f.repo).await.unwrap();
    assert_eq!(
        git_trim(
            &pr_remote,
            &["--git-dir", &pr_remote, "rev-parse", "refs/heads/main"]
        ),
        head
    );
    assert_eq!(upstream_of(&f.repo).as_deref(), Some("paseo-pr-526/main"));
}

#[tokio::test]
async fn pushes_the_current_branch_to_its_configured_push_remote() {
    let f = init_repo();
    let origin = bare_clone(&f, "origin.git");
    let pr_remote = bare_clone(&f, "pr-remote.git");
    git(&f.repo, &["remote", "add", "origin", &origin]);
    git(&f.repo, &["remote", "add", "paseo-pr-526", &pr_remote]);
    git(&f.repo, &["checkout", "-b", "therainisme/main"]);
    git(
        &f.repo,
        &[
            "config",
            "branch.therainisme/main.pushRemote",
            "paseo-pr-526",
        ],
    );
    git(
        &f.repo,
        &["config", "remote.paseo-pr-526.push", "HEAD:refs/heads/main"],
    );
    commit_file(&f.repo, "fork-pr.txt", "fork pr edit\n", "fork pr edit");
    let head = git_trim(&f.repo, &["rev-parse", "HEAD"]);
    assert_eq!(upstream_of(&f.repo), None);
    push_current_branch(&f.repo).await.unwrap();
    assert_eq!(
        git_trim(
            &pr_remote,
            &["--git-dir", &pr_remote, "rev-parse", "refs/heads/main"]
        ),
        head
    );
    assert_eq!(upstream_of(&f.repo).as_deref(), Some("paseo-pr-526/main"));
    assert_eq!(
        git_trim(&f.repo, &["rev-parse", "refs/remotes/paseo-pr-526/main"]),
        head
    );
    let CheckoutStatusResult::Git(s) = get_checkout_status(&f.repo, &CheckoutContext::default())
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!((s.ahead_of_origin, s.behind_of_origin), (Some(0), Some(0)));
    assert!(!git_succeeds(
        &origin,
        &[
            "--git-dir",
            &origin,
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/therainisme/main"
        ]
    ));
}

#[tokio::test]
async fn refreshes_the_tracked_ref_after_pushing_through_a_configured_push_remote() {
    let f = init_repo();
    let origin = bare_clone(&f, "origin.git");
    git(&f.repo, &["remote", "add", "origin", &origin]);
    git(&f.repo, &["checkout", "-b", "feature"]);
    git(&f.repo, &["push", "-u", "origin", "feature"]);
    git(&f.repo, &["remote", "add", "paseo-pr-1790", &origin]);
    git(
        &f.repo,
        &["config", "branch.feature.pushRemote", "paseo-pr-1790"],
    );
    git(
        &f.repo,
        &[
            "config",
            "remote.paseo-pr-1790.push",
            "HEAD:refs/heads/feature",
        ],
    );
    commit_file(&f.repo, "feature.txt", "feature edit\n", "feature edit");
    let head = git_trim(&f.repo, &["rev-parse", "HEAD"]);
    let before = match get_checkout_status(&f.repo, &CheckoutContext::default())
        .await
        .unwrap()
    {
        CheckoutStatusResult::Git(g) => g,
        _ => panic!(),
    };
    assert_eq!(
        (before.ahead_of_origin, before.behind_of_origin),
        (Some(1), Some(0))
    );
    push_current_branch(&f.repo).await.unwrap();
    assert_eq!(
        git_trim(&f.repo, &["rev-parse", "refs/remotes/origin/feature"]),
        head
    );
    let after = match get_checkout_status(&f.repo, &CheckoutContext::default())
        .await
        .unwrap()
    {
        CheckoutStatusResult::Git(g) => g,
        _ => panic!(),
    };
    assert_eq!(
        (after.ahead_of_origin, after.behind_of_origin),
        (Some(0), Some(0))
    );
}

#[tokio::test]
async fn pushes_ordinary_branches_to_their_configured_upstream() {
    let f = init_repo();
    let origin = bare_clone(&f, "origin.git");
    let upstream = bare_clone(&f, "upstream.git");
    git(&f.repo, &["remote", "add", "origin", &origin]);
    git(&f.repo, &["remote", "add", "upstream", &upstream]);
    git(&f.repo, &["fetch", "upstream", "main"]);
    git(&f.repo, &["checkout", "-b", "contrib"]);
    git(&f.repo, &["config", "branch.contrib.remote", "upstream"]);
    git(
        &f.repo,
        &["config", "branch.contrib.merge", "refs/heads/main"],
    );
    commit_file(&f.repo, "contrib.txt", "contrib edit\n", "contrib edit");
    let head = git_trim(&f.repo, &["rev-parse", "HEAD"]);
    push_current_branch(&f.repo).await.unwrap();
    assert_eq!(
        git_trim(
            &upstream,
            &["--git-dir", &upstream, "rev-parse", "refs/heads/main"]
        ),
        head
    );
    assert!(!git_succeeds(
        &origin,
        &[
            "--git-dir",
            &origin,
            "show-ref",
            "--verify",
            "--quiet",
            "refs/heads/contrib"
        ]
    ));
}

// ---- commit / stage / unstage -------------------------------------------------------------------------

#[tokio::test]
async fn commit_changes_honors_paths_amend_and_add_all() {
    let f = init_repo();
    write(format!("{}/a.txt", f.repo), "a\n");
    write(format!("{}/b.txt", f.repo), "b\n");
    commit_changes(
        &f.repo,
        &CommitOptions {
            message: "only a".into(),
            paths: Some(vec!["a.txt".into()]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        git_trim(&f.repo, &["show", "--name-only", "--format=", "HEAD"]),
        "a.txt"
    );
    assert_eq!(git_trim(&f.repo, &["status", "--porcelain"]), "?? b.txt");

    commit_changes(
        &f.repo,
        &CommitOptions {
            message: "amended".into(),
            add_all: Some(false),
            amend: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(git_trim(&f.repo, &["log", "-1", "--format=%s"]), "amended");
    assert_eq!(git_trim(&f.repo, &["rev-list", "--count", "HEAD"]), "2");

    commit_changes(
        &f.repo,
        &CommitOptions {
            message: "rest".into(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(git_trim(&f.repo, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn commit_paths_are_literal_pathspecs() {
    let f = init_repo();
    write(format!("{}/foo[ab].txt", f.repo), "literal\n");
    write(format!("{}/fooa.txt", f.repo), "sibling\n");
    commit_changes(
        &f.repo,
        &CommitOptions {
            message: "literal".into(),
            paths: Some(vec!["foo[ab].txt".into()]),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(git_trim(&f.repo, &["status", "--porcelain"]), "?? fooa.txt");
}

#[tokio::test]
async fn stage_and_unstage_round_trip_with_the_discard_action_dispatch() {
    let f = init_repo();
    write(format!("{}/new.txt", f.repo), "new\n");
    write(format!("{}/file.txt", f.repo), "changed\n");
    apply_discard_action(
        &f.repo,
        &["new.txt".into(), "file.txt".into()],
        Some(DiscardAction::Stage),
    )
    .await
    .unwrap();
    assert_eq!(
        git_trim(&f.repo, &["status", "--porcelain"]),
        "M  file.txt\nA  new.txt"
    );
    apply_discard_action(&f.repo, &["new.txt".into()], Some(DiscardAction::Unstage))
        .await
        .unwrap();
    assert_eq!(
        git_trim(&f.repo, &["status", "--porcelain"]),
        "M  file.txt\n?? new.txt"
    );
    apply_discard_action(&f.repo, &["file.txt".into(), "new.txt".into()], None)
        .await
        .unwrap();
    assert_eq!(git_trim(&f.repo, &["status", "--porcelain"]), "");
    stage_changes(&f.repo, &[]).await.unwrap();
    unstage_changes(&f.repo, &[]).await.unwrap();
}

#[tokio::test]
async fn unstage_works_with_an_unborn_head() {
    let f = init_repo();
    let repo = f.path("unborn");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    write(format!("{repo}/new.txt"), "new\n");
    git(&repo, &["add", "new.txt"]);
    unstage_changes(&repo, &["new.txt".into()]).await.unwrap();
    assert_eq!(git_trim(&repo, &["status", "--porcelain"]), "?? new.txt");
}

#[test]
fn success_payload_shape() {
    assert_eq!(
        build_success_payload("/c", "r", &Ok(())),
        json!({"cwd": "/c", "success": true, "error": null, "requestId": "r"})
    );
    let failed = build_success_payload("/c", "r", &Err(GitError::NotGitRepo { cwd: "/c".into() }));
    assert_eq!(
        failed["error"],
        json!({"code": "NOT_GIT_REPO", "message": "Not a git repository: /c"})
    );
    assert_eq!(failed["success"], false);
}
