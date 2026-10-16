mod common;

use std::sync::Arc;

use common::*;
use paseo_schedule::logger::NoopLogger;
use paseo_schedule::rpc::*;
use paseo_schedule::types::*;
use serde_json::{json, Value};

const AGENT: &str = "00000000-0000-0000-0000-000000000009";

fn session() -> (tempfile::TempDir, TestClock, ScheduleSession) {
    let (dir, clock, host) = (
        tempfile::tempdir().unwrap(),
        TestClock::new("2026-01-01T00:00:00.000Z"),
        FakeHost::new(),
    );
    let svc = service(dir.path(), &clock, host, ok_runner());
    (dir, clock, ScheduleSession::new(svc, Arc::new(NoopLogger)))
}

fn parse(value: Value) -> ScheduleRequest {
    ScheduleRequest::from_value(value).unwrap()
}

fn wire(outbound: &ScheduleOutbound) -> Value {
    serde_json::to_value(outbound).unwrap()
}

// --- protocol/rpc-schemas.test.ts ---

#[test]
fn round_trips_new_agent_run_options_on_create_requests() {
    let request = parse(json!({
        "type": "schedule/create",
        "requestId": "request-1",
        "prompt": "Run the task",
        "cadence": {"type": "every", "everyMs": 60000},
        "target": {"type": "new-agent", "config": {
            "provider": "claude", "cwd": "/tmp/project", "thinkingOptionId": "think-hard",
            "archiveOnFinish": false, "isolation": "worktree"
        }}
    }));
    match request {
        ScheduleRequest::Create(r) => {
            assert_eq!(r.request_id, "request-1");
            assert_eq!(r.cadence, every(60_000));
            match r.target {
                CreateTarget::NewAgent { config } => {
                    assert_eq!(config.provider, "claude");
                    assert_eq!(config.cwd, "/tmp/project");
                    assert_eq!(config.thinking_option_id.as_deref(), Some("think-hard"));
                    assert_eq!(config.archive_on_finish, Some(false));
                    assert_eq!(config.isolation, Some(Isolation::Worktree));
                }
                other => panic!("unexpected target {other:?}"),
            }
        }
        other => panic!("unexpected request {other:?}"),
    }
}

#[test]
fn round_trips_new_agent_run_options_on_update_requests() {
    let request = parse(json!({
        "type": "schedule/update", "requestId": "request-1", "scheduleId": "schedule-1",
        "newAgentConfig": {"thinkingOptionId": "think-hard", "archiveOnFinish": false, "isolation": "worktree"}
    }));
    match request {
        ScheduleRequest::Update(r) => {
            assert_eq!(r.schedule_id, "schedule-1");
            let cfg = r.new_agent_config.unwrap();
            assert_eq!(
                serde_json::to_value(&cfg).unwrap(),
                json!({"thinkingOptionId": "think-hard", "archiveOnFinish": false, "isolation": "worktree"})
            );
        }
        other => panic!("unexpected request {other:?}"),
    }
}

#[test]
fn request_validation_mirrors_the_zod_schemas() {
    let base = |extra: Value| {
        let mut v = json!({
            "type": "schedule/create", "requestId": "r", "prompt": "p",
            "cadence": {"type": "every", "everyMs": 1000},
            "target": {"type": "agent", "agentId": AGENT}
        });
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    };
    assert!(ScheduleRequest::from_value(base(json!({}))).is_ok());
    assert!(ScheduleRequest::from_value(base(json!({"prompt": ""}))).is_err());
    assert!(ScheduleRequest::from_value(base(json!({"maxRuns": 0}))).is_err());
    assert!(ScheduleRequest::from_value(base(
        json!({"target": {"type": "agent", "agentId": "nope"}})
    ))
    .is_err());
    assert!(
        ScheduleRequest::from_value(base(json!({"cadence": {"type": "every", "everyMs": 0}})))
            .is_err()
    );
    assert!(ScheduleRequest::from_value(base(
        json!({"cadence": {"type": "cron", "expression": "  "}})
    ))
    .is_err());
    assert!(
        ScheduleRequest::from_value(json!({"type": "schedule/nope", "requestId": "r"})).is_err()
    );
    // cadence strings are trimmed like z.string().trim()
    match ScheduleRequest::from_value(base(
        json!({"cadence": {"type": "cron", "expression": " 0 9 * * * "}}),
    ))
    .unwrap()
    {
        ScheduleRequest::Create(r) => assert_eq!(r.cadence, cron("0 9 * * *")),
        _ => unreachable!(),
    }
    // update: null clears nullable fields, empty provider is rejected
    let upd = |extra: Value| {
        let mut v = json!({"type": "schedule/update", "requestId": "r", "scheduleId": "s"});
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    };
    match ScheduleRequest::from_value(upd(
        json!({"name": null, "maxRuns": null, "expiresAt": null}),
    ))
    .unwrap()
    {
        ScheduleRequest::Update(r) => {
            assert_eq!(r.name, Some(None));
            assert_eq!(r.max_runs, Some(None));
            assert_eq!(r.expires_at, Some(None));
        }
        _ => unreachable!(),
    }
    assert!(ScheduleRequest::from_value(upd(json!({"newAgentConfig": {"provider": ""}}))).is_err());
}

// --- session/schedule/schedule-session.test.ts ---

#[test]
fn schedule_create_returns_a_summary_with_the_runs_stripped() {
    let (_dir, _clock, session) = session();
    let request = parse(json!({
        "type": "schedule/create", "requestId": "sc1", "prompt": "p",
        "cadence": {"type": "every", "everyMs": 1000},
        "target": {"type": "agent", "agentId": AGENT}
    }));
    let out = wire(&session.handle(&request));
    assert_eq!(out["type"], "schedule/create/response");
    let payload = &out["payload"];
    assert_eq!(payload["requestId"], "sc1");
    assert_eq!(payload["error"], Value::Null);
    assert!(payload["schedule"].get("runs").is_none());
    assert_eq!(payload["schedule"]["id"].as_str().unwrap().len(), 8);
    assert_eq!(payload["schedule"]["status"], "active");
    assert_eq!(payload["schedule"]["lastRunAt"], Value::Null);
}

#[test]
fn schedule_create_remaps_a_self_target_to_an_agent_target_before_creating() {
    let (_dir, _clock, session) = session();
    let request = parse(json!({
        "type": "schedule/create", "requestId": "sc2", "prompt": "p",
        "cadence": {"type": "every", "everyMs": 1000},
        "target": {"type": "self", "agentId": AGENT}
    }));
    let out = wire(&session.handle(&request));
    assert_eq!(
        out["payload"]["schedule"]["target"],
        json!({"type": "agent", "agentId": AGENT})
    );
    assert_eq!(out["payload"]["error"], Value::Null);
}

// --- coverage for the remaining handlers and the error envelope ---

#[test]
fn full_request_cycle_matches_the_wire_shapes() {
    let (_dir, _clock, session) = session();
    let created = wire(&session.handle(&parse(json!({
        "type": "schedule/create", "requestId": "1", "prompt": "p", "name": "n",
        "cadence": {"type": "cron", "expression": "30 9 * * *"},
        "target": {"type": "agent", "agentId": AGENT}
    }))));
    let id = created["payload"]["schedule"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let listed = wire(&session.handle(&parse(json!({"type": "schedule/list", "requestId": "2"}))));
    assert_eq!(listed["type"], "schedule/list/response");
    assert_eq!(listed["payload"]["schedules"].as_array().unwrap().len(), 1);
    assert!(listed["payload"]["schedules"][0].get("runs").is_none());

    let inspected = wire(&session.handle(&parse(
        json!({"type": "schedule/inspect", "requestId": "3", "scheduleId": id}),
    )));
    assert_eq!(inspected["payload"]["schedule"]["runs"], json!([]));

    let run = wire(&session.handle(&parse(
        json!({"type": "schedule/run-once", "requestId": "4", "scheduleId": id}),
    )));
    assert_eq!(run["type"], "schedule/run-once/response");
    assert_eq!(run["payload"]["schedule"]["runs"][0]["status"], "succeeded");

    let logs = wire(&session.handle(&parse(
        json!({"type": "schedule/logs", "requestId": "5", "scheduleId": id}),
    )));
    assert_eq!(logs["payload"]["runs"].as_array().unwrap().len(), 1);

    let paused = wire(&session.handle(&parse(
        json!({"type": "schedule/pause", "requestId": "6", "scheduleId": id}),
    )));
    assert_eq!(paused["payload"]["schedule"]["status"], "paused");
    assert_eq!(paused["payload"]["schedule"]["nextRunAt"], Value::Null);

    let resumed = wire(&session.handle(&parse(
        json!({"type": "schedule/resume", "requestId": "7", "scheduleId": id}),
    )));
    assert_eq!(resumed["payload"]["schedule"]["status"], "active");

    let updated = wire(&session.handle(&parse(json!({
        "type": "schedule/update", "requestId": "8", "scheduleId": id, "name": null, "prompt": "new"
    }))));
    assert_eq!(updated["payload"]["schedule"]["name"], Value::Null);
    assert_eq!(updated["payload"]["schedule"]["prompt"], "new");

    let deleted = wire(&session.handle(&parse(
        json!({"type": "schedule/delete", "requestId": "9", "scheduleId": id}),
    )));
    assert_eq!(
        deleted,
        json!({"type": "schedule/delete/response", "payload": {"requestId": "9", "scheduleId": id, "error": null}})
    );
}

#[test]
fn failures_become_rpc_error_envelopes() {
    let (_dir, _clock, session) = session();
    let out = wire(&session.handle(&parse(
        json!({"type": "schedule/inspect", "requestId": "x", "scheduleId": "missing1"}),
    )));
    assert_eq!(
        out,
        json!({"type": "rpc_error", "payload": {
            "requestId": "x", "requestType": "schedule/inspect",
            "error": "Schedule not found: missing1", "code": "schedule_request_failed"
        }})
    );
}
