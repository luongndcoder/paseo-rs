//! Project `.git` root observer (docs/architecture.md, "Project identity is
//! daemon-global"): one non-recursive watch on each lexically equivalent
//! active project root, reacting only to the root `.git` entry, with a slow
//! rescan as the missed-event fallback. It deliberately never watches the
//! working tree. Ported from the root-watch half of
//! `workspace-reconciliation-service.ts`; the registry reconciliation that
//! consumes the signals stays with the caller.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, watch};

use crate::backend::{EventSink, Incoming, WatchSource};
use crate::error::FileObserverError;
use crate::watcher::NotifySource;

pub const DEFAULT_RESCAN_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(100);

/// What the caller should reconcile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectRootSignal {
    /// A root `.git` entry changed (debounced). Reconcile Git metadata.
    GitEntryChanged,
    /// Periodic pass: run the full reconciliation. Missing watchers have been
    /// re-attempted just before this fires.
    Rescan,
}

pub type RootWatchFactory =
    Arc<dyn Fn(&Path, EventSink) -> Result<Box<dyn WatchSource>, FileObserverError> + Send + Sync>;

#[derive(Clone)]
pub struct ProjectRootConfig {
    pub debounce: Duration,
    pub rescan_interval: Duration,
    pub factory: RootWatchFactory,
}

impl Default for ProjectRootConfig {
    fn default() -> Self {
        Self {
            debounce: DEFAULT_DEBOUNCE,
            rescan_interval: DEFAULT_RESCAN_INTERVAL,
            factory: Arc::new(|root, sink| {
                let mut source = NotifySource::new(root, sink)?;
                source.watch(root, false)?;
                Ok(Box::new(source) as Box<dyn WatchSource>)
            }),
        }
    }
}

struct Target {
    id: u64,
    root: PathBuf,
    source: Box<dyn WatchSource>,
}

/// Deterministic core: time is an argument, attaching a watch is a callback.
pub(crate) struct Core {
    debounce: Duration,
    rescan_interval: Duration,
    active: Vec<PathBuf>,
    targets: Vec<Target>,
    next_id: u64,
    debounce_at: Option<Instant>,
    rescan_at: Instant,
}

pub(crate) type Attach<'a> =
    &'a mut dyn FnMut(&Path, u64) -> Result<Box<dyn WatchSource>, FileObserverError>;

impl Core {
    pub(crate) fn new(debounce: Duration, rescan_interval: Duration, now: Instant) -> Self {
        Self {
            debounce,
            rescan_interval,
            active: Vec::new(),
            targets: Vec::new(),
            next_id: 0,
            debounce_at: None,
            rescan_at: now + rescan_interval,
        }
    }

    #[cfg(test)]
    pub(crate) fn watched_roots(&self) -> Vec<PathBuf> {
        self.targets.iter().map(|t| t.root.clone()).collect()
    }

    pub(crate) fn set_roots(&mut self, roots: &[PathBuf], attach: Attach<'_>) {
        self.active = roots.to_vec();
        self.sync(attach);
    }

    /// Tears down watches of roots no longer active, installs missing ones.
    /// A root that cannot be watched yet is retried on the next sync.
    pub(crate) fn sync(&mut self, attach: Attach<'_>) {
        let active = &self.active;
        self.targets.retain_mut(|target| {
            let keep = active
                .iter()
                .any(|root| are_equivalent_paths(root, &target.root));
            if !keep {
                target.source.close();
            }
            keep
        });
        for root in self.active.clone() {
            if self
                .targets
                .iter()
                .any(|target| are_equivalent_paths(&target.root, &root))
            {
                continue;
            }
            let id = self.next_id;
            if let Ok(source) = attach(&root, id) {
                self.next_id += 1;
                self.targets.push(Target { id, root, source });
            }
        }
    }

    pub(crate) fn on_event(&mut self, id: u64, path: Option<&Path>, now: Instant) {
        let Some(target) = self.targets.iter().find(|t| t.id == id) else {
            return;
        };
        let relevant = match path {
            None => true,
            Some(path) => {
                path.file_name().is_some_and(|name| name == ".git")
                    && path.parent().is_some_and(|parent| parent == target.root)
            }
        };
        if relevant && self.debounce_at.is_none() {
            self.debounce_at = Some(now + self.debounce);
        }
    }

    pub(crate) fn on_error(&mut self, id: u64) {
        if let Some(index) = self.targets.iter().position(|t| t.id == id) {
            let mut target = self.targets.remove(index);
            target.source.close();
        }
    }

    pub(crate) fn next_wake(&self) -> Instant {
        self.debounce_at
            .map_or(self.rescan_at, |at| at.min(self.rescan_at))
    }

    pub(crate) fn tick(&mut self, now: Instant, attach: Attach<'_>) -> Vec<ProjectRootSignal> {
        let mut signals = Vec::new();
        if self.debounce_at.is_some_and(|at| now >= at) {
            self.debounce_at = None;
            self.sync(attach);
            signals.push(ProjectRootSignal::GitEntryChanged);
        }
        if now >= self.rescan_at {
            self.rescan_at = now + self.rescan_interval;
            self.sync(attach);
            signals.push(ProjectRootSignal::Rescan);
        }
        signals
    }

    pub(crate) fn dispose(&mut self) {
        self.debounce_at = None;
        self.active.clear();
        for target in &mut self.targets {
            target.source.close();
        }
        self.targets.clear();
    }
}

enum Msg {
    Event(u64, Incoming),
    SetRoots(Vec<PathBuf>, oneshot::Sender<()>),
    Dispose,
}

/// Daemon-global project root observer. Runs for empty projects and without
/// connected clients.
pub struct ProjectRootObserver {
    tx: mpsc::Sender<Msg>,
    closed: watch::Receiver<bool>,
}

impl ProjectRootObserver {
    pub fn new<F>(config: ProjectRootConfig, on_signal: F) -> Self
    where
        F: Fn(ProjectRootSignal) + Send + Sync + 'static,
    {
        let (tx, rx) = mpsc::channel::<Msg>();
        let (closed_tx, closed) = watch::channel(false);
        let loop_tx = tx.clone();
        std::thread::Builder::new()
            .name("paseo-project-root-observer".into())
            .spawn(move || {
                let factory = config.factory.clone();
                let mut attach = move |root: &Path, id: u64| {
                    let tx = loop_tx.clone();
                    let sink: EventSink = Arc::new(move |incoming| {
                        let _ = tx.send(Msg::Event(id, incoming));
                    });
                    factory(root, sink)
                };
                let mut core = Core::new(config.debounce, config.rescan_interval, Instant::now());
                loop {
                    let wait = core.next_wake().saturating_duration_since(Instant::now());
                    let msg = match rx.recv_timeout(wait) {
                        Ok(msg) => Some(msg),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => Some(Msg::Dispose),
                    };
                    let now = Instant::now();
                    match msg {
                        Some(Msg::Event(id, Incoming::Event(event))) => {
                            core.on_event(id, event.path.as_deref(), now)
                        }
                        Some(Msg::Event(id, Incoming::Error(_))) => core.on_error(id),
                        Some(Msg::SetRoots(roots, ack)) => {
                            core.set_roots(&roots, &mut attach);
                            let _ = ack.send(());
                        }
                        Some(Msg::Dispose) => break,
                        None => {}
                    }
                    for signal in core.tick(now, &mut attach) {
                        on_signal(signal);
                    }
                }
                core.dispose();
                closed_tx.send_replace(true);
            })
            .expect("spawn project root observer thread");
        Self { tx, closed }
    }

    /// Sets the active project roots. Resolves once every watchable root has
    /// its watch installed, which closes the `git init` race for new empty
    /// projects. Unwatchable roots are retried on later passes.
    pub async fn sync_roots(&self, roots: Vec<PathBuf>) {
        let (ack, done) = oneshot::channel();
        if self.tx.send(Msg::SetRoots(roots, ack)).is_ok() {
            let _ = done.await;
        }
    }

    /// Barrier: no watch handle or signal remains after this resolves.
    pub async fn dispose(&self) {
        let _ = self.tx.send(Msg::Dispose);
        let mut closed = self.closed.clone();
        let _ = closed.wait_for(|done| *done).await;
    }
}

impl Drop for ProjectRootObserver {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Dispose);
    }
}

// ---- path equivalence (string-only, mirrors utils/path.ts) --------------

fn looks_like_windows_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    let sep = |b: u8| b == b'\\' || b == b'/';
    let device =
        bytes.len() >= 4 && sep(bytes[0]) && sep(bytes[1]) && bytes[2] == b'?' && sep(bytes[3]);
    let unc =
        value.starts_with("\\\\") && value[2..].split('\\').filter(|s| !s.is_empty()).count() >= 2;
    drive || device || unc
}

fn normalize_for_comparison(value: &str, windows: bool) -> String {
    let mut text = value.to_string();
    if windows {
        // `\\?\C:\x` -> `C:\x`
        let swapped = text.replace('/', "\\");
        text = match swapped.strip_prefix("\\\\?\\") {
            Some(rest) if rest.len() >= 2 && rest.as_bytes()[1] == b':' => rest.to_string(),
            _ => swapped,
        };
    }
    let sep = if windows { '\\' } else { '/' };
    let (prefix, rest) = split_root(&text, windows);
    let mut stack: Vec<&str> = Vec::new();
    for segment in rest.split(sep) {
        match segment {
            "" | "." => {}
            ".." => {
                if !stack.is_empty() && stack.last() != Some(&"..") {
                    stack.pop();
                } else if prefix.is_empty() {
                    stack.push("..");
                }
            }
            other => stack.push(other),
        }
    }
    let joined = format!("{prefix}{}", stack.join(&sep.to_string()));
    if windows {
        joined.to_lowercase()
    } else {
        joined
    }
}

fn split_root(text: &str, windows: bool) -> (String, &str) {
    if windows {
        if let Some(rest) = text.strip_prefix("\\\\") {
            return ("\\\\".to_string(), rest);
        }
        if text.len() >= 2 && text.as_bytes()[1] == b':' {
            let rest = text[2..].trim_start_matches('\\');
            return (format!("{}\\", &text[..2]), rest);
        }
        if let Some(rest) = text.strip_prefix('\\') {
            return ("\\".to_string(), rest);
        }
        (String::new(), text)
    } else if let Some(rest) = text.strip_prefix('/') {
        ("/".to_string(), rest)
    } else {
        (String::new(), text)
    }
}

/// String-only equivalence: separators, dot segments, trailing separators and
/// Windows namespace prefixes are normalized; case folds only for Windows
/// shapes. Never touches the filesystem.
pub fn are_equivalent_paths(left: &Path, right: &Path) -> bool {
    let (left, right) = (left.to_string_lossy(), right.to_string_lossy());
    let windows = looks_like_windows_path(&left) || looks_like_windows_path(&right);
    normalize_for_comparison(&left, windows) == normalize_for_comparison(&right, windows)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const DEBOUNCE: Duration = Duration::from_millis(10);
    const RESCAN: Duration = Duration::from_millis(50);

    #[derive(Clone, Default)]
    struct Fakes {
        installed: Arc<Mutex<Vec<PathBuf>>>,
        closed: Arc<Mutex<Vec<PathBuf>>>,
        fail_next: Arc<Mutex<Vec<PathBuf>>>,
    }

    struct FakeWatch {
        root: PathBuf,
        closed: Arc<Mutex<Vec<PathBuf>>>,
        done: bool,
    }

    impl WatchSource for FakeWatch {
        fn watch(&mut self, _: &Path, _: bool) -> Result<(), FileObserverError> {
            Ok(())
        }
        fn unwatch(&mut self, _: &Path) {}
        fn close(&mut self) {
            if !self.done {
                self.done = true;
                self.closed.lock().unwrap().push(self.root.clone());
            }
        }
    }

    impl Fakes {
        fn attach(
            &self,
        ) -> impl FnMut(&Path, u64) -> Result<Box<dyn WatchSource>, FileObserverError> {
            let fakes = self.clone();
            move |root, _id| {
                let mut fail = fakes.fail_next.lock().unwrap();
                if let Some(index) = fail.iter().position(|p| p == root) {
                    fail.remove(index);
                    return Err(FileObserverError::Io {
                        kind: std::io::ErrorKind::NotFound,
                        message: "root unavailable".into(),
                    });
                }
                fakes.installed.lock().unwrap().push(root.to_path_buf());
                Ok(Box::new(FakeWatch {
                    root: root.to_path_buf(),
                    closed: fakes.closed.clone(),
                    done: false,
                }))
            }
        }
    }

    fn roots(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    fn core(now: Instant) -> Core {
        Core::new(DEBOUNCE, RESCAN, now)
    }

    #[test]
    fn installs_watches_for_new_roots_and_deduplicates_equivalent_ones() {
        let fakes = Fakes::default();
        let mut attach = fakes.attach();
        let mut core = core(Instant::now());
        core.set_roots(&roots(&["/p/repo", "/p/repo/", "/p/other"]), &mut attach);
        core.set_roots(&roots(&["/p/repo", "/p/repo/", "/p/other"]), &mut attach);
        assert_eq!(core.watched_roots(), roots(&["/p/repo", "/p/other"]));
        assert_eq!(fakes.installed.lock().unwrap().len(), 2);
    }

    #[test]
    fn tears_down_watches_when_roots_are_archived_or_removed() {
        let fakes = Fakes::default();
        let mut attach = fakes.attach();
        let mut core = core(Instant::now());
        core.set_roots(&roots(&["/p/repo", "/p/other"]), &mut attach);
        core.set_roots(&roots(&["/p/other"]), &mut attach);
        assert_eq!(core.watched_roots(), roots(&["/p/other"]));
        core.set_roots(&[], &mut attach);
        assert!(core.watched_roots().is_empty());
        assert_eq!(
            *fakes.closed.lock().unwrap(),
            roots(&["/p/repo", "/p/other"])
        );
    }

    #[test]
    fn filters_unrelated_files_and_coalesces_git_bursts() {
        let fakes = Fakes::default();
        let mut attach = fakes.attach();
        let t0 = Instant::now();
        let mut core = core(t0);
        core.set_roots(&roots(&["/p/repo"]), &mut attach);
        core.on_event(0, Some(Path::new("/p/repo/README.md")), t0);
        core.on_event(0, Some(Path::new("/p/repo/.git/HEAD")), t0);
        assert!(core.tick(t0 + DEBOUNCE, &mut attach).is_empty());

        core.on_event(0, Some(Path::new("/p/repo/.git")), t0);
        core.on_event(0, Some(Path::new("/p/repo/.git")), t0);
        core.on_event(0, None, t0);
        assert_eq!(
            core.tick(t0 + DEBOUNCE, &mut attach),
            vec![ProjectRootSignal::GitEntryChanged]
        );
        assert!(core.tick(t0 + DEBOUNCE * 2, &mut attach).is_empty());
    }

    #[test]
    fn recovers_errored_and_unavailable_watchers_on_the_periodic_pass() {
        let fakes = Fakes::default();
        fakes
            .fail_next
            .lock()
            .unwrap()
            .push(PathBuf::from("/p/unavailable"));
        let mut attach = fakes.attach();
        let t0 = Instant::now();
        let mut core = core(t0);
        core.set_roots(&roots(&["/p/errored", "/p/unavailable"]), &mut attach);
        assert_eq!(core.watched_roots(), roots(&["/p/errored"]));

        core.on_error(0);
        assert!(core.watched_roots().is_empty());
        assert_eq!(
            core.tick(t0 + RESCAN, &mut attach),
            vec![ProjectRootSignal::Rescan]
        );
        assert_eq!(
            core.watched_roots(),
            roots(&["/p/errored", "/p/unavailable"])
        );
        assert_eq!(*fakes.closed.lock().unwrap(), roots(&["/p/errored"]));
    }

    #[test]
    fn dispose_closes_every_watch_and_stops_signals() {
        let fakes = Fakes::default();
        let mut attach = fakes.attach();
        let t0 = Instant::now();
        let mut core = core(t0);
        core.set_roots(&roots(&["/p/repo"]), &mut attach);
        core.on_event(0, None, t0);
        core.dispose();
        assert_eq!(*fakes.closed.lock().unwrap(), roots(&["/p/repo"]));
        assert!(core.tick(t0 + DEBOUNCE, &mut attach).is_empty());
    }

    #[test]
    fn path_equivalence_handles_posix_and_windows_shapes() {
        let eq = |a: &str, b: &str| are_equivalent_paths(Path::new(a), Path::new(b));
        assert!(eq("/a/b/", "/a/./b"));
        assert!(eq("/a/b/../c", "/a/c"));
        assert!(!eq("/a/B", "/a/b"));
        assert!(eq("C:\\Repo\\App", "c:/repo/app/"));
        assert!(eq("\\\\?\\C:\\Repo", "c:\\repo"));
        assert!(!eq("/a/b", "/a/c"));
    }
}
