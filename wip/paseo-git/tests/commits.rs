mod common;

use common::*;
use paseo_git::commits::{list_checkout_commits, CommitFileStatus};
use paseo_git::repo::CheckoutContext;
use paseo_git::worktree_meta::write_paseo_worktree_metadata;
use serde_json::json;
use std::io::Write;
use std::process::{Command, Stdio};

fn commit_as(repo: &str, message: &str) {
    git(
        repo,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--author=Test User <test@test.com>",
            "-m",
            message,
        ],
    );
}

fn commit_file_as(repo: &str, name: &str, content: &str, message: &str) {
    write(format!("{repo}/{name}"), content);
    git(repo, &["add", "."]);
    commit_as(repo, message);
}

fn import_linear_history(repo: &str, branch: &str, file: &str, subject: &str, count: usize) {
    let starting = git_trim(repo, &["rev-parse", "HEAD"]);
    let mut commands = String::new();
    let mut parent = starting;
    for index in 1..=count {
        let blob_mark = index * 2 - 1;
        let commit_mark = index * 2;
        let content = format!("{index}\n");
        let message = format!("{subject} {index}");
        commands.push_str(&format!(
            "blob\nmark :{blob_mark}\ndata {}\n{content}\n",
            content.len()
        ));
        commands.push_str(&format!(
            "commit refs/heads/{branch}\nmark :{commit_mark}\ncommitter Test User <test@test.com> {} +0000\ndata {}\n{message}\nfrom {parent}\nM 100644 :{blob_mark} {file}\n\n",
            1_700_000_000 + index,
            message.len(),
        ));
        parent = format!(":{commit_mark}");
    }
    let mut child = Command::new("git")
        .args(["fast-import", "--quiet"])
        .current_dir(repo)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(commands.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    git(repo, &["reset", "--hard", branch]);
}

fn init_repo_on_main() -> Fixture {
    let f = init_empty_repo("Test User");
    commit_file_as(&f.repo, "README.md", "base\n", "initial");
    f
}

fn subjects(commits: &[paseo_git::commits::CheckoutCommit]) -> Vec<&str> {
    commits.iter().map(|c| c.subject.as_str()).collect()
}

#[tokio::test]
async fn fixture_authors_are_independent_of_command_scoped_git_configuration() {
    let (_t, temp) = temp_dir();
    let repo = format!("{temp}/repo");
    std::fs::create_dir_all(&repo).unwrap();
    let overrides = [
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "user.name"),
        ("GIT_CONFIG_VALUE_0", "Other User"),
    ];
    git_env(&repo, &["init", "-b", "main"], &overrides);
    git_env(
        &repo,
        &["config", "user.email", "test@test.com"],
        &overrides,
    );
    git_env(&repo, &["config", "user.name", "Test User"], &overrides);
    write(format!("{repo}/README.md"), "base\n");
    git_env(&repo, &["add", "."], &overrides);
    git_env(
        &repo,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--author=Test User <test@test.com>",
            "-m",
            "initial",
        ],
        &overrides,
    );
    let result = list_checkout_commits(&repo, &CheckoutContext::default())
        .await
        .unwrap();
    assert_eq!(result.commits[0].author_name, "Test User");
}

#[tokio::test]
async fn lists_recent_commits_newest_first_with_on_remote_flags_and_file_stats() {
    let f = init_repo_on_main();
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file_as(&f.repo, "foo.txt", "a\nb\nc\n", "Add foo");
    add_bare_remote(&f);
    git(&f.repo, &["push", "-u", "origin", "feature"]);
    commit_file_as(&f.repo, "bar.txt", "x\n", "Add bar");

    let r = list_checkout_commits(&f.repo, &CheckoutContext::default())
        .await
        .unwrap();
    assert_eq!(r.base_ref.as_deref(), Some("main"));
    assert_eq!(subjects(&r.commits), ["Add bar", "Add foo", "initial"]);
    let remote: Vec<_> = r.commits.iter().map(|c| c.is_on_remote).collect();
    assert_eq!(remote, [false, true, true]);
    let on_base: Vec<_> = r.commits.iter().map(|c| c.is_on_base).collect();
    assert_eq!(on_base, [Some(false), Some(false), Some(true)]);
    assert_eq!(
        serde_json::to_value(&r.commits[0].files).unwrap(),
        json!([{"path": "bar.txt", "additions": 1, "deletions": 0, "status": "added"}])
    );
    assert_eq!(
        serde_json::to_value(&r.commits[1].files).unwrap(),
        json!([{"path": "foo.txt", "additions": 3, "deletions": 0, "status": "added"}])
    );
    assert_eq!(r.commits[0].author_name, "Test User");
    assert_eq!(r.commits[0].sha.len(), 40);
    assert!(!r.commits[0].short_sha.is_empty());
    assert!(
        r.commits[0].author_date.contains('T'),
        "ISO 8601: {}",
        r.commits[0].author_date
    );
}

#[tokio::test]
async fn wire_shape_of_the_commits_list_payload() {
    let f = init_repo_on_main();
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file_as(&f.repo, "foo.txt", "a\n", "Add foo");
    let r = list_checkout_commits(&f.repo, &CheckoutContext::default())
        .await
        .unwrap();
    let payload = paseo_git::commits::build_commits_list_payload(&f.repo, "req-1", &Ok(r));
    let commit = &payload["commits"][0];
    for key in [
        "sha",
        "shortSha",
        "subject",
        "authorName",
        "authorDate",
        "isOnRemote",
        "isOnBase",
        "files",
    ] {
        assert!(commit.get(key).is_some(), "missing {key}");
    }
    for key in ["authorEmail", "body", "parents", "refs"] {
        assert!(commit.get(key).is_none(), "unexpected {key}");
    }
    assert_eq!(payload["baseRef"], "main");
    assert_eq!(payload["error"], serde_json::Value::Null);
    assert_eq!(payload["requestId"], "req-1");
}

#[tokio::test]
async fn shows_recent_history_on_the_base_branch() {
    let f = init_repo_on_main();
    let r = list_checkout_commits(&f.repo, &CheckoutContext::default())
        .await
        .unwrap();
    assert_eq!(r.base_ref, None);
    assert_eq!(subjects(&r.commits), ["initial"]);
    assert_eq!(r.commits[0].is_on_base, Some(true));
}

#[tokio::test]
async fn keeps_recent_history_when_the_saved_base_branch_no_longer_exists() {
    let f = init_repo_on_main();
    let worktrees_root = f.path("worktrees");
    let worktree = format!("{worktrees_root}/repo-hash/feature");
    std::fs::create_dir_all(format!("{worktrees_root}/repo-hash")).unwrap();
    git(&f.repo, &["worktree", "add", "-b", "feature", &worktree]);
    commit_file_as(&worktree, "feature.txt", "feature\n", "Feature work");
    write_paseo_worktree_metadata(&worktree, "deleted-base", None).unwrap();
    let ctx = CheckoutContext {
        worktrees_root: Some(worktrees_root.clone()),
        ..Default::default()
    };
    let r = list_checkout_commits(&worktree, &ctx).await.unwrap();
    assert_eq!(r.base_ref.as_deref(), Some("main"));
    assert_eq!(subjects(&r.commits), ["Feature work", "initial"]);
    let on_base: Vec<_> = r.commits.iter().map(|c| c.is_on_base).collect();
    assert_eq!(on_base, [Some(false), Some(true)]);
}

#[tokio::test]
async fn marks_all_commits_local_only_when_there_is_no_remote() {
    let f = init_repo_on_main();
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file_as(&f.repo, "foo.txt", "a\n", "Add foo");
    commit_file_as(&f.repo, "bar.txt", "b\n", "Add bar");
    let r = list_checkout_commits(&f.repo, &CheckoutContext::default())
        .await
        .unwrap();
    assert_eq!(r.base_ref.as_deref(), Some("main"));
    assert_eq!(r.commits.len(), 3);
    assert!(r.commits.iter().all(|c| !c.is_on_remote));
}

#[tokio::test]
async fn recognizes_base_history_on_a_remote_before_the_feature_branch_is_pushed() {
    let f = init_repo_on_main();
    add_bare_remote(&f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file_as(&f.repo, "feature.txt", "local\n", "Local feature");
    let r = list_checkout_commits(&f.repo, &CheckoutContext::default())
        .await
        .unwrap();
    let pairs: Vec<_> = r
        .commits
        .iter()
        .map(|c| (c.subject.as_str(), c.is_on_remote))
        .collect();
    assert_eq!(pairs, [("Local feature", false), ("initial", true)]);
}

#[tokio::test]
async fn keeps_local_base_commits_out_of_workspace_history_when_the_local_base_is_ahead() {
    let f = init_repo_on_main();
    add_bare_remote(&f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    commit_file_as(&f.repo, "local-base.txt", "base\n", "Local base work");
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file_as(&f.repo, "feature.txt", "feature\n", "Feature work");
    let r = list_checkout_commits(&f.repo, &CheckoutContext::default())
        .await
        .unwrap();
    let pairs: Vec<_> = r
        .commits
        .iter()
        .map(|c| (c.subject.as_str(), c.is_on_base))
        .collect();
    assert_eq!(
        pairs,
        [
            ("Feature work", Some(false)),
            ("Local base work", Some(true)),
            ("initial", Some(true))
        ]
    );
}

#[tokio::test]
async fn shows_every_workspace_commit_followed_by_at_most_10_base_commits() {
    let f = init_repo_on_main();
    import_linear_history(&f.repo, "main", "base-history.txt", "Base", 14);
    git(&f.repo, &["checkout", "-b", "feature"]);
    import_linear_history(&f.repo, "feature", "workspace-history.txt", "Workspace", 24);
    let r = list_checkout_commits(&f.repo, &CheckoutContext::default())
        .await
        .unwrap();
    assert_eq!(r.commits.len(), 34);
    assert!(r.commits.iter().all(|c| !c.is_on_remote));
    let expected: Vec<String> = (0..24).map(|i| format!("Workspace {}", 24 - i)).collect();
    assert_eq!(
        subjects(&r.commits[..24]),
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(r.commits[..24].iter().all(|c| c.is_on_base == Some(false)));
    let base: Vec<String> = (0..10).map(|i| format!("Base {}", 14 - i)).collect();
    assert_eq!(
        subjects(&r.commits[24..]),
        base.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(r.commits[24..].iter().all(|c| c.is_on_base == Some(true)));
}

#[tokio::test]
async fn starts_base_context_at_the_fork_point_when_the_base_branch_has_advanced() {
    let f = init_repo_on_main();
    commit_file_as(&f.repo, "shared.txt", "shared\n", "Shared base");
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file_as(&f.repo, "feature.txt", "feature\n", "Feature work");
    git(&f.repo, &["checkout", "main"]);
    commit_file_as(&f.repo, "newer-base.txt", "newer\n", "Newer base");
    git(&f.repo, &["checkout", "feature"]);
    let r = list_checkout_commits(&f.repo, &CheckoutContext::default())
        .await
        .unwrap();
    let pairs: Vec<_> = r
        .commits
        .iter()
        .map(|c| (c.subject.as_str(), c.is_on_base))
        .collect();
    assert_eq!(
        pairs,
        [
            ("Feature work", Some(false)),
            ("Shared base", Some(true)),
            ("initial", Some(true))
        ]
    );
}

#[tokio::test]
async fn limits_base_branch_history_to_10_commits() {
    let f = init_repo_on_main();
    import_linear_history(&f.repo, "main", "history.txt", "Commit", 14);
    let r = list_checkout_commits(&f.repo, &CheckoutContext::default())
        .await
        .unwrap();
    let expected: Vec<String> = (0..10).map(|i| format!("Commit {}", 14 - i)).collect();
    assert_eq!(
        subjects(&r.commits),
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(r.commits.iter().all(|c| c.is_on_base == Some(true)));
}

#[tokio::test]
async fn shows_merged_branch_commits_and_compares_the_merge_against_its_first_parent() {
    let f = init_repo_on_main();
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file_as(&f.repo, "feature.txt", "feature\n", "Add feature");
    git(&f.repo, &["checkout", "main"]);
    commit_file_as(&f.repo, "main.txt", "main\n", "Advance main");
    git(
        &f.repo,
        &["merge", "--no-ff", "feature", "-m", "Merge feature"],
    );
    let r = list_checkout_commits(&f.repo, &CheckoutContext::default())
        .await
        .unwrap();
    assert_eq!(
        subjects(&r.commits),
        ["Merge feature", "Advance main", "Add feature", "initial"]
    );
    assert_eq!(
        serde_json::to_value(&r.commits[0].files).unwrap(),
        json!([{"path": "feature.txt", "additions": 1, "deletions": 0, "status": "added"}])
    );
}

#[tokio::test]
async fn classifies_renamed_files_with_status_renamed_and_correct_destination_path() {
    let f = init_repo_on_main();
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file_as(&f.repo, "original.txt", "content\n", "Add original");
    git(&f.repo, &["mv", "original.txt", "renamed.txt"]);
    commit_as(&f.repo, "Rename file");
    let r = list_checkout_commits(&f.repo, &CheckoutContext::default())
        .await
        .unwrap();
    let files = &r.commits[0].files;
    assert_eq!(files.len(), 1);
    assert_eq!(
        (
            files[0].path.as_str(),
            files[0].additions,
            files[0].deletions,
            files[0].status
        ),
        ("renamed.txt", 0, 0, Some(CommitFileStatus::Renamed))
    );
}

#[tokio::test]
async fn derives_status_for_modified_and_deleted_files() {
    let f = init_repo_on_main();
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file_as(&f.repo, "README.md", "base\nmore\n", "Edit readme");
    git(&f.repo, &["rm", "README.md"]);
    commit_as(&f.repo, "Delete readme");
    let r = list_checkout_commits(&f.repo, &CheckoutContext::default())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&r.commits[0].files).unwrap(),
        json!([{"path": "README.md", "additions": 0, "deletions": 2, "status": "deleted"}])
    );
    assert_eq!(
        serde_json::to_value(&r.commits[1].files).unwrap(),
        json!([{"path": "README.md", "additions": 1, "deletions": 0, "status": "modified"}])
    );
}

#[tokio::test]
async fn detached_or_unborn_head_has_no_commits() {
    let (_t, temp) = temp_dir();
    let repo = format!("{temp}/empty");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    let r = list_checkout_commits(&repo, &CheckoutContext::default())
        .await
        .unwrap();
    assert!(r.commits.is_empty() && r.base_ref.is_none());
}
