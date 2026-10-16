//! `LaneMap`: FIFO async serialization keyed by agent id.
//!
//! Replaces TS `lifecycleMutationTails` and `foregroundMutationTails`. Keyed by id, not by actor
//! existence, because resume runs before the agent is live. A failed step never poisons the
//! lane; the entry disappears when the last holder leaves.
//!
//! Waiters are served in the order they first poll `run` (tokio's mutex is fair). TS enqueues
//! at call time; every caller here awaits the call immediately, so the orders coincide.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;

#[derive(Debug)]
pub struct LaneMap<K: Eq + Hash + Clone> {
    lanes: Mutex<HashMap<K, Arc<AsyncMutex<()>>>>,
}

impl<K: Eq + Hash + Clone> Default for LaneMap<K> {
    fn default() -> Self {
        Self {
            lanes: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Eq + Hash + Clone> LaneMap<K> {
    pub fn new() -> Self {
        Self::default()
    }

    fn lane(&self, key: &K) -> Arc<AsyncMutex<()>> {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        lanes.entry(key.clone()).or_default().clone()
    }

    fn release(&self, key: &K, lane: Arc<AsyncMutex<()>>) {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        // The map holds one reference and `lane` the other: nobody else is queued.
        if Arc::strong_count(&lane) == 2 {
            lanes.remove(key);
        }
    }

    /// Run `step` after every step queued before it on `key`.
    pub async fn run<T, Fut>(&self, key: &K, step: impl FnOnce() -> Fut) -> T
    where
        Fut: Future<Output = T>,
    {
        let lane = self.lane(key);
        let guard = lane.lock().await;
        let result = step().await;
        drop(guard);
        self.release(key, lane);
        result
    }

    /// Resolve once every step queued so far on `key` has finished (`await tails.get(id)`).
    pub async fn wait_idle(&self, key: &K) {
        let existing = {
            let lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
            lanes.get(key).cloned()
        };
        if let Some(lane) = existing {
            drop(lane.lock().await);
            self.release(key, lane);
        }
    }

    pub fn is_busy(&self, key: &K) -> bool {
        let lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        lanes.contains_key(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn steps_run_in_fifo_order_and_the_entry_is_removed() {
        let lanes = Arc::new(LaneMap::<String>::new());
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut handles = Vec::new();
        for i in 0..3 {
            let lanes = lanes.clone();
            let order = order.clone();
            handles.push(tokio::spawn(async move {
                lanes
                    .run(&"a".to_string(), || async move {
                        tokio::time::sleep(Duration::from_millis(10 * (3 - i))).await;
                        order.lock().unwrap().push(i);
                    })
                    .await;
            }));
            tokio::task::yield_now().await;
        }
        for handle in handles {
            handle.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
        assert!(!lanes.is_busy(&"a".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_step_does_not_poison_the_lane() {
        let lanes = LaneMap::<u8>::new();
        let failed: Result<(), &str> = lanes.run(&1, || async { Err("boom") }).await;
        assert!(failed.is_err());
        let ran = AtomicUsize::new(0);
        lanes
            .run(&1, || async {
                ran.fetch_add(1, Ordering::SeqCst);
            })
            .await;
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_idle_waits_for_queued_steps() {
        let lanes = Arc::new(LaneMap::<u8>::new());
        let done = Arc::new(AtomicUsize::new(0));
        let (lanes2, done2) = (lanes.clone(), done.clone());
        let step = tokio::spawn(async move {
            lanes2
                .run(&7, || async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    done2.fetch_add(1, Ordering::SeqCst);
                })
                .await;
        });
        tokio::task::yield_now().await;
        lanes.wait_idle(&7).await;
        assert_eq!(done.load(Ordering::SeqCst), 1);
        step.await.unwrap();
    }
}
