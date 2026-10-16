mod common;

use common::*;
use paseo_git::diff::{get_checkout_diff, CheckoutDiffCompare, DiffMode};
use paseo_git::diff_parse::DiffFileStatus;
use paseo_git::file_contents::read_git_file_contents;
use paseo_git::repo::CheckoutContext;
use paseo_git::run::{run_git_bytes, start_git_command_metrics, GitOptions};
use paseo_git::status::{get_checkout_status, CheckoutStatusResult};
use std::collections::HashMap;

fn empty_git_dir() -> (tempfile::TempDir, String) {
    let (t, dir) = temp_dir();
    git(&dir, &["init", "-q"]);
    (t, dir)
}

#[tokio::test]
async fn batch_byte_framing_survives_unicode_invalid_utf8_and_missing_objects() {
    let (_t, cwd) = empty_git_dir();
    let mut text = "中文👩‍💻\n".as_bytes().to_vec();
    text.extend([0xff, b'\n']);
    let oid = String::from_utf8(
        run_git_bytes(
            &["hash-object", "-w", "--stdin"],
            &GitOptions::new(&cwd).input(text.clone()),
        )
        .await
        .unwrap()
        .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let empty = String::from_utf8(
        run_git_bytes(
            &["hash-object", "-w", "--stdin"],
            &GitOptions::new(&cwd).input(vec![]),
        )
        .await
        .unwrap()
        .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let contents = read_git_file_contents(
        &cwd,
        &[
            oid.clone(),
            "HEAD:missing".into(),
            empty.clone(),
            oid.clone(),
        ],
    )
    .await
    .unwrap()
    .unwrap();
    let expected: HashMap<String, Option<String>> = HashMap::from([
        (
            oid.clone(),
            Some(String::from_utf8_lossy(&text).into_owned()),
        ),
        ("HEAD:missing".to_string(), None),
        (empty, Some(String::new())),
    ]);
    assert_eq!(contents, expected);
    let raw = run_git_bytes(&["cat-file", "blob", &oid], &GitOptions::new(&cwd))
        .await
        .unwrap();
    assert_eq!(raw.stdout, text);
}

#[tokio::test]
async fn oversized_batch_output_stays_bounded_and_requests_the_per_file_path() {
    let (_t, cwd) = empty_git_dir();
    write(format!("{cwd}/big"), vec![b'a'; 20 * 1024 * 1024]);
    let oid = git_trim(&cwd, &["hash-object", "-w", "big"]);
    assert!(read_git_file_contents(&cwd, std::slice::from_ref(&oid))
        .await
        .unwrap()
        .is_none());
    let limited = run_git_bytes(
        &["cat-file", "--batch"],
        &GitOptions::new(&cwd)
            .input(format!("{oid}\n").into_bytes())
            .max_output(100),
    )
    .await
    .unwrap();
    assert!(limited.truncated);
    assert_eq!(limited.stdout.len(), 100);
}

#[tokio::test]
async fn newline_paths_do_not_become_extra_batch_requests() {
    let (_t, cwd) = empty_git_dir();
    assert!(read_git_file_contents(&cwd, &["HEAD:with\nnewline".into()])
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        read_git_file_contents(&cwd, &[])
            .await
            .unwrap()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn resolves_the_worktree_root_from_a_nested_real_git_checkout() {
    let (_t, root) = temp_dir();
    let nested = format!("{root}/packages/server/src");
    std::fs::create_dir_all(&nested).unwrap();
    git(&root, &["init"]);
    let CheckoutStatusResult::Git(status) =
        get_checkout_status(&nested, &CheckoutContext::default())
            .await
            .unwrap()
    else {
        panic!("expected a git checkout");
    };
    assert_eq!(real(&status.repo_root), real(&root));
}

fn repo_with_tracked_changes(count: usize) -> Fixture {
    let f = init_repo();
    for i in 0..count {
        write(format!("{}/file-{i}.txt", f.repo), format!("before-{i}\n"));
    }
    git(&f.repo, &["add", "."]);
    commit(&f.repo, "add files");
    for i in 0..count {
        write(format!("{}/file-{i}.txt", f.repo), format!("after-{i}\n"));
    }
    f
}

#[tokio::test]
async fn collects_ordinary_tracked_patches_with_one_bounded_git_command() {
    let f = repo_with_tracked_changes(20);
    let metrics = start_git_command_metrics();
    let compare = CheckoutDiffCompare {
        include_structured: Some(false),
        ..CheckoutDiffCompare::uncommitted()
    };
    let r = get_checkout_diff(&f.repo, &compare, &CheckoutContext::default())
        .await
        .unwrap();
    assert!(r.diff.contains("file-0.txt") && r.diff.contains("file-19.txt"));
    let commands = metrics.stop();
    let patches = commands
        .iter()
        .filter(|c| c.cwd == f.repo && c.args[0] == "diff" && c.args.iter().any(|a| a == "--"))
        .count();
    assert_eq!(patches, 1);
}

#[tokio::test]
async fn base_diffs_read_every_file_without_per_file_show_commands() {
    let f = repo_with_tracked_changes(20);
    git(&f.repo, &["checkout", "-b", "feature"]);
    git(&f.repo, &["add", "."]);
    commit(&f.repo, "change");
    let metrics = start_git_command_metrics();
    let compare = CheckoutDiffCompare {
        mode: DiffMode::Base,
        base_ref: Some("main".into()),
        ignore_whitespace: None,
        include_structured: Some(true),
    };
    let r = get_checkout_diff(&f.repo, &compare, &CheckoutContext::default())
        .await
        .unwrap();
    let commands = metrics.stop();
    let files = r.structured.unwrap();
    assert_eq!(files.len(), 20);
    assert!(files.iter().all(|f| f.additions == 1 && f.deletions == 1));
    assert_eq!(
        commands
            .iter()
            .filter(|c| c.cwd == f.repo && c.args[0] == "show")
            .count(),
        0
    );
}

#[tokio::test]
async fn keeps_small_neighbors_when_a_patch_exceeds_the_entire_batch_budget() {
    let f = repo_with_tracked_changes(20);
    write(
        format!("{}/file-0.txt", f.repo),
        "x".repeat(9 * 1024 * 1024) + "\n",
    );
    let compare = CheckoutDiffCompare {
        include_structured: Some(true),
        ..CheckoutDiffCompare::uncommitted()
    };
    let r = get_checkout_diff(&f.repo, &compare, &CheckoutContext::default())
        .await
        .unwrap();
    let files = r.structured.as_ref().unwrap();
    assert_eq!(files.len(), 20);
    let big = files.iter().find(|f| f.path == "file-0.txt").unwrap();
    assert_eq!(big.status, Some(DiffFileStatus::TooLarge));
    assert!(big.hunks.is_empty());
    assert_eq!(
        files
            .iter()
            .filter(|f| f.status == Some(DiffFileStatus::Ok))
            .count(),
        19
    );
    assert!(r.diff.contains("+after-19"));
    assert!(!r.diff.contains(&"x".repeat(1_000)));
}
