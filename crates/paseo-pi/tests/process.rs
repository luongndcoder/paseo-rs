//! Exercises `ProcessTransport` against throwaway shell scripts (no pi needed), plus an opt-in
//! live test against the real `pi` (`PASEO_PI_LIVE=1 cargo test --test process -- --ignored`).
#![cfg(unix)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use paseo_pi::agent::{AgentClient, AgentEvent, LaunchContext, SessionConfig};
use paseo_pi::{Error, Event, Options, RpcAgentClient, Session};

fn script(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("paseo-pi-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

const STATE: &str = r#"{"model":{"provider":"p","id":"m"},"thinkingLevel":"off","isStreaming":false,"isCompacting":false,"sessionId":"s1","messageCount":0}"#;

#[test]
fn spawned_process_answers_requests_and_closes_cleanly() {
    let path = script(
        "echo-state.sh",
        &format!(
            r#"n=0
while IFS= read -r line; do
  n=$((n+1))
  printf '{{"id":"req_%s","type":"response","command":"get_state","success":true,"data":{STATE}}}\n' "$n"
done
"#
        ),
    );
    let options = Options::pi()
        .command(["sh", path.to_str().unwrap()])
        .request_timeout(Duration::from_secs(10));
    let session = Session::start(&options).unwrap();
    assert_eq!(session.get_state().unwrap().session_id, "s1");
    assert_eq!(session.get_state().unwrap().session_id, "s1");
    let started = Instant::now();
    session.close();
    assert!(started.elapsed() < Duration::from_secs(5), "close was slow");
    assert!(matches!(session.get_state(), Err(Error::Closed(_))));
}

#[test]
fn exit_code_and_stderr_reach_the_caller() {
    let path = script("dies.sh", "echo 'no such model' >&2\nexit 3\n");
    let options = Options::pi()
        .command(["sh", path.to_str().unwrap()])
        .request_timeout(Duration::from_secs(10));
    let session = Session::start(&options).unwrap();
    let events = session.subscribe();
    let err = session.get_state().unwrap_err().to_string();
    assert!(err.contains("exited with code 3"), "{err}");
    assert!(err.contains("no such model"), "{err}");
    let exit = events.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(exit, Event::ProcessExit(_)));
}

#[test]
fn request_timeout_does_not_kill_the_session() {
    let path = script("silent.sh", "while IFS= read -r line; do :; done\n");
    let options = Options::pi()
        .command(["sh", path.to_str().unwrap()])
        .request_timeout(Duration::from_millis(200));
    let session = Session::start(&options).unwrap();
    let err = session.get_state().unwrap_err();
    assert!(matches!(err, Error::Timeout { .. }), "{err}");
    assert!(!session.rpc().is_closed());
}

#[test]
fn missing_binary_is_a_spawn_error() {
    let options = Options::pi().command(["/definitely/not/a/pi-binary"]);
    assert!(matches!(Session::start(&options), Err(Error::Spawn { .. })));
    let client = RpcAgentClient::pi().with_command(["/definitely/not/a/pi-binary"]);
    assert!(!client.is_available());
}

#[test]
fn omp_flavor_waits_for_ready_and_times_out_without_it() {
    let path = script("mute.sh", "while IFS= read -r line; do :; done\n");
    let options = Options::omp()
        .command(["sh", path.to_str().unwrap()])
        .ready_timeout(Duration::from_millis(200));
    let err = Session::start(&options).err().expect("start must fail");
    assert!(
        err.to_string()
            .contains("Timed out waiting for OMP RPC to become ready"),
        "{err}"
    );
}

/// Real pi, real model. Skipped unless asked for; uses the free nvidia model by default.
#[test]
#[ignore = "needs an authenticated pi on PATH; set PASEO_PI_LIVE=1"]
fn live_pi_prompt_and_interrupt() {
    if std::env::var("PASEO_PI_LIVE").is_err() {
        return;
    }
    let model =
        std::env::var("PASEO_PI_LIVE_MODEL").unwrap_or_else(|_| "nvidia/openai/gpt-oss-20b".into());
    let cwd = std::env::temp_dir();
    let client = RpcAgentClient::pi().with_request_timeout(Duration::from_secs(60));
    assert!(client.is_available());
    let config = SessionConfig {
        cwd,
        model: Some(model),
        internal: true,
        ..SessionConfig::default()
    };
    let session = client
        .create_session(&config, &LaunchContext::default())
        .unwrap();
    let result = session.run("Reply with exactly: pong".into()).unwrap();
    assert!(
        result.final_text.to_lowercase().contains("pong"),
        "{}",
        result.final_text
    );

    let rx = session.subscribe();
    let turn = session
        .start_turn("Count from 1 to 400, one number per line, with no other text.".into())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let event = rx.recv_timeout(deadline - Instant::now()).unwrap();
        if matches!(&event, AgentEvent::Provider { event_type, .. } if event_type == "message_update")
        {
            break;
        }
    }
    session.interrupt().unwrap();
    loop {
        match rx.recv_timeout(deadline - Instant::now()).unwrap() {
            AgentEvent::TurnCanceled { turn_id, .. } => {
                assert_eq!(turn_id, turn.turn_id);
                break;
            }
            AgentEvent::TurnCompleted { .. } => panic!("interrupted turn completed"),
            _ => {}
        }
    }
    session.close().unwrap();
}

/// A writer blocked on a full stdin pipe holds the stdin lock. `close()` must not wait for it:
/// it falls through to TERM/KILL, whose broken pipe unblocks the writer with an error.
#[test]
fn close_while_a_writer_is_blocked_in_write_line() {
    use paseo_pi::launch::Launch;
    use paseo_pi::transport::{ProcessTransport, Transport};
    use std::sync::{mpsc, Arc, Barrier};

    let launch = Launch {
        argv: vec!["sh".into(), "-c".into(), "exec sleep 60".into()],
        cwd: None,
        env: Default::default(),
    };
    let transport = Arc::new(ProcessTransport::spawn(&launch).unwrap());
    // Larger than any pipe buffer, so write_all blocks: the child never reads.
    let payload = "x".repeat(4 * 1024 * 1024);
    let barrier = Arc::new(Barrier::new(2));
    let (tx, rx) = mpsc::channel();
    let writer = {
        let (transport, barrier) = (transport.clone(), barrier.clone());
        std::thread::spawn(move || {
            barrier.wait();
            tx.send(transport.write_line(&payload)).unwrap();
        })
    };
    barrier.wait();
    // The writer cannot finish: nothing drains the pipe.
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());

    let started = Instant::now();
    let closer = {
        let transport = transport.clone();
        std::thread::spawn(move || transport.close())
    };
    closer.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "close hung behind the writer");

    let result = rx.recv_timeout(Duration::from_secs(5)).expect("writer never unblocked");
    assert!(result.is_err(), "blocked write must fail once the process is gone");
    writer.join().unwrap();

    // Later writes see a closed/broken transport, and close() stays idempotent.
    assert!(transport.write_line("{}").is_err());
    transport.close();
}
