//! `relationship-remote.test.ts`: the network remote against local servers, then the
//! relationship actor driving the real remote.
#![allow(clippy::result_large_err)] // tungstenite handshake callback signature fixes ErrorResponse

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use paseo_hub::{
    DirectHubRelationshipRemote, DirectHubSocket, HubConnectionState, HubEnrollment, HubOptions,
    HubPermissionUpdate, HubRelationshipActor, HubRemote, HubRevocation, HubSocket,
    HubSocketCredentials, HubSocketEvents, RemoteError, SessionProtocol,
};
use serde_json::json;
use support::http::{RawServer, Reply, UpgradeBehavior, UpgradeRejectingHub};
use support::{wait_until, FakeBackend, FakeHost, ManualClock, RecordingLogger};

fn enrollment(hub_origin: &str) -> HubEnrollment {
    HubEnrollment {
        daemon_id: "daemon-1".into(),
        idempotency_key: "ceremony-1".into(),
        hub_origin: hub_origin.into(),
        token: "token".into(),
        hostname: "test-daemon.local".into(),
        server_id: "server-1".into(),
        daemon_public_key: "public-key".into(),
        credential_verifier: "verifier".into(),
        permissions: vec!["hub.execute".into()],
    }
}

fn revocation(hub_origin: &str) -> HubRevocation {
    HubRevocation {
        daemon_id: "daemon-1".into(),
        hub_origin: hub_origin.into(),
        credential: "credential".into(),
    }
}

async fn hub_returning(status: u16) -> RawServer {
    RawServer::start(move |_| Reply::Status(status)).await
}

async fn enrollment_hub(web_socket_url: &'static str) -> RawServer {
    RawServer::start(move |request| {
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        Reply::Json(
            200,
            json!({ "daemonId": body["daemonId"], "permissions": ["hub.execute"], "webSocketUrl": web_socket_url })
                .to_string(),
        )
    })
    .await
}

// ---- HTTP ---------------------------------------------------------------------------------

#[tokio::test]
async fn revocation_statuses_that_mean_the_authority_is_already_gone_succeed() {
    for status in [200, 204, 401, 403, 404] {
        let hub = hub_returning(status).await;
        let remote = DirectHubRelationshipRemote::new();
        remote
            .revoke(revocation(&hub.origin()))
            .await
            .unwrap_or_else(|e| panic!("{status}: {e}"));
    }
}

#[tokio::test]
async fn transient_revocation_failures_remain_retryable() {
    for status in [500, 503, 408, 429] {
        let hub = hub_returning(status).await;
        let error = DirectHubRelationshipRemote::new()
            .revoke(revocation(&hub.origin()))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            RemoteError::failed(format!("Hub revocation failed ({status})"))
        );
    }
}

#[tokio::test]
async fn revocation_sends_a_delete_with_the_daemon_credential_to_the_encoded_path() {
    let hub = hub_returning(204).await;
    let mut input = revocation(&hub.origin());
    input.daemon_id = "daemon/1 é".into();

    DirectHubRelationshipRemote::new()
        .revoke(input)
        .await
        .unwrap();

    let request = hub.requests().remove(0);
    assert_eq!(request.method, "DELETE");
    assert_eq!(request.path, "/api/daemons/daemon%2F1%20%C3%A9");
    assert_eq!(request.headers["authorization"], "Bearer credential");
}

#[tokio::test]
async fn permission_updates_use_the_daemon_credential_and_preserve_semantic_permissions() {
    let hub = RawServer::start(|_| {
        Reply::Json(200, json!({ "permissions": ["hub.execute"] }).to_string())
    })
    .await;

    let result = DirectHubRelationshipRemote::new()
        .update_permissions(HubPermissionUpdate {
            daemon_id: "daemon-1".into(),
            hub_origin: hub.origin(),
            credential: "daemon-secret".into(),
            permissions: vec!["hub.execute".into()],
        })
        .await
        .unwrap();

    assert_eq!(result, vec!["hub.execute".to_owned()]);
    let request = hub.requests().remove(0);
    assert_eq!(request.method, "PATCH");
    assert_eq!(request.path, "/api/daemons/daemon-1");
    assert_eq!(request.headers["authorization"], "Bearer daemon-secret");
    assert_eq!(request.headers["content-type"], "application/json");
    assert_eq!(request.body, r#"{"permissions":["hub.execute"]}"#);
}

#[tokio::test]
async fn a_failed_or_malformed_permission_update_is_an_error() {
    let hub = hub_returning(500).await;
    let update = |origin: String| HubPermissionUpdate {
        daemon_id: "daemon-1".into(),
        hub_origin: origin,
        credential: "c".into(),
        permissions: vec![],
    };
    let remote = DirectHubRelationshipRemote::new();
    assert_eq!(
        remote
            .update_permissions(update(hub.origin()))
            .await
            .unwrap_err(),
        RemoteError::failed("Hub permission update failed (500)")
    );
    let hub =
        RawServer::start(|_| Reply::Json(200, json!({ "permissions": "all" }).to_string())).await;
    assert!(remote
        .update_permissions(update(hub.origin()))
        .await
        .is_err());
}

#[tokio::test]
async fn enrollment_posts_the_ceremony_with_the_bearer_token() {
    let hub = enrollment_hub("ws://127.0.0.1:1/daemon").await;

    let result = DirectHubRelationshipRemote::new()
        .enroll(enrollment(&hub.origin()))
        .await;

    // The ws URL points at another origin, so the result is rejected; the request itself is
    // what this test looks at.
    assert_eq!(
        result.unwrap_err(),
        RemoteError::failed("Hub WebSocket URL must match the Hub origin")
    );
    let request = hub.requests().remove(0);
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/api/daemons/enroll");
    assert_eq!(request.headers["authorization"], "Bearer token");
    assert_eq!(request.headers["content-type"], "application/json");
    assert_eq!(
        request.body,
        r#"{"daemonId":"daemon-1","idempotencyKey":"ceremony-1","hostname":"test-daemon.local","serverId":"server-1","daemonPublicKey":"public-key","credentialVerifier":"verifier","permissions":["hub.execute"]}"#
    );
}

#[tokio::test]
async fn enrollment_returns_the_hub_answer_when_it_names_the_hub_origin() {
    let hub = RawServer::start(|request| {
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        let host = request.headers["host"].clone();
        Reply::Json(
            200,
            json!({ "daemonId": body["daemonId"], "permissions": ["hub.execute"], "webSocketUrl": format!("ws://{host}/daemon") })
                .to_string(),
        )
    })
    .await;

    let result = DirectHubRelationshipRemote::new()
        .enroll(enrollment(&hub.origin()))
        .await
        .unwrap();

    assert_eq!(result.daemon_id, "daemon-1");
    assert_eq!(result.permissions, vec!["hub.execute".to_owned()]);
    assert_eq!(
        result.web_socket_url,
        format!("ws://127.0.0.1:{}/daemon", hub.addr.port())
    );
}

#[tokio::test]
async fn transient_enrollment_statuses_remain_retryable_and_auth_statuses_are_terminal() {
    for status in [408, 429, 500, 503] {
        let hub = hub_returning(status).await;
        let error = DirectHubRelationshipRemote::new()
            .enroll(enrollment(&hub.origin()))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            RemoteError::failed(format!("Hub enrollment failed ({status})"))
        );
    }
    for status in [401, 403] {
        let hub = hub_returning(status).await;
        let error = DirectHubRelationshipRemote::new()
            .enroll(enrollment(&hub.origin()))
            .await
            .unwrap_err();
        assert_eq!(error, RemoteError::EnrollmentRejected { status });
        assert_eq!(
            error.to_string(),
            format!("Hub enrollment failed ({status})")
        );
    }
}

#[tokio::test]
async fn enrollment_rejects_a_transport_url_that_cannot_open_a_websocket() {
    let hub = enrollment_hub("ftp://hub.test/daemon").await;
    let error = DirectHubRelationshipRemote::new()
        .enroll(enrollment(&hub.origin()))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        RemoteError::failed("Hub WebSocket URL must use ws or wss")
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&hub.requests()[0].body).unwrap()["hostname"],
        "test-daemon.local"
    );
}

#[tokio::test]
async fn enrollment_rejects_a_websocket_url_with_a_fragment() {
    let hub = enrollment_hub("ws://hub.test/daemon#fragment").await;
    let error = DirectHubRelationshipRemote::new()
        .enroll(enrollment(&hub.origin()))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        RemoteError::failed("Hub WebSocket URL cannot include a fragment")
    );
}

#[tokio::test]
async fn enrollment_rejects_a_websocket_outside_the_enrolled_hub_authority() {
    let hub = enrollment_hub("ws://other-hub.test/daemon").await;
    let error = DirectHubRelationshipRemote::new()
        .enroll(enrollment(&hub.origin()))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        RemoteError::failed("Hub WebSocket URL must match the Hub origin")
    );
}

#[tokio::test]
async fn enrollment_rejects_a_malformed_answer() {
    for body in [
        "not json",
        "[]",
        r#"{"daemonId":1,"permissions":[],"webSocketUrl":"ws://x"}"#,
        r#"{"daemonId":"d","permissions":"x","webSocketUrl":"ws://x"}"#,
        r#"{"daemonId":"d","permissions":[]}"#,
    ] {
        let hub = RawServer::start(move |_| Reply::Json(200, body.into())).await;
        assert!(
            DirectHubRelationshipRemote::new()
                .enroll(enrollment(&hub.origin()))
                .await
                .is_err(),
            "{body}"
        );
    }
}

#[tokio::test]
async fn http_calls_are_bounded() {
    let hub = RawServer::start(|_| Reply::Stall).await;
    let remote = DirectHubRelationshipRemote::with_request_timeout(Duration::from_millis(25));

    let enroll = remote.enroll(enrollment(&hub.origin())).await.unwrap_err();
    let revoke = remote.revoke(revocation(&hub.origin())).await.unwrap_err();
    let update = remote
        .update_permissions(HubPermissionUpdate {
            daemon_id: "daemon-1".into(),
            hub_origin: hub.origin(),
            credential: "c".into(),
            permissions: vec![],
        })
        .await
        .unwrap_err();

    for error in [enroll, revoke, update] {
        assert_eq!(error, RemoteError::failed("Hub request timed out"));
    }
}

// ---- socket -------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Connected(SessionProtocol),
    Rejected(u16),
    Closed(u16),
    Failed(String),
}

#[derive(Default)]
struct Outcomes {
    seen: Mutex<Vec<Outcome>>,
    sockets: Mutex<Vec<Box<dyn HubSocket>>>,
}

impl HubSocketEvents for Outcomes {
    fn connected(&self, socket: Box<dyn HubSocket>, protocol: SessionProtocol) {
        self.sockets.lock().unwrap().push(socket);
        self.seen.lock().unwrap().push(Outcome::Connected(protocol));
    }
    fn rejected(&self, status: u16) {
        self.seen.lock().unwrap().push(Outcome::Rejected(status));
    }
    fn closed(&self, code: u16) {
        self.seen.lock().unwrap().push(Outcome::Closed(code));
    }
    fn failed(&self, message: String) {
        self.seen.lock().unwrap().push(Outcome::Failed(message));
    }
}

impl Outcomes {
    async fn first(&self) -> Outcome {
        wait_until("a socket outcome", || !self.seen.lock().unwrap().is_empty()).await;
        self.seen.lock().unwrap()[0].clone()
    }

    async fn after_cleanup(&self) -> Vec<Outcome> {
        tokio::time::sleep(Duration::from_millis(30)).await;
        self.seen.lock().unwrap().clone()
    }
}

fn credentials(url: &str) -> HubSocketCredentials {
    HubSocketCredentials {
        daemon_id: "daemon-1".into(),
        web_socket_url: url.into(),
        credential: "credential".into(),
    }
}

#[tokio::test]
async fn negotiates_the_session_protocol_during_the_websocket_upgrade() {
    for (acknowledge, expected) in [
        (false, SessionProtocol::Legacy),
        (true, SessionProtocol::SessionV1),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let offered: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let seen = offered.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let callback = move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                                 mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                let header = |name: &str| {
                    request.headers().get(name).map(|v| v.to_str().unwrap().to_owned()).unwrap_or_default()
                };
                *seen.lock().unwrap() = vec![
                    ("x-paseo-session-protocol".into(), header("x-paseo-session-protocol")),
                    ("authorization".into(), header("authorization")),
                    ("x-paseo-daemon-id".into(), header("x-paseo-daemon-id")),
                ];
                if acknowledge {
                    response.headers_mut().insert("x-paseo-session-protocol", "1".parse().unwrap());
                }
                Ok(response)
            };
            let mut socket = tokio_tungstenite::accept_hdr_async(stream, callback)
                .await
                .unwrap();
            // Echo one message so the test can prove the handed-over socket works.
            use futures_util::{SinkExt, StreamExt};
            if let Some(Ok(message)) = socket.next().await {
                let _ = socket.send(message).await;
            }
            let _ = socket.next().await;
        });

        let outcomes = Arc::new(Outcomes::default());
        let remote = DirectHubRelationshipRemote::new();
        let connection = remote.open_socket(
            credentials(&format!("ws://127.0.0.1:{port}/daemon")),
            outcomes.clone(),
        );

        assert_eq!(outcomes.first().await, Outcome::Connected(expected));
        assert_eq!(
            *offered.lock().unwrap(),
            vec![
                ("x-paseo-session-protocol".to_owned(), "1".to_owned()),
                ("authorization".to_owned(), "Bearer credential".to_owned()),
                ("x-paseo-daemon-id".to_owned(), "daemon-1".to_owned()),
            ]
        );
        let socket = outcomes.sockets.lock().unwrap().remove(0);
        let mut direct = socket
            .into_any()
            .downcast::<DirectHubSocket>()
            .expect("a tungstenite socket");
        direct
            .outgoing
            .send(tokio_tungstenite::tungstenite::Message::text("ping"))
            .unwrap();
        let echoed = direct.incoming.recv().await.unwrap();
        assert_eq!(echoed.into_text().unwrap().as_str(), "ping");
        connection.close();
    }
}

#[tokio::test]
async fn socket_authentication_statuses_are_terminal_and_release_the_failed_upgrade() {
    for status in [401u16, 403] {
        let hub = UpgradeRejectingHub::start(vec![UpgradeBehavior::Status(status)]).await;
        let outcomes = Arc::new(Outcomes::default());

        let _socket = DirectHubRelationshipRemote::new()
            .open_socket(credentials(&hub.web_socket_url()), outcomes.clone());

        assert_eq!(outcomes.first().await, Outcome::Rejected(status));
        hub.expect_attempt_released(1).await;
        assert_eq!(
            outcomes.after_cleanup().await,
            vec![Outcome::Rejected(status)]
        );
        assert_eq!(hub.open_connections(), 0);
    }
}

#[tokio::test]
async fn a_transient_socket_response_releases_the_failed_upgrade_and_reports_one_connection_loss() {
    let hub = UpgradeRejectingHub::start(vec![UpgradeBehavior::Status(503)]).await;
    let outcomes = Arc::new(Outcomes::default());

    let _socket = DirectHubRelationshipRemote::new()
        .open_socket(credentials(&hub.web_socket_url()), outcomes.clone());

    assert_eq!(outcomes.first().await, Outcome::Closed(1006));
    hub.expect_attempt_released(1).await;
    assert_eq!(outcomes.after_cleanup().await, vec![Outcome::Closed(1006)]);
    assert_eq!(hub.open_connections(), 0);
}

#[tokio::test]
async fn a_stalled_socket_opening_handshake_times_out_and_releases_the_connection() {
    let hub = UpgradeRejectingHub::start(vec![UpgradeBehavior::Stall]).await;
    let outcomes = Arc::new(Outcomes::default());

    let _socket = DirectHubRelationshipRemote::with_request_timeout(Duration::from_millis(25))
        .open_socket(credentials(&hub.web_socket_url()), outcomes.clone());

    let expected = Outcome::Failed("Opening handshake has timed out".into());
    assert_eq!(outcomes.first().await, expected);
    assert_eq!(outcomes.after_cleanup().await, vec![expected]);
    hub.expect_attempt_released(1).await;
}

#[tokio::test]
async fn closing_a_dial_in_progress_releases_the_connection_and_reports_nothing() {
    let hub = UpgradeRejectingHub::start(vec![UpgradeBehavior::Stall]).await;
    let outcomes = Arc::new(Outcomes::default());
    let socket = DirectHubRelationshipRemote::new()
        .open_socket(credentials(&hub.web_socket_url()), outcomes.clone());
    wait_until("the hub saw the upgrade", || hub.attempt_count() == 1).await;

    socket.close();

    hub.expect_attempt_released(1).await;
    assert!(outcomes.after_cleanup().await.is_empty());
}

// ---- the actor over the real remote ------------------------------------------------------

async fn connect_controller(
    hub: &UpgradeRejectingHub,
    clock: &Arc<ManualClock>,
) -> (paseo_hub::HubHandle, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let host = FakeHost::new(FakeBackend::new());
    let handle = HubRelationshipActor::spawn(HubOptions {
        paseo_home: home.path().to_path_buf(),
        hostname: "test-daemon.local".into(),
        server_id: "server-1".into(),
        daemon_public_key: "daemon-public-key".into(),
        logger: Arc::new(RecordingLogger::default()),
        remote: Arc::new(DirectHubRelationshipRemote::new()),
        clock: clock.clone(),
        retry_policy: clock.clone(),
        create_ceremony: None,
        host,
    })
    .unwrap();
    handle
        .connect(paseo_hub::ConnectInput {
            hub_url: hub.origin(),
            token: "enrollment-token".into(),
            permissions: vec!["hub.execute".into()],
        })
        .await
        .unwrap();
    (handle, home)
}

#[tokio::test]
async fn controller_treats_socket_authentication_statuses_as_terminal_without_redialing() {
    for status in [401u16, 403] {
        let hub = UpgradeRejectingHub::start(vec![UpgradeBehavior::Status(status)]).await;
        let clock = ManualClock::new();
        let (handle, _home) = connect_controller(&hub, &clock).await;

        hub.expect_attempt_released(1).await;
        wait_until("revoked", || {
            handle.status().state == HubConnectionState::Revoked
        })
        .await;

        assert_eq!(
            handle.status().last_error,
            Some(format!("Hub rejected socket authentication ({status})"))
        );
        assert_eq!(hub.attempt_count(), 1);
        assert_eq!(clock.pending_tasks(), 0);
        assert_eq!(hub.open_connections(), 0);
    }
}

#[tokio::test]
async fn controller_redials_after_a_transient_socket_response_and_releases_both_failed_upgrades() {
    let hub = UpgradeRejectingHub::start(vec![
        UpgradeBehavior::Status(503),
        UpgradeBehavior::Status(401),
    ])
    .await;
    let clock = ManualClock::new();
    let (handle, _home) = connect_controller(&hub, &clock).await;

    hub.expect_attempt_released(1).await;
    wait_until("reconnecting", || {
        handle.status().state == HubConnectionState::Reconnecting
    })
    .await;
    wait_until("one timer", || clock.pending_tasks() == 1).await;

    clock.run_next();
    wait_until("second attempt", || hub.attempt_count() == 2).await;
    hub.expect_attempt_released(2).await;
    wait_until("revoked", || {
        handle.status().state == HubConnectionState::Revoked
    })
    .await;

    assert_eq!(hub.attempt_count(), 2);
    assert_eq!(clock.pending_tasks(), 0);
    assert_eq!(hub.open_connections(), 0);
}

#[tokio::test]
async fn controller_redials_once_after_a_failed_upgrade_without_also_handling_its_close() {
    let hub =
        UpgradeRejectingHub::start(vec![UpgradeBehavior::Destroy, UpgradeBehavior::Status(401)])
            .await;
    let clock = ManualClock::new();
    let (handle, _home) = connect_controller(&hub, &clock).await;

    hub.expect_attempt_released(1).await;
    wait_until("reconnecting", || {
        handle.status().state == HubConnectionState::Reconnecting
    })
    .await;

    assert_eq!(clock.scheduled_attempts(), vec![0]);
    assert_eq!(clock.pending_tasks(), 1);

    clock.run_next();
    wait_until("second attempt", || hub.attempt_count() == 2).await;
    hub.expect_attempt_released(2).await;
    wait_until("revoked", || {
        handle.status().state == HubConnectionState::Revoked
    })
    .await;

    assert_eq!(hub.attempt_count(), 2);
    assert_eq!(clock.pending_tasks(), 0);
}

// ---- the connected socket keeps reporting ---------------------------------------------------

/// A Hub that upgrades one connection, then runs `after` on the server side of the socket.
async fn hub_then<F, Fut>(after: F) -> String
where
    F: FnOnce(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        after(socket).await;
    });
    format!("ws://127.0.0.1:{port}/daemon")
}

async fn connected_outcomes(url: &str) -> (Arc<Outcomes>, Box<dyn paseo_hub::HubSocketConnection>) {
    let outcomes = Arc::new(Outcomes::default());
    let dial = DirectHubRelationshipRemote::new().open_socket(credentials(url), outcomes.clone());
    assert!(matches!(outcomes.first().await, Outcome::Connected(_)));
    (outcomes, dial)
}

#[tokio::test]
async fn an_established_socket_reports_a_hub_close_code() {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
    let url = hub_then(|mut socket| async move {
        let _ = socket
            .send(tokio_tungstenite::tungstenite::Message::Close(Some(
                CloseFrame {
                    code: CloseCode::Library(4403),
                    reason: "revoked".into(),
                },
            )))
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    })
    .await;

    let (outcomes, _dial) = connected_outcomes(&url).await;

    wait_until("closed 4403", || outcomes.seen.lock().unwrap().len() == 2).await;
    assert_eq!(outcomes.seen.lock().unwrap()[1], Outcome::Closed(4403));
}

#[tokio::test]
async fn an_established_socket_reports_a_dropped_connection() {
    let url = hub_then(|socket| async move { drop(socket) }).await;

    let (outcomes, _dial) = connected_outcomes(&url).await;

    wait_until("loss reported", || outcomes.seen.lock().unwrap().len() == 2).await;
    assert!(matches!(
        outcomes.seen.lock().unwrap()[1],
        Outcome::Closed(_) | Outcome::Failed(_)
    ));
}

#[tokio::test]
async fn the_close_handle_closes_an_established_socket_without_reporting() {
    use futures_util::StreamExt;
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let url = hub_then(|mut socket| async move {
        let end = socket.next().await;
        let _ = seen_tx.send(matches!(
            end,
            Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) | None | Some(Err(_))
        ));
    })
    .await;
    let (outcomes, _dial) = connected_outcomes(&url).await;
    let socket = outcomes.sockets.lock().unwrap().remove(0);

    socket.close_handle().close();

    assert!(seen_rx.await.unwrap(), "the Hub sees the socket end");
    assert_eq!(
        outcomes.after_cleanup().await.len(),
        1,
        "only `connected` was reported"
    );
}

#[tokio::test]
async fn controller_reconnects_after_an_established_socket_drops_and_revokes_on_4403() {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
    // One Hub on one port: answers the enrollment POST, then upgrades the daemon socket and
    // closes it with 4403 after the daemon connected.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut head = [0u8; 4];
                if stream.peek(&mut head).await.unwrap_or(0) == 4 && &head == b"POST" {
                    let mut raw = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let n = stream.read(&mut chunk).await.unwrap();
                        raw.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&raw).to_string();
                        if let Some(split) = text.find("\r\n\r\n") {
                            let length = text[..split]
                                .to_lowercase()
                                .lines()
                                .find_map(|l| {
                                    l.strip_prefix("content-length: ")
                                        .map(|v| v.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            if raw.len() >= split + 4 + length {
                                let body: serde_json::Value =
                                    serde_json::from_str(&text[split + 4..]).unwrap();
                                let answer = json!({
                                    "daemonId": body["daemonId"],
                                    "permissions": ["hub.execute"],
                                    "webSocketUrl": format!("ws://127.0.0.1:{port}/daemon"),
                                })
                                .to_string();
                                let reply = format!(
                                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
                                    answer.len()
                                );
                                stream.write_all(reply.as_bytes()).await.unwrap();
                                return;
                            }
                        }
                    }
                }
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = socket
                    .send(tokio_tungstenite::tungstenite::Message::Close(Some(
                        CloseFrame {
                            code: CloseCode::Library(4403),
                            reason: "revoked".into(),
                        },
                    )))
                    .await;
                tokio::time::sleep(Duration::from_millis(200)).await;
            });
        }
    });
    let hub_origin = format!("http://127.0.0.1:{port}");
    let clock = ManualClock::new();
    let home = tempfile::tempdir().unwrap();
    let handle = HubRelationshipActor::spawn(HubOptions {
        paseo_home: home.path().to_path_buf(),
        hostname: "test-daemon.local".into(),
        server_id: "server-1".into(),
        daemon_public_key: "daemon-public-key".into(),
        logger: Arc::new(RecordingLogger::default()),
        remote: Arc::new(DirectHubRelationshipRemote::new()),
        clock: clock.clone(),
        retry_policy: clock.clone(),
        create_ceremony: None,
        host: FakeHost::new(FakeBackend::new()),
    })
    .unwrap();
    handle
        .connect(paseo_hub::ConnectInput {
            hub_url: hub_origin,
            token: "enrollment-token".into(),
            permissions: vec!["hub.execute".into()],
        })
        .await
        .unwrap();

    wait_until("revoked by close 4403", || {
        handle.status().state == HubConnectionState::Revoked
    })
    .await;
    assert_eq!(
        handle.status().last_error.as_deref(),
        Some("Hub revoked this relationship")
    );
}
