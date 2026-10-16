mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use paseo_git::diff::{get_checkout_diff, CheckoutDiffCompare};
use paseo_git::mutations::{commit_all, discard_changes};
use paseo_git::repo::{
    get_checkout_snapshot_facts, get_main_repo_root, resolve_repository_default_branch,
    AheadBehind, CheckoutContext, CheckoutSnapshotFacts, GitFacts, UpstreamStatus,
};
use paseo_git::run::start_git_command_metrics;
use paseo_git::shortstat::{
    get_checkout_ref_derived_state, get_checkout_shortstat_uncached, CheckoutRefDerivedState,
    CheckoutShortstat, CheckoutShortstatCache,
};
use paseo_git::status::{get_checkout_status, CheckoutStatusGit, CheckoutStatusResult};
use serde_json::json;

async fn status(cwd: &str, ctx: &CheckoutContext) -> CheckoutStatusGit {
    match get_checkout_status(cwd, ctx).await.unwrap() {
        CheckoutStatusResult::Git(g) => *g,
        CheckoutStatusResult::NotGit => panic!("expected a git checkout at {cwd}"),
    }
}

async fn shortstat(cwd: &str) -> Option<CheckoutShortstat> {
    get_checkout_shortstat_uncached(cwd, &CheckoutContext::default(), false)
        .await
        .unwrap()
}

fn ss(additions: u64, deletions: u64) -> Option<CheckoutShortstat> {
    Some(CheckoutShortstat {
        additions,
        deletions,
    })
}

// ---- non-git / basic -----------------------------------------------------------------

#[tokio::test]
async fn reports_an_ordinary_directory_as_non_git() {
    let f = init_repo();
    let dir = f.path("not-git-status");
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(
        get_checkout_status(&dir, &CheckoutContext::default())
            .await
            .unwrap(),
        CheckoutStatusResult::NotGit
    );
}

#[tokio::test]
async fn reports_a_missing_directory_as_non_git() {
    let f = init_repo();
    let missing = f.path("missing-git-cwd");
    assert_eq!(
        get_checkout_status(&missing, &CheckoutContext::default())
            .await
            .unwrap(),
        CheckoutStatusResult::NotGit
    );
}

#[tokio::test]
async fn handles_status_diff_commit_in_a_normal_repo() {
    let f = init_repo();
    write(format!("{}/file.txt", f.repo), "updated\n");
    let s = status(&f.repo, &CheckoutContext::default()).await;
    assert_eq!(
        (s.current_branch.as_deref(), s.is_dirty, s.has_remote),
        (Some("main"), true, false)
    );
    let diff = get_checkout_diff(
        &f.repo,
        &CheckoutDiffCompare::uncommitted(),
        &CheckoutContext::default(),
    )
    .await
    .unwrap();
    assert!(diff.diff.contains("-hello") && diff.diff.contains("+updated"));
    commit_all(&f.repo, "update file").await.unwrap();
    assert!(!status(&f.repo, &CheckoutContext::default()).await.is_dirty);
    assert_eq!(
        git_trim(&f.repo, &["log", "-1", "--pretty=%B"]),
        "update file"
    );
}

#[tokio::test]
async fn honors_commit_signing_configuration() {
    let f = init_repo();
    git(&f.repo, &["config", "commit.gpgsign", "true"]);
    git(&f.repo, &["config", "gpg.format", "openpgp"]);
    git(&f.repo, &["config", "gpg.program", "/usr/bin/false"]);
    write(format!("{}/file.txt", f.repo), "signed\n");
    let err = commit_all(&f.repo, "signed update").await.unwrap_err();
    assert!(err.to_string().contains("failed to sign the data"), "{err}");
}

#[tokio::test]
async fn commits_messages_with_quotes_safely() {
    let f = init_repo();
    let message = "He said \"hello\" and it's fine";
    write(format!("{}/file.txt", f.repo), "quoted\n");
    commit_all(&f.repo, message).await.unwrap();
    assert_eq!(git_trim(&f.repo, &["log", "-1", "--pretty=%B"]), message);
}

#[tokio::test]
async fn returns_checkout_root_metadata_for_normal_repos() {
    let f = init_repo();
    let s = status(&f.repo, &CheckoutContext::default()).await;
    assert_eq!(s.current_branch.as_deref(), Some("main"));
    assert_eq!(real(&s.repo_root), real(&f.repo));
    assert!(!s.is_paseo_owned_worktree);
    assert_eq!(s.main_repo_root, None);
}

#[tokio::test]
async fn exposes_has_remote_when_origin_is_configured() {
    let f = init_repo();
    add_bare_remote(&f);
    assert!(
        status(&f.repo, &CheckoutContext::default())
            .await
            .has_remote
    );
}

// ---- ahead / behind / upstream ---------------------------------------------------------------

#[tokio::test]
async fn reports_ahead_behind_relative_to_origin_on_the_base_branch() {
    let f = init_repo();
    let (_remote, clone) = setup_remote_tracking_main(&f);
    commit_file(&clone, "file.txt", "remote\n", "remote update");
    git(&clone, &["push"]);
    git(&f.repo, &["fetch", "origin"]);
    let behind = status(&f.repo, &CheckoutContext::default()).await;
    assert_eq!(
        (behind.ahead_of_origin, behind.behind_of_origin),
        (Some(0), Some(1))
    );

    for i in 1..=3 {
        commit_file(
            &f.repo,
            &format!("local-{i}.txt"),
            &format!("local {i}\n"),
            &format!("local update {i}"),
        );
    }
    commit_file(&clone, "remote-2.txt", "remote 2\n", "remote update 2");
    git(&clone, &["push"]);
    git(&f.repo, &["fetch", "origin"]);

    let metrics = start_git_command_metrics();
    let facts = get_checkout_snapshot_facts(&f.repo, &CheckoutContext::default())
        .await
        .unwrap();
    let ctx = CheckoutContext {
        facts: Some(facts),
        ..Default::default()
    };
    let diverged = status(&f.repo, &ctx).await;
    let commands = metrics.stop();
    let upstream: Vec<_> = commands
        .iter()
        .filter(|c| {
            c.cwd == f.repo
                && c.args[0] == "for-each-ref"
                && c.args.join(" ").contains("%(upstream)")
        })
        .collect();
    assert_eq!(
        diverged.upstream_ref.as_deref(),
        Some("refs/remotes/origin/main")
    );
    assert_eq!(
        (diverged.ahead_of_origin, diverged.behind_of_origin),
        (Some(3), Some(2))
    );
    assert_eq!(upstream.len(), 1);
    assert_eq!(
        upstream[0].args,
        [
            "for-each-ref",
            "--format=%(upstream)%00%(upstream:track,nobracket)",
            "refs/heads/main"
        ]
    );
}

#[tokio::test]
async fn reports_the_fork_remote_not_origin_when_the_branch_tracks_a_second_remote() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    let fork = f.path("fork.git");
    git(&f.temp_dir, &["init", "--bare", "-b", "main", &fork]);
    git(&f.repo, &["remote", "add", "upstream", &fork]);
    git(&f.repo, &["push", "-u", "upstream", "main"]);
    assert_eq!(
        status(&f.repo, &CheckoutContext::default())
            .await
            .upstream_ref
            .as_deref(),
        Some("refs/remotes/upstream/main")
    );
}

#[tokio::test]
async fn reports_the_upstream_branch_name_when_it_differs_from_the_local_branch_name() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "feature.txt", "feature\n", "feature commit");
    git(&f.repo, &["push", "-u", "origin", "HEAD:other-name"]);
    assert_eq!(
        status(&f.repo, &CheckoutContext::default())
            .await
            .upstream_ref
            .as_deref(),
        Some("refs/remotes/origin/other-name")
    );
}

#[tokio::test]
async fn reports_the_upstream_when_the_repos_only_remote_is_not_origin() {
    let f = init_repo();
    let fork = f.path("fork.git");
    git(&f.temp_dir, &["init", "--bare", "-b", "main", &fork]);
    git(&f.repo, &["remote", "add", "upstream", &fork]);
    git(&f.repo, &["push", "-u", "upstream", "main"]);
    let s = status(&f.repo, &CheckoutContext::default()).await;
    assert_eq!(
        s.upstream_ref.as_deref(),
        Some("refs/remotes/upstream/main")
    );
    assert_eq!(s.ahead_of_origin, Some(0));
}

#[tokio::test]
async fn is_not_fooled_by_a_local_branch_named_like_the_remote_tracking_ref() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    commit_file(&f.repo, "shadow.txt", "shadow\n", "shadow commit");
    git(&f.repo, &["branch", "origin/main", "main"]);
    let s = status(&f.repo, &CheckoutContext::default()).await;
    assert_eq!(s.upstream_ref.as_deref(), Some("refs/remotes/origin/main"));
    assert_eq!((s.ahead_of_origin, s.behind_of_origin), (Some(1), Some(0)));
}

#[tokio::test]
async fn reports_no_upstream_ref_for_a_branch_that_was_never_pushed() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    git(&f.repo, &["checkout", "-b", "local-only"]);
    commit_file(&f.repo, "local-only.txt", "local\n", "local commit");
    let s = status(&f.repo, &CheckoutContext::default()).await;
    assert_eq!((s.upstream_ref, s.ahead_of_origin), (None, None));
}

async fn pr_worktree_setup(f: &Fixture) -> (String, String) {
    setup_remote_tracking_main(f);
    let pr_remote = f.path("pr-remote.git");
    git(&f.temp_dir, &["init", "--bare", "-b", "main", &pr_remote]);
    let branch = "aaronzhongg/open-button-targets-active-file";
    git(&f.repo, &["checkout", "-b", branch]);
    commit_file(&f.repo, "feature.txt", "feature\n", "feature commit");
    git(&f.repo, &["remote", "add", "paseo-pr-1285", &pr_remote]);
    git(
        &f.repo,
        &[
            "push",
            "paseo-pr-1285",
            "HEAD:refs/heads/open-button-targets-active-file",
        ],
    );
    git(
        &f.repo,
        &[
            "config",
            &format!("branch.{branch}.remote"),
            "paseo-pr-1285",
        ],
    );
    git(
        &f.repo,
        &[
            "config",
            &format!("branch.{branch}.merge"),
            "refs/heads/open-button-targets-active-file",
        ],
    );
    (pr_remote, branch.to_string())
}

#[tokio::test]
async fn reports_a_pr_worktree_as_not_ahead_when_its_branch_is_pushed_to_the_configured_pr_remote()
{
    let f = init_repo();
    let (_, branch) = pr_worktree_setup(&f).await;
    let s = status(&f.repo, &CheckoutContext::default()).await;
    assert_eq!(
        (s.current_branch.as_deref(), s.ahead_of_origin),
        (Some(branch.as_str()), Some(0))
    );
}

#[tokio::test]
async fn reports_a_pr_worktree_as_behind_when_its_configured_pr_remote_has_newer_commits() {
    let f = init_repo();
    let (pr_remote, _) = pr_worktree_setup(&f).await;
    let clone = clone_remote(&f, &pr_remote, "pr-clone");
    git(&clone, &["checkout", "open-button-targets-active-file"]);
    commit_file(&clone, "remote.txt", "remote\n", "remote update");
    git(&clone, &["push"]);
    git(&f.repo, &["fetch", "paseo-pr-1285"]);
    assert_eq!(
        status(&f.repo, &CheckoutContext::default())
            .await
            .behind_of_origin,
        Some(1)
    );
}

#[tokio::test]
async fn does_not_report_the_full_branch_history_as_ahead_when_the_current_branch_remote_is_gone() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "feature.txt", "feature\n", "feature commit");
    git(&f.repo, &["push", "-u", "origin", "feature"]);
    git(&f.repo, &["push", "origin", "--delete", "feature"]);
    git(&f.repo, &["fetch", "--prune", "origin"]);
    let s = status(&f.repo, &CheckoutContext::default()).await;
    assert_eq!((s.ahead_of_origin, s.behind_of_origin), (None, None));
}

#[tokio::test]
async fn does_not_report_full_history_as_unpushed_for_fresh_no_track_paseo_worktrees() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    commit_file(&f.repo, "second.txt", "second\n", "second commit");
    git(&f.repo, &["push"]);
    let wt = create_paseo_worktree(&f, "fresh-feature", "fresh-feature", "main").await;
    let s = status(&wt, &f.ctx()).await;
    assert!(s.is_paseo_owned_worktree);
    assert_eq!(s.base_ref.as_deref(), Some("main"));
    assert_eq!(
        s.ahead_behind,
        Some(AheadBehind {
            ahead: 0,
            behind: 0
        })
    );
    assert_eq!(s.ahead_of_origin, None);
}

#[tokio::test]
async fn does_not_report_local_only_no_track_worktree_commits_as_ahead_of_origin() {
    let f = init_repo();
    git(&f.repo, &["remote", "add", "origin", &f.repo]);
    git(
        &f.repo,
        &["fetch", "origin", "main:refs/remotes/origin/main"],
    );
    let wt = create_paseo_worktree(&f, "fresh-feature", "fresh-feature", "main").await;
    commit_file(&wt, "feature.txt", "feature\n", "feature commit");
    let s = status(&wt, &f.ctx()).await;
    assert!(s.has_remote && s.is_paseo_owned_worktree);
    assert_eq!(s.base_ref.as_deref(), Some("main"));
    assert_eq!(
        s.ahead_behind,
        Some(AheadBehind {
            ahead: 1,
            behind: 0
        })
    );
    assert_eq!(s.ahead_of_origin, None);
}

#[tokio::test]
async fn uses_the_freshest_comparison_base_for_status_and_shortstat_when_local_main_is_stale() {
    let f = init_repo();
    let remote = add_bare_remote(&f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    let clone = clone_remote(&f, &remote, "clone");
    commit_file(
        &clone,
        "upstream.txt",
        "upstream 1\nupstream 2\n",
        "remote update",
    );
    git(&clone, &["push"]);
    git(&f.repo, &["fetch", "origin"]);
    git(&f.repo, &["checkout", "-b", "feature", "origin/main"]);
    commit_file(&f.repo, "feature.txt", "feature\n", "feature update");
    let s = status(&f.repo, &CheckoutContext::default()).await;
    assert_eq!(s.base_ref.as_deref(), Some("main"));
    assert_eq!(
        s.ahead_behind,
        Some(AheadBehind {
            ahead: 1,
            behind: 0
        })
    );
    assert_eq!(shortstat(&f.repo).await, ss(1, 0));
}

#[tokio::test]
async fn does_not_count_origin_base_commits_as_feature_changes_in_shortstat_when_local_main_is_stale(
) {
    let f = init_repo();
    let remote = add_bare_remote(&f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    let other = clone_remote(&f, &remote, "other-clone");
    commit_file(
        &other,
        "already-on-origin.txt",
        "origin\n",
        "origin base commit",
    );
    git(&other, &["push"]);
    commit_file(
        &f.repo,
        "local-only-base.txt",
        "local\n",
        "local base drift",
    );
    git(&f.repo, &["fetch", "origin"]);
    git(&f.repo, &["checkout", "-b", "feature", "origin/main"]);
    assert_eq!(shortstat(&f.repo).await, None);
}

// ---- shortstat ------------------------------------------------------------------------------

#[tokio::test]
async fn does_not_report_incoming_additions_when_the_base_branch_is_behind_its_remote() {
    let f = init_repo();
    let (_, clone) = setup_remote_tracking_main(&f);
    commit_file(
        &clone,
        "file.txt",
        "remote one\nremote two\n",
        "remote update",
    );
    git(&clone, &["push"]);
    git(&f.repo, &["fetch", "origin"]);
    assert_eq!(shortstat(&f.repo).await, None);
}

#[tokio::test]
async fn does_not_report_incoming_deletions_when_the_base_branch_is_behind_its_remote() {
    let f = init_repo();
    let (_, clone) = setup_remote_tracking_main(&f);
    commit_file(&clone, "file.txt", "", "remote deletion");
    git(&clone, &["push"]);
    git(&f.repo, &["fetch", "origin"]);
    assert_eq!(shortstat(&f.repo).await, None);
}

#[tokio::test]
async fn reports_outgoing_changes_when_the_base_branch_is_ahead_of_its_remote() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    commit_file(
        &f.repo,
        "file.txt",
        "local one\nlocal two\n",
        "local update",
    );
    assert_eq!(shortstat(&f.repo).await, ss(2, 1));
}

#[tokio::test]
async fn uses_the_merge_base_for_shortstat_when_the_base_branch_diverged_from_its_remote() {
    let f = init_repo();
    let (_, clone) = setup_remote_tracking_main(&f);
    commit_file(
        &clone,
        "file.txt",
        "remote one\nremote two\n",
        "remote update",
    );
    git(&clone, &["push"]);
    git(&f.repo, &["fetch", "origin"]);
    commit_file(&f.repo, "local.txt", "local\n", "local update");
    assert_eq!(shortstat(&f.repo).await, ss(1, 0));
}

#[tokio::test]
async fn keeps_base_branch_divergence_pointed_at_local_work_when_the_remote_has_more_commits() {
    let f = init_repo();
    let (_, clone) = setup_remote_tracking_main(&f);
    commit_file(
        &clone,
        "remote-one.txt",
        "remote one\n",
        "remote update one",
    );
    commit_file(
        &clone,
        "remote-two.txt",
        "remote two\n",
        "remote update two",
    );
    git(&clone, &["push"]);
    git(&f.repo, &["fetch", "origin"]);
    commit_file(&f.repo, "local.txt", "local\n", "local update");
    assert_eq!(shortstat(&f.repo).await, ss(1, 0));
}

#[tokio::test]
async fn reports_only_working_tree_changes_when_the_base_branch_is_behind() {
    let f = init_repo();
    commit_file(&f.repo, "tracked.txt", "tracked base\n", "add tracked file");
    let (_, clone) = setup_remote_tracking_main(&f);
    commit_file(&clone, "incoming.txt", "incoming\n", "remote incoming");
    git(&clone, &["push"]);
    git(&f.repo, &["fetch", "origin"]);
    write(format!("{}/tracked.txt", f.repo), "local one\nlocal two\n");
    assert_eq!(shortstat(&f.repo).await, ss(2, 1));
}

#[tokio::test]
async fn keeps_feature_shortstat_scoped_to_feature_changes_when_the_base_remote_is_ahead() {
    let f = init_repo();
    let (_, clone) = setup_remote_tracking_main(&f);
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "feature.txt", "feature\n", "feature update");
    git(&clone, &["checkout", "main"]);
    commit_file(&clone, "base.txt", "base\n", "base update");
    git(&clone, &["push"]);
    git(&f.repo, &["fetch", "origin"]);
    assert_eq!(shortstat(&f.repo).await, ss(1, 0));
}

#[tokio::test]
async fn does_not_report_incoming_base_changes_when_a_feature_branch_has_no_local_work_beyond_merge_base(
) {
    let f = init_repo();
    let (_, clone) = setup_remote_tracking_main(&f);
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&clone, "incoming.txt", "incoming\n", "remote incoming");
    git(&clone, &["push"]);
    git(&f.repo, &["fetch", "origin"]);
    assert_eq!(shortstat(&f.repo).await, None);
}

#[tokio::test]
async fn reports_feature_shortstat_ahead_of_the_comparison_merge_base() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "feature.txt", "feature\n", "feature update");
    assert_eq!(shortstat(&f.repo).await, ss(1, 0));
}

#[tokio::test]
async fn includes_untracked_file_lines_in_shortstat_additions() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "committed.txt", "one\n", "add committed");
    write(format!("{}/untracked.txt", f.repo), "line1\nline2\nline3\n");
    assert_eq!(shortstat(&f.repo).await, ss(4, 0));
}

#[tokio::test]
async fn reports_untracked_only_additions_when_no_tracked_changes_exist() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    write(format!("{}/newfile.txt", f.repo), "a\nb\n");
    assert_eq!(shortstat(&f.repo).await, ss(2, 0));
}

#[tokio::test]
async fn counts_empty_untracked_files_as_zero_additions() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "committed.txt", "one\n", "add committed");
    write(format!("{}/empty.txt", f.repo), "");
    assert_eq!(shortstat(&f.repo).await, ss(1, 0));
}

#[tokio::test]
async fn uses_the_merge_base_for_shortstat_when_a_feature_branch_diverged_from_its_tracked_remote()
{
    let f = init_repo();
    let (remote, _) = setup_remote_tracking_main(&f);
    git(&f.repo, &["checkout", "-b", "feature"]);
    git(&f.repo, &["push", "-u", "origin", "feature"]);
    let feature_clone = clone_remote(&f, &remote, "feature-clone");
    git(&feature_clone, &["checkout", "feature"]);
    commit_file(
        &feature_clone,
        "remote-feature.txt",
        "remote feature\n",
        "remote feature update",
    );
    git(&feature_clone, &["push"]);
    commit_file(
        &f.repo,
        "local-feature.txt",
        "local feature\n",
        "local feature update",
    );
    git(&f.repo, &["fetch", "origin"]);
    assert_eq!(shortstat(&f.repo).await, ss(1, 0));
}

#[tokio::test]
async fn uses_the_remote_only_base_branch_as_the_feature_shortstat_comparison() {
    let f = init_repo();
    let (_, clone) = setup_remote_tracking_main(&f);
    git(&f.repo, &["remote", "set-head", "origin", "main"]);
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "feature.txt", "feature\n", "feature update");
    git(&f.repo, &["branch", "-D", "main"]);
    commit_file(&clone, "base.txt", "base\n", "base update");
    git(&clone, &["push"]);
    git(&f.repo, &["fetch", "origin"]);
    assert_eq!(shortstat(&f.repo).await, ss(1, 0));
}

#[tokio::test]
async fn returns_no_shortstat_for_a_clean_base_branch_that_is_up_to_date_with_its_remote() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    git(&f.repo, &["fetch", "origin"]);
    assert_eq!(shortstat(&f.repo).await, None);
}

#[tokio::test]
async fn reports_working_tree_changes_when_the_base_branch_has_no_ahead_commits() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    write(format!("{}/file.txt", f.repo), "local one\nlocal two\n");
    assert_eq!(shortstat(&f.repo).await, ss(2, 1));
}

#[tokio::test]
async fn falls_back_to_the_local_base_branch_when_origin_is_absent() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "local-feature.txt", "feature\n", "local feature");
    assert_eq!(shortstat(&f.repo).await, ss(1, 0));
}

#[tokio::test]
async fn shortstat_cache_warms_in_the_background_and_caches_null() {
    let f = init_repo();
    let cache = Arc::new(CheckoutShortstatCache::default());
    assert_eq!(cache.peek(&f.repo), None);
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = done.clone();
    cache.warm_in_background(
        &f.repo,
        &CheckoutContext::default(),
        Some(Box::new(move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst)
        })),
    );
    // A second warm while the first is in flight is a no-op.
    cache.warm_in_background(&f.repo, &CheckoutContext::default(), None);
    for _ in 0..40 {
        if let Some(cached) = cache.peek(&f.repo) {
            // A repo with no origin/main computes to null, but null is still cached.
            assert_eq!(cached, None);
            assert!(
                done.load(std::sync::atomic::Ordering::SeqCst) || {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    done.load(std::sync::atomic::Ordering::SeqCst)
                }
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("shortstat background warm did not populate the cache in time");
}

#[tokio::test]
async fn shortstat_cache_serves_fresh_values_and_force_refreshes() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    let cache = CheckoutShortstatCache::default();
    let ctx = CheckoutContext::default();
    assert_eq!(cache.get(&f.repo, &ctx, false).await.unwrap(), None);
    write(format!("{}/file.txt", f.repo), "local one\nlocal two\n");
    assert_eq!(
        cache.get(&f.repo, &ctx, false).await.unwrap(),
        None,
        "served from cache"
    );
    assert_eq!(cache.get(&f.repo, &ctx, true).await.unwrap(), ss(2, 1));
    assert_eq!(
        cache.get(&f.repo, &ctx, false).await.unwrap(),
        ss(2, 1),
        "force repopulates the cache"
    );
}

// ---- facts ------------------------------------------------------------------------------------

#[tokio::test]
async fn reads_the_origin_url_once_when_collecting_facts_for_an_origin_tracking_branch() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    let metrics = start_git_command_metrics();
    let facts = get_checkout_snapshot_facts(&f.repo, &f.ctx())
        .await
        .unwrap();
    let commands: Vec<String> = metrics
        .stop()
        .into_iter()
        .filter(|c| c.cwd == f.repo)
        .map(|c| c.args.join(" "))
        .collect();
    assert!(matches!(facts, CheckoutSnapshotFacts::Git(_)));
    assert_eq!(
        commands
            .iter()
            .filter(|c| *c == "config --get remote.origin.url")
            .count(),
        1
    );
}

#[tokio::test]
async fn reuses_checkout_snapshot_facts_across_status_and_shortstat_reads() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    git(&f.repo, &["checkout", "-b", "feature/facts"]);
    commit_file(&f.repo, "feature.txt", "feature\n", "feature");
    write(format!("{}/feature.txt", f.repo), "feature\nchanged\n");

    let facts = get_checkout_snapshot_facts(&f.repo, &f.ctx())
        .await
        .unwrap();
    let ctx = CheckoutContext {
        facts: Some(facts),
        ..f.ctx()
    };
    let plain = status(&f.repo, &f.ctx()).await;
    let plain_stat = get_checkout_shortstat_uncached(&f.repo, &f.ctx(), false)
        .await
        .unwrap();

    let metrics = start_git_command_metrics();
    let with_facts = status(&f.repo, &ctx).await;
    let stat_with_facts = get_checkout_shortstat_uncached(&f.repo, &ctx, false)
        .await
        .unwrap();
    let commands: Vec<String> = metrics
        .stop()
        .into_iter()
        .filter(|c| c.cwd == f.repo)
        .map(|c| c.args.join(" "))
        .collect();
    assert_eq!(with_facts, plain);
    assert_eq!(stat_with_facts, plain_stat);
    assert!(!commands.contains(&"rev-parse --show-toplevel".to_string()));
    assert!(!commands.contains(&"rev-parse --abbrev-ref HEAD".to_string()));
}

#[tokio::test]
async fn facts_describe_a_non_origin_branch_remote() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    git(
        &f.repo,
        &[
            "remote",
            "add",
            "fork",
            "git@github.com:contributor/repo.git",
        ],
    );
    git(&f.repo, &["config", "branch.main.remote", "fork"]);
    git(&f.repo, &["config", "branch.main.merge", "refs/heads/main"]);
    let CheckoutSnapshotFacts::Git(facts) = get_checkout_snapshot_facts(&f.repo, &f.ctx())
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(facts.branch_remote_name.as_deref(), Some("fork"));
    assert_eq!(facts.branch_merge_ref.as_deref(), Some("refs/heads/main"));
    assert!(facts.remote_url.is_some());
}

// ---- ref-derived state --------------------------------------------------------------------------

fn git_facts(
    cwd: &str,
    current: &str,
    comparison: Option<&str>,
    ahead: u64,
    behind: u64,
) -> GitFacts {
    GitFacts {
        worktree_root: cwd.to_string(),
        current_branch: Some(current.to_string()),
        remote_url: Some("https://example.com/repo.git".into()),
        absolute_git_dir: Some(format!("{cwd}/.git")),
        git_common_dir: Some(format!("{cwd}/.git")),
        paseo_worktree: paseo_git::repo::PaseoWorktreeForCwd::NotOwned,
        stored_base_ref: None,
        resolved_base_ref: Some("main".into()),
        main_repo_root: None,
        comparison_base_ref: comparison.map(str::to_string),
        branch_remote_name: Some("origin".into()),
        branch_merge_ref: Some("refs/heads/main".into()),
        upstream_status: Some(UpstreamStatus {
            reference: "refs/remotes/origin/main".into(),
            ahead_behind: AheadBehind { ahead, behind },
        }),
    }
}

fn seed_feature_checkout() -> (Fixture, GitFacts) {
    let f = init_empty_repo("Repro");
    write(format!("{}/base.txt", f.repo), "base\n");
    git(&f.repo, &["add", "base.txt"]);
    commit(&f.repo, "base");
    let base = git_trim(&f.repo, &["rev-parse", "HEAD"]);
    git(&f.repo, &["update-ref", "refs/remotes/origin/main", &base]);
    git(&f.repo, &["switch", "--quiet", "-c", "feature"]);
    commit_file(&f.repo, "feature.txt", "feature\n", "feature");
    let facts = git_facts(&f.repo, "feature", Some("origin/main"), 0, 0);
    (f, facts)
}

fn seed_main_checkout() -> (Fixture, GitFacts) {
    let f = init_empty_repo("Repro");
    write(format!("{}/base.txt", f.repo), "base\n");
    git(&f.repo, &["add", "base.txt"]);
    commit(&f.repo, "base");
    let head = git_trim(&f.repo, &["rev-parse", "HEAD"]);
    git(&f.repo, &["update-ref", "refs/remotes/origin/main", &head]);
    commit_file(&f.repo, "local.txt", "local\n", "local");
    let facts = git_facts(&f.repo, "main", None, 1, 0);
    (f, facts)
}

fn moved(refs: &[&str]) -> HashSet<String> {
    refs.iter().map(|r| r.to_string()).collect()
}

#[tokio::test]
async fn one_comparison_refresh_updates_a_shared_base_and_upstream_ref() {
    let (f, facts) = seed_feature_checkout();
    let metrics = start_git_command_metrics();
    let result = get_checkout_ref_derived_state(
        &f.repo,
        &facts,
        &CheckoutRefDerivedState {
            ahead_behind: Some(AheadBehind {
                ahead: 0,
                behind: 0,
            }),
            diff_stat: None,
            upstream_status: None,
        },
        &moved(&["origin/main"]),
        &CheckoutContext::default(),
    )
    .await
    .unwrap();
    let mut commands: Vec<String> = metrics
        .stop()
        .into_iter()
        .filter(|c| c.cwd == f.repo)
        .map(|c| c.args[0].clone())
        .collect();
    commands.sort();
    assert_eq!(
        result.ahead_behind,
        Some(AheadBehind {
            ahead: 1,
            behind: 0
        })
    );
    assert_eq!(
        result.upstream_status,
        Some(UpstreamStatus {
            reference: "refs/remotes/origin/main".into(),
            ahead_behind: AheadBehind {
                ahead: 1,
                behind: 0
            }
        })
    );
    assert_eq!(result.diff_stat, ss(1, 0));
    assert_eq!(commands, ["diff", "ls-files", "merge-base", "rev-list"]);
}

#[tokio::test]
async fn an_upstream_move_refreshes_the_main_checkout_shortstat() {
    let (f, facts) = seed_main_checkout();
    let head = git_trim(&f.repo, &["rev-parse", "HEAD"]);
    git(&f.repo, &["update-ref", "refs/remotes/origin/main", &head]);
    let result = get_checkout_ref_derived_state(
        &f.repo,
        &facts,
        &CheckoutRefDerivedState {
            ahead_behind: None,
            diff_stat: ss(1, 0),
            upstream_status: None,
        },
        &moved(&["origin/main"]),
        &CheckoutContext::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        result,
        CheckoutRefDerivedState {
            ahead_behind: None,
            diff_stat: None,
            upstream_status: Some(UpstreamStatus {
                reference: "refs/remotes/origin/main".into(),
                ahead_behind: AheadBehind {
                    ahead: 0,
                    behind: 0
                }
            }),
        }
    );
}

#[tokio::test]
async fn an_origin_move_refreshes_an_untracked_main_checkout_shortstat() {
    let (f, mut facts) = seed_main_checkout();
    facts.branch_remote_name = None;
    facts.branch_merge_ref = None;
    facts.upstream_status = None;
    let head = git_trim(&f.repo, &["rev-parse", "HEAD"]);
    git(&f.repo, &["update-ref", "refs/remotes/origin/main", &head]);
    let result = get_checkout_ref_derived_state(
        &f.repo,
        &facts,
        &CheckoutRefDerivedState {
            ahead_behind: None,
            diff_stat: ss(1, 0),
            upstream_status: None,
        },
        &moved(&["origin/main"]),
        &CheckoutContext::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        result,
        CheckoutRefDerivedState {
            ahead_behind: None,
            diff_stat: None,
            upstream_status: None
        }
    );
}

#[tokio::test]
async fn unrelated_ref_moves_leave_derived_state_untouched() {
    let (f, facts) = seed_feature_checkout();
    let current = CheckoutRefDerivedState {
        ahead_behind: Some(AheadBehind {
            ahead: 9,
            behind: 9,
        }),
        diff_stat: ss(5, 5),
        upstream_status: facts.upstream_status.clone(),
    };
    let result = get_checkout_ref_derived_state(
        &f.repo,
        &facts,
        &current,
        &moved(&["origin/other"]),
        &CheckoutContext::default(),
    )
    .await
    .unwrap();
    assert_eq!(result, current);
}

// ---- worktrees & roots -----------------------------------------------------------------------------

#[tokio::test]
async fn handles_status_diff_commit_in_a_paseo_worktree() {
    let f = init_repo();
    let wt = create_paseo_worktree(&f, "alpha", "alpha", "main").await;
    write(format!("{wt}/file.txt"), "worktree change\n");
    let s = status(&wt, &f.ctx()).await;
    assert_eq!(real(&s.repo_root), real(&wt));
    assert!(s.is_dirty && s.is_paseo_owned_worktree);
    assert_eq!(real(s.main_repo_root.as_deref().unwrap()), real(&f.repo));
    let diff = get_checkout_diff(&wt, &CheckoutDiffCompare::uncommitted(), &f.ctx())
        .await
        .unwrap();
    assert!(diff.diff.contains("-hello") && diff.diff.contains("+worktree change"));
    commit_all(&wt, "worktree update").await.unwrap();
    assert!(!status(&wt, &f.ctx()).await.is_dirty);
    assert_eq!(
        git_trim(&wt, &["log", "-1", "--pretty=%B"]),
        "worktree update"
    );
}

#[tokio::test]
async fn returns_main_repo_root_pointing_to_the_first_non_bare_worktree_for_bare_repos() {
    let f = init_repo();
    let bare = f.path("bare-repo");
    git(&f.temp_dir, &["clone", "--bare", &f.repo, &bare]);
    let main_checkout = f.path("main-checkout");
    git(&bare, &["worktree", "add", &main_checkout, "main"]);
    set_identity(&main_checkout);
    let options = f.root_options();
    let path = paseo_git::worktree_meta::compute_worktree_path(
        &main_checkout,
        "feature-worktree",
        &options,
    )
    .await;
    std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
    git(
        &main_checkout,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            "--no-track",
            &path,
            "main",
        ],
    );
    paseo_git::worktree_meta::write_paseo_worktree_metadata(&path, "main", Some("refs/heads/main"))
        .unwrap();
    let s = status(&path, &f.ctx()).await;
    assert!(s.is_paseo_owned_worktree);
    assert_eq!(
        real(s.main_repo_root.as_deref().unwrap()),
        real(&main_checkout)
    );
    assert_eq!(
        real(&get_main_repo_root(&path, &f.ctx()).await.unwrap()),
        real(&main_checkout)
    );
}

#[tokio::test]
async fn detects_plain_git_worktrees_from_git_alone() {
    let f = init_repo();
    let wt = f.path("plain-git-worktree");
    git(
        &f.repo,
        &["worktree", "add", "-b", "feature/plain", &wt, "main"],
    );
    let s = status(&wt, &f.ctx()).await;
    assert_eq!(real(&s.repo_root), real(&wt));
    assert!(!s.is_paseo_owned_worktree);
    assert_eq!(real(s.main_repo_root.as_deref().unwrap()), real(&f.repo));
    assert_eq!(s.current_branch.as_deref(), Some("feature/plain"));
}

#[tokio::test]
async fn falls_back_to_plain_git_status_when_paseo_worktree_metadata_is_missing() {
    let f = init_repo();
    let wt = create_paseo_worktree(&f, "missing-metadata-status-fallback", "feature", "main").await;
    std::fs::remove_file(paseo_git::worktree_meta::paseo_worktree_metadata_path(&wt).unwrap())
        .unwrap();
    let s = status(&wt, &f.ctx()).await;
    assert_eq!(s.current_branch.as_deref(), Some("feature"));
    assert_eq!(real(&s.repo_root), real(&wt));
    assert!(s.is_paseo_owned_worktree);
    assert_eq!(real(s.main_repo_root.as_deref().unwrap()), real(&f.repo));
    assert_eq!(s.base_ref.as_deref(), Some("main"));
}

#[tokio::test]
async fn uses_stored_base_ref_name_for_paseo_worktree_status() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "develop"]);
    commit_file(&f.repo, "file.txt", "develop\n", "develop change");
    git(&f.repo, &["checkout", "main"]);
    let wt = create_paseo_worktree(&f, "feature", "feature", "develop").await;
    commit_file(&wt, "feature.txt", "feature\n", "feature commit");
    let s = status(&wt, &f.ctx()).await;
    assert_eq!(s.base_ref.as_deref(), Some("develop"));
    assert_eq!(s.ahead_behind.map(|a| a.ahead), Some(1));
}

#[tokio::test]
async fn resolves_the_repository_default_branch_from_origin_head() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "develop"]);
    git(&f.repo, &["checkout", "main"]);
    git(
        &f.repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/repo.git",
        ],
    );
    git(
        &f.repo,
        &["update-ref", "refs/remotes/origin/main", "refs/heads/main"],
    );
    git(
        &f.repo,
        &[
            "update-ref",
            "refs/remotes/origin/develop",
            "refs/heads/develop",
        ],
    );
    git(
        &f.repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );
    assert_eq!(
        resolve_repository_default_branch(&f.repo)
            .await
            .unwrap()
            .as_deref(),
        Some("main")
    );
}

#[tokio::test]
async fn default_branch_falls_back_to_main_master_then_none() {
    let (_t, dir) = temp_dir();
    let repo = format!("{dir}/r");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "trunk"]);
    set_identity(&repo);
    commit_file(&repo, "a.txt", "a\n", "init");
    assert_eq!(
        resolve_repository_default_branch(&repo).await.unwrap(),
        None
    );
    git(&repo, &["branch", "master"]);
    assert_eq!(
        resolve_repository_default_branch(&repo)
            .await
            .unwrap()
            .as_deref(),
        Some("master")
    );
    git(&repo, &["branch", "main"]);
    assert_eq!(
        resolve_repository_default_branch(&repo)
            .await
            .unwrap()
            .as_deref(),
        Some("main")
    );
}

#[tokio::test]
async fn status_wire_payload_for_a_feature_branch() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "feature.txt", "feature\n", "feature");
    let s = get_checkout_status(&f.repo, &CheckoutContext::default())
        .await
        .unwrap();
    let payload = paseo_git::status::build_checkout_status_payload(&f.repo, "req", &s).unwrap();
    assert_eq!(
        payload,
        json!({
            "cwd": f.repo, "isGit": true, "repoRoot": real(&f.repo), "mainRepoRoot": null,
            "currentBranch": "feature", "isDirty": false, "baseRef": "main",
            "aheadBehind": {"ahead": 1, "behind": 0}, "upstreamRef": null,
            "aheadOfOrigin": null, "behindOfOrigin": null, "hasRemote": false,
            "remoteUrl": null, "isPaseoOwnedWorktree": false, "error": null, "requestId": "req",
        })
    );
}

// ---- discard ------------------------------------------------------------------------------------------

#[tokio::test]
async fn discards_staged_and_unstaged_modifications_deletions_and_untracked_files() {
    let f = init_repo();
    write(format!("{}/file.txt", f.repo), "changed\n");
    write(format!("{}/staged.txt", f.repo), "staged\n");
    git(&f.repo, &["add", "staged.txt"]);
    write(format!("{}/junk/scratch.txt", f.repo), "scratch\n");
    discard_changes(
        &f.repo,
        &["file.txt".into(), "staged.txt".into(), "junk".into()],
    )
    .await
    .unwrap();
    assert_eq!(read(format!("{}/file.txt", f.repo)), "hello\n");
    assert!(!exists(format!("{}/staged.txt", f.repo)));
    assert!(!exists(format!("{}/junk", f.repo)));
    assert_eq!(git_trim(&f.repo, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn restores_both_sides_of_a_staged_rename() {
    let f = init_repo();
    git(&f.repo, &["mv", "file.txt", "renamed.txt"]);
    discard_changes(&f.repo, &["file.txt".into(), "renamed.txt".into()])
        .await
        .unwrap();
    assert_eq!(read(format!("{}/file.txt", f.repo)), "hello\n");
    assert!(!exists(format!("{}/renamed.txt", f.repo)));
    assert_eq!(git(&f.repo, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn unstages_and_removes_files_in_a_repository_with_an_unborn_head() {
    let f = init_repo();
    let repo = f.path("unborn-discard");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    write(format!("{repo}/new.txt"), "new\n");
    git(&repo, &["add", "new.txt"]);
    discard_changes(&repo, &["new.txt".into()]).await.unwrap();
    assert!(!exists(format!("{repo}/new.txt")));
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn discards_a_nested_folder_pathspec_without_touching_sibling_changes() {
    let f = init_repo();
    commit_file(&f.repo, "nested/tracked.txt", "original\n", "add nested");
    write(format!("{}/nested/tracked.txt", f.repo), "changed\n");
    write(format!("{}/nested/untracked.txt", f.repo), "remove\n");
    write(format!("{}/outside.txt", f.repo), "keep\n");
    discard_changes(&f.repo, &["nested".into()]).await.unwrap();
    assert_eq!(read(format!("{}/nested/tracked.txt", f.repo)), "original\n");
    assert!(!exists(format!("{}/nested/untracked.txt", f.repo)));
    assert_eq!(read(format!("{}/outside.txt", f.repo)), "keep\n");
    assert_eq!(git(&f.repo, &["status", "--porcelain"]), "?? outside.txt\n");
}

#[tokio::test]
async fn restores_a_staged_deletion() {
    let f = init_repo();
    git(&f.repo, &["rm", "file.txt"]);
    discard_changes(&f.repo, &["file.txt".into()])
        .await
        .unwrap();
    assert_eq!(read(format!("{}/file.txt", f.repo)), "hello\n");
}

#[tokio::test]
async fn leaves_files_outside_the_given_pathspecs_untouched() {
    let f = init_repo();
    write(format!("{}/file.txt", f.repo), "changed\n");
    write(format!("{}/other.txt", f.repo), "keep\n");
    discard_changes(&f.repo, &["file.txt".into()])
        .await
        .unwrap();
    assert_eq!(read(format!("{}/file.txt", f.repo)), "hello\n");
    assert_eq!(read(format!("{}/other.txt", f.repo)), "keep\n");
}

#[tokio::test]
async fn treats_requested_filenames_as_literal_pathspecs() {
    let f = init_repo();
    write(format!("{}/foo[ab].txt", f.repo), "literal original\n");
    write(format!("{}/fooa.txt", f.repo), "sibling original\n");
    git(&f.repo, &["add", "foo[ab].txt", "fooa.txt"]);
    commit(&f.repo, "add pathspec files");
    write(format!("{}/foo[ab].txt", f.repo), "literal changed\n");
    write(format!("{}/fooa.txt", f.repo), "sibling changed\n");
    discard_changes(&f.repo, &["foo[ab].txt".into()])
        .await
        .unwrap();
    assert_eq!(
        read(format!("{}/foo[ab].txt", f.repo)),
        "literal original\n"
    );
    assert_eq!(read(format!("{}/fooa.txt", f.repo)), "sibling changed\n");
}
