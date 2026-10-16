use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::error::FileObserverError;
use crate::host::Host;
use crate::types::BackendDiagnostics;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawKind {
    /// Content or metadata modification.
    Change,
    /// Create, delete or rename: the path needs classification.
    Rename,
}

/// Normalized native notification (the equivalent of Node's `fs.watch` event).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEvent {
    pub kind: RawKind,
    /// Absolute path under the observed root. `None` for pathless notices.
    pub path: Option<PathBuf>,
}

pub enum Incoming {
    Event(RawEvent),
    Error(FileObserverError),
}

pub type EventSink = std::sync::Arc<dyn Fn(Incoming) + Send + Sync>;

/// Platform watch handle owner. Dropping or `close()` releases every native
/// handle.
pub trait WatchSource: Send {
    fn watch(&mut self, dir: &std::path::Path, recursive: bool) -> Result<(), FileObserverError>;
    fn unwatch(&mut self, dir: &std::path::Path);
    fn close(&mut self);
}

pub type SourceFactory =
    Box<dyn FnOnce(EventSink) -> Result<Box<dyn WatchSource>, FileObserverError> + Send>;

/// Timing knobs. Defaults match the TS implementation.
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    pub event_batch_delay: Duration,
    pub linux_reconcile_delay: Duration,
    pub audit_quiet: Duration,
    pub audit_max_dirty: Duration,
    pub optional_audit_quiet: Duration,
    pub optional_audit_max_dirty: Duration,
    pub full_audit_quiet: Duration,
    pub full_audit_min_interval: Duration,
    pub full_audit_max_dirty: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            event_batch_delay: Duration::from_millis(10),
            linux_reconcile_delay: Duration::from_millis(50),
            audit_quiet: Duration::from_millis(500),
            audit_max_dirty: Duration::from_millis(5_000),
            optional_audit_quiet: Duration::from_millis(8_000),
            optional_audit_max_dirty: Duration::from_millis(8_000),
            full_audit_quiet: Duration::from_millis(30_000),
            full_audit_min_interval: Duration::from_millis(30_000),
            full_audit_max_dirty: Duration::from_millis(5 * 60_000),
        }
    }
}

/// Deterministic, single-threaded state machine. Every method receives the
/// current time so timers can be driven without sleeping.
pub trait Backend: Send {
    fn start(&mut self, host: &mut Host, now: Instant) -> Result<(), FileObserverError>;
    fn update_ignore(&mut self, host: &mut Host, now: Instant) -> Result<(), FileObserverError>;
    fn on_event(&mut self, host: &mut Host, event: RawEvent, now: Instant);
    fn on_watch_error(&mut self, host: &mut Host, error: FileObserverError, now: Instant);
    /// Runs due timers and one bounded slice of ready work.
    fn tick(&mut self, host: &mut Host, now: Instant);
    fn next_wake(&self) -> Option<Instant>;
    fn has_ready_work(&self) -> bool;
    fn diagnostics(&self) -> BackendDiagnostics;
    fn close(&mut self);
}

/// Matches `MAX_CONCURRENT_CLASSIFICATIONS`: stat calls per work slice.
pub(crate) const CLASSIFICATIONS_PER_SLICE: usize = 32;
