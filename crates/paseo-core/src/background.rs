//! Counting task tracker: `backgroundTasks` and `agentRegistrationTasks` in TS.
//!
//! `flush` waits until the count reaches zero, including tasks spawned while waiting.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

#[derive(Debug, Default)]
struct Inner {
    count: AtomicUsize,
    notify: Notify,
}

#[derive(Debug, Clone, Default)]
pub struct TaskTracker(Arc<Inner>);

/// Decrements on drop, so a panicking task still releases `wait_idle`.
#[derive(Debug)]
pub struct TaskGuard(Arc<Inner>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.count.fetch_sub(1, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }
}

impl TaskTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Count a task that is not spawned through [`TaskTracker::spawn`].
    pub fn track(&self) -> TaskGuard {
        self.0.count.fetch_add(1, Ordering::SeqCst);
        TaskGuard(self.0.clone())
    }

    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let guard = self.track();
        tokio::spawn(async move {
            task.await;
            drop(guard);
        });
    }

    pub fn len(&self) -> usize {
        self.0.count.load(Ordering::SeqCst)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub async fn wait_idle(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_empty() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn wait_idle_covers_tasks_spawned_while_waiting() {
        let tracker = TaskTracker::new();
        let inner = tracker.clone();
        tracker.spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            inner.spawn(async {
                tokio::time::sleep(Duration::from_millis(10)).await;
            });
        });
        tracker.wait_idle().await;
        assert!(tracker.is_empty());
    }
}
