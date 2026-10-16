//! Per-directory backend (inotify semantics): one non-recursive watch per
//! directory, topology repaired by trailing-debounced scoped scans. It keeps
//! no file inventory, so it reports zero tracked files and fails loudly rather
//! than shedding a classification it could never recover.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::backend::{Backend, RawEvent, RawKind, Timings, WatchSource, CLASSIFICATIONS_PER_SLICE};
use crate::error::FileObserverError;
use crate::host::Host;
use crate::paths::is_missing_io_error;
use crate::types::{BackendDiagnostics, FileChangeType, SharedMetrics};

pub const MAX_WATCHED_DIRECTORIES: usize = 5_000;
pub const MAX_QUEUED_CLASSIFICATIONS: usize = 16_384;

struct DirectorySnapshot {
    directories: HashSet<PathBuf>,
    files_discovered_before_coverage: Vec<PathBuf>,
}

pub struct LinuxBackend {
    source: Box<dyn WatchSource>,
    watchers: BTreeSet<PathBuf>,
    classification_queue: VecDeque<PathBuf>,
    pending_scopes: BTreeSet<PathBuf>,
    reconcile_at: Option<Instant>,
    timings: Timings,
}

impl LinuxBackend {
    pub fn new(source: Box<dyn WatchSource>, timings: Timings) -> Self {
        Self {
            source,
            watchers: BTreeSet::new(),
            classification_queue: VecDeque::new(),
            pending_scopes: BTreeSet::new(),
            reconcile_at: None,
            timings,
        }
    }

    fn watch_directory(
        &mut self,
        host: &mut Host,
        directory: &Path,
    ) -> Result<(), FileObserverError> {
        if !host.is_active() || self.watchers.contains(directory) || host.is_ignored(directory) {
            return Ok(());
        }
        if self.watchers.len() >= MAX_WATCHED_DIRECTORIES {
            SharedMetrics::bump(&host.metrics.directory_limit_failure_count);
            return Err(directory_limit(host));
        }
        self.source.watch(directory, false)?;
        self.watchers.insert(directory.to_path_buf());
        Ok(())
    }

    fn request_reconcile(&mut self, host: &Host, scope: PathBuf, now: Instant) {
        if !host.is_active() {
            return;
        }
        self.pending_scopes.insert(scope);
        if self.reconcile_at.is_none() {
            self.reconcile_at = Some(now + self.timings.linux_reconcile_delay);
        }
    }

    fn run(&mut self, host: &mut Host, scopes: Vec<PathBuf>) -> Result<(), FileObserverError> {
        if !host.is_active() {
            return Ok(());
        }
        let started = Instant::now();
        let collapsed = host.paths.collapse(&scopes);
        let result = self.reconcile(host, collapsed);
        if result.is_err() {
            SharedMetrics::bump(&host.metrics.reconciliation_failure_count);
        }
        SharedMetrics::bump(&host.metrics.reconciliation_count);
        SharedMetrics::bump(&host.metrics.scoped_reconciliation_count);
        host.metrics
            .record_reconciliation_duration(started.elapsed().as_secs_f64() * 1000.0);
        result
    }

    fn reconcile(
        &mut self,
        host: &mut Host,
        scopes: Vec<PathBuf>,
    ) -> Result<(), FileObserverError> {
        for scope in scopes {
            let mut snapshot = self.scan(host, &scope)?;
            if !host.is_active() {
                return Ok(());
            }
            let found: Vec<PathBuf> = snapshot.directories.iter().cloned().collect();
            for directory in found {
                if !self.watch_snapshot_directory(host, &directory)? {
                    snapshot.directories.remove(&directory);
                }
            }
            for path in &snapshot.files_discovered_before_coverage {
                host.queue_event(FileChangeType::Create, path);
            }
            let disappeared: Vec<PathBuf> = self
                .watchers
                .iter()
                .filter(|directory| {
                    **directory != host.root
                        && host.is_path_inside(&scope, directory)
                        && !snapshot.directories.contains(*directory)
                })
                .cloned()
                .collect();
            for directory in disappeared {
                self.source.unwatch(&directory);
                self.watchers.remove(&directory);
                host.queue_event(FileChangeType::Delete, &directory);
            }
        }
        Ok(())
    }

    fn watch_snapshot_directory(
        &mut self,
        host: &mut Host,
        directory: &Path,
    ) -> Result<bool, FileObserverError> {
        match self.watch_directory(host, directory) {
            Ok(()) => Ok(true),
            Err(error) => {
                if !host.paths.is_expected_watch_disappearance(&error) || directory == host.root {
                    Err(error)
                } else {
                    Ok(false)
                }
            }
        }
    }

    fn scan(&self, host: &Host, scope: &Path) -> Result<DirectorySnapshot, FileObserverError> {
        let empty = || DirectorySnapshot {
            directories: HashSet::new(),
            files_discovered_before_coverage: Vec::new(),
        };
        let metadata = match fs::metadata(scope) {
            Ok(metadata) => metadata,
            Err(error) => {
                if !is_missing_io_error(&error) || scope == host.root {
                    return Err(error.into());
                }
                return Ok(empty());
            }
        };
        if !metadata.is_dir() {
            return Ok(empty());
        }
        let mut directories = HashSet::from([scope.to_path_buf()]);
        let mut files = Vec::new();
        let mut pending = vec![scope.to_path_buf()];
        while let Some(directory) = pending.pop() {
            if !host.is_active() {
                break;
            }
            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(error) => {
                    if !is_missing_io_error(&error) {
                        return Err(error.into());
                    }
                    if directory == host.root && host.is_active() {
                        return Err(error.into());
                    }
                    continue;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) if is_missing_io_error(&error) => continue,
                    Err(error) => return Err(error.into()),
                };
                let child = entry.path();
                if host.is_ignored(&child) {
                    continue;
                }
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if !is_dir {
                    if !self.watchers.contains(&directory) {
                        files.push(child);
                    }
                    continue;
                }
                directories.insert(child.clone());
                if directories.len() > MAX_WATCHED_DIRECTORIES {
                    SharedMetrics::bump(&host.metrics.directory_limit_failure_count);
                    return Err(directory_limit(host));
                }
                pending.push(child);
            }
        }
        Ok(DirectorySnapshot {
            directories,
            files_discovered_before_coverage: files,
        })
    }

    fn classify(&mut self, host: &mut Host, path: PathBuf) {
        if self.classification_queue.len() >= MAX_QUEUED_CLASSIFICATIONS {
            // No inventory and no safety audit: a shed would silently lose
            // this create/delete. Fail so the owner falls back to polling.
            host.fail(FileObserverError::ClassificationLimit {
                limit: MAX_QUEUED_CLASSIFICATIONS,
                root: host.root.clone(),
            });
            return;
        }
        self.classification_queue.push_back(path);
    }

    fn pump_classifications(&mut self, host: &mut Host) {
        for _ in 0..CLASSIFICATIONS_PER_SLICE {
            if !host.is_active() {
                return;
            }
            let Some(path) = self.classification_queue.pop_front() else {
                return;
            };
            SharedMetrics::bump(&host.metrics.native_classification_count);
            match fs::metadata(&path) {
                Ok(_) => host.queue_event(FileChangeType::Create, &path),
                Err(error) if is_missing_io_error(&error) => {
                    host.queue_event(FileChangeType::Delete, &path)
                }
                Err(error) => host.fail(error.into()),
            }
        }
    }

    fn cancel_queued(&mut self) {
        self.reconcile_at = None;
        self.pending_scopes.clear();
    }
}

fn directory_limit(host: &Host) -> FileObserverError {
    FileObserverError::DirectoryLimit {
        limit: MAX_WATCHED_DIRECTORIES,
        root: host.root.clone(),
    }
}

impl Backend for LinuxBackend {
    fn start(&mut self, host: &mut Host, _now: Instant) -> Result<(), FileObserverError> {
        let root = host.root.clone();
        self.watch_directory(host, &root)?;
        self.run(host, vec![root])
    }

    fn update_ignore(&mut self, host: &mut Host, _now: Instant) -> Result<(), FileObserverError> {
        self.classification_queue
            .retain(|path| !host.is_ignored(path));
        let ignored: Vec<PathBuf> = self
            .watchers
            .iter()
            .filter(|directory| **directory != host.root && host.is_ignored(directory))
            .cloned()
            .collect();
        for directory in ignored {
            self.source.unwatch(&directory);
            self.watchers.remove(&directory);
        }
        self.cancel_queued();
        let root = host.root.clone();
        self.run(host, vec![root])
    }

    fn on_event(&mut self, host: &mut Host, event: RawEvent, now: Instant) {
        if !host.is_active() {
            return;
        }
        let pathless = event.path.is_none();
        let path = event.path.unwrap_or_else(|| host.root.clone());
        if !host.is_ignored(&path) {
            if event.kind == RawKind::Change {
                host.queue_event(FileChangeType::Update, &path);
            } else {
                self.classify(host, path.clone());
            }
        }
        if event.kind == RawKind::Rename || pathless {
            self.request_reconcile(host, path, now);
        }
    }

    fn on_watch_error(&mut self, host: &mut Host, error: FileObserverError, now: Instant) {
        if !host.is_active() {
            return;
        }
        if !host.paths.is_expected_watch_disappearance(&error) {
            host.fail(error);
            return;
        }
        let mut scopes: Vec<PathBuf> = error.watch_paths().to_vec();
        if scopes.is_empty() {
            scopes.push(host.root.clone());
        }
        for directory in scopes {
            if self.watchers.remove(&directory) {
                self.source.unwatch(&directory);
            }
            if directory == host.root {
                host.fail(error);
                return;
            }
            self.request_reconcile(host, directory, now);
        }
    }

    fn tick(&mut self, host: &mut Host, now: Instant) {
        if !host.is_active() {
            return;
        }
        self.pump_classifications(host);
        if !host.is_active() {
            return;
        }
        if self.reconcile_at.is_some_and(|at| now >= at) {
            self.reconcile_at = None;
            let scopes: Vec<PathBuf> = std::mem::take(&mut self.pending_scopes)
                .into_iter()
                .collect();
            if let Err(error) = self.run(host, scopes) {
                host.fail(error);
            }
        }
    }

    fn next_wake(&self) -> Option<Instant> {
        self.reconcile_at
    }

    fn has_ready_work(&self) -> bool {
        !self.classification_queue.is_empty()
    }

    fn diagnostics(&self) -> BackendDiagnostics {
        BackendDiagnostics {
            native_handle_count: self.watchers.len() as u64,
            native_tracked_file_count: 0,
            pending_reconciliation_work_count: (self.pending_scopes.len()
                + self.classification_queue.len()
                + usize::from(self.reconcile_at.is_some()))
                as u64,
            pending_classification_count: self.classification_queue.len() as u64,
            reconciliation_in_flight: false,
        }
    }

    fn close(&mut self) {
        self.cancel_queued();
        self.classification_queue.clear();
        self.source.close();
        self.watchers.clear();
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::*;
    use crate::paths::{ObserverPaths, PathPlatform};
    use crate::types::FileChange;

    /// Records watch calls; shared so tests can inspect after boxing.
    #[derive(Clone, Default)]
    pub(crate) struct FakeSource {
        pub watched: Arc<Mutex<Vec<PathBuf>>>,
        pub fail_paths: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl WatchSource for FakeSource {
        fn watch(&mut self, dir: &Path, _recursive: bool) -> Result<(), FileObserverError> {
            if self.fail_paths.lock().unwrap().iter().any(|p| p == dir) {
                return Err(FileObserverError::Io {
                    kind: std::io::ErrorKind::NotFound,
                    message: "gone".into(),
                });
            }
            self.watched.lock().unwrap().push(dir.to_path_buf());
            Ok(())
        }
        fn unwatch(&mut self, dir: &Path) {
            self.watched.lock().unwrap().retain(|p| p != dir);
        }
        fn close(&mut self) {
            self.watched.lock().unwrap().clear();
        }
    }

    pub(crate) fn host_for(root: &Path) -> Host {
        let mut host = Host::new(
            root.to_path_buf(),
            &[],
            ObserverPaths::new(PathPlatform::Posix),
            Arc::default(),
            Duration::from_millis(10),
        );
        host.record_raw();
        host
    }

    fn rename(path: PathBuf) -> RawEvent {
        RawEvent {
            kind: RawKind::Rename,
            path: Some(path),
        }
    }

    // The linux backend has no inventory and no audit, so a shed
    // classification could never be recovered: overflow must fail loudly.
    #[test]
    fn classification_queue_overflow_fails_the_observation() {
        let dir = tempfile::tempdir().unwrap();
        let mut host = host_for(dir.path());
        let mut backend = LinuxBackend::new(Box::new(FakeSource::default()), Timings::default());
        let now = Instant::now();
        backend.start(&mut host, now).unwrap();
        for index in 0..20_000 {
            backend.on_event(
                &mut host,
                rename(dir.path().join(format!("module-{index}.js"))),
                now,
            );
        }
        assert!(!host.is_active());
        let error = host.take_failure().unwrap();
        assert!(matches!(
            error,
            FileObserverError::ClassificationLimit { .. }
        ));
        assert!(error.to_string().contains("queued classifications"));
    }

    #[test]
    fn ignore_update_drains_classifications_queued_below_new_excluded_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let ignored = root.join("generated");
        fs::create_dir(&ignored).unwrap();
        let tracked = root.join("tracked.txt");
        fs::write(&tracked, "tracked").unwrap();
        let mut host = host_for(&root);
        let mut backend = LinuxBackend::new(Box::new(FakeSource::default()), Timings::default());
        let now = Instant::now();
        backend.start(&mut host, now).unwrap();
        for index in 0..6_000 {
            backend.on_event(
                &mut host,
                rename(ignored.join(format!("file-{index}.js"))),
                now,
            );
        }
        backend.on_event(&mut host, rename(tracked.clone()), now);
        assert!(backend.diagnostics().pending_classification_count > 0);
        host.replace_ignored(&[ignored]);
        backend.update_ignore(&mut host, now).unwrap();
        assert!(backend.diagnostics().pending_classification_count <= 1);
        while backend.has_ready_work() {
            backend.tick(&mut host, now);
        }
        assert!(host
            .raw()
            .contains(&FileChange::new(FileChangeType::Create, &tracked)));
    }

    #[test]
    fn start_watches_every_directory_and_reports_files_found_before_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/x.txt"), "x").unwrap();
        let source = FakeSource::default();
        let mut host = host_for(&root);
        let mut backend = LinuxBackend::new(Box::new(source.clone()), Timings::default());
        backend.start(&mut host, Instant::now()).unwrap();
        assert_eq!(backend.diagnostics().native_handle_count, 3);
        assert_eq!(source.watched.lock().unwrap().len(), 3);
        // Root was watched before the scan, so its own files are covered; the
        // nested file lives in a directory discovered by the scan.
        assert!(host.raw().contains(&FileChange::new(
            FileChangeType::Create,
            root.join("a/b/x.txt")
        )));
    }

    #[test]
    fn removed_directory_is_unwatched_and_reported_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let child = root.join("child");
        fs::create_dir(&child).unwrap();
        let source = FakeSource::default();
        let mut host = host_for(&root);
        let mut backend = LinuxBackend::new(Box::new(source.clone()), Timings::default());
        let t0 = Instant::now();
        backend.start(&mut host, t0).unwrap();
        fs::remove_dir(&child).unwrap();
        backend.on_event(&mut host, rename(root.clone()), t0);
        backend.tick(&mut host, t0 + Duration::from_secs(1));
        assert_eq!(backend.diagnostics().native_handle_count, 1);
        assert!(host
            .raw()
            .contains(&FileChange::new(FileChangeType::Delete, &child)));
    }

    #[test]
    fn watch_error_on_root_fails_but_child_disappearance_is_churn() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let child = root.join("child");
        fs::create_dir(&child).unwrap();
        let mut host = host_for(&root);
        let mut backend = LinuxBackend::new(Box::new(FakeSource::default()), Timings::default());
        let now = Instant::now();
        backend.start(&mut host, now).unwrap();
        let missing = |p: &Path| FileObserverError::Watch {
            message: "gone".into(),
            paths: vec![p.to_path_buf()],
            missing: true,
            permission_denied: false,
        };
        backend.on_watch_error(&mut host, missing(&child), now);
        assert!(host.is_active());
        backend.on_watch_error(&mut host, missing(&root), now);
        assert!(!host.is_active());
    }
}
