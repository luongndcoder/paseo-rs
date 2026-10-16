//! In-memory transport for tests and for embedding the driver behind another IPC layer.

use tokio::sync::mpsc;

use crate::error::Result;
use crate::transport::{ExitInfo, Transport, TransportEvent, TransportIo};

/// Client half; hand it to [`crate::Session::start_with_transport`].
pub struct InMemoryTransport {
    io: TransportIo,
}

/// The fake CLI: reads what the client writes, emits stdout lines.
pub struct InMemoryPeer {
    written: mpsc::UnboundedReceiver<String>,
    events: mpsc::UnboundedSender<TransportEvent>,
    killed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl InMemoryTransport {
    pub fn new() -> (Self, InMemoryPeer) {
        let (stdin_tx, stdin_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let killed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let k = killed.clone();
        let kill_tx = ev_tx.clone();
        let io = TransportIo {
            stdin: stdin_tx,
            events: ev_rx,
            kill: Box::new(move || {
                k.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = kill_tx.send(TransportEvent::Exit(ExitInfo {
                    code: None,
                    signal: Some(15),
                    stderr_tail: String::new(),
                }));
            }),
        };
        (
            Self { io },
            InMemoryPeer {
                written: stdin_rx,
                events: ev_tx,
                killed,
            },
        )
    }
}

impl Transport for InMemoryTransport {
    fn start(self: Box<Self>) -> Result<TransportIo> {
        Ok(self.io)
    }
}

impl InMemoryPeer {
    /// Next line the client wrote; `None` once the client closed stdin.
    pub async fn recv(&mut self) -> Option<String> {
        self.written.recv().await
    }

    /// Next written line parsed as JSON.
    pub async fn recv_json(&mut self) -> Option<serde_json::Value> {
        loop {
            let line = self.recv().await?;
            if let Ok(v) = serde_json::from_str(&line) {
                return Some(v);
            }
        }
    }

    pub fn send(&self, line: impl Into<String>) {
        let _ = self.events.send(TransportEvent::Line(line.into()));
    }

    pub fn send_json(&self, value: &serde_json::Value) {
        self.send(value.to_string());
    }

    pub fn exit(&self, info: ExitInfo) {
        let _ = self.events.send(TransportEvent::Exit(info));
    }

    pub fn was_killed(&self) -> bool {
        self.killed.load(std::sync::atomic::Ordering::SeqCst)
    }
}
