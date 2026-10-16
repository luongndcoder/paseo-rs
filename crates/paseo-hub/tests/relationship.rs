//! `relationship-controller.test.ts` with a fake clock, remote and session host. The TS cases
//! that need a whole daemon (`hub.management.*` over a real socket, provider restarts) are
//! covered at the seam this crate owns: the actor, `DaemonExecutions` and the persisted file.

mod support;

use std::sync::atomic::Ordering;

use paseo_authz::{PermSet, Permission, SessionAuthz};
use paseo_hub::{
    secret_fingerprint, Ceremony, ConnectInput, ExecError, HubConnectionState, HubError,
    HubExecutionAgentCreateInput, HubExecutionAgents, PermissionChange, SessionProtocol,
};
use serde_json::{json, Value};
use support::{connect_input, perms, settle, wait_until, Operation, Rig};

fn state_of(rig: &Rig) -> HubConnectionState {
    rig.handle.status().state
}

fn file_state(rig: &Rig) -> Option<String> {
    rig.file().map(|f| f["state"].as_str().unwrap().to_owned())
}

fn spawn_connect(
    rig: &Rig,
    token: &str,
) -> tokio::task::JoinHandle<Result<paseo_hub::HubRelationshipStatus, HubError>> {
    let handle = rig.handle.clone();
    let input = connect_input(token, "https://hub.test");
    tokio::spawn(async move { handle.connect(input).await })
}

fn create_input(execution_id: &str) -> HubExecutionAgentCreateInput {
    HubExecutionAgentCreateInput {
        execution_id: execution_id.into(),
        provider: "codex".into(),
        cwd: "/workspace".into(),
        prompt: "run".into(),
        ..Default::default()
    }
}

/// The TS harness fixes `createDaemonId`; re-enrollment must keep the daemon id.
fn fixed_daemon(daemon_id: &str) -> Ceremony {
    Ceremony {
        daemon_id: daemon_id.into(),
        idempotency_key: "ceremony-1".into(),
        secret: "c2VjcmV0".into(),
    }
}

// ---- connect and enrollment ---------------------------------------------------------------

#[tokio::test]
async fn hub_urls_cannot_persist_embedded_credentials() {
    let rig = Rig::start().await;

    let result = rig
        .handle
        .connect(connect_input(
            "ceremony-token",
            "https://user:password@hub.example",
        ))
        .await;

    assert_eq!(
        result.unwrap_err(),
        HubError::message("Hub URL cannot include credentials")
    );
    assert!(rig.file().is_none());
    assert!(rig.remote.enrollment_attempts().is_empty());
}

#[tokio::test]
async fn rejects_urls_that_are_not_http_or_carry_query_or_fragment() {
    let rig = Rig::start().await;
    for (url, message) in [
        ("ftp://hub.test", "Hub URL must use HTTP or HTTPS"),
        (
            "https://hub.test?x=1",
            "Hub URL cannot include a query or fragment",
        ),
        (
            "https://hub.test#frag",
            "Hub URL cannot include a query or fragment",
        ),
        ("not a url", "Invalid URL"),
    ] {
        let error = rig
            .handle
            .connect(connect_input("t", url))
            .await
            .unwrap_err();
        assert_eq!(error, HubError::message(message), "{url}");
    }
    assert!(rig.file().is_none());
}

#[tokio::test]
async fn invalid_permission_names_are_rejected_before_anything_is_persisted() {
    let rig = Rig::start().await;
    let error = rig
        .handle
        .connect(ConnectInput {
            hub_url: "https://hub.test".into(),
            token: "t".into(),
            permissions: perms(&["hub.execution.*"]),
        })
        .await
        .unwrap_err();
    assert_eq!(error, HubError::message("Invalid daemon permission"));
    assert!(rig.file().is_none());
}

#[tokio::test]
async fn persists_private_generated_authority_before_enrollment_and_active_before_dialing() {
    let rig = Rig::start().await;
    rig.remote.hold_enrollment();
    let connecting = spawn_connect(&rig, "one-time-token");

    wait_until("enrollment begins", || {
        rig.remote.enrollment_attempts().len() == 1
    })
    .await;
    let enrollment = rig.remote.enrollment_attempts().remove(0);
    let pending = rig.file().unwrap();
    assert_eq!(pending["state"], "pending");
    assert_eq!(
        pending["relationship"]["daemonId"],
        json!(enrollment.daemon_id)
    );
    assert_eq!(
        pending["relationship"]["idempotencyKey"],
        json!(enrollment.idempotency_key)
    );
    assert_eq!(pending["enrollment"]["token"], "one-time-token");
    assert_eq!(pending["identity"]["serverId"], "server-fixture");
    let secret = pending["credential"]["secret"].as_str().unwrap().to_owned();
    assert!(!secret.is_empty());
    assert_eq!(enrollment.credential_verifier, secret_fingerprint(&secret));
    assert_eq!(enrollment.hostname, "fixture-host");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&rig.relationship_path()), 0o600);
        assert_eq!(mode(rig.home.path()), 0o700);
    }

    rig.remote.complete_enrollment();
    connecting.await.unwrap().unwrap();
    assert_eq!(rig.remote.socket_attempts(), 1);
    // The record was `active` on disk before the dial (the dial happens inside the same step).
    let active = rig.file().unwrap();
    assert_eq!(active["state"], "active");
    assert_eq!(
        active["relationship"]["daemonId"],
        json!(enrollment.daemon_id)
    );
    assert_eq!(rig.remote.socket_input(0).credential, secret);
    assert_eq!(
        rig.remote.socket_input(0).web_socket_url,
        "wss://hub.test/daemon"
    );
}

#[test]
fn the_credential_verifier_is_base64url_sha256_without_padding() {
    // sha256("abc")
    assert_eq!(
        secret_fingerprint("abc"),
        "ungWv48Bz-pBQUDeXa4iI7ADYaOWF3qctBD_YfIAFa0"
    );
}

#[tokio::test]
async fn new_relationships_can_connect_without_granting_hub_execution() {
    let rig = Rig::start().await;
    rig.remote.return_enrollment_permissions(&[]);

    let status = rig
        .handle
        .connect(ConnectInput {
            hub_url: "https://hub.test".into(),
            token: "registered-token".into(),
            permissions: vec![],
        })
        .await
        .unwrap();
    assert_eq!(status.state, HubConnectionState::Connecting);
    rig.remote.connect_latest_socket();
    wait_until("attached", || rig.host.attach_count() == 1).await;

    assert_eq!(
        rig.file().unwrap()["relationship"]["permissions"],
        json!([])
    );
    let denied = SessionAuthz::new(permission_set(&rig.host.attached_permissions()[0]));
    assert!(!denied.allows_inbound("hub.execution.agent.create.request"));

    rig.handle
        .update_permissions(PermissionChange {
            grant: perms(&["hub.execute"]),
            revoke: vec![],
        })
        .await
        .unwrap();
    assert_eq!(
        rig.file().unwrap()["relationship"]["permissions"],
        json!(["hub.execute"])
    );
    let (principal, granted) = rig
        .host
        .permission_updates
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap();
    assert!(principal.starts_with("hub:"));
    assert_eq!(granted, vec![Permission::HubExecute]);
    assert!(SessionAuthz::new(permission_set(&granted))
        .allows_inbound("hub.execution.agent.create.request"));

    rig.handle
        .update_permissions(PermissionChange {
            grant: vec![],
            revoke: perms(&["hub.execute"]),
        })
        .await
        .unwrap();
    assert_eq!(
        rig.file().unwrap()["relationship"]["permissions"],
        json!([])
    );
    let (_, revoked) = rig
        .host
        .permission_updates
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap();
    assert!(!SessionAuthz::new(permission_set(&revoked))
        .allows_inbound("hub.execution.agent.create.request"));
}

fn permission_set(permissions: &[Permission]) -> PermSet {
    permissions.iter().copied().collect()
}

#[tokio::test]
async fn hub_enrollment_cannot_widen_the_daemons_locally_granted_permissions() {
    let rig = Rig::start().await;
    rig.remote
        .return_enrollment_permissions(&["hub.execute", "daemon.manage"]);

    rig.connect("token").await;

    assert_eq!(
        rig.file().unwrap()["relationship"]["permissions"],
        json!(["hub.execute"])
    );
    assert_eq!(state_of(&rig), HubConnectionState::Reconnecting);
    assert_eq!(rig.remote.socket_attempts(), 0);
    assert_eq!(
        rig.handle.status().last_error.as_deref(),
        Some("Hub enrollment response did not match the pending relationship")
    );
}

#[tokio::test]
async fn a_lost_enrollment_response_reuses_the_exact_ceremony() {
    let rig = Rig::start().await;
    rig.remote.hold_enrollment();
    let connecting = spawn_connect(&rig, "same-token");
    wait_until("enrollment begins", || {
        rig.remote.enrollment_attempts().len() == 1
    })
    .await;
    rig.remote.lose_enrollment_response();
    connecting.await.unwrap().unwrap();

    rig.clock.run_next();
    wait_until("retry", || rig.remote.enrollment_attempts().len() == 2).await;
    let attempts = rig.remote.enrollment_attempts();

    assert_eq!(attempts[1], attempts[0]);
    assert_eq!(rig.remote.enrolled_relationships(), 1);
    wait_until("active", || file_state(&rig).as_deref() == Some("active")).await;
    assert_eq!(rig.clock.scheduled_attempts(), vec![0]);
}

#[tokio::test]
async fn a_fresh_token_replaces_the_token_for_a_pending_enrollment_ceremony() {
    let rig = Rig::start().await;
    rig.remote.hold_enrollment();
    let first = spawn_connect(&rig, "expired-token");
    wait_until("first enrollment", || {
        rig.remote.enrollment_attempts().len() == 1
    })
    .await;
    rig.remote.lose_enrollment_response();
    assert_eq!(
        first.await.unwrap().unwrap().state,
        HubConnectionState::Reconnecting
    );

    rig.remote.hold_enrollment();
    let second = spawn_connect(&rig, "fresh-token");
    wait_until("retried enrollment", || {
        rig.remote.enrollment_attempts().len() == 2
    })
    .await;

    assert_eq!(rig.remote.enrollment_attempts()[1].token, "fresh-token");
    assert_eq!(rig.file().unwrap()["enrollment"]["token"], "fresh-token");
    assert_eq!(
        rig.clock.pending_tasks(),
        0,
        "the retry timer of the old attempt is cancelled"
    );
    rig.remote.complete_enrollment();
    assert_eq!(
        second.await.unwrap().unwrap().state,
        HubConnectionState::Connecting
    );
}

#[tokio::test]
async fn a_pending_enrollment_for_a_different_hub_is_refused() {
    let rig = Rig::start().await;
    rig.remote.hold_enrollment();
    let _first = spawn_connect(&rig, "token");
    wait_until("enrollment", || rig.remote.enrollment_attempts().len() == 1).await;

    let error = rig
        .handle
        .connect(connect_input("token", "https://other.test"))
        .await
        .unwrap_err();

    assert_eq!(
        error,
        HubError::message("A pending Hub enrollment already exists for a different Hub")
    );
}

#[tokio::test]
async fn an_existing_relationship_blocks_connect_but_a_revoked_one_does_not() {
    let rig = Rig::start().await;
    rig.connect("token").await;
    let error = rig
        .handle
        .connect(connect_input("again", "https://hub.test"))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        HubError::message("This daemon already has a Hub relationship")
    );

    rig.remote.reject_socket(0, 401);
    wait_until("revoked", || state_of(&rig) == HubConnectionState::Revoked).await;
    let status = rig
        .handle
        .connect(connect_input("fresh", "https://hub.test"))
        .await
        .unwrap();
    assert_eq!(status.state, HubConnectionState::Connecting);
    assert_eq!(rig.file().unwrap()["state"], "active");
}

#[tokio::test]
async fn a_stale_enrollment_rejection_cannot_discard_a_fresh_pending_ceremony() {
    let rig = Rig::start().await;
    rig.remote.hold_enrollment();
    let expired = spawn_connect(&rig, "expired-token");
    wait_until("first enrollment", || {
        rig.remote.enrollment_attempts().len() == 1
    })
    .await;
    rig.remote.hold_enrollment();
    let fresh = spawn_connect(&rig, "fresh-token");
    wait_until("fresh enrollment", || {
        rig.remote.enrollment_attempts().len() == 2
    })
    .await;

    rig.remote.reject_enrollment(0, 403);
    expired.await.unwrap().unwrap();

    assert_eq!(rig.file().unwrap()["enrollment"]["token"], "fresh-token");
    assert_eq!(rig.clock.pending_tasks(), 0);

    rig.remote.complete_enrollment();
    fresh.await.unwrap().unwrap();
    assert_eq!(file_state(&rig).as_deref(), Some("active"));
    assert_eq!(rig.remote.enrollment_attempts().len(), 2);
    assert_eq!(rig.clock.pending_tasks(), 0);
}

#[tokio::test]
async fn a_rejected_pending_enrollment_is_discarded_without_blocking_daemon_restart() {
    let mut rig = Rig::start().await;
    rig.remote.hold_enrollment();
    let connecting = spawn_connect(&rig, "expired-token");
    wait_until("enrollment", || rig.remote.enrollment_attempts().len() == 1).await;
    rig.remote.lose_enrollment_response();
    connecting.await.unwrap().unwrap();
    rig.remote.reject_next_enrollment(401);

    rig.restart().await;

    assert_eq!(state_of(&rig), HubConnectionState::NotConnected);
    assert!(rig.file().is_none());
    assert!(rig
        .logger
        .messages()
        .contains(&"Discarded rejected pending Hub enrollment during startup".to_owned()));
}

#[tokio::test]
async fn a_rejected_connect_discards_the_ceremony_and_reports_the_status_code() {
    let rig = Rig::start().await;
    rig.remote.reject_next_enrollment(403);

    let error = rig
        .handle
        .connect(connect_input("bad", "https://hub.test"))
        .await
        .unwrap_err();

    assert_eq!(error, HubError::EnrollmentRejected { status: 403 });
    assert_eq!(error.to_string(), "Hub enrollment failed (403)");
    assert!(rig.file().is_none());
    assert_eq!(state_of(&rig), HubConnectionState::NotConnected);
}

#[tokio::test]
async fn a_scheduled_enrollment_rejection_is_contained_after_removing_pending_authority() {
    let rig = Rig::start().await;
    rig.remote.hold_enrollment();
    let connecting = spawn_connect(&rig, "expired-token");
    wait_until("enrollment", || rig.remote.enrollment_attempts().len() == 1).await;
    rig.remote.lose_enrollment_response();
    connecting.await.unwrap().unwrap();
    rig.remote.reject_next_enrollment(403);

    rig.clock.run_next();
    wait_until("rejection processed", || {
        state_of(&rig) == HubConnectionState::NotConnected
    })
    .await;

    assert!(rig.file().is_none());
    assert_eq!(rig.clock.pending_tasks(), 0);
}

// ---- persisted files ----------------------------------------------------------------------

fn write_file(rig: &Rig, text: &str) {
    std::fs::write(rig.relationship_path(), text).unwrap();
}

#[tokio::test]
async fn invalid_relationship_files_are_quarantined_without_blocking_daemon_startup() {
    let v1 = |origin: &str, transport: Value| {
        json!({
            "version": 1, "state": "active",
            "relationship": {
                "daemonId": "daemon-1", "idempotencyKey": "ceremony-1", "hubOrigin": origin,
                "createdAt": "2026-07-13T00:00:00.000Z", "scopes": ["hub.execution.*"]
            },
            "credential": { "secret": "credential" },
            "transport": transport
        })
        .to_string()
    };
    let ws = |url: &str| json!({ "kind": "direct_websocket", "webSocketUrl": url });
    let cases = [
        "{not-json".to_owned(),
        json!({ "version": 1, "state": "unknown" }).to_string(),
        v1("ftp://hub.test", ws("wss://hub.test/daemon")),
        v1(
            "https://user:password@hub.test",
            ws("wss://hub.test/daemon"),
        ),
        v1("https://hub.test?token=secret", ws("wss://hub.test/daemon")),
        v1("https://hub.test#secret", ws("wss://hub.test/daemon")),
        v1("https://hub.test", ws("ftp://hub.test/daemon")),
        v1("https://hub.test", ws("wss://hub.test/daemon#fragment")),
        v1("https://hub.test", ws("wss://elsewhere.test/daemon")),
    ];
    for text in cases {
        let rig = Rig::start().await;
        write_file(&rig, &text);

        let handle = rig.respawn();
        handle.start().await.unwrap();

        assert_eq!(
            handle.status().state,
            HubConnectionState::NotConnected,
            "{text}"
        );
        assert!(rig.file().is_none(), "{text}");
        assert_eq!(rig.quarantined_files().len(), 1, "{text}");
        assert_eq!(rig.remote.socket_attempts(), 0, "{text}");
        assert!(rig.remote.enrollment_attempts().is_empty(), "{text}");
        assert!(rig
            .logger
            .messages()
            .contains(&"Quarantined invalid Hub relationship authority".to_owned()));
    }
}

#[tokio::test]
async fn a_legacy_execution_scope_migrates_once_to_the_semantic_permission() {
    let rig = Rig::start().await;
    write_file(
        &rig,
        &json!({
            "version": 1, "state": "active",
            "relationship": {
                "daemonId": "daemon-legacy", "idempotencyKey": "ceremony-legacy",
                "hubOrigin": "https://hub.test", "createdAt": "2026-07-13T00:00:00.000Z",
                "scopes": ["hub.execution.*"]
            },
            "credential": { "secret": "credential" },
            "transport": { "kind": "direct_websocket", "webSocketUrl": "wss://hub.test/daemon" }
        })
        .to_string(),
    );

    let handle = rig.respawn();

    let file = rig.file().unwrap();
    assert_eq!(file["version"], 2);
    assert_eq!(file["relationship"]["permissions"], json!(["hub.execute"]));
    assert!(file["relationship"].get("scopes").is_none());
    assert_eq!(handle.status().permissions, vec![Permission::HubExecute]);
}

#[tokio::test]
async fn a_persisted_relationship_cannot_widen_its_local_permissions() {
    let rig = Rig::start().await;
    rig.connect("token").await;
    let before = rig.remote.socket_attempts();
    let mut persisted = rig.file().unwrap();
    persisted["relationship"]["permissions"] = json!(["*"]);
    write_file(&rig, &persisted.to_string());
    rig.handle.stop().await.unwrap();

    let handle = rig.respawn();
    handle.start().await.unwrap();

    assert_eq!(handle.status().state, HubConnectionState::NotConnected);
    assert!(rig.file().is_none());
    assert_eq!(rig.quarantined_files().len(), 1);
    assert_eq!(rig.remote.socket_attempts(), before);
}

#[tokio::test]
async fn startup_removes_a_legacy_persisted_disconnecting_relationship() {
    let rig = Rig::start().await;
    rig.connect("token").await;
    let mut active = rig.file().unwrap();
    active["state"] = "disconnecting".into();
    write_file(&rig, &active.to_string());
    rig.handle.stop().await.unwrap();
    let revocations = rig.remote.revocation_attempts();

    let handle = rig.respawn();
    handle.start().await.unwrap();

    let status = handle.status();
    assert_eq!(status.state, HubConnectionState::NotConnected);
    assert_eq!(status.hub_origin, None);
    assert!(rig.file().is_none());
    assert_eq!(rig.remote.revocation_attempts(), revocations);
    assert_eq!(rig.clock.pending_tasks(), 0);
    assert!(rig
        .logger
        .messages()
        .contains(&"Removed legacy disconnecting Hub relationship during startup".to_owned()));
}

#[tokio::test]
async fn a_revoked_relationship_is_restored_with_its_reason_and_never_redials() {
    for (close, reason) in [
        (Err(4403), "Hub revoked this relationship"),
        (Ok(401), "Hub rejected socket authentication (401)"),
        (Ok(403), "Hub rejected socket authentication (403)"),
    ] {
        let mut rig = Rig::start().await;
        rig.connect("private-token").await;
        let enrollment = rig.remote.enrollment_attempts().remove(0);
        let secret = rig.file().unwrap()["credential"]["secret"]
            .as_str()
            .unwrap()
            .to_owned();
        match close {
            Ok(status) => rig.remote.reject_socket(0, status),
            Err(code) => rig.remote.close_socket(0, code),
        }
        wait_until("revoked", || state_of(&rig) == HubConnectionState::Revoked).await;

        rig.restart().await;
        let status = rig.handle.status();
        let persisted = rig.file().unwrap();

        assert_eq!(status.state, HubConnectionState::Revoked);
        assert_eq!(
            status.daemon_id.as_deref(),
            Some(enrollment.daemon_id.as_str())
        );
        assert_eq!(status.hub_origin.as_deref(), Some("https://hub.test"));
        assert_eq!(status.permissions, vec![Permission::HubExecute]);
        assert_eq!(status.last_error.as_deref(), Some(reason));
        assert_eq!(persisted["state"], "revoked");
        assert_eq!(persisted["reason"], reason);
        assert!(persisted.get("credential").is_none());
        assert!(persisted["relationship"].get("idempotencyKey").is_none());
        assert_eq!(rig.remote.socket_attempts(), 1);
        assert_eq!(rig.clock.pending_tasks(), 0);
        let everything = format!(
            "{}{}{}",
            serde_json::to_string(&status).unwrap(),
            persisted,
            rig.logger.all_text()
        );
        for secret_value in [
            secret.as_str(),
            enrollment.credential_verifier.as_str(),
            enrollment.token.as_str(),
            enrollment.idempotency_key.as_str(),
        ] {
            assert!(!everything.contains(secret_value), "{secret_value} leaked");
        }
    }
}

// ---- disconnect ---------------------------------------------------------------------------

#[tokio::test]
async fn disconnect_revokes_an_ambiguous_pending_enrollment_before_removing_local_authority() {
    let rig = Rig::start().await;
    rig.remote.hold_enrollment();
    let connecting = spawn_connect(&rig, "one-time-token");
    wait_until("enrollment", || rig.remote.enrollment_attempts().len() == 1).await;
    let enrollment = rig.remote.enrollment_attempts().remove(0);
    let credential = rig.file().unwrap()["credential"]["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    rig.remote.lose_enrollment_response();
    connecting.await.unwrap().unwrap();
    rig.remote.fail_revocations(1);

    let disconnected = rig.handle.disconnect(false).await.unwrap();
    let reconnected = rig.connect("fresh-token").await;

    assert_eq!(disconnected.status.state, HubConnectionState::NotConnected);
    assert_eq!(disconnected.status.hub_origin, None);
    assert!(disconnected
        .warning
        .unwrap()
        .contains("server-side revocation may remain pending"));
    assert_eq!(reconnected.state, HubConnectionState::Connecting);
    assert_eq!(rig.remote.revocation_attempts(), 1);
    let revocation = rig.remote.latest_revocation().unwrap();
    assert_eq!(revocation.daemon_id, enrollment.daemon_id);
    assert_eq!(revocation.hub_origin, enrollment.hub_origin);
    assert_eq!(revocation.credential, credential);
    assert_eq!(rig.clock.pending_tasks(), 0);
    assert!(rig
        .logger
        .messages()
        .contains(&"Failed to notify Hub before removing local relationship".to_owned()));
}

#[tokio::test]
async fn disconnect_revokes_only_after_an_in_flight_enrollment_settles() {
    let rig = Rig::start().await;
    rig.remote.hold_enrollment();
    let connecting = spawn_connect(&rig, "one-time-token");
    wait_until("enrollment", || rig.remote.enrollment_attempts().len() == 1).await;
    let enrollment = rig.remote.enrollment_attempts().remove(0);

    let handle = rig.handle.clone();
    let disconnecting = tokio::spawn(async move { handle.disconnect(false).await });
    wait_until("disconnecting", || {
        state_of(&rig) == HubConnectionState::Disconnecting
    })
    .await;
    assert_eq!(file_state(&rig).as_deref(), Some("disconnecting"));
    settle().await;
    assert_eq!(rig.remote.revocation_attempts(), 0);

    rig.remote.complete_enrollment();
    connecting.await.unwrap().unwrap();
    let disconnected = disconnecting.await.unwrap().unwrap();

    assert_eq!(disconnected.status.state, HubConnectionState::NotConnected);
    assert_eq!(
        rig.remote.latest_revocation().unwrap().daemon_id,
        enrollment.daemon_id
    );
    assert_eq!(rig.remote.revocation_attempts(), 1);
    assert_eq!(rig.remote.socket_attempts(), 0);
    assert!(rig.file().is_none());
}

#[tokio::test]
async fn disconnect_persists_terminal_intent_only_while_remote_notification_is_in_flight() {
    let rig = Rig::start().await;
    rig.connect("token").await;
    rig.remote.hold_revocation();

    let handle = rig.handle.clone();
    let disconnecting = tokio::spawn(async move { handle.disconnect(false).await });
    wait_until("disconnecting on disk", || {
        file_state(&rig).as_deref() == Some("disconnecting")
    })
    .await;

    assert_eq!(state_of(&rig), HubConnectionState::Disconnecting);
    assert_eq!(rig.clock.pending_tasks(), 0);
    assert!(
        rig.remote.socket_closed(0),
        "the socket is closed before the Hub is told"
    );

    rig.remote.complete_revocation();
    let outcome = disconnecting.await.unwrap().unwrap();
    assert_eq!(outcome.status.state, HubConnectionState::NotConnected);
    assert!(rig.file().is_none());
}

#[tokio::test]
async fn force_disconnect_does_not_wait_for_or_notify_an_in_flight_enrollment() {
    let rig = Rig::start().await;
    rig.remote.hold_enrollment();
    let connecting = spawn_connect(&rig, "one-time-token");
    wait_until("enrollment", || rig.remote.enrollment_attempts().len() == 1).await;

    let disconnected = rig.handle.disconnect(true).await.unwrap();

    assert_eq!(disconnected.status.state, HubConnectionState::NotConnected);
    assert_eq!(disconnected.status.hub_origin, None);
    assert_eq!(rig.remote.revocation_attempts(), 0);
    assert!(rig.file().is_none());

    rig.remote.complete_enrollment();
    connecting.await.unwrap().unwrap();
    assert_eq!(rig.remote.socket_attempts(), 0);
    assert_eq!(state_of(&rig), HubConnectionState::NotConnected);
    assert_eq!(rig.handle.status().hub_origin, None);
}

#[tokio::test]
async fn offline_disconnect_removes_local_authority_after_one_notify_attempt() {
    let mut rig = Rig::start().await;
    rig.connect("token").await;
    rig.remote.connect_latest_socket();
    rig.remote.fail_revocations(3);

    let disconnected = rig.handle.disconnect(false).await.unwrap();
    rig.restart().await;

    assert_eq!(disconnected.status.state, HubConnectionState::NotConnected);
    assert_eq!(disconnected.status.hub_origin, None);
    assert!(disconnected
        .warning
        .unwrap()
        .contains("server-side revocation may remain pending"));
    assert_eq!(state_of(&rig), HubConnectionState::NotConnected);
    assert_eq!(rig.remote.revocation_attempts(), 1);
    assert_eq!(rig.remote.socket_attempts(), 1);
    assert_eq!(rig.clock.pending_tasks(), 0);
    assert!(rig.file().is_none());
}

#[tokio::test]
async fn successful_disconnect_notifies_the_hub_and_returns_clean_local_status() {
    let rig = Rig::start().await;
    rig.connect("token").await;

    let disconnected = rig.handle.disconnect(false).await.unwrap();

    assert_eq!(disconnected.status.state, HubConnectionState::NotConnected);
    assert_eq!(disconnected.status.hub_origin, None);
    assert_eq!(disconnected.status.last_error, None);
    assert_eq!(disconnected.warning, None);
    assert_eq!(rig.remote.revocation_attempts(), 1);
    assert_eq!(rig.handle.status().last_error, None);
}

#[tokio::test]
async fn force_disconnect_removes_local_authority_without_notifying_the_hub() {
    let rig = Rig::start().await;
    rig.connect("token").await;

    let forced = rig.handle.disconnect(true).await.unwrap();

    assert_eq!(forced.status.state, HubConnectionState::NotConnected);
    assert_eq!(forced.warning, None);
    assert_eq!(rig.remote.revocation_attempts(), 0);
    assert!(!rig
        .logger
        .messages()
        .contains(&"Failed to notify Hub before removing local relationship".to_owned()));
    assert!(rig.file().is_none());
}

#[tokio::test]
async fn disconnecting_with_nothing_connected_or_a_revoked_record_just_removes_the_file() {
    let rig = Rig::start().await;
    let outcome = rig.handle.disconnect(false).await.unwrap();
    assert_eq!(outcome.status.state, HubConnectionState::NotConnected);
    assert_eq!(rig.remote.revocation_attempts(), 0);

    rig.connect("token").await;
    rig.remote.reject_socket(0, 403);
    wait_until("revoked", || state_of(&rig) == HubConnectionState::Revoked).await;
    let outcome = rig.handle.disconnect(false).await.unwrap();
    assert_eq!(outcome.status.state, HubConnectionState::NotConnected);
    assert_eq!(
        rig.remote.revocation_attempts(),
        0,
        "a revoked relationship has no credential to send"
    );
    assert!(rig.file().is_none());
}

// ---- sockets ------------------------------------------------------------------------------

#[tokio::test]
async fn daemon_restart_reconnects_the_same_durable_relationship() {
    let mut rig = Rig::start().await;
    rig.connect("token").await;
    let daemon_id = rig.file().unwrap()["relationship"]["daemonId"].clone();
    rig.remote.connect_latest_socket();
    wait_until("connected", || {
        state_of(&rig) == HubConnectionState::Connected
    })
    .await;

    rig.restart().await;

    assert_eq!(rig.file().unwrap()["relationship"]["daemonId"], daemon_id);
    assert_eq!(rig.remote.socket_attempts(), 2);
    assert!(
        rig.remote.handed_over_closed(0),
        "stopping the old actor closes its socket"
    );
}

#[tokio::test]
async fn disconnect_and_stop_close_the_socket_handed_to_the_session_layer() {
    let rig = Rig::start().await;
    rig.connect("token").await;
    rig.remote.connect_latest_socket();
    wait_until("connected", || {
        state_of(&rig) == HubConnectionState::Connected
    })
    .await;
    assert!(!rig.remote.handed_over_closed(0));

    rig.handle.disconnect(true).await.unwrap();

    assert!(
        rig.remote.handed_over_closed(0),
        "disconnect closes the live socket, not just the finished dial"
    );

    let rig = Rig::start().await;
    rig.connect("token").await;
    rig.remote.connect_latest_socket();
    wait_until("connected", || {
        state_of(&rig) == HubConnectionState::Connected
    })
    .await;
    rig.handle.stop().await.unwrap();
    assert!(rig.remote.handed_over_closed(0));
}

#[tokio::test]
async fn connecting_hands_the_socket_and_identity_to_the_session_layer() {
    let rig = Rig::start().await;
    rig.connect("token").await;
    let before = rig.handle.status();
    assert_eq!(before.state, HubConnectionState::Connecting);
    assert_eq!(before.connected_at, None);

    rig.remote.connect_latest_socket();
    wait_until("attached", || rig.host.attach_count() == 1).await;

    let status = rig.handle.status();
    assert_eq!(status.state, HubConnectionState::Connected);
    assert_eq!(status.connected_at.as_deref(), Some(support::NOW));
    let attached = rig.host.attached.lock().unwrap();
    let options = &attached[0].options;
    let secret = rig.file().unwrap()["credential"]["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        options.principal_id,
        format!("hub:{}:{}", options.daemon_id, secret_fingerprint(&secret))
    );
    assert_eq!(options.permissions, vec![Permission::HubExecute]);
    assert_eq!(options.session_protocol, SessionProtocol::SessionV1);
}

#[tokio::test]
async fn stale_socket_generations_cannot_replace_or_unregister_the_current_socket() {
    let rig = Rig::start().await;
    rig.connect("token").await;
    rig.remote.close_socket(0, 1006);
    wait_until("reconnecting", || {
        state_of(&rig) == HubConnectionState::Reconnecting
    })
    .await;
    rig.clock.run_next();
    wait_until("second dial", || rig.remote.socket_attempts() == 2).await;
    rig.remote.connect_socket(1);
    wait_until("connected", || rig.host.attach_count() == 1).await;

    let stale_closed = rig.remote.connect_socket(0);
    rig.remote.close_socket(0, 1000);
    settle().await;

    assert!(
        stale_closed.load(Ordering::SeqCst),
        "a stale connect closes its socket"
    );
    assert_eq!(rig.host.attach_count(), 1);
    assert_eq!(state_of(&rig), HubConnectionState::Connected);
    assert_eq!(rig.remote.socket_attempts(), 2);
    assert_eq!(rig.clock.pending_tasks(), 0);
}

#[tokio::test]
async fn reconnect_backoff_counts_failures_and_resets_after_a_connection() {
    let rig = Rig::start().await;
    rig.connect("token").await;

    rig.remote.fail_socket(0, "connection refused");
    wait_until("scheduled", || rig.clock.pending_tasks() == 1).await;
    assert_eq!(
        rig.handle.status().last_error.as_deref(),
        Some("connection refused")
    );
    assert_eq!(state_of(&rig), HubConnectionState::Reconnecting);
    rig.clock.run_next();
    wait_until("dial 2", || rig.remote.socket_attempts() == 2).await;
    rig.remote.close_socket(1, 1006);
    wait_until("scheduled", || rig.clock.pending_tasks() == 1).await;
    rig.clock.run_next();
    wait_until("dial 3", || rig.remote.socket_attempts() == 3).await;
    assert_eq!(rig.clock.scheduled_attempts(), vec![0, 1]);

    rig.remote.connect_socket(2);
    wait_until("connected", || {
        state_of(&rig) == HubConnectionState::Connected
    })
    .await;
    assert_eq!(rig.handle.status().last_error, None);
    rig.remote.close_socket(2, 1006);
    wait_until("scheduled", || rig.clock.pending_tasks() == 1).await;
    assert_eq!(
        rig.clock.scheduled_attempts(),
        vec![0, 1, 0],
        "a connection resets the counter"
    );
}

#[tokio::test]
async fn a_retry_timer_that_outlives_its_generation_does_nothing() {
    let rig = Rig::start().await;
    rig.connect("token").await;
    rig.remote.close_socket(0, 1006);
    wait_until("scheduled", || rig.clock.pending_tasks() == 1).await;

    rig.handle.disconnect(true).await.unwrap();

    assert_eq!(rig.clock.pending_tasks(), 0, "disconnect cancels the timer");
    assert_eq!(rig.remote.socket_attempts(), 1);
}

#[tokio::test]
async fn stopping_cancels_timers_and_closes_the_socket() {
    let rig = Rig::start().await;
    rig.connect("token").await;
    rig.remote.close_socket(0, 1006);
    wait_until("scheduled", || rig.clock.pending_tasks() == 1).await;
    rig.clock.run_next();
    wait_until("dial 2", || rig.remote.socket_attempts() == 2).await;

    rig.handle.stop().await.unwrap();

    assert!(rig.remote.socket_closed(1));
    assert_eq!(rig.clock.pending_tasks(), 0);
}

// ---- permission updates -------------------------------------------------------------------

#[tokio::test]
async fn updating_permissions_requires_an_active_relationship() {
    let rig = Rig::start().await;
    let error = rig
        .handle
        .update_permissions(PermissionChange {
            grant: perms(&["hub.execute"]),
            revoke: vec![],
        })
        .await
        .unwrap_err();
    assert_eq!(
        error,
        HubError::message("This daemon is not connected to a Hub")
    );
}

#[tokio::test]
async fn update_permissions_applies_revoke_then_grant_and_requires_the_hub_to_agree() {
    let rig = Rig::start().await;
    rig.connect("token").await;

    let status = rig
        .handle
        .update_permissions(PermissionChange {
            grant: perms(&["daemon.read", "hub.execute"]),
            revoke: perms(&["hub.execute"]),
        })
        .await
        .unwrap();

    // `(current - revoke) + grant`: the revoke removes first, the grant appends in order.
    assert_eq!(
        status.permissions,
        vec![Permission::DaemonRead, Permission::HubExecute]
    );
    let update = rig.remote.permission_updates().remove(0);
    assert_eq!(update.permissions, perms(&["daemon.read", "hub.execute"]));
    assert_eq!(update.hub_origin, "https://hub.test");
    assert_eq!(
        update.credential,
        rig.file().unwrap()["credential"]["secret"]
            .as_str()
            .unwrap()
    );

    rig.remote.respond_to_updates_with(&["hub.execute"]);
    let error = rig
        .handle
        .update_permissions(PermissionChange {
            grant: perms(&["workspace.read"]),
            revoke: vec![],
        })
        .await
        .unwrap_err();
    assert_eq!(
        error,
        HubError::message("Hub permission response did not match the local grant")
    );
    assert_eq!(
        rig.file().unwrap()["relationship"]["permissions"],
        json!(["daemon.read", "hub.execute"]),
        "a mismatching response persists nothing"
    );

    let error = rig
        .handle
        .update_permissions(PermissionChange {
            grant: perms(&["*"]),
            revoke: vec![],
        })
        .await
        .unwrap_err();
    assert_eq!(error, HubError::message("Invalid daemon permission"));
}

#[tokio::test]
async fn parity_quirk_a_reconnect_attaches_the_permissions_the_dial_was_opened_with() {
    // PARITY-QUIRK(reconnect-uses-captured-record): the TS closure keeps the record of the
    // first dial; a permission revoked later is attached again after a reconnect.
    let rig = Rig::start().await;
    rig.connect("token").await;
    rig.remote.connect_latest_socket();
    wait_until("attached", || rig.host.attach_count() == 1).await;
    rig.handle
        .update_permissions(PermissionChange {
            grant: vec![],
            revoke: perms(&["hub.execute"]),
        })
        .await
        .unwrap();
    assert_eq!(
        rig.file().unwrap()["relationship"]["permissions"],
        json!([])
    );

    rig.remote.close_socket(0, 1006);
    wait_until("scheduled", || rig.clock.pending_tasks() == 1).await;
    rig.clock.run_next();
    wait_until("dial 2", || rig.remote.socket_attempts() == 2).await;
    rig.remote.connect_socket(1);
    wait_until("attached again", || rig.host.attach_count() == 2).await;

    assert_eq!(
        rig.host.attached_permissions()[1],
        vec![Permission::HubExecute],
        "stale permissions are attached"
    );
}

// ---- execution authority across relationship transitions ---------------------------------

async fn connected_rig() -> Rig {
    let rig = Rig::with_ceremony(fixed_daemon("daemon-1")).await;
    rig.connect("token").await;
    rig.remote.connect_latest_socket();
    wait_until("attached", || rig.host.attach_count() == 1).await;
    rig
}

fn spawn_create(
    agents: std::sync::Arc<dyn HubExecutionAgents>,
    execution_id: &str,
) -> tokio::task::JoinHandle<Result<paseo_hub::OwnedAgentSnapshot, ExecError>> {
    let input = create_input(execution_id);
    tokio::spawn(async move { agents.create(input).await })
}

#[tokio::test]
async fn replayed_create_across_socket_generations_shares_one_pending_durable_execution() {
    let rig = connected_rig().await;
    let backend = rig.host.backend.clone();
    backend.hold_creation();
    let first = spawn_create(rig.host.latest_attached_agents(), "execution-race");
    wait_until("creation", || backend.creation_attempts() == 1).await;

    rig.remote.close_latest_socket(1006);
    wait_until("scheduled", || rig.clock.pending_tasks() == 1).await;
    rig.clock.run_next();
    wait_until("dial 2", || rig.remote.socket_attempts() == 2).await;
    rig.remote.connect_latest_socket();
    wait_until("attached again", || rig.host.attach_count() == 2).await;
    let replayed = spawn_create(rig.host.latest_attached_agents(), "execution-race");
    settle().await;
    backend.finish_creation();

    let replayed = replayed.await.unwrap().unwrap();
    let first = first.await.unwrap().unwrap();
    assert_eq!(replayed.execution_id, "execution-race");
    assert_eq!(replayed.agent["id"], first.agent["id"]);
    assert_eq!(backend.creation_attempts(), 1);
    assert_eq!(backend.durable_agent_ids().len(), 1);
    assert_eq!(
        rig.host.created_agents.lock().unwrap().len(),
        1,
        "both socket generations share one execution-agents instance"
    );
}

#[tokio::test]
async fn force_disconnect_fences_a_pending_create_before_removing_authority() {
    let rig = connected_rig().await;
    let backend = rig.host.backend.clone();
    backend.hold_creation();
    let mut input = create_input("cancelled-execution");
    input.worktree = Some(paseo_hub::ExecutionWorktree {
        action: paseo_hub::WorktreeAction::BranchOff,
        worktree_name: Some("cancelled-hub-create".into()),
        base_branch: None,
        ref_name: None,
        github_pr_number: None,
    });
    let agents = rig.host.latest_attached_agents();
    let create = tokio::spawn(async move { agents.create(input).await });
    wait_until("creation", || backend.creation_attempts() == 1).await;

    let handle = rig.handle.clone();
    let disconnecting = tokio::spawn(async move { handle.disconnect(true).await });
    wait_until("authority removed", || rig.file().is_none()).await;
    backend.finish_creation();
    disconnecting.await.unwrap().unwrap();

    let error = create.await.unwrap().unwrap_err();
    assert_eq!(
        error,
        ExecError::other("Hub relationship authority ended during agent creation")
    );
    assert!(backend.live_agent_ids().is_empty());
    assert!(backend.durable_agent_ids().is_empty());
    assert!(backend.operations().iter().any(|op| matches!(
        op,
        Operation::CleanupFailedCreate(Some(worktree)) if worktree["created"] == true
    )));
}

#[tokio::test]
async fn re_enrollment_gives_the_same_daemon_fresh_execution_authority() {
    let rig = connected_rig().await;
    let first = rig
        .host
        .latest_attached_agents()
        .create(create_input("first-execution"))
        .await
        .unwrap();
    let old_agents = rig.host.latest_attached_agents();

    rig.handle.disconnect(false).await.unwrap();
    assert_eq!(
        old_agents.create(create_input("late")).await.unwrap_err(),
        ExecError::other("Hub relationship authority is no longer active"),
        "the old authority is fenced"
    );
    rig.connect("fresh-token").await;
    rig.remote.connect_latest_socket();
    wait_until("attached again", || rig.host.attach_count() == 2).await;
    let second = rig
        .host
        .latest_attached_agents()
        .create(create_input("second-execution"))
        .await
        .unwrap();

    assert_eq!(rig.file().unwrap()["relationship"]["daemonId"], "daemon-1");
    assert_eq!(first.execution_id, "first-execution");
    assert_eq!(second.execution_id, "second-execution");
    assert_eq!(rig.host.backend.durable_agent_ids().len(), 2);
    assert_eq!(rig.host.created_agents.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn daemon_shutdown_fences_a_pending_create_before_closing_owned_agents() {
    let rig = connected_rig().await;
    let backend = rig.host.backend.clone();
    backend.hold_creation();
    let create = spawn_create(rig.host.latest_attached_agents(), "execution-shutdown");
    wait_until("creation", || backend.creation_attempts() == 1).await;

    let handle = rig.handle.clone();
    let stopping = tokio::spawn(async move { handle.stop().await });
    settle().await;
    backend.finish_creation();
    stopping.await.unwrap().unwrap();

    assert!(create.await.unwrap().is_err());
    assert!(backend.durable_agent_ids().is_empty());
    assert!(backend.live_agent_ids().is_empty());
}

#[tokio::test]
async fn revocation_by_the_hub_fences_execution_authority() {
    let rig = connected_rig().await;
    let agents = rig.host.latest_attached_agents();

    rig.remote.reject_socket(0, 401);
    wait_until("revoked", || state_of(&rig) == HubConnectionState::Revoked).await;

    assert_eq!(
        agents
            .create(create_input("after-revoke"))
            .await
            .unwrap_err(),
        ExecError::other("Hub relationship authority is no longer active")
    );
}

#[tokio::test]
async fn status_serializes_to_the_ts_wire_shape() {
    let rig = Rig::start().await;
    assert_eq!(
        serde_json::to_value(rig.handle.status()).unwrap(),
        json!({
            "state": "not_connected",
            "daemonId": null,
            "hubOrigin": null,
            "permissions": [],
            "connectedAt": null,
            "lastError": null
        })
    );
    rig.connect("token").await;
    let status = serde_json::to_value(rig.handle.status()).unwrap();
    assert_eq!(status["state"], "connecting");
    assert_eq!(status["permissions"], json!(["hub.execute"]));
    assert_eq!(status["hubOrigin"], "https://hub.test");
}
