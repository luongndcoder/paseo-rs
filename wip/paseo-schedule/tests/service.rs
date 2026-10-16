mod common;

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};

use common::*;
use paseo_schedule::host::*;
use paseo_schedule::types::*;
use paseo_schedule::{ScheduleError, ScheduleService};

const A1: &str = "00000000-0000-0000-0000-000000000001";

fn setup() -> (tempfile::TempDir, TestClock, Arc<FakeHost>) {
    (
        tempfile::tempdir().unwrap(),
        TestClock::new("2026-01-01T00:00:00.000Z"),
        FakeHost::new(),
    )
}

fn cwd(dir: &tempfile::TempDir) -> String {
    dir.path().to_str().unwrap().to_string()
}

fn ok_with(agent: Option<&str>, output: &str) -> Option<paseo_schedule::service::ScheduleRunner> {
    let agent = agent.map(str::to_string);
    let output = output.to_string();
    runner(move |s, _| {
        Ok(ScheduleExecutionResult {
            agent_id: agent.clone(),
            output: Some(format!(
                "{output}{}",
                if output.ends_with(':') {
                    s.prompt.as_str()
                } else {
                    ""
                }
            )),
        })
    })
}

// ----- ticking ---------------------------------------------------------------

#[test]
fn ticks_due_schedules_and_records_run_history_on_disk() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_with(Some(A1), "ran:"));
    let created = svc
        .create(input(
            "Review new PRs",
            every(60_000),
            new_agent(&cwd(&dir)),
        ))
        .unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    let inspected = svc.inspect(&created.id).unwrap();
    assert_eq!(inspected.runs.len(), 1);
    assert_eq!(inspected.runs[0].status, RunStatus::Succeeded);
    assert_eq!(inspected.runs[0].agent_id.as_deref(), Some(A1));
    assert_eq!(
        inspected.runs[0].output.as_deref(),
        Some("ran:Review new PRs")
    );
    assert_eq!(
        inspected.next_run_at.as_deref(),
        Some("2026-01-01T00:02:00.000Z")
    );
}

fn shared_slot_case(kind: &str) {
    let (dir, clock, host) = setup();
    let later_runs = Arc::new(AtomicUsize::new(0));
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let started_tx = Mutex::new(started_tx);
    let release_rx = Mutex::new(release_rx);
    let counter = later_runs.clone();
    let svc = service(
        dir.path(),
        &clock,
        host,
        runner(move |s, _| {
            if s.prompt == "block the first tick" {
                started_tx.lock().unwrap().send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
            } else {
                counter.fetch_add(1, Ordering::SeqCst);
            }
            Ok(ScheduleExecutionResult {
                agent_id: None,
                output: Some("ok".into()),
            })
        }),
    );
    let cadence = ScheduleCadence::Cron {
        expression: "0 6 * * 1".into(),
        timezone: Some("Europe/Prague".into()),
    };
    svc.create(input(
        "block the first tick",
        cadence.clone(),
        new_agent(&cwd(&dir)),
    ))
    .unwrap();
    clock.set("2026-01-01T00:00:01.000Z");
    let target = if kind == "agent" {
        agent(A1)
    } else {
        new_agent(&cwd(&dir))
    };
    let mut later_input = input("later due schedule", cadence, target);
    if kind == "maxRuns: 1" {
        later_input.max_runs = Some(1);
    }
    let later = svc.create(later_input).unwrap();
    clock.set("2026-01-05T05:00:00.000Z");
    std::thread::scope(|scope| {
        let delayed = scope.spawn(|| svc.tick().unwrap());
        started_rx.recv().unwrap();
        clock.set("2026-01-05T05:00:01.000Z");
        svc.tick().unwrap();
        release_tx.send(()).unwrap();
        delayed.join().unwrap();
    });
    let result = svc.inspect(&later.id).unwrap();
    assert_eq!(later_runs.load(Ordering::SeqCst), 1, "{kind}");
    assert_eq!(result.runs.len(), 1);
    assert_eq!(result.runs[0].scheduled_for, "2026-01-05T05:00:00.000Z");
    if kind == "maxRuns: 1" {
        assert_eq!(result.next_run_at, None);
        assert_eq!(result.status, ScheduleStatus::Completed);
    } else {
        assert_eq!(
            result.next_run_at.as_deref(),
            Some("2026-01-12T05:00:00.000Z")
        );
        assert_eq!(result.status, ScheduleStatus::Active);
    }
}

#[test]
fn dispatches_a_shared_cron_slot_only_once_for_new_agent() {
    shared_slot_case("new-agent");
}

#[test]
fn dispatches_a_shared_cron_slot_only_once_for_agent() {
    shared_slot_case("agent");
}

#[test]
fn dispatches_a_shared_cron_slot_only_once_for_max_runs_1() {
    shared_slot_case("maxRuns: 1");
}

#[test]
fn advances_the_next_slot_past_a_run_that_crosses_a_cadence_boundary() {
    let (dir, clock, host) = setup();
    let c = clock.clone();
    let svc = service(
        dir.path(),
        &clock,
        host,
        runner(move |_, _| {
            c.set("2026-01-01T00:02:00.000Z");
            Ok(ScheduleExecutionResult {
                agent_id: None,
                output: Some("ok".into()),
            })
        }),
    );
    let created = svc
        .create(input(
            "long-running task",
            every(60_000),
            new_agent(&cwd(&dir)),
        ))
        .unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    assert_eq!(
        svc.inspect(&created.id).unwrap().next_run_at.as_deref(),
        Some("2026-01-01T00:03:00.000Z")
    );
}

#[test]
fn runs_the_final_allowed_cron_slot_without_requiring_another_occurrence() {
    let (dir, clock, host) = setup();
    let dispatches = Arc::new(AtomicUsize::new(0));
    let d = dispatches.clone();
    let svc = service(
        dir.path(),
        &clock,
        host,
        runner(move |_, _| {
            d.fetch_add(1, Ordering::SeqCst);
            Ok(ScheduleExecutionResult {
                agent_id: None,
                output: Some("done".into()),
            })
        }),
    );
    let mut i = input(
        "Run once on January 5 when it is Monday",
        cron("0 6 5 1 1"),
        new_agent(&cwd(&dir)),
    );
    i.max_runs = Some(1);
    let created = svc.create(i).unwrap();
    clock.set("2026-01-05T06:00:00.000Z");
    svc.tick().unwrap();
    let result = svc.inspect(&created.id).unwrap();
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(result.status, ScheduleStatus::Completed);
    assert_eq!(result.next_run_at, None);
    assert_eq!(result.runs.len(), 1);
    assert_eq!(result.runs[0].status, RunStatus::Succeeded);
}

#[cfg(unix)]
#[test]
fn keeps_firing_future_slots_after_an_outcome_could_not_be_saved() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, clock, host) = setup();
    let schedules_dir = dir.path().join("schedules");
    let dispatches = Arc::new(AtomicUsize::new(0));
    let (d, sd) = (dispatches.clone(), schedules_dir.clone());
    let svc = service(
        dir.path(),
        &clock,
        host,
        runner(move |_, _| {
            if d.fetch_add(1, Ordering::SeqCst) == 0 {
                fs::set_permissions(&sd, fs::Permissions::from_mode(0o555)).unwrap();
            }
            Ok(ScheduleExecutionResult {
                agent_id: None,
                output: Some("done".into()),
            })
        }),
    );
    let mut i = input(
        "Check status each minute",
        every(60_000),
        new_agent(&cwd(&dir)),
    );
    i.run_on_create = Some(false);
    let created = svc.create(i).unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    let first = svc.tick();
    fs::set_permissions(&schedules_dir, fs::Permissions::from_mode(0o755)).unwrap();
    if fs::write(schedules_dir.join(".probe"), "").is_ok() && first.is_ok() {
        return; // root: permissions did not apply
    }
    let _ = fs::remove_file(schedules_dir.join(".probe"));
    assert!(first.is_err());
    assert_eq!(
        svc.inspect(&created.id).unwrap().runs[0].status,
        RunStatus::Running
    );
    clock.set("2026-01-01T00:02:00.000Z");
    svc.tick().unwrap();
    let result = svc.inspect(&created.id).unwrap();
    assert_eq!(dispatches.load(Ordering::SeqCst), 2);
    let scheduled: Vec<_> = result
        .runs
        .iter()
        .map(|r| r.scheduled_for.as_str())
        .collect();
    assert_eq!(
        scheduled,
        vec!["2026-01-01T00:01:00.000Z", "2026-01-01T00:02:00.000Z"]
    );
    assert_eq!(
        result.next_run_at.as_deref(),
        Some("2026-01-01T00:03:00.000Z")
    );
}

fn interrupted_run(id: &str, at: &str) -> ScheduleRun {
    ScheduleRun {
        id: id.into(),
        scheduled_for: at.into(),
        started_at: at.into(),
        ended_at: None,
        status: RunStatus::Running,
        agent_id: None,
        workspace_id: None,
        output: None,
        error: None,
    }
}

#[test]
fn completes_an_interrupted_final_slot_during_restart_recovery() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let mut i = input("Run once", cron("0 6 5 1 1"), new_agent(&cwd(&dir)));
    i.max_runs = Some(1);
    let created = svc.create(i).unwrap();
    clock.set("2026-01-05T06:00:00.000Z");
    svc.store()
        .update(&created.id, |s| {
            let mut n = s.clone();
            n.next_run_at = None;
            n.runs = vec![interrupted_run(
                "interrupted-final-slot",
                "2026-01-05T06:00:00.000Z",
            )];
            Ok(Some(n))
        })
        .unwrap();
    clock.set("2026-01-05T06:01:00.000Z");
    svc.start().unwrap();
    let result = svc.inspect(&created.id).unwrap();
    assert_eq!(result.status, ScheduleStatus::Completed);
    assert_eq!(result.next_run_at, None);
    assert_eq!(result.runs.len(), 1);
    assert_eq!(result.runs[0].status, RunStatus::Failed);
    assert_eq!(
        result.runs[0].error.as_deref(),
        Some("Daemon restarted before the scheduled run completed")
    );
}

fn max_runs_changes_case(new_max: Option<i64>) {
    let (dir, clock, host) = setup();
    let cell: Arc<OnceLock<Arc<ScheduleService>>> = Arc::new(OnceLock::new());
    let c2 = cell.clone();
    let svc = service(
        dir.path(),
        &clock,
        host,
        runner(move |s, _| {
            c2.get().unwrap().update(UpdateScheduleInput {
                id: s.id.clone(),
                max_runs: Some(new_max),
                ..Default::default()
            })?;
            Ok(ScheduleExecutionResult {
                agent_id: None,
                output: Some("done".into()),
            })
        }),
    );
    cell.set(svc.clone()).ok();
    let mut i = input(
        "Allow more runs during execution",
        every(60_000),
        new_agent(&cwd(&dir)),
    );
    i.run_on_create = Some(false);
    i.max_runs = Some(1);
    let created = svc.create(i).unwrap();
    clock.set("2026-01-01T00:01:30.000Z");
    svc.tick().unwrap();
    let result = svc.inspect(&created.id).unwrap();
    assert_eq!(result.status, ScheduleStatus::Active);
    assert_eq!(
        result.next_run_at.as_deref(),
        Some("2026-01-01T00:02:00.000Z")
    );
    assert_eq!(result.runs.len(), 1);
}

#[test]
fn keeps_a_next_slot_when_the_final_runs_limit_changes_to_2() {
    max_runs_changes_case(Some(2));
}

#[test]
fn keeps_a_next_slot_when_the_final_runs_limit_changes_to_null() {
    max_runs_changes_case(None);
}

#[test]
fn pause_and_resume_update_persisted_schedule_state() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let created = svc
        .create(input("Check status", every(60_000), new_agent(&cwd(&dir))))
        .unwrap();
    let paused = svc.pause(&created.id).unwrap();
    assert_eq!(paused.status, ScheduleStatus::Paused);
    assert_eq!(paused.next_run_at, None);
    assert!(paused.paused_at.is_some());
    clock.set("2026-01-01T00:03:00.000Z");
    let resumed = svc.resume(&created.id).unwrap();
    assert_eq!(resumed.status, ScheduleStatus::Active);
    assert_eq!(resumed.paused_at, None);
    assert_eq!(
        resumed.next_run_at.as_deref(),
        Some("2026-01-01T00:04:00.000Z")
    );
}

#[test]
fn pause_and_resume_are_idempotent_and_reject_completed_schedules() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let created = svc
        .create(input("p", every(60_000), new_agent(&cwd(&dir))))
        .unwrap();
    assert_eq!(
        svc.resume(&created.id).unwrap().status,
        ScheduleStatus::Active
    );
    svc.pause(&created.id).unwrap();
    assert_eq!(
        svc.pause(&created.id).unwrap().status,
        ScheduleStatus::Paused
    );
    svc.store()
        .update(&created.id, |s| {
            let mut n = s.clone();
            n.status = ScheduleStatus::Completed;
            Ok(Some(n))
        })
        .unwrap();
    assert_eq!(
        svc.pause(&created.id).unwrap_err().to_string(),
        format!("Schedule {} is already completed", created.id)
    );
    assert_eq!(
        svc.resume("nope1234").unwrap_err().to_string(),
        "Schedule not found: nope1234"
    );
}

#[test]
fn completes_schedules_when_max_runs_is_reached() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_with(None, "done"));
    let mut i = input("One shot", every(60_000), new_agent(&cwd(&dir)));
    i.max_runs = Some(1);
    let created = svc.create(i).unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    let inspected = svc.inspect(&created.id).unwrap();
    assert_eq!(inspected.status, ScheduleStatus::Completed);
    assert_eq!(inspected.next_run_at, None);
    assert_eq!(inspected.runs.len(), 1);
}

#[test]
fn completes_expired_schedules_on_tick() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let mut i = input("expiring", every(60_000), new_agent(&cwd(&dir)));
    i.run_on_create = Some(false);
    i.expires_at = Some("2026-01-01T00:00:30.000Z".into());
    let created = svc.create(i).unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    let inspected = svc.inspect(&created.id).unwrap();
    assert_eq!(inspected.status, ScheduleStatus::Completed);
    assert!(inspected.runs.is_empty());
}

#[test]
fn keeps_schedules_paused_when_an_in_flight_run_finishes_after_pause() {
    let (dir, clock, host) = setup();
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (started_tx, release_rx) = (Mutex::new(started_tx), Mutex::new(release_rx));
    let svc = service(
        dir.path(),
        &clock,
        host,
        runner(move |_, _| {
            started_tx.lock().unwrap().send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            Ok(ScheduleExecutionResult {
                agent_id: None,
                output: Some("finished".into()),
            })
        }),
    );
    let created = svc
        .create(input("Check status", every(60_000), new_agent(&cwd(&dir))))
        .unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    std::thread::scope(|scope| {
        let tick = scope.spawn(|| svc.tick().unwrap());
        started_rx.recv().unwrap();
        let paused = svc.pause(&created.id).unwrap();
        assert_eq!(paused.status, ScheduleStatus::Paused);
        assert_eq!(paused.next_run_at, None);
        release_tx.send(()).unwrap();
        tick.join().unwrap();
    });
    let inspected = svc.inspect(&created.id).unwrap();
    assert_eq!(inspected.status, ScheduleStatus::Paused);
    assert_eq!(inspected.next_run_at, None);
    assert_eq!(inspected.runs.len(), 1);
    assert_eq!(inspected.runs[0].status, RunStatus::Succeeded);
}

// ----- creation defaults -----------------------------------------------------

#[test]
fn defaults_every_schedules_to_fire_immediately_on_creation() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let created = svc
        .create(input("p", every(60_000), new_agent(&cwd(&dir))))
        .unwrap();
    assert_eq!(
        created.next_run_at.as_deref(),
        Some("2026-01-01T00:00:00.000Z")
    );
}

#[test]
fn every_with_run_on_create_false_waits_the_full_interval() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let mut i = input("p", every(60_000), new_agent(&cwd(&dir)));
    i.run_on_create = Some(false);
    let created = svc.create(i).unwrap();
    assert_eq!(
        created.next_run_at.as_deref(),
        Some("2026-01-01T00:01:00.000Z")
    );
}

#[test]
fn cron_defaults_to_the_next_cron_slot() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let created = svc
        .create(input("p", cron("30 9 * * *"), new_agent(&cwd(&dir))))
        .unwrap();
    assert_eq!(
        created.next_run_at.as_deref(),
        Some("2026-01-01T09:30:00.000Z")
    );
}

#[test]
fn cron_with_run_on_create_true_fires_immediately() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let mut i = input("p", cron("30 9 * * *"), new_agent(&cwd(&dir)));
    i.run_on_create = Some(true);
    let created = svc.create(i).unwrap();
    assert_eq!(
        created.next_run_at.as_deref(),
        Some("2026-01-01T00:00:00.000Z")
    );
}

#[test]
fn create_validates_prompt_cadence_and_max_runs() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let c = cwd(&dir);
    assert_eq!(
        svc.create(input("   ", every(1000), new_agent(&c)))
            .unwrap_err()
            .to_string(),
        "Schedule prompt is required"
    );
    assert_eq!(
        svc.create(input("p", cron("bad"), new_agent(&c)))
            .unwrap_err()
            .to_string(),
        "Cron expressions must have 5 fields"
    );
    let mut i = input("p", every(1000), new_agent(&c));
    i.max_runs = Some(0);
    assert_eq!(
        svc.create(i).unwrap_err().to_string(),
        "maxRuns must be a positive integer"
    );
    let mut named = input("p", every(1000), new_agent(&c));
    named.name = Some("  padded  ".into());
    assert_eq!(svc.create(named).unwrap().name.as_deref(), Some("padded"));
    let mut blank = input("p", every(1000), new_agent(&c));
    blank.name = Some("   ".into());
    assert_eq!(svc.create(blank).unwrap().name, None);
}

// ----- run once --------------------------------------------------------------

#[test]
fn run_once_records_a_run_without_changing_next_run_at_or_completing_the_schedule() {
    let (dir, clock, host) = setup();
    let svc = service(
        dir.path(),
        &clock,
        host,
        runner(|s, _| {
            Ok(ScheduleExecutionResult {
                agent_id: Some("00000000-0000-0000-0000-000000000099".into()),
                output: Some(format!("manual:{}", s.prompt)),
            })
        }),
    );
    let mut i = input("manual fire", cron("30 9 * * *"), new_agent(&cwd(&dir)));
    i.max_runs = Some(1);
    let created = svc.create(i).unwrap();
    assert_eq!(
        created.next_run_at.as_deref(),
        Some("2026-01-01T09:30:00.000Z")
    );
    let after = svc.run_once(&created.id).unwrap();
    assert_eq!(
        after.next_run_at.as_deref(),
        Some("2026-01-01T09:30:00.000Z")
    );
    assert_eq!(after.status, ScheduleStatus::Active);
    assert_eq!(after.runs.len(), 1);
    assert_eq!(after.runs[0].status, RunStatus::Succeeded);
    assert_eq!(
        after.runs[0].agent_id.as_deref(),
        Some("00000000-0000-0000-0000-000000000099")
    );
    assert_eq!(after.runs[0].output.as_deref(), Some("manual:manual fire"));
}

#[test]
fn run_once_rejects_completed_schedules() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let mut i = input("one-shot", every(60_000), new_agent(&cwd(&dir)));
    i.max_runs = Some(1);
    let created = svc.create(i).unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    let err = svc.run_once(&created.id).unwrap_err();
    assert!(err.to_string().contains("already completed"));
}

#[test]
fn run_once_rejects_a_schedule_that_is_already_running() {
    let (dir, clock, host) = setup();
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (started_tx, release_rx) = (Mutex::new(started_tx), Mutex::new(release_rx));
    let svc = service(
        dir.path(),
        &clock,
        host,
        runner(move |_, _| {
            started_tx.lock().unwrap().send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            Ok(ScheduleExecutionResult {
                agent_id: None,
                output: None,
            })
        }),
    );
    let created = svc
        .create(input("p", every(60_000), new_agent(&cwd(&dir))))
        .unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    std::thread::scope(|scope| {
        let tick = scope.spawn(|| svc.tick().unwrap());
        started_rx.recv().unwrap();
        assert_eq!(
            svc.run_once(&created.id).unwrap_err().to_string(),
            format!("Schedule {} is already running", created.id)
        );
        release_tx.send(()).unwrap();
        tick.join().unwrap();
    });
}

#[test]
fn run_once_completes_the_schedule_when_the_target_is_gone() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, None);
    let created = svc
        .create(input(
            "manual ping gone target",
            every(60_000),
            agent("13131313-1313-4131-8131-131313131313"),
        ))
        .unwrap();
    let after = svc.run_once(&created.id).unwrap();
    assert_eq!(after.status, ScheduleStatus::Completed);
    assert_eq!(after.next_run_at, None);
    assert_eq!(after.runs.len(), 1);
    assert_eq!(after.runs[0].status, RunStatus::Failed);
}

// ----- update ----------------------------------------------------------------

#[test]
fn update_mutates_cadence_prompt_name_and_target_fields_in_place() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let mut config = NewAgentConfig::new("claude", cwd(&dir));
    config.mode_id = Some("default".into());
    let mut i = input(
        "first prompt",
        every(60_000),
        ScheduleTarget::NewAgent { config },
    );
    i.name = Some("morning".into());
    let created = svc.create(i).unwrap();
    assert!(created.runs.is_empty());

    clock.set("2026-01-01T00:00:30.000Z");
    let next_cwd = dir.path().join("new-path");
    fs::create_dir_all(&next_cwd).unwrap();
    let next_cwd = next_cwd.to_str().unwrap().to_string();
    let updated = svc
        .update(UpdateScheduleInput {
            id: created.id.clone(),
            prompt: Some("second prompt".into()),
            name: Some(Some("renamed".into())),
            cadence: Some(every(5 * 60_000)),
            new_agent_config: Some(UpdateNewAgentConfig {
                provider: Some("codex".into()),
                model: Some(Some("gpt-5".into())),
                mode_id: Some(Some("full-access".into())),
                thinking_option_id: Some(Some("deep-thought".into())),
                archive_on_finish: Some(false),
                isolation: Some(Isolation::Worktree),
                cwd: Some(next_cwd.clone()),
            }),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(updated.prompt, "second prompt");
    assert_eq!(updated.name.as_deref(), Some("renamed"));
    assert_eq!(updated.cadence, every(5 * 60_000));
    let mut expected = NewAgentConfig::new("codex", next_cwd);
    expected.model = Some("gpt-5".into());
    expected.mode_id = Some("full-access".into());
    expected.thinking_option_id = Some("deep-thought".into());
    expected.archive_on_finish = Some(false);
    expected.isolation = Some(Isolation::Worktree);
    assert_eq!(
        updated.target,
        ScheduleTarget::NewAgent { config: expected }
    );
    assert_eq!(
        updated.next_run_at.as_deref(),
        Some("2026-01-01T00:05:30.000Z")
    );
    assert_eq!(updated.updated_at, "2026-01-01T00:00:30.000Z");
    assert_eq!(updated.created_at, created.created_at);
}

#[test]
fn update_switches_between_every_and_cron_cadences_and_recomputes_next_run_at() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let created = svc
        .create(input("p", every(60_000), new_agent(&cwd(&dir))))
        .unwrap();
    assert_eq!(
        created.next_run_at.as_deref(),
        Some("2026-01-01T00:00:00.000Z")
    );
    let c = svc
        .update(UpdateScheduleInput {
            id: created.id.clone(),
            cadence: Some(cron("30 9 * * *")),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(c.cadence, cron("30 9 * * *"));
    assert_eq!(c.next_run_at.as_deref(), Some("2026-01-01T09:30:00.000Z"));
    let back = svc
        .update(UpdateScheduleInput {
            id: created.id.clone(),
            cadence: Some(every(120_000)),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(back.cadence, every(120_000));
    assert_eq!(
        back.next_run_at.as_deref(),
        Some("2026-01-01T00:02:00.000Z")
    );
}

#[test]
fn update_preserves_a_cron_cadence_timezone_when_the_new_cadence_omits_it() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let tz = ScheduleCadence::Cron {
        expression: "0 9 * * *".into(),
        timezone: Some("America/New_York".into()),
    };
    let created = svc.create(input("p", tz, new_agent(&cwd(&dir)))).unwrap();
    let updated = svc
        .update(UpdateScheduleInput {
            id: created.id,
            cadence: Some(cron("30 9 * * *")),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        updated.cadence,
        ScheduleCadence::Cron {
            expression: "30 9 * * *".into(),
            timezone: Some("America/New_York".into())
        }
    );
}

#[test]
fn update_preserves_next_run_at_and_run_history_when_cadence_is_unchanged() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_with(None, "ran"));
    let created = svc
        .create(input("p", every(60_000), new_agent(&cwd(&dir))))
        .unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    let after = svc.inspect(&created.id).unwrap();
    assert_eq!(after.runs.len(), 1);
    clock.set("2026-01-01T00:01:30.000Z");
    let updated = svc
        .update(UpdateScheduleInput {
            id: created.id,
            prompt: Some("new prompt".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(updated.prompt, "new prompt");
    assert_eq!(updated.cadence, created.cadence);
    assert_eq!(updated.next_run_at, after.next_run_at);
    assert_eq!(updated.runs, after.runs);
    assert_eq!(updated.last_run_at, after.last_run_at);
}

#[test]
fn update_clears_the_schedule_name_when_given_an_empty_string() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let mut i = input("p", every(60_000), new_agent(&cwd(&dir)));
    i.name = Some("named".into());
    let created = svc.create(i).unwrap();
    assert_eq!(created.name.as_deref(), Some("named"));
    let cleared = svc
        .update(UpdateScheduleInput {
            id: created.id.clone(),
            name: Some(Some(String::new())),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(cleared.name, None);
    let renamed = svc
        .update(UpdateScheduleInput {
            id: created.id,
            name: Some(Some("again".into())),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(renamed.name.as_deref(), Some("again"));
}

#[test]
fn update_rejects_new_agent_fields_on_agent_target_schedules() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let created = svc
        .create(input(
            "agent target",
            every(60_000),
            agent("00000000-0000-0000-0000-000000000005"),
        ))
        .unwrap();
    let err = svc
        .update(UpdateScheduleInput {
            id: created.id,
            new_agent_config: Some(UpdateNewAgentConfig {
                provider: Some("codex".into()),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("only valid for new-agent target schedules"));
}

#[test]
fn update_changes_individual_new_agent_fields_independently() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let mut config = NewAgentConfig::new("claude", cwd(&dir));
    config.model = Some("sonnet".into());
    config.mode_id = Some("default".into());
    let created = svc
        .create(input(
            "p",
            every(60_000),
            ScheduleTarget::NewAgent { config },
        ))
        .unwrap();
    let mode_only = svc
        .update(UpdateScheduleInput {
            id: created.id.clone(),
            new_agent_config: Some(UpdateNewAgentConfig {
                mode_id: Some(Some("bypassPermissions".into())),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
    match &mode_only.target {
        ScheduleTarget::NewAgent { config } => {
            assert_eq!(config.provider, "claude");
            assert_eq!(config.model.as_deref(), Some("sonnet"));
            assert_eq!(config.mode_id.as_deref(), Some("bypassPermissions"));
        }
        _ => panic!("target type changed"),
    }
    let cleared = svc
        .update(UpdateScheduleInput {
            id: created.id,
            new_agent_config: Some(UpdateNewAgentConfig {
                model: Some(None),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
    match &cleared.target {
        ScheduleTarget::NewAgent { config } => {
            assert_eq!(config.model, None);
            assert_eq!(config.mode_id.as_deref(), Some("bypassPermissions"));
        }
        _ => panic!("target type changed"),
    }
}

#[test]
fn update_returns_a_schedule_that_round_trips_through_the_store() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let created = svc
        .create(input("p", every(60_000), new_agent(&cwd(&dir))))
        .unwrap();
    let returned = svc
        .update(UpdateScheduleInput {
            id: created.id.clone(),
            cadence: Some(cron("0 9 * * *")),
            new_agent_config: Some(UpdateNewAgentConfig {
                provider: Some("codex".into()),
                mode_id: Some(Some("full-access".into())),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
    let reloaded = svc.inspect(&created.id).unwrap();
    assert_eq!(reloaded, returned);
    assert_eq!(reloaded.cadence, cron("0 9 * * *"));
    let mut expected = NewAgentConfig::new("codex", cwd(&dir));
    expected.mode_id = Some("full-access".into());
    assert_eq!(
        reloaded.target,
        ScheduleTarget::NewAgent { config: expected }
    );
}

#[test]
fn update_rejects_empty_provider_and_non_positive_max_runs() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let created = svc
        .create(input("p", every(60_000), new_agent(&cwd(&dir))))
        .unwrap();
    let e = svc
        .update(UpdateScheduleInput {
            id: created.id.clone(),
            new_agent_config: Some(UpdateNewAgentConfig {
                provider: Some("  ".into()),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(e.to_string(), "provider cannot be empty");
    let e = svc
        .update(UpdateScheduleInput {
            id: created.id.clone(),
            max_runs: Some(Some(0)),
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(e.to_string(), "maxRuns must be a positive integer");
    let ok = svc
        .update(UpdateScheduleInput {
            id: created.id,
            max_runs: Some(Some(3)),
            expires_at: Some(Some("2027-01-01T00:00:00.000Z".into())),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(ok.max_runs, Some(3));
    assert_eq!(ok.expires_at.as_deref(), Some("2027-01-01T00:00:00.000Z"));
}

// ----- agent completion and recovery -----------------------------------------

#[test]
fn complete_for_agent_completes_only_schedules_targeting_that_agent() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let target_id = "11111111-1111-4111-8111-111111111111";
    let other_id = "22222222-2222-4222-8222-222222222222";
    let targeted = svc
        .create(input(
            "ping the doomed agent",
            every(60_000),
            agent(target_id),
        ))
        .unwrap();
    let other = svc
        .create(input(
            "ping the other agent",
            every(60_000),
            agent(other_id),
        ))
        .unwrap();
    let fresh = svc
        .create(input(
            "spawn a fresh agent",
            every(60_000),
            new_agent(&cwd(&dir)),
        ))
        .unwrap();
    clock.set("2026-01-01T00:05:00.000Z");
    assert_eq!(svc.complete_for_agent(target_id).unwrap(), 1);
    assert_eq!(svc.list().unwrap().len(), 3);
    let doomed = svc.inspect(&targeted.id).unwrap();
    assert_eq!(doomed.status, ScheduleStatus::Completed);
    assert_eq!(doomed.next_run_at, None);
    assert_eq!(doomed.updated_at, "2026-01-01T00:05:00.000Z");
    assert_eq!(
        svc.inspect(&other.id).unwrap().status,
        ScheduleStatus::Active
    );
    assert_eq!(
        svc.inspect(&fresh.id).unwrap().status,
        ScheduleStatus::Active
    );
}

#[test]
fn complete_for_agent_skips_schedules_that_are_already_completed() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let id = "11111111-1111-4111-8111-111111111111";
    let mut i = input("one shot", every(60_000), agent(id));
    i.max_runs = Some(1);
    let created = svc.create(i).unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    assert_eq!(
        svc.inspect(&created.id).unwrap().status,
        ScheduleStatus::Completed
    );
    let before = svc.inspect(&created.id).unwrap();
    assert_eq!(svc.complete_for_agent(id).unwrap(), 0);
    assert_eq!(svc.inspect(&created.id).unwrap(), before);
}

#[test]
fn startup_sweep_completes_agent_target_schedules_whose_agent_is_gone() {
    let (dir, clock, host) = setup();
    let missing_id = "44444444-4444-4444-8444-444444444444";
    let archived_id = "55555555-5555-4555-8555-555555555555";
    let live_id = "66666666-6666-4666-8666-666666666666";
    host.set_agent(archived_id, AgentTargetState::Archived);
    host.set_agent(live_id, AgentTargetState::Active);
    let svc1 = service(dir.path(), &clock, host.clone(), ok_runner());
    let missing = svc1
        .create(input("ping missing", every(60_000), agent(missing_id)))
        .unwrap();
    let archived = svc1
        .create(input("ping archived", every(60_000), agent(archived_id)))
        .unwrap();
    let live = svc1
        .create(input("ping live", every(60_000), agent(live_id)))
        .unwrap();
    let fresh = svc1
        .create(input("spawn fresh", every(60_000), new_agent(&cwd(&dir))))
        .unwrap();
    let paused_missing = svc1
        .create(input(
            "paused ping missing",
            every(60_000),
            agent("a1a1a1a1-a1a1-4a1a-8a1a-a1a1a1a1a1a1"),
        ))
        .unwrap();
    svc1.pause(&paused_missing.id).unwrap();
    let paused_live = svc1
        .create(input("paused ping live", every(60_000), agent(live_id)))
        .unwrap();
    svc1.pause(&paused_live.id).unwrap();

    clock.set("2026-01-01T00:10:00.000Z");
    let svc2 = service(dir.path(), &clock, host, ok_runner());
    svc2.start().unwrap();
    assert_eq!(
        svc2.inspect(&missing.id).unwrap().status,
        ScheduleStatus::Completed
    );
    assert_eq!(svc2.inspect(&missing.id).unwrap().next_run_at, None);
    assert_eq!(
        svc2.inspect(&archived.id).unwrap().status,
        ScheduleStatus::Completed
    );
    assert_eq!(
        svc2.inspect(&live.id).unwrap().status,
        ScheduleStatus::Active
    );
    assert_eq!(
        svc2.inspect(&fresh.id).unwrap().status,
        ScheduleStatus::Active
    );
    assert_eq!(
        svc2.inspect(&paused_missing.id).unwrap().status,
        ScheduleStatus::Completed
    );
    assert_eq!(
        svc2.inspect(&paused_live.id).unwrap().status,
        ScheduleStatus::Paused
    );
}

#[test]
fn advances_stale_next_run_at_on_daemon_restart() {
    let (dir, clock, host) = setup();
    let svc1 = service(dir.path(), &clock, host.clone(), ok_runner());
    let mut i = input("Periodic check", every(60_000), new_agent(&cwd(&dir)));
    i.run_on_create = Some(false);
    let created = svc1.create(i).unwrap();
    assert_eq!(
        created.next_run_at.as_deref(),
        Some("2026-01-01T00:01:00.000Z")
    );
    clock.set("2026-01-01T00:10:00.000Z");
    let svc2 = service(dir.path(), &clock, host, ok_runner());
    svc2.start().unwrap();
    let inspected = svc2.inspect(&created.id).unwrap();
    assert_eq!(
        inspected.next_run_at.as_deref(),
        Some("2026-01-01T00:11:00.000Z")
    );
}

#[test]
fn starts_with_the_valid_schedules_when_the_directory_holds_files_that_are_not_schedules() {
    let (dir, clock, host) = setup();
    let svc1 = service(dir.path(), &clock, host.clone(), ok_runner());
    let mut i = input("Still scheduled", every(60_000), new_agent(&cwd(&dir)));
    i.run_on_create = Some(false);
    let created = svc1.create(i).unwrap();

    let schedules = dir.path().join("schedules");
    fs::write(schedules.join("notes.json"), r#"{"hello":"world"}"#).unwrap();
    let mut value = serde_json::to_value(&created).unwrap();
    value.as_object_mut().unwrap().remove("lastRunAt");
    value["id"] = "deadbeef".into();
    fs::write(schedules.join("deadbeef.json"), value.to_string()).unwrap();
    fs::write(schedules.join("broken.json"), "{ not json").unwrap();

    let logger = collecting_logger();
    let svc2 = service_with_logger(dir.path(), &clock, host, ok_runner(), logger.clone());
    svc2.start().unwrap();
    let ids: Vec<_> = svc2.list().unwrap().into_iter().map(|s| s.id).collect();
    assert_eq!(ids, vec![created.id]);
    assert_eq!(logger.errors.lock().unwrap().len(), 3);
}

// ----- run outcomes ----------------------------------------------------------

#[test]
fn completes_the_schedule_when_a_scheduled_run_reports_the_target_is_gone() {
    let (dir, clock, host) = setup();
    let id = "77777777-7777-4777-8777-777777777777";
    let svc = service(
        dir.path(),
        &clock,
        host,
        runner(move |_, _| Err(ScheduleError::TargetGone(format!("Agent {id} is archived")))),
    );
    let created = svc
        .create(input("ping gone target", every(60_000), agent(id)))
        .unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    let inspected = svc.inspect(&created.id).unwrap();
    assert_eq!(inspected.status, ScheduleStatus::Completed);
    assert_eq!(inspected.next_run_at, None);
    assert_eq!(inspected.runs.len(), 1);
    assert_eq!(inspected.runs[0].status, RunStatus::Failed);
    assert_eq!(
        inspected.runs[0].error.as_deref(),
        Some(format!("Agent {id} is archived").as_str())
    );
}

#[test]
fn does_not_resurrect_next_run_at_when_the_schedule_completes_during_an_in_flight_run() {
    let (dir, clock, host) = setup();
    let agent_id = "ffffffff-ffff-4fff-8fff-ffffffffffff";
    let cell: Arc<OnceLock<Arc<ScheduleService>>> = Arc::new(OnceLock::new());
    let c2 = cell.clone();
    let svc = service(
        dir.path(),
        &clock,
        host,
        runner(move |_, _| {
            c2.get().unwrap().complete_for_agent(agent_id)?;
            Ok(ScheduleExecutionResult {
                agent_id: None,
                output: Some("ok".into()),
            })
        }),
    );
    cell.set(svc.clone()).ok();
    let created = svc
        .create(input("ping", every(60_000), agent(agent_id)))
        .unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    let inspected = svc.inspect(&created.id).unwrap();
    assert_eq!(inspected.status, ScheduleStatus::Completed);
    assert_eq!(inspected.next_run_at, None);
    assert_eq!(inspected.runs[0].status, RunStatus::Succeeded);
}

#[test]
fn keeps_the_schedule_active_when_a_run_fails_for_a_transient_reason() {
    let (dir, clock, host) = setup();
    let svc = service(
        dir.path(),
        &clock,
        host,
        runner(|_, _| Err(ScheduleError::msg("network blip"))),
    );
    let created = svc
        .create(input(
            "ping flaky target",
            every(60_000),
            agent("88888888-8888-4888-8888-888888888888"),
        ))
        .unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    let inspected = svc.inspect(&created.id).unwrap();
    assert_eq!(inspected.status, ScheduleStatus::Active);
    assert_eq!(
        inspected.next_run_at.as_deref(),
        Some("2026-01-01T00:02:00.000Z")
    );
    assert_eq!(inspected.runs[0].status, RunStatus::Failed);
    assert_eq!(inspected.runs[0].error.as_deref(), Some("network blip"));
}

#[cfg(unix)]
#[test]
fn fires_again_after_a_tick_whose_run_could_not_be_recorded() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_with(None, "ran"));
    let created = svc
        .create(input(
            "ping target",
            every(60_000),
            agent("88888888-8888-4888-8888-888888888888"),
        ))
        .unwrap();
    let schedules_dir = dir.path().join("schedules");
    clock.set("2026-01-01T00:01:00.000Z");
    fs::set_permissions(&schedules_dir, fs::Permissions::from_mode(0o555)).unwrap();
    let first = svc.tick();
    fs::set_permissions(&schedules_dir, fs::Permissions::from_mode(0o755)).unwrap();
    if first.is_ok() {
        return; // root: permissions did not apply
    }
    svc.tick().unwrap();
    let inspected = svc.inspect(&created.id).unwrap();
    assert_eq!(inspected.runs.len(), 1);
    assert_eq!(inspected.runs[0].status, RunStatus::Succeeded);
    assert_eq!(
        inspected.next_run_at.as_deref(),
        Some("2026-01-01T00:02:00.000Z")
    );
}

// ----- host-backed execution -------------------------------------------------

#[test]
fn completes_the_schedule_when_a_scheduled_run_targets_an_archived_agent() {
    let (dir, clock, host) = setup();
    let id = "99999999-9999-4999-8999-999999999999";
    host.set_agent(id, AgentTargetState::Archived);
    let svc = service(dir.path(), &clock, host, None);
    let created = svc
        .create(input("ping archived target", every(60_000), agent(id)))
        .unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    let inspected = svc.inspect(&created.id).unwrap();
    assert_eq!(inspected.status, ScheduleStatus::Completed);
    assert_eq!(inspected.next_run_at, None);
    assert_eq!(inspected.runs[0].status, RunStatus::Failed);
    assert!(inspected.runs[0]
        .error
        .as_deref()
        .unwrap()
        .contains("is archived"));
}

#[test]
fn completes_the_schedule_when_a_scheduled_run_targets_a_missing_agent() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, None);
    let created = svc
        .create(input(
            "ping missing target",
            every(60_000),
            agent("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
        ))
        .unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    let inspected = svc.inspect(&created.id).unwrap();
    assert_eq!(inspected.status, ScheduleStatus::Completed);
    assert_eq!(inspected.runs[0].status, RunStatus::Failed);
    assert!(inspected.runs[0]
        .error
        .as_deref()
        .unwrap()
        .contains("no longer exists"));
}

#[test]
fn completes_the_schedule_when_a_new_agent_runs_cwd_no_longer_exists() {
    let (dir, clock, host) = setup();
    let gone = dir.path().join("deleted-worktree");
    fs::create_dir_all(&gone).unwrap();
    let svc = service(dir.path(), &clock, host.clone(), None);
    let created = svc
        .create(input(
            "spawn in a deleted dir",
            every(60_000),
            new_agent(gone.to_str().unwrap()),
        ))
        .unwrap();
    fs::remove_dir_all(&gone).unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    let inspected = svc.inspect(&created.id).unwrap();
    assert_eq!(inspected.status, ScheduleStatus::Completed);
    assert_eq!(inspected.next_run_at, None);
    assert_eq!(inspected.runs[0].status, RunStatus::Failed);
    assert!(inspected.runs[0]
        .error
        .as_deref()
        .unwrap()
        .contains("no longer exists"));
    assert!(host.state.lock().unwrap().workspaces.is_empty());
}

#[test]
fn new_agent_cwd_existence_is_checked_at_run_time_not_when_editing_the_schedule() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, None);
    let missing = dir.path().join("not-yet");
    let created = svc
        .create(input("p", every(60_000), new_agent(&cwd(&dir))))
        .unwrap();
    let updated = svc
        .update(UpdateScheduleInput {
            id: created.id,
            new_agent_config: Some(UpdateNewAgentConfig {
                cwd: Some(missing.to_str().unwrap().into()),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
    assert!(matches!(updated.target, ScheduleTarget::NewAgent { .. }));
}

#[test]
fn agent_target_run_delivers_a_system_notification_prompt_and_records_output() {
    let (dir, clock, host) = setup();
    let id = "12121212-1212-4121-8121-121212121212";
    host.set_agent(id, AgentTargetState::Active);
    host.state.lock().unwrap().existing_outcome = AgentWaitOutcome {
        last_message: Some("  all good  ".into()),
        ..Default::default()
    };
    let svc = service(dir.path(), &clock, host.clone(), None);
    let mut i = input("check the build", every(60_000), agent(id));
    i.name = Some("babysit".into());
    let created = svc.create(i).unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    let inspected = svc.inspect(&created.id).unwrap();
    let run = &inspected.runs[0];
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(run.agent_id.as_deref(), Some(id));
    assert_eq!(run.output.as_deref(), Some("all good"));
    let delivered = host.state.lock().unwrap().delivered.clone();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, id);
    assert_eq!(
        delivered[0].1,
        format!(
            "<paseo-system>\nSchedule \"babysit\" fired (id={}, run={}).\ncheck the build\n</paseo-system>",
            created.id, run.id
        )
    );
}

#[test]
fn agent_target_run_fails_when_the_agent_errors_or_waits_for_permission() {
    let (dir, clock, host) = setup();
    let id = "12121212-1212-4121-8121-121212121212";
    host.set_agent(id, AgentTargetState::Active);
    host.state.lock().unwrap().existing_outcome = AgentWaitOutcome {
        permission_pending: true,
        ..Default::default()
    };
    let svc = service(dir.path(), &clock, host.clone(), None);
    let created = svc.create(input("p", every(60_000), agent(id))).unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    let run = &svc.inspect(&created.id).unwrap().runs[0];
    assert_eq!(
        run.error.as_deref(),
        Some(format!("Scheduled agent {id} is waiting for permission").as_str())
    );

    host.state.lock().unwrap().existing_outcome = AgentWaitOutcome {
        errored: true,
        last_message: Some("boom".into()),
        ..Default::default()
    };
    clock.set("2026-01-01T00:02:00.000Z");
    svc.tick().unwrap();
    let inspected = svc.inspect(&created.id).unwrap();
    assert_eq!(inspected.runs[1].error.as_deref(), Some("boom"));
    assert_eq!(inspected.status, ScheduleStatus::Active);
}

fn new_agent_with(dir: &tempfile::TempDir, f: impl FnOnce(&mut NewAgentConfig)) -> ScheduleTarget {
    let mut config = NewAgentConfig::new("claude", cwd(dir));
    f(&mut config);
    ScheduleTarget::NewAgent { config }
}

fn tick_once(
    dir: &tempfile::TempDir,
    clock: &TestClock,
    host: &Arc<FakeHost>,
    target: ScheduleTarget,
) -> (Arc<ScheduleService>, StoredSchedule) {
    let svc = service(dir.path(), clock, host.clone(), None);
    let created = svc
        .create(input("do the thing", every(60_000), target))
        .unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    let inspected = svc.inspect(&created.id).unwrap();
    (svc, inspected)
}

#[test]
fn new_agent_run_goes_through_the_host_and_archives_the_workspace_by_default() {
    let (dir, clock, host) = setup();
    host.state.lock().unwrap().run_outcome = AgentWaitOutcome {
        last_message: Some("waited".into()),
        ..Default::default()
    };
    let target = new_agent_with(&dir, |c| {
        c.model = Some("sonnet".into());
        c.mode_id = Some("auto".into());
        c.isolation = Some(Isolation::Worktree);
    });
    let (_svc, s) = tick_once(&dir, &clock, &host, target);
    let run = &s.runs[0];
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(
        run.agent_id.as_deref(),
        Some("00000000-0000-0000-0000-0000000000aa")
    );
    assert_eq!(run.workspace(), Some("ws-1"));
    assert_eq!(run.output.as_deref(), Some("waited"));
    let st = host.state.lock().unwrap();
    assert_eq!(st.workspaces, vec![(cwd(&dir), Isolation::Worktree)]);
    assert_eq!(st.archived, vec!["ws-1".to_string()]);
    let created = &st.created[0];
    assert_eq!(created.provider, "claude/sonnet");
    assert_eq!(created.config.mode_id.as_deref(), Some("auto"));
    assert_eq!(created.prompt, "do the thing");
    assert_eq!(
        created.labels,
        vec![
            ("paseo.schedule-id".to_string(), s.id.clone()),
            ("paseo.schedule-run".to_string(), run.id.clone())
        ]
    );
}

#[test]
fn archive_on_finish_false_keeps_the_run_workspace_when_an_agent_exists() {
    let (dir, clock, host) = setup();
    let target = new_agent_with(&dir, |c| c.archive_on_finish = Some(false));
    let (_svc, s) = tick_once(&dir, &clock, &host, target);
    assert_eq!(s.runs[0].status, RunStatus::Succeeded);
    assert!(host.state.lock().unwrap().archived.is_empty());
}

#[test]
fn archives_the_run_workspace_when_agent_creation_fails_even_with_archive_on_finish_false() {
    let (dir, clock, host) = setup();
    host.state.lock().unwrap().fail_create_agent = Some("provider unavailable".into());
    let target = new_agent_with(&dir, |c| c.archive_on_finish = Some(false));
    let (_svc, s) = tick_once(&dir, &clock, &host, target);
    assert_eq!(s.runs[0].status, RunStatus::Failed);
    assert_eq!(s.runs[0].error.as_deref(), Some("provider unavailable"));
    assert_eq!(s.runs[0].workspace(), Some("ws-1"));
    assert_eq!(
        host.state.lock().unwrap().archived,
        vec!["ws-1".to_string()]
    );
    assert_eq!(s.status, ScheduleStatus::Active);
}

#[test]
fn records_prompt_start_failures_as_failed_and_keeps_the_agent_association() {
    let (dir, clock, host) = setup();
    host.state.lock().unwrap().initial_prompt_error = Some("prompt rejected".into());
    let (_svc, s) = tick_once(&dir, &clock, &host, new_agent_with(&dir, |_| {}));
    assert_eq!(s.runs[0].status, RunStatus::Failed);
    assert_eq!(s.runs[0].error.as_deref(), Some("prompt rejected"));
    assert_eq!(
        s.runs[0].agent_id.as_deref(),
        Some("00000000-0000-0000-0000-0000000000aa")
    );
    assert_eq!(
        host.state.lock().unwrap().archived,
        vec!["ws-1".to_string()]
    );
}

#[test]
fn scheduled_new_agent_cancellations_fail_the_run() {
    let (dir, clock, host) = setup();
    host.state.lock().unwrap().run_result.canceled = true;
    let (_svc, s) = tick_once(&dir, &clock, &host, new_agent_with(&dir, |_| {}));
    assert_eq!(s.runs[0].status, RunStatus::Failed);
    assert_eq!(
        s.runs[0].error.as_deref(),
        Some("Scheduled agent 00000000-0000-0000-0000-0000000000aa was canceled")
    );
}

#[test]
fn failed_new_agent_run_keeps_the_run_error_when_workspace_archive_also_fails() {
    let (dir, clock, host) = setup();
    {
        let mut st = host.state.lock().unwrap();
        st.run_outcome = AgentWaitOutcome {
            errored: true,
            last_message: Some("agent crashed".into()),
            ..Default::default()
        };
        st.fail_archive = true;
    }
    let (_svc, s) = tick_once(&dir, &clock, &host, new_agent_with(&dir, |_| {}));
    assert_eq!(s.runs[0].status, RunStatus::Failed);
    assert_eq!(s.runs[0].error.as_deref(), Some("agent crashed"));
}

#[test]
fn new_agent_run_output_falls_back_to_final_text_then_curated_timeline() {
    let (dir, clock, host) = setup();
    host.state.lock().unwrap().run_result = AgentRunResult {
        canceled: false,
        final_text: "  final answer \n".into(),
        timeline_text: "timeline".into(),
    };
    let (_svc, s) = tick_once(&dir, &clock, &host, new_agent_with(&dir, |_| {}));
    assert_eq!(s.runs[0].output.as_deref(), Some("final answer"));

    let (dir2, clock2, host2) = setup();
    host2.state.lock().unwrap().run_result = AgentRunResult {
        canceled: false,
        final_text: "   ".into(),
        timeline_text: " from timeline ".into(),
    };
    let (_svc2, s2) = tick_once(&dir2, &clock2, &host2, new_agent_with(&dir2, |_| {}));
    assert_eq!(s2.runs[0].output.as_deref(), Some("from timeline"));
}

#[test]
fn startup_recovery_archives_an_interrupted_run_workspace_with_an_associated_agent() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host.clone(), ok_runner());
    let created = svc
        .create(input("p", every(60_000), new_agent(&cwd(&dir))))
        .unwrap();
    svc.store()
        .update(&created.id, |s| {
            let mut n = s.clone();
            let mut run = interrupted_run("run-1", "2026-01-01T00:00:00.000Z");
            run.workspace_id = Some(Some("ws-77".into()));
            run.agent_id = Some("00000000-0000-0000-0000-0000000000aa".into());
            n.runs = vec![run];
            Ok(Some(n))
        })
        .unwrap();
    clock.set("2026-01-01T00:05:00.000Z");
    svc.start().unwrap();
    let after = svc.inspect(&created.id).unwrap();
    assert_eq!(after.runs[0].status, RunStatus::Failed);
    assert_eq!(
        host.state.lock().unwrap().archived,
        vec!["ws-77".to_string()]
    );
}

#[test]
fn startup_recovery_archives_an_interrupted_run_workspace_before_agent_association() {
    let (dir, clock, host) = setup();
    let target = new_agent_with(&dir, |c| c.archive_on_finish = Some(false));
    let svc = service(dir.path(), &clock, host.clone(), ok_runner());
    let created = svc.create(input("p", every(60_000), target)).unwrap();
    svc.store()
        .update(&created.id, |s| {
            let mut n = s.clone();
            let mut run = interrupted_run("run-1", "2026-01-01T00:00:00.000Z");
            run.workspace_id = Some(Some("ws-78".into()));
            n.runs = vec![run];
            Ok(Some(n))
        })
        .unwrap();
    svc.start().unwrap();
    // archiveOnFinish=false does not protect a workspace that never got an agent.
    assert_eq!(
        host.state.lock().unwrap().archived,
        vec!["ws-78".to_string()]
    );
}

#[test]
fn startup_recovery_keeps_the_workspace_when_archive_on_finish_is_false_and_an_agent_exists() {
    let (dir, clock, host) = setup();
    let target = new_agent_with(&dir, |c| c.archive_on_finish = Some(false));
    let svc = service(dir.path(), &clock, host.clone(), ok_runner());
    let created = svc.create(input("p", every(60_000), target)).unwrap();
    svc.store()
        .update(&created.id, |s| {
            let mut n = s.clone();
            let mut run = interrupted_run("run-1", "2026-01-01T00:00:00.000Z");
            run.workspace_id = Some(Some("ws-79".into()));
            run.agent_id = Some("00000000-0000-0000-0000-0000000000aa".into());
            n.runs = vec![run];
            Ok(Some(n))
        })
        .unwrap();
    svc.start().unwrap();
    assert!(host.state.lock().unwrap().archived.is_empty());
}

// ----- createOrReplace -------------------------------------------------------

fn named(
    name: &str,
    prompt: &str,
    cadence: ScheduleCadence,
    target: ScheduleTarget,
) -> CreateScheduleInput {
    let mut i = input(prompt, cadence, target);
    i.name = Some(name.into());
    i
}

#[test]
fn create_or_replace_updates_the_matching_schedule_in_place_instead_of_duplicating() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let agent_id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let first = svc
        .create_or_replace(named(
            "babysit-pr PR 1112",
            "watch the build",
            every(60_000),
            agent(agent_id),
        ))
        .unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    assert_eq!(svc.inspect(&first.id).unwrap().runs.len(), 1);
    svc.pause(&first.id).unwrap();
    clock.set("2026-01-01T00:02:00.000Z");
    let second = svc
        .create_or_replace(named(
            "babysit-pr PR 1112",
            "watch the build v2",
            cron("30 9 * * *"),
            agent(agent_id),
        ))
        .unwrap();
    assert_eq!(second.id, first.id);
    assert_eq!(second.status, ScheduleStatus::Active);
    assert_eq!(second.prompt, "watch the build v2");
    assert_eq!(second.cadence, cron("30 9 * * *"));
    assert_eq!(
        second.next_run_at.as_deref(),
        Some("2026-01-01T09:30:00.000Z")
    );
    assert_eq!(second.created_at, first.created_at);
    assert_eq!(second.runs.len(), 1);
    assert_eq!(svc.list().unwrap().len(), 1);
}

#[test]
fn create_or_replace_preserves_an_existing_cron_timezone_when_replacement_omits_timezone() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let agent_id = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let tz = ScheduleCadence::Cron {
        expression: "0 9 * * *".into(),
        timezone: Some("America/New_York".into()),
    };
    let first = svc
        .create_or_replace(named("timezone-aware cron", "v1", tz, agent(agent_id)))
        .unwrap();
    let second = svc
        .create_or_replace(named(
            "timezone-aware cron",
            "v2",
            cron("30 9 * * *"),
            agent(agent_id),
        ))
        .unwrap();
    assert_eq!(second.id, first.id);
    assert_eq!(
        second.cadence,
        ScheduleCadence::Cron {
            expression: "30 9 * * *".into(),
            timezone: Some("America/New_York".into())
        }
    );
    assert_eq!(svc.list().unwrap().len(), 1);
}

#[test]
fn create_or_replace_creates_a_sibling_when_name_target_or_completion_differ() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let a = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let b = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    svc.create_or_replace(named("dup", "p", every(60_000), agent(a)))
        .unwrap();
    svc.create_or_replace(named("other", "p", every(60_000), agent(a)))
        .unwrap();
    svc.create_or_replace(named("dup", "p", every(60_000), agent(b)))
        .unwrap();
    let mut done_input = named("done", "p", every(60_000), agent(a));
    done_input.max_runs = Some(1);
    let done = svc.create_or_replace(done_input).unwrap();
    clock.set("2026-01-01T00:01:00.000Z");
    svc.tick().unwrap();
    assert_eq!(
        svc.inspect(&done.id).unwrap().status,
        ScheduleStatus::Completed
    );
    let redone = svc
        .create_or_replace(named("done", "p", every(60_000), agent(a)))
        .unwrap();
    assert_ne!(redone.id, done.id);
    assert_eq!(svc.list().unwrap().len(), 5);
}

#[test]
fn create_or_replace_never_dedups_anonymous_schedules() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let id = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    svc.create_or_replace(input("p", every(60_000), agent(id)))
        .unwrap();
    svc.create_or_replace(input("p", every(60_000), agent(id)))
        .unwrap();
    assert_eq!(svc.list().unwrap().len(), 2);
}

#[test]
fn create_or_replace_matches_new_agent_targets_by_config() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let first = svc
        .create_or_replace(named(
            "nightly",
            "audit",
            every(60_000),
            new_agent(&cwd(&dir)),
        ))
        .unwrap();
    let second = svc
        .create_or_replace(named(
            "nightly",
            "audit v2",
            every(60_000),
            new_agent(&cwd(&dir)),
        ))
        .unwrap();
    assert_eq!(second.id, first.id);
    assert_eq!(svc.list().unwrap().len(), 1);
    let sub = dir.path().join("sub");
    fs::create_dir_all(&sub).unwrap();
    let third = svc
        .create_or_replace(named(
            "nightly",
            "audit elsewhere",
            every(60_000),
            new_agent(sub.to_str().unwrap()),
        ))
        .unwrap();
    assert_ne!(third.id, first.id);
    assert_eq!(svc.list().unwrap().len(), 2);
}

#[test]
fn concurrent_create_or_replace_first_creates_share_one_schedule() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let c = cwd(&dir);
    let (a, b) = std::thread::scope(|scope| {
        let t1 = scope.spawn(|| {
            svc.create_or_replace(named("nightly race", "audit", every(60_000), new_agent(&c)))
                .unwrap()
        });
        let t2 = scope.spawn(|| {
            svc.create_or_replace(named("nightly race", "audit", every(60_000), new_agent(&c)))
                .unwrap()
        });
        (t1.join().unwrap(), t2.join().unwrap())
    });
    assert_eq!(a.id, b.id);
    assert_eq!(svc.list().unwrap().len(), 1);
}

#[test]
fn create_or_replace_dedups_new_agent_targets_regardless_of_config_key_order() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let mut options_a = JsonMap::new();
    options_a.insert("alpha".into(), 1.into());
    options_a.insert("beta".into(), serde_json::json!({"x": 1, "y": 2}));
    let mut options_b = JsonMap::new();
    options_b.insert("beta".into(), serde_json::json!({"y": 2, "x": 1}));
    options_b.insert("alpha".into(), 1.into());
    let target_a = new_agent_with(&dir, |c| c.provider_options = Some(options_a));
    let target_b = new_agent_with(&dir, |c| c.provider_options = Some(options_b));
    let first = svc
        .create_or_replace(named("keyed", "p", every(60_000), target_a))
        .unwrap();
    let second = svc
        .create_or_replace(named("keyed", "p", every(60_000), target_b))
        .unwrap();
    assert_eq!(second.id, first.id);
    assert_eq!(svc.list().unwrap().len(), 1);
}

// ----- logs / delete ---------------------------------------------------------

#[test]
fn logs_returns_runs_ordered_by_started_at_and_delete_removes_the_file() {
    let (dir, clock, host) = setup();
    let svc = service(dir.path(), &clock, host, ok_runner());
    let created = svc
        .create(input("p", every(60_000), new_agent(&cwd(&dir))))
        .unwrap();
    for t in ["2026-01-01T00:01:00.000Z", "2026-01-01T00:02:00.000Z"] {
        clock.set(t);
        svc.tick().unwrap();
    }
    let runs = svc.logs(&created.id).unwrap();
    assert_eq!(runs.len(), 2);
    assert!(runs[0].started_at < runs[1].started_at);
    svc.delete(&created.id).unwrap();
    assert_eq!(
        svc.inspect(&created.id).unwrap_err().to_string(),
        format!("Schedule not found: {}", created.id)
    );
}

#[test]
fn ticker_does_not_block_other_schedules_on_a_slow_run() {
    let (dir, clock, host) = setup();
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (fast_tx, fast_rx) = mpsc::channel::<()>();
    let started_tx = Mutex::new(started_tx);
    let release_rx = Mutex::new(release_rx);
    let fast_tx = Mutex::new(fast_tx);
    let svc = Arc::new(service(
        dir.path(),
        &clock,
        host,
        runner(move |s, _| {
            if s.prompt == "slow" {
                started_tx.lock().unwrap().send(()).unwrap();
                let _ = release_rx.lock().unwrap().recv();
            } else {
                let _ = fast_tx.lock().unwrap().send(());
            }
            Ok(ScheduleExecutionResult {
                agent_id: None,
                output: Some("ok".into()),
            })
        }),
    ));
    let cadence = ScheduleCadence::Every {
        every_ms: 3_600_000,
    };
    svc.create(input("slow", cadence.clone(), new_agent(&cwd(&dir))))
        .unwrap();
    svc.create(input("fast", cadence, new_agent(&cwd(&dir))))
        .unwrap();
    let ticker = svc.spawn_ticker();
    let timeout = std::time::Duration::from_secs(10);
    started_rx.recv_timeout(timeout).unwrap();
    fast_rx.recv_timeout(timeout).unwrap();
    // Stopping must not wait for the in-flight slow run.
    ticker.stop();
    release_tx.send(()).unwrap();
}
