//! A spawned unit of work whose result any number of tasks can await: the Rust form of a TS
//! promise stored in a map (`pendingCreates`, `pendingControlActions`). The work keeps running
//! when every waiter goes away, as a TS promise does when its caller is gone.

use std::future::Future;

use tokio::sync::watch;

#[derive(Clone)]
pub struct Pending<T: Clone + Send + Sync + 'static> {
    rx: watch::Receiver<Option<T>>,
}

impl<T: Clone + Send + Sync + 'static> Pending<T> {
    /// Creates the handle and the sender the work reports through. Spawn the work yourself
    /// (the entry must be in its map before the work can finish).
    pub fn channel() -> (watch::Sender<Option<T>>, Self) {
        let (tx, rx) = watch::channel(None);
        (tx, Self { rx })
    }

    /// Waits for the result. `None` means the work was dropped without reporting (its task
    /// panicked or the runtime shut down).
    pub async fn wait(&self) -> Option<T> {
        let mut rx = self.rx.clone();
        rx.wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|value| value.clone())
    }
}

/// Runs `work`, calls `before_report` (the map cleanup that TS does in `finally`), then
/// publishes the result.
pub fn run<T, F>(
    tx: watch::Sender<Option<T>>,
    work: F,
    before_report: impl FnOnce() + Send + 'static,
) where
    T: Clone + Send + Sync + 'static,
    F: Future<Output = T> + Send + 'static,
{
    tokio::spawn(async move {
        let result = work.await;
        before_report();
        let _ = tx.send(Some(result));
    });
}
