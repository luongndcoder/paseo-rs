//! Real-process lifecycle: process-group cleanup and the pipe-drain grace. Unix only (/bin/sh).
#![cfg(unix)]

use std::time::{Duration, Instant};

use paseo_claude::{
    ExitInfo, ProcessTransport, SpawnConfig, Transport, TransportEvent, TransportIo,
};

fn start(script: &str) -> TransportIo {
    let cfg = SpawnConfig {
        command: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        ..SpawnConfig::default()
    };
    Box::new(ProcessTransport::new(cfg))
        .start()
        .expect("spawn sh")
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn wait_dead(pid: i32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !alive(pid)
}

/// Reads events until Exit; returns the stdout lines and the exit info.
async fn drain(io: &mut TransportIo, limit: Duration) -> (Vec<String>, ExitInfo) {
    let mut lines = Vec::new();
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            ev = io.events.recv() => match ev {
                Some(TransportEvent::Line(l)) => lines.push(l),
                Some(TransportEvent::Exit(info)) => return (lines, info),
                None => panic!("channel closed without Exit; lines: {lines:?}"),
            },
            () = &mut deadline => panic!("no Exit within {limit:?}; lines: {lines:?}"),
        }
    }
}

fn pid_of(lines: &[String]) -> i32 {
    lines
        .first()
        .and_then(|l| l.trim().parse().ok())
        .expect("first line is orphan pid")
}

#[tokio::test]
async fn orphan_that_handles_sigterm_is_reaped_and_exit_is_prompt() {
    // Leader exits at once; the background sleep inherits stdout/stderr but dies on SIGTERM.
    let mut io = start("sleep 300 & echo $!; exit 3");
    let t0 = Instant::now();
    let (lines, info) = drain(&mut io, Duration::from_secs(10)).await;
    assert_eq!(info.code, Some(3));
    assert!(
        t0.elapsed() < Duration::from_millis(1500),
        "Exit held back: {:?}",
        t0.elapsed()
    );
    assert!(
        wait_dead(pid_of(&lines), Duration::from_secs(3)).await,
        "orphan survived SIGTERM"
    );
}

#[tokio::test]
async fn orphan_ignoring_sigterm_holds_pipes_then_exit_after_grace_and_sigkill() {
    let mut io = start("trap '' TERM; sleep 300 & echo $!; exit 0");
    let t0 = Instant::now();
    let (lines, info) = drain(&mut io, Duration::from_secs(10)).await;
    let waited = t0.elapsed();
    let pid = pid_of(&lines);
    assert!(info.success());
    // Pipes stay open (orphan ignores SIGTERM), so Exit comes from PIPE_DRAIN_GRACE (2s).
    assert!(
        waited >= Duration::from_millis(1800),
        "Exit before grace: {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(4),
        "Exit far past grace: {waited:?}"
    );
    assert!(
        alive(pid),
        "orphan should still be alive until KILL_ESCALATION"
    );
    // KILL_ESCALATION (5s after leader exit) sends SIGKILL to the group.
    assert!(
        wait_dead(pid, Duration::from_secs(8)).await,
        "orphan survived SIGKILL escalation"
    );
}

#[tokio::test]
async fn kill_terminates_whole_process_group() {
    let mut io = start("sleep 300 & echo $!; wait");
    let first = io.events.recv().await;
    let Some(TransportEvent::Line(l)) = first else {
        panic!("expected pid line, got {first:?}")
    };
    let pid: i32 = l.trim().parse().unwrap();
    assert!(alive(pid));
    (io.kill)();
    let (_, info) = drain(&mut io, Duration::from_secs(10)).await;
    assert_eq!(info.signal, Some(libc::SIGTERM));
    assert!(
        wait_dead(pid, Duration::from_secs(3)).await,
        "descendant survived group kill"
    );
}
