use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::backend::Timings;
use crate::error::FileObserverError;
use crate::observation::{
    spawn, BackendKind, FileObserverCallback, ObservationHandle, ObservationInput,
};
use crate::paths::{resolve_path, ObserverPaths};
use crate::types::{FileChange, FileObserverDiagnostics, FileObserverOptions, SharedMetrics};
use crate::watcher::NotifySource;

/// Tunables for tests and benchmarks. `Default` matches the daemon.
#[derive(Debug, Clone, Copy)]
pub struct FileObserverConfig {
    pub backend: BackendKind,
    pub timings: Timings,
    pub paths: ObserverPaths,
}

impl Default for FileObserverConfig {
    fn default() -> Self {
        Self {
            backend: BackendKind::platform_default(),
            timings: Timings::default(),
            paths: ObserverPaths::host(),
        }
    }
}

enum State {
    Open {
        observations: HashMap<u64, Arc<ObservationHandle>>,
        next_id: u64,
    },
    Closed,
}

struct Inner {
    config: FileObserverConfig,
    metrics: Arc<SharedMetrics>,
    state: Mutex<State>,
}

/// Owner-level service: create one at the owning boundary, subscribe roots
/// through it, `close()` it during shutdown. Cheap to clone (shared state).
#[derive(Clone)]
pub struct FileObserver {
    inner: Arc<Inner>,
}

impl Default for FileObserver {
    fn default() -> Self {
        Self::new()
    }
}

impl FileObserver {
    pub fn new() -> Self {
        Self::with_config(FileObserverConfig::default())
    }

    pub fn with_config(config: FileObserverConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                metrics: Arc::new(SharedMetrics::default()),
                state: Mutex::new(State::Open {
                    observations: HashMap::new(),
                    next_id: 0,
                }),
            }),
        }
    }

    /// Starts observing `directory` recursively. Resolves after the initial
    /// scan, so a change made after it returns is not missed. The callback runs
    /// on the observation thread: `Ok(batch)` for coalesced changes, `Err` once
    /// when the observation fails (permission, resource limit), after which the
    /// subscription is dead and the owner should fall back to polling.
    pub async fn subscribe<F>(
        &self,
        directory: impl AsRef<Path>,
        callback: F,
        options: FileObserverOptions,
    ) -> Result<FileObserverSubscription, FileObserverError>
    where
        F: Fn(Result<Vec<FileChange>, FileObserverError>) + Send + Sync + 'static,
    {
        let root = resolve_path(directory.as_ref());
        let callback: FileObserverCallback = Arc::new(callback);
        let inner = &self.inner;
        let (id, on_closed_inner) = {
            let mut state = inner.state.lock().unwrap();
            match &mut *state {
                State::Closed => return Err(FileObserverError::Closed),
                State::Open { next_id, .. } => {
                    let id = *next_id;
                    *next_id += 1;
                    (id, inner.clone())
                }
            }
        };
        let spawned = spawn(ObservationInput {
            root: root.clone(),
            options,
            callback,
            paths: inner.config.paths,
            metrics: inner.metrics.clone(),
            kind: inner.config.backend,
            timings: inner.config.timings,
            source_factory: NotifySource::factory(root),
            on_closed: Box::new(move || {
                if let State::Open { observations, .. } =
                    &mut *on_closed_inner.state.lock().unwrap()
                {
                    observations.remove(&id);
                }
            }),
        })?;
        let handle = spawned.handle;
        // A cancelled subscribe() must not leave an ownerless observation.
        let mut guard = CloseOnDrop(Some(handle.clone()));
        let closed = {
            let mut state = inner.state.lock().unwrap();
            match &mut *state {
                State::Open { observations, .. } => {
                    observations.insert(id, handle.clone());
                    false
                }
                State::Closed => true,
            }
        };
        if closed {
            handle.close().await;
            return Err(FileObserverError::Closed);
        }
        let started = spawned
            .started
            .await
            .unwrap_or(Err(FileObserverError::Closed));
        if let Err(error) = started {
            if let State::Open { observations, .. } = &mut *inner.state.lock().unwrap() {
                observations.remove(&id);
            }
            return Err(error);
        }
        // The service may have closed while the initial scan ran.
        if matches!(*inner.state.lock().unwrap(), State::Closed) {
            handle.close().await;
            return Err(FileObserverError::Closed);
        }
        guard.0 = None;
        Ok(FileObserverSubscription { handle })
    }

    pub fn get_diagnostics(&self) -> FileObserverDiagnostics {
        let mut out = FileObserverDiagnostics::default();
        let state = self.inner.state.lock().unwrap();
        let State::Open { observations, .. } = &*state else {
            return out;
        };
        out.active_observation_count = observations.len() as u64;
        for handle in observations.values() {
            let diag = *handle.diagnostics.lock().unwrap();
            out.native_handle_count += diag.backend.native_handle_count;
            out.native_tracked_file_count += diag.backend.native_tracked_file_count;
            out.pending_event_count += diag.pending_event_count;
            out.pending_reconciliation_work_count += diag.backend.pending_reconciliation_work_count;
            out.pending_classification_count += diag.backend.pending_classification_count;
            out.reconciliation_in_flight_count += u64::from(diag.backend.reconciliation_in_flight);
        }
        self.inner.metrics.fill(&mut out);
        out
    }

    /// Releases every subscription and rejects further `subscribe` calls.
    pub async fn close(&self) {
        let handles: Vec<Arc<ObservationHandle>> = {
            let mut state = self.inner.state.lock().unwrap();
            match std::mem::replace(&mut *state, State::Closed) {
                State::Open { observations, .. } => observations.into_values().collect(),
                State::Closed => Vec::new(),
            }
        };
        for handle in &handles {
            let _ = handle.tx.send(crate::observation::Msg::Close);
        }
        for handle in handles {
            handle.close().await;
        }
    }
}

/// Sends `Close` on drop until disarmed by setting the inner handle to `None`.
struct CloseOnDrop(Option<Arc<ObservationHandle>>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            let _ = handle.tx.send(crate::observation::Msg::Close);
        }
    }
}

pub struct FileObserverSubscription {
    handle: Arc<ObservationHandle>,
}

impl FileObserverSubscription {
    /// Replaces the excluded subtrees. Barrier: once it resolves, nothing
    /// queued below a newly excluded root is delivered.
    pub async fn update_ignore(&self, paths: Vec<PathBuf>) -> Result<(), FileObserverError> {
        self.handle.update_ignore(paths).await
    }

    /// Barrier and idempotent: once it resolves no scan, queued event, native
    /// handle or callback remains. Safe after the root was renamed or removed.
    pub async fn unsubscribe(&self) {
        self.handle.close().await;
    }
}

impl Drop for FileObserverSubscription {
    fn drop(&mut self) {
        let _ = self.handle.tx.send(crate::observation::Msg::Close);
    }
}
