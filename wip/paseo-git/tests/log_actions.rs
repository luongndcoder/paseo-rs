mod common;

use common::*;
use paseo_git::log_actions::*;
use serde_json::json;

fn action(kind: &str, target: &str) -> GitLogAction {
    serde_json::from_value(json!({"kind": kind, "target": target})).unwrap()
}

fn action_with(kind: &str, target: &str, extra: serde_json::Value) -> GitLogAction {
    let mut v = json!({"kind": kind, "target": target});
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::from_value(v).unwrap()
}

async fn run(repo: &str, a: GitLogAction) -> ActionResult {
    execute_git_log_action(repo, &a).await.unwrap()
}

fn subjects(repo: &str) -> Vec<String> {
    git_trim(repo, &["log", "--format=%s"])
        .lines()
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn cherry_picks_and_reverts_commits() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "feature.txt", "f\n", "feature commit");
    let sha = git_trim(&f.repo, &["rev-parse", "HEAD"]);
    git(&f.repo, &["checkout", "main"]);
    let r = run(&f.repo, action("cherry-pick", &sha)).await;
    assert_eq!(
        r,
        ActionResult {
            success: true,
            message: format!("Cherry-picked {}", &sha[..7])
        }
    );
    assert_eq!(subjects(&f.repo)[0], "feature commit");
    let head = git_trim(&f.repo, &["rev-parse", "HEAD"]);
    let r = run(&f.repo, action("revert", &head)).await;
    assert_eq!(r.message, format!("Reverted {}", &head[..7]));
    assert!(subjects(&f.repo)[0].starts_with("Revert"));
    assert!(!exists(format!("{}/feature.txt", f.repo)));
}

#[tokio::test]
async fn resets_in_each_mode() {
    let f = init_repo();
    commit_file(&f.repo, "a.txt", "a\n", "add a");
    let initial = git_trim(&f.repo, &["rev-parse", "HEAD~1"]);
    let r = run(
        &f.repo,
        action_with("reset", &initial, json!({"mode": "soft"})),
    )
    .await;
    assert_eq!(r.message, format!("Reset (soft) to {}", &initial[..7]));
    assert_eq!(git_trim(&f.repo, &["status", "--porcelain"]), "A  a.txt");
    run(
        &f.repo,
        action_with("reset", &initial, json!({"mode": "mixed"})),
    )
    .await;
    assert_eq!(git_trim(&f.repo, &["status", "--porcelain"]), "?? a.txt");
    let default = run(&f.repo, action("reset", &initial)).await;
    assert!(default.message.starts_with("Reset (mixed)"));
    git(&f.repo, &["add", "a.txt"]);
    run(
        &f.repo,
        action_with("reset", &initial, json!({"mode": "hard"})),
    )
    .await;
    assert!(!exists(format!("{}/a.txt", f.repo)));
}

#[tokio::test]
async fn creates_and_deletes_branches_and_tags() {
    let f = init_repo();
    assert_eq!(
        run(&f.repo, action("create-branch", "topic")).await.message,
        "Created and checked out branch topic"
    );
    assert_eq!(git_trim(&f.repo, &["branch", "--show-current"]), "topic");
    git(&f.repo, &["checkout", "main"]);
    run(
        &f.repo,
        action_with(
            "create-branch",
            "from-start",
            json!({"secondTarget": "topic"}),
        ),
    )
    .await;
    git(&f.repo, &["checkout", "main"]);
    assert_eq!(
        run(&f.repo, action("delete-branch", "from-start"))
            .await
            .message,
        "Deleted branch from-start"
    );
    assert!(!git_succeeds(
        &f.repo,
        &["rev-parse", "--verify", "refs/heads/from-start"]
    ));
    // unmerged branches need force
    git(&f.repo, &["checkout", "topic"]);
    commit_file(&f.repo, "t.txt", "t\n", "topic work");
    git(&f.repo, &["checkout", "main"]);
    assert!(
        execute_git_log_action(&f.repo, &action("delete-branch", "topic"))
            .await
            .is_err()
    );
    run(
        &f.repo,
        action_with("delete-branch", "topic", json!({"force": true})),
    )
    .await;

    assert_eq!(
        run(&f.repo, action("create-tag", "v1")).await.message,
        "Created tag v1"
    );
    run(
        &f.repo,
        action_with("create-tag", "v2", json!({"secondTarget": "HEAD"})),
    )
    .await;
    assert_eq!(
        run(&f.repo, action("delete-tag", "v1")).await.message,
        "Deleted tag v1"
    );
    assert_eq!(git_trim(&f.repo, &["tag"]), "v2");
}

#[tokio::test]
async fn checks_out_refs_including_remote_tracking_branches() {
    let f = init_repo();
    let (remote, _) = setup_remote_tracking_main(&f);
    let other = clone_remote(&f, &remote, "other");
    git(&other, &["checkout", "-b", "remote-topic"]);
    commit_file(&other, "r.txt", "r\n", "remote topic");
    git(&other, &["push", "-u", "origin", "remote-topic"]);
    git(&f.repo, &["fetch", "origin"]);
    let r = run(&f.repo, action("checkout-ref", "origin/remote-topic")).await;
    assert_eq!(
        r.message,
        "Checked out remote-topic (tracking origin/remote-topic)"
    );
    assert_eq!(
        git_trim(&f.repo, &["branch", "--show-current"]),
        "remote-topic"
    );
    git(&f.repo, &["checkout", "main"]);
    let again = run(&f.repo, action("checkout-ref", "origin/remote-topic")).await;
    assert_eq!(again.message, "Checked out remote-topic");
    let plain = run(&f.repo, action("checkout-ref", "main")).await;
    assert_eq!(plain.message, "Checked out main");
}

#[tokio::test]
async fn smart_checkout_auto_stashes_a_dirty_tree_and_restores_it() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "other"]);
    commit_file(&f.repo, "file.txt", "other version\n", "other edit");
    git(&f.repo, &["checkout", "main"]);
    write(format!("{}/file.txt", f.repo), "dirty local\n");
    let r = run(&f.repo, action("checkout-ref", "other")).await;
    assert!(r.success);
    // The checkout conflicts with the local edit: the stash pop reports it or restores it.
    assert!(
        r.message.starts_with("Smart Checkout") || r.message.starts_with("CONFLICT"),
        "{}",
        r.message
    );
    assert_eq!(git_trim(&f.repo, &["branch", "--show-current"]), "other");
}

#[tokio::test]
async fn merges_and_rebases_with_conflict_reporting() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "feature.txt", "f\n", "feature commit");
    git(&f.repo, &["checkout", "main"]);
    let r = run(&f.repo, action("merge-ref", "feature")).await;
    assert_eq!(r.message, "Merged feature into current branch");
    git(&f.repo, &["checkout", "feature"]);
    commit_file(&f.repo, "conflict.txt", "feature\n", "feature conflict");
    git(&f.repo, &["checkout", "main"]);
    commit_file(&f.repo, "conflict.txt", "main\n", "main conflict");
    let conflicted = run(&f.repo, action("merge-ref", "feature")).await;
    assert!(conflicted.success);
    assert!(
        conflicted
            .message
            .starts_with("CONFLICT: Merge of feature resulted in 1 conflict(s) (conflict.txt)"),
        "{}",
        conflicted.message
    );
    assert_eq!(
        run(&f.repo, action("abort-operation", "")).await.message,
        "Aborted merge"
    );
    assert_eq!(git_trim(&f.repo, &["status", "--porcelain"]), "");

    git(&f.repo, &["checkout", "feature"]);
    let rebase = run(&f.repo, action("rebase-onto", "main")).await;
    assert!(
        rebase.message.starts_with("CONFLICT: Rebase of main"),
        "{}",
        rebase.message
    );
    assert_eq!(
        run(&f.repo, action("abort-operation", "")).await.message,
        "Aborted rebase"
    );
    assert!(
        execute_git_log_action(&f.repo, &action("abort-operation", ""))
            .await
            .unwrap_err()
            .to_string()
            .contains("No active merge, rebase, or cherry-pick operation to abort")
    );
}

#[tokio::test]
async fn resolves_conflicts_and_continues_the_operation() {
    let f = init_repo();
    commit_file(&f.repo, "conflict.txt", "base\n", "base");
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "conflict.txt", "feature\n", "feature");
    git(&f.repo, &["checkout", "main"]);
    commit_file(&f.repo, "conflict.txt", "main\n", "main");
    assert!(!git_succeeds(&f.repo, &["merge", "feature"]));
    let r = run(
        &f.repo,
        action_with(
            "resolve-conflict",
            "conflict.txt",
            json!({"content": "resolved\n"}),
        ),
    )
    .await;
    assert_eq!(r.message, "Resolved conflict in conflict.txt");
    assert_eq!(
        run(&f.repo, action("continue-operation", "")).await.message,
        "Continued merge"
    );
    assert_eq!(read(format!("{}/conflict.txt", f.repo)), "resolved\n");
    assert!(execute_git_log_action(
        &f.repo,
        &action_with(
            "resolve-conflict",
            "../outside.txt",
            json!({"content": "x"})
        )
    )
    .await
    .is_err());
    assert!(
        execute_git_log_action(&f.repo, &action("continue-operation", ""))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn rewords_the_head_commit_and_an_older_commit() {
    let f = init_repo();
    commit_file(&f.repo, "a.txt", "a\n", "first");
    commit_file(&f.repo, "b.txt", "b\n", "second");
    commit_file(&f.repo, "c.txt", "c\n", "third");
    let head = git_trim(&f.repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        run(
            &f.repo,
            action_with("reword-commit", &head, json!({"message": "third reworded"}))
        )
        .await
        .message,
        format!("Reworded commit {}", &head[..7])
    );
    let second = git_trim(&f.repo, &["rev-parse", "HEAD~1"]);
    run(
        &f.repo,
        action_with(
            "reword-commit",
            &second,
            json!({"message": "second reworded"}),
        ),
    )
    .await;
    assert_eq!(
        subjects(&f.repo)[..3],
        ["third reworded", "second reworded", "first"]
    );
    // Message is required.
    let err = execute_git_log_action(&f.repo, &action("reword-commit", &head))
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Commit message is required for reword-commit"
    );
    // Rewording the root commit goes through --root.
    let root = git_trim(&f.repo, &["rev-list", "--max-parents=0", "HEAD"]);
    run(
        &f.repo,
        action_with("reword-commit", &root, json!({"message": "root reworded"})),
    )
    .await;
    assert_eq!(subjects(&f.repo).last().unwrap(), "root reworded");
    assert_eq!(git_trim(&f.repo, &["status", "--porcelain"]), "");
}

#[tokio::test]
async fn squashes_a_commit_into_its_parent() {
    let f = init_repo();
    commit_file(&f.repo, "a.txt", "a\n", "first");
    commit_file(&f.repo, "b.txt", "b\n", "second");
    commit_file(&f.repo, "c.txt", "c\n", "third");
    commit_file(&f.repo, "d.txt", "d\n", "fourth");
    let second = git_trim(&f.repo, &["rev-parse", "HEAD~2"]);
    let r = run(&f.repo, action("squash-commit", &second)).await;
    assert_eq!(
        r.message,
        format!("Squashed commit {} into parent", &second[..7])
    );
    assert_eq!(subjects(&f.repo)[..3], ["fourth", "third", "first"]);
    assert!(exists(format!("{}/b.txt", f.repo)));

    let third = git_trim(&f.repo, &["rev-parse", "HEAD~1"]);
    run(
        &f.repo,
        action_with("squash-commit", &third, json!({"message": "combined"})),
    )
    .await;
    assert_eq!(subjects(&f.repo)[..2], ["fourth", "combined"]);

    let root = git_trim(&f.repo, &["rev-list", "--max-parents=0", "HEAD"]);
    let err = execute_git_log_action(&f.repo, &action("squash-commit", &root))
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Cannot squash root commit without a parent"
    );
}

#[tokio::test]
async fn drops_a_commit() {
    let f = init_repo();
    commit_file(&f.repo, "a.txt", "a\n", "keep");
    commit_file(&f.repo, "b.txt", "b\n", "drop me");
    commit_file(&f.repo, "c.txt", "c\n", "also keep");
    let target = git_trim(&f.repo, &["rev-parse", "HEAD~1"]);
    let r = run(&f.repo, action("drop-commit", &target)).await;
    assert_eq!(r.message, format!("Dropped commit {}", &target[..7]));
    assert_eq!(subjects(&f.repo)[..3], ["also keep", "keep", "initial"]);
    assert!(!exists(format!("{}/b.txt", f.repo)));
}

#[tokio::test]
async fn stash_actions_round_trip() {
    let f = init_repo();
    write(format!("{}/file.txt", f.repo), "wip\n");
    write(format!("{}/untracked.txt", f.repo), "u\n");
    assert_eq!(
        run(
            &f.repo,
            action_with("stash-create", "", json!({"message": "my stash"}))
        )
        .await
        .message,
        "Created stash: my stash"
    );
    assert_eq!(git_trim(&f.repo, &["status", "--porcelain"]), "");
    assert_eq!(
        run(&f.repo, action("stash-apply", "0")).await.message,
        "Applied stash@{0}"
    );
    assert_eq!(read(format!("{}/file.txt", f.repo)), "wip\n");
    git(&f.repo, &["checkout", "--", "file.txt"]);
    git(&f.repo, &["clean", "-fdq"]);
    assert_eq!(
        run(&f.repo, action("stash-pop", "stash@{0}")).await.message,
        "Popped stash@{0}"
    );
    assert_eq!(git_trim(&f.repo, &["stash", "list"]), "");
    git(&f.repo, &["stash", "push", "-u", "-m", "again"]);
    assert_eq!(
        run(
            &f.repo,
            action_with("stash-branch", "from-stash", json!({"secondTarget": "0"}))
        )
        .await
        .message,
        "Created branch from-stash from stash@{0}"
    );
    assert_eq!(
        git_trim(&f.repo, &["branch", "--show-current"]),
        "from-stash"
    );
    git(&f.repo, &["stash", "push", "-u", "-m", "drop"]);
    // nothing to stash on a clean tree -> drop the named one instead
    git(&f.repo, &["checkout", "--", "."]);
    write(format!("{}/file.txt", f.repo), "second\n");
    git(&f.repo, &["stash", "push", "-m", "second"]);
    assert_eq!(
        run(&f.repo, action("stash-drop", "0")).await.message,
        "Dropped stash@{0}"
    );
    // default stash message falls back to the target, then "WIP"
    write(format!("{}/file.txt", f.repo), "x\n");
    assert_eq!(
        run(&f.repo, action("stash-create", "")).await.message,
        "Created stash: WIP"
    );
}

#[tokio::test]
async fn applies_hunk_patches_to_stage_or_roll_back() {
    let f = init_repo();
    write(format!("{}/file.txt", f.repo), "changed\n");
    let patch = "diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-hello\n+changed";
    let r = run(
        &f.repo,
        action_with(
            "apply-hunk-patch",
            "",
            json!({"patch": patch, "patchMode": "stage"}),
        ),
    )
    .await;
    assert_eq!(r.message, "Staged hunk");
    assert_eq!(
        git_trim(&f.repo, &["diff", "--cached", "--name-only"]),
        "file.txt"
    );
    git(&f.repo, &["reset", "-q"]);
    let r = run(
        &f.repo,
        action_with(
            "apply-hunk-patch",
            "",
            json!({"patch": patch, "patchMode": "rollback"}),
        ),
    )
    .await;
    assert_eq!(r.message, "Rolled back hunk");
    assert_eq!(read(format!("{}/file.txt", f.repo)), "hello\n");
    let err = execute_git_log_action(
        &f.repo,
        &action_with("apply-hunk-patch", "", json!({"patch": "  "})),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Patch content is required for apply-hunk-patch"
    );
}

#[tokio::test]
async fn rejects_invalid_targets() {
    let f = init_repo();
    for (kind, target) in [
        ("cherry-pick", ""),
        ("revert", "-x"),
        ("checkout-ref", "--force"),
    ] {
        let err = execute_git_log_action(&f.repo, &action(kind, target))
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), format!("Invalid git target: {target}"));
    }
    let err = execute_git_log_action(
        &f.repo,
        &action_with("create-branch", "ok", json!({"secondTarget": "-bad"})),
    )
    .await
    .unwrap_err();
    assert_eq!(err.to_string(), "Invalid secondary git target: -bad");
    let plain = f.path("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert!(matches!(
        execute_git_log_action(&plain, &action("create-tag", "v1"))
            .await
            .unwrap_err(),
        paseo_git::GitError::NotGitRepo { .. }
    ));
}
