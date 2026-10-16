//! Single recursive native watcher (FSEvents / ReadDirectoryChangesW
//! semantics) plus an ignored-pruned, directory-indexed inventory. Native
//! events deliver immediately; named events trigger shallow scans of their
//! parent directories to recover creates/deletes the OS coalesced away, and a
//! delayed full audit is the last-resort safety net.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::backend::{Backend, RawEvent, RawKind, Timings, WatchSource, CLASSIFICATIONS_PER_SLICE};
use crate::error::FileObserverError;
use crate::host::Host;
use crate::paths::is_missing_io_error;
use crate::types::{BackendDiagnostics, FileChangeType, SharedMetrics};

pub const MAX_TRACKED_ENTRIES: usize = 250_000;
/// Beyond this queue depth, shed to a scoped directory scan, which finds the
/// same paths at a fraction of the cost of one stat per entry.
pub const MAX_QUEUED_CLASSIFICATIONS: usize = 2_048;

#[derive(Debug, Clone, Default)]
struct DirectoryEntry {
    directories: HashSet<PathBuf>,
    files: HashSet<PathBuf>,
}

#[derive(Default)]
struct Inventory {
    directories: HashSet<PathBuf>,
    entries: HashMap<PathBuf, DirectoryEntry>,
    files: HashSet<PathBuf>,
}

struct Classification {
    path: PathBuf,
    scope: PathBuf,
    known_directory: bool,
}

pub struct NativeRecursiveBackend {
    source: Box<dyn WatchSource>,
    watching: bool,
    timings: Timings,
    files: HashSet<PathBuf>,
    // Files classified before their parent has an entries bucket. The subtree
    // walk reaches files only through buckets, so these stay tracked directly
    // until a scoped audit builds the bucket and adopts them.
    orphan_files: HashSet<PathBuf>,
    directories: HashSet<PathBuf>,
    entries: HashMap<PathBuf, DirectoryEntry>,
    classification_queue: VecDeque<Classification>,
    local_scopes: BTreeSet<PathBuf>,
    change_scopes: BTreeSet<PathBuf>,
    recursive_scopes: BTreeSet<PathBuf>,
    audit_timer: Option<Instant>,
    full_audit_timer: Option<Instant>,
    full_audit_requested: bool,
    audit_dirty: bool,
    audit_dirty_since: Option<Instant>,
    audit_last_dirty_at: Option<Instant>,
    safety_audit_pending: bool,
    safety_audit_dirty_since: Option<Instant>,
    safety_audit_last_dirty_at: Option<Instant>,
    last_full_audit_at: Option<Instant>,
}

impl NativeRecursiveBackend {
    pub fn new(source: Box<dyn WatchSource>, timings: Timings) -> Self {
        Self {
            source,
            watching: false,
            timings,
            files: HashSet::new(),
            orphan_files: HashSet::new(),
            directories: HashSet::new(),
            entries: HashMap::new(),
            classification_queue: VecDeque::new(),
            local_scopes: BTreeSet::new(),
            change_scopes: BTreeSet::new(),
            recursive_scopes: BTreeSet::new(),
            audit_timer: None,
            full_audit_timer: None,
            full_audit_requested: false,
            audit_dirty: false,
            audit_dirty_since: None,
            audit_last_dirty_at: None,
            safety_audit_pending: false,
            safety_audit_dirty_since: None,
            safety_audit_last_dirty_at: None,
            last_full_audit_at: None,
        }
    }

    // ---- audit execution -------------------------------------------------

    fn run_audit(
        &mut self,
        host: &mut Host,
        emit_diff: bool,
        now: Instant,
    ) -> Result<(), FileObserverError> {
        if !host.is_active() {
            return Ok(());
        }
        let full_audit = self.full_audit_requested;
        let local: Vec<PathBuf> = std::mem::take(&mut self.local_scopes).into_iter().collect();
        let change: Vec<PathBuf> = std::mem::take(&mut self.change_scopes)
            .into_iter()
            .collect();
        let recursive: Vec<PathBuf> = std::mem::take(&mut self.recursive_scopes)
            .into_iter()
            .collect();
        self.full_audit_requested = false;
        if full_audit {
            self.clear_safety_audit();
        }
        self.audit_dirty = false;
        self.audit_dirty_since = None;
        self.audit_last_dirty_at = None;
        let started = Instant::now();
        let result = if full_audit {
            self.run_full_audit(host, emit_diff, now)
        } else {
            let result = self.reconcile_scopes(host, &local, &change, &recursive);
            if result.is_ok() {
                SharedMetrics::bump(&host.metrics.scoped_reconciliation_count);
            }
            result
        };
        if result.is_err() {
            SharedMetrics::bump(&host.metrics.reconciliation_failure_count);
        }
        SharedMetrics::bump(&host.metrics.reconciliation_count);
        host.metrics
            .record_reconciliation_duration(started.elapsed().as_secs_f64() * 1000.0);
        if self.safety_audit_pending && host.is_active() {
            self.schedule_full_audit(host, now);
        }
        if self.audit_dirty && host.is_active() {
            self.schedule_audit(host, now);
        }
        result
    }

    fn run_full_audit(
        &mut self,
        host: &mut Host,
        emit_diff: bool,
        now: Instant,
    ) -> Result<(), FileObserverError> {
        let next = self.scan_tree(host, &host.root.clone())?;
        if !host.is_active() {
            return Ok(());
        }
        if emit_diff {
            Self::queue_diff(host, &next.files, &self.files);
            Self::queue_removed_directories(host, &next.directories, self.directories.iter());
        }
        self.files = next.files;
        // A full scan gives every existing file a directory-index home.
        self.orphan_files.clear();
        self.directories = next.directories;
        self.entries = next.entries;
        self.last_full_audit_at = Some(now);
        SharedMetrics::bump(&host.metrics.full_reconciliation_count);
        Ok(())
    }

    fn scan_tree(&self, host: &Host, root: &Path) -> Result<Inventory, FileObserverError> {
        let mut inventory = Inventory::default();
        let mut pending = vec![root.to_path_buf()];
        while let Some(directory) = pending.pop() {
            if !host.is_active() {
                break;
            }
            let children = match fs::read_dir(&directory) {
                Ok(children) => children,
                Err(error) => {
                    if !is_missing_io_error(&error) || directory == host.root {
                        return Err(error.into());
                    }
                    continue;
                }
            };
            inventory.directories.insert(directory.clone());
            self.assert_within_limit(host, inventory.files.len() + inventory.directories.len())?;
            let mut entry = DirectoryEntry::default();
            for child in children {
                let child = match child {
                    Ok(child) => child,
                    Err(error) if is_missing_io_error(&error) => continue,
                    Err(error) => return Err(error.into()),
                };
                let path = child.path();
                if host.is_ignored(&path) {
                    continue;
                }
                if child.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    entry.directories.insert(path.clone());
                    pending.push(path);
                } else {
                    entry.files.insert(path.clone());
                    inventory.files.insert(path);
                    self.assert_within_limit(
                        host,
                        inventory.files.len() + inventory.directories.len(),
                    )?;
                }
            }
            inventory.entries.insert(directory, entry);
        }
        Ok(inventory)
    }

    fn reconcile_scopes(
        &mut self,
        host: &mut Host,
        local: &[PathBuf],
        change: &[PathBuf],
        recursive: &[PathBuf],
    ) -> Result<(), FileObserverError> {
        let forced_local: HashSet<&PathBuf> = local.iter().collect();
        let forced_recursive = host.paths.collapse(recursive);
        for directory in local {
            self.reconcile_directory(host, directory)?;
        }
        for directory in recursive {
            self.reconcile_subtree(host, directory)?;
        }
        // Parent scans are shallow, so a queued parent cannot cover its children.
        for directory in change {
            let covered = forced_local.contains(directory)
                || forced_recursive
                    .iter()
                    .any(|scope| host.is_path_inside(scope, directory));
            if covered {
                continue;
            }
            self.reconcile_directory(host, directory)?;
        }
        Ok(())
    }

    fn reconcile_directory(
        &mut self,
        host: &mut Host,
        directory: &Path,
    ) -> Result<(), FileObserverError> {
        if host.is_ignored(directory) {
            self.remove_subtree(host, directory, false);
            return Ok(());
        }
        if directory != host.root && !self.directories.contains(directory) {
            return self.reconcile_unknown_subtree(host, directory);
        }
        let Some(mut next) = self.read_directory(host, directory)? else {
            self.remove_subtree(host, directory, true);
            return Ok(());
        };
        let previous = self.entries.get(directory).cloned().unwrap_or_default();
        for path in &previous.files {
            if !next.files.contains(path) {
                self.files.remove(path);
                host.queue_event(FileChangeType::Delete, path);
            }
        }
        for path in &previous.directories {
            if !next.directories.contains(path) {
                self.remove_subtree(host, path, true);
            }
        }
        for path in &next.files {
            self.orphan_files.remove(path);
            if !previous.files.contains(path) {
                self.files.insert(path.clone());
                host.queue_event(FileChangeType::Create, path);
            }
        }
        let new_directories: Vec<PathBuf> = next.directories.iter().cloned().collect();
        for path in new_directories {
            if previous.directories.contains(&path) {
                continue;
            }
            let inventory = self.scan_tree(host, &path)?;
            if !inventory.directories.contains(&path) {
                next.directories.remove(&path);
                continue;
            }
            self.merge_inventory(host, inventory, true)?;
        }
        self.directories.insert(directory.to_path_buf());
        self.entries.insert(directory.to_path_buf(), next);
        self.assert_within_limit(host, self.files.len() + self.directories.len())
    }

    fn reconcile_unknown_subtree(
        &mut self,
        host: &mut Host,
        directory: &Path,
    ) -> Result<(), FileObserverError> {
        let inventory = self.scan_tree(host, directory)?;
        if !inventory.directories.contains(directory) {
            self.remove_subtree(host, directory, true);
            return Ok(());
        }
        if let Some(parent) = directory.parent().and_then(|p| self.entries.get_mut(p)) {
            parent.directories.insert(directory.to_path_buf());
        }
        self.merge_inventory(host, inventory, true)
    }

    fn read_directory(
        &self,
        host: &Host,
        directory: &Path,
    ) -> Result<Option<DirectoryEntry>, FileObserverError> {
        SharedMetrics::bump(&host.metrics.native_shallow_scan_count);
        let children = match fs::read_dir(directory) {
            Ok(children) => children,
            Err(error) => {
                if !is_missing_io_error(&error) || directory == host.root {
                    return Err(error.into());
                }
                return Ok(None);
            }
        };
        let mut entry = DirectoryEntry::default();
        for child in children {
            let child = match child {
                Ok(child) => child,
                Err(error) if is_missing_io_error(&error) => continue,
                Err(error) => return Err(error.into()),
            };
            let path = child.path();
            if host.is_ignored(&path) {
                continue;
            }
            if child.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                entry.directories.insert(path);
            } else {
                entry.files.insert(path);
            }
        }
        Ok(Some(entry))
    }

    fn reconcile_subtree(
        &mut self,
        host: &mut Host,
        directory: &Path,
    ) -> Result<(), FileObserverError> {
        if !self.directories.contains(directory) || host.is_ignored(directory) {
            return Ok(());
        }
        let inventory = self.scan_tree(host, directory)?;
        let tracked = self.walk_subtree(directory);
        let mut previous_files = HashSet::new();
        for dir in &tracked {
            if let Some(entry) = self.entries.get(dir) {
                previous_files.extend(entry.files.iter().cloned());
            }
        }
        Self::queue_diff(host, &inventory.files, &previous_files);
        Self::queue_removed_directories(host, &inventory.directories, tracked.iter());
        self.remove_subtree(host, directory, false);
        // remove_subtree unlinks `directory` from its parent as part of tearing
        // down the old subtree. Re-link it (it can vanish between the check
        // above and this scan) or an ancestor removal can no longer reach
        // anything below it.
        if inventory.directories.contains(directory) {
            if let Some(parent) = directory.parent().and_then(|p| self.entries.get_mut(p)) {
                parent.directories.insert(directory.to_path_buf());
            }
        }
        self.merge_inventory(host, inventory, false)
    }

    fn queue_diff(host: &mut Host, next: &HashSet<PathBuf>, previous: &HashSet<PathBuf>) {
        for path in next {
            if !previous.contains(path) {
                host.queue_event(FileChangeType::Create, path);
            }
        }
        for path in previous {
            if !next.contains(path) {
                host.queue_event(FileChangeType::Delete, path);
            }
        }
    }

    fn queue_removed_directories<'a>(
        host: &mut Host,
        next: &HashSet<PathBuf>,
        previous: impl Iterator<Item = &'a PathBuf>,
    ) {
        for directory in previous {
            if !next.contains(directory) {
                host.queue_event(FileChangeType::Delete, directory);
            }
        }
    }

    fn merge_inventory(
        &mut self,
        host: &mut Host,
        inventory: Inventory,
        emit_creates: bool,
    ) -> Result<(), FileObserverError> {
        self.directories.extend(inventory.directories);
        self.entries.extend(inventory.entries);
        for path in inventory.files {
            self.orphan_files.remove(&path);
            let added = self.files.insert(path.clone());
            if emit_creates && added {
                host.queue_event(FileChangeType::Create, &path);
            }
        }
        self.assert_within_limit(host, self.files.len() + self.directories.len())
    }

    /// Directories under `root` reachable through the entries index. Relies on
    /// `directories` and `entries` being written and cleared together; nothing
    /// enforces that beyond this comment.
    fn walk_subtree(&self, root: &Path) -> Vec<PathBuf> {
        let mut visited = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(directory) = stack.pop() {
            let Some(entry) = self.entries.get(&directory) else {
                continue;
            };
            stack.extend(entry.directories.iter().cloned());
            visited.push(directory);
        }
        visited
    }

    fn remove_subtree(&mut self, host: &mut Host, root: &Path, emit_deletes: bool) {
        for directory in self.walk_subtree(root) {
            if let Some(entry) = self.entries.remove(&directory) {
                for file in &entry.files {
                    if self.files.remove(file) && emit_deletes {
                        host.queue_event(FileChangeType::Delete, file);
                    }
                }
                // A native rename can name only a deleted file. The scoped scan
                // still knows its parent disappeared, so report that too.
                if emit_deletes {
                    host.queue_event(FileChangeType::Delete, &directory);
                }
            }
            self.directories.remove(&directory);
        }
        // Orphaned files have no directory-index home, so walk_subtree cannot
        // find them. Bounded by files classified within one full-audit window
        // whose parent never got a bucket.
        let orphans: Vec<PathBuf> = self
            .orphan_files
            .iter()
            .filter(|path| host.is_path_inside(root, path))
            .cloned()
            .collect();
        for path in orphans {
            self.orphan_files.remove(&path);
            if self.files.remove(&path) && emit_deletes {
                host.queue_event(FileChangeType::Delete, &path);
            }
        }
        // `root` can be a tracked file rather than a directory.
        if self.files.remove(root) && emit_deletes {
            host.queue_event(FileChangeType::Delete, root);
        }
        if let Some(parent) = root.parent().and_then(|p| self.entries.get_mut(p)) {
            parent.directories.remove(root);
            parent.files.remove(root);
        }
    }

    // ---- audit scheduling ------------------------------------------------

    fn request_audit(&mut self, host: &Host, scope: PathBuf, recursive: bool, now: Instant) {
        if !host.is_active() {
            return;
        }
        if recursive {
            self.recursive_scopes.insert(scope);
        } else {
            self.local_scopes.insert(scope);
        }
        self.mark_audit_dirty(host, now);
    }

    fn request_change_audit(&mut self, host: &Host, scope: PathBuf, now: Instant) {
        if !host.is_active() {
            return;
        }
        self.change_scopes.insert(scope);
        self.mark_audit_dirty(host, now);
    }

    fn mark_audit_dirty(&mut self, host: &Host, now: Instant) {
        self.request_safety_audit(host, now);
        self.audit_dirty = true;
        self.audit_dirty_since.get_or_insert(now);
        self.audit_last_dirty_at = Some(now);
        self.audit_timer = None;
        self.schedule_audit(host, now);
    }

    fn audit_deadline(&self, now: Instant) -> Instant {
        let (quiet, max_dirty) = if self.is_optional_only() {
            (
                self.timings.optional_audit_quiet,
                self.timings.optional_audit_max_dirty,
            )
        } else {
            (self.timings.audit_quiet, self.timings.audit_max_dirty)
        };
        let quiet_deadline = self.audit_last_dirty_at.unwrap_or(now) + quiet;
        let dirty_deadline = self.audit_dirty_since.unwrap_or(now) + max_dirty;
        quiet_deadline.min(dirty_deadline)
    }

    fn schedule_audit(&mut self, host: &Host, now: Instant) {
        if !host.is_active() || !self.audit_dirty || self.audit_timer.is_some() {
            return;
        }
        self.audit_timer = Some(self.audit_deadline(now));
    }

    fn run_scheduled_audit(&mut self, host: &mut Host, now: Instant) {
        self.audit_timer = None;
        if !host.is_active() {
            return;
        }
        if now < self.audit_deadline(now) {
            self.schedule_audit(host, now);
            return;
        }
        if let Err(error) = self.run_audit(host, true, now) {
            host.fail(error);
        }
    }

    fn is_optional_only(&self) -> bool {
        !self.full_audit_requested
            && self.local_scopes.is_empty()
            && self.recursive_scopes.is_empty()
            && !self.change_scopes.is_empty()
    }

    fn request_safety_audit(&mut self, host: &Host, now: Instant) {
        self.safety_audit_pending = true;
        self.safety_audit_dirty_since.get_or_insert(now);
        self.safety_audit_last_dirty_at = Some(now);
        self.schedule_full_audit(host, now);
    }

    fn schedule_full_audit(&mut self, host: &Host, now: Instant) {
        if !host.is_active() || !self.safety_audit_pending || self.full_audit_timer.is_some() {
            return;
        }
        self.full_audit_timer = Some(self.full_audit_deadline(now));
    }

    fn run_scheduled_full_audit(&mut self, host: &Host, now: Instant) {
        self.full_audit_timer = None;
        if !host.is_active() || !self.safety_audit_pending {
            return;
        }
        if now < self.full_audit_deadline(now) {
            self.schedule_full_audit(host, now);
            return;
        }
        self.safety_audit_pending = false;
        self.full_audit_requested = true;
        self.audit_dirty = true;
        self.audit_dirty_since.get_or_insert(now);
        self.schedule_audit(host, now);
    }

    fn full_audit_deadline(&self, now: Instant) -> Instant {
        let quiet = self.safety_audit_last_dirty_at.unwrap_or(now) + self.timings.full_audit_quiet;
        let starvation =
            self.safety_audit_dirty_since.unwrap_or(now) + self.timings.full_audit_max_dirty;
        let latest_of_quiet_and_interval = match self.last_full_audit_at {
            Some(last) => quiet.max(last + self.timings.full_audit_min_interval),
            None => quiet,
        };
        starvation.min(latest_of_quiet_and_interval)
    }

    fn clear_safety_audit(&mut self) {
        self.safety_audit_pending = false;
        self.safety_audit_dirty_since = None;
        self.safety_audit_last_dirty_at = None;
        self.full_audit_timer = None;
    }

    fn cancel_audits(&mut self) {
        self.audit_timer = None;
        self.full_audit_timer = None;
        self.local_scopes.clear();
        self.change_scopes.clear();
        self.recursive_scopes.clear();
        self.full_audit_requested = false;
        self.audit_dirty = false;
        self.audit_dirty_since = None;
        self.audit_last_dirty_at = None;
        self.safety_audit_pending = false;
        self.safety_audit_dirty_since = None;
        self.safety_audit_last_dirty_at = None;
    }

    // ---- classification --------------------------------------------------

    fn classify(
        &mut self,
        host: &Host,
        path: PathBuf,
        scope: PathBuf,
        known_directory: bool,
        now: Instant,
    ) {
        if self.classification_queue.len() >= MAX_QUEUED_CLASSIFICATIONS {
            // Shed onto the scoped audit, which reads the directory once
            // instead of stat-ing every entry in it.
            let parent = path.parent().map(Path::to_path_buf).unwrap_or(path);
            self.request_audit(host, parent, false, now);
            return;
        }
        self.classification_queue.push_back(Classification {
            path,
            scope,
            known_directory,
        });
    }

    fn pump_classifications(&mut self, host: &mut Host, now: Instant) {
        for _ in 0..CLASSIFICATIONS_PER_SLICE {
            if !host.is_active() {
                return;
            }
            let Some(next) = self.classification_queue.pop_front() else {
                return;
            };
            SharedMetrics::bump(&host.metrics.native_classification_count);
            match fs::metadata(&next.path) {
                Ok(metadata) => {
                    host.queue_event(FileChangeType::Create, &next.path);
                    if host.is_active() && !host.is_ignored(&next.path) {
                        self.on_present(host, next, metadata.is_dir(), now);
                    }
                }
                Err(error) if is_missing_io_error(&error) => {
                    host.queue_event(FileChangeType::Delete, &next.path)
                }
                Err(error) => host.fail(error.into()),
            }
        }
    }

    fn on_present(&mut self, host: &Host, item: Classification, is_directory: bool, now: Instant) {
        if !is_directory {
            // Remember files as soon as we announce them: a coalesced delete
            // must still be found by the next audit, even before the first
            // directory scan.
            self.files.insert(item.path.clone());
            if let Some(entry) = self.entries.get_mut(&item.scope) {
                entry.files.insert(item.path);
            } else {
                // No inventory for the parent yet. The create is already
                // announced, so it must stay findable for a matching delete.
                self.orphan_files.insert(item.path);
                self.request_audit(host, item.scope, false, now);
            }
            return;
        }
        if item.known_directory {
            self.request_audit(host, item.path, true, now);
        } else {
            self.request_audit(host, item.scope, false, now);
        }
    }

    fn assert_within_limit(&self, host: &Host, count: usize) -> Result<(), FileObserverError> {
        if count <= MAX_TRACKED_ENTRIES {
            return Ok(());
        }
        Err(FileObserverError::EntryLimit {
            limit: MAX_TRACKED_ENTRIES,
            root: host.root.clone(),
        })
    }
}

impl Backend for NativeRecursiveBackend {
    fn start(&mut self, host: &mut Host, now: Instant) -> Result<(), FileObserverError> {
        self.source.watch(&host.root.clone(), true)?;
        self.watching = true;
        self.full_audit_requested = true;
        self.run_audit(host, false, now)
    }

    fn update_ignore(&mut self, host: &mut Host, now: Instant) -> Result<(), FileObserverError> {
        self.classification_queue
            .retain(|item| !host.is_ignored(&item.path));
        self.cancel_audits();
        self.full_audit_requested = true;
        self.run_audit(host, true, now)
    }

    fn on_event(&mut self, host: &mut Host, event: RawEvent, now: Instant) {
        if !host.is_active() {
            return;
        }
        SharedMetrics::bump(&host.metrics.native_event_count);
        let root = host.root.clone();
        let Some(path) = event.path else {
            SharedMetrics::bump(&host.metrics.native_pathless_event_count);
            host.queue_event(FileChangeType::Update, &root);
            self.request_audit(host, root, false, now);
            return;
        };
        if host.is_ignored(&path) {
            return;
        }
        let scope = if path == root {
            root
        } else {
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| path.clone())
        };
        let known_directory = self.directories.contains(&path);
        self.request_change_audit(host, scope.clone(), now);
        if event.kind == RawKind::Change {
            SharedMetrics::bump(&host.metrics.native_change_event_count);
            host.queue_event(FileChangeType::Update, &path);
            if known_directory {
                self.request_audit(host, path.clone(), true, now);
            }
            if known_directory || self.files.contains(&path) {
                return;
            }
            // A coalesced creation can arrive only as a change. Classify
            // unknown paths so later deletion scans can find them.
        } else {
            SharedMetrics::bump(&host.metrics.native_rename_event_count);
            if known_directory {
                self.request_audit(host, scope.clone(), false, now);
            }
        }
        self.classify(host, path, scope, known_directory, now);
    }

    fn on_watch_error(&mut self, host: &mut Host, error: FileObserverError, _now: Instant) {
        if host.is_active() {
            host.fail(error);
        }
    }

    fn tick(&mut self, host: &mut Host, now: Instant) {
        if !host.is_active() {
            return;
        }
        self.pump_classifications(host, now);
        if self.full_audit_timer.is_some_and(|at| now >= at) {
            self.run_scheduled_full_audit(host, now);
        }
        if self.audit_timer.is_some_and(|at| now >= at) {
            self.run_scheduled_audit(host, now);
        }
    }

    fn next_wake(&self) -> Option<Instant> {
        match (self.audit_timer, self.full_audit_timer) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn has_ready_work(&self) -> bool {
        !self.classification_queue.is_empty()
    }

    fn diagnostics(&self) -> BackendDiagnostics {
        BackendDiagnostics {
            native_handle_count: u64::from(self.watching),
            native_tracked_file_count: self.files.len() as u64,
            pending_reconciliation_work_count: (self.local_scopes.len()
                + self.change_scopes.len()
                + self.recursive_scopes.len()
                + self.classification_queue.len()
                + usize::from(self.audit_timer.is_some() || self.audit_dirty))
                as u64,
            pending_classification_count: self.classification_queue.len() as u64,
            reconciliation_in_flight: false,
        }
    }

    fn close(&mut self) {
        self.cancel_audits();
        self.classification_queue.clear();
        self.source.close();
        self.watching = false;
        self.files.clear();
        self.orphan_files.clear();
        self.directories.clear();
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::linux::tests::{host_for, FakeSource};
    use crate::types::FileChange;

    /// Drives the backend with a synthetic clock over a real filesystem, so
    /// the 8s/30s audit windows cost nothing.
    struct Rig {
        host: Host,
        backend: NativeRecursiveBackend,
        now: Instant,
        root: PathBuf,
        _dir: tempfile::TempDir,
    }

    impl Rig {
        fn new(prefill: impl FnOnce(&Path)) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            prefill(&root);
            let mut rig = Self {
                host: host_for(&root),
                backend: NativeRecursiveBackend::new(
                    Box::new(FakeSource::default()),
                    Timings::default(),
                ),
                now: Instant::now(),
                root,
                _dir: dir,
            };
            rig.backend.start(&mut rig.host, rig.now).unwrap();
            rig
        }

        fn emit(&mut self, kind: RawKind, path: &Path) {
            let event = RawEvent {
                kind,
                path: Some(path.to_path_buf()),
            };
            self.backend.on_event(&mut self.host, event, self.now);
        }

        fn settle(&mut self) {
            for _ in 0..64 {
                self.backend.tick(&mut self.host, self.now);
                let timer_due = self.backend.next_wake().is_some_and(|at| at <= self.now);
                if !self.backend.has_ready_work() && !timer_due {
                    return;
                }
            }
            panic!("backend did not settle");
        }

        fn advance(&mut self, by: Duration) {
            self.now += by;
            self.settle();
        }

        fn types(&self, kind: FileChangeType) -> Vec<PathBuf> {
            let mut paths: Vec<PathBuf> = self
                .host
                .raw()
                .iter()
                .filter(|c| c.change_type == kind)
                .map(|c| c.path.clone())
                .collect();
            paths.sort();
            paths
        }

        fn tracked(&self) -> u64 {
            self.backend.diagnostics().native_tracked_file_count
        }
    }

    fn sorted(mut v: Vec<PathBuf>) -> Vec<PathBuf> {
        v.sort();
        v
    }

    // Native watchers may coalesce file removals into change notifications.
    fn shallow_scans_recover_coalesced_deletes(population_new: bool) {
        let mut rig = Rig::new(|root| fs::create_dir_all(root.join("child/deep")).unwrap());
        let removed = vec![
            rig.root.join("root.txt"),
            rig.root.join("child/child.txt"),
            rig.root.join("child/deep/deep.txt"),
        ];
        if !population_new {
            // Written before the watch: only the initial inventory knows them.
            for path in &removed {
                fs::write(path, "before").unwrap();
            }
            rig.backend.close();
            rig.backend =
                NativeRecursiveBackend::new(Box::new(FakeSource::default()), Timings::default());
            rig.host = host_for(&rig.root);
            rig.backend.start(&mut rig.host, rig.now).unwrap();
        } else {
            for path in &removed {
                fs::write(path, "before").unwrap();
            }
            for path in &removed {
                rig.emit(RawKind::Change, path);
            }
            rig.settle();
        }
        for path in &removed {
            fs::remove_file(path).unwrap();
        }
        for path in &removed {
            rig.emit(RawKind::Change, path);
        }
        rig.advance(Duration::from_secs(8));
        assert_eq!(rig.types(FileChangeType::Delete), sorted(removed));
    }

    #[test]
    fn shallow_scans_recover_coalesced_deletes_of_existing_files() {
        shallow_scans_recover_coalesced_deletes(false);
    }

    #[test]
    fn shallow_scans_recover_coalesced_deletes_of_new_files() {
        shallow_scans_recover_coalesced_deletes(true);
    }

    #[test]
    fn a_file_announced_only_as_changed_remains_visible_to_coalesced_deletion_scans() {
        let mut rig = Rig::new(|root| fs::create_dir(root.join("nested")).unwrap());
        let directory = rig.root.join("nested");
        let path = directory.join("changed.txt");
        fs::write(&path, "created").unwrap();
        rig.emit(RawKind::Change, &path);
        rig.settle();
        assert_eq!(rig.tracked(), 1);

        fs::remove_file(&path).unwrap();
        rig.emit(RawKind::Change, &directory);
        rig.advance(Duration::from_secs(10));
        assert_eq!(rig.types(FileChangeType::Delete), vec![path]);
    }

    #[test]
    fn a_native_ignore_update_drains_classifications_queued_below_the_new_excluded_root() {
        let mut rig = Rig::new(|root| {
            fs::create_dir(root.join("generated")).unwrap();
            fs::write(root.join("tracked.txt"), "tracked").unwrap();
        });
        let ignored = rig.root.join("generated");
        let tracked = rig.root.join("tracked.txt");
        for index in 0..2_000 {
            let path = ignored.join(format!("file-{index}.js"));
            rig.emit(RawKind::Rename, &path);
        }
        rig.emit(RawKind::Rename, &tracked);
        assert!(rig.backend.diagnostics().pending_classification_count > 0);
        rig.host.replace_ignored(&[ignored]);
        rig.backend.update_ignore(&mut rig.host, rig.now).unwrap();
        assert!(rig.backend.diagnostics().pending_classification_count <= 1);
        rig.settle();
        assert!(rig.types(FileChangeType::Create).contains(&tracked));
    }

    // One stat per native event is how a dependency install pins the pool.
    #[test]
    fn a_rename_burst_does_not_queue_one_stat_per_event() {
        let mut rig = Rig::new(|_| {});
        for index in 0..5_000 {
            let path = rig.root.join(format!("package-{index}")).join("index.js");
            rig.emit(RawKind::Rename, &path);
        }
        assert!(rig.backend.diagnostics().pending_classification_count <= 2_080);
    }

    // The shed path is only safe because this backend has an inventory to
    // diff against: force a shed onto a tracked directory, the create must
    // still surface.
    #[test]
    fn a_classification_shed_onto_the_scoped_audit_still_recovers_the_file() {
        let mut rig = Rig::new(|root| fs::create_dir(root.join("shed")).unwrap());
        let filler = rig.root.join("filler");
        for index in 0..(MAX_QUEUED_CLASSIFICATIONS + 32) {
            rig.emit(RawKind::Rename, &filler.join(format!("file-{index}.js")));
        }
        assert_eq!(
            rig.backend.diagnostics().pending_classification_count as usize,
            MAX_QUEUED_CLASSIFICATIONS
        );
        let shed_path = rig.root.join("shed/recovered.txt");
        fs::write(&shed_path, "content").unwrap();
        rig.emit(RawKind::Rename, &shed_path);

        // 2s clears the mandatory-audit deadline (500ms quiet / 5s max-dirty)
        // the shed forces, but stays under the 8s optional-only deadline the
        // ambient change-scope audit alone would use. Only the shed's own
        // request_audit can have done the work.
        rig.advance(Duration::from_secs(2));
        assert!(rig
            .host
            .raw()
            .contains(&FileChange::new(FileChangeType::Create, &shed_path)));
    }

    // C1 regression: remove_subtree unlinks its root from the parent entry;
    // reconcile_subtree must re-link it or an ancestor removal cannot reach
    // anything below.
    #[test]
    fn a_recursive_scope_reconcile_re_links_its_directory() {
        let mut rig = Rig::new(|root| {
            fs::create_dir_all(root.join("a/b/c")).unwrap();
            fs::write(root.join("a/b/c/deep.txt"), "content").unwrap();
        });
        let deep_directory = rig.root.join("a/b/c");
        let deep_file = deep_directory.join("deep.txt");
        rig.emit(RawKind::Rename, &rig.root.join("a/b"));
        rig.advance(Duration::from_secs(1));
        // The recursive scope on a/b rebuilt its subtree and re-linked it.
        assert!(rig
            .backend
            .walk_subtree(&rig.root.join("a"))
            .contains(&deep_directory));

        fs::remove_dir_all(rig.root.join("a")).unwrap();
        rig.emit(RawKind::Rename, &deep_file);
        rig.advance(Duration::from_secs(10));
        assert!(rig.types(FileChangeType::Delete).contains(&deep_file));
        assert!(rig.types(FileChangeType::Delete).contains(&deep_directory));
    }

    #[test]
    fn a_full_native_inventory_audit_reports_a_removed_directory() {
        let mut rig = Rig::new(|root| {
            fs::create_dir(root.join("removed")).unwrap();
            fs::write(root.join("removed/tracked.txt"), "before").unwrap();
        });
        let directory = rig.root.join("removed");
        fs::remove_dir_all(&directory).unwrap();
        rig.backend.update_ignore(&mut rig.host, rig.now).unwrap();
        assert!(rig.types(FileChangeType::Delete).contains(&directory));
    }

    // I2 regression: the create is announced before the parent bucket exists,
    // so the file must stay tracked or its delete can never be emitted.
    #[test]
    fn a_file_classified_with_no_parent_bucket_still_gets_its_delete_emitted() {
        let mut rig = Rig::new(|_| {});
        let new_directory = rig.root.join("newdir");
        fs::create_dir(&new_directory).unwrap();
        let file = new_directory.join("orphan.txt");
        fs::write(&file, "content").unwrap();
        rig.emit(RawKind::Rename, &file);
        rig.settle();
        assert_eq!(rig.tracked(), 1);
        assert!(rig.types(FileChangeType::Create).contains(&file));

        fs::remove_dir_all(&new_directory).unwrap();
        rig.advance(Duration::from_secs(10));
        assert!(rig.types(FileChangeType::Delete).contains(&file));
        assert_eq!(rig.tracked(), 0);
    }

    #[test]
    fn newly_announced_files_are_remembered_before_reconciliation() {
        let mut rig = Rig::new(|_| {});
        let path = rig.root.join("new.txt");
        fs::write(&path, "created").unwrap();
        rig.emit(RawKind::Rename, &path);
        rig.settle();
        assert!(rig.types(FileChangeType::Create).contains(&path));
        // A later audit needs it in its comparison inventory even if the
        // native watcher coalesces the subsequent delete.
        assert_eq!(rig.tracked(), 1);
        fs::remove_file(&path).unwrap();
        rig.backend.update_ignore(&mut rig.host, rig.now).unwrap();
        assert!(rig.types(FileChangeType::Delete).contains(&path));
    }

    // docs/file-observation.md: activity arms a full safety audit 30s after it
    // settles; it must recover a delete no event ever announced.
    #[test]
    fn the_safety_audit_recovers_a_silent_delete_after_activity_settles() {
        let mut rig = Rig::new(|root| {
            fs::create_dir(root.join("sub")).unwrap();
            fs::write(root.join("sub/silent.txt"), "x").unwrap();
        });
        let silent = rig.root.join("sub/silent.txt");
        fs::remove_file(&silent).unwrap();
        // Root-level activity arms the safety audit; its shallow scan of the root
        // cannot see into sub/, so only the full audit can find the delete.
        rig.emit(RawKind::Change, &rig.root.join("elsewhere.txt"));
        rig.advance(Duration::from_secs(29));
        assert!(!rig.types(FileChangeType::Delete).contains(&silent));
        // The full audit arms at 30s. TS parity: with no interim activity the
        // mandatory-audit quiet window keeps sliding, so the run lands on the
        // 5s max-dirty bound after arming.
        rig.advance(Duration::from_secs(2));
        rig.advance(Duration::from_secs(5));
        assert!(rig.types(FileChangeType::Delete).contains(&silent));
        assert_eq!(
            rig.backend.diagnostics().pending_reconciliation_work_count,
            0,
            "{:?} {:?} {:?} {} {}",
            rig.backend.local_scopes,
            rig.backend.change_scopes,
            rig.backend.recursive_scopes,
            rig.backend.audit_dirty,
            rig.backend.audit_timer.is_some()
        );
    }

    #[test]
    fn exceeding_the_entry_limit_is_an_error() {
        let rig = Rig::new(|_| {});
        assert!(rig
            .backend
            .assert_within_limit(&rig.host, MAX_TRACKED_ENTRIES + 1)
            .is_err());
    }

    #[test]
    fn pathless_events_update_the_root_and_request_a_shallow_scan() {
        let mut rig = Rig::new(|_| {});
        rig.backend.on_event(
            &mut rig.host,
            RawEvent {
                kind: RawKind::Change,
                path: None,
            },
            rig.now,
        );
        assert!(rig
            .host
            .raw()
            .contains(&FileChange::new(FileChangeType::Update, &rig.root)));
        let file = rig.root.join("late.txt");
        fs::write(&file, "x").unwrap();
        rig.advance(Duration::from_secs(1));
        assert!(rig.types(FileChangeType::Create).contains(&file));
        assert_eq!(
            rig.host
                .metrics
                .native_pathless_event_count
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }
}
