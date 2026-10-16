//! The real process transport against a stand-in `app-server` (a POSIX shell script), so the
//! spawn / framing / exit path is exercised without Codex installed.

#![cfg(unix)]

use std::time::Duration;

use paseo_codex::options::CodexOptions;
use paseo_codex::rpc::{RpcClient, RpcEvent};
use paseo_codex::transport::TransportFactory;
use paseo_codex::Error;
use serde_json::json;

// Answers `initialize` and `ping`, pushes a notification whose string holds raw U+2028, ignores
// everything else, and writes to stderr when stdin closes.
const SCRIPT: &str = r#"
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{"userAgent":"codex_cli_rs/0.160.0"}}' ;;
    *'"method":"ping"'*)
      printf '{"method":"x/y","params":{"u":"a\342\200\250b"},"emittedAtMs":7}\n'
      printf '%s\n' '{"id":2,"result":{"pong":true}}'
      printf '%s\n' '{"id":9,"method":"item/tool/call","params":{}}'
      ;;
    *'"method":"boom"'*) printf '%s\n' '{"id":3,"error":{"code":-32600,"message":"no active turn to interrupt"}}' ;;
  esac
done
echo "stdin closed" >&2
"#;

fn factory() -> paseo_codex::transport::ProcessFactory {
    CodexOptions::new()
        .command("sh")
        .prefix_args(["-c", SCRIPT, "sh"])
        .into_factory()
}

#[tokio::test]
async fn requests_notifications_and_clean_exit() {
    let transport = factory().open(&["app-server".to_string()]).unwrap();
    let (rpc, mut events) = RpcClient::start(transport).unwrap();

    let init = rpc
        .request("initialize", json!({}), Some(Duration::from_secs(5)))
        .await
        .unwrap();
    assert_eq!(init["userAgent"], "codex_cli_rs/0.160.0");

    let pong = rpc
        .request("ping", json!({}), Some(Duration::from_secs(5)))
        .await
        .unwrap();
    assert_eq!(pong, json!({"pong": true}));

    // the notification and the server request arrive on the event stream, in order, intact
    match events.recv().await.unwrap() {
        RpcEvent::Notification(n) => {
            assert_eq!(n.method, "x/y");
            assert_eq!(n.params["u"], "a\u{2028}b");
            assert_eq!(n.emitted_at_ms, Some(7));
        }
        other => panic!("{other:?}"),
    }
    match events.recv().await.unwrap() {
        RpcEvent::ServerRequest(r) => assert_eq!(r.method, "item/tool/call"),
        other => panic!("{other:?}"),
    }

    let err = rpc
        .request("boom", json!({}), Some(Duration::from_secs(5)))
        .await
        .unwrap_err();
    assert!(err.is_already_idle_interrupt(), "{err:?}");

    rpc.close().await;
    // after close: the last event reports the exit with the stderr tail
    loop {
        match events.recv().await.unwrap() {
            RpcEvent::Closed(info) => {
                assert!(info.stderr_tail.contains("stdin closed"), "{info:?}");
                break;
            }
            _ => continue,
        }
    }
    assert!(matches!(
        rpc.request("ping", json!({}), None).await,
        Err(Error::Exited(_)) | Err(Error::Closed)
    ));
}

#[tokio::test]
async fn request_times_out_when_the_server_stays_silent() {
    let transport = factory().open(&["app-server".to_string()]).unwrap();
    let (rpc, _events) = RpcClient::start(transport).unwrap();
    let err = rpc
        .request(
            "never-answered",
            json!({}),
            Some(Duration::from_millis(100)),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Timeout { ref method } if method == "never-answered"),
        "{err:?}"
    );
    rpc.close().await;
}

#[tokio::test]
async fn unexpected_exit_fails_pending_requests_with_stderr() {
    let opts =
        CodexOptions::new()
            .command("sh")
            .prefix_args(["-c", "echo dying >&2; exit 3", "sh"]);
    let transport = opts
        .into_factory()
        .open(&["app-server".to_string()])
        .unwrap();
    let (rpc, mut events) = RpcClient::start(transport).unwrap();
    let err = rpc
        .request("initialize", json!({}), Some(Duration::from_secs(5)))
        .await
        .unwrap_err();
    match err {
        Error::Exited(msg) => assert!(msg.contains("dying"), "{msg}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(events.recv().await, Some(RpcEvent::Closed(_))));
}

#[tokio::test]
async fn missing_binary_is_an_io_error() {
    let opts = CodexOptions::new().command("/definitely/not/a/codex");
    let transport = opts
        .into_factory()
        .open(&["app-server".to_string()])
        .unwrap();
    assert!(matches!(RpcClient::start(transport), Err(Error::Io(_))));
}

// A grandchild that inherits stdout must not outlive the server or block the exit event.
#[tokio::test]
async fn orphan_holding_stdout_is_killed_with_the_group() {
    let dir = std::env::temp_dir().join(format!("paseo-codex-orphan-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pid_file = dir.join("pid");
    let script = format!(
        "sleep 300 & echo $! > '{}'\nwhile IFS= read -r line; do :; done\n",
        pid_file.display()
    );
    let factory = CodexOptions::new()
        .command("sh")
        .prefix_args(["-c".to_string(), script, "sh".to_string()])
        .into_factory();
    let transport = factory.open(&["app-server".to_string()]).unwrap();
    let (rpc, mut events) = RpcClient::start(transport).unwrap();
    let mut pid = String::new();
    for _ in 0..100 {
        pid = std::fs::read_to_string(&pid_file).unwrap_or_default();
        if pid.trim().parse::<i32>().is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid: i32 = pid.trim().parse().expect("orphan pid");
    rpc.close().await;
    let closed = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match events.recv().await {
                Some(RpcEvent::Closed(_)) | None => break,
                _ => {}
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "Closed never delivered");
    tokio::time::sleep(Duration::from_millis(200)).await;
    // kill(pid, 0) fails with ESRCH once the orphan is gone (zombies reaped by init)
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "orphan survived");
    let _ = std::fs::remove_dir_all(&dir);
}
