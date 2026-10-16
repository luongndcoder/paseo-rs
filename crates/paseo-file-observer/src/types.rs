use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileChangeType {
    Create,
    Update,
    Delete,
}

/// One delivered change. Serializes to `{ "path": "...", "type": "create" }`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileChange {
    #[serde(with = "path_string")]
    pub path: PathBuf,
    #[serde(rename = "type")]
    pub change_type: FileChangeType,
}

impl FileChange {
    pub fn new(change_type: FileChangeType, path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            change_type,
        }
    }
}

mod path_string {
    use std::path::{Path, PathBuf};

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&path.to_string_lossy())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
        Ok(PathBuf::from(String::deserialize(deserializer)?))
    }
}

/// Options for a subscription. `ignore` holds absolute excluded subtrees.
#[derive(Debug, Clone, Default)]
pub struct FileObserverOptions {
    pub ignore: Vec<PathBuf>,
}

/// Aggregate runtime health, exact wire shape of the TS `FileObserverDiagnostics`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileObserverDiagnostics {
    pub active_observation_count: u64,
    pub native_handle_count: u64,
    pub native_tracked_file_count: u64,
    pub pending_event_count: u64,
    pub pending_reconciliation_work_count: u64,
    pub pending_classification_count: u64,
    pub reconciliation_in_flight_count: u64,
    pub reconciliation_count: u64,
    pub scoped_reconciliation_count: u64,
    pub full_reconciliation_count: u64,
    pub reconciliation_failure_count: u64,
    pub observer_failure_count: u64,
    pub directory_limit_failure_count: u64,
    pub native_event_count: u64,
    pub native_change_event_count: u64,
    pub native_rename_event_count: u64,
    pub native_pathless_event_count: u64,
    pub native_classification_count: u64,
    pub native_shallow_scan_count: u64,
    pub last_reconciliation_duration_ms: f64,
    pub max_reconciliation_duration_ms: f64,
}

/// Per-backend view, summed by the service.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackendDiagnostics {
    pub native_handle_count: u64,
    pub native_tracked_file_count: u64,
    pub pending_reconciliation_work_count: u64,
    pub pending_classification_count: u64,
    pub reconciliation_in_flight: bool,
}

/// Cumulative counters shared by every observation of one service.
#[derive(Debug, Default)]
pub struct SharedMetrics {
    pub reconciliation_count: AtomicU64,
    pub scoped_reconciliation_count: AtomicU64,
    pub full_reconciliation_count: AtomicU64,
    pub reconciliation_failure_count: AtomicU64,
    pub observer_failure_count: AtomicU64,
    pub directory_limit_failure_count: AtomicU64,
    pub native_event_count: AtomicU64,
    pub native_change_event_count: AtomicU64,
    pub native_rename_event_count: AtomicU64,
    pub native_pathless_event_count: AtomicU64,
    pub native_classification_count: AtomicU64,
    pub native_shallow_scan_count: AtomicU64,
    last_duration_bits: AtomicU64,
    max_duration_bits: AtomicU64,
}

impl SharedMetrics {
    pub fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_reconciliation_duration(&self, millis: f64) {
        self.last_duration_bits
            .store(millis.to_bits(), Ordering::Relaxed);
        let mut current = self.max_duration_bits.load(Ordering::Relaxed);
        while millis > f64::from_bits(current) {
            match self.max_duration_bits.compare_exchange_weak(
                current,
                millis.to_bits(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    pub(crate) fn fill(&self, out: &mut FileObserverDiagnostics) {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        out.reconciliation_count = load(&self.reconciliation_count);
        out.scoped_reconciliation_count = load(&self.scoped_reconciliation_count);
        out.full_reconciliation_count = load(&self.full_reconciliation_count);
        out.reconciliation_failure_count = load(&self.reconciliation_failure_count);
        out.observer_failure_count = load(&self.observer_failure_count);
        out.directory_limit_failure_count = load(&self.directory_limit_failure_count);
        out.native_event_count = load(&self.native_event_count);
        out.native_change_event_count = load(&self.native_change_event_count);
        out.native_rename_event_count = load(&self.native_rename_event_count);
        out.native_pathless_event_count = load(&self.native_pathless_event_count);
        out.native_classification_count = load(&self.native_classification_count);
        out.native_shallow_scan_count = load(&self.native_shallow_scan_count);
        out.last_reconciliation_duration_ms = f64::from_bits(load(&self.last_duration_bits));
        out.max_reconciliation_duration_ms = f64::from_bits(load(&self.max_duration_bits));
    }
}
