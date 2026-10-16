//! `CoreScheduleHost`: the `ScheduleAgentHost` the schedule service launches agents through.
//! The host blocks on the manager, so every call runs on `spawn_blocking`.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use paseo_core::mock::{boxed, MockClient, MockSession, ScriptedBehavior};
use paseo_core::{CoreScheduleHost, ScheduleWorkspaces};
use paseo_provider_api::TurnHandle;
use paseo_schedule::host::{AgentTargetState, CreateScheduledAgent, RunWorkspace};
use paseo_schedule::{
    CreateScheduleInput, Isolation, NewAgentConfig, Result as ScheduleResult, RunStatus,
    ScheduleAgentHost, ScheduleCadence, ScheduleService, ScheduleServiceOptions, ScheduleTarget,
};

#[derive(Default)]
struct Workspaces {
    created: Mutex<Vec<(String, Isolation, String)>>,
    archived: Mutex<Vec<String>>,
}

impl ScheduleWorkspaces for Workspaces {
    fn create_run_workspace(
        &self,
        cwd: &str,
        isolation: Isolation,
        prompt: &str,
    ) -> ScheduleResult<RunWorkspace> {
        self.created
            .lock()
            .unwrap()
            .push((cwd.to_owned(), isolation, prompt.to_owned()));
        Ok(RunWorkspace {
            workspace_id: "wks_run".into(),
            cwd: cwd.to_owned(),
        })
    }

    fn archive_workspace(&self, workspace_id: &str) -> ScheduleResult<()> {
        self.archived.lock().unwrap().push(workspace_id.to_owned());
        Ok(())
    }
}

/// A session whose turn answers `reply` and completes.
fn replying_client(reply: &'static str) -> MockClient {
    let client = MockClient::new("codex");
    client.on_create(move |req| {
        boxed(async move {
            Ok(MockSession::builder("codex")
                .config(&req.config)
                .behavior(
                    ScriptedBehavior::new().on_start(move |session, prompt, options| {
                        boxed(async move {
                            let n = session.record_start(prompt, options);
                            let turn = format!("turn-{n}");
                            let (soon, turn_id) = (session.clone(), turn.clone());
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::ZERO).await;
                                soon.push_event(provider_event_turn_started("codex", &turn_id));
                                soon.push_event(assistant(reply, "codex", Some(&turn_id)));
                                soon.push_event(turn_completed(&turn_id));
                            });
                            Ok(TurnHandle { turn_id: turn })
                        })
                    }),
                )
                .build())
        })
    });
    client
}

struct Fixture {
    lc: Lc,
    host: Arc<CoreScheduleHost>,
    workspaces: Arc<Workspaces>,
}

async fn fixture(client: MockClient) -> Fixture {
    let lc = Lc::with_client(client).await;
    let workspaces = Arc::new(Workspaces::default());
    let host = Arc::new(CoreScheduleHost::new(
        lc.manager.clone(),
        tokio::runtime::Handle::current(),
        workspaces.clone(),
    ));
    Fixture {
        lc,
        host,
        workspaces,
    }
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(work).await.unwrap()
}

fn new_agent_request(cwd: &str, prompt: &str) -> CreateScheduledAgent {
    let mut config = NewAgentConfig::new("codex", cwd);
    config.model = Some("gpt-5.4".into());
    CreateScheduledAgent {
        provider: "codex/gpt-5.4".into(),
        config,
        cwd: cwd.to_owned(),
        workspace_id: "wks_run".into(),
        labels: vec![
            ("paseo.schedule-id".into(), "sched-1".into()),
            ("paseo.schedule-run".into(), "run-1".into()),
        ],
        prompt: prompt.to_owned(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_target_state_reports_missing_archived_and_active_agents() {
    let f = fixture(MockClient::new("codex")).await;
    let agent = f.lc.create().await;
    f.lc.flush().await;
    let host = f.host.clone();
    let id = agent.id.clone();
    assert_eq!(
        blocking({
            let host = host.clone();
            let id = id.clone();
            move || host.agent_target_state(&id)
        })
        .await
        .unwrap(),
        AgentTargetState::Active
    );
    assert_eq!(
        blocking({
            let host = host.clone();
            move || host.agent_target_state(&id_n(900))
        })
        .await
        .unwrap(),
        AgentTargetState::Missing
    );
    f.lc.manager.archive_agent(&agent.id).await.unwrap();
    assert_eq!(
        blocking(move || host.agent_target_state(&id))
            .await
            .unwrap(),
        AgentTargetState::Archived
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_agent_labels_the_agent_derives_its_title_and_assigns_the_workspace() {
    let f = fixture(MockClient::new("codex")).await;
    let cwd = f.lc.cwd();
    let request = new_agent_request(&cwd, "  \nSummarize   the day\nthen rest");
    let host = f.host.clone();
    let created = blocking(move || host.create_agent(&request)).await.unwrap();

    assert_eq!(created.initial_prompt_error, None);
    let record = f.lc.record(&created.agent_id).await.unwrap();
    assert_eq!(
        record.title.value().map(String::as_str),
        Some("Summarize the day")
    );
    assert_eq!(record.workspace_id.as_deref(), Some("wks_run"));
    assert_eq!(
        record.labels.get("paseo.schedule-id").map(String::as_str),
        Some("sched-1")
    );
    assert_eq!(
        record.labels.get("paseo.schedule-run").map(String::as_str),
        Some("run-1")
    );
    assert_eq!(
        f.lc.client.creates()[0].config.model.as_deref(),
        Some("gpt-5.4")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_agent_returns_the_final_text_and_the_curated_activity() {
    let f = fixture(replying_client("all done")).await;
    let agent = f.lc.create().await;
    let (host, id) = (f.host.clone(), agent.id.clone());
    let result = blocking(move || host.run_agent(&id, "work")).await.unwrap();
    assert!(!result.canceled);
    assert_eq!(result.final_text, "all done");
    assert_eq!(result.timeline_text, "all done");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_existing_agent_loads_a_stored_agent_runs_the_prompt_and_reports_the_last_message() {
    let f = fixture(replying_client("fired ok")).await;
    let agent = f.lc.create().await;
    f.lc.manager.close_agent(&agent.id).await.unwrap();
    // The resumed session answers like the original one.
    f.lc.client.on_resume(|req| {
        boxed(async move {
            let session = MockSession::builder("codex")
                .behavior(
                    ScriptedBehavior::new().on_start(|session, prompt, options| {
                        boxed(async move {
                            let n = session.record_start(prompt, options);
                            let turn = format!("resumed-turn-{n}");
                            let (soon, turn_id) = (session.clone(), turn.clone());
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::ZERO).await;
                                soon.push_event(provider_event_turn_started("codex", &turn_id));
                                soon.push_event(assistant("fired ok", "codex", Some(&turn_id)));
                                soon.push_event(turn_completed(&turn_id));
                            });
                            Ok(TurnHandle { turn_id: turn })
                        })
                    }),
                )
                .build();
            session.set_persistence(req.handle.clone());
            Ok(session)
        })
    });

    let (host, id) = (f.host.clone(), agent.id.clone());
    let outcome =
        blocking(move || host.run_existing_agent(&id, "<paseo-system>tick</paseo-system>"))
            .await
            .unwrap();
    assert!(!outcome.permission_pending);
    assert!(!outcome.errored);
    assert_eq!(outcome.last_message.as_deref(), Some("fired ok"));
    assert_eq!(f.lc.client.resumes().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_existing_agent_refuses_an_agent_that_already_runs() {
    let client = MockClient::new("codex");
    client.on_create(|req| {
        boxed(async move {
            Ok(MockSession::builder("codex")
                .config(&req.config)
                .behavior(
                    ScriptedBehavior::new().on_start(|session, prompt, options| {
                        boxed(async move {
                            let n = session.record_start(prompt, options);
                            let turn = format!("turn-{n}");
                            session.push_event_soon(provider_event_turn_started("codex", &turn));
                            Ok(TurnHandle { turn_id: turn })
                        })
                    }),
                )
                .build())
        })
    });
    let f = fixture(client).await;
    let agent = f.lc.create().await;
    let _running = f.lc.start_background_run(&agent.id, "long task").await;

    let (host, id) = (f.host.clone(), agent.id.clone());
    let error = blocking(move || host.run_existing_agent(&id, "again"))
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("Agent {} already has an active run", agent.id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_calls_go_to_the_workspace_service() {
    let f = fixture(MockClient::new("codex")).await;
    let host = f.host.clone();
    let workspace =
        blocking(move || host.create_run_workspace("/repo", Isolation::Worktree, "do it"))
            .await
            .unwrap();
    assert_eq!(workspace.workspace_id, "wks_run");
    let host = f.host.clone();
    blocking(move || host.archive_workspace("wks_run"))
        .await
        .unwrap();
    assert_eq!(
        *f.workspaces.created.lock().unwrap(),
        vec![("/repo".to_owned(), Isolation::Worktree, "do it".to_owned())]
    );
    assert_eq!(
        *f.workspaces.archived.lock().unwrap(),
        vec!["wks_run".to_owned()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_agent_schedule_runs_end_to_end_through_the_core_host() {
    let f = fixture(replying_client("scheduled result")).await;
    let cwd = f.lc.cwd();
    let home = tempfile::tempdir().unwrap();
    let service = Arc::new(ScheduleService::new(ScheduleServiceOptions {
        paseo_home: home.path().to_path_buf(),
        logger: Arc::new(paseo_schedule::logger::NoopLogger),
        host: f.host.clone(),
        now: None,
        runner: None,
    }));
    let target = ScheduleTarget::NewAgent {
        config: NewAgentConfig::new("codex", &cwd),
    };
    let service2 = service.clone();
    let after = blocking(move || {
        let schedule = service2.create(CreateScheduleInput::new(
            "Report status",
            ScheduleCadence::Every {
                every_ms: 3_600_000,
            },
            target,
        ))?;
        service2.run_once(&schedule.id)
    })
    .await
    .unwrap();

    assert_eq!(after.runs.len(), 1);
    let run = &after.runs[0];
    assert_eq!(run.status, RunStatus::Succeeded, "{run:?}");
    assert!(run
        .output
        .as_deref()
        .unwrap_or_default()
        .contains("scheduled result"));
    let agent_id = run.agent_id.clone().expect("run agent");
    let record = f.lc.record(&agent_id).await.unwrap();
    assert_eq!(
        record.title.value().map(String::as_str),
        Some("Report status")
    );
    assert_eq!(record.workspace_id.as_deref(), Some("wks_run"));
    assert_eq!(
        *f.workspaces.created.lock().unwrap(),
        vec![(cwd, Isolation::Local, "Report status".to_owned())]
    );
}

// ---- outcomes the schedule service branches on ----

/// A client whose turns emit `events(turn_id)` after `turn_started` and never complete on their own.
fn scripted_turn_client(
    events: impl Fn(&str) -> Vec<paseo_agent_types::AgentStreamEvent> + Send + Sync + Clone + 'static,
) -> MockClient {
    let client = MockClient::new("codex");
    client.on_create(move |req| {
        let events = events.clone();
        boxed(async move {
            Ok(MockSession::builder("codex")
                .config(&req.config)
                .behavior(
                    ScriptedBehavior::new().on_start(move |session, prompt, options| {
                        let events = events.clone();
                        boxed(async move {
                            let n = session.record_start(prompt, options);
                            let turn = format!("turn-{n}");
                            let (soon, turn_id) = (session.clone(), turn.clone());
                            tokio::spawn(async move {
                                tokio::time::sleep(Duration::ZERO).await;
                                soon.push_event(provider_event_turn_started("codex", &turn_id));
                                for event in events(&turn_id) {
                                    soon.push_event(event);
                                }
                            });
                            Ok(TurnHandle { turn_id: turn })
                        })
                    }),
                )
                .build())
        })
    });
    client
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_existing_agent_reports_a_failed_run_as_errored() {
    let f = fixture(scripted_turn_client(|turn| {
        vec![turn_failed(Some(turn), "provider exploded")]
    }))
    .await;
    let agent = f.lc.create().await;
    let (host, id) = (f.host.clone(), agent.id.clone());
    let outcome = blocking(move || host.run_existing_agent(&id, "go"))
        .await
        .unwrap();
    assert!(outcome.errored);
    assert!(!outcome.permission_pending);
    assert!(outcome
        .last_message
        .as_deref()
        .unwrap_or_default()
        .contains("provider exploded"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_existing_agent_stops_waiting_when_a_permission_is_pending() {
    let f = fixture(scripted_turn_client(|turn| {
        vec![paseo_agent_types::AgentStreamEvent::PermissionRequested {
            provider: "codex".into(),
            request: Box::new(paseo_agent_types::PermissionRequest {
                id: "perm-1".into(),
                provider: "codex".into(),
                name: "Read file".into(),
                kind: paseo_agent_types::PermissionKind::Tool,
                title: None,
                description: None,
                input: None,
                detail: None,
                suggestions: None,
                actions: None,
                metadata: None,
            }),
            turn_id: Some(turn.to_owned()),
        }]
    }))
    .await;
    let agent = f.lc.create().await;
    let (host, id) = (f.host.clone(), agent.id.clone());
    let outcome = blocking(move || host.run_existing_agent(&id, "go"))
        .await
        .unwrap();
    assert!(outcome.permission_pending);
    assert!(!outcome.errored);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wait_for_agent_event_waits_for_an_active_run_and_reports_its_last_message() {
    let f = fixture(replying_client("late answer")).await;
    let agent = f.lc.create().await;
    let _running = f.lc.start_background_run(&agent.id, "work").await;
    let (host, id) = (f.host.clone(), agent.id.clone());
    let outcome = blocking(move || host.wait_for_agent_event(&id))
        .await
        .unwrap();
    assert!(!outcome.errored);
    assert!(!outcome.permission_pending);
    assert_eq!(outcome.last_message.as_deref(), Some("late answer"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn target_lookup_without_a_store_fails_like_ts_without_a_registry() {
    let lc = Lc::with(
        MockClient::new("codex"),
        LcOptions {
            store: false,
            ..LcOptions::default()
        },
    )
    .await;
    let host = Arc::new(CoreScheduleHost::new(
        lc.manager.clone(),
        tokio::runtime::Handle::current(),
        Arc::new(Workspaces::default()),
    ));
    let error = blocking(move || host.agent_target_state(&id_n(1)))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Agent storage unavailable");
}
