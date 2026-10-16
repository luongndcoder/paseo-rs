//! Real child-process transport, against a few lines of `sh` posing as an agent.
#![cfg(unix)]

use std::collections::BTreeMap;

use paseo_acp::agent::{CreateSessionOptions, SessionConfig};
use paseo_acp::connection::{Connection, Incoming};
use paseo_acp::launch::LaunchSpec;
use paseo_acp::protocol::{ClientCapabilities, Implementation, InitializeParams, PROTOCOL_VERSION};
use paseo_acp::providers::generic::{generic_client, GenericOptions};
use paseo_acp::replay::Fixture;
use paseo_acp::transport::{StdioTransport, StdioTransportFactory, Transport};
use serde_json::json;

fn init_params() -> InitializeParams {
    InitializeParams {
        protocol_version: PROTOCOL_VERSION,
        client_capabilities: ClientCapabilities::default(),
        client_info: Implementation {
            name: "Paseo".into(),
            version: "dev".into(),
        },
    }
}

/// Answers `initialize`, then `session/new`, ignoring everything else; prints one garbage line first.
const AGENT: &str = r#"
echo 'not json at all'
read line
printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{}}}'
read line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"sessionId":"sh-1"}}'
echo "agent stderr line" 1>&2
cat > /dev/null
"#;

#[tokio::test]
async fn stdio_transport_speaks_json_lines_and_ignores_non_json_stdout() {
    let spec = LaunchSpec::new("sh", ["-c", AGENT]);
    let handle = Box::new(StdioTransport::new(spec)).start().unwrap();
    let (conn, mut incoming) = Connection::start(handle);
    let init = conn.initialize(&init_params()).await.unwrap();
    assert_eq!(init.protocol_version, 1);
    // The garbage line was reported, not fatal.
    match incoming.recv().await.unwrap() {
        Incoming::Malformed(l) => assert_eq!(l, "not json at all"),
        other => panic!("{other:?}"),
    }
    let s = conn
        .new_session(&paseo_acp::protocol::NewSessionParams {
            cwd: "/tmp".into(),
            mcp_servers: vec![],
        })
        .await
        .unwrap();
    assert_eq!(s.session_id.as_deref(), Some("sh-1"));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(conn.process().stderr().contains("agent stderr line"));

    // Closing stdin ends the agent gracefully (`cat` sees EOF); the pending stream reports Closed.
    conn.process()
        .terminate(
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(2),
        )
        .await;
    let exit = conn.process().exit().expect("process exited");
    assert_eq!(exit.code, Some(0));
    assert!(matches!(incoming.recv().await, Some(Incoming::Closed)));
    assert!(conn
        .new_session(&paseo_acp::protocol::NewSessionParams {
            cwd: "/tmp".into(),
            mcp_servers: vec![]
        })
        .await
        .is_err());
}

#[tokio::test]
async fn terminate_kills_an_agent_that_ignores_stdin_eof() {
    let spec = LaunchSpec::new(
        "sh",
        ["-c", "trap '' HUP; read line; while true; do sleep 1; done"],
    );
    let handle = Box::new(StdioTransport::new(spec)).start().unwrap();
    let (conn, _incoming) = Connection::start(handle);
    let started = std::time::Instant::now();
    conn.process()
        .terminate(
            std::time::Duration::from_millis(200),
            std::time::Duration::from_secs(5),
        )
        .await;
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
    let exit = conn.process().exit().expect("killed");
    assert_eq!(exit.code, None);
    assert_eq!(exit.signal, Some(9));
}

#[tokio::test]
async fn spawn_failure_is_an_io_error() {
    let spec = LaunchSpec::new("/definitely/not/a/binary", Vec::<String>::new());
    let err = Box::new(StdioTransport::new(spec))
        .start()
        .err()
        .expect("spawn must fail");
    assert!(matches!(err, paseo_acp::Error::Io(_)));
}

#[tokio::test]
async fn tee_records_traffic_in_the_fixture_format() {
    let dir = std::env::temp_dir().join(format!("paseo-acp-tee-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let factory = std::sync::Arc::new(StdioTransportFactory::with_tee_dir(&dir));
    let mut options = GenericOptions::new(vec!["sh".into(), "-c".into(), AGENT.into()]);
    options.env = BTreeMap::from([("SECRET_TOKEN".into(), "do-not-record-me".into())]);
    let client = generic_client(options).with_transport_factory(factory);
    let session = client
        .create_session(
            SessionConfig {
                cwd: "/tmp".into(),
                ..Default::default()
            },
            CreateSessionOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(session.id().as_deref(), Some("sh-1"));
    session.close().await;

    let path = std::fs::read_dir(&dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("do-not-record-me"),
        "env values are never recorded"
    );
    assert!(text.contains("SECRET_TOKEN"), "env keys are");
    let fixture = Fixture::load(&path).unwrap();
    let inbound: Vec<_> = fixture
        .inbound()
        .map(|v| v["method"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(inbound, ["initialize", "session/new"]);
    assert_eq!(fixture.outbound().count(), 2);
    assert_eq!(
        fixture.outbound().last().unwrap()["result"],
        json!({"sessionId": "sh-1"})
    );
    assert!(matches!(
        fixture.entries.last(),
        Some(paseo_acp::replay::Entry::Exit(_))
    ));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn cancelled_open_kills_the_agent_process() {
    let marker = "sleep 7771913";
    let client = generic_client(GenericOptions::new(vec!["sleep".into(), "7771913".into()]));
    let r = tokio::time::timeout(
        std::time::Duration::from_millis(300),
        client.fetch_catalog(Some("/tmp")),
    )
    .await;
    assert!(r.is_err());
    let alive = || {
        !std::process::Command::new("pgrep")
            .args(["-f", marker])
            .output()
            .unwrap()
            .stdout
            .is_empty()
    };
    for _ in 0..40 {
        if !alive() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let _ = std::process::Command::new("pkill")
        .args(["-f", marker])
        .status();
    panic!("agent process still alive after the open future was dropped");
}
