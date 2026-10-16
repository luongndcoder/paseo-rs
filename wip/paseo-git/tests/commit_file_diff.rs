mod common;

use common::*;
use paseo_git::commits::{get_commit_file_diff, list_checkout_commits};
use paseo_git::diff_parse::{DiffLineType, ParsedDiffFile};
use paseo_git::repo::CheckoutContext;
use paseo_git::run::start_git_command_metrics;
use serde_json::json;

fn init() -> String {
    init_empty_repo("Test User").repo_path()
}

trait RepoPath {
    fn repo_path(self) -> String;
}

// Keep the temp dir alive for the whole test by leaking the fixture; the OS
// temp dir is cleaned by the test runner machine, and tests are short-lived.
impl RepoPath for Fixture {
    fn repo_path(self) -> String {
        let repo = self.repo.clone();
        std::mem::forget(self);
        repo
    }
}

fn head(repo: &str) -> String {
    git_trim(repo, &["rev-parse", "HEAD"])
}

fn lines(file: &ParsedDiffFile) -> Vec<(DiffLineType, String)> {
    file.hunks
        .iter()
        .flat_map(|h| h.lines.iter().map(|l| (l.kind, l.content.clone())))
        .collect()
}

async fn rename_only_case(name: &str) {
    let repo = init();
    commit_file(&repo, "old.txt", "same\n", "initial");
    git(&repo, &["mv", "old.txt", name]);
    commit(&repo, "rename");
    let sha = head(&repo);
    let history = list_checkout_commits(&repo, &CheckoutContext::default())
        .await
        .unwrap();
    let entry = history.commits.iter().find(|c| c.sha == sha).unwrap();
    assert_eq!(
        serde_json::to_value(&entry.files).unwrap(),
        json!([{"path": name, "status": "renamed", "additions": 0, "deletions": 0}])
    );
    let file = get_commit_file_diff(&repo, &sha, name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&file).unwrap(),
        json!({"path": name, "isNew": false, "isDeleted": false, "additions": 0, "deletions": 0, "hunks": []})
    );
}

#[tokio::test]
async fn preserves_a_rename_only_commit_to_new_txt() {
    rename_only_case("new.txt").await;
}

#[tokio::test]
async fn preserves_a_rename_only_commit_to_a_name_with_spaces() {
    rename_only_case("with spaces.txt").await;
}

async fn rename_with_edit_case(name: &str) {
    let repo = init();
    commit_file(&repo, "old.txt", "same\nkeep\nlast\n", "initial");
    git(&repo, &["mv", "old.txt", name]);
    write(format!("{repo}/{name}"), "same\nKEEP\nlast\n");
    git(&repo, &["add", "."]);
    commit(&repo, "rename and edit");
    let sha = head(&repo);
    let history = list_checkout_commits(&repo, &CheckoutContext::default())
        .await
        .unwrap();
    let entry = history.commits.iter().find(|c| c.sha == sha).unwrap();
    assert_eq!(
        serde_json::to_value(&entry.files).unwrap(),
        json!([{"path": name, "status": "renamed", "additions": 1, "deletions": 1}])
    );
    let file = get_commit_file_diff(&repo, &sha, name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            file.path.as_str(),
            file.is_new,
            file.is_deleted,
            file.additions,
            file.deletions
        ),
        (name, false, false, 1, 1)
    );
    let l = lines(&file);
    assert!(l.contains(&(DiffLineType::Remove, "keep".into())));
    assert!(l.contains(&(DiffLineType::Add, "KEEP".into())));
}

#[tokio::test]
async fn preserves_the_edits_in_a_committed_rename_to_new_txt() {
    rename_with_edit_case("new.txt").await;
}

#[tokio::test]
async fn preserves_the_edits_in_a_committed_rename_to_a_unicode_name() {
    rename_with_edit_case("目录.txt").await;
}

#[tokio::test]
async fn preserves_a_rename_to_a_quoted_filename() {
    let repo = init();
    let name = "new \"draft\".txt";
    commit_file(&repo, "old.txt", "same\n", "initial");
    git(&repo, &["mv", "old.txt", name]);
    commit(&repo, "rename");
    let file = get_commit_file_diff(&repo, &head(&repo), name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&file).unwrap(),
        json!({"path": name, "isNew": false, "isDeleted": false, "additions": 0, "deletions": 0, "hunks": []})
    );
}

#[tokio::test]
async fn returns_the_parsed_diff_for_a_modified_file_in_a_commit() {
    let repo = init();
    commit_file(&repo, "foo.txt", "a\nb\nc\n", "initial");
    commit_file(&repo, "foo.txt", "a\nB\nc\nd\n", "edit foo");
    let file = get_commit_file_diff(&repo, &head(&repo), "foo.txt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(file.path, "foo.txt");
    assert_eq!((file.additions, file.deletions), (2, 1));
    assert!(!file.hunks.is_empty());
    let l: Vec<_> = file.hunks[0]
        .lines
        .iter()
        .map(|l| (l.kind, l.content.as_str()))
        .collect();
    assert!(l.contains(&(DiffLineType::Add, "B")));
    assert!(l.contains(&(DiffLineType::Add, "d")));
    assert!(l.contains(&(DiffLineType::Remove, "b")));
}

#[tokio::test]
async fn returns_a_diff_flagged_as_new_for_an_added_file() {
    let repo = init();
    commit_file(&repo, "README.md", "base\n", "initial");
    commit_file(&repo, "added.txt", "x\ny\n", "add file");
    let file = get_commit_file_diff(&repo, &head(&repo), "added.txt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            file.path.as_str(),
            file.is_new,
            file.additions,
            file.deletions
        ),
        ("added.txt", true, 2, 0)
    );
}

async fn added_copy_case(added: &str) {
    let repo = init();
    commit_file(
        &repo,
        "source.txt",
        "line1\nline2\nline3\nline4\nline5\nline6\n",
        "initial",
    );
    commit_file(&repo, "added.txt", added, "add similar file");
    let sha = head(&repo);
    let history = list_checkout_commits(&repo, &CheckoutContext::default())
        .await
        .unwrap();
    let entry = history.commits.iter().find(|c| c.sha == sha).unwrap();
    assert_eq!(
        serde_json::to_value(&entry.files).unwrap(),
        json!([{"path": "added.txt", "status": "added", "additions": 6, "deletions": 0}])
    );
    let file = get_commit_file_diff(&repo, &sha, "added.txt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (file.is_new, file.is_deleted, file.additions, file.deletions),
        (true, false, 6, 0)
    );
    let body: Vec<_> = lines(&file)
        .into_iter()
        .filter(|(k, _)| *k != DiffLineType::Header)
        .collect();
    let expected: Vec<_> = added
        .trim_end()
        .split('\n')
        .map(|c| (DiffLineType::Add, c.to_string()))
        .collect();
    assert_eq!(body, expected);
}

#[tokio::test]
async fn shows_an_added_exact_copy_as_a_new_file() {
    added_copy_case("line1\nline2\nline3\nline4\nline5\nline6\n").await;
}

#[tokio::test]
async fn shows_an_added_edited_copy_as_a_new_file() {
    added_copy_case("line1\nline2\nline3\nline4\nline5\nCHANGED\n").await;
}

#[tokio::test]
async fn shares_commit_rename_discovery_across_concurrent_and_later_file_requests() {
    let repo = init();
    commit_file(&repo, "old.txt", "same\n", "initial");
    git(&repo, &["mv", "old.txt", "new.txt"]);
    commit_file(&repo, "added.txt", "added\n", "rename and add");
    let sha = head(&repo);

    let metrics = start_git_command_metrics();
    let (renamed, added) = tokio::join!(
        get_commit_file_diff(&repo, &sha, "new.txt"),
        get_commit_file_diff(&repo, &sha, "added.txt"),
    );
    let (renamed, added) = (renamed.unwrap().unwrap(), added.unwrap().unwrap());
    assert_eq!(
        (renamed.is_new, renamed.additions, renamed.deletions),
        (false, 0, 0)
    );
    assert_eq!(
        (added.is_new, added.additions, added.deletions),
        (true, 1, 0)
    );
    assert_eq!(
        get_commit_file_diff(&repo, &sha, "new.txt")
            .await
            .unwrap()
            .unwrap(),
        renamed
    );
    let commands = metrics.stop();
    let scans = commands
        .iter()
        .filter(|c| c.cwd == repo && c.args.iter().any(|a| a == "--name-status"))
        .count();
    assert_eq!(scans, 1);
}

#[tokio::test]
async fn rediscovers_renames_when_a_symbolic_commit_ref_moves() {
    let repo = init();
    commit_file(&repo, "old.txt", "same\n", "initial");
    let first = get_commit_file_diff(&repo, "HEAD", "old.txt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((first.is_new, first.additions), (true, 1));
    git(&repo, &["mv", "old.txt", "new.txt"]);
    commit(&repo, "rename");
    let second = get_commit_file_diff(&repo, "HEAD", "new.txt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (second.is_new, second.additions, second.deletions),
        (false, 0, 0)
    );
}

#[tokio::test]
async fn returns_none_for_a_path_not_changed_in_the_commit() {
    let repo = init();
    commit_file(&repo, "foo.txt", "a\n", "initial");
    commit_file(&repo, "foo.txt", "a\nb\n", "edit foo");
    assert!(
        get_commit_file_diff(&repo, &head(&repo), "does-not-exist.txt")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn does_not_follow_an_unchanged_file_back_to_an_earlier_commit() {
    let repo = init();
    commit_file(&repo, "foo.txt", "same\n", "initial");
    commit_file(&repo, "other.txt", "other\n", "unrelated change");
    assert!(get_commit_file_diff(&repo, &head(&repo), "foo.txt")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn returns_the_added_file_in_a_root_commit_and_the_deleted_file_in_its_deletion_commit() {
    let repo = init();
    commit_file(&repo, "foo.txt", "same\n", "initial");
    let added = get_commit_file_diff(&repo, &head(&repo), "foo.txt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            added.is_new,
            added.is_deleted,
            added.additions,
            added.deletions
        ),
        (true, false, 1, 0)
    );
    git(&repo, &["rm", "foo.txt"]);
    commit(&repo, "delete");
    let deleted = get_commit_file_diff(&repo, &head(&repo), "foo.txt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            deleted.is_new,
            deleted.is_deleted,
            deleted.additions,
            deleted.deletions
        ),
        (false, true, 0, 1)
    );
}

#[tokio::test]
async fn preserves_a_rename_introduced_by_a_merge_against_its_first_parent() {
    let repo = init();
    commit_file(&repo, "old.txt", "same\n", "initial");
    git(&repo, &["checkout", "-b", "feature"]);
    git(&repo, &["mv", "old.txt", "new.txt"]);
    commit(&repo, "rename");
    git(&repo, &["checkout", "main"]);
    commit_file(&repo, "main.txt", "main\n", "advance main");
    git(
        &repo,
        &["merge", "--no-ff", "feature", "-m", "merge feature"],
    );
    let file = get_commit_file_diff(&repo, &head(&repo), "new.txt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            file.path.as_str(),
            file.is_new,
            file.is_deleted,
            file.additions,
            file.deletions
        ),
        ("new.txt", false, false, 0, 0)
    );
    assert!(file.hunks.is_empty());
}

#[tokio::test]
async fn returns_a_merge_commit_diff_against_its_first_parent() {
    let repo = init();
    commit_file(&repo, "README.md", "base\n", "initial");
    git(&repo, &["checkout", "-b", "feature"]);
    commit_file(&repo, "feature.txt", "feature\n", "add feature");
    git(&repo, &["checkout", "main"]);
    commit_file(&repo, "main.txt", "main\n", "advance main");
    git(
        &repo,
        &["merge", "--no-ff", "feature", "-m", "merge feature"],
    );
    let file = get_commit_file_diff(&repo, &head(&repo), "feature.txt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            file.path.as_str(),
            file.is_new,
            file.additions,
            file.deletions
        ),
        ("feature.txt", true, 1, 0)
    );
}

#[tokio::test]
async fn binary_only_changes_report_as_absent() {
    let repo = init();
    commit_file(&repo, "README.md", "base\n", "initial");
    write(format!("{repo}/blob.bin"), [0u8, 1, 2, 3]);
    git(&repo, &["add", "."]);
    commit(&repo, "add binary");
    assert!(get_commit_file_diff(&repo, &head(&repo), "blob.bin")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn unknown_sha_surfaces_the_git_error() {
    let repo = init();
    commit_file(&repo, "README.md", "base\n", "initial");
    let err = get_commit_file_diff(
        &repo,
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        "README.md",
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("Git command failed: git show deadbeef"),
        "{err}"
    );
}

#[test]
fn request_validation_rejects_unsafe_refs_and_paths() {
    use paseo_git::commits::validate_commit_file_diff_request as v;
    assert!(v("abc123", "src/a.txt").is_ok());
    assert_eq!(
        v("a..b", "x").unwrap_err().to_string(),
        "Invalid commit: a..b"
    );
    for bad in ["", "/abs", "../x", "a/../b", "a\\..\\b"] {
        assert_eq!(
            v("abc", bad).unwrap_err().to_string(),
            format!("Invalid path: {bad}")
        );
    }
}
