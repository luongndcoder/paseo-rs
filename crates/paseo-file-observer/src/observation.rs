//! One observation = one dedicated thread owning the `Host` and a backend.
//! Everything (native events, timers, ignore updates, close) is serialized
//! through one channel, so `update_ignore` and `unsubscribe` are real barriers:
//! once their acknowledgement is sent, no scan, queued event or callback is
//! left, and the native handles are already dropped.

use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::{oneshot, watch};

use crate::backend::{Backend, EventSink, Incoming, SourceFactory, Timings};
use crate::error::FileObserverError;
use crate::host::Host;
use crate::linux::LinuxBackend;
use crate::native_recursive::NativeRecursiveBackend;
use crate::paths::ObserverPaths;
use crate::types::{BackendDiagnostics, FileChange, FileObserverOptions, SharedMetrics};

pub type FileObserverCallback =
    Arc<dyn Fn(Result<Vec<FileChange>, FileObserverError>) + Send + Sync + 'static>;

/// Which backend watches the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// One non-recursive watch per directory (Linux / inotify).
    PerDirectory,
    /// One recursive native watcher plus inventory (macOS / Windows).
    NativeRecursive,
}

impl BackendKind {
    pub fn platform_default() -> Self {
        if cfg!(any(target_os = "macos", windows)) {
            Self::NativeRecursive
        } else {
            Self::PerDirectory
        }
    }
}

pub(crate) enum Msg {
    Incoming(Incoming),
    UpdateIgnore(Vec<PathBuf>, oneshot::Sender<Result<(), FileObserverError>>),
    Close,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ObservationDiagnostics {
    pub backend: BackendDiagnostics,
    pub pending_event_count: u64,
}

pub(crate) struct ObservationHandle {
    pub tx: mpsc::Sender<Msg>,
    pub closed: watch::Receiver<bool>,
    pub diagnostics: Arc<Mutex<ObservationDiagnostics>>,
}

impl ObservationHandle {
    pub async fn close(&self) {
        let _ = self.tx.send(Msg::Close);
        let mut closed = self.closed.clone();
        let _ = closed.wait_for(|done| *done).await;
    }

    pub async fn update_ignore(&self, paths: Vec<PathBuf>) -> Result<(), FileObserverError> {
        let (ack, done) = oneshot::channel();
        if self.tx.send(Msg::UpdateIgnore(paths, ack)).is_err() {
            return Ok(());
        }
        // A dropped acknowledgement means the observation ended: nothing to do.
        done.await.unwrap_or(Ok(()))
    }
}

pub(crate) struct ObservationInput {
    pub root: PathBuf,
    pub options: FileObserverOptions,
    pub callback: FileObserverCallback,
    pub paths: ObserverPaths,
    pub metrics: Arc<SharedMetrics>,
    pub kind: BackendKind,
    pub timings: Timings,
    pub source_factory: SourceFactory,
    pub on_closed: Box<dyn FnOnce() + Send>,
}

pub(crate) struct Spawned {
    pub handle: Arc<ObservationHandle>,
    pub started: oneshot::Receiver<Result<(), FileObserverError>>,
}

pub(crate) fn spawn(input: ObservationInput) -> Result<Spawned, FileObserverError> {
    let (tx, rx) = mpsc::channel::<Msg>();
    let (closed_tx, closed_rx) = watch::channel(false);
    let (started_tx, started_rx) = oneshot::channel();
    let diagnostics = Arc::new(Mutex::new(ObservationDiagnostics::default()));
    let handle = Arc::new(ObservationHandle {
        tx: tx.clone(),
        closed: closed_rx,
        diagnostics: diagnostics.clone(),
    });
    let thread_diag = diagnostics;
    std::thread::Builder::new()
        .name("paseo-file-observer".into())
        .spawn(move || run(input, tx, rx, closed_tx, started_tx, thread_diag))
        .map_err(FileObserverError::from)?;
    Ok(Spawned {
        handle,
        started: started_rx,
    })
}

fn make_backend(
    kind: BackendKind,
    source: Box<dyn crate::backend::WatchSource>,
    timings: Timings,
) -> Box<dyn Backend> {
    match kind {
        BackendKind::PerDirectory => Box::new(LinuxBackend::new(source, timings)),
        BackendKind::NativeRecursive => Box::new(NativeRecursiveBackend::new(source, timings)),
    }
}

fn run(
    input: ObservationInput,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    closed_tx: watch::Sender<bool>,
    started_tx: oneshot::Sender<Result<(), FileObserverError>>,
    diagnostics: Arc<Mutex<ObservationDiagnostics>>,
) {
    let ObservationInput {
        root,
        options,
        callback,
        paths,
        metrics,
        kind,
        timings,
        source_factory,
        on_closed,
    } = input;
    let finish = |on_closed: Box<dyn FnOnce() + Send>| {
        on_closed();
        *diagnostics.lock().unwrap() = ObservationDiagnostics::default();
        closed_tx.send_replace(true);
    };

    match std::fs::metadata(&root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            finish(on_closed);
            let _ = started_tx.send(Err(FileObserverError::NotADirectory(root)));
            return;
        }
        Err(error) => {
            finish(on_closed);
            let _ = started_tx.send(Err(error.into()));
            return;
        }
    }

    let sink: EventSink = {
        let tx = tx.clone();
        Arc::new(move |incoming| {
            let _ = tx.send(Msg::Incoming(incoming));
        })
    };
    drop(tx);
    let source = match source_factory(sink) {
        Ok(source) => source,
        Err(error) => {
            finish(on_closed);
            let _ = started_tx.send(Err(error));
            return;
        }
    };
    let mut backend = make_backend(kind, source, timings);
    let mut host = Host::new(
        root,
        &options.ignore,
        paths,
        metrics,
        timings.event_batch_delay,
    );

    let now = Instant::now();
    host.now = now;
    if let Err(error) = backend.start(&mut host, now) {
        backend.close();
        finish(on_closed);
        let _ = started_tx.send(Err(error));
        return;
    }
    publish(&diagnostics, &*backend, &host);
    let _ = started_tx.send(Ok(()));

    'outer: loop {
        let mut batch = Vec::new();
        if backend.has_ready_work() {
            if let Ok(msg) = rx.try_recv() {
                batch.push(msg);
            }
        } else {
            let wake = [host.flush_deadline(), backend.next_wake()]
                .into_iter()
                .flatten()
                .min();
            let received = match wake {
                Some(at) => rx.recv_timeout(at.saturating_duration_since(Instant::now())),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match received {
                Ok(msg) => batch.push(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => batch.push(Msg::Close),
            }
        }
        while batch.len() < 1024 {
            match rx.try_recv() {
                Ok(msg) => batch.push(msg),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }

        let now = Instant::now();
        host.now = now;
        for msg in batch {
            match msg {
                Msg::Incoming(Incoming::Event(event)) => backend.on_event(&mut host, event, now),
                Msg::Incoming(Incoming::Error(error)) => {
                    backend.on_watch_error(&mut host, error, now)
                }
                Msg::UpdateIgnore(paths, ack) => {
                    let result = update_ignore(&mut *backend, &mut host, &paths, now);
                    let _ = ack.send(result);
                }
                Msg::Close => {
                    host.close();
                    backend.close();
                    break 'outer;
                }
            }
        }
        backend.tick(&mut host, now);

        if let Some(error) = host.take_failure() {
            SharedMetrics::bump(&host.metrics.observer_failure_count);
            backend.close();
            callback(Err(error));
            break;
        }
        if let Some(events) = host.take_due_batch(Instant::now()) {
            callback(Ok(events));
        }
        publish(&diagnostics, &*backend, &host);
    }

    finish(on_closed);
    // Answer barriers that raced the shutdown.
    while let Ok(msg) = rx.try_recv() {
        if let Msg::UpdateIgnore(_, ack) = msg {
            let _ = ack.send(Ok(()));
        }
    }
}

fn update_ignore(
    backend: &mut dyn Backend,
    host: &mut Host,
    paths: &[PathBuf],
    now: Instant,
) -> Result<(), FileObserverError> {
    if !host.is_active() || !host.replace_ignored(paths) {
        return Ok(());
    }
    backend.update_ignore(host, now).inspect_err(|error| {
        host.fail(error.clone());
    })
}

fn publish(slot: &Mutex<ObservationDiagnostics>, backend: &dyn Backend, host: &Host) {
    *slot.lock().unwrap() = ObservationDiagnostics {
        backend: backend.diagnostics(),
        pending_event_count: host.pending_len() as u64,
    };
}
