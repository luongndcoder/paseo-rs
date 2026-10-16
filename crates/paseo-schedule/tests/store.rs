mod common;

use std::fs;
use std::sync::mpsc;
use std::sync::Arc;

use common::*;
use paseo_schedule::store::ScheduleStore;
use paseo_schedule::types::*;

const TARGET_AGENT: &str = "00000000-0000-0000-0000-000000000001";

fn store(dir: &std::path::Path) -> ScheduleStore {
    ScheduleStore::new(dir, Arc::new(paseo_schedule::logger::NoopLogger))
}

#[test]
fn creates_and_reloads_schedules_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let created = store(dir.path())
        .create(base_schedule(Some("Morning summary"), new_agent(cwd)))
        .unwrap();
    assert_eq!(created.id.len(), 8);
    let listed = store(dir.path()).list().unwrap();
    assert_eq!(listed, vec![created]);
}

#[test]
fn reports_an_invalid_schedule_file_once_while_it_stays_invalid_and_lists_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap().to_string();
    let created = store(dir.path())
        .create(base_schedule(Some("Morning summary"), new_agent(&cwd)))
        .unwrap();
    fs::write(dir.path().join("notes.json"), r#"{"hello":"world"}"#).unwrap();

    let logger = collecting_logger();
    let reloaded = ScheduleStore::new(dir.path(), logger.clone());
    assert_eq!(reloaded.list().unwrap(), vec![created.clone()]);
    assert_eq!(reloaded.list().unwrap(), vec![created.clone()]);
    fs::remove_file(dir.path().join("notes.json")).unwrap();
    assert_eq!(reloaded.list().unwrap(), vec![created.clone()]);
    fs::write(dir.path().join("notes.json"), "{ not json").unwrap();
    assert_eq!(reloaded.list().unwrap(), vec![created]);

    let errors = logger.errors.lock().unwrap();
    assert_eq!(errors.len(), 2);
    let notes = dir.path().join("notes.json");
    assert!(errors
        .iter()
        .all(|e| e.starts_with("Skipping invalid schedule file")
            && e.contains(notes.to_str().unwrap())));
}

#[cfg(unix)]
#[test]
fn fails_the_listing_when_a_schedule_file_cannot_be_read() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("unreadable.json");
    fs::write(&file, "{}").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&file).is_ok() {
        return; // running as root: permissions do not apply
    }
    let result = store(dir.path()).list();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(result.is_err());
}

#[test]
fn update_round_trips_an_updated_schedule_to_disk() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let s = store(dir.path());
    let created = s
        .create(base_schedule(Some("before"), new_agent(cwd)))
        .unwrap();
    let mut updated = created.clone();
    updated.name = Some("after".into());
    updated.prompt = "after".into();
    updated.cadence = cron("0 9 * * *");
    let mut config = NewAgentConfig::new("codex", "/elsewhere");
    config.mode_id = Some("full-access".into());
    updated.target = ScheduleTarget::NewAgent { config };
    updated.next_run_at = Some("2026-01-01T09:00:00.000Z".into());
    updated.updated_at = "2026-01-01T00:00:30.000Z".into();
    let expected = updated.clone();
    s.update(&created.id, move |_| Ok(Some(updated))).unwrap();
    let reloaded = store(dir.path()).get(&created.id).unwrap();
    assert_eq!(reloaded, Some(expected));
}

#[test]
fn deletes_schedules_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let s = store(dir.path());
    let created = s.create(base_schedule(None, new_agent(cwd))).unwrap();
    s.delete(&created.id).unwrap();
    assert_eq!(s.get(&created.id).unwrap(), None);
    assert!(s.list().unwrap().is_empty());
}

#[test]
fn update_rejects_an_id_change_and_missing_schedules_yield_none() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let s = store(dir.path());
    let created = s.create(base_schedule(None, new_agent(cwd))).unwrap();
    let err = s
        .update(&created.id, |c| {
            let mut n = c.clone();
            n.id = "other".into();
            Ok(Some(n))
        })
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        format!("Schedule update cannot change id: {}", created.id)
    );
    assert_eq!(s.update("missing1", |_| Ok(None)).unwrap(), None);
}

#[test]
fn path_traversal_ids_are_treated_as_missing() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(&dir.path().join("schedules"));
    assert_eq!(s.get("../secret").unwrap(), None);
}

#[test]
fn serializes_concurrent_updates_on_one_schedule_without_losing_writes() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let s = Arc::new(store(dir.path()));
    let created = s
        .create(base_schedule(Some("before"), new_agent(cwd)))
        .unwrap();

    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let id = created.id.clone();

    let first = {
        let s = s.clone();
        let id = id.clone();
        std::thread::spawn(move || {
            s.update(&id, |schedule| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                let mut next = schedule.clone();
                next.runs.push(ScheduleRun {
                    id: "run-1".into(),
                    scheduled_for: "2026-01-01T00:01:00.000Z".into(),
                    started_at: "2026-01-01T00:01:00.000Z".into(),
                    ended_at: None,
                    status: RunStatus::Running,
                    agent_id: None,
                    workspace_id: None,
                    output: None,
                    error: None,
                });
                Ok(Some(next))
            })
            .unwrap()
        })
    };
    entered_rx.recv().unwrap();

    let (saw_tx, saw_rx) = mpsc::channel::<usize>();
    let second = {
        let s = s.clone();
        let id = id.clone();
        std::thread::spawn(move || {
            s.update(&id, |schedule| {
                saw_tx.send(schedule.runs.len()).unwrap();
                let mut next = schedule.clone();
                next.prompt = "after".into();
                Ok(Some(next))
            })
            .unwrap()
        })
    };
    // The second updater cannot start while the first holds the schedule lock.
    assert!(saw_rx
        .recv_timeout(std::time::Duration::from_millis(150))
        .is_err());
    release_tx.send(()).unwrap();
    first.join().unwrap();
    let second = second.join().unwrap().unwrap();
    assert_eq!(saw_rx.recv().unwrap(), 1);
    assert_eq!(second.prompt, "after");
    assert_eq!(second.runs[0].id, "run-1");
    let reloaded = store(dir.path()).get(&id).unwrap().unwrap();
    assert_eq!(reloaded.prompt, "after");
    assert_eq!(reloaded.runs[0].id, "run-1");
}

#[test]
fn revalidates_a_named_target_match_after_waiting_for_the_schedule_update_queue() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap().to_string();
    let s = Arc::new(store(dir.path()));
    let target = new_agent(&cwd);
    let created = s
        .create(base_schedule(Some("race"), target.clone()))
        .unwrap();

    // Completion holds the schedule lock while the upsert has already matched the schedule.
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let completer = {
        let s = s.clone();
        let id = created.id.clone();
        std::thread::spawn(move || {
            s.update(&id, |schedule| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                let mut next = schedule.clone();
                next.status = ScheduleStatus::Completed;
                next.next_run_at = None;
                next.updated_at = "2026-01-01T00:00:30.000Z".into();
                Ok(Some(next))
            })
            .unwrap()
        })
    };
    entered_rx.recv().unwrap();

    let upsert = {
        let s = s.clone();
        let target = target.clone();
        std::thread::spawn(move || {
            let mut fresh = base_schedule(Some("race"), target.clone());
            fresh.prompt = "after".into();
            s.upsert_by_name_and_target(
                "race",
                &target,
                || Ok(fresh.clone()),
                |_| panic!("stale identity match should not update"),
            )
            .unwrap()
        })
    };
    // Let the upsert list the (still active) schedule and block on its lock.
    std::thread::sleep(std::time::Duration::from_millis(200));
    release_tx.send(()).unwrap();
    completer.join().unwrap();
    let upserted = upsert.join().unwrap();

    // The upsert either saw the schedule already completed (create path) or
    // re-checked the match under the lock and looped into create. Never an update.
    assert_ne!(upserted.id, created.id);
    assert_eq!(upserted.prompt, "after");
    assert_eq!(upserted.status, ScheduleStatus::Active);
    let old = s.get(&created.id).unwrap().unwrap();
    assert_eq!(old.status, ScheduleStatus::Completed);
    assert_eq!(old.prompt, "p");
    assert_eq!(s.list().unwrap().len(), 2);
}

#[test]
fn stored_json_matches_the_ts_pretty_format_and_key_order() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let created = s
        .create(base_schedule(
            None,
            ScheduleTarget::Agent {
                agent_id: TARGET_AGENT.into(),
            },
        ))
        .unwrap();
    let text = fs::read_to_string(dir.path().join(format!("{}.json", created.id))).unwrap();
    let expected = format!(
        "{{\n  \"id\": \"{}\",\n  \"name\": null,\n  \"prompt\": \"p\",\n  \"cadence\": {{\n    \"type\": \"every\",\n    \"everyMs\": 60000\n  }},\n  \"target\": {{\n    \"type\": \"agent\",\n    \"agentId\": \"{TARGET_AGENT}\"\n  }},\n  \"status\": \"active\",\n  \"createdAt\": \"2026-01-01T00:00:00.000Z\",\n  \"updatedAt\": \"2026-01-01T00:00:00.000Z\",\n  \"nextRunAt\": \"2026-01-01T00:01:00.000Z\",\n  \"lastRunAt\": null,\n  \"pausedAt\": null,\n  \"expiresAt\": null,\n  \"maxRuns\": null,\n  \"runs\": []\n}}",
        created.id
    );
    assert_eq!(text, expected);
}
