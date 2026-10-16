//! Admission control for `git` processes (`git-process-scheduler.ts`): a cap
//! on concurrently running processes, a strict per-second start budget and a
//! two-level priority queue (user operations before observation work).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitProcessPolicy {
    pub max_processes_per_second: usize,
    pub max_process_concurrency: usize,
}

pub const DEFAULT_GIT_PROCESS_POLICY: GitProcessPolicy = GitProcessPolicy {
    max_processes_per_second: 64,
    max_process_concurrency: 8,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GitProcessPriority {
    #[default]
    Normal,
    High,
}

fn parse_positive(value: Option<&str>) -> Option<usize> {
    let parsed: usize = value?.trim().parse().ok()?;
    (parsed > 0).then_some(parsed)
}

/// Environment beats persisted settings beats defaults; the legacy
/// `PASEO_GIT_CONCURRENCY` name is still honored (COMPAT(gitConcurrencyEnv):
/// renamed in v0.2.6; remove after 2027-02-02).
pub fn resolve_git_process_policy(
    env: &dyn Fn(&str) -> Option<String>,
    persisted: Option<GitProcessPolicy>,
) -> GitProcessPolicy {
    GitProcessPolicy {
        max_processes_per_second: parse_positive(
            env("PASEO_GIT_MAX_PROCESSES_PER_SECOND").as_deref(),
        )
        .or(persisted.map(|p| p.max_processes_per_second))
        .unwrap_or(DEFAULT_GIT_PROCESS_POLICY.max_processes_per_second),
        max_process_concurrency: parse_positive(
            env("PASEO_GIT_MAX_PROCESS_CONCURRENCY").as_deref(),
        )
        .or(parse_positive(env("PASEO_GIT_CONCURRENCY").as_deref()))
        .or(persisted.map(|p| p.max_process_concurrency))
        .unwrap_or(DEFAULT_GIT_PROCESS_POLICY.max_process_concurrency),
    }
}

#[derive(Default)]
struct State {
    high: VecDeque<oneshot::Sender<()>>,
    normal: VecDeque<oneshot::Sender<()>>,
    admitted: usize,
    running: usize,
    starts: VecDeque<Instant>,
}

pub struct GitProcessScheduler {
    policy: GitProcessPolicy,
    state: Arc<Mutex<State>>,
}

/// Held while a git process runs; dropping it frees the slot and admits the next waiter.
pub struct ProcessPermit {
    scheduler: Arc<Mutex<State>>,
    max: usize,
    counted_running: bool,
}

impl Drop for ProcessPermit {
    fn drop(&mut self) {
        let mut s = self.scheduler.lock().unwrap();
        if self.counted_running {
            s.running = s.running.saturating_sub(1);
        }
        s.admitted = s.admitted.saturating_sub(1);
        drain(&mut s, self.max);
    }
}

/// A queued, not yet admitted acquire. Cancelling it owns no slot, unless the
/// admission signal raced in before the future was dropped.
struct Waiting {
    rx: Option<oneshot::Receiver<()>>,
    state: Arc<Mutex<State>>,
    max: usize,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        let Some(mut rx) = self.rx.take() else { return };
        rx.close();
        if rx.try_recv().is_ok() {
            let mut s = self.state.lock().unwrap();
            s.admitted = s.admitted.saturating_sub(1);
            drain(&mut s, self.max);
        }
    }
}

fn drain(s: &mut State, max: usize) {
    while s.admitted < max {
        let next = loop {
            match s.high.pop_front().or_else(|| s.normal.pop_front()) {
                // A waiter that gave up (dropped its future) must not hold a slot.
                Some(tx) if tx.is_closed() => continue,
                other => break other,
            }
        };
        let Some(tx) = next else { break };
        s.admitted += 1;
        if tx.send(()).is_err() {
            s.admitted -= 1;
        }
    }
}

impl GitProcessScheduler {
    pub fn new(policy: GitProcessPolicy) -> Self {
        Self {
            policy,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    pub fn policy(&self) -> GitProcessPolicy {
        self.policy
    }

    /// Processes currently running.
    pub fn active_count(&self) -> usize {
        self.state.lock().unwrap().running
    }

    /// Submissions waiting for admission.
    pub fn pending_count(&self) -> usize {
        let s = self.state.lock().unwrap();
        s.high.len() + s.normal.len()
    }

    /// Wait for admission and for a free slot in the strict one-second start budget.
    pub async fn acquire(&self, priority: GitProcessPriority) -> ProcessPermit {
        let (tx, rx) = oneshot::channel();
        {
            let mut s = self.state.lock().unwrap();
            match priority {
                GitProcessPriority::High => s.high.push_back(tx),
                GitProcessPriority::Normal => s.normal.push_back(tx),
            }
            drain(&mut s, self.policy.max_process_concurrency);
        }
        let mut waiting = Waiting {
            rx: Some(rx),
            state: self.state.clone(),
            max: self.policy.max_process_concurrency,
        };
        if waiting.rx.as_mut().unwrap().await.is_err() {
            waiting.rx = None;
            unreachable!("scheduler dropped a waiter without admitting it");
        }
        // Admitted: from here the permit owns the slot, including if this future is dropped.
        waiting.rx = None;
        let mut permit = ProcessPermit {
            scheduler: self.state.clone(),
            max: self.policy.max_process_concurrency,
            counted_running: false,
        };
        // The start budget: at most N starts in any trailing second.
        loop {
            let wait = {
                let mut s = self.state.lock().unwrap();
                let now = Instant::now();
                while s
                    .starts
                    .front()
                    .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(1))
                {
                    s.starts.pop_front();
                }
                if s.starts.len() < self.policy.max_processes_per_second {
                    s.starts.push_back(now);
                    s.running += 1;
                    permit.counted_running = true;
                    None
                } else {
                    Some(*s.starts.front().unwrap() + Duration::from_secs(1) - now)
                }
            };
            match wait {
                None => return permit,
                Some(d) => tokio::time::sleep(d).await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::task::yield_now;

    fn policy(per_second: usize, concurrency: usize) -> GitProcessPolicy {
        GitProcessPolicy {
            max_processes_per_second: per_second,
            max_process_concurrency: concurrency,
        }
    }

    async fn settle() {
        for _ in 0..20 {
            yield_now().await;
        }
    }

    #[tokio::test]
    async fn limits_unresolved_processes_independently_of_their_start_rate() {
        let scheduler = Arc::new(GitProcessScheduler::new(policy(100, 2)));
        let started = Arc::new(Mutex::new(Vec::<usize>::new()));
        let mut releases = Vec::new();
        let mut tasks = Vec::new();
        for index in 0..4usize {
            let (tx, rx) = oneshot::channel::<()>();
            releases.push(tx);
            let (s, st) = (scheduler.clone(), started.clone());
            tasks.push(tokio::spawn(async move {
                let permit = s.acquire(GitProcessPriority::Normal).await;
                st.lock().unwrap().push(index);
                let _ = rx.await;
                drop(permit);
            }));
            settle().await;
        }
        assert_eq!(*started.lock().unwrap(), vec![0, 1]);
        releases.remove(0).send(()).unwrap();
        settle().await;
        assert!(started.lock().unwrap().contains(&2));
        for tx in releases {
            let _ = tx.send(());
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(
            (scheduler.active_count(), scheduler.pending_count()),
            (0, 0)
        );
    }

    #[tokio::test]
    async fn admits_a_high_priority_user_operation_before_queued_observation_work() {
        let scheduler = Arc::new(GitProcessScheduler::new(policy(100, 1)));
        let started = Arc::new(Mutex::new(Vec::<usize>::new()));
        let first = scheduler.acquire(GitProcessPriority::Normal).await;
        started.lock().unwrap().push(0);
        let mut tasks = Vec::new();
        for (index, priority) in [
            (2usize, GitProcessPriority::Normal),
            (3, GitProcessPriority::High),
        ] {
            let (s, st) = (scheduler.clone(), started.clone());
            tasks.push(tokio::spawn(async move {
                let _permit = s.acquire(priority).await;
                st.lock().unwrap().push(index);
            }));
            settle().await;
        }
        assert_eq!(scheduler.pending_count(), 2);
        drop(first);
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(*started.lock().unwrap(), vec![0, 3, 2]);
    }

    #[tokio::test]
    async fn an_isolated_acquire_does_not_wait() {
        let scheduler = GitProcessScheduler::new(policy(100, 8));
        let permit = tokio::time::timeout(
            Duration::from_millis(50),
            scheduler.acquire(GitProcessPriority::Normal),
        )
        .await;
        assert!(permit.is_ok());
        assert_eq!(scheduler.active_count(), 1);
    }

    #[tokio::test]
    async fn abandoned_waiters_do_not_hold_a_slot() {
        let scheduler = Arc::new(GitProcessScheduler::new(policy(100, 1)));
        let first = scheduler.acquire(GitProcessPriority::Normal).await;
        let s = scheduler.clone();
        let abandoned = tokio::spawn(async move { s.acquire(GitProcessPriority::Normal).await });
        settle().await;
        abandoned.abort();
        let _ = abandoned.await;
        drop(first);
        let again = tokio::time::timeout(
            Duration::from_millis(200),
            scheduler.acquire(GitProcessPriority::Normal),
        )
        .await;
        assert!(again.is_ok(), "slot leaked to an abandoned waiter");
    }

    #[tokio::test]
    async fn cancelled_queued_acquire_does_not_free_a_slot_it_never_owned() {
        let scheduler = Arc::new(GitProcessScheduler::new(policy(100, 1)));
        let first = scheduler.acquire(GitProcessPriority::Normal).await;
        let timed_out = tokio::time::timeout(
            Duration::from_millis(50),
            scheduler.acquire(GitProcessPriority::Normal),
        )
        .await;
        assert!(timed_out.is_err());
        let c = tokio::time::timeout(
            Duration::from_millis(50),
            scheduler.acquire(GitProcessPriority::Normal),
        )
        .await;
        assert!(c.is_err(), "cap exceeded after cancellation");
        drop(first);
        let d = tokio::time::timeout(
            Duration::from_millis(200),
            scheduler.acquire(GitProcessPriority::Normal),
        )
        .await;
        assert!(d.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn limits_process_starts_in_each_strict_one_second_interval() {
        let scheduler = Arc::new(GitProcessScheduler::new(policy(2, 4)));
        let started = Arc::new(Mutex::new(Vec::<Instant>::new()));
        let count = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let (s, st, c) = (scheduler.clone(), started.clone(), count.clone());
            tasks.push(tokio::spawn(async move {
                let _permit = s.acquire(GitProcessPriority::Normal).await;
                st.lock().unwrap().push(Instant::now());
                c.fetch_add(1, Ordering::SeqCst);
            }));
        }
        tokio::time::sleep(Duration::from_millis(999)).await;
        assert!(count.load(Ordering::SeqCst) <= 2);
        for t in tasks {
            t.await.unwrap();
        }
        let at = started.lock().unwrap().clone();
        assert_eq!(at.len(), 4);
        assert!(at[2].duration_since(at[0]) >= Duration::from_secs(1));
    }

    #[test]
    fn policy_prefers_renamed_env_over_legacy_and_persisted_values() {
        let env = |k: &str| match k {
            "PASEO_GIT_MAX_PROCESSES_PER_SECOND" => Some("12".to_string()),
            "PASEO_GIT_MAX_PROCESS_CONCURRENCY" => Some("7".to_string()),
            "PASEO_GIT_CONCURRENCY" => Some("3".to_string()),
            _ => None,
        };
        assert_eq!(
            resolve_git_process_policy(&env, Some(policy(5, 4))),
            policy(12, 7)
        );
        let legacy = |k: &str| (k == "PASEO_GIT_CONCURRENCY").then(|| "3".to_string());
        assert_eq!(
            resolve_git_process_policy(&legacy, Some(policy(5, 4))),
            policy(5, 3)
        );
        let none = |_: &str| None;
        assert_eq!(
            resolve_git_process_policy(&none, None),
            DEFAULT_GIT_PROCESS_POLICY
        );
        let junk = |_: &str| Some("0".to_string());
        assert_eq!(
            resolve_git_process_policy(&junk, None),
            DEFAULT_GIT_PROCESS_POLICY
        );
    }
}
