use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::FileObserverError;
use crate::paths::ObserverPaths;
use crate::types::{FileChange, FileChangeType, SharedMetrics};

/// What a backend sees of its observation: root, exclusion set, event queue
/// with batching/coalescing, and the failure slot.
pub struct Host {
    pub root: PathBuf,
    pub paths: ObserverPaths,
    pub metrics: Arc<SharedMetrics>,
    /// Loop time, set before every backend call so timers are testable.
    pub now: Instant,
    ignored: Vec<PathBuf>,
    active: bool,
    failure: Option<FileObserverError>,
    pending: Vec<FileChange>,
    index: HashMap<PathBuf, usize>,
    flush_deadline: Option<Instant>,
    batch_delay: Duration,
    raw_log: Option<Vec<FileChange>>,
}

impl Host {
    pub fn new(
        root: PathBuf,
        ignore: &[PathBuf],
        paths: ObserverPaths,
        metrics: Arc<SharedMetrics>,
        batch_delay: Duration,
    ) -> Self {
        let ignored = paths.normalize_ignored_roots(&root, ignore);
        Self {
            root,
            paths,
            metrics,
            now: Instant::now(),
            ignored,
            active: true,
            failure: None,
            pending: Vec::new(),
            index: HashMap::new(),
            flush_deadline: None,
            batch_delay,
            raw_log: None,
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn is_ignored(&self, path: &Path) -> bool {
        self.ignored
            .iter()
            .any(|root| self.paths.is_inside(root, path))
    }

    pub fn is_path_inside(&self, root: &Path, path: &Path) -> bool {
        self.paths.is_inside(root, path)
    }

    pub fn ignored_roots(&self) -> &[PathBuf] {
        &self.ignored
    }

    /// Replaces the exclusion set and drops queued events below it. Returns
    /// false when the normalized set is unchanged.
    pub(crate) fn replace_ignored(&mut self, paths: &[PathBuf]) -> bool {
        let next = self.paths.normalize_ignored_roots(&self.root, paths);
        if next == self.ignored {
            return false;
        }
        self.ignored = next;
        let pending = std::mem::take(&mut self.pending);
        self.index.clear();
        for change in pending {
            if !self.is_ignored(&change.path) {
                self.index.insert(change.path.clone(), self.pending.len());
                self.pending.push(change);
            }
        }
        true
    }

    pub fn queue_event(&mut self, change_type: FileChangeType, path: &Path) {
        if !self.active || self.is_ignored(path) {
            return;
        }
        let next = FileChange::new(change_type, path);
        if let Some(log) = &mut self.raw_log {
            log.push(next.clone());
        }
        match self.index.get(path) {
            Some(&slot) => {
                let merged = merge_change(&self.pending[slot], next);
                self.pending[slot] = merged;
            }
            None => {
                self.index.insert(path.to_path_buf(), self.pending.len());
                self.pending.push(next);
            }
        }
        if self.flush_deadline.is_none() {
            self.flush_deadline = Some(self.now + self.batch_delay);
        }
    }

    /// Marks the observation failed. Backends must stop work; the loop
    /// delivers the error and tears the backend down.
    pub fn fail(&mut self, error: FileObserverError) {
        if !self.active {
            return;
        }
        self.active = false;
        self.failure = Some(error);
        self.cancel_delivery();
    }

    pub(crate) fn take_failure(&mut self) -> Option<FileObserverError> {
        self.failure.take()
    }

    pub(crate) fn close(&mut self) {
        self.active = false;
        self.cancel_delivery();
    }

    pub(crate) fn cancel_delivery(&mut self) {
        self.pending.clear();
        self.index.clear();
        self.flush_deadline = None;
    }

    pub(crate) fn flush_deadline(&self) -> Option<Instant> {
        self.flush_deadline
    }

    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub(crate) fn take_due_batch(&mut self, now: Instant) -> Option<Vec<FileChange>> {
        match self.flush_deadline {
            Some(deadline) if now >= deadline => {}
            _ => return None,
        }
        self.flush_deadline = None;
        if !self.active || self.pending.is_empty() {
            return None;
        }
        self.index.clear();
        Some(std::mem::take(&mut self.pending))
    }

    /// Test hook: keep every queued event unmerged.
    #[cfg(test)]
    pub(crate) fn record_raw(&mut self) {
        self.raw_log = Some(Vec::new());
    }

    #[cfg(test)]
    pub(crate) fn raw(&self) -> &[FileChange] {
        self.raw_log.as_deref().unwrap_or(&[])
    }
}

fn merge_change(previous: &FileChange, next: FileChange) -> FileChange {
    use FileChangeType::{Create, Delete};
    match (previous.change_type, next.change_type) {
        (Create, Delete) | (Delete, Create) => FileChange::new(FileChangeType::Update, next.path),
        (Create, _) => previous.clone(),
        _ => next,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::PathPlatform;

    fn host() -> Host {
        Host::new(
            PathBuf::from("/r"),
            &[],
            ObserverPaths::new(PathPlatform::Posix),
            Arc::default(),
            Duration::from_millis(10),
        )
    }

    fn kinds(host: &mut Host, events: &[FileChangeType]) -> FileChangeType {
        for event in events {
            host.queue_event(*event, Path::new("/r/a"));
        }
        let batch = host
            .take_due_batch(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(batch.len(), 1);
        batch[0].change_type
    }

    #[test]
    fn create_then_delete_collapses_to_update() {
        use FileChangeType::*;
        assert_eq!(kinds(&mut host(), &[Create, Delete]), Update);
        assert_eq!(kinds(&mut host(), &[Delete, Create]), Update);
    }

    #[test]
    fn create_survives_later_update_and_update_yields_to_delete() {
        use FileChangeType::*;
        assert_eq!(kinds(&mut host(), &[Create, Update]), Create);
        assert_eq!(kinds(&mut host(), &[Update, Delete]), Delete);
    }

    #[test]
    fn ignore_replacement_drops_queued_events_below_new_root() {
        let mut host = host();
        host.queue_event(FileChangeType::Create, Path::new("/r/gen/x"));
        host.queue_event(FileChangeType::Create, Path::new("/r/keep"));
        assert!(host.replace_ignored(&[PathBuf::from("/r/gen")]));
        assert!(!host.replace_ignored(&[PathBuf::from("/r/gen")]));
        let batch = host
            .take_due_batch(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(
            batch,
            vec![FileChange::new(FileChangeType::Create, "/r/keep")]
        );
    }
}
