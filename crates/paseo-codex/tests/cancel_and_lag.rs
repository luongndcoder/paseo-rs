//! Cancellation and lag regressions for `CodexSession`, driven through the golden replay with a
//! gate in front of `turn/start`:
//! - `start_turn` dropped while `turn/start` is in flight must release `pending_start` /
//!   `active_turn` (StartGuard) and wake a waiting `interrupt()`.
//! - `run()` must survive a lagged event receiver and take the outcome from the outcome channel.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::ReplayFactory;
use paseo_codex::agent::*;
use paseo_codex::error::Result;
use paseo_codex::transport::{BoxFuture, Transport, TransportEvent, TransportFactory, TransportIo};
use paseo_codex::{CodexClient, Error};
use tokio::sync::{mpsc, Notify};

#[derive(Default)]
struct Gate {
    /// Swallow the next `turn/start` (it never reaches the server) and signal `held`.
    hold_next: AtomicBool,
    held: Notify,
    /// Notifications injected ahead of every forwarded `turn/start` reply.
    fillers: AtomicUsize,
}

struct GatedFactory {
    inner: Arc<ReplayFactory>,
    gate: Arc<Gate>,
}

impl TransportFactory for GatedFactory {
    fn open(&self, args: &[String]) -> Result<Box<dyn Transport>> {
        Ok(Box::new(GatedTransport {
            inner: self.inner.open(args)?,
            gate: self.gate.clone(),
        }))
    }

    fn version(&self) -> BoxFuture<Result<String>> {
        self.inner.version()
    }
}

struct GatedTransport {
    inner: Box<dyn Transport>,
    gate: Arc<Gate>,
}

impl Transport for GatedTransport {
    fn start(self: Box<Self>) -> Result<TransportIo> {
        let GatedTransport { inner, gate } = *self;
        let mut io = inner.start()?;
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<TransportEvent>();

        let inner_out = io.outgoing.clone();
        let inject = in_tx.clone();
        tokio::spawn(async move {
            while let Some(line) = out_rx.recv().await {
                if line.contains("\"method\":\"turn/start\"") {
                    if gate.hold_next.swap(false, Ordering::SeqCst) {
                        gate.held.notify_one();
                        continue;
                    }
                    for _ in 0..gate.fillers.load(Ordering::SeqCst) {
                        let _ = inject.send(TransportEvent::Line(
                            r#"{"method":"x/filler","params":{}}"#.into(),
                        ));
                    }
                }
                let _ = inner_out.send(line);
            }
        });
        tokio::spawn(async move {
            while let Some(ev) = io.incoming.recv().await {
                let _ = in_tx.send(ev);
            }
        });
        Ok(TransportIo {
            outgoing: out_tx,
            incoming: in_rx,
            control: io.control,
        })
    }
}

fn setup(gate: &Arc<Gate>) -> CodexClient {
    let factory = GatedFactory {
        inner: ReplayFactory::new("01-single-prompt"),
        gate: gate.clone(),
    };
    CodexClient::with_factory(Arc::new(factory))
}

fn config() -> SessionConfig {
    SessionConfig {
        cwd: "/tmp/paseo-codex-fixture".into(),
        model: Some("gpt-6-luna".into()),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_start_turn_releases_the_guard_and_wakes_interrupt() {
    let gate = Arc::new(Gate::default());
    gate.hold_next.store(true, Ordering::SeqCst);
    let session = Arc::new(setup(&gate).create_session(config()).await.unwrap());

    let starter = {
        let session = session.clone();
        tokio::spawn(async move { session.start_turn("hi".into(), RunOptions::default()).await })
    };
    tokio::time::timeout(Duration::from_secs(5), gate.held.notified())
        .await
        .expect("turn/start never reached the transport");

    // While the start is in flight the session is busy.
    assert!(matches!(
        session
            .start_turn("again".into(), RunOptions::default())
            .await,
        Err(Error::TurnActive)
    ));

    // interrupt() parks on the pending start; it must wake when the start future is dropped.
    let interrupter = {
        let session = session.clone();
        tokio::spawn(async move { session.interrupt().await })
    };
    tokio::task::yield_now().await;
    starter.abort();
    assert!(starter.await.unwrap_err().is_cancelled());
    let woke = tokio::time::timeout(Duration::from_secs(1), interrupter)
        .await
        .expect("interrupt() stayed parked after the start future was dropped")
        .unwrap();
    assert!(woke.is_ok(), "{woke:?}");

    // State is clean: a fresh start is not rejected as busy. (The replay has no recorded
    // `thread/resume`, so the attempt itself fails with an Rpc error; what matters is that it
    // got past the TurnActive check, and that its own failure leaves the session idle too.)
    for _ in 0..2 {
        let again = session.start_turn("hi".into(), RunOptions::default()).await;
        assert!(
            !matches!(again, Err(Error::TurnActive)),
            "StartGuard left the session busy"
        );
    }
    session.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_survives_a_lagged_event_receiver() {
    const FILLERS: usize = 5000; // > EVENT_CAPACITY (4096)
    let gate = Arc::new(Gate::default());
    gate.fillers.store(FILLERS, Ordering::SeqCst);
    let session = setup(&gate).create_session(config()).await.unwrap();

    // The fillers reach the broadcast channel before the turn/start reply wakes run(), so its
    // receiver is lagged by the time it first polls.
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        session.run(
            "Reply with exactly the word: pong. No tools.".into(),
            RunOptions::default(),
        ),
    )
    .await
    .expect("run() hung after lag")
    .expect("run() failed after lag");

    assert!(!result.canceled);
    assert_eq!(result.final_text, "pong");
    assert!(result.usage.is_some());
    assert!(
        result.events.len() < FILLERS,
        "no events were lost, so the Lagged path was not exercised ({})",
        result.events.len()
    );
    session.close().await.unwrap();
}
