use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::config::{BackendConfig, RestartPolicy};
use crate::internal::LifecycleEvent;

/// Fake backend health endpoint: answers every connection `200`.
async fn fake_health() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
                    .await;
            });
        }
    });
    port
}

fn sh(script: &str, port: u16) -> BackendConfig {
    let mut cfg = BackendConfig::new(vec!["sh".into(), "-c".into(), script.into()]);
    cfg.internal_port = Some(port);
    cfg.shutdown_grace = Duration::from_secs(3);
    cfg.restart = RestartPolicy {
        base_backoff: Duration::from_millis(10),
        max_backoff: Duration::from_millis(20),
        crash_window: Duration::from_secs(60),
        max_crashes: 3,
    };
    cfg
}

const READY: &str =
    r#"echo "@@paseo {\"type\":\"paseo:ready\",\"listen\":\"x\",\"serverId\":\"s1\"}""#;

#[tokio::test]
async fn becomes_ready_then_shuts_down() {
    let port = fake_health().await;
    let backend = Backend::spawn(&sh(&format!("{READY}; cat"), port), "127.0.0.1:6767")
        .await
        .unwrap();
    let info = tokio::time::timeout(Duration::from_secs(10), backend.wait_ready())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info.server_id, "s1");
    assert_eq!(
        backend.internal_ws_url().unwrap(),
        format!("ws://127.0.0.1:{port}/ws")
    );
    tokio::time::timeout(Duration::from_secs(10), backend.shutdown())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn worker_gets_internal_env_and_token_is_random() {
    let port = fake_health().await;
    let script = r#"echo "@@paseo {\"type\":\"paseo:ready\",\"listen\":\"x\",\"serverId\":\"$PASEO_EDGE_TOKEN|$PASEO_LISTEN|$PASEO_RELAY_ENABLED|$PASEO_EDGE_PUBLIC_LISTEN|$PASEO_CORS_ORIGINS\"}"; cat"#;
    let backend = Backend::spawn(&sh(script, port), "127.0.0.1:6767")
        .await
        .unwrap();
    let info = tokio::time::timeout(Duration::from_secs(10), backend.wait_ready())
        .await
        .unwrap()
        .unwrap();
    let parts: Vec<&str> = info.server_id.split('|').collect();
    assert_eq!(parts[0], backend.edge_token());
    assert_eq!(backend.edge_token().len(), 43);
    assert_eq!(parts[1], format!("127.0.0.1:{port}"));
    assert_eq!(parts[2], "", "relay env must not be forced for the worker");
    assert_eq!(parts[3], "127.0.0.1:6767");
    assert!(parts[4].contains("http://localhost:6767"));
    assert!(parts[4].contains("http://127.0.0.1:6767"));
    backend.shutdown().await.unwrap();
}

#[tokio::test]
async fn repeated_crashes_fail_the_backend() {
    let port = fake_health().await;
    let backend = Backend::spawn(&sh("exit 3", port), "127.0.0.1:6767")
        .await
        .unwrap();
    let err = tokio::time::timeout(Duration::from_secs(10), backend.wait_ready())
        .await
        .unwrap()
        .unwrap_err();
    assert!(err.to_string().contains("crashed"), "{err}");
}

#[tokio::test]
async fn ready_timeout_fails_and_kills_worker() {
    let port = fake_health().await;
    let mut cfg = sh("sleep 30", port);
    cfg.ready_timeout = Duration::from_millis(300);
    let backend = Backend::spawn(&cfg, "127.0.0.1:6767").await.unwrap();
    let err = tokio::time::timeout(Duration::from_secs(10), backend.wait_ready())
        .await
        .unwrap()
        .unwrap_err();
    assert!(err.to_string().contains("ready"), "{err}");
}

#[tokio::test]
async fn worker_shutdown_request_is_forwarded_and_not_respawned() {
    let port = fake_health().await;
    let script = format!(
        r#"{READY}; sleep 0.3; echo "@@paseo {{\"type\":\"paseo:shutdown\",\"reason\":\"bye\"}}"; exit 0"#
    );
    let backend = Backend::spawn(&sh(&script, port), "127.0.0.1:6767")
        .await
        .unwrap();
    let mut life = backend.subscribe_lifecycle();
    let event = tokio::time::timeout(Duration::from_secs(10), life.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        LifecycleEvent::Shutdown {
            reason: Some("bye".into())
        }
    );
    let mut state = backend.state();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(&*state.borrow_and_update(), BackendState::Failed { .. }) {
            state.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[test]
fn public_origins_cover_loopback_and_bound_host() {
    assert_eq!(
        public_origins("127.0.0.1:6767"),
        vec!["http://localhost:6767", "http://127.0.0.1:6767"]
    );
    assert!(public_origins("192.168.1.5:80").contains(&"http://192.168.1.5:80".to_string()));
    assert_eq!(public_origins("0.0.0.0:80").len(), 2);
    assert!(public_origins("/tmp/p.sock").is_empty());
}
