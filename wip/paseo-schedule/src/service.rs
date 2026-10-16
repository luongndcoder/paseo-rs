//! Schedule orchestration: create/update/pause/resume, due-run claiming, run
//! bookkeeping, restart recovery. Port of `server/schedule/service.ts`.
//!
//! The service is synchronous and `Send + Sync`. [`ScheduleService::tick`] and
//! [`ScheduleService::run_once`] execute runs on the calling thread; the background
//! ticker dispatches each claimed run to its own worker thread (TS overlaps ticks
//! and only guards the same schedule id). No store lock is held while a run executes,
//! so a runner may call back into the service (pause, update, complete_for_agent).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::cron::{compute_next_run_at, validate_schedule_cadence};
use crate::error::{Result, ScheduleError};
use crate::host::{
    format_provider_model, format_system_notification_prompt, AgentTargetState,
    CreateScheduledAgent, ScheduleAgentHost,
};
use crate::logger::ScheduleLogger;
use crate::store::ScheduleStore;
use crate::time::{now_ms, parse_js_date, to_iso};
use crate::types::{
    CreateScheduleInput, Isolation, NewAgentConfig, RunStatus, ScheduleCadence,
    ScheduleExecutionResult, ScheduleRun, ScheduleStatus, ScheduleTarget, StoredSchedule,
    UpdateNewAgentConfig, UpdateScheduleInput,
};

pub const SCHEDULE_TICK_INTERVAL_MS: u64 = 1000;

/// Replaces the host-backed execution (used by tests and embedders).
pub type ScheduleRunner =
    Arc<dyn Fn(&StoredSchedule, &str) -> Result<ScheduleExecutionResult> + Send + Sync>;

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub struct ScheduleServiceOptions {
    pub paseo_home: PathBuf,
    pub logger: Arc<dyn ScheduleLogger>,
    pub host: Arc<dyn ScheduleAgentHost>,
    /// Epoch milliseconds. Defaults to the system clock.
    pub now: Option<Clock>,
    pub runner: Option<ScheduleRunner>,
}

pub struct ScheduleService {
    store: ScheduleStore,
    logger: Arc<dyn ScheduleLogger>,
    host: Arc<dyn ScheduleAgentHost>,
    now: Clock,
    runner: Option<ScheduleRunner>,
    running: Arc<Mutex<HashSet<String>>>,
}

struct RunningGuard {
    running: Arc<Mutex<HashSet<String>>>,
    id: String,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

fn advance_next_run_at_past(cadence: &ScheduleCadence, next_run_at: i64, now: i64) -> Result<i64> {
    let mut next = next_run_at;
    while next <= now {
        next = compute_next_run_at(cadence, next)?;
    }
    Ok(next)
}

fn trim_optional_name(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn build_schedule_fire_body(schedule: &StoredSchedule, run_id: &str) -> String {
    let heading = match schedule.name.as_deref() {
        Some(name) if !name.is_empty() => format!(
            "Schedule \"{name}\" fired (id={}, run={run_id}).",
            schedule.id
        ),
        _ => format!("Schedule fired (id={}, run={run_id}).", schedule.id),
    };
    format!("{heading}\n{}", schedule.prompt)
}

fn normalize_prompt(prompt: &str) -> Result<String> {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        return Err(ScheduleError::msg("Schedule prompt is required"));
    }
    Ok(trimmed.to_string())
}

fn apply_nullable(value: &Option<Option<String>>, slot: &mut Option<String>) {
    if let Some(patch) = value {
        let trimmed = patch.as_deref().map(str::trim).unwrap_or("");
        *slot = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }
}

pub fn apply_new_agent_config(
    config: &NewAgentConfig,
    patch: &UpdateNewAgentConfig,
) -> Result<NewAgentConfig> {
    let mut config = config.clone();
    if let Some(provider) = &patch.provider {
        let trimmed = provider.trim();
        if trimmed.is_empty() {
            return Err(ScheduleError::msg("provider cannot be empty"));
        }
        config.provider = trimmed.to_string();
    }
    if let Some(cwd) = &patch.cwd {
        let trimmed = cwd.trim();
        if trimmed.is_empty() {
            return Err(ScheduleError::msg("cwd cannot be empty"));
        }
        config.cwd = trimmed.to_string();
    }
    apply_nullable(&patch.model, &mut config.model);
    apply_nullable(&patch.mode_id, &mut config.mode_id);
    apply_nullable(&patch.thinking_option_id, &mut config.thinking_option_id);
    if let Some(v) = patch.archive_on_finish {
        config.archive_on_finish = Some(v);
    }
    if let Some(v) = patch.isolation {
        config.isolation = Some(v);
    }
    Ok(config)
}

fn normalize_max_runs(value: Option<i64>) -> Result<Option<u64>> {
    match value {
        None => Ok(None),
        Some(v) if v > 0 => Ok(Some(v as u64)),
        Some(_) => Err(ScheduleError::msg("maxRuns must be a positive integer")),
    }
}

fn count_completed_runs(schedule: &StoredSchedule) -> u64 {
    schedule
        .runs
        .iter()
        .filter(|r| r.status != RunStatus::Running)
        .count() as u64
}

/// New-agent runs archive their workspace unless the agent exists and
/// `archiveOnFinish` is explicitly false.
pub fn should_archive_schedule_run_workspace(
    agent_id: Option<&str>,
    archive_on_finish: Option<bool>,
) -> bool {
    agent_id.is_none() || archive_on_finish.unwrap_or(true)
}

pub fn should_complete_schedule(schedule: &StoredSchedule, now: i64) -> bool {
    if let Some(expires_at) = &schedule.expires_at {
        if let Some(expires) = parse_js_date(expires_at) {
            if expires <= now {
                return true;
            }
        }
    }
    match schedule.max_runs {
        None => false,
        Some(max) => count_completed_runs(schedule) >= max,
    }
}

fn require_schedule(schedule: Option<StoredSchedule>, id: &str) -> Result<StoredSchedule> {
    schedule.ok_or_else(|| ScheduleError::Message(format!("Schedule not found: {id}")))
}

fn complete_schedule(schedule: &StoredSchedule, now: i64) -> StoredSchedule {
    StoredSchedule {
        status: ScheduleStatus::Completed,
        next_run_at: None,
        paused_at: None,
        updated_at: to_iso(now),
        ..schedule.clone()
    }
}

fn merge_cadence_timezone(current: &ScheduleCadence, next: &ScheduleCadence) -> ScheduleCadence {
    if let (
        ScheduleCadence::Cron {
            timezone: Some(tz), ..
        },
        ScheduleCadence::Cron {
            expression,
            timezone: None,
        },
    ) = (current, next)
    {
        return ScheduleCadence::Cron {
            expression: expression.clone(),
            timezone: Some(tz.clone()),
        };
    }
    next.clone()
}

pub fn build_run_output(
    output: Option<&str>,
    timeline_text: &str,
    final_text: &str,
) -> Option<String> {
    if let Some(o) = output {
        if !o.trim().is_empty() {
            return Some(o.to_string());
        }
    }
    if !final_text.trim().is_empty() {
        return Some(final_text.trim().to_string());
    }
    if !timeline_text.trim().is_empty() {
        return Some(timeline_text.trim().to_string());
    }
    None
}

fn due(next_run_at: &str, now: i64) -> bool {
    parse_js_date(next_run_at).is_some_and(|t| t <= now)
}

// ---------------------------------------------------------------------------

impl ScheduleService {
    pub fn new(options: ScheduleServiceOptions) -> Self {
        ScheduleService {
            store: ScheduleStore::new(options.paseo_home.join("schedules"), options.logger.clone()),
            logger: options.logger,
            host: options.host,
            now: options.now.unwrap_or_else(|| Arc::new(now_ms)),
            runner: options.runner,
            running: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn store(&self) -> &ScheduleStore {
        &self.store
    }

    fn now(&self) -> i64 {
        (self.now)()
    }

    fn reserve(&self, id: &str) -> Option<RunningGuard> {
        let mut running = self.running.lock().unwrap_or_else(|p| p.into_inner());
        if running.insert(id.to_string()) {
            Some(RunningGuard {
                running: Arc::clone(&self.running),
                id: id.to_string(),
            })
        } else {
            None
        }
    }

    fn is_running(&self, id: &str) -> bool {
        self.running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(id)
    }

    /// Recover interrupted runs and sweep orphaned schedules. Call before ticking.
    pub fn start(&self) -> Result<()> {
        self.recover_interrupted_runs()?;
        self.sweep_orphaned_schedules()
    }

    /// Run [`Self::tick`] every second on a background thread until the handle is stopped.
    pub fn spawn_ticker(self: &Arc<Self>) -> TickerHandle {
        let (tx, rx) = mpsc::channel::<()>();
        let service = Arc::clone(self);
        let thread = std::thread::spawn(move || {
            while let Err(RecvTimeoutError::Timeout) =
                rx.recv_timeout(Duration::from_millis(SCHEDULE_TICK_INTERVAL_MS))
            {
                if let Err(error) = service.tick_dispatch() {
                    service
                        .logger
                        .error(&format!("Failed to process schedule tick: {error}"));
                }
            }
        });
        TickerHandle {
            stop: Some(tx),
            thread: Some(thread),
        }
    }

    // ----- create -----------------------------------------------------------

    pub fn create(&self, input: CreateScheduleInput) -> Result<StoredSchedule> {
        let prompt = normalize_prompt(&input.prompt)?;
        validate_schedule_cadence(&input.cadence)?;
        let name = trim_optional_name(input.name.as_deref());
        let record = self.build_schedule_record(&input, name, prompt, input.target.clone())?;
        self.store.create(record)
    }

    fn build_schedule_record(
        &self,
        input: &CreateScheduleInput,
        name: Option<String>,
        prompt: String,
        target: ScheduleTarget,
    ) -> Result<StoredSchedule> {
        let now = self.now();
        let run_on_create = input
            .run_on_create
            .unwrap_or(matches!(input.cadence, ScheduleCadence::Every { .. }));
        let next_run_at = if run_on_create {
            now
        } else {
            compute_next_run_at(&input.cadence, now)?
        };
        Ok(StoredSchedule {
            id: String::new(),
            name,
            prompt,
            cadence: input.cadence.clone(),
            target,
            status: ScheduleStatus::Active,
            created_at: to_iso(now),
            updated_at: to_iso(now),
            next_run_at: Some(to_iso(next_run_at)),
            last_run_at: None,
            paused_at: None,
            expires_at: input.expires_at.clone(),
            max_runs: normalize_max_runs(input.max_runs)?,
            runs: Vec::new(),
        })
    }

    /// Idempotent create: repeating a create with the same name and target refreshes
    /// the existing non-completed schedule in place instead of minting a duplicate.
    pub fn create_or_replace(&self, input: CreateScheduleInput) -> Result<StoredSchedule> {
        let name = trim_optional_name(input.name.as_deref());
        let prompt = normalize_prompt(&input.prompt)?;
        validate_schedule_cadence(&input.cadence)?;
        let name = match name {
            None => {
                let record =
                    self.build_schedule_record(&input, None, prompt, input.target.clone())?;
                return self.store.create(record);
            }
            Some(n) => n,
        };
        let target = input.target.clone();
        self.store.upsert_by_name_and_target(
            &name,
            &target,
            || {
                self.build_schedule_record(
                    &input,
                    Some(name.clone()),
                    prompt.clone(),
                    target.clone(),
                )
            },
            |current| {
                let now = self.now();
                let cadence = merge_cadence_timezone(&current.cadence, &input.cadence);
                let run_on_create = input
                    .run_on_create
                    .unwrap_or(matches!(cadence, ScheduleCadence::Every { .. }));
                let next_run_at = if run_on_create {
                    now
                } else {
                    compute_next_run_at(&cadence, now)?
                };
                Ok(StoredSchedule {
                    name: Some(name.clone()),
                    prompt: prompt.clone(),
                    cadence,
                    target: target.clone(),
                    status: ScheduleStatus::Active,
                    paused_at: None,
                    next_run_at: Some(to_iso(next_run_at)),
                    expires_at: input.expires_at.clone(),
                    max_runs: normalize_max_runs(input.max_runs)?,
                    updated_at: to_iso(now),
                    ..current.clone()
                })
            },
        )
    }

    // ----- read -------------------------------------------------------------

    pub fn list(&self) -> Result<Vec<StoredSchedule>> {
        self.store.list()
    }

    pub fn inspect(&self, id: &str) -> Result<StoredSchedule> {
        self.store
            .get(id)?
            .ok_or_else(|| ScheduleError::Message(format!("Schedule not found: {id}")))
    }

    /// Runs ordered by `startedAt`.
    pub fn logs(&self, id: &str) -> Result<Vec<ScheduleRun>> {
        let mut runs = self.inspect(id)?.runs;
        runs.sort_by(|a, b| a.started_at.cmp(&b.started_at));
        Ok(runs)
    }

    // ----- state changes ----------------------------------------------------

    pub fn pause(&self, id: &str) -> Result<StoredSchedule> {
        let paused = self.store.update(id, |schedule| {
            if schedule.status == ScheduleStatus::Completed {
                return Err(ScheduleError::Message(format!(
                    "Schedule {id} is already completed"
                )));
            }
            if schedule.status == ScheduleStatus::Paused {
                return Ok(None);
            }
            let now = to_iso(self.now());
            Ok(Some(StoredSchedule {
                status: ScheduleStatus::Paused,
                next_run_at: None,
                paused_at: Some(now.clone()),
                updated_at: now,
                ..schedule.clone()
            }))
        })?;
        require_schedule(paused, id)
    }

    pub fn resume(&self, id: &str) -> Result<StoredSchedule> {
        let resumed = self.store.update(id, |schedule| {
            if schedule.status == ScheduleStatus::Completed {
                return Err(ScheduleError::Message(format!(
                    "Schedule {id} is already completed"
                )));
            }
            if schedule.status == ScheduleStatus::Active {
                return Ok(None);
            }
            let now = self.now();
            Ok(Some(StoredSchedule {
                status: ScheduleStatus::Active,
                paused_at: None,
                next_run_at: Some(to_iso(compute_next_run_at(&schedule.cadence, now)?)),
                updated_at: to_iso(now),
                ..schedule.clone()
            }))
        })?;
        require_schedule(resumed, id)
    }

    pub fn update(&self, input: UpdateScheduleInput) -> Result<StoredSchedule> {
        let next = self.store.update(&input.id, |schedule| {
            let now = self.now();
            let mut updated = schedule.clone();

            if let Some(prompt) = &input.prompt {
                updated.prompt = normalize_prompt(prompt)?;
            }
            if let Some(name) = &input.name {
                updated.name = trim_optional_name(name.as_deref());
            }
            if let Some(next_cadence) = &input.cadence {
                let cadence = merge_cadence_timezone(&updated.cadence, next_cadence);
                validate_schedule_cadence(&cadence)?;
                let next_run_at = if updated.status == ScheduleStatus::Active {
                    Some(to_iso(compute_next_run_at(&cadence, now)?))
                } else {
                    None
                };
                updated.cadence = cadence;
                updated.next_run_at = next_run_at;
            }
            if let Some(patch) = &input.new_agent_config {
                match &updated.target {
                    ScheduleTarget::NewAgent { config } => {
                        updated.target = ScheduleTarget::NewAgent {
                            config: apply_new_agent_config(config, patch)?,
                        };
                    }
                    ScheduleTarget::Agent { .. } => {
                        return Err(ScheduleError::msg(
                            "new-agent config updates are only valid for new-agent target schedules",
                        ));
                    }
                }
            }
            if let Some(max_runs) = &input.max_runs {
                updated.max_runs = normalize_max_runs(*max_runs)?;
            }
            if let Some(expires_at) = &input.expires_at {
                updated.expires_at = expires_at.clone();
            }
            updated.updated_at = to_iso(now);
            Ok(Some(updated))
        })?;
        require_schedule(next, &input.id)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        self.store.delete(id)
    }

    /// Complete every non-completed `agent`-target schedule of an archived/deleted
    /// agent. Returns how many were completed.
    pub fn complete_for_agent(&self, agent_id: &str) -> Result<usize> {
        let now = self.now();
        let matches: Vec<StoredSchedule> = self
            .store
            .list()?
            .into_iter()
            .filter(|s| {
                matches!(&s.target, ScheduleTarget::Agent { agent_id: a } if a == agent_id)
                    && s.status != ScheduleStatus::Completed
            })
            .collect();
        let mut completed = 0;
        for schedule in matches {
            match self.complete_schedule_for_agent(&schedule.id, agent_id, now) {
                Ok(true) => completed += 1,
                Ok(false) => {}
                Err(error) => self.logger.warn(&format!(
                    "Failed to complete schedule for archived agent; continuing (scheduleId={}, agentId={agent_id}): {error}",
                    schedule.id
                )),
            }
        }
        Ok(completed)
    }

    fn complete_schedule_for_agent(&self, id: &str, agent_id: &str, now: i64) -> Result<bool> {
        let mut completed = false;
        let updated = self.store.update(id, |schedule| {
            let matches_agent = matches!(
                &schedule.target,
                ScheduleTarget::Agent { agent_id: a } if a == agent_id
            );
            if !matches_agent || schedule.status == ScheduleStatus::Completed {
                return Ok(None);
            }
            completed = true;
            Ok(Some(complete_schedule(schedule, now)))
        })?;
        require_schedule(updated, id)?;
        Ok(completed)
    }

    // ----- running ----------------------------------------------------------

    /// Run once now without advancing the cadence or recomputing completion.
    pub fn run_once(&self, id: &str) -> Result<StoredSchedule> {
        let schedule = self.inspect(id)?;
        if schedule.status == ScheduleStatus::Completed {
            return Err(ScheduleError::Message(format!(
                "Schedule {id} is already completed"
            )));
        }
        if self.is_running(id) {
            return Err(ScheduleError::Message(format!(
                "Schedule {id} is already running"
            )));
        }
        self.run_schedule(&schedule, self.now(), true)?;
        self.inspect(id)
    }

    /// Process every due schedule once. Errors from storage abort the tick.
    pub fn tick(&self) -> Result<()> {
        self.tick_with(&mut |schedule, now| self.run_schedule(&schedule, now, false))
    }

    /// Like [`Self::tick`] but each due run goes to its own worker thread, so a slow
    /// agent never delays other schedules. Returns once every due run is dispatched.
    fn tick_dispatch(self: &Arc<Self>) -> Result<()> {
        self.tick_with(&mut |schedule, now| {
            // Reserve before spawning so the next tick sees the schedule as running.
            let Some(guard) = self.reserve(&schedule.id) else {
                return Ok(());
            };
            let service = Arc::clone(self);
            let spawned = std::thread::Builder::new()
                .name(format!("paseo-schedule-run-{}", schedule.id))
                .spawn(move || {
                    if let Err(error) = service.run_reserved(&schedule, now, false, guard) {
                        service
                            .logger
                            .error(&format!("Scheduled run failed: {error}"));
                    }
                });
            if let Err(error) = spawned {
                self.logger
                    .error(&format!("Failed to spawn schedule worker: {error}"));
            }
            Ok(())
        })
    }

    fn tick_with(&self, run: &mut dyn FnMut(StoredSchedule, i64) -> Result<()>) -> Result<()> {
        let now = self.now();
        for schedule in self.store.list()? {
            if schedule.status != ScheduleStatus::Active {
                continue;
            }
            let next_run_at = match &schedule.next_run_at {
                Some(n) => n.clone(),
                None => continue,
            };
            if self.is_running(&schedule.id) {
                continue;
            }
            if should_complete_schedule(&schedule, now) {
                self.complete_schedule_if_due(&schedule.id, now)?;
                continue;
            }
            if !due(&next_run_at, now) {
                continue;
            }
            run(schedule, now)?;
        }
        Ok(())
    }

    fn complete_schedule_if_due(&self, id: &str, now: i64) -> Result<()> {
        let updated = self.store.update(id, |schedule| {
            if schedule.status != ScheduleStatus::Active
                || schedule.next_run_at.is_none()
                || !should_complete_schedule(schedule, now)
            {
                return Ok(None);
            }
            Ok(Some(complete_schedule(schedule, now)))
        })?;
        require_schedule(updated, id)?;
        Ok(())
    }

    fn recover_interrupted_runs(&self) -> Result<()> {
        let now = self.now();
        for schedule in self.store.list()? {
            self.recover_interrupted_schedule(&schedule.id, now)?;
        }
        Ok(())
    }

    fn recover_interrupted_schedule(&self, id: &str, now: i64) -> Result<()> {
        // (workspace_id, agent_id, run_id)
        let mut interrupted: Vec<(String, Option<String>, String)> = Vec::new();
        self.store.update(id, |current| {
            let mut updated = current.clone();
            let mut dirty = false;

            if let Some(index) = updated
                .runs
                .iter()
                .position(|r| r.status == RunStatus::Running)
            {
                let running = updated.runs[index].clone();
                if let ScheduleTarget::NewAgent { config } = &updated.target {
                    if let Some(workspace_id) = running.workspace().filter(|w| !w.is_empty()) {
                        if should_archive_schedule_run_workspace(
                            running.agent_id.as_deref(),
                            config.archive_on_finish,
                        ) {
                            interrupted.push((
                                workspace_id.to_string(),
                                running.agent_id.clone(),
                                running.id.clone(),
                            ));
                        }
                    }
                }
                updated.runs[index] = ScheduleRun {
                    status: RunStatus::Failed,
                    ended_at: Some(to_iso(now)),
                    error: Some("Daemon restarted before the scheduled run completed".to_string()),
                    ..running
                };
                if should_complete_schedule(&updated, now) {
                    updated = complete_schedule(&updated, now);
                }
                dirty = true;
            }

            if updated.status == ScheduleStatus::Active {
                if let Some(next) = updated.next_run_at.clone().filter(|n| due(n, now)) {
                    let mut next_run_at =
                        compute_next_run_at(&updated.cadence, parse_js_date(&next).unwrap_or(now))?;
                    while next_run_at <= now {
                        next_run_at = compute_next_run_at(&updated.cadence, next_run_at)?;
                    }
                    updated.next_run_at = Some(to_iso(next_run_at));
                    dirty = true;
                }
            }

            if dirty {
                updated.updated_at = to_iso(now);
                Ok(Some(updated))
            } else {
                Ok(None)
            }
        })?;
        if let Some((workspace_id, agent_id, run_id)) = interrupted.into_iter().next() {
            if let Err(error) = self.host.archive_workspace(&workspace_id) {
                self.logger.warn(&format!(
                    "Failed to archive interrupted scheduled workspace after daemon restart (scheduleId={id}, runId={run_id}, workspaceId={workspace_id}, agentId={}): {error}",
                    agent_id.as_deref().unwrap_or("none")
                ));
            }
        }
        Ok(())
    }

    /// Agent-target schedules whose agent is gone or archived can never fire
    /// successfully; complete them on startup.
    fn sweep_orphaned_schedules(&self) -> Result<()> {
        let now = self.now();
        for schedule in self.store.list()? {
            self.store.update(&schedule.id, |current| {
                let agent_id = match &current.target {
                    ScheduleTarget::Agent { agent_id } => agent_id,
                    ScheduleTarget::NewAgent { .. } => return Ok(None),
                };
                if current.status == ScheduleStatus::Completed {
                    return Ok(None);
                }
                if self.host.agent_target_state(agent_id)? == AgentTargetState::Active {
                    return Ok(None);
                }
                Ok(Some(complete_schedule(current, now)))
            })?;
        }
        Ok(())
    }

    fn run_schedule(&self, schedule: &StoredSchedule, now: i64, manual: bool) -> Result<()> {
        // Reserve before store I/O; persisted history cannot prove a run is still alive.
        let guard = match self.reserve(&schedule.id) {
            Some(g) => g,
            None if manual => {
                return Err(ScheduleError::Message(format!(
                    "Schedule {} is already running",
                    schedule.id
                )))
            }
            None => return Ok(()),
        };
        self.run_reserved(schedule, now, manual, guard)
    }

    fn run_reserved(
        &self,
        schedule: &StoredSchedule,
        now: i64,
        manual: bool,
        _guard: RunningGuard,
    ) -> Result<()> {
        let (schedule_with_run, run_id) = if manual {
            let run_id = uuid::Uuid::new_v4().to_string();
            let running = ScheduleRun {
                id: run_id.clone(),
                scheduled_for: to_iso(now),
                started_at: to_iso(now),
                ended_at: None,
                status: RunStatus::Running,
                agent_id: None,
                workspace_id: None,
                output: None,
                error: None,
            };
            (self.append_running_run(&schedule.id, running)?, run_id)
        } else {
            match self.claim_due_run(&schedule.id, now)? {
                Some(claimed) => claimed,
                None => return Ok(()),
            }
        };

        match self.invoke(&schedule_with_run, &run_id) {
            Ok(result) => self.finish_run(FinishRun {
                schedule_id: &schedule.id,
                run_id: &run_id,
                status: RunStatus::Succeeded,
                agent_id: result.agent_id,
                output: result.output,
                error: None,
                target_gone: false,
                manual,
            }),
            Err(error) => self.finish_run(FinishRun {
                schedule_id: &schedule.id,
                run_id: &run_id,
                status: RunStatus::Failed,
                agent_id: None,
                output: None,
                error: Some(error.message().to_string()),
                target_gone: error.is_target_gone(),
                manual,
            }),
        }
    }

    fn invoke(&self, schedule: &StoredSchedule, run_id: &str) -> Result<ScheduleExecutionResult> {
        match &self.runner {
            Some(runner) => runner(schedule, run_id),
            None => self.execute_schedule(schedule, run_id),
        }
    }

    fn claim_due_run(&self, id: &str, now: i64) -> Result<Option<(StoredSchedule, String)>> {
        let mut claimed: Option<(StoredSchedule, String)> = None;
        let updated = self.store.update(id, |schedule| {
            let next_run_at = match &schedule.next_run_at {
                Some(n) if schedule.status == ScheduleStatus::Active => n.clone(),
                _ => return Ok(None),
            };
            if should_complete_schedule(schedule, now) || !due(&next_run_at, now) {
                return Ok(None);
            }
            let scheduled_for = next_run_at;
            let is_final_run = schedule
                .max_runs
                .is_some_and(|max| count_completed_runs(schedule) + 1 >= max);
            let next = if is_final_run {
                None
            } else {
                let scheduled_ms = parse_js_date(&scheduled_for).unwrap_or(now);
                let after = compute_next_run_at(&schedule.cadence, scheduled_ms)?;
                Some(to_iso(advance_next_run_at_past(
                    &schedule.cadence,
                    after,
                    now,
                )?))
            };
            // Claim the due slot and advance its cursor in the same serialized update.
            let run_id = uuid::Uuid::new_v4().to_string();
            let running = ScheduleRun {
                id: run_id.clone(),
                scheduled_for,
                started_at: to_iso(now),
                ended_at: None,
                status: RunStatus::Running,
                agent_id: None,
                workspace_id: None,
                output: None,
                error: None,
            };
            let mut updated = schedule.clone();
            updated.next_run_at = next;
            updated.updated_at = to_iso(now);
            updated.runs.push(running);
            claimed = Some((updated.clone(), run_id));
            Ok(Some(updated))
        })?;
        require_schedule(updated, id)?;
        Ok(claimed)
    }

    fn append_running_run(&self, id: &str, running: ScheduleRun) -> Result<StoredSchedule> {
        let updated = self.store.update(id, |schedule| {
            let mut next = schedule.clone();
            next.updated_at = running.started_at.clone();
            next.runs.push(running.clone());
            Ok(Some(next))
        })?;
        require_schedule(updated, id)
    }

    fn finish_run(&self, p: FinishRun<'_>) -> Result<()> {
        let updated = self.store.update(p.schedule_id, |schedule| {
            let now = self.now();
            let now_iso = to_iso(now);
            let mut updated = schedule.clone();
            for run in &mut updated.runs {
                if run.id == p.run_id {
                    run.status = p.status;
                    run.ended_at = Some(now_iso.clone());
                    run.agent_id = p.agent_id.clone().or_else(|| run.agent_id.clone());
                    run.output = p.output.clone();
                    run.error = p.error.clone();
                }
            }
            updated.last_run_at = Some(now_iso.clone());
            updated.updated_at = now_iso;

            if p.target_gone {
                // The target is permanently gone; retrying only burns the schedule
                // down to its expiry, so complete it regardless of manual/scheduled origin.
                updated = complete_schedule(&updated, now);
            } else if updated.status == ScheduleStatus::Completed {
                // Completed concurrently (e.g. the target agent was archived mid-run):
                // record the outcome but leave the schedule terminal.
            } else if p.manual {
                // Manual one-shot runs do not advance the cadence or recompute completion.
            } else if should_complete_schedule(&updated, now) {
                updated = complete_schedule(&updated, now);
            } else if updated.status == ScheduleStatus::Paused {
                updated.next_run_at = None;
            } else {
                // A final slot has no successor until its run limit is raised or cleared.
                let next = match &updated.next_run_at {
                    Some(n) => {
                        parse_js_date(n).ok_or_else(|| ScheduleError::msg("Invalid time value"))?
                    }
                    None => {
                        let finished = updated.runs.iter().find(|r| r.id == p.run_id);
                        let scheduled_for = finished
                            .and_then(|r| parse_js_date(&r.scheduled_for))
                            .ok_or_else(|| ScheduleError::msg("Invalid time value"))?;
                        compute_next_run_at(&updated.cadence, scheduled_for)?
                    }
                };
                // Move an overdue cursor past a long run without advancing a future one twice.
                updated.next_run_at = Some(to_iso(advance_next_run_at_past(
                    &updated.cadence,
                    next,
                    now,
                )?));
            }
            Ok(Some(updated))
        })?;
        require_schedule(updated, p.schedule_id)?;
        Ok(())
    }

    fn record_run_workspace(
        &self,
        schedule_id: &str,
        run_id: &str,
        workspace_id: &str,
        agent_id: Option<&str>,
    ) -> Result<()> {
        let updated = self.store.update(schedule_id, |schedule| {
            let mut next = schedule.clone();
            next.updated_at = to_iso(self.now());
            for run in &mut next.runs {
                if run.id == run_id && run.status == RunStatus::Running {
                    run.workspace_id = Some(Some(workspace_id.to_string()));
                    run.agent_id = agent_id.map(str::to_string);
                }
            }
            Ok(Some(next))
        })?;
        require_schedule(updated, schedule_id)?;
        Ok(())
    }

    // ----- host-backed execution -------------------------------------------

    fn execute_schedule(
        &self,
        schedule: &StoredSchedule,
        run_id: &str,
    ) -> Result<ScheduleExecutionResult> {
        match &schedule.target {
            ScheduleTarget::Agent { agent_id } => {
                self.execute_agent_target(schedule, run_id, agent_id)
            }
            ScheduleTarget::NewAgent { config } => {
                self.execute_new_agent_target(schedule, run_id, config)
            }
        }
    }

    fn execute_agent_target(
        &self,
        schedule: &StoredSchedule,
        run_id: &str,
        agent_id: &str,
    ) -> Result<ScheduleExecutionResult> {
        let prompt = format_system_notification_prompt(&build_schedule_fire_body(schedule, run_id));
        match self.host.agent_target_state(agent_id)? {
            AgentTargetState::Missing => {
                return Err(ScheduleError::TargetGone(format!(
                    "Agent {agent_id} no longer exists"
                )))
            }
            AgentTargetState::Archived => {
                return Err(ScheduleError::TargetGone(format!(
                    "Agent {agent_id} is archived"
                )))
            }
            AgentTargetState::Active => {}
        }
        let outcome = self.host.run_existing_agent(agent_id, &prompt)?;
        if outcome.permission_pending {
            return Err(ScheduleError::Message(format!(
                "Scheduled agent {agent_id} is waiting for permission"
            )));
        }
        if outcome.errored {
            return Err(ScheduleError::Message(
                outcome
                    .last_message
                    .unwrap_or_else(|| format!("Scheduled agent {agent_id} failed")),
            ));
        }
        Ok(ScheduleExecutionResult {
            agent_id: Some(agent_id.to_string()),
            output: build_run_output(None, "", outcome.last_message.as_deref().unwrap_or("")),
        })
    }

    fn execute_new_agent_target(
        &self,
        schedule: &StoredSchedule,
        run_id: &str,
        config: &NewAgentConfig,
    ) -> Result<ScheduleExecutionResult> {
        assert_new_agent_cwd_directory(&config.cwd)?;
        let mut workspace_id: Option<String> = None;
        let mut agent_id: Option<String> = None;
        let result = self.run_new_agent(schedule, run_id, config, &mut workspace_id, &mut agent_id);
        if let Some(workspace_id) = workspace_id {
            if should_archive_schedule_run_workspace(agent_id.as_deref(), config.archive_on_finish)
            {
                if let Err(error) = self.host.archive_workspace(&workspace_id) {
                    self.logger.warn(&format!(
                        "Failed to archive scheduled workspace after run (scheduleId={}, runId={run_id}, workspaceId={workspace_id}): {error}",
                        schedule.id
                    ));
                }
            }
        }
        result
    }

    fn run_new_agent(
        &self,
        schedule: &StoredSchedule,
        run_id: &str,
        config: &NewAgentConfig,
        workspace_out: &mut Option<String>,
        agent_out: &mut Option<String>,
    ) -> Result<ScheduleExecutionResult> {
        let workspace = self.host.create_run_workspace(
            &config.cwd,
            config.isolation.unwrap_or(Isolation::Local),
            &schedule.prompt,
        )?;
        *workspace_out = Some(workspace.workspace_id.clone());
        self.record_run_workspace(&schedule.id, run_id, &workspace.workspace_id, None)?;

        let mut run_config = config.clone();
        run_config.cwd = workspace.cwd.clone();
        let created = self.host.create_agent(&CreateScheduledAgent {
            provider: format_provider_model(&config.provider, config.model.as_deref()),
            config: run_config,
            cwd: workspace.cwd.clone(),
            workspace_id: workspace.workspace_id.clone(),
            labels: vec![
                ("paseo.schedule-id".to_string(), schedule.id.clone()),
                ("paseo.schedule-run".to_string(), run_id.to_string()),
            ],
            prompt: schedule.prompt.clone(),
        })?;
        *agent_out = Some(created.agent_id.clone());
        self.record_run_workspace(
            &schedule.id,
            run_id,
            &workspace.workspace_id,
            Some(&created.agent_id),
        )?;
        if let Some(error) = created.initial_prompt_error {
            return Err(ScheduleError::Message(error));
        }
        let result = self.host.run_agent(&created.agent_id, &schedule.prompt)?;
        let outcome = self.host.wait_for_agent_event(&created.agent_id)?;
        if result.canceled {
            return Err(ScheduleError::Message(format!(
                "Scheduled agent {} was canceled",
                created.agent_id
            )));
        }
        if outcome.permission_pending {
            return Err(ScheduleError::Message(format!(
                "Scheduled agent {} is waiting for permission",
                created.agent_id
            )));
        }
        if outcome.errored {
            return Err(ScheduleError::Message(outcome.last_message.unwrap_or_else(
                || format!("Scheduled agent {} failed", created.agent_id),
            )));
        }
        Ok(ScheduleExecutionResult {
            agent_id: Some(created.agent_id),
            output: build_run_output(
                outcome.last_message.as_deref(),
                &result.timeline_text,
                &result.final_text,
            ),
        })
    }
}

struct FinishRun<'a> {
    schedule_id: &'a str,
    run_id: &'a str,
    status: RunStatus,
    agent_id: Option<String>,
    output: Option<String>,
    error: Option<String>,
    target_gone: bool,
    manual: bool,
}

fn assert_new_agent_cwd_directory(cwd: &str) -> Result<()> {
    match std::fs::metadata(cwd) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(ScheduleError::TargetGone(format!(
            "Working directory {cwd} is not a directory"
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(ScheduleError::TargetGone(
            format!("Working directory {cwd} no longer exists"),
        )),
        Err(e) => Err(e.into()),
    }
}

/// Stops the background ticker on [`TickerHandle::stop`] or drop.
pub struct TickerHandle {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl TickerHandle {
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for TickerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}
