//! `hub-relationship.json` parity with the TS controller: `fixtures/p4/hub` is TS output
//! (`capture-hub-relationship.mts`). Every state the TS actor writes is reproduced byte for
//! byte by the Rust actor, and every persisted file in the load table (v1, v2, invalid) ends
//! in the same state, rewrite and quarantine decision.

mod support;

use paseo_hub::{ConnectInput, PermissionChange};
use serde_json::Value;
use support::{perms, settle, wait_until, Rig};

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/../../fixtures/p4/hub/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

fn status_json(rig: &Rig) -> Value {
    serde_json::to_value(rig.handle.status()).unwrap()
}

fn text(file: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(file).ok()
}

#[tokio::test]
async fn every_persisted_file_in_the_load_table_ends_as_in_ts() {
    let cases = fixture("hub-relationship-load.json");
    assert!(cases.as_array().unwrap().len() >= 50);
    for case in cases.as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("hub-relationship.json"),
            case["text"].as_str().unwrap(),
        )
        .unwrap();
        let rig = Rig::with_home(home).await;
        rig.remote.fail_enrollments("offline");

        assert_eq!(
            status_json(&rig),
            case["statusAtConstruction"],
            "{name}: status after construction"
        );
        rig.handle.start().await.unwrap();

        assert_eq!(
            text(&rig.relationship_path())
                .map(Value::from)
                .unwrap_or(Value::Null),
            case["fileAfterLoad"],
            "{name}: file after load"
        );
        let quarantined = rig.quarantined_files();
        assert_eq!(
            quarantined.len() as u64,
            case["quarantined"].as_u64().unwrap(),
            "{name}: quarantine"
        );
        if let Some(path) = quarantined.first() {
            assert_eq!(
                Value::from(text(path).unwrap()),
                case["quarantinedText"],
                "{name}: the quarantined file keeps the original bytes"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{name}: quarantine file is private");
            }
        }
        assert_eq!(
            status_json(&rig),
            case["statusAfterStart"],
            "{name}: status after start"
        );
        assert_eq!(
            rig.remote.enrollment_attempts().len() as u64,
            case["enrollAttempts"].as_u64().unwrap(),
            "{name}: enrollment attempts"
        );
        assert_eq!(
            rig.remote.socket_attempts() as u64,
            case["socketAttempts"].as_u64().unwrap(),
            "{name}: socket attempts"
        );
    }
}

fn ceremony(states: &Value) -> paseo_hub::Ceremony {
    let c = &states["ceremony"];
    paseo_hub::Ceremony {
        daemon_id: c["daemonId"].as_str().unwrap().into(),
        idempotency_key: c["idempotencyKey"].as_str().unwrap().into(),
        secret: c["secret"].as_str().unwrap().into(),
    }
}

/// The ceremony a scenario drew in TS (each scenario advanced the capture's counters).
fn ceremony_of_state(states: &Value, name: &str) -> paseo_hub::Ceremony {
    let record: Value =
        serde_json::from_str(states["states"][name]["text"].as_str().unwrap()).unwrap();
    paseo_hub::Ceremony {
        daemon_id: record["relationship"]["daemonId"].as_str().unwrap().into(),
        idempotency_key: record["relationship"]["idempotencyKey"]
            .as_str()
            .unwrap()
            .into(),
        secret: record["credential"]["secret"].as_str().unwrap().into(),
    }
}

fn expect_state(rig: &Rig, states: &Value, name: &str) {
    let golden = &states["states"][name];
    assert_eq!(
        text(&rig.relationship_path()).as_deref(),
        golden["text"].as_str(),
        "{name}: bytes written"
    );
    assert_eq!(status_json(rig), golden["status"], "{name}: status");
}

#[tokio::test]
async fn the_actor_writes_the_bytes_ts_writes_for_every_state() {
    let states = fixture("hub-relationship-states.json");

    // pending -> pending with a fresh token -> active -> permission updates
    let rig = Rig::with_ceremony(ceremony(&states)).await;
    rig.remote.hold_enrollment();
    let first = {
        let handle = rig.handle.clone();
        tokio::spawn(async move {
            handle
                .connect(ConnectInput {
                    hub_url: "https://hub.test/".into(),
                    token: "token-1".into(),
                    permissions: perms(&["hub.execute"]),
                })
                .await
        })
    };
    wait_until("first enrollment", || {
        rig.remote.enrollment_attempts().len() == 1
    })
    .await;
    expect_state(&rig, &states, "pending");

    rig.remote.hold_enrollment();
    let second = {
        let handle = rig.handle.clone();
        tokio::spawn(async move {
            handle
                .connect(ConnectInput {
                    hub_url: "https://hub.test".into(),
                    token: "fresh-token".into(),
                    permissions: perms(&["hub.execute"]),
                })
                .await
        })
    };
    wait_until("fresh enrollment", || {
        rig.remote.enrollment_attempts().len() == 2
    })
    .await;
    expect_state(&rig, &states, "pending_fresh_token");

    rig.remote.complete_enrollment();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    expect_state(&rig, &states, "active");

    rig.handle
        .update_permissions(PermissionChange {
            grant: perms(&["daemon.read"]),
            revoke: vec![],
        })
        .await
        .unwrap();
    expect_state(&rig, &states, "active_updated_permissions");
    rig.handle
        .update_permissions(PermissionChange {
            grant: vec![],
            revoke: perms(&["hub.execute"]),
        })
        .await
        .unwrap();
    expect_state(&rig, &states, "active_revoked_execute");
}

async fn active_rig(ceremony: paseo_hub::Ceremony) -> Rig {
    let rig = Rig::with_ceremony(ceremony).await;
    rig.handle
        .connect(ConnectInput {
            hub_url: "https://hub.test/".into(),
            token: "token-1".into(),
            permissions: perms(&["hub.execute"]),
        })
        .await
        .unwrap();
    rig
}

#[tokio::test]
async fn disconnecting_revoked_and_reconnecting_records_match_ts() {
    let states = fixture("hub-relationship-states.json");

    let rig = active_rig(ceremony_of_state(&states, "disconnecting")).await;
    rig.remote.hold_revocation();
    let handle = rig.handle.clone();
    let disconnecting = tokio::spawn(async move { handle.disconnect(false).await });
    wait_until("revocation in flight", || {
        rig.remote.revocation_attempts() == 1
    })
    .await;
    expect_state(&rig, &states, "disconnecting");
    rig.remote.complete_revocation();
    disconnecting.await.unwrap().unwrap();

    let rig = active_rig(ceremony(&states)).await;
    rig.remote.reject_socket(0, 401);
    wait_until("revoked", || {
        rig.handle.status().state == paseo_hub::HubConnectionState::Revoked
    })
    .await;
    expect_state(&rig, &states, "revoked_401");

    let rig = active_rig(ceremony(&states)).await;
    rig.remote.close_socket(0, 4403);
    wait_until("revoked", || {
        rig.handle.status().state == paseo_hub::HubConnectionState::Revoked
    })
    .await;
    expect_state(&rig, &states, "revoked_4403");

    let rig = active_rig(ceremony_of_state(&states, "reconnecting_after_close")).await;
    rig.remote.close_socket(0, 1006);
    settle().await;
    expect_state(&rig, &states, "reconnecting_after_close");
}
