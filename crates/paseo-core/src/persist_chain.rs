//! Per-agent write order of snapshot persistence.
//!
//! TS calls `registry.applySnapshot` synchronously where the snapshot is taken, and the registry
//! queues writes per agent in call order. Here the write runs in a spawned task, and tasks reach
//! the store lane in scheduler order, so on a multi-threaded runtime an older snapshot could
//! overwrite a newer one. A [`Link`] fixes the order at the place the snapshot is taken (inside
//! the actor step); the write itself waits for the previous link of the same agent.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

#[derive(Default)]
pub(crate) struct PersistChain {
    tails: Mutex<HashMap<String, (u64, oneshot::Receiver<()>)>>,
    seq: Mutex<u64>,
}

impl PersistChain {
    /// Take the next place in the agent's write order. Call it synchronously where the snapshot
    /// is read.
    pub(crate) fn begin(self: &Arc<Self>, agent_id: &str) -> Link {
        let (done, rx) = oneshot::channel();
        let seq = {
            let mut seq = self.seq.lock().unwrap_or_else(|e| e.into_inner());
            *seq += 1;
            *seq
        };
        let prev = self
            .tails
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(agent_id.to_owned(), (seq, rx))
            .map(|(_, prev)| prev);
        Link {
            chain: self.clone(),
            agent_id: agent_id.to_owned(),
            seq,
            prev,
            _done: Some(done),
        }
    }
}

/// One place in an agent's write order. Dropping it (done, failed or abandoned) lets the next
/// link run.
pub(crate) struct Link {
    chain: Arc<PersistChain>,
    agent_id: String,
    seq: u64,
    prev: Option<oneshot::Receiver<()>>,
    _done: Option<oneshot::Sender<()>>,
}

impl Link {
    /// Wait until every earlier link of this agent is dropped.
    pub(crate) async fn wait_turn(&mut self) {
        if let Some(prev) = self.prev.take() {
            // The sender is dropped, never sent: `Err` is the signal.
            let _ = prev.await;
        }
    }
}

impl PersistChain {
    fn release(&self, agent_id: &str, seq: u64) {
        let mut tails = self.tails.lock().unwrap_or_else(|e| e.into_inner());
        if tails.get(agent_id).is_some_and(|(s, _)| *s == seq) {
            tails.remove(agent_id);
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        // A link dropped before it waited for its predecessor must not release its successor
        // early, or the successor could write while an older link is still in flight. Hand the
        // release to a task that waits for the predecessor first.
        if let Some(prev) = self.prev.take() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let chain = self.chain.clone();
                let agent_id = std::mem::take(&mut self.agent_id);
                let seq = self.seq;
                let done = self._done.take();
                handle.spawn(async move {
                    let _ = prev.await;
                    chain.release(&agent_id, seq);
                    drop(done);
                });
                return;
            }
        }
        self.chain.release(&self.agent_id, self.seq);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn writes_run_in_begin_order_whatever_the_scheduler_does() {
        let chain = Arc::new(PersistChain::default());
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for n in 0..32u64 {
            let mut link = chain.begin("a");
            let order = order.clone();
            tasks.push(tokio::spawn(async move {
                // Later links start earlier, so only the chain can keep the order.
                tokio::time::sleep(Duration::from_millis(32 - n)).await;
                link.wait_turn().await;
                order.lock().unwrap().push(n);
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), (0..32).collect::<Vec<_>>());
        assert!(chain.tails.lock().unwrap().is_empty(), "no tail is leaked");
    }

    #[tokio::test]
    async fn agents_do_not_wait_for_each_other_and_an_abandoned_link_unblocks() {
        let chain = Arc::new(PersistChain::default());
        let first = chain.begin("a");
        let mut other = chain.begin("b");
        other.wait_turn().await;
        let mut second = chain.begin("a");
        drop(first);
        second.wait_turn().await;
    }

    #[tokio::test]
    async fn dropping_a_middle_link_early_keeps_the_successor_behind_the_first() {
        let chain = Arc::new(PersistChain::default());
        let first = chain.begin("a");
        let second = chain.begin("a");
        let mut third = chain.begin("a");
        drop(second);
        let waited = tokio::time::timeout(Duration::from_millis(50), third.wait_turn()).await;
        assert!(waited.is_err(), "third must wait while first is in flight");
        drop(first);
        tokio::time::timeout(Duration::from_secs(1), third.wait_turn())
            .await
            .expect("third runs once first is done");
        drop(third);
        tokio::task::yield_now().await;
        assert!(chain.tails.lock().unwrap().is_empty(), "no tail is leaked");
    }
}
