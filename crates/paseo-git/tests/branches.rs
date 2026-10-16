mod common;

use common::*;
use paseo_git::branches::*;
use paseo_git::error::GitError;
use serde_json::json;

fn names(branches: &[BranchSuggestion]) -> Vec<&str> {
    branches.iter().map(|b| b.name.as_str()).collect()
}

async fn remote_with_branches(f: &Fixture) -> (String, String) {
    let remote = add_bare_remote(f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    git(&f.repo, &["checkout", "-b", "feature/local-only"]);
    git(&f.repo, &["checkout", "main"]);
    git(&f.repo, &["checkout", "-b", "feature/shared"]);
    commit_file(&f.repo, "shared.txt", "shared\n", "shared branch");
    git(&f.repo, &["push", "-u", "origin", "feature/shared"]);
    git(&f.repo, &["checkout", "main"]);
    let other = clone_remote(f, &remote, "other-clone");
    git(&other, &["checkout", "-b", "feature/remote-only"]);
    commit_file(
        &other,
        "remote-only.txt",
        "remote-only\n",
        "remote only branch",
    );
    git(&other, &["push", "-u", "origin", "feature/remote-only"]);
    git(&f.repo, &["fetch", "origin"]);
    (remote, other)
}

#[tokio::test]
async fn lists_merged_local_and_remote_branch_suggestions_with_provenance() {
    let f = init_repo();
    remote_with_branches(&f).await;
    let branches = list_branch_suggestions(&f.repo, None, Some(50))
        .await
        .unwrap();
    let n = names(&branches);
    for expected in [
        "main",
        "feature/local-only",
        "feature/remote-only",
        "feature/shared",
    ] {
        assert!(n.contains(&expected), "{expected} in {n:?}");
    }
    assert_eq!(n.iter().filter(|x| **x == "main").count(), 1);
    assert!(!n.contains(&"HEAD"));
    assert!(!n.iter().any(|x| x.starts_with("origin/")));
    let find = |name: &str| branches.iter().find(|b| b.name == name).unwrap();
    assert_eq!(
        (
            find("feature/local-only").has_local,
            find("feature/local-only").has_remote
        ),
        (true, false)
    );
    assert_eq!(
        (
            find("feature/remote-only").has_local,
            find("feature/remote-only").has_remote
        ),
        (false, true)
    );
    let shared = find("feature/shared");
    assert_eq!(
        (
            shared.has_local,
            shared.has_remote,
            shared.local_ahead,
            shared.local_behind
        ),
        (true, true, Some(0), Some(0))
    );
    let filtered = list_branch_suggestions(&f.repo, Some("origin/main"), None)
        .await
        .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(
        (
            filtered[0].name.as_str(),
            filtered[0].has_local,
            filtered[0].has_remote
        ),
        ("main", true, true)
    );
}

#[tokio::test]
async fn reports_local_and_origin_divergence_for_branch_suggestions() {
    let f = init_repo();
    let remote = add_bare_remote(&f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    commit_file(&f.repo, "local.txt", "local\n", "local commit");
    let other = clone_remote(&f, &remote, "diverged-clone");
    commit_file(&other, "remote.txt", "remote\n", "remote commit");
    git(&other, &["push", "origin", "main"]);
    git(&f.repo, &["fetch", "origin"]);
    let branches = list_branch_suggestions(&f.repo, None, Some(50))
        .await
        .unwrap();
    let main = branches.iter().find(|b| b.name == "main").unwrap();
    assert_eq!(
        (
            main.has_local,
            main.has_remote,
            main.local_ahead,
            main.local_behind
        ),
        (true, true, Some(1), Some(1))
    );
}

#[tokio::test]
async fn filters_branch_suggestions_by_query_and_enforces_result_limit() {
    let f = init_repo();
    for b in ["feature/alpha", "feature/beta", "chore/docs"] {
        git(&f.repo, &["checkout", "-b", b]);
        git(&f.repo, &["checkout", "main"]);
    }
    let branches = list_branch_suggestions(&f.repo, Some("FEATURE/"), Some(1))
        .await
        .unwrap();
    assert_eq!(branches.len(), 1);
    assert!(branches[0].name.to_lowercase().contains("feature/"));
}

#[tokio::test]
async fn orders_prefix_matches_first_then_newest_commit_date() {
    let f = init_repo();
    let dated = |date: &'static str| [("GIT_COMMITTER_DATE", date), ("GIT_AUTHOR_DATE", date)];
    git(&f.repo, &["checkout", "-b", "zz-feature"]);
    git_env(
        &f.repo,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "old",
        ],
        &dated("2020-01-01T00:00:00Z"),
    );
    git(&f.repo, &["checkout", "main"]);
    git(&f.repo, &["checkout", "-b", "feature-new"]);
    git_env(
        &f.repo,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "new",
        ],
        &dated("2024-01-01T00:00:00Z"),
    );
    git(&f.repo, &["checkout", "main"]);
    let all = list_branch_suggestions(&f.repo, None, None).await.unwrap();
    assert_eq!(names(&all), ["main", "feature-new", "zz-feature"]);
    let prefixed = list_branch_suggestions(&f.repo, Some("feature"), None)
        .await
        .unwrap();
    assert_eq!(names(&prefixed), ["feature-new", "zz-feature"]);
    let reversed_by_prefix = list_branch_suggestions(&f.repo, Some("zz"), None)
        .await
        .unwrap();
    assert_eq!(names(&reversed_by_prefix), ["zz-feature"]);
}

#[tokio::test]
async fn non_git_directory_rejects_branch_suggestions() {
    let f = init_repo();
    let dir = f.path("plain");
    std::fs::create_dir_all(&dir).unwrap();
    let err = list_branch_suggestions(&dir, None, None).await.unwrap_err();
    assert!(matches!(err, GitError::NotGitRepo { .. }));
}

#[tokio::test]
async fn resolves_branch_checkout_targets_with_local_precedence_and_origin_normalization() {
    let f = init_repo();
    remote_with_branches(&f).await;
    let r = |n: &str| {
        let repo = f.repo.clone();
        let n = n.to_string();
        async move { resolve_branch_checkout(&repo, &n).await.unwrap() }
    };
    assert_eq!(
        r("feature/local-only").await,
        BranchCheckoutResolution::Local {
            name: "feature/local-only".into()
        }
    );
    let remote_only = BranchCheckoutResolution::RemoteOnly {
        name: "feature/remote-only".into(),
        remote_ref: "origin/feature/remote-only".into(),
    };
    assert_eq!(r("feature/remote-only").await, remote_only);
    assert_eq!(r("origin/feature/remote-only").await, remote_only);
    assert_eq!(
        r("feature/shared").await,
        BranchCheckoutResolution::Local {
            name: "feature/shared".into()
        }
    );
    assert_eq!(
        r("feature/unknown").await,
        BranchCheckoutResolution::NotFound
    );
    assert_eq!(r("").await, BranchCheckoutResolution::NotFound);
}

#[tokio::test]
async fn does_not_resolve_tags_as_branch_checkout_targets() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature/a"]);
    git(&f.repo, &["checkout", "main"]);
    git(&f.repo, &["tag", "v1"]);
    assert_eq!(
        resolve_branch_checkout(&f.repo, "v1").await.unwrap(),
        BranchCheckoutResolution::NotFound
    );
}

async fn checkout_remote_only(input: &str) {
    let f = init_repo();
    let remote = add_bare_remote(&f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    let other = clone_remote(&f, &remote, "other-clone");
    git(&other, &["checkout", "-b", "feature/remote-only"]);
    commit_file(
        &other,
        "remote-only.txt",
        "remote-only\n",
        "remote only branch",
    );
    git(&other, &["push", "-u", "origin", "feature/remote-only"]);
    git(&f.repo, &["fetch", "origin"]);
    let resolution = resolve_branch_checkout(&f.repo, input).await.unwrap();
    assert_eq!(
        checkout_resolved_branch(&f.repo, &resolution, None)
            .await
            .unwrap(),
        BranchCheckoutSource::Remote
    );
    assert_eq!(
        git_trim(&f.repo, &["symbolic-ref", "--short", "HEAD"]),
        "feature/remote-only"
    );
    assert_eq!(
        git_trim(
            &f.repo,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]
        ),
        "origin/feature/remote-only"
    );
}

#[tokio::test]
async fn checks_out_a_remote_only_branch_as_a_local_tracking_branch() {
    checkout_remote_only("feature/remote-only").await;
}

#[tokio::test]
async fn normalizes_explicit_origin_input_when_checking_out_a_remote_only_branch() {
    checkout_remote_only("origin/feature/remote-only").await;
}

#[tokio::test]
async fn checks_out_the_local_branch_when_local_and_remote_branches_share_a_name() {
    let f = init_repo();
    add_bare_remote(&f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    git(&f.repo, &["checkout", "-b", "feature/shared"]);
    commit_file(&f.repo, "shared.txt", "shared\n", "shared branch");
    git(&f.repo, &["push", "-u", "origin", "feature/shared"]);
    git(&f.repo, &["checkout", "main"]);
    let resolution = resolve_branch_checkout(&f.repo, "feature/shared")
        .await
        .unwrap();
    assert_eq!(
        checkout_resolved_branch(&f.repo, &resolution, None)
            .await
            .unwrap(),
        BranchCheckoutSource::Local
    );
    assert_eq!(
        git_trim(&f.repo, &["symbolic-ref", "--short", "HEAD"]),
        "feature/shared"
    );
}

#[tokio::test]
async fn throws_the_existing_branch_not_found_message_for_unknown_checkout_targets() {
    let f = init_repo();
    let err = checkout_resolved_branch(
        &f.repo,
        &BranchCheckoutResolution::NotFound,
        Some("missing-branch"),
    )
    .await
    .unwrap_err();
    assert_eq!(err.to_string(), "Branch not found: missing-branch");
    let err = checkout_resolved_branch(&f.repo, &BranchCheckoutResolution::NotFound, None)
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "Branch not found: unknown");
}

#[tokio::test]
async fn renames_the_checked_out_branch_and_returns_concrete_branch_names() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature/old-name"]);
    let r = rename_current_branch(&f.repo, "feature/new-name")
        .await
        .unwrap();
    assert_eq!(
        git_trim(&f.repo, &["branch", "--show-current"]),
        "feature/new-name"
    );
    assert_eq!(
        (r.previous_branch.as_deref(), r.current_branch.as_deref()),
        (Some("feature/old-name"), Some("feature/new-name"))
    );
    assert!(!git_succeeds(
        &f.repo,
        &["show-ref", "--verify", "refs/heads/feature/old-name"]
    ));
    assert!(git_succeeds(
        &f.repo,
        &["show-ref", "--verify", "refs/heads/feature/new-name"]
    ));
}

#[tokio::test]
async fn fails_when_renaming_the_checked_out_branch_to_an_existing_branch() {
    let f = init_repo();
    git(&f.repo, &["branch", "feature/new-name"]);
    git(&f.repo, &["checkout", "-b", "feature/old-name"]);
    assert!(rename_current_branch(&f.repo, "feature/new-name")
        .await
        .is_err());
    assert_eq!(
        git_trim(&f.repo, &["branch", "--show-current"]),
        "feature/old-name"
    );
    assert!(git_succeeds(
        &f.repo,
        &["show-ref", "--verify", "refs/heads/feature/old-name"]
    ));
    assert!(git_succeeds(
        &f.repo,
        &["show-ref", "--verify", "refs/heads/feature/new-name"]
    ));
}

#[tokio::test]
async fn refuses_to_rename_a_detached_head() {
    let f = init_repo();
    git(&f.repo, &["checkout", "--detach"]);
    let err = rename_current_branch(&f.repo, "x").await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "Cannot rename branch in detached HEAD state"
    );
}

#[tokio::test]
async fn rename_moves_a_managed_branch_identity_pin() {
    let f = init_repo();
    let wt = create_paseo_worktree(&f, "managed", "feature/old", "main").await;
    let meta_path = paseo_git::worktree_meta::paseo_worktree_metadata_path(&wt).unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    doc["changeRequestLookupTarget"] =
        json!({"headRef": "feature/old", "localBranchName": "feature/old"});
    std::fs::write(&meta_path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();

    rename_current_branch(&wt, "feature/new").await.unwrap();
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert_eq!(
        after["changeRequestLookupTarget"],
        json!({"headRef": "feature/new", "localBranchName": "feature/new"})
    );
    assert_eq!(after["baseRefName"], "main");
}

#[tokio::test]
async fn rename_keeps_a_fork_identity_and_only_rebinds_the_local_name() {
    let f = init_repo();
    let wt = create_paseo_worktree(&f, "fork", "feature/old", "main").await;
    let meta_path = paseo_git::worktree_meta::paseo_worktree_metadata_path(&wt).unwrap();
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    doc["changeRequestLookupTarget"] = json!({"headRef": "their-branch", "headRepositoryOwner": "someone", "changeRequestNumber": 42, "localBranchName": "feature/old"});
    std::fs::write(&meta_path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    rename_current_branch(&wt, "feature/new").await.unwrap();
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert_eq!(
        after["changeRequestLookupTarget"]["headRef"],
        "their-branch"
    );
    assert_eq!(
        after["changeRequestLookupTarget"]["localBranchName"],
        "feature/new"
    );
}

#[tokio::test]
async fn checkout_existing_branch_validates_and_requires_a_clean_tree() {
    let f = init_repo();
    git(&f.repo, &["branch", "other"]);
    assert_eq!(
        checkout_existing_branch(&f.repo, "other").await.unwrap(),
        BranchCheckoutSource::Local
    );
    assert_eq!(git_trim(&f.repo, &["branch", "--show-current"]), "other");
    assert_eq!(
        checkout_existing_branch(&f.repo, "nope")
            .await
            .unwrap_err()
            .to_string(),
        "Branch not found: nope"
    );
    assert_eq!(
        checkout_existing_branch(&f.repo, "bad name")
            .await
            .unwrap_err()
            .to_string(),
        "Invalid branch: bad name"
    );
    write(format!("{}/file.txt", f.repo), "dirty\n");
    let err = checkout_existing_branch(&f.repo, "main").await.unwrap_err();
    assert_eq!(
        err.to_string(),
        "Working directory has uncommitted changes. Commit or stash before switching branches."
    );
}

#[tokio::test]
async fn checkout_new_branch_rejects_existing_names() {
    let f = init_repo();
    assert_eq!(
        checkout_new_branch(&f.repo, "feature/x").await.unwrap(),
        BranchCheckoutSource::Local
    );
    assert_eq!(
        git_trim(&f.repo, &["branch", "--show-current"]),
        "feature/x"
    );
    assert_eq!(
        checkout_new_branch(&f.repo, "main")
            .await
            .unwrap_err()
            .to_string(),
        "Branch already exists: main"
    );
    assert_eq!(
        checkout_new_branch(&f.repo, "a..b")
            .await
            .unwrap_err()
            .to_string(),
        "Invalid new branch: a..b"
    );
}

#[tokio::test]
async fn create_branch_from_base_does_not_inherit_the_base_upstream() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    git(&f.repo, &["config", "branch.autoSetupMerge", "always"]);
    create_branch_from_base(&f.repo, "origin/main", "feature/from-base")
        .await
        .unwrap();
    assert_eq!(
        git_trim(&f.repo, &["branch", "--show-current"]),
        "feature/from-base"
    );
    assert!(!git_succeeds(
        &f.repo,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"]
    ));
    let err = create_branch_from_base(&f.repo, "missing", "feature/y")
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "Base branch not found: missing");
}

#[tokio::test]
async fn branch_wire_payloads() {
    let f = init_repo();
    git(&f.repo, &["branch", "other"]);
    let suggestions = list_branch_suggestions(&f.repo, None, None).await;
    let v = build_branch_suggestions_payload("r1", &suggestions);
    assert_eq!(v["branches"].as_array().unwrap().len(), 2);
    assert!(v["branchDetails"][0].get("committerDate").is_some());
    assert!(v["branchDetails"][0].get("localAhead").is_none());
    assert_eq!(v["error"], serde_json::Value::Null);
    let failed = build_branch_suggestions_payload("r1", &Err(GitError::other("boom")));
    assert_eq!(
        failed,
        json!({"branches": [], "branchDetails": [], "error": "boom", "requestId": "r1"})
    );

    let local =
        build_validate_branch_payload("r", &resolve_branch_checkout(&f.repo, "other").await);
    assert_eq!(
        local,
        json!({"exists": true, "resolvedRef": "other", "isRemote": false, "error": null, "requestId": "r"})
    );
    let missing =
        build_validate_branch_payload("r", &resolve_branch_checkout(&f.repo, "zzz").await);
    assert_eq!(
        missing,
        json!({"exists": false, "resolvedRef": null, "isRemote": false, "error": null, "requestId": "r"})
    );

    let switched =
        build_switch_branch_payload("/c", "other", "r", &Ok(BranchCheckoutSource::Remote));
    assert_eq!(
        switched,
        json!({"cwd": "/c", "success": true, "branch": "other", "source": "remote", "error": null, "requestId": "r"})
    );
    let failed = build_switch_branch_payload("/c", "other", "r", &Err(GitError::other("nope")));
    assert_eq!(
        failed,
        json!({"cwd": "/c", "success": false, "branch": "other", "error": {"code": "UNKNOWN", "message": "nope"}, "requestId": "r"})
    );
    let renamed = build_rename_branch_payload(
        "/c",
        "r",
        &Ok(RenameResult {
            previous_branch: Some("a".into()),
            current_branch: Some("b".into()),
        }),
    );
    assert_eq!(
        renamed,
        json!({"cwd": "/c", "success": true, "currentBranch": "b", "error": null, "requestId": "r"})
    );
}
