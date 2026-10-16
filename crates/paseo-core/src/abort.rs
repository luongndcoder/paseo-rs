//! Minimal abort signal for the manager waits (`WaitForAgentOptions.signal`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

#[derive(Debug, Default)]
struct Inner {
    aborted: AtomicBool,
    reason: Mutex<Option<String>>,
    notify: Notify,
}

/// Cloneable abort signal. `abort` is sticky and wakes every pending `aborted()`.
#[derive(Debug, Clone, Default)]
pub struct AbortSignal(Arc<Inner>);

impl AbortSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn abort(&self, reason: impl Into<String>) {
        *self.0.reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason.into());
        self.0.aborted.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn is_aborted(&self) -> bool {
        self.0.aborted.load(Ordering::SeqCst)
    }

    /// The abort reason, or `fallback` (`abortMessage`).
    pub fn message(&self, fallback: &str) -> String {
        self.0
            .reason
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| fallback.to_owned())
    }

    /// Resolves once aborted.
    pub async fn aborted(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            // Register before the flag check so an abort in between is not lost.
            notified.as_mut().enable();
            if self.is_aborted() {
                return;
            }
            notified.await;
        }
    }
}
