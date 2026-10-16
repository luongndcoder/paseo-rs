//! Injectable time and retry policy of the relationship actor (TS `HubRelationshipClock`,
//! `HubRelationshipRetryPolicy`, `relationship-retry.ts`).

use std::sync::Arc;

use paseo_core::time::iso8601_from_millis;

/// A task handed to [`HubClock::schedule`]; cancelling it makes it never run.
pub trait ScheduledTask: Send {
    fn cancel(&self);
}

/// Wall clock plus a one-shot scheduler. The scheduled task only posts an event into the
/// actor mailbox, so it may run on any thread.
pub trait HubClock: Send + Sync + 'static {
    /// `Date.now()`.
    fn now_ms(&self) -> i64;

    fn schedule(&self, delay_ms: u64, task: Box<dyn FnOnce() + Send>) -> Box<dyn ScheduledTask>;

    /// `new Date().toISOString()`.
    fn now_iso(&self) -> String {
        iso8601_from_millis(self.now_ms())
    }
}

/// Real clock: `tokio::time` timers (so `start_paused` tests work).
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemHubClock;

struct TokioTimer(tokio::task::JoinHandle<()>);

impl ScheduledTask for TokioTimer {
    fn cancel(&self) {
        self.0.abort();
    }
}

impl HubClock for SystemHubClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64)
    }

    fn schedule(&self, delay_ms: u64, task: Box<dyn FnOnce() + Send>) -> Box<dyn ScheduledTask> {
        Box::new(TokioTimer(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            task();
        })))
    }
}

pub trait RetryPolicy: Send + Sync + 'static {
    /// Delay before retry number `attempt` (0 based), in milliseconds.
    fn delay(&self, attempt: u32) -> u64;
}

const INITIAL_DELAY_MS: f64 = 500.0;
const MAX_BASE_DELAY_MS: f64 = 30_000.0;
const MIN_JITTER_FACTOR: f64 = 0.75;
const JITTER_RANGE: f64 = 0.5;

/// `BoundedExponentialHubRetryPolicy`: `max(1, round(min(30000, 500 * 2^attempt) * jitter))`
/// with `jitter = 0.75 + random * 0.5`.
pub struct BoundedExponentialRetryPolicy {
    random: Box<dyn Fn() -> f64 + Send + Sync>,
}

impl BoundedExponentialRetryPolicy {
    pub fn new(random: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        Self {
            random: Box::new(random),
        }
    }
}

impl Default for BoundedExponentialRetryPolicy {
    fn default() -> Self {
        Self::new(rand::random::<f64>)
    }
}

impl RetryPolicy for BoundedExponentialRetryPolicy {
    fn delay(&self, attempt: u32) -> u64 {
        // `2 ** attempt` is Infinity for huge attempts in JS; `min` brings it back to the cap.
        let base = (INITIAL_DELAY_MS * 2f64.powf(f64::from(attempt))).min(MAX_BASE_DELAY_MS);
        let jitter = MIN_JITTER_FACTOR + (self.random)() * JITTER_RANGE;
        // `Math.round` rounds half up; the value is positive so `round` agrees.
        ((base * jitter).round() as u64).max(1)
    }
}

pub type SharedClock = Arc<dyn HubClock>;
pub type SharedRetryPolicy = Arc<dyn RetryPolicy>;
