//! Transport abstraction: the session driver only sees lines in and lines out, so tests can
//! drive it with [`crate::InMemoryTransport`] instead of a real `claude` process.

use tokio::sync::mpsc;

use crate::error::Result;

/// How the other side ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    /// Last ~2 KiB of stderr (diagnostics only).
    pub stderr_tail: String,
}

impl ExitInfo {
    pub fn success(&self) -> bool {
        self.code == Some(0) && self.signal.is_none()
    }

    pub fn describe(&self) -> String {
        match (self.code, self.signal) {
            (Some(c), _) => format!("code {c}"),
            (None, Some(s)) => format!("signal {s}"),
            (None, None) => "unknown status".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportEvent {
    /// One stdout line without the trailing newline (may be blank or not JSON).
    Line(String),
    /// Always the last event. Closing the channel without it counts as a clean exit.
    Exit(ExitInfo),
}

/// The running side of a transport.
pub struct TransportIo {
    /// Lines to write (without newline; the transport appends it). Dropping every sender
    /// closes stdin.
    pub stdin: mpsc::UnboundedSender<String>,
    pub events: mpsc::UnboundedReceiver<TransportEvent>,
    /// Hard stop: terminate the process (and its group). Must be callable from any thread,
    /// any number of times, even outside a tokio runtime.
    pub kill: Box<dyn Fn() + Send + Sync>,
}

pub trait Transport: Send + 'static {
    /// Start the transport. Called inside a tokio runtime.
    fn start(self: Box<Self>) -> Result<TransportIo>;
}
