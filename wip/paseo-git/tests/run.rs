//! Runner behavior. Lives in its own binary because it reconfigures the
//! process-wide admission policy.

mod common;

use std::time::{Duration, Instant};

use common::*;
use paseo_git::error::GitError;
use paseo_git::run::{
    configure_git_process_policy, run_git, run_git_bytes, run_with_git_command_priority, GitOptions,
};
use paseo_git::scheduler::{GitProcessPolicy, GitProcessPriority};

fn configure_fsmonitor(repo: &str) -> String {
    let hook = format!("{repo}/fsmonitor-hook.sh");
    let marker = format!("{hook}.marker");
    write(&hook, "#!/bin/sh\n: > \"$0.marker\"\nprintf '1\\n'\n");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    git(repo, &["config", "core.fsmonitor", &hook]);
    marker
}

#[tokio::test]
async fn does_not_execute_a_repository_configured_fsmonitor_command() {
    let f = init_repo();
    let marker = configure_fsmonitor(&f.repo);
    run_git(&["status", "--porcelain"], &GitOptions::new(&f.repo))
        .await
        .unwrap();
    assert!(!exists(marker));
}

#[tokio::test]
async fn overrides_core_fsmonitor_for_commands_other_than_status() {
    let f = init_repo();
    configure_fsmonitor(&f.repo);
    let out = run_git(
        &["config", "--get", "core.fsmonitor"],
        &GitOptions::new(&f.repo),
    )
    .await
    .unwrap();
    assert_eq!(out.stdout.trim(), "false");
}

#[tokio::test]
async fn resolves_truncated_stdout_caps_output_and_kills_the_child() {
    let f = init_repo();
    write(format!("{}/big", f.repo), vec![b'a'; 4 * 1024 * 1024]);
    let oid = git_trim(&f.repo, &["hash-object", "-w", "big"]);
    let started = Instant::now();
    let out = run_git_bytes(
        &["cat-file", "--batch"],
        &GitOptions::new(&f.repo)
            .input(format!("{oid}\n").into_bytes())
            .max_output(100),
    )
    .await
    .unwrap();
    assert!(out.truncated);
    assert_eq!(out.stdout.len(), 100);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn rejects_non_zero_exit_codes_that_are_not_accepted() {
    let f = init_repo();
    let err = run_git(
        &["rev-parse", "--verify", "--quiet", "refs/heads/nope"],
        &GitOptions::new(&f.repo),
    )
    .await
    .unwrap_err();
    assert_eq!(err.to_string(), "Git command failed: git rev-parse --verify --quiet refs/heads/nope (exit code: 1, signal: none)\n(no stderr)");
    let err = run_git(&["show", "deadbeef"], &GitOptions::new(&f.repo).read_only())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, GitError::Command { stderr, .. } if stderr.contains("unknown revision") || stderr.contains("bad object")),
        "{err}"
    );
    assert!(
        err.to_string().starts_with(
            "Git command failed: git show deadbeef (exit code: 128, signal: none)\nfatal:"
        ),
        "{err}"
    );
    // the next command still runs
    run_git(&["status"], &GitOptions::new(&f.repo))
        .await
        .unwrap();
}

#[tokio::test]
async fn resolves_accepted_non_zero_exit_codes() {
    let f = init_repo();
    let out = run_git(
        &["rev-parse", "--verify", "--quiet", "refs/heads/nope"],
        &GitOptions::new(&f.repo).accept(&[0, 1]),
    )
    .await
    .unwrap();
    assert_eq!(
        (out.exit_code, out.signal, out.truncated),
        (Some(1), None, false)
    );
}

#[tokio::test]
async fn kills_timed_out_processes_and_reports_the_command() {
    let f = init_repo();
    let started = Instant::now();
    let err = run_git(
        &["-c", "alias.sleepy=!sleep 5", "sleepy"],
        &GitOptions::new(&f.repo).timeout_ms(200),
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Git command timed out after 200ms: git -c alias.sleepy=!sleep 5 sleepy"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    run_git(&["status"], &GitOptions::new(&f.repo))
        .await
        .unwrap();
}

#[tokio::test]
async fn rejects_spawn_failures_for_a_missing_cwd() {
    let f = init_repo();
    let err = run_git(&["status"], &GitOptions::new(f.path("does-not-exist")))
        .await
        .unwrap_err();
    assert!(matches!(err, GitError::Other(_)), "{err:?}");
}

#[tokio::test]
async fn read_only_env_forces_the_c_locale() {
    let f = init_repo();
    let err = run_git(
        &["show", "nonexistent-ref-x"],
        &GitOptions::new(&f.repo).read_only(),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("ambiguous argument 'nonexistent-ref-x'"),
        "{err}"
    );
}

#[tokio::test]
async fn stdout_is_decoded_lossily_like_node() {
    let f = init_repo();
    let oid = {
        let out = run_git_bytes(
            &["hash-object", "-w", "--stdin"],
            &GitOptions::new(&f.repo).input(vec![b'a', 0xff, b'b']),
        )
        .await
        .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    let text = run_git(&["cat-file", "blob", &oid], &GitOptions::new(&f.repo))
        .await
        .unwrap();
    assert_eq!(text.stdout, "a\u{fffd}b");
    let raw = run_git_bytes(&["cat-file", "blob", &oid], &GitOptions::new(&f.repo))
        .await
        .unwrap();
    assert_eq!(raw.stdout, vec![b'a', 0xff, b'b']);
}

// Keep this last: it changes the global policy for the rest of the binary.
#[tokio::test]
async fn throttles_concurrent_git_commands_and_prefers_high_priority() {
    let f = init_repo();
    configure_git_process_policy(GitProcessPolicy {
        max_processes_per_second: 100,
        max_process_concurrency: 2,
    });
    let started = Instant::now();
    let tasks: Vec<_> = (0..6)
        .map(|_| {
            let repo = f.repo.clone();
            tokio::spawn(async move {
                run_git(
                    &["-c", "alias.nap=!sleep 0.3", "nap"],
                    &GitOptions::new(&repo),
                )
                .await
                .unwrap()
            })
        })
        .collect();
    for t in tasks {
        t.await.unwrap();
    }
    // 6 commands of ~300ms at concurrency 2 need at least 3 waves.
    assert!(
        started.elapsed() >= Duration::from_millis(850),
        "{:?}",
        started.elapsed()
    );

    // High priority jumps the queue.
    let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let blockers: Vec<_> = (0..2)
        .map(|_| {
            let repo = f.repo.clone();
            tokio::spawn(async move {
                run_git(
                    &["-c", "alias.nap=!sleep 0.4", "nap"],
                    &GitOptions::new(&repo),
                )
                .await
                .unwrap()
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (o1, r1) = (order.clone(), f.repo.clone());
    let normal = tokio::spawn(async move {
        run_git(&["status"], &GitOptions::new(&r1)).await.unwrap();
        o1.lock().unwrap().push("normal");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (o2, r2) = (order.clone(), f.repo.clone());
    let high = tokio::spawn(async move {
        run_with_git_command_priority(GitProcessPriority::High, async {
            run_git(&["status"], &GitOptions::new(&r2)).await.unwrap();
        })
        .await;
        o2.lock().unwrap().push("high");
    });
    for b in blockers {
        b.await.unwrap();
    }
    normal.await.unwrap();
    high.await.unwrap();
    assert_eq!(order.lock().unwrap()[0], "high");
}
