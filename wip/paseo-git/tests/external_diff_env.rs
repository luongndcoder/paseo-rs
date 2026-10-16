//! GIT_EXTERNAL_DIFF is process-wide, so this check lives in its own test binary.

mod common;

use common::*;
use paseo_git::diff::{get_checkout_diff, CheckoutDiffCompare, DiffMode};
use paseo_git::repo::CheckoutContext;

#[tokio::test]
async fn returns_git_patch_hunks_when_git_external_diff_configures_the_external_command() {
    let f = init_empty_repo("Test");
    let cwd = f.repo.clone();
    write(format!("{cwd}/file.txt"), "before\n");
    git(&cwd, &["add", "."]);
    commit(&cwd, "initial");
    git(&cwd, &["config", "color.diff", "always"]);
    write(format!("{cwd}/file.txt"), "after\n");

    let marker = format!("{}/invoked", f.temp_dir);
    let script = format!("{}/external-diff.sh", f.temp_dir);
    write(
        &script,
        format!("#!/bin/sh\necho invoked > '{marker}'\necho 'CUSTOM EXTERNAL DIFF'\n"),
    );
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::env::set_var("GIT_EXTERNAL_DIFF", &script);
    assert_eq!(
        git(&cwd, &["diff", "HEAD", "--", "file.txt"]),
        "CUSTOM EXTERNAL DIFF\n"
    );
    assert!(exists(&marker));
    std::fs::remove_file(&marker).unwrap();

    let compare = CheckoutDiffCompare {
        mode: DiffMode::Uncommitted,
        base_ref: None,
        ignore_whitespace: None,
        include_structured: Some(true),
    };
    let result = get_checkout_diff(&cwd, &compare, &CheckoutContext::default())
        .await
        .unwrap();
    let files = result.structured.unwrap();
    assert_eq!(
        (
            files[0].path.as_str(),
            files[0].additions,
            files[0].deletions
        ),
        ("file.txt", 1, 1)
    );
    assert_eq!(files[0].hunks[0].lines[0].content, "@@ -1 +1 @@");
    assert!(!exists(&marker), "external diff must not run");
    assert_eq!(std::env::var("GIT_EXTERNAL_DIFF").unwrap(), script);
}
