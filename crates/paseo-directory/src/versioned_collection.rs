//! Port of `directory-sync/internal/versioned-collection.ts`.
//!
//! Latest-state sequencing for one entity type. Sequence numbers start at 1 and advance only when
//! the fingerprint of a value changes or an existing value is removed.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

pub const DEFAULT_MAX_TOMBSTONES: usize = 5_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionedValue<T> {
    pub seq: u64,
    pub value: T,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionedRemoval {
    pub id: String,
    pub seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    Changes,
    Snapshot,
}

impl ReadMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ReadMode::Changes => "changes",
            ReadMode::Snapshot => "snapshot",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadReason {
    NoCursor,
    GenerationChanged,
    CursorExpired,
}

impl ReadReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ReadReason::NoCursor => "no_cursor",
            ReadReason::GenerationChanged => "generation_changed",
            ReadReason::CursorExpired => "cursor_expired",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionRead<T> {
    pub mode: ReadMode,
    pub reason: Option<ReadReason>,
    pub head_seq: u64,
    pub values: Vec<VersionedValue<T>>,
    pub removals: Vec<VersionedRemoval>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishedChange<T> {
    Upsert { seq: u64, value: T },
    Remove { seq: u64, id: String },
}

struct Stored<T> {
    seq: u64,
    /// Position in JS `Map` insertion order; `set` on an existing key keeps it.
    ordinal: u64,
    fingerprint: String,
    value: T,
}

type IdFn<T> = Box<dyn Fn(&T) -> String + Send + Sync>;
type FingerprintFn<T> = Box<dyn Fn(&T) -> String + Send + Sync>;

/// Owns latest-state sequencing for one directory entity type.
pub struct VersionedCollection<T> {
    values: HashMap<String, Stored<T>>,
    /// ordinal -> id, iteration order of the TS `valuesById` map.
    value_order: BTreeMap<u64, String>,
    tombstones: HashMap<String, u64>,
    /// tombstone seq -> id. Seqs are unique and monotonic, so this is the TS map's insertion order.
    tombstone_order: BTreeMap<u64, String>,
    get_id: IdFn<T>,
    fingerprint: FingerprintFn<T>,
    max_tombstones: usize,
    expired_through_seq: u64,
    next_seq: u64,
    next_ordinal: u64,
}

impl<T: Serialize + Clone> VersionedCollection<T> {
    /// Fingerprint is the compact JSON serialization (equal to `JSON.stringify` for equal inputs).
    pub fn new(get_id: impl Fn(&T) -> String + Send + Sync + 'static) -> Self {
        Self::with_options(get_id, None, DEFAULT_MAX_TOMBSTONES)
    }

    pub fn with_options(
        get_id: impl Fn(&T) -> String + Send + Sync + 'static,
        fingerprint: Option<FingerprintFn<T>>,
        max_tombstones: usize,
    ) -> Self {
        let fingerprint: FingerprintFn<T> = fingerprint.unwrap_or_else(|| {
            Box::new(|value: &T| serde_json::to_string(value).unwrap_or_default())
        });
        Self {
            values: HashMap::new(),
            value_order: BTreeMap::new(),
            tombstones: HashMap::new(),
            tombstone_order: BTreeMap::new(),
            get_id: Box::new(get_id),
            fingerprint,
            max_tombstones,
            expired_through_seq: 0,
            next_seq: 0,
            next_ordinal: 0,
        }
    }

    pub fn head_seq(&self) -> u64 {
        self.next_seq
    }

    pub fn sequence_for(&self, id: &str) -> Option<u64> {
        self.values
            .get(id)
            .map(|stored| stored.seq)
            .or_else(|| self.tombstones.get(id).copied())
    }

    pub fn replace(&mut self, value: T) -> Option<PublishedChange<T>> {
        let id = (self.get_id)(&value);
        let fingerprint = (self.fingerprint)(&value);
        if self
            .values
            .get(&id)
            .is_some_and(|previous| previous.fingerprint == fingerprint)
        {
            return None;
        }
        let seq = self.advance();
        let ordinal = match self.values.get(&id) {
            Some(previous) => previous.ordinal,
            None => {
                let ordinal = self.next_ordinal;
                self.next_ordinal += 1;
                self.value_order.insert(ordinal, id.clone());
                ordinal
            }
        };
        self.values.insert(
            id.clone(),
            Stored {
                seq,
                ordinal,
                fingerprint,
                value: value.clone(),
            },
        );
        if let Some(old) = self.tombstones.remove(&id) {
            self.tombstone_order.remove(&old);
        }
        Some(PublishedChange::Upsert { seq, value })
    }

    pub fn remove(&mut self, id: &str) -> Option<PublishedChange<T>> {
        // TS: a missing value (with or without a tombstone) is a no-op.
        let stored = self.values.remove(id)?;
        self.value_order.remove(&stored.ordinal);
        let seq = self.advance();
        if let Some(old) = self.tombstones.remove(id) {
            self.tombstone_order.remove(&old);
        }
        self.tombstones.insert(id.to_string(), seq);
        self.tombstone_order.insert(seq, id.to_string());
        self.trim_tombstones();
        Some(PublishedChange::Remove {
            seq,
            id: id.to_string(),
        })
    }

    pub fn replace_all(&mut self, values: impl IntoIterator<Item = T>) {
        let mut retained = std::collections::HashSet::new();
        for value in values {
            retained.insert((self.get_id)(&value));
            self.replace(value);
        }
        let stale: Vec<String> = self
            .value_order
            .values()
            .filter(|id| !retained.contains(*id))
            .cloned()
            .collect();
        for id in stale {
            self.remove(&id);
        }
    }

    pub fn read_snapshot(&self, reason: ReadReason) -> CollectionRead<T> {
        CollectionRead {
            mode: ReadMode::Snapshot,
            reason: Some(reason),
            head_seq: self.head_seq(),
            values: self.sorted_values(),
            removals: Vec::new(),
        }
    }

    /// `after_seq` is signed so a negative client cursor reads as expired, like the TS number compare.
    pub fn read_after(&self, after_seq: i64) -> CollectionRead<T> {
        let expired = after_seq < self.expired_through_seq as i64;
        let ahead = after_seq > self.head_seq() as i64;
        if expired || ahead {
            return self.read_snapshot(ReadReason::CursorExpired);
        }
        let after = after_seq as u64;
        // Tombstone seqs are unique, so ordering by seq equals TS `seq` then `localeCompare(id)`.
        let removals = self
            .tombstone_order
            .range(after + 1..)
            .map(|(seq, id)| VersionedRemoval {
                id: id.clone(),
                seq: *seq,
            })
            .collect();
        CollectionRead {
            mode: ReadMode::Changes,
            reason: None,
            head_seq: self.head_seq(),
            values: self
                .sorted_values()
                .into_iter()
                .filter(|entry| entry.seq > after)
                .collect(),
            removals,
        }
    }

    fn advance(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    // Seqs are unique per live value, so the TS `localeCompare(id)` tie-break never fires.
    fn sorted_values(&self) -> Vec<VersionedValue<T>> {
        let mut out: Vec<VersionedValue<T>> = self
            .values
            .values()
            .map(|stored| VersionedValue {
                seq: stored.seq,
                value: stored.value.clone(),
            })
            .collect();
        out.sort_by_key(|entry| entry.seq);
        out
    }

    fn trim_tombstones(&mut self) {
        while self.tombstones.len() > self.max_tombstones {
            let Some((seq, id)) = self.tombstone_order.pop_first() else {
                return;
            };
            self.tombstones.remove(&id);
            self.expired_through_seq = self.expired_through_seq.max(seq);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn coll() -> VersionedCollection<Value> {
        VersionedCollection::new(|v: &Value| v["id"].as_str().unwrap().to_string())
    }

    fn vals(read: &CollectionRead<Value>) -> Vec<(u64, Value)> {
        read.values
            .iter()
            .map(|v| (v.seq, v.value.clone()))
            .collect()
    }

    // TS: returns only each entity's latest visible snapshot after a sequence
    #[test]
    fn latest_snapshot_after_sequence() {
        let mut c = coll();
        c.replace(json!({"id":"one","title":"first"}));
        let cursor = c.head_seq();
        c.replace(json!({"id":"one","title":"second"}));
        c.replace(json!({"id":"one","title":"latest"}));
        c.replace(json!({"id":"two","title":"other"}));
        let read = c.read_after(cursor as i64);
        assert_eq!(read.mode, ReadMode::Changes);
        assert_eq!(read.reason, None);
        assert_eq!(read.head_seq, 4);
        assert_eq!(
            vals(&read),
            vec![
                (3, json!({"id":"one","title":"latest"})),
                (4, json!({"id":"two","title":"other"}))
            ]
        );
        assert!(read.removals.is_empty());
    }

    // TS: does not advance for an unchanged projection or repeated removal
    #[test]
    fn no_advance_for_unchanged_or_repeat_removal() {
        let mut c = coll();
        let v = json!({"id":"one","title":"same"});
        assert!(matches!(
            c.replace(v.clone()),
            Some(PublishedChange::Upsert { seq: 1, .. })
        ));
        assert_eq!(c.replace(v), None);
        assert!(matches!(
            c.remove("one"),
            Some(PublishedChange::Remove { seq: 2, .. })
        ));
        assert_eq!(c.remove("one"), None);
        assert_eq!(c.head_seq(), 2);
    }

    // TS: returns tombstones and lets a later value replace them
    #[test]
    fn tombstones_then_replaced() {
        let mut c = coll();
        c.replace(json!({"id":"one","title":"before"}));
        c.remove("one");
        let read = c.read_after(1);
        assert_eq!(read.mode, ReadMode::Changes);
        assert_eq!(read.head_seq, 2);
        assert!(read.values.is_empty());
        assert_eq!(
            read.removals,
            vec![VersionedRemoval {
                id: "one".into(),
                seq: 2
            }]
        );

        c.replace(json!({"id":"one","title":"after"}));
        let read = c.read_after(2);
        assert_eq!(read.head_seq, 3);
        assert_eq!(vals(&read), vec![(3, json!({"id":"one","title":"after"}))]);
        assert!(read.removals.is_empty());
        assert_eq!(c.sequence_for("one"), Some(3));
    }

    // TS: falls back to a latest snapshot when a required tombstone expired
    #[test]
    fn expired_tombstone_falls_back() {
        let mut c = VersionedCollection::with_options(
            |v: &Value| v["id"].as_str().unwrap().to_string(),
            None,
            1,
        );
        c.replace(json!({"id":"one"}));
        c.replace(json!({"id":"two"}));
        c.remove("one");
        c.remove("two");
        let read = c.read_after(2);
        assert_eq!(read.mode, ReadMode::Snapshot);
        assert_eq!(read.reason, Some(ReadReason::CursorExpired));
        assert_eq!(read.head_seq, 4);
        assert!(read.values.is_empty());
        assert!(read.removals.is_empty());
    }

    #[test]
    fn cursor_ahead_of_head_or_negative_expires() {
        let mut c = coll();
        c.replace(json!({"id":"one"}));
        assert_eq!(c.read_after(2).reason, Some(ReadReason::CursorExpired));
        assert_eq!(c.read_after(-1).reason, Some(ReadReason::CursorExpired));
        assert_eq!(c.read_after(1).mode, ReadMode::Changes);
    }

    #[test]
    fn replace_all_removes_stale_in_insertion_order() {
        let mut c = coll();
        c.replace_all([json!({"id":"a"}), json!({"id":"b"}), json!({"id":"c"})]);
        assert_eq!(c.head_seq(), 3);
        // b re-set keeps map position; a and c are stale and removed in map order (a, then c).
        c.replace_all([json!({"id":"b","x":1})]);
        assert_eq!(c.sequence_for("b"), Some(4));
        assert_eq!(c.sequence_for("a"), Some(5));
        assert_eq!(c.sequence_for("c"), Some(6));
    }

    #[test]
    fn tombstone_reremoval_moves_to_end_and_trim_advances_expiry() {
        let mut c = VersionedCollection::with_options(
            |v: &Value| v["id"].as_str().unwrap().to_string(),
            None,
            2,
        );
        for id in ["a", "b", "c"] {
            c.replace(json!({ "id": id }));
        }
        c.remove("a"); // seq 4
        c.remove("b"); // seq 5
        c.remove("c"); // seq 6 -> trims a (4)
        assert_eq!(c.sequence_for("a"), None);
        assert_eq!(c.read_after(3).reason, Some(ReadReason::CursorExpired));
        let read = c.read_after(4);
        assert_eq!(read.mode, ReadMode::Changes);
        assert_eq!(
            read.removals,
            vec![
                VersionedRemoval {
                    id: "b".into(),
                    seq: 5
                },
                VersionedRemoval {
                    id: "c".into(),
                    seq: 6
                }
            ]
        );
    }

    #[test]
    fn fingerprint_is_json_stringify_equivalent() {
        let mut c = coll();
        c.replace(json!({"id":"a","n":1}));
        // identical value: no bump
        assert!(c.replace(json!({"id":"a","n":1})).is_none());
        // key order matters (JSON.stringify is order-sensitive)
        assert!(c.replace(json!({"n":1,"id":"a"})).is_some());
    }
}
