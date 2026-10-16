//! Wall clock seam. Timers use `tokio::time` (paused in tests); timestamps and `updatedAt`
//! monotonicity use this clock so tests can pin a millisecond.

use std::sync::atomic::{AtomicI64, Ordering};

pub use paseo_timeline::store::iso8601_from_millis;

pub trait Clock: Send + Sync + 'static {
    /// `Date.now()`.
    fn now_ms(&self) -> i64;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64)
    }
}

/// Settable clock for tests.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    pub fn new(ms: i64) -> Self {
        Self(AtomicI64::new(ms))
    }

    pub fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }

    pub fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// `touchUpdatedAt`: `max(now, previous + 1)`.
pub fn next_updated_at(previous_ms: i64, now_ms: i64) -> i64 {
    if now_ms > previous_ms {
        now_ms
    } else {
        previous_ms + 1
    }
}
