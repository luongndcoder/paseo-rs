mod common;

use common::*;
use paseo_git::commits::{get_commit_file_diff, list_checkout_commits};
use paseo_git::diff::{get_checkout_diff, CheckoutDiffCompare, CheckoutDiffResult, DiffMode};
use paseo_git::diff_parse::{DiffFileStatus, DiffLineType, ParsedDiffFile};
use paseo_git::repo::CheckoutContext;
use paseo_git::shortstat::{get_checkout_shortstat_uncached, CheckoutShortstat};
use serde_json::json;

fn structured_compare(mode: DiffMode, base_ref: Option<&str>) -> CheckoutDiffCompare {
    CheckoutDiffCompare {
        mode,
        base_ref: base_ref.map(str::to_string),
        ignore_whitespace: None,
        include_structured: Some(true),
    }
}

async fn diff(cwd: &str, compare: &CheckoutDiffCompare) -> CheckoutDiffResult {
    get_checkout_diff(cwd, compare, &CheckoutContext::default())
        .await
        .unwrap()
}

fn created_changed_repo(config: &str) -> String {
    let f = init_empty_repo("Test");
    let cwd = f.repo.clone();
    std::mem::forget(f);
    write(format!("{cwd}/file.txt"), "before\n");
    git(&cwd, &["add", "."]);
    commit(&cwd, "initial");
    let (key, value) = config.split_once('=').unwrap();
    git(&cwd, &["config", key, value]);
    write(format!("{cwd}/file.txt"), "after\n");
    cwd
}

fn edited_text_diff(file: &ParsedDiffFile, name: &str) {
    assert_eq!(file.path, name);
    assert_eq!(file.status, Some(DiffFileStatus::Ok));
    assert_eq!(
        (file.is_new, file.is_deleted, file.additions, file.deletions),
        (false, false, 1, 1)
    );
    assert_eq!(
        serde_json::to_value(&file.hunks).unwrap(),
        json!([{"oldStart": 1, "oldCount": 1, "newStart": 1, "newCount": 1, "lines": [
            {"type": "header", "content": "@@ -1 +1 @@"},
            {"type": "remove", "content": "before"},
            {"type": "add", "content": "after"},
        ]}])
    );
}

#[tokio::test]
async fn returns_a_quoted_filename_and_its_uncommitted_diff_using_its_actual_path() {
    let cwd = created_changed_repo("core.quotepath=false");
    let name = "report \"draft\".txt";
    write(format!("{cwd}/file.txt"), "before\n");
    write(format!("{cwd}/{name}"), "before\n");
    git(&cwd, &["add", "--", name]);
    commit(&cwd, "add quoted filename");
    write(format!("{cwd}/{name}"), "after\n");
    let result = diff(&cwd, &structured_compare(DiffMode::Uncommitted, None)).await;
    let files = result.structured.unwrap();
    assert_eq!(files.len(), 1);
    edited_text_diff(&files[0], name);
}

const NAMES: [&str; 8] = [
    "normal.txt",
    "with spaces.txt",
    "report \"draft\".txt",
    "back\\slash.txt",
    "tab\tname.txt",
    "line\nname.txt",
    " leading and trailing .txt ",
    "report \"old => draft\".txt",
];

fn create_quoted_paths_repo() -> String {
    let cwd = created_changed_repo("core.quotepath=false");
    write(format!("{cwd}/file.txt"), "before\n");
    for name in NAMES {
        write(format!("{cwd}/{name}"), "before\n");
    }
    let mut add = vec!["add", "--"];
    add.extend(NAMES);
    git(&cwd, &add);
    commit(&cwd, "add filenames");
    git(&cwd, &["checkout", "-b", "feature"]);
    for name in NAMES {
        write(format!("{cwd}/{name}"), "after\n");
    }
    cwd
}

#[tokio::test]
async fn preserves_tracked_and_untracked_paths_diff_lines_and_summary_counts() {
    let cwd = create_quoted_paths_repo();
    let untracked: Vec<String> = NAMES.iter().map(|n| format!("untracked-{n}")).collect();
    for name in &untracked {
        write(format!("{cwd}/{name}"), "new file\n");
    }
    let result = diff(&cwd, &structured_compare(DiffMode::Uncommitted, None)).await;
    let files = result.structured.unwrap();
    let mut got: Vec<_> = files.iter().map(|f| f.path.clone()).collect();
    got.sort();
    let mut want: Vec<String> = NAMES
        .iter()
        .map(|n| n.to_string())
        .chain(untracked.iter().cloned())
        .collect();
    want.sort();
    assert_eq!(got, want);
    for name in NAMES {
        edited_text_diff(files.iter().find(|f| f.path == name).unwrap(), name);
    }
    for name in &untracked {
        let f = files.iter().find(|f| &f.path == name).unwrap();
        assert_eq!(f.status, Some(DiffFileStatus::Ok));
        assert_eq!(
            (f.is_new, f.is_deleted, f.additions, f.deletions),
            (true, false, 1, 0)
        );
        assert_eq!(
            serde_json::to_value(&f.hunks).unwrap(),
            json!([{"oldStart": 0, "oldCount": 0, "newStart": 1, "newCount": 1, "lines": [
                {"type": "header", "content": "@@ -0,0 +1 @@"},
                {"type": "add", "content": "new file"},
            ]}])
        );
    }
    let stat = get_checkout_shortstat_uncached(&cwd, &CheckoutContext::default(), false)
        .await
        .unwrap();
    assert_eq!(
        stat,
        Some(CheckoutShortstat {
            additions: (NAMES.len() + untracked.len()) as u64,
            deletions: NAMES.len() as u64
        })
    );
}

#[tokio::test]
async fn opens_base_and_commit_diffs_using_the_actual_paths_from_the_commit_list() {
    let cwd = create_quoted_paths_repo();
    let mut add = vec!["add", "--"];
    add.extend(NAMES);
    git(&cwd, &add);
    commit(&cwd, "edit filenames");
    let sha = git_trim(&cwd, &["rev-parse", "HEAD"]);

    let base = diff(&cwd, &structured_compare(DiffMode::Base, Some("main")))
        .await
        .structured
        .unwrap();
    let mut got: Vec<_> = base.iter().map(|f| f.path.clone()).collect();
    got.sort();
    let mut want: Vec<String> = NAMES.iter().map(|n| n.to_string()).collect();
    want.sort();
    assert_eq!(got, want);
    for name in NAMES {
        edited_text_diff(base.iter().find(|f| f.path == name).unwrap(), name);
    }

    let history = list_checkout_commits(&cwd, &CheckoutContext::default())
        .await
        .unwrap();
    let commit_entry = &history.commits[0];
    assert_eq!(commit_entry.sha, sha);
    let mut sorted_names: Vec<&str> = NAMES.to_vec();
    sorted_names.sort();
    let expected: Vec<_> = sorted_names
        .iter()
        .map(|n| json!({"path": n, "additions": 1, "deletions": 1, "status": "modified"}))
        .collect();
    assert_eq!(
        serde_json::to_value(&commit_entry.files).unwrap(),
        json!(expected)
    );
    for file in &commit_entry.files {
        let parsed = get_commit_file_diff(&cwd, &sha, &file.path)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(parsed.path, file.path);
        assert_eq!((parsed.additions, parsed.deletions), (1, 1));
        assert_eq!(parsed.hunks[0].lines[1].content, "before");
    }
}

#[tokio::test]
async fn preserves_quoted_rename_destinations_and_binary_paths_in_diff_and_commit_lists() {
    let cwd = created_changed_repo("core.quotepath=false");
    let old_path = "old \"draft\".txt";
    let new_path = "new \"draft\".txt";
    let binary_path = "image\tdata.bin";
    write(format!("{cwd}/file.txt"), "before\n");
    write(format!("{cwd}/{old_path}"), "unchanged\n");
    write(format!("{cwd}/{binary_path}"), [0u8, 1, 2]);
    git(&cwd, &["add", "--", old_path, binary_path]);
    commit(&cwd, "add rename and binary files");
    git(&cwd, &["checkout", "-b", "feature"]);
    git(&cwd, &["mv", "--", old_path, new_path]);
    write(format!("{cwd}/{binary_path}"), [0u8, 3, 4]);

    let result = diff(&cwd, &structured_compare(DiffMode::Uncommitted, None)).await;
    let files = result.structured.unwrap();
    let summary: Vec<_> = files
        .iter()
        .map(|f| (f.path.as_str(), f.old_path.as_deref(), f.status))
        .collect();
    assert_eq!(
        summary,
        [
            (binary_path, None, Some(DiffFileStatus::Binary)),
            (new_path, Some(old_path), Some(DiffFileStatus::Ok))
        ]
    );
    // The staged flags are annotated for uncommitted structured diffs.
    let mut binary = files[0].clone();
    binary.staged = None;
    binary.unstaged = None;
    assert_eq!(
        serde_json::to_value(&binary).unwrap(),
        json!({"path": binary_path, "isNew": false, "isDeleted": false, "additions": 0, "deletions": 0, "hunks": [], "status": "binary"})
    );

    git(&cwd, &["add", "--", binary_path]);
    commit(&cwd, "rename file and edit binary");
    let history = list_checkout_commits(&cwd, &CheckoutContext::default())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&history.commits[0].files).unwrap(),
        json!([
            {"path": binary_path, "additions": 0, "deletions": 0, "status": "modified"},
            {"path": new_path, "additions": 0, "deletions": 0, "status": "renamed"},
        ])
    );
}

async fn batched_paths_case(config: &str) {
    let cwd = created_changed_repo(config);
    std::fs::create_dir_all(format!("{cwd}/a")).unwrap();
    for name in ["a/literal.txt", "with spaces.txt"] {
        write(format!("{cwd}/{name}"), "before\n");
    }
    git(&cwd, &["add", "a/literal.txt", "with spaces.txt"]);
    commit(&cwd, "add files");
    for name in ["a/literal.txt", "with spaces.txt"] {
        write(format!("{cwd}/{name}"), "after\n");
    }
    let files = diff(&cwd, &structured_compare(DiffMode::Uncommitted, None))
        .await
        .structured
        .unwrap();
    let paths: Vec<_> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["a/literal.txt", "file.txt", "with spaces.txt"]);
    for file in &files {
        let l: Vec<_> = file
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter().map(|l| (l.kind, l.content.as_str())))
            .collect();
        assert!(
            l.contains(&(DiffLineType::Remove, "before")),
            "{}",
            file.path
        );
        assert!(l.contains(&(DiffLineType::Add, "after")), "{}", file.path);
    }
    let (key, value) = config.split_once('=').unwrap();
    assert_eq!(git_trim(&cwd, &["config", "--get", key]), value);
}

#[tokio::test]
async fn preserves_batched_paths_with_mnemonic_prefix() {
    batched_paths_case("diff.mnemonicPrefix=true").await;
}

#[tokio::test]
async fn preserves_batched_paths_with_forced_color() {
    batched_paths_case("color.diff=always").await;
}

#[tokio::test]
async fn preserves_batched_paths_with_noprefix() {
    batched_paths_case("diff.noprefix=true").await;
}

// ---- external diff -----------------------------------------------------------------

fn external_diff_command() -> (String, String) {
    let (temp, dir) = temp_dir();
    std::mem::forget(temp);
    let marker = format!("{dir}/invoked");
    let script = format!("{dir}/external-diff.sh");
    write(
        &script,
        format!("#!/bin/sh\necho invoked > '{marker}'\necho 'CUSTOM EXTERNAL DIFF'\n"),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (format!("'{script}'"), marker)
}

fn expect_standard_patch(result: &CheckoutDiffResult) {
    let files = result.structured.as_ref().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        (
            files[0].path.as_str(),
            files[0].additions,
            files[0].deletions,
            files[0].status
        ),
        ("file.txt", 1, 1, Some(DiffFileStatus::Ok))
    );
    let l: Vec<_> = files[0].hunks[0]
        .lines
        .iter()
        .map(|l| (l.kind, l.content.as_str()))
        .collect();
    assert_eq!(
        l,
        [
            (DiffLineType::Header, "@@ -1 +1 @@"),
            (DiffLineType::Remove, "before"),
            (DiffLineType::Add, "after")
        ]
    );
}

fn external_diff_repo() -> (String, String, String) {
    let cwd = created_changed_repo("color.diff=always");
    let (command, marker) = external_diff_command();
    git(&cwd, &["config", "diff.external", &command]);
    assert_eq!(
        git(&cwd, &["diff", "HEAD", "--", "file.txt"]),
        "CUSTOM EXTERNAL DIFF\n"
    );
    assert!(exists(&marker));
    std::fs::remove_file(&marker).unwrap();
    (cwd, command, marker)
}

#[tokio::test]
async fn returns_git_patch_hunks_when_a_repository_configures_an_external_diff_command() {
    let (cwd, command, marker) = external_diff_repo();
    let result = diff(&cwd, &structured_compare(DiffMode::Uncommitted, None)).await;
    expect_standard_patch(&result);
    assert!(!exists(&marker));
    assert_eq!(
        git_trim(&cwd, &["config", "--get", "diff.external"]),
        command
    );
}

#[tokio::test]
async fn returns_git_patch_hunks_against_the_base_branch_with_an_external_diff_command() {
    let (cwd, command, marker) = external_diff_repo();
    git(&cwd, &["checkout", "-b", "feature"]);
    git(&cwd, &["add", "."]);
    commit(&cwd, "edit");
    let result = diff(&cwd, &structured_compare(DiffMode::Base, Some("main"))).await;
    expect_standard_patch(&result);
    assert!(!exists(&marker));
    assert_eq!(
        git_trim(&cwd, &["config", "--get", "diff.external"]),
        command
    );
}

async fn patch_format_case(config: &str) {
    // uncommitted
    let cwd = created_changed_repo(config);
    let files = diff(&cwd, &structured_compare(DiffMode::Uncommitted, None))
        .await
        .structured
        .unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        (
            files[0].path.as_str(),
            files[0].additions,
            files[0].deletions
        ),
        ("file.txt", 1, 1)
    );
    let l: Vec<_> = files[0]
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter().map(|l| (l.kind, l.content.as_str())))
        .collect();
    assert!(
        l.contains(&(DiffLineType::Remove, "before")) && l.contains(&(DiffLineType::Add, "after"))
    );

    // base branch
    let cwd = created_changed_repo(config);
    git(&cwd, &["checkout", "-b", "feature"]);
    git(&cwd, &["add", "."]);
    commit(&cwd, "edit");
    let files = diff(&cwd, &structured_compare(DiffMode::Base, Some("main")))
        .await
        .structured
        .unwrap();
    assert_eq!(
        (
            files[0].path.as_str(),
            files[0].additions,
            files[0].deletions
        ),
        ("file.txt", 1, 1)
    );
    assert!(files[0]
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .any(|l| l.kind == DiffLineType::Add && l.content == "after"));

    // individual commit file
    let cwd = created_changed_repo(config);
    git(&cwd, &["add", "."]);
    commit(&cwd, "edit");
    let sha = git_trim(&cwd, &["rev-parse", "HEAD"]);
    let file = get_commit_file_diff(&cwd, &sha, "file.txt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (file.path.as_str(), file.additions, file.deletions),
        ("file.txt", 1, 1)
    );
    assert!(file
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .any(|l| l.kind == DiffLineType::Add && l.content == "after"));
}

#[tokio::test]
async fn patch_format_with_mnemonic_prefix() {
    patch_format_case("diff.mnemonicPrefix=true").await;
}

#[tokio::test]
async fn patch_format_with_forced_color() {
    patch_format_case("color.diff=always").await;
}
