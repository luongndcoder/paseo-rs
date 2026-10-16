//! In-memory change journal for the workspace label catalog (`workspace-labels/internal/sequence.ts`).
//! Nothing here is persisted: the generation changes on every process start, which forces
//! clients back to a snapshot.

use crate::labels::LabelDefinition;
use indexmap::IndexMap;
use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq)]
pub enum WorkspaceLabelChange {
    Upsert {
        label: LabelDefinition,
        previous_name: Option<String>,
    },
    Remove {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SequencedChange {
    pub generation: String,
    pub seq: u64,
    pub change: WorkspaceLabelChange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceLabelCursor {
    pub generation: String,
    pub after_seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    Snapshot,
    Changes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelRemoval {
    pub name: String,
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceLabelSyncMeta {
    pub mode: SyncMode,
    pub generation: String,
    pub head_seq: u64,
    pub removals: Vec<LabelRemoval>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceLabelSync {
    pub labels: Vec<LabelDefinition>,
    pub sync: WorkspaceLabelSyncMeta,
}

pub type ChangeListener = Arc<dyn Fn(&SequencedChange) + Send + Sync>;

struct Inner {
    head_seq: u64,
    journal: VecDeque<SequencedChange>,
    subscribers: Vec<(u64, ChangeListener)>,
    next_subscriber: u64,
}

pub struct WorkspaceLabelSequence {
    generation: String,
    journal_limit: usize,
    inner: Arc<Mutex<Inner>>,
}

/// Dropping the handle does not unsubscribe; call [`LabelSubscription::unsubscribe`].
pub struct LabelSubscription {
    inner: Arc<Mutex<Inner>>,
    id: u64,
}

impl LabelSubscription {
    pub fn unsubscribe(self) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subscribers
            .retain(|(id, _)| *id != self.id);
    }
}

impl Default for WorkspaceLabelSequence {
    fn default() -> Self {
        Self::new(256)
    }
}

impl WorkspaceLabelSequence {
    pub fn new(journal_limit: usize) -> Self {
        WorkspaceLabelSequence {
            generation: uuid::Uuid::new_v4().to_string(),
            journal_limit,
            inner: Arc::new(Mutex::new(Inner {
                head_seq: 0,
                journal: VecDeque::new(),
                subscribers: Vec::new(),
                next_subscriber: 0,
            })),
        }
    }

    pub fn generation(&self) -> &str {
        &self.generation
    }

    pub fn subscribe(&self, listener: ChangeListener) -> LabelSubscription {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.next_subscriber += 1;
        let id = inner.next_subscriber;
        inner.subscribers.push((id, listener));
        LabelSubscription {
            inner: self.inner.clone(),
            id,
        }
    }

    pub fn publish(&self, change: WorkspaceLabelChange) {
        let (entry, subscribers) = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.head_seq += 1;
            let entry = SequencedChange {
                generation: self.generation.clone(),
                seq: inner.head_seq,
                change,
            };
            inner.journal.push_back(entry.clone());
            if inner.journal.len() > self.journal_limit {
                inner.journal.pop_front();
            }
            let subscribers: Vec<ChangeListener> =
                inner.subscribers.iter().map(|(_, l)| l.clone()).collect();
            (entry, subscribers)
        };
        for subscriber in subscribers {
            // Publication follows durable persistence and cannot fail the mutation.
            let _ = catch_unwind(AssertUnwindSafe(|| subscriber(&entry)));
        }
    }

    pub fn synchronize(
        &self,
        catalog: &[LabelDefinition],
        cursor: Option<&WorkspaceLabelCursor>,
    ) -> WorkspaceLabelSync {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let head_seq = inner.head_seq;
        let oldest_seq = inner.journal.front().map_or(head_seq + 1, |e| e.seq);
        let can_catch_up = cursor.is_some_and(|c| {
            c.generation == self.generation
                && c.after_seq <= head_seq
                && c.after_seq + 1 >= oldest_seq
        });
        let Some(cursor) = cursor.filter(|_| can_catch_up) else {
            return WorkspaceLabelSync {
                labels: catalog.to_vec(),
                sync: WorkspaceLabelSyncMeta {
                    mode: SyncMode::Snapshot,
                    generation: self.generation.clone(),
                    head_seq,
                    removals: Vec::new(),
                },
            };
        };

        let mut upserts: IndexMap<String, LabelDefinition> = IndexMap::new();
        let mut removals: IndexMap<String, LabelRemoval> = IndexMap::new();
        for entry in inner.journal.iter().filter(|e| e.seq > cursor.after_seq) {
            match &entry.change {
                WorkspaceLabelChange::Remove { name } => {
                    let key = name.to_lowercase();
                    let created_during_catch_up = upserts.shift_remove(&key).is_some();
                    if !created_during_catch_up {
                        removals.insert(
                            key,
                            LabelRemoval {
                                name: name.clone(),
                                seq: entry.seq,
                            },
                        );
                    }
                }
                WorkspaceLabelChange::Upsert {
                    label,
                    previous_name,
                } => {
                    if let Some(previous) = previous_name.as_ref().filter(|p| !p.is_empty()) {
                        let previous_key = previous.to_lowercase();
                        let created_during_catch_up = upserts.shift_remove(&previous_key).is_some();
                        if !created_during_catch_up {
                            removals.insert(
                                previous_key,
                                LabelRemoval {
                                    name: previous.clone(),
                                    seq: entry.seq,
                                },
                            );
                        }
                    }
                    let key = label.name.to_lowercase();
                    removals.shift_remove(&key);
                    upserts.insert(key, label.clone());
                }
            }
        }
        WorkspaceLabelSync {
            labels: upserts.into_values().collect(),
            sync: WorkspaceLabelSyncMeta {
                mode: SyncMode::Changes,
                generation: self.generation.clone(),
                head_seq,
                removals: removals.into_values().collect(),
            },
        }
    }
}
