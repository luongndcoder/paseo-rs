mod common;

use common::*;
use paseo_git::commits_list::{handle_commits_list_request, CommitsListRequest};
use paseo_git::graph::*;
use paseo_git::repo::CheckoutContext;
use serde_json::json;

fn ctx() -> CheckoutContext {
    CheckoutContext::default()
}

// ---- refs, tags, stashes ---------------------------------------------------------------------

#[tokio::test]
async fn lists_branches_remote_branches_tags_and_stashes() {
    let f = init_repo();
    let (_, clone) = setup_remote_tracking_main(&f);
    commit_file(&clone, "remote.txt", "r\n", "remote commit");
    git(&clone, &["push"]);
    git(&f.repo, &["fetch", "origin"]);
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "feature.txt", "f\n", "feature commit");
    git(&f.repo, &["tag", "light"]);
    git(&f.repo, &["tag", "-a", "annotated", "-m", "annotated tag"]);
    write(format!("{}/file.txt", f.repo), "stash me\n");
    git(&f.repo, &["stash", "push", "-m", "my wip"]);
    write(format!("{}/file.txt", f.repo), "stash two\n");
    git(&f.repo, &["stash", "push"]);

    let refs = list_refs_and_branches(&f.repo).await;
    let main = refs.branches.iter().find(|b| b.name == "main").unwrap();
    assert_eq!(
        (
            main.is_current,
            main.upstream.as_deref(),
            main.ahead,
            main.behind
        ),
        (false, Some("origin/main"), 0, 1)
    );
    let feature = refs.branches.iter().find(|b| b.name == "feature").unwrap();
    assert_eq!(
        (
            feature.is_current,
            feature.upstream.clone(),
            feature.sha.len()
        ),
        (true, None, 40)
    );

    let remote_names: Vec<_> = refs
        .remote_branches
        .iter()
        .map(|b| (b.name.as_str(), b.remote.as_str()))
        .collect();
    assert_eq!(remote_names, [("origin/main", "origin")]);

    let tag_names: Vec<_> = refs.tags.iter().map(|t| t.name.as_str()).collect();
    assert!(tag_names.contains(&"light") && tag_names.contains(&"annotated"));
    let head = git_trim(&f.repo, &["rev-parse", "HEAD"]);
    assert!(
        refs.tags.iter().all(|t| t.sha == head),
        "annotated tags resolve to the peeled commit"
    );
    assert!(refs
        .tags
        .iter()
        .find(|t| t.name == "annotated")
        .unwrap()
        .date
        .is_some());

    assert_eq!(refs.stashes.len(), 2);
    assert_eq!(
        (
            refs.stashes[0].index,
            refs.stashes[0].r#ref.as_str(),
            refs.stashes[0].branch.as_deref()
        ),
        (0, "stash@{0}", Some("feature"))
    );
    assert!(
        refs.stashes[0].message.contains("feature commit"),
        "{:?}",
        refs.stashes[0]
    );
    assert_eq!(
        (refs.stashes[1].index, refs.stashes[1].message.as_str()),
        (1, "my wip")
    );
    assert_eq!(
        refs.conflict_state,
        GitConflictState {
            in_progress: None,
            conflict_files: vec![]
        }
    );
}

#[tokio::test]
async fn stash_nodes_serialize_with_a_ref_key() {
    let f = init_repo();
    write(format!("{}/file.txt", f.repo), "x\n");
    git(&f.repo, &["stash", "push", "-m", "wip"]);
    let refs = list_refs_and_branches(&f.repo).await;
    let v = serde_json::to_value(&refs.stashes[0]).unwrap();
    assert_eq!(v["ref"], "stash@{0}");
    assert_eq!(v["message"], "wip");
    assert!(v.get("r#ref").is_none());
}

// ---- graph ----------------------------------------------------------------------------------------

#[tokio::test]
async fn queries_the_graph_across_branches_with_parents_and_refs() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(
        &f.repo,
        "feature.txt",
        "feature body\n",
        "feature commit\n\nlong description",
    );
    git(&f.repo, &["checkout", "main"]);
    commit_file(&f.repo, "main.txt", "m\n", "main commit");

    let g = query_git_graph_and_refs(&f.repo, &GraphQuery::default(), &ctx())
        .await
        .unwrap();
    let subjects: Vec<_> = g.commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects.len(), 3);
    assert!(
        subjects.contains(&"feature commit")
            && subjects.contains(&"main commit")
            && subjects.contains(&"initial")
    );
    assert_eq!(g.current_branch.as_deref(), Some("main"));
    assert_eq!(
        g.head_sha.as_deref(),
        Some(git_trim(&f.repo, &["rev-parse", "HEAD"]).as_str())
    );
    assert_eq!(g.last_commit_message.as_deref(), Some("main commit"));
    let feature = g
        .commits
        .iter()
        .find(|c| c.subject == "feature commit")
        .unwrap();
    assert_eq!(feature.body.as_deref(), Some("long description"));
    assert_eq!(feature.author_email.as_deref(), Some("test@test.com"));
    assert_eq!(feature.parents.as_ref().unwrap().len(), 1);
    assert!(feature
        .refs
        .as_ref()
        .unwrap()
        .iter()
        .any(|r| r.contains("feature")));
    assert!(feature.is_on_base.is_none());
    let head_commit = g
        .commits
        .iter()
        .find(|c| c.subject == "main commit")
        .unwrap();
    assert!(head_commit
        .refs
        .as_ref()
        .unwrap()
        .iter()
        .any(|r| r.contains("HEAD -> main")));
    assert_eq!(
        serde_json::to_value(&feature.files).unwrap(),
        json!([{"path": "feature.txt", "additions": 1, "deletions": 0, "status": "added"}])
    );
}

#[tokio::test]
async fn graph_filters_by_ref_search_author_and_file_with_follow() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "feature.txt", "one\n", "Add Feature");
    git(&f.repo, &["mv", "feature.txt", "renamed.txt"]);
    commit(&f.repo, "Rename feature");
    git_env(
        &f.repo,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "--author=Other Person <other@example.com>",
            "-m",
            "Empty by other",
        ],
        &[],
    );
    git(&f.repo, &["checkout", "main"]);

    let by_ref = query_git_graph_and_refs(
        &f.repo,
        &GraphQuery {
            r#ref: Some("feature".into()),
            ..Default::default()
        },
        &ctx(),
    )
    .await
    .unwrap();
    assert_eq!(by_ref.commits.len(), 4);
    let head_only = query_git_graph_and_refs(
        &f.repo,
        &GraphQuery {
            all_branches: Some(false),
            ..Default::default()
        },
        &ctx(),
    )
    .await
    .unwrap();
    assert_eq!(head_only.commits.len(), 1);
    let search = query_git_graph_and_refs(
        &f.repo,
        &GraphQuery {
            search: Some("add feature".into()),
            ..Default::default()
        },
        &ctx(),
    )
    .await
    .unwrap();
    assert_eq!(
        search
            .commits
            .iter()
            .map(|c| c.subject.as_str())
            .collect::<Vec<_>>(),
        ["Add Feature"]
    );
    let author = query_git_graph_and_refs(
        &f.repo,
        &GraphQuery {
            author: Some("OTHER".into()),
            ..Default::default()
        },
        &ctx(),
    )
    .await
    .unwrap();
    assert_eq!(
        author
            .commits
            .iter()
            .map(|c| c.subject.as_str())
            .collect::<Vec<_>>(),
        ["Empty by other"]
    );
    let limited = query_git_graph_and_refs(
        &f.repo,
        &GraphQuery {
            limit: Some(2),
            ..Default::default()
        },
        &ctx(),
    )
    .await
    .unwrap();
    assert_eq!(limited.commits.len(), 2);

    git(&f.repo, &["checkout", "feature"]);
    let followed = query_git_graph_and_refs(
        &f.repo,
        &GraphQuery {
            file_path: Some("renamed.txt".into()),
            ..Default::default()
        },
        &ctx(),
    )
    .await
    .unwrap();
    assert_eq!(
        followed
            .commits
            .iter()
            .map(|c| c.subject.as_str())
            .collect::<Vec<_>>(),
        ["Rename feature", "Add Feature"]
    );
}

#[tokio::test]
async fn graph_of_an_unborn_repository_is_empty() {
    let f = init_repo();
    let repo = f.path("unborn");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    let g = query_git_graph_and_refs(&repo, &GraphQuery::default(), &ctx())
        .await
        .unwrap();
    assert!(g.commits.is_empty() && g.head_sha.is_none() && g.last_commit_message.is_none());
}

#[tokio::test]
async fn graph_marks_unpushed_commits() {
    let f = init_repo();
    setup_remote_tracking_main(&f);
    commit_file(&f.repo, "local.txt", "l\n", "local only");
    let g = query_git_graph_and_refs(&f.repo, &GraphQuery::default(), &ctx())
        .await
        .unwrap();
    let flags: Vec<_> = g
        .commits
        .iter()
        .map(|c| (c.subject.as_str(), c.is_on_remote))
        .collect();
    assert_eq!(flags, [("local only", false), ("initial", true)]);
}

// ---- conflicts ----------------------------------------------------------------------------------------

fn conflicted(f: &Fixture) {
    commit_file(&f.repo, "conflict.txt", "base\n", "base");
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "conflict.txt", "feature\n", "feature");
    git(&f.repo, &["checkout", "main"]);
    commit_file(&f.repo, "conflict.txt", "main\n", "main");
    assert!(!git_succeeds(&f.repo, &["merge", "feature"]));
}

#[tokio::test]
async fn reports_conflict_state_and_conflict_file_contents_during_a_merge() {
    let f = init_repo();
    conflicted(&f);
    let state = detect_git_conflict_state(&f.repo).await;
    assert_eq!(
        state,
        GitConflictState {
            in_progress: Some(GitActiveOperation::Merge),
            conflict_files: vec!["conflict.txt".into()]
        }
    );
    assert_eq!(
        serde_json::to_value(&state).unwrap(),
        json!({"inProgress": "merge", "conflictFiles": ["conflict.txt"]})
    );

    let file = get_git_conflict_file(&f.repo, "conflict.txt")
        .await
        .unwrap();
    assert_eq!(
        (
            file.base_content.as_str(),
            file.ours_content.as_str(),
            file.theirs_content.as_str()
        ),
        ("base\n", "main\n", "feature\n")
    );
    assert!(file.working_content.contains("<<<<<<<"));
    assert_eq!(
        (file.ours_label.as_str(), file.theirs_label.as_str()),
        ("HEAD (main)", "MERGE_HEAD / Incoming")
    );

    for bad in ["", "-x", "../outside"] {
        let err = get_git_conflict_file(&f.repo, bad)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("Invalid conflict file path"),
            "{bad}: {err}"
        );
    }
}

#[tokio::test]
async fn detects_a_rebase_in_progress() {
    let f = init_repo();
    commit_file(&f.repo, "conflict.txt", "base\n", "base");
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "conflict.txt", "feature\n", "feature");
    git(&f.repo, &["checkout", "main"]);
    commit_file(&f.repo, "conflict.txt", "main\n", "main");
    git(&f.repo, &["checkout", "feature"]);
    assert!(!git_succeeds(&f.repo, &["rebase", "main"]));
    assert_eq!(
        detect_git_active_operation(&f.repo).await,
        Some(GitActiveOperation::Rebase)
    );
    let file = get_git_conflict_file(&f.repo, "conflict.txt")
        .await
        .unwrap();
    assert_eq!(file.theirs_label, "REBASE_HEAD / Incoming");
}

// ---- details ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn stash_and_compare_details_carry_files_and_parsed_diffs() {
    let f = init_repo();
    write(format!("{}/file.txt", f.repo), "stashed\n");
    write(format!("{}/new.txt", f.repo), "new\n");
    git(&f.repo, &["add", "new.txt"]);
    git(&f.repo, &["stash", "push", "-m", "wip"]);
    let detail = get_git_stash_detail(&f.repo, 0).await.unwrap();
    assert_eq!(detail.label, "stash@{0}");
    let files: Vec<_> = detail
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.additions, f.deletions))
        .collect();
    assert_eq!(files, [("file.txt", 1, 1), ("new.txt", 1, 0)]);
    assert_eq!(detail.diffs.len(), 2);
    assert!(get_git_stash_detail(&f.repo, 9).await.is_err());

    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "feature.txt", "a\nb\n", "feature");
    let compare = get_git_compare_detail(&f.repo, "main").await.unwrap();
    assert_eq!(compare.label, "main \u{2194} HEAD");
    assert_eq!(
        serde_json::to_value(&compare.files).unwrap(),
        json!([{"path": "feature.txt", "additions": 2, "deletions": 0, "status": "added"}])
    );
    assert!(get_git_compare_detail(&f.repo, "-x")
        .await
        .unwrap_err()
        .to_string()
        .starts_with("Invalid compare ref"));
}

#[tokio::test]
async fn blames_every_line_of_a_file() {
    let f = init_repo();
    commit_file(&f.repo, "a.txt", "one\ntwo\n", "add a");
    commit_file(&f.repo, "a.txt", "one\nTWO\nthree\n", "edit a");
    let lines = get_git_file_blame(&f.repo, "a.txt").await.unwrap();
    assert_eq!(
        lines.iter().map(|l| l.content.as_str()).collect::<Vec<_>>(),
        ["one", "TWO", "three"]
    );
    assert_eq!(
        lines.iter().map(|l| l.line_number).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(lines[0].summary, "add a");
    assert_eq!(lines[1].summary, "edit a");
    assert_eq!(
        (lines[0].author.as_str(), lines[0].short_sha.len()),
        ("Test", 7)
    );
    assert!(lines[0].author_time > 1_000_000_000);
    assert!(get_git_file_blame(&f.repo, "-bad").await.is_err());
}

// ---- request dispatch -------------------------------------------------------------------------------------

fn request(cwd: &str, extra: serde_json::Value) -> CommitsListRequest {
    let mut v = json!({"cwd": cwd, "requestId": "req-1"});
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    serde_json::from_value(v).unwrap()
}

#[tokio::test]
async fn dispatches_each_commits_list_view_to_its_payload() {
    let f = init_repo();
    commit_file(&f.repo, "a.txt", "a\n", "add a");
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "b.txt", "b\n", "add b");
    write(format!("{}/file.txt", f.repo), "stash\n");
    git(&f.repo, &["stash", "push", "-m", "wip"]);

    let plain = handle_commits_list_request(&request(&f.repo, json!({})), &ctx()).await;
    assert!(!plain.git_mutated);
    assert_eq!(plain.payload["baseRef"], "main");
    assert_eq!(plain.payload["commits"][0]["subject"], "add b");
    assert_eq!(plain.payload["error"], serde_json::Value::Null);

    let graph =
        handle_commits_list_request(&request(&f.repo, json!({"includeGraph": true})), &ctx()).await;
    for key in [
        "headSha",
        "currentBranch",
        "lastCommitMessage",
        "branches",
        "remoteBranches",
        "tags",
        "stashes",
        "conflictState",
        "commits",
        "baseRef",
    ] {
        assert!(
            graph.payload.get(key).is_some(),
            "graph payload misses {key}"
        );
    }
    assert_eq!(graph.payload["requestId"], "req-1");
    assert_eq!(graph.payload["stashes"][0]["ref"], "stash@{0}");

    let blame =
        handle_commits_list_request(&request(&f.repo, json!({"blamePath": "a.txt"})), &ctx()).await;
    assert_eq!(blame.payload["blameLines"][0]["content"], "a");
    assert_eq!(blame.payload["commits"], json!([]));
    assert_eq!(blame.payload["baseRef"], serde_json::Value::Null);

    let stash =
        handle_commits_list_request(&request(&f.repo, json!({"stashIndex": 0})), &ctx()).await;
    assert_eq!(stash.payload["stashDetail"]["label"], "stash@{0}");

    let compare =
        handle_commits_list_request(&request(&f.repo, json!({"compareRef": "main"})), &ctx()).await;
    assert_eq!(
        compare.payload["compareDetail"]["files"][0]["path"],
        "b.txt"
    );

    let action = handle_commits_list_request(
        &request(
            &f.repo,
            json!({"gitAction": {"kind": "create-tag", "target": "v1"}}),
        ),
        &ctx(),
    )
    .await;
    assert!(action.git_mutated);
    assert_eq!(
        action.payload["actionResult"],
        json!({"success": true, "message": "Created tag v1"})
    );

    let failing = handle_commits_list_request(
        &request(&f.repo, json!({"conflictFilePath": "../x"})),
        &ctx(),
    )
    .await;
    assert_eq!(failing.payload["commits"], json!([]));
    assert_eq!(failing.payload["error"]["code"], "UNKNOWN");
    let not_git = f.path("plain");
    std::fs::create_dir_all(&not_git).unwrap();
    let nogit =
        handle_commits_list_request(&request(&not_git, json!({"includeGraph": true})), &ctx())
            .await;
    assert_eq!(nogit.payload["error"]["code"], "NOT_GIT_REPO");
}
