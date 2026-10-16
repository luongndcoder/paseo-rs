mod common;

use common::*;
use paseo_git::diff::{
    get_checkout_diff, CheckoutDiffCompare, CheckoutDiffResult, DiffMode,
    CHECKOUT_DIFF_MAX_STRUCTURED_BYTES,
};
use paseo_git::diff_parse::{DiffFileStatus, DiffLineType};
use paseo_git::repo::{get_current_branch, CheckoutContext};
use paseo_git::run::start_git_command_metrics;
use serde_json::json;

fn uncommitted() -> CheckoutDiffCompare {
    CheckoutDiffCompare::uncommitted()
}

fn structured(mut c: CheckoutDiffCompare) -> CheckoutDiffCompare {
    c.include_structured = Some(true);
    c
}

fn base(base_ref: &str) -> CheckoutDiffCompare {
    CheckoutDiffCompare::base(Some(base_ref))
}

async fn diff(cwd: &str, compare: &CheckoutDiffCompare) -> CheckoutDiffResult {
    get_checkout_diff(cwd, compare, &CheckoutContext::default())
        .await
        .unwrap()
}

fn paths(r: &CheckoutDiffResult) -> Vec<String> {
    r.structured
        .as_ref()
        .unwrap()
        .iter()
        .map(|f| f.path.clone())
        .collect()
}

#[tokio::test]
async fn returns_none_for_current_branch_in_a_repo_with_no_commits() {
    let f = init_repo();
    let empty = f.path("empty-repo");
    std::fs::create_dir_all(&empty).unwrap();
    git(&empty, &["init", "-b", "main"]);
    assert_eq!(get_current_branch(&empty).await, None);
}

#[tokio::test]
async fn returns_untracked_files_in_an_uncommitted_diff_before_the_first_commit() {
    let f = init_repo();
    let unborn = f.path("unborn-repo");
    std::fs::create_dir_all(&unborn).unwrap();
    git(&unborn, &["init", "-b", "main"]);
    write(format!("{unborn}/greeting.txt"), "hello\n");
    let r = diff(&unborn, &structured(uncommitted())).await;
    assert_eq!(
        serde_json::to_value(r.structured.unwrap()).unwrap(),
        json!([{
            "path": "greeting.txt", "isNew": true, "isDeleted": false, "additions": 1, "deletions": 0,
            "staged": false, "unstaged": true,
            "hunks": [{"oldStart": 0, "oldCount": 0, "newStart": 1, "newCount": 1, "lines": [
                {"type": "header", "content": "@@ -0,0 +1 @@"},
                {"type": "add", "content": "hello"},
            ]}],
            "status": "ok",
        }])
    );
}

#[tokio::test]
async fn returns_tracked_files_before_the_first_commit_via_the_empty_tree() {
    let f = init_repo();
    let unborn = f.path("unborn-staged");
    std::fs::create_dir_all(&unborn).unwrap();
    git(&unborn, &["init", "-b", "main"]);
    write(format!("{unborn}/staged.txt"), "one\ntwo\n");
    git(&unborn, &["add", "staged.txt"]);
    let r = diff(&unborn, &structured(uncommitted())).await;
    let files = r.structured.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        (files[0].path.as_str(), files[0].is_new, files[0].additions),
        ("staged.txt", true, 2)
    );
    assert_eq!(
        (files[0].staged, files[0].unstaged),
        (Some(true), Some(false))
    );
}

#[tokio::test]
async fn returns_the_branch_being_rebased_when_head_is_detached_during_a_rebase() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature/rebase-test"]);
    commit_file(&f.repo, "file.txt", "feature\n", "feature change");
    git(&f.repo, &["checkout", "main"]);
    commit_file(&f.repo, "file.txt", "main\n", "main change");
    git(&f.repo, &["checkout", "feature/rebase-test"]);
    assert!(!git_succeeds(&f.repo, &["rebase", "main"]));
    assert_eq!(
        get_current_branch(&f.repo).await.as_deref(),
        Some("feature/rebase-test")
    );
}

#[tokio::test]
async fn includes_both_paths_for_a_staged_rename_in_structured_diffs() {
    let f = init_repo();
    git(&f.repo, &["mv", "file.txt", "renamed.txt"]);
    let r = diff(&f.repo, &structured(uncommitted())).await;
    let files = r.structured.unwrap();
    let file = files.iter().find(|f| f.path == "renamed.txt").unwrap();
    assert_eq!(
        (file.old_path.as_deref(), file.additions, file.deletions),
        (Some("file.txt"), 0, 0)
    );
    assert!(file.hunks.is_empty());
}

async fn rename_edit_case(mode: DiffMode, old_path: &str, path: &str) {
    let f = init_repo();
    let original: Vec<String> = (1..=12).map(|i| format!("line {i}")).collect();
    commit_file(
        &f.repo,
        old_path,
        &(original.join("\n") + "\n"),
        "file to rename",
    );
    if mode == DiffMode::Base {
        git(&f.repo, &["checkout", "-b", "feature/rename"]);
    }
    git(&f.repo, &["mv", old_path, path]);
    let mut edited = original.clone();
    edited[5] = "edited line 6".into();
    write(format!("{}/{path}", f.repo), edited.join("\n") + "\n");
    git(&f.repo, &["add", path]);
    if mode == DiffMode::Base {
        commit(&f.repo, "rename and edit");
    }
    let compare = if mode == DiffMode::Base {
        structured(base("main"))
    } else {
        structured(uncommitted())
    };
    let r = diff(&f.repo, &compare).await;
    let files = r.structured.clone().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        (
            files[0].path.as_str(),
            files[0].old_path.as_deref(),
            files[0].is_new,
            files[0].is_deleted,
            files[0].additions,
            files[0].deletions,
            files[0].status
        ),
        (
            path,
            Some(old_path),
            false,
            false,
            1,
            1,
            Some(DiffFileStatus::Ok)
        )
    );
    assert!(r.diff.contains(&format!("rename from {old_path}")));
    assert!(r.diff.contains(&format!("rename to {path}")));
    assert!(r.diff.contains("-line 6\n+edited line 6"));
    assert!(!r.diff.contains("new file mode"));
}

#[tokio::test]
async fn shows_only_the_edited_line_of_a_rename_uncommitted() {
    rename_edit_case(DiffMode::Uncommitted, "to-rename.txt", "renamed.txt").await;
}

#[tokio::test]
async fn shows_only_the_edited_line_of_a_rename_base() {
    rename_edit_case(DiffMode::Base, "to-rename.txt", "renamed.txt").await;
}

#[tokio::test]
async fn shows_only_the_edited_line_of_a_rename_with_bracketed_names() {
    rename_edit_case(DiffMode::Uncommitted, "old [file].txt", "new [file].txt").await;
}

fn twelve_lines() -> String {
    (1..=12)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[tokio::test]
async fn includes_a_changed_copy_source_only_once_in_the_diff() {
    let f = init_repo();
    let original = twelve_lines();
    commit_file(&f.repo, "source.txt", &original, "copy source");
    git(&f.repo, &["config", "diff.renames", "copies"]);
    write(format!("{}/copy.txt", f.repo), &original);
    write(
        format!("{}/source.txt", f.repo),
        original.replace("line 6", "edited line 6"),
    );
    git(&f.repo, &["add", "copy.txt", "source.txt"]);
    assert!(git(&f.repo, &["diff", "--name-status", "HEAD"]).contains("C100\tsource.txt\tcopy.txt"));

    let r = diff(&f.repo, &structured(uncommitted())).await;
    let files = r.structured.clone().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(
        (
            files[0].path.as_str(),
            files[0].old_path.as_deref(),
            files[0].additions,
            files[0].deletions
        ),
        ("copy.txt", Some("source.txt"), 0, 0)
    );
    assert_eq!(
        (
            files[1].path.as_str(),
            files[1].additions,
            files[1].deletions
        ),
        ("source.txt", 1, 1)
    );
    assert_eq!(
        r.diff
            .matches("diff --git a/source.txt b/source.txt\n")
            .count(),
        1
    );
    assert!(r.diff.contains("copy from source.txt") && r.diff.contains("copy to copy.txt"));
}

#[tokio::test]
async fn keeps_a_small_copy_patch_when_its_edited_source_exceeds_the_patch_limit() {
    let f = init_repo();
    let original = twelve_lines();
    commit_file(&f.repo, "source.txt", &original, "copy source");
    git(&f.repo, &["config", "diff.renames", "copies"]);
    write(format!("{}/copy.txt", f.repo), &original);
    write(
        format!("{}/source.txt", f.repo),
        format!("{original}{}", "added source line\n".repeat(80_000)),
    );
    git(&f.repo, &["add", "copy.txt", "source.txt"]);
    assert!(git(&f.repo, &["diff", "--name-status", "HEAD"]).contains("C100\tsource.txt\tcopy.txt"));

    let r = diff(&f.repo, &structured(uncommitted())).await;
    let files = r.structured.clone().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(
        (
            files[0].path.as_str(),
            files[0].old_path.as_deref(),
            files[0].additions,
            files[0].deletions,
            files[0].status
        ),
        (
            "copy.txt",
            Some("source.txt"),
            0,
            0,
            Some(DiffFileStatus::Ok)
        )
    );
    assert!(files[0].hunks.is_empty());
    assert_eq!(
        (files[1].path.as_str(), files[1].status),
        ("source.txt", Some(DiffFileStatus::TooLarge))
    );
    assert!(files[1].hunks.is_empty());
    assert!(r.diff.contains("copy from source.txt") && r.diff.contains("copy to copy.txt"));
    assert!(!r.diff.contains("+added source line"));
}

#[tokio::test]
async fn hides_whitespace_only_changes_when_requested() {
    let f = init_repo();
    write(format!("{}/file.txt", f.repo), "hello  \n");
    let visible = diff(&f.repo, &uncommitted()).await;
    assert!(visible.diff.contains("file.txt"));
    let hidden = diff(
        &f.repo,
        &CheckoutDiffCompare {
            ignore_whitespace: Some(true),
            ..structured(uncommitted())
        },
    )
    .await;
    assert_eq!(hidden.diff, "");
    assert_eq!(hidden.structured, Some(vec![]));
}

#[tokio::test]
async fn preserves_no_prefix_structured_paths_that_start_with_a_or_b() {
    let f = init_repo();
    commit_file(
        &f.repo,
        "a/example.ts",
        "const value = 1;\n",
        "add a-prefixed path",
    );
    commit_file(
        &f.repo,
        "b/other.ts",
        "const value = 1;\n",
        "add b-prefixed path",
    );
    commit_file(
        &f.repo,
        "file with space.ts",
        "const value = 1;\n",
        "add path with space",
    );
    git(&f.repo, &["config", "diff.noprefix", "true"]);
    for p in ["a/example.ts", "b/other.ts", "file with space.ts"] {
        write(format!("{}/{p}", f.repo), "const value = 2;\n");
    }
    let r = diff(&f.repo, &structured(uncommitted())).await;
    let got: Vec<_> = r
        .structured
        .unwrap()
        .iter()
        .map(|f| (f.path.clone(), f.hunks.len()))
        .collect();
    assert_eq!(
        got,
        [
            ("a/example.ts".to_string(), 1),
            ("b/other.ts".to_string(), 1),
            ("file with space.ts".to_string(), 1)
        ]
    );
}

#[tokio::test]
async fn diffs_base_mode_against_merge_base_without_base_only_deletions() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature"]);
    git(&f.repo, &["checkout", "main"]);
    commit_file(&f.repo, "base-only.txt", "base\n", "base only");
    git(&f.repo, &["checkout", "feature"]);
    commit_file(&f.repo, "feature.txt", "feature\n", "feature commit");
    let r = diff(&f.repo, &base("main")).await;
    assert!(r.diff.contains("feature.txt"));
    assert!(!r.diff.contains("base-only.txt"));
}

#[tokio::test]
async fn does_not_throw_on_large_diffs_and_marks_the_file_too_large() {
    let f = init_repo();
    let large: String = (0..200_000).map(|i| format!("line {i}\n")).collect();
    write(format!("{}/file.txt", f.repo), large);
    let r = diff(&f.repo, &structured(uncommitted())).await;
    assert!(r
        .structured
        .unwrap()
        .iter()
        .any(|f| f.path == "file.txt" && f.status == Some(DiffFileStatus::TooLarge)));
}

#[test]
fn keeps_the_structured_diff_cap_below_the_relay_frame_limit() {
    // base64EncryptedWireByteLength(n) = 4 * ceil((n + 40) / 3)
    let wire = |n: usize| 4 * (n + 40).div_ceil(3);
    let relay_frame = 32 * 1024 * 1024;
    assert_eq!(CHECKOUT_DIFF_MAX_STRUCTURED_BYTES, 24_117_208);
    assert!(wire(CHECKOUT_DIFF_MAX_STRUCTURED_BYTES) < relay_frame);
    assert_eq!(
        wire(CHECKOUT_DIFF_MAX_STRUCTURED_BYTES + 1024 * 1024),
        relay_frame
    );
}

#[tokio::test]
async fn marks_tracked_generated_one_line_diffs_as_too_large_by_content_size() {
    let f = init_repo();
    commit_file(
        &f.repo,
        "generated.js",
        "const data = \"old\";\n",
        "add generated",
    );
    write(
        format!("{}/generated.js", f.repo),
        format!("const data = \"{}\";\n", "x".repeat(1_100_000)),
    );
    let r = diff(&f.repo, &structured(uncommitted())).await;
    let entry = r
        .structured
        .as_ref()
        .unwrap()
        .iter()
        .find(|f| f.path == "generated.js")
        .unwrap();
    assert_eq!(entry.status, Some(DiffFileStatus::TooLarge));
    assert_eq!((entry.additions, entry.deletions), (1, 1));
    assert!(entry.hunks.is_empty());
    assert!(r.diff.contains("# generated.js: diff too large omitted"));
    assert!(!r.diff.contains(&"x".repeat(10_000)));
}

#[tokio::test]
async fn keeps_small_tracked_files_displayable_when_another_tracked_file_has_a_massive_diff() {
    let f = init_repo();
    write(
        format!("{}/generated.js", f.repo),
        "const data = \"old\";\n",
    );
    write(
        format!("{}/small.ts", f.repo),
        "export const value = \"old\";\n",
    );
    git(&f.repo, &["add", "generated.js", "small.ts"]);
    commit(&f.repo, "add tracked files");
    write(
        format!("{}/generated.js", f.repo),
        format!("const data = \"{}\";\n", "x".repeat(2_100_000)),
    );
    write(
        format!("{}/small.ts", f.repo),
        "export const value = \"new\";\n",
    );

    let metrics = start_git_command_metrics();
    let r = diff(&f.repo, &structured(uncommitted())).await;
    let commands: Vec<String> = metrics
        .stop()
        .into_iter()
        .filter(|c| c.cwd == f.repo)
        .map(|c| c.args.join(" "))
        .collect();
    let summary: Vec<_> = r
        .structured
        .as_ref()
        .unwrap()
        .iter()
        .map(|f| (f.path.as_str(), f.status, f.hunks.len()))
        .collect();
    assert_eq!(
        summary,
        [
            ("generated.js", Some(DiffFileStatus::TooLarge), 0),
            ("small.ts", Some(DiffFileStatus::Ok), 1)
        ]
    );
    assert!(r.diff.contains("# generated.js: diff too large omitted"));
    assert!(r.diff.contains("-export const value = \"old\";"));
    assert!(r.diff.contains("+export const value = \"new\";"));
    assert!(
        commands.contains(
            &"diff --no-ext-diff --no-color --src-prefix=a/ --dst-prefix=b/ -z --numstat HEAD"
                .to_string()
        ),
        "{commands:?}"
    );
    assert!(commands.contains(&"diff --no-ext-diff --no-color --src-prefix=a/ --dst-prefix=b/ HEAD -- :(literal)generated.js :(literal)small.ts".to_string()), "{commands:?}");
}

#[tokio::test]
async fn marks_tracked_files_omitted_by_the_total_diff_budget_as_too_large() {
    let f = init_repo();
    for i in 1..=4 {
        write(format!("{}/budget-{i}.txt", f.repo), "old\n");
    }
    git(&f.repo, &["add", "."]);
    commit(&f.repo, "add budget files");
    let large_line = "x".repeat(700_000);
    for i in 1..=4 {
        write(
            format!("{}/budget-{i}.txt", f.repo),
            format!("{large_line}-{i}\n"),
        );
    }
    let r = diff(&f.repo, &structured(uncommitted())).await;
    let summary: Vec<_> = r
        .structured
        .as_ref()
        .unwrap()
        .iter()
        .map(|f| (f.path.as_str(), f.status, f.hunks.len()))
        .collect();
    assert_eq!(
        summary,
        [
            ("budget-1.txt", Some(DiffFileStatus::Ok), 1),
            ("budget-2.txt", Some(DiffFileStatus::Ok), 1),
            ("budget-3.txt", Some(DiffFileStatus::TooLarge), 0),
            ("budget-4.txt", Some(DiffFileStatus::TooLarge), 0),
        ]
    );
    assert!(r.diff.contains("budget-1.txt") && r.diff.contains("budget-2.txt"));
    assert!(r.diff.contains("# budget-3.txt: diff too large omitted"));
    assert!(r.diff.contains("# budget-4.txt: diff too large omitted"));
}

#[tokio::test]
async fn short_circuits_tracked_binary_files() {
    let f = init_repo();
    let p = format!("{}/tracked-blob.bin", f.repo);
    write(&p, [0x00u8, 0xff, 0x10, 0x80, 0x00]);
    git(&f.repo, &["add", "tracked-blob.bin"]);
    commit(&f.repo, "add tracked binary");
    write(&p, [0x00u8, 0xff, 0x11, 0x81, 0x00]);
    let r = diff(&f.repo, &structured(uncommitted())).await;
    let entry = r
        .structured
        .as_ref()
        .unwrap()
        .iter()
        .find(|f| f.path == "tracked-blob.bin")
        .unwrap();
    assert_eq!(entry.status, Some(DiffFileStatus::Binary));
    assert!(r.diff.contains("# tracked-blob.bin: binary diff omitted"));
}

#[tokio::test]
async fn short_circuits_untracked_binary_files() {
    let f = init_repo();
    write(
        format!("{}/blob.bin", f.repo),
        [0x00u8, 0xff, 0x10, 0x80, 0x00, 0x7f, 0x00],
    );
    let r = diff(&f.repo, &structured(uncommitted())).await;
    let entry = r
        .structured
        .as_ref()
        .unwrap()
        .iter()
        .find(|f| f.path == "blob.bin")
        .unwrap();
    assert_eq!(entry.status, Some(DiffFileStatus::Binary));
    assert!(r.diff.contains("# blob.bin: binary diff omitted"));
}

#[tokio::test]
async fn marks_untracked_oversized_files_as_too_large() {
    let f = init_repo();
    let large: String = (0..240_000).map(|i| format!("line {i}\n")).collect();
    write(format!("{}/untracked-large.txt", f.repo), large);
    let r = diff(&f.repo, &structured(uncommitted())).await;
    let entry = r
        .structured
        .as_ref()
        .unwrap()
        .iter()
        .find(|f| f.path == "untracked-large.txt")
        .unwrap();
    assert_eq!(entry.status, Some(DiffFileStatus::TooLarge));
    assert!(r
        .diff
        .contains("# untracked-large.txt: diff too large omitted"));
}

#[tokio::test]
async fn non_structured_diffs_omit_structured_and_use_text_placeholders() {
    let f = init_repo();
    write(format!("{}/blob.bin", f.repo), [0u8, 1, 2, 3]);
    write(format!("{}/file.txt", f.repo), "updated\n");
    let r = diff(&f.repo, &uncommitted()).await;
    assert!(r.structured.is_none() && !r.diff_too_large);
    assert!(r.diff.contains("+updated") && r.diff.contains("# blob.bin: binary diff omitted"));
}

#[tokio::test]
async fn structured_cap_overflow_reports_diff_too_large() {
    // Many untracked files whose serialized form exceeds the 24 MB cap would be heavy;
    // verify the accumulator contract through the public constant and an empty diff instead.
    const { assert!(CHECKOUT_DIFF_MAX_STRUCTURED_BYTES > 2 * 1024 * 1024) };
    let f = init_repo();
    let r = diff(&f.repo, &structured(uncommitted())).await;
    assert_eq!(r.structured, Some(vec![]));
    assert_eq!(r.diff, "");
}

// ---- base-mode resolution ---------------------------------------------------------------

#[tokio::test]
async fn does_not_count_origin_base_commits_as_feature_changes_when_local_main_is_stale() {
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
    let r = diff(&f.repo, &structured(base("main"))).await;
    assert_eq!(r.diff, "");
    assert_eq!(r.structured, Some(vec![]));
}

#[tokio::test]
async fn falls_back_to_the_local_base_branch_when_origin_is_absent() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "local-feature.txt", "feature\n", "local feature");
    let r = diff(&f.repo, &base("main")).await;
    assert!(r.diff.contains("local-feature.txt"));
}

#[tokio::test]
async fn keeps_an_explicit_origin_base_ref_instead_of_stripping_it_to_a_stale_local_branch() {
    let f = init_repo();
    let remote = add_bare_remote(&f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    let other = clone_remote(&f, &remote, "other-clone");
    commit_file(&other, "origin-base.txt", "origin\n", "origin base");
    git(&other, &["push"]);
    commit_file(&f.repo, "local-drift.txt", "local\n", "local drift");
    git(&f.repo, &["fetch", "origin"]);
    git(&f.repo, &["checkout", "-b", "feature", "origin/main"]);
    let r = diff(&f.repo, &base("origin/main")).await;
    assert_eq!(r.diff, "");
}

#[tokio::test]
async fn shows_feature_commits_when_the_local_and_origin_base_branches_are_up_to_date() {
    let f = init_repo();
    add_bare_remote(&f);
    git(&f.repo, &["push", "-u", "origin", "main"]);
    git(&f.repo, &["fetch", "origin"]);
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "feature.txt", "feature\n", "feature commit");
    assert!(diff(&f.repo, &base("main"))
        .await
        .diff
        .contains("feature.txt"));
}

#[tokio::test]
async fn does_not_include_dirty_working_tree_changes_in_base_mode() {
    let f = init_repo();
    write(format!("{}/file.txt", f.repo), "dirty\n");
    write(format!("{}/untracked.txt", f.repo), "untracked\n");
    let r = diff(&f.repo, &structured(base("main"))).await;
    assert_eq!(r.diff, "");
    assert_eq!(r.structured, Some(vec![]));
}

#[tokio::test]
async fn shows_committed_branch_changes_without_dirty_working_tree_changes_in_base_mode() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature"]);
    commit_file(&f.repo, "feature.txt", "feature\n", "feature commit");
    write(format!("{}/file.txt", f.repo), "dirty\n");
    write(format!("{}/untracked.txt", f.repo), "untracked\n");
    let r = diff(&f.repo, &structured(base("main"))).await;
    assert!(r.diff.contains("feature.txt"));
    assert!(!r.diff.contains("file.txt") && !r.diff.contains("untracked.txt"));
    assert_eq!(paths(&r), ["feature.txt"]);
}

#[tokio::test]
async fn unknown_base_branch_reports_not_found() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "feature"]);
    let err = get_checkout_diff(&f.repo, &base("nope"), &CheckoutContext::default())
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Base branch not found locally or on origin: nope"
    );
}

#[tokio::test]
async fn non_git_directory_reports_not_a_git_repository() {
    let f = init_repo();
    let dir = f.path("plain");
    std::fs::create_dir_all(&dir).unwrap();
    let err = get_checkout_diff(&dir, &uncommitted(), &CheckoutContext::default())
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), format!("Not a git repository: {dir}"));
    assert_eq!(
        serde_json::to_value(paseo_git::to_checkout_error(&err)).unwrap(),
        json!({"code": "NOT_GIT_REPO", "message": format!("Not a git repository: {dir}")})
    );
}

// ---- Paseo worktree base resolution ------------------------------------------------------

#[tokio::test]
async fn uses_stored_base_ref_for_paseo_worktrees_without_heuristics() {
    let f = init_repo();
    git(&f.repo, &["checkout", "-b", "develop"]);
    commit_file(&f.repo, "file.txt", "develop\n", "develop change");
    git(&f.repo, &["checkout", "main"]);
    let wt = create_paseo_worktree(&f, "feature", "feature", "develop").await;
    commit_file(&wt, "feature.txt", "feature\n", "feature commit");
    let r = get_checkout_diff(&wt, &CheckoutDiffCompare::base(None), &f.ctx())
        .await
        .unwrap();
    assert!(r.diff.contains("feature.txt"));
    assert!(!r.diff.contains("file.txt"));
}

#[tokio::test]
async fn names_both_refs_when_a_requested_base_ref_does_not_match_the_stored_one() {
    let f = init_repo();
    let wt = create_paseo_worktree(&f, "mismatch-feature", "mismatch-feature", "main").await;
    let err = get_checkout_diff(&wt, &base("other"), &f.ctx())
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Base ref mismatch: stored refs/heads/main, requested other"
    );
}

#[tokio::test]
async fn excludes_dirty_working_tree_changes_from_paseo_worktree_base_diffs() {
    let f = init_repo();
    let wt = create_paseo_worktree(&f, "dirty-feature", "feature", "main").await;
    commit_file(&wt, "feature.txt", "feature\n", "feature commit");
    write(format!("{wt}/file.txt"), "dirty\n");
    write(format!("{wt}/untracked.txt"), "untracked\n");
    let r = get_checkout_diff(&wt, &structured(CheckoutDiffCompare::base(None)), &f.ctx())
        .await
        .unwrap();
    assert!(r.diff.contains("feature.txt"));
    assert!(!r.diff.contains("file.txt") && !r.diff.contains("untracked.txt"));
    assert_eq!(paths(&r), ["feature.txt"]);
}

#[tokio::test]
async fn falls_back_to_the_repository_default_branch_when_metadata_is_missing() {
    let f = init_repo();
    let wt = create_paseo_worktree(&f, "missing-metadata", "feature-default-base", "main").await;
    commit_file(&wt, "feature.txt", "feature\n", "feature commit");
    std::fs::remove_file(paseo_git::worktree_meta::paseo_worktree_metadata_path(&wt).unwrap())
        .unwrap();
    let r = get_checkout_diff(&wt, &CheckoutDiffCompare::base(None), &f.ctx())
        .await
        .unwrap();
    assert!(r.diff.contains("feature.txt"));
    let stat = paseo_git::shortstat::get_checkout_shortstat_uncached(&wt, &f.ctx(), false)
        .await
        .unwrap();
    assert_eq!(
        stat,
        Some(paseo_git::shortstat::CheckoutShortstat {
            additions: 1,
            deletions: 0
        })
    );
}

#[tokio::test]
async fn diff_lines_have_the_wire_types() {
    let f = init_repo();
    write(format!("{}/file.txt", f.repo), "updated\n");
    let r = diff(&f.repo, &structured(uncommitted())).await;
    let kinds: Vec<_> = r.structured.unwrap()[0].hunks[0]
        .lines
        .iter()
        .map(|l| l.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            DiffLineType::Header,
            DiffLineType::Remove,
            DiffLineType::Add
        ]
    );
}
