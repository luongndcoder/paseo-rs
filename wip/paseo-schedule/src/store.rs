//! File-backed schedule store: one `<id>.json` per schedule under `PASEO_HOME/schedules`.
//! Port of `server/schedule/store.ts`.
//!
//! Concurrency: mutations of one schedule are serialized by a per-id lock held
//! while the updater runs, so read-modify-write cycles never lose writes. Name+target
//! upserts additionally serialize on an identity lock.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::atomic::write_json_file_atomic;
use crate::error::{Result, ScheduleError};
use crate::logger::ScheduleLogger;
use crate::types::{ScheduleStatus, ScheduleTarget, StoredSchedule};

/// Updater result: `Ok(None)` keeps the stored schedule as is (no write).
pub type UpdateResult = Result<Option<StoredSchedule>>;

type LockMap = Mutex<HashMap<String, Arc<Mutex<()>>>>;

pub struct ScheduleStore {
    dir: PathBuf,
    logger: Arc<dyn ScheduleLogger>,
    schedule_locks: LockMap,
    identity_locks: LockMap,
    reported_invalid: Mutex<HashSet<PathBuf>>,
}

fn generate_schedule_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

fn lock_for(map: &LockMap, key: &str) -> Arc<Mutex<()>> {
    let mut guard = map.lock().unwrap_or_else(|p| p.into_inner());
    guard
        .entry(key.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        Value::Object(map) => {
            let sorted: BTreeMap<&String, Value> =
                map.iter().map(|(k, v)| (k, canonicalize(v))).collect();
            let mut out = serde_json::Map::new();
            for (k, v) in sorted {
                out.insert(k.clone(), v);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn normalize_schedule_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ScheduleError::msg("Schedule name is required"));
    }
    Ok(trimmed.to_string())
}

fn normalize_optional_schedule_name(name: Option<&str>) -> Option<String> {
    let trimmed = name?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn target_identity(target: &ScheduleTarget) -> Value {
    // The enum already serializes to {type, agentId} / {type, config}.
    serde_json::to_value(target).unwrap_or(Value::Null)
}

fn name_target_identity_key(name: &str, target: &ScheduleTarget) -> Result<String> {
    let value = serde_json::json!({
        "name": normalize_schedule_name(name)?,
        "target": target_identity(target),
    });
    Ok(canonicalize(&value).to_string())
}

fn matches_name_and_target(
    schedule: &StoredSchedule,
    name: &str,
    target: &ScheduleTarget,
) -> Result<bool> {
    let schedule_name = match normalize_optional_schedule_name(schedule.name.as_deref()) {
        Some(n) => n,
        None => return Ok(false),
    };
    if schedule.status == ScheduleStatus::Completed
        || schedule_name != normalize_schedule_name(name)?
    {
        return Ok(false);
    }
    Ok(name_target_identity_key(&schedule_name, &schedule.target)?
        == name_target_identity_key(name, target)?)
}

fn is_safe_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\', '\0'])
}

impl ScheduleStore {
    pub fn new(dir: impl Into<PathBuf>, logger: Arc<dyn ScheduleLogger>) -> Self {
        ScheduleStore {
            dir: dir.into(),
            logger,
            schedule_locks: Mutex::new(HashMap::new()),
            identity_locks: Mutex::new(HashMap::new()),
            reported_invalid: Mutex::new(HashSet::new()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn ensure_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.dir)?;
        Ok(())
    }

    /// Lists valid schedules ordered by `createdAt`. Files that are not valid
    /// schedules are skipped; each is reported once while it stays invalid.
    pub fn list(&self) -> Result<Vec<StoredSchedule>> {
        self.ensure_dir()?;
        let mut entries: Vec<PathBuf> = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_file() && name.ends_with(".json") {
                entries.push(entry.path());
            }
        }
        let mut schedules = Vec::new();
        let mut invalid: HashSet<PathBuf> = HashSet::new();
        let mut reports: Vec<(PathBuf, String)> = Vec::new();
        {
            let reported = self
                .reported_invalid
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            for path in entries {
                let content = fs::read_to_string(&path)?;
                match StoredSchedule::parse(&content) {
                    Ok(schedule) => schedules.push(schedule),
                    Err(error) => {
                        if !reported.contains(&path) {
                            reports.push((path.clone(), error));
                        }
                        invalid.insert(path);
                    }
                }
            }
        }
        for (path, error) in reports {
            self.logger.error(&format!(
                "Skipping invalid schedule file {}: {}",
                path.display(),
                error
            ));
        }
        *self
            .reported_invalid
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = invalid;
        schedules.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(schedules)
    }

    pub fn get(&self, id: &str) -> Result<Option<StoredSchedule>> {
        self.ensure_dir()?;
        if !is_safe_id(id) {
            return Ok(None);
        }
        match fs::read_to_string(self.file_path(id)) {
            Ok(content) => StoredSchedule::parse(&content)
                .map(Some)
                .map_err(ScheduleError::Message),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Persists a new schedule under a fresh 8-hex-char id (`schedule.id` is ignored).
    pub fn create(&self, mut schedule: StoredSchedule) -> Result<StoredSchedule> {
        schedule.id = generate_schedule_id();
        self.validate(&mut schedule)?;
        self.write(&schedule)?;
        Ok(schedule)
    }

    pub fn update<F>(&self, id: &str, updater: F) -> UpdateResult
    where
        F: FnOnce(&StoredSchedule) -> UpdateResult,
    {
        let lock = lock_for(&self.schedule_locks, id);
        let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());
        let current = match self.get(id)? {
            Some(c) => c,
            None => return Ok(None),
        };
        let next = match updater(&current)? {
            Some(n) => n,
            None => return Ok(Some(current)),
        };
        if next.id != id {
            return Err(ScheduleError::Message(format!(
                "Schedule update cannot change id: {id}"
            )));
        }
        let mut updated = next;
        self.validate(&mut updated)?;
        self.write(&updated)?;
        Ok(Some(updated))
    }

    /// Finds a non-completed schedule with the same name and target and updates it,
    /// otherwise creates one. Concurrent calls with one identity share a schedule.
    pub fn upsert_by_name_and_target<C, U>(
        &self,
        name: &str,
        target: &ScheduleTarget,
        create: C,
        update: U,
    ) -> Result<StoredSchedule>
    where
        C: Fn() -> Result<StoredSchedule>,
        U: Fn(&StoredSchedule) -> Result<StoredSchedule>,
    {
        let identity = name_target_identity_key(name, target)?;
        let lock = lock_for(&self.identity_locks, &identity);
        let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            let mut existing = None;
            for schedule in self.list()? {
                if matches_name_and_target(&schedule, name, target)? {
                    existing = Some(schedule);
                    break;
                }
            }
            match existing {
                None => {
                    let mut created = create()?;
                    created.id = generate_schedule_id();
                    self.validate(&mut created)?;
                    if !matches_name_and_target(&created, name, target)? {
                        return Err(ScheduleError::msg(
                            "Created schedule does not match requested identity",
                        ));
                    }
                    self.write(&created)?;
                    return Ok(created);
                }
                Some(existing) => {
                    if let Some(updated) =
                        self.update_matched_schedule(&existing.id, name, target, &update)?
                    {
                        return Ok(updated);
                    }
                }
            }
        }
    }

    fn update_matched_schedule<U>(
        &self,
        id: &str,
        name: &str,
        target: &ScheduleTarget,
        update: &U,
    ) -> Result<Option<StoredSchedule>>
    where
        U: Fn(&StoredSchedule) -> Result<StoredSchedule>,
    {
        let lock = lock_for(&self.schedule_locks, id);
        let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());
        let current = match self.get(id)? {
            Some(c) if matches_name_and_target(&c, name, target)? => c,
            _ => return Ok(None),
        };
        let mut updated = update(&current)?;
        if updated.id != id {
            return Err(ScheduleError::Message(format!(
                "Schedule update cannot change id: {id}"
            )));
        }
        self.validate(&mut updated)?;
        if !matches_name_and_target(&updated, name, target)? {
            return Err(ScheduleError::msg(
                "Updated schedule does not match requested identity",
            ));
        }
        self.write(&updated)?;
        Ok(Some(updated))
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let lock = lock_for(&self.schedule_locks, id);
        let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());
        self.ensure_dir()?;
        if !is_safe_id(id) {
            return Ok(());
        }
        match fs::remove_file(self.file_path(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn validate(&self, schedule: &mut StoredSchedule) -> Result<()> {
        schedule.normalize().map_err(ScheduleError::Message)
    }

    fn write(&self, schedule: &StoredSchedule) -> Result<()> {
        self.ensure_dir()?;
        write_json_file_atomic(&self.file_path(&schedule.id), schedule)
    }
}
