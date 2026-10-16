//! Generic whole-array JSON registry (`FileBackedRegistry` in `workspace-registry.ts`).
//!
//! Mutation protocol: take the queue, stage a copy of the cache, run the updater, write the
//! whole array if any record changed, swap the cache, then run `after_commit`. A failed write
//! leaves the cache untouched. Reads come from an immutable snapshot and never wait for the
//! queue.

use crate::error::{Result, StoreError};
use crate::js_json::write_js_json_atomic;
use indexmap::IndexMap;
use paseo_fs_atomic::FsyncPolicy;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tokio::sync::OnceCell;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
pub type BorrowedFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait RegistryRecord: Clone + Serialize + DeserializeOwned + Send + Sync + 'static {
    fn id(&self) -> &str;
}

pub type RecordMap<R> = IndexMap<String, R>;

/// Injectable writer; tests use it to fail or observe writes.
pub type WriteRecords<R> = Arc<dyn Fn(&Path, &[R]) -> std::io::Result<()> + Send + Sync>;

/// Result of an updater plus whether it changed the record set. TS decides this by object
/// identity (`mapsEqual`): any `set`/`delete` of a freshly parsed record is a change.
pub struct Staged<T> {
    pub result: T,
    pub changed: bool,
}

/// Hooks around the durable write of one mutation.
pub struct CommitHooks<'a, T, R> {
    pub force_persist: Option<&'a (dyn Fn(&T) -> bool + Send + Sync)>,
    pub before_write: Option<&'a (dyn Fn(Vec<R>) -> BorrowedFuture<'a, Result<()>> + Send + Sync)>,
    pub after_write: Option<&'a (dyn Fn() -> BorrowedFuture<'a, Result<()>> + Send + Sync)>,
    pub after_commit: Option<&'a (dyn Fn() + Send + Sync)>,
}

impl<T, R> Default for CommitHooks<'_, T, R> {
    fn default() -> Self {
        CommitHooks {
            force_persist: None,
            before_write: None,
            after_write: None,
            after_commit: None,
        }
    }
}

pub(crate) struct FileBackedRegistry<R: RegistryRecord> {
    file_path: PathBuf,
    component: &'static str,
    cache: RwLock<Arc<RecordMap<R>>>,
    loaded: OnceCell<()>,
    queue: tokio::sync::Mutex<()>,
    blocked: AtomicBool,
    write_records: WriteRecords<R>,
}

impl<R: RegistryRecord> FileBackedRegistry<R> {
    pub fn new(
        file_path: PathBuf,
        component: &'static str,
        fsync: FsyncPolicy,
        write_records: Option<WriteRecords<R>>,
    ) -> Self {
        let write_records = write_records.unwrap_or_else(|| {
            Arc::new(move |path: &Path, records: &[R]| write_js_json_atomic(path, &records, fsync))
        });
        FileBackedRegistry {
            file_path,
            component,
            cache: RwLock::new(Arc::new(IndexMap::new())),
            loaded: OnceCell::new(),
            queue: tokio::sync::Mutex::new(()),
            blocked: AtomicBool::new(false),
            write_records,
        }
    }

    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    pub async fn initialize(&self) {
        self.load().await;
    }

    pub async fn exists_on_disk(&self) -> bool {
        tokio::fs::metadata(&self.file_path).await.is_ok()
    }

    pub async fn list(&self) -> Vec<R> {
        self.load().await;
        self.snapshot().values().cloned().collect()
    }

    pub async fn get(&self, id: &str) -> Option<R> {
        self.load().await;
        self.snapshot().get(id).cloned()
    }

    fn snapshot(&self) -> Arc<RecordMap<R>> {
        self.cache.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    async fn load(&self) {
        self.loaded.get_or_init(|| self.do_load()).await;
    }

    async fn do_load(&self) {
        let mut next: RecordMap<R> = IndexMap::new();
        match tokio::fs::read_to_string(&self.file_path).await {
            Ok(raw) => match crate::js_json::from_js_json_str::<Vec<R>>(&raw) {
                Ok(records) => {
                    for record in records {
                        next.insert(record.id().to_string(), record);
                    }
                }
                Err(error) => {
                    // PARITY-QUIRK(corrupt-registry-truncate): TS logs, serves an empty
                    // registry and the next write replaces the unreadable file; the old
                    // bytes are not quarantined.
                    tracing::error!(
                        component = self.component,
                        file = %self.file_path.display(),
                        %error,
                        "Failed to load registry file"
                    );
                    tracing::warn!(
                        component = self.component,
                        file = %self.file_path.display(),
                        "Corrupt registry file will be overwritten by the next write"
                    );
                }
            },
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::error!(
                        component = self.component,
                        file = %self.file_path.display(),
                        %error,
                        "Failed to load registry file"
                    );
                }
            }
        }
        *self.cache.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(next);
    }

    pub fn freeze_mutations_until_restart(&self) {
        self.blocked.store(true, Ordering::SeqCst);
    }

    /// Core mutation. `updater` may fail; nothing is written then.
    pub async fn mutate_cache<T>(
        &self,
        updater: impl FnOnce(&mut RecordMap<R>) -> Result<Staged<T>> + Send,
        hooks: CommitHooks<'_, T, R>,
    ) -> Result<T> {
        let _queue = self.queue.lock().await;
        self.load().await;
        if self.blocked.load(Ordering::SeqCst) {
            return Err(StoreError::RegistryBlocked);
        }
        let current = self.snapshot();
        let mut staged: RecordMap<R> = (*current).clone();
        let Staged { result, changed } = updater(&mut staged)?;
        let force = hooks.force_persist.is_some_and(|f| f(&result));
        if !changed && !force {
            return Ok(result);
        }
        let records: Vec<R> = staged.values().cloned().collect();
        if let Some(before) = hooks.before_write {
            before(records.clone()).await?;
        }
        if changed {
            let (path, writer, to_write) =
                (self.file_path.clone(), self.write_records.clone(), records);
            tokio::task::spawn_blocking(move || writer(&path, &to_write))
                .await
                .map_err(|e| StoreError::Other(e.to_string()))??;
        }
        if let Some(after) = hooks.after_write {
            after().await?;
        }
        if changed {
            *self.cache.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(staged);
        }
        if let Some(commit) = hooks.after_commit {
            commit();
        }
        Ok(result)
    }

    pub async fn upsert(&self, record: R) -> Result<()> {
        self.mutate_cache(
            |records| {
                records.insert(record.id().to_string(), record);
                Ok(Staged {
                    result: (),
                    changed: true,
                })
            },
            CommitHooks::default(),
        )
        .await
    }

    pub async fn update(
        &self,
        id: &str,
        updater: impl FnOnce(&R) -> R + Send,
    ) -> Result<Option<R>> {
        self.mutate_cache(
            |records| {
                let Some(existing) = records.get(id) else {
                    return Ok(Staged {
                        result: None,
                        changed: false,
                    });
                };
                let next = updater(existing);
                records.insert(id.to_string(), next.clone());
                Ok(Staged {
                    result: Some(next),
                    changed: true,
                })
            },
            CommitHooks::default(),
        )
        .await
    }

    pub async fn mutate_many(
        &self,
        updater: impl FnOnce(&RecordMap<R>) -> Vec<R> + Send,
    ) -> Result<Vec<R>> {
        self.mutate_cache(
            |records| {
                let changed = updater(records);
                if changed.is_empty() {
                    return Ok(Staged {
                        result: changed,
                        changed: false,
                    });
                }
                for record in &changed {
                    records.insert(record.id().to_string(), record.clone());
                }
                Ok(Staged {
                    result: changed,
                    changed: true,
                })
            },
            CommitHooks::default(),
        )
        .await
    }

    pub async fn remove_if_present(&self, id: &str) -> Result<Option<R>> {
        self.mutate_cache(
            |records| {
                let removed = records.shift_remove(id);
                let changed = removed.is_some();
                Ok(Staged {
                    result: removed,
                    changed,
                })
            },
            CommitHooks::default(),
        )
        .await
    }
}

/// Listener bookkeeping shared by both registries.
pub(crate) struct Listeners<M> {
    next_id: Mutex<u64>,
    listeners: Arc<Mutex<ListenerList<M>>>,
}

type ListenerList<M> = Vec<(u64, MutationListener<M>)>;

pub type MutationListener<M> = Arc<dyn Fn(M) -> BoxFuture<Result<()>> + Send + Sync>;

/// Handle returned by `subscribe`; the listener stays registered until `unsubscribe`.
pub struct Subscription {
    remove: Box<dyn Fn() + Send + Sync>,
}

impl Subscription {
    pub fn unsubscribe(self) {
        (self.remove)();
    }
}

impl<M: Clone + Send + 'static> Listeners<M> {
    pub fn new() -> Self {
        Listeners {
            next_id: Mutex::new(0),
            listeners: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn subscribe(&self, listener: MutationListener<M>) -> Subscription {
        let id = {
            let mut next = self.next_id.lock().unwrap_or_else(|e| e.into_inner());
            *next += 1;
            *next
        };
        self.listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, listener));
        let listeners = self.listeners.clone();
        Subscription {
            remove: Box::new(move || {
                listeners
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|(i, _)| *i != id);
            }),
        }
    }

    /// Runs every listener (all of them, even after a failure) and returns each outcome.
    pub async fn notify(&self, mutation: M) -> Vec<Result<()>> {
        let snapshot: Vec<MutationListener<M>> = self
            .listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(_, l)| l.clone())
            .collect();
        let mut outcomes = Vec::with_capacity(snapshot.len());
        for listener in snapshot {
            outcomes.push(listener(mutation.clone()).await);
        }
        outcomes
    }
}
