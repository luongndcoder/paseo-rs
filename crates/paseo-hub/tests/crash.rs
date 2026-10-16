//! kill -9 between persist steps. A child process (this test binary re-run on one hidden test)
//! drives the relationship actor into a step and idles; the parent SIGKILLs it the moment the
//! step's file is on disk and checks that the next daemon start finds a complete, valid record
//! and continues the same ceremony. The persisted bytes are what a crash leaves behind, so the
//! parent only trusts the file.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use paseo_hub::{secret_fingerprint, HubConnectionState, PermissionChange, Record};
use serde_json::Value;
use support::{connect_input, perms, wait_until, Rig};

const SCENARIO: &str = "PASEO_HUB_CRASH_SCENARIO";
const HOME: &str = "PASEO_HUB_CRASH_HOME";

// ---- child --------------------------------------------------------------------------------

/// Not a test when run normally: it only acts for the parent's scenario variable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_child() {
    let Ok(scenario) = std::env::var(SCENARIO) else {
        return;
    };
    let home = PathBuf::from(std::env::var(HOME).expect("home"));
    let rig = Rig::in_dir(&home).await;
    match scenario.as_str() {
        "pending" => {
            rig.remote.hold_enrollment();
            let _ = rig
                .handle
                .connect(connect_input("one-time-token", "https://hub.test"))
                .await;
        }
        "active" => {
            rig.connect("token").await;
        }
        "disconnecting" => {
            rig.connect("token").await;
            rig.remote.hold_revocation();
            let _ = rig.handle.disconnect(false).await;
        }
        "revoked" => {
            rig.connect("token").await;
            rig.remote.reject_socket(0, 401);
        }
        "update_loop" => {
            rig.connect("token").await;
            for _ in 0..1_000_000 {
                for (grant, revoke) in [
                    (&["daemon.read"][..], &[][..]),
                    (&[][..], &["daemon.read"][..]),
                ] {
                    rig.handle
                        .update_permissions(PermissionChange {
                            grant: perms(grant),
                            revoke: perms(revoke),
                        })
                        .await
                        .unwrap();
                }
            }
        }
        other => panic!("unknown scenario {other}"),
    }
    // Idle until the parent kills us (and exit on our own if it never does).
    tokio::time::sleep(Duration::from_secs(120)).await;
}

// ---- parent -------------------------------------------------------------------------------

fn spawn_child(scenario: &str, home: &Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["crash_child", "--exact", "--nocapture", "--test-threads=1"])
        .env(SCENARIO, scenario)
        .env(HOME, home)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn child")
}

fn read_state(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(home.join("hub-relationship.json")).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value["state"].as_str().map(str::to_owned)
}

/// Waits (up to 20 s) for the child's file to reach `state`, then `kill -9`s it.
fn kill_when_state(child: &mut Child, home: &Path, state: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while read_state(home).as_deref() != Some(state) {
        assert!(Instant::now() < deadline, "the child never reached {state}");
        assert!(
            child.try_wait().unwrap().is_none(),
            "the child exited early"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    child.kill().expect("SIGKILL");
    child.wait().unwrap();
}

fn file(home: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(home.join("hub-relationship.json")).unwrap())
        .unwrap()
}

fn assert_valid_record(home: &Path) -> Record {
    let text =
        std::fs::read_to_string(home.join("hub-relationship.json")).expect("the file exists");
    assert!(
        text.ends_with("}\n"),
        "a complete write ends with its newline"
    );
    let raw: Value = serde_json::from_str(&text).expect("complete JSON");
    Record::parse(&raw).expect("a valid record").0
}

#[tokio::test(flavor = "multi_thread")]
async fn killed_after_the_pending_record_is_persisted_the_next_start_continues_the_same_ceremony() {
    let home = tempfile::tempdir().unwrap();
    let mut child = spawn_child("pending", home.path());
    kill_when_state(&mut child, home.path(), "pending");

    let persisted = file(home.path());
    assert_valid_record(home.path());
    let rig = Rig::in_dir(home.path()).await;
    assert_eq!(rig.handle.status().state, HubConnectionState::Connecting);
    rig.handle.start().await.unwrap();

    let attempts = rig.remote.enrollment_attempts();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].daemon_id, persisted["relationship"]["daemonId"]);
    assert_eq!(
        attempts[0].idempotency_key,
        persisted["relationship"]["idempotencyKey"]
    );
    assert_eq!(attempts[0].token, "one-time-token");
    assert_eq!(
        attempts[0].credential_verifier,
        secret_fingerprint(persisted["credential"]["secret"].as_str().unwrap()),
        "the retried ceremony sends the verifier of the surviving secret"
    );
    assert_eq!(rig.handle.status().state, HubConnectionState::Connecting);
    assert_eq!(read_state(home.path()).as_deref(), Some("active"));
    assert_eq!(rig.remote.socket_attempts(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn killed_right_after_the_active_record_is_persisted_the_next_start_dials_it() {
    let home = tempfile::tempdir().unwrap();
    let mut child = spawn_child("active", home.path());
    kill_when_state(&mut child, home.path(), "active");

    let before = file(home.path());
    assert_valid_record(home.path());
    let rig = Rig::in_dir(home.path()).await;
    rig.handle.start().await.unwrap();

    assert_eq!(rig.remote.socket_attempts(), 1);
    assert_eq!(
        rig.remote.socket_input(0).credential,
        before["credential"]["secret"]
    );
    assert_eq!(rig.file().unwrap(), before, "starting rewrites nothing");
}

#[tokio::test(flavor = "multi_thread")]
async fn killed_while_the_hub_is_being_told_a_persisted_disconnecting_record_is_deleted_at_startup()
{
    let home = tempfile::tempdir().unwrap();
    let mut child = spawn_child("disconnecting", home.path());
    kill_when_state(&mut child, home.path(), "disconnecting");

    assert_valid_record(home.path());
    let rig = Rig::in_dir(home.path()).await;
    rig.handle.start().await.unwrap();

    // COMPAT(hubUnilateralDisconnect): the record is dropped without telling the Hub.
    assert_eq!(rig.handle.status().state, HubConnectionState::NotConnected);
    assert!(rig.file().is_none());
    assert_eq!(rig.remote.revocation_attempts(), 0);
    assert_eq!(rig.remote.socket_attempts(), 0);
    assert!(rig
        .logger
        .messages()
        .contains(&"Removed legacy disconnecting Hub relationship during startup".to_owned()));
}

#[tokio::test(flavor = "multi_thread")]
async fn killed_after_the_revoked_record_is_persisted_the_relationship_stays_revoked() {
    let home = tempfile::tempdir().unwrap();
    let mut child = spawn_child("revoked", home.path());
    kill_when_state(&mut child, home.path(), "revoked");

    assert_valid_record(home.path());
    let rig = Rig::in_dir(home.path()).await;
    rig.handle.start().await.unwrap();

    let status = rig.handle.status();
    assert_eq!(status.state, HubConnectionState::Revoked);
    assert_eq!(
        status.last_error.as_deref(),
        Some("Hub rejected socket authentication (401)")
    );
    assert_eq!(rig.remote.socket_attempts(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn killed_at_any_instant_of_a_persist_loop_never_leaves_a_truncated_record() {
    for delay_ms in [0u64, 1, 2, 3, 5, 8, 13, 21, 34, 55] {
        let home = tempfile::tempdir().unwrap();
        let mut child = spawn_child("update_loop", home.path());
        kill_when_state(&mut child, home.path(), "active");
        std::thread::sleep(Duration::from_millis(delay_ms));
        let _ = child.kill();
        child.wait().unwrap();

        let record = assert_valid_record(home.path());
        assert!(matches!(record, Record::Active(_)), "delay {delay_ms}ms");
        assert!(
            std::fs::metadata(home.path().join("hub-relationship.json"))
                .unwrap()
                .len()
                > 0
        );
        // A killed write may leave its private temp file behind; it is never the target.
        for entry in std::fs::read_dir(home.path()).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                name == "hub-relationship.json" || name.starts_with(".hub-relationship.json."),
                "unexpected file {name}"
            );
        }
        // And the next daemon start loads it without quarantining anything.
        let rig = Rig::in_dir(home.path()).await;
        rig.handle.start().await.unwrap();
        assert_eq!(rig.quarantined_files().len(), 0, "delay {delay_ms}ms");
        assert_eq!(rig.remote.socket_attempts(), 1);
        wait_until("socket dialed", || rig.remote.socket_attempts() == 1).await;
    }
}
