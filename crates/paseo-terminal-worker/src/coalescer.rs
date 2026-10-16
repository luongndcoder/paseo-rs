//! Port of terminal-output-coalescer.ts: leading-edge flush, then a 5ms trailing window.
//! Pure state machine; the owner supplies `now` and drives the timer via [`Coalescer::deadline`].

use std::time::{Duration, Instant};

pub const FLUSH_DELAY: Duration = Duration::from_millis(5);

pub struct Flushed {
    pub payload: String,
    /// Revision of the LAST chunk in the batch (the highest), for snapshot replay dedup.
    pub revision: Option<u64>,
}

pub struct Coalescer {
    delay: Duration,
    buf: String,
    revision: Option<u64>,
    deadline: Option<Instant>,
    last_flush: Option<Instant>,
}

impl Coalescer {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            buf: String::new(),
            revision: None,
            deadline: None,
            last_flush: None,
        }
    }

    pub fn handle(&mut self, data: &str, revision: u64, now: Instant) -> Option<Flushed> {
        if data.is_empty() {
            return None;
        }
        self.buf.push_str(data);
        self.revision = Some(revision);
        if self.deadline.is_none() {
            let elapsed = self.last_flush.map(|t| now.saturating_duration_since(t));
            if elapsed.is_none_or(|e| e >= self.delay) {
                return self.flush(now);
            }
            self.deadline = Some(now + self.delay);
        }
        None
    }

    pub fn flush(&mut self, now: Instant) -> Option<Flushed> {
        self.deadline = None;
        if self.buf.is_empty() {
            return None;
        }
        self.last_flush = Some(now);
        Some(Flushed {
            payload: std::mem::take(&mut self.buf),
            revision: self.revision.take(),
        })
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn on_timer(&mut self, now: Instant) -> Option<Flushed> {
        match self.deadline {
            Some(d) if now >= d => self.flush(now),
            _ => None,
        }
    }

    pub fn dispose(&mut self) {
        self.deadline = None;
        self.buf.clear();
        self.revision = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_edge_then_trailing_batch() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(FLUSH_DELAY);
        let f = c.handle("a", 1, t0).unwrap();
        assert_eq!((f.payload.as_str(), f.revision), ("a", Some(1)));
        assert!(c.handle("b", 2, t0 + Duration::from_millis(1)).is_none());
        assert!(c.handle("c", 3, t0 + Duration::from_millis(2)).is_none());
        assert!(c.on_timer(t0 + Duration::from_millis(3)).is_none());
        let f = c.on_timer(t0 + Duration::from_millis(6)).unwrap();
        assert_eq!((f.payload.as_str(), f.revision), ("bc", Some(3)));
        assert!(c.deadline().is_none());
    }

    #[test]
    fn flush_drains_and_idle_gap_flushes_leading() {
        let t0 = Instant::now();
        let mut c = Coalescer::new(FLUSH_DELAY);
        c.handle("a", 1, t0);
        c.handle("b", 2, t0 + Duration::from_millis(1));
        let f = c.flush(t0 + Duration::from_millis(2)).unwrap();
        assert_eq!(f.payload, "b");
        assert!(c.flush(t0).is_none());
        assert!(c.handle("c", 3, t0 + Duration::from_millis(20)).is_some());
    }

    #[test]
    fn empty_data_ignored() {
        let mut c = Coalescer::new(FLUSH_DELAY);
        assert!(c.handle("", 1, Instant::now()).is_none());
    }
}
