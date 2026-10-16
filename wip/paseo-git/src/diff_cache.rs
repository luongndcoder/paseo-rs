//! Completed-diff cache with in-flight sharing and invalidation
//! (`checkout-diff-cache.ts`). Completed payloads are bounded (64, LRU); active
//! work survives invalidation and eviction. Work is spawned on the Tokio
//! runtime, so `read` must be called from within one.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::sync::watch;

use crate::diff::{CheckoutDiffCompare, CheckoutDiffResult, DiffMode};
use crate::error::{GitError, Result};

const MAX_COMPLETED: usize = 64;
const FRESH_FOR_MS: u64 = 15_000;
const RECENT_READ_MS: u64 = 2_000;

pub type DiffReadFuture = Pin<Box<dyn Future<Output = Result<Arc<CheckoutDiffResult>>> + Send>>;
pub type DiffLoad =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<CheckoutDiffResult>> + Send>> + Send + Sync>;

#[derive(Clone)]
pub struct CheckoutDiffReadInput {
    pub cwd: String,
    pub compare: CheckoutDiffCompare,
    pub force: bool,
    /// Required with `force`.
    pub reason: Option<String>,
    pub load: DiffLoad,
}

struct Cached {
    value: Arc<CheckoutDiffResult>,
    loaded_at: u64,
    last_read_started: u64,
    tick: u64,
}

type Outcome = Option<Result<Arc<CheckoutDiffResult>>>;

struct ActiveRead {
    invalidated: AtomicBool,
    rx: watch::Receiver<Outcome>,
}

#[derive(Default)]
struct State {
    values: HashMap<String, Cached>,
    active: HashMap<String, Arc<ActiveRead>>,
    tick: u64,
}

#[derive(Clone)]
pub struct CheckoutDiffCache {
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
    state: Arc<Mutex<State>>,
}

fn cache_key(cwd: &str, compare: &CheckoutDiffCompare) -> String {
    json!([
        cwd,
        compare.mode.as_str(),
        if compare.mode == DiffMode::Base {
            compare.base_ref.clone().map_or(Value::Null, Value::String)
        } else {
            Value::Null
        },
        compare.ignore_whitespace == Some(true),
        compare.include_structured == Some(true),
    ])
    .to_string()
}

async fn wait_for(mut rx: watch::Receiver<Outcome>) -> Result<Arc<CheckoutDiffResult>> {
    match rx.wait_for(|v| v.is_some()).await {
        Ok(v) => v.clone().expect("checked is_some"),
        Err(_) => Err(GitError::other("Checkout diff read was cancelled")),
    }
}

fn waiter(rx: watch::Receiver<Outcome>) -> DiffReadFuture {
    Box::pin(wait_for(rx))
}

fn spawn_outcome<F>(fut: F) -> DiffReadFuture
where
    F: Future<Output = Result<Arc<CheckoutDiffResult>>> + Send + 'static,
{
    let (tx, rx) = watch::channel(None);
    tokio::spawn(async move {
        let _ = tx.send(Some(fut.await));
    });
    waiter(rx)
}

impl CheckoutDiffCache {
    /// `now` returns milliseconds on any monotonic clock.
    pub fn new(now: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self {
            now: Arc::new(now),
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// Starts (or joins) a read immediately and returns a future for its result.
    /// All bookkeeping happens synchronously in this call, so ordering between
    /// reads, forced reads and invalidations is the call order.
    pub fn read(&self, input: CheckoutDiffReadInput) -> DiffReadFuture {
        if input.force && input.reason.is_none() {
            return Box::pin(async {
                Err(GitError::other(
                    "WorkspaceGitService forced read requires a reason",
                ))
            });
        }
        let key = cache_key(&input.cwd, &input.compare);
        let now = (self.now)();
        let mut state = self.state.lock().unwrap();
        state.tick += 1;
        let tick = state.tick;

        if let Some(cached) = state.values.get_mut(&key) {
            cached.tick = tick;
            if !input.force
                && (now.saturating_sub(cached.loaded_at) <= FRESH_FOR_MS
                    || now.saturating_sub(cached.last_read_started) < RECENT_READ_MS)
            {
                let value = cached.value.clone();
                return Box::pin(async move { Ok(value) });
            }
        }

        if let Some(pending) = state.active.get(&key).cloned() {
            if input.force {
                pending.invalidated.store(true, Ordering::SeqCst);
                state.values.remove(&key);
            }
            if !pending.invalidated.load(Ordering::SeqCst) {
                return waiter(pending.rx.clone());
            }
            // A caller arriving after a change needs a fresh read once the old one
            // finishes. The old result cannot enter the cache. Force is dropped on
            // the retry so callers waiting on the same result share the next build.
            drop(state);
            let cache = self.clone();
            let retry = CheckoutDiffReadInput {
                force: false,
                ..input
            };
            let rx = pending.rx.clone();
            return spawn_outcome(async move {
                let _ = wait_for(rx).await;
                cache.read(retry).await
            });
        }

        if let Some(cached) = state.values.get_mut(&key) {
            cached.last_read_started = now;
        }

        let (tx, rx) = watch::channel(None);
        let read = Arc::new(ActiveRead {
            invalidated: AtomicBool::new(false),
            rx: rx.clone(),
        });
        state.active.insert(key.clone(), read.clone());
        drop(state);

        let cache = self.clone();
        let load = input.load.clone();
        tokio::spawn(async move {
            // Ownership is installed before `load` runs, including synchronous invalidations.
            let result = load().await.map(Arc::new);
            {
                let mut state = cache.state.lock().unwrap();
                // Never repopulate the cache with a snapshot invalidated during its build.
                if let (false, Ok(value)) = (read.invalidated.load(Ordering::SeqCst), &result) {
                    state.tick += 1;
                    let tick = state.tick;
                    let loaded_at = (cache.now)();
                    state.values.insert(
                        key.clone(),
                        Cached {
                            value: value.clone(),
                            loaded_at,
                            last_read_started: now,
                            tick,
                        },
                    );
                    while state.values.len() > MAX_COMPLETED {
                        let oldest = state
                            .values
                            .iter()
                            .min_by_key(|(_, c)| c.tick)
                            .map(|(k, _)| k.clone());
                        match oldest {
                            Some(k) => state.values.remove(&k),
                            None => break,
                        };
                    }
                }
                state.active.remove(&key);
            }
            let _ = tx.send(Some(result));
        });
        waiter(rx)
    }

    pub fn invalidate(&self, cwd: &str, mode: DiffMode) {
        let mut state = self.state.lock().unwrap();
        let keys: Vec<String> = state
            .values
            .keys()
            .chain(state.active.keys())
            .cloned()
            .collect();
        for key in keys {
            let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(&key) else {
                continue;
            };
            if parts.first().and_then(Value::as_str) != Some(cwd)
                || parts.get(1).and_then(Value::as_str) != Some(mode.as_str())
            {
                continue;
            }
            state.values.remove(&key);
            if let Some(pending) = state.active.get(&key) {
                pending.invalidated.store(true, Ordering::SeqCst);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::oneshot;
    use tokio::task::yield_now;

    fn result(text: &str) -> CheckoutDiffResult {
        CheckoutDiffResult {
            diff: text.into(),
            structured: None,
            diff_too_large: false,
        }
    }

    /// First call awaits `first`, later calls await `second`.
    struct Sequenced {
        calls: Arc<AtomicUsize>,
        load: DiffLoad,
    }

    fn sequenced(
        first: oneshot::Receiver<Result<CheckoutDiffResult>>,
        second: oneshot::Receiver<Result<CheckoutDiffResult>>,
    ) -> Sequenced {
        let calls = Arc::new(AtomicUsize::new(0));
        let receivers = Arc::new(Mutex::new(vec![Some(second), Some(first)]));
        let c = calls.clone();
        let load: DiffLoad = Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            let rx = receivers.lock().unwrap().pop().flatten();
            Box::pin(async move {
                match rx {
                    Some(rx) => rx.await.unwrap_or_else(|_| Err(GitError::other("dropped"))),
                    None => Ok(result("unexpected extra load")),
                }
            })
        });
        Sequenced { calls, load }
    }

    fn input(cwd: &str, mode: DiffMode, load: &DiffLoad) -> CheckoutDiffReadInput {
        CheckoutDiffReadInput {
            cwd: cwd.into(),
            compare: if mode == DiffMode::Base {
                CheckoutDiffCompare::base(None)
            } else {
                CheckoutDiffCompare::uncommitted()
            },
            force: false,
            reason: None,
            load: load.clone(),
        }
    }

    fn forced(cwd: &str, mode: DiffMode, load: &DiffLoad) -> CheckoutDiffReadInput {
        CheckoutDiffReadInput {
            force: true,
            reason: Some("repository-mutation".into()),
            ..input(cwd, mode, load)
        }
    }

    async fn settle() {
        for _ in 0..10 {
            yield_now().await;
        }
    }

    #[tokio::test]
    async fn forced_reads_after_a_mutation_share_a_fresh_build_after_the_active_build_finishes() {
        let cache = CheckoutDiffCache::new(|| 0);
        let ((t1, r1), (t2, r2)) = (oneshot::channel(), oneshot::channel());
        let s = sequenced(r1, r2);
        let pending = cache.read(input("repo", DiffMode::Base, &s.load));
        yield_now().await;
        let fresh = cache.read(forced("repo", DiffMode::Base, &s.load));
        let another = cache.read(forced("repo", DiffMode::Base, &s.load));
        assert_eq!(s.calls.load(Ordering::SeqCst), 1);
        t1.send(Ok(result("before-mutation"))).ok();
        assert_eq!(pending.await.unwrap().diff, "before-mutation");
        settle().await;
        assert_eq!(s.calls.load(Ordering::SeqCst), 2);
        t2.send(Ok(result("after-mutation"))).ok();
        assert_eq!(fresh.await.unwrap().diff, "after-mutation");
        assert_eq!(another.await.unwrap().diff, "after-mutation");
        let again = cache
            .read(input("repo", DiffMode::Base, &s.load))
            .await
            .unwrap();
        assert_eq!(again.diff, "after-mutation");
        assert_eq!(s.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn reads_after_invalidation_wait_for_fresh_work_without_overlapping_the_original_read() {
        let cache = CheckoutDiffCache::new(|| 0);
        let ((t1, r1), (t2, r2)) = (oneshot::channel(), oneshot::channel());
        let s = sequenced(r1, r2);
        let read = || cache.read(input("repo", DiffMode::Base, &s.load));
        let pending = read();
        yield_now().await;
        cache.invalidate("repo", DiffMode::Base);
        cache.invalidate("repo", DiffMode::Base);
        let fresh = read();
        let another = read();
        assert_eq!(s.calls.load(Ordering::SeqCst), 1);
        t1.send(Ok(result("old"))).ok();
        assert_eq!(pending.await.unwrap().diff, "old");
        settle().await;
        assert_eq!(s.calls.load(Ordering::SeqCst), 2);
        t2.send(Ok(result("new"))).ok();
        assert_eq!(fresh.await.unwrap().diff, "new");
        assert_eq!(another.await.unwrap().diff, "new");
        assert_eq!(read().await.unwrap().diff, "new");
        assert_eq!(s.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn edits_during_a_read_do_not_delay_its_caller_or_cache_its_outdated_snapshot() {
        let cache = CheckoutDiffCache::new(|| 0);
        let (tx, rx) = oneshot::channel::<Result<CheckoutDiffResult>>();
        let calls = Arc::new(AtomicUsize::new(0));
        let rx = Arc::new(Mutex::new(Some(rx)));
        let c = calls.clone();
        let load: DiffLoad = Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            let rx = rx.lock().unwrap().take().unwrap();
            Box::pin(async move { rx.await.unwrap() })
        });
        let pending = cache.read(input("repo", DiffMode::Uncommitted, &load));
        yield_now().await;
        cache.invalidate("repo", DiffMode::Uncommitted);
        tx.send(Ok(result("snapshot"))).ok();
        assert_eq!(pending.await.unwrap().diff, "snapshot");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let fresh_load: DiffLoad = Arc::new(|| Box::pin(async { Ok(result("fresh")) }));
        let next = cache
            .read(input("repo", DiffMode::Uncommitted, &fresh_load))
            .await
            .unwrap();
        assert_eq!(next.diff, "fresh");
    }

    #[tokio::test]
    async fn active_reads_survive_completed_payload_eviction_and_failed_reads_can_retry() {
        let cache = CheckoutDiffCache::new(|| 0);
        let (tx, rx) = oneshot::channel::<Result<CheckoutDiffResult>>();
        let rx = Arc::new(Mutex::new(Some(rx)));
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let load: DiffLoad = Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
            let rx = rx.lock().unwrap().take().unwrap();
            Box::pin(async move { rx.await.unwrap() })
        });
        let first = cache.read(input("active", DiffMode::Uncommitted, &load));
        yield_now().await;
        let filler: DiffLoad = Arc::new(|| Box::pin(async { Ok(result("")) }));
        for i in 0..70 {
            cache
                .read(input(&i.to_string(), DiffMode::Base, &filler))
                .await
                .unwrap();
        }
        let joined = cache.read(input("active", DiffMode::Uncommitted, &load));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "joined the active read");
        tx.send(Err(GitError::other("read failed"))).ok();
        assert_eq!(first.await.unwrap_err().to_string(), "read failed");
        assert_eq!(joined.await.unwrap_err().to_string(), "read failed");
        let retry: DiffLoad = Arc::new(|| Box::pin(async { Ok(result("retry")) }));
        assert_eq!(
            cache
                .read(input("active", DiffMode::Uncommitted, &retry))
                .await
                .unwrap()
                .diff,
            "retry"
        );
    }

    #[tokio::test]
    async fn forced_read_requires_a_reason() {
        let cache = CheckoutDiffCache::new(|| 0);
        let load: DiffLoad = Arc::new(|| Box::pin(async { Ok(result("x")) }));
        let bad = CheckoutDiffReadInput {
            force: true,
            reason: None,
            ..input("r", DiffMode::Base, &load)
        };
        assert_eq!(
            cache.read(bad).await.unwrap_err().to_string(),
            "WorkspaceGitService forced read requires a reason"
        );
    }

    #[tokio::test]
    async fn fresh_values_are_served_until_they_age_out() {
        let clock = Arc::new(AtomicUsize::new(0));
        let c = clock.clone();
        let cache = CheckoutDiffCache::new(move || c.load(Ordering::SeqCst) as u64);
        let calls = Arc::new(AtomicUsize::new(0));
        let n = calls.clone();
        let load: DiffLoad = Arc::new(move || {
            let v = n.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(result(&format!("v{v}"))) })
        });
        assert_eq!(
            cache
                .read(input("r", DiffMode::Base, &load))
                .await
                .unwrap()
                .diff,
            "v0"
        );
        clock.store(14_000, Ordering::SeqCst);
        assert_eq!(
            cache
                .read(input("r", DiffMode::Base, &load))
                .await
                .unwrap()
                .diff,
            "v0"
        );
        clock.store(40_000, Ordering::SeqCst);
        assert_eq!(
            cache
                .read(input("r", DiffMode::Base, &load))
                .await
                .unwrap()
                .diff,
            "v1"
        );
    }
}
