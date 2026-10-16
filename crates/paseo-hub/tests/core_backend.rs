//! `CoreExecutionBackend` over the real `AgentManager` and `AgentStore`: the projection of
//! owned agents, the stored-record snapshot, and `DaemonExecutions` end to end with a
//! registered mock session.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem};
use paseo_core::mock::MockSession;
use paseo_core::{AgentManager, ManagerOptions, RegisterOptions, SnapshotPersister};
use paseo_hub::{
    ControlAction, CoreExecutionBackend, CreateProgress, CreatedExecution, CreatedWorktree,
    DaemonExecutions, ExecError, ExecutionActions, ExecutionBackend, ExecutionCreateRequest,
    HubExecutionAgentCreateInput, HubExecutionAgents, HubExecutionControlInput, OwnedAgentEvent,
};
use paseo_provider_api::AgentSessionConfig;
use paseo_store::agent_record::OwnerKind;
use paseo_store::{AgentOwner, AgentStore, StoredAgentRecord};
use serde_json::{json, Value};

const DAEMON: &str = "daemon-1";

/// Manager agent ids must be UUIDs.
fn uuid(n: u32) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

fn owner(daemon: &str, execution: &str) -> AgentOwner {
    AgentOwner {
        kind: OwnerKind::Daemon,
        daemon_id: daemon.into(),
        execution_id: execution.into(),
        extra: Default::default(),
    }
}

async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..4000 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("timed out waiting for {what}");
}

#[derive(Default)]
struct Actions {
    manager: Mutex<Option<Arc<AgentManager>>>,
    store: Mutex<Option<Arc<AgentStore>>>,
    interrupts: Mutex<Vec<String>>,
    archives: Mutex<Vec<(String, String)>>,
    cleanups: Mutex<Vec<Option<Value>>>,
    fail_after_register: Mutex<bool>,
    sessions: Mutex<Vec<MockSession>>,
}

#[async_trait]
impl ExecutionActions for Actions {
    async fn create_agent(
        &self,
        request: ExecutionCreateRequest,
        progress: CreateProgress,
    ) -> Result<CreatedExecution, ExecError> {
        let manager = self.manager.lock().unwrap().clone().unwrap();
        let store = self.store.lock().unwrap().clone().unwrap();
        let id = uuid(100 + self.sessions.lock().unwrap().len() as u32);
        let session = MockSession::new("codex");
        self.sessions.lock().unwrap().push(session.clone());
        if request.worktree.is_some() {
            progress.worktree_created(CreatedWorktree {
                created: true,
                value: json!({ "path": "/wt" }),
            });
        }
        let view = manager
            .register_session(
                Box::new(session),
                AgentSessionConfig::new("codex", &request.cwd),
                &id,
                RegisterOptions {
                    history_primed: Some(true),
                    workspace_id: Some("workspace-1".into()),
                    owner: Some(request.owner.clone()),
                    ..RegisterOptions::default()
                },
            )
            .await
            .map_err(|e| ExecError::other(e.to_string()))?;
        progress.agent_created(id.clone());
        let mut record =
            StoredAgentRecord::new(&id, "codex", &request.cwd, "2026-07-13T00:00:00.000Z");
        record.workspace_id = Some("workspace-1".into());
        record.owner = Some(request.owner.clone());
        store
            .upsert(record)
            .await
            .map_err(|e| ExecError::other(e.to_string()))?;
        if *self.fail_after_register.lock().unwrap() {
            return Err(ExecError::other("prompt start failed"));
        }
        Ok(CreatedExecution {
            agent_id: id,
            workspace_id: view.workspace_id.clone(),
            snapshot: paseo_core::agent_payload::to_agent_payload(&view),
        })
    }

    async fn cleanup_failed_create(
        &self,
        created_worktree: Option<Value>,
    ) -> Result<(), ExecError> {
        self.cleanups.lock().unwrap().push(created_worktree);
        Ok(())
    }

    async fn interrupt_agent(&self, agent_id: &str) -> Result<(), ExecError> {
        self.interrupts.lock().unwrap().push(agent_id.into());
        Ok(())
    }

    async fn archive_workspace(
        &self,
        workspace_id: &str,
        request_id: &str,
    ) -> Result<(), ExecError> {
        self.archives
            .lock()
            .unwrap()
            .push((workspace_id.into(), request_id.into()));
        Ok(())
    }
}

struct Rig {
    manager: Arc<AgentManager>,
    store: Arc<AgentStore>,
    actions: Arc<Actions>,
    backend: Arc<CoreExecutionBackend>,
    _dir: tempfile::TempDir,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(AgentStore::new(dir.path().join("agents")));
    store.initialize().await.unwrap();
    let manager = AgentManager::new(ManagerOptions {
        store: Some(store.clone()),
        persister: Some(Arc::new(NoPersist) as Arc<dyn SnapshotPersister>),
        ..ManagerOptions::default()
    });
    let actions = Arc::new(Actions::default());
    *actions.manager.lock().unwrap() = Some(manager.clone());
    *actions.store.lock().unwrap() = Some(store.clone());
    let backend = Arc::new(CoreExecutionBackend::new(
        manager.clone(),
        store.clone(),
        actions.clone(),
    ));
    Rig {
        manager,
        store,
        actions,
        backend,
        _dir: dir,
    }
}

/// The agents these tests register persist through `Actions`, not the background persister.
struct NoPersist;

#[async_trait]
impl SnapshotPersister for NoPersist {
    async fn persist(
        &self,
        _: &paseo_core::ManagedAgentView,
        _: paseo_core::PersistOptions,
    ) -> Result<(), String> {
        Ok(())
    }
}

fn input(execution_id: &str) -> HubExecutionAgentCreateInput {
    HubExecutionAgentCreateInput {
        execution_id: execution_id.into(),
        provider: "codex".into(),
        cwd: "/workspace".into(),
        prompt: "run".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn only_agents_owned_by_the_daemon_are_projected_as_updates_and_streams() {
    let rig = rig().await;
    let events: Arc<Mutex<Vec<OwnedAgentEvent>>> = Arc::default();
    let sink = events.clone();
    let _guard = rig.backend.subscribe_owned(
        DAEMON.into(),
        Arc::new(move |e| sink.lock().unwrap().push(e)),
    );
    tokio::time::sleep(Duration::from_millis(20)).await;

    let owned = MockSession::new("codex");
    rig.manager
        .register_session(
            Box::new(owned.clone()),
            AgentSessionConfig::new("codex", "/workspace"),
            &uuid(1),
            RegisterOptions {
                history_primed: Some(true),
                owner: Some(owner(DAEMON, "execution-1")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    for (id, who) in [(uuid(4), Some(owner("daemon-other", "x"))), (uuid(5), None)] {
        rig.manager
            .register_session(
                Box::new(MockSession::new("codex")),
                AgentSessionConfig::new("codex", "/workspace"),
                &id,
                RegisterOptions {
                    history_primed: Some(true),
                    owner: who,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    eventually("an update of the owned agent", || {
        !events.lock().unwrap().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    owned.push_event(AgentStreamEvent::Timeline {
        item: Box::new(AgentTimelineItem::AssistantMessage {
            text: "hello".into(),
            message_id: None,
        }),
        provider: "codex".into(),
        turn_id: None,
        timestamp: None,
    });
    eventually("a stream event", || {
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, OwnedAgentEvent::Stream { .. }))
    })
    .await;

    let events = events.lock().unwrap();
    assert!(!events.is_empty());
    for event in events.iter() {
        match event {
            OwnedAgentEvent::Update {
                execution_id,
                agent,
            } => {
                assert_eq!(execution_id, "execution-1");
                assert_eq!(agent["id"], uuid(1));
                assert!(
                    agent.get("config").is_none(),
                    "snapshots never carry the launch config"
                );
                assert!(agent.get("mcpServers").is_none());
            }
            OwnedAgentEvent::Stream {
                execution_id,
                agent_id,
                event,
            } => {
                assert_eq!(execution_id, "execution-1");
                assert_eq!(agent_id, &uuid(1));
                assert_eq!(event["type"], "timeline");
            }
        }
    }
}

#[tokio::test]
async fn unsubscribing_stops_the_projection_and_releases_the_bus_subscription() {
    let rig = rig().await;
    let baseline = rig.manager.subscription_count();
    let events: Arc<Mutex<Vec<OwnedAgentEvent>>> = Arc::default();
    let sink = events.clone();

    let guard = rig.backend.subscribe_owned(
        DAEMON.into(),
        Arc::new(move |e| sink.lock().unwrap().push(e)),
    );
    eventually("registered", || {
        rig.manager.subscription_count() == baseline + 1
    })
    .await;
    guard.call();
    eventually("released", || rig.manager.subscription_count() == baseline).await;

    rig.manager
        .register_session(
            Box::new(MockSession::new("codex")),
            AgentSessionConfig::new("codex", "/workspace"),
            &uuid(2),
            RegisterOptions {
                history_primed: Some(true),
                owner: Some(owner(DAEMON, "e")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(events.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_stored_record_projects_as_a_closed_snapshot_and_a_live_one_as_its_live_state() {
    let rig = rig().await;
    let mut record =
        StoredAgentRecord::new(&uuid(3), "codex", "/workspace", "2026-07-13T00:00:00.000Z");
    record.workspace_id = Some("workspace-9".into());
    record.owner = Some(owner(DAEMON, "execution-1"));

    let stored = rig.backend.project_record(&record);

    assert_eq!(stored["id"], uuid(3));
    assert_eq!(stored["status"], "closed");
    assert_eq!(stored["workspaceId"], "workspace-9");
    assert_eq!(stored["cwd"], "/workspace");

    rig.manager
        .register_session(
            Box::new(MockSession::new("codex")),
            AgentSessionConfig::new("codex", "/workspace"),
            &uuid(3),
            RegisterOptions {
                history_primed: Some(true),
                owner: record.owner.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let live = rig.backend.project_record(&record);
    assert_eq!(live["status"], "idle");
    assert!(rig.backend.is_live(&uuid(3)));
}

#[tokio::test]
async fn the_store_finds_an_execution_by_daemon_and_forgets_it_when_the_record_is_removed() {
    let rig = rig().await;
    let mut record =
        StoredAgentRecord::new(&uuid(7), "codex", "/workspace", "2026-07-13T00:00:00.000Z");
    record.workspace_id = Some("workspace-1".into());
    record.owner = Some(owner(DAEMON, "execution-1"));
    rig.store.upsert(record).await.unwrap();

    assert!(rig
        .backend
        .find_by_daemon_execution(&owner(DAEMON, "execution-1"))
        .await
        .unwrap()
        .is_some());
    assert!(rig
        .backend
        .find_by_daemon_execution(&owner("daemon-other", "execution-1"))
        .await
        .unwrap()
        .is_none());
    rig.backend.remove_agent_record(&uuid(7)).await.unwrap();
    assert!(rig
        .backend
        .find_by_daemon_execution(&owner(DAEMON, "execution-1"))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn create_replay_and_restart_over_the_real_manager_and_store() {
    let rig = rig().await;
    let agents = DaemonExecutions::new(DAEMON, rig.backend.clone());

    let created = agents.create(input("execution-1")).await.unwrap();
    let replay = agents.create(input("execution-1")).await.unwrap();

    assert_eq!(created.agent["id"], uuid(100));
    assert_eq!(replay.agent["id"], uuid(100));
    assert_eq!(
        replay.agent["status"], "idle",
        "live agent: the live snapshot"
    );
    assert_eq!(rig.actions.sessions.lock().unwrap().len(), 1);

    // A daemon restart: the manager is new, the record is on disk.
    let restarted = AgentManager::new(ManagerOptions::default());
    let backend = Arc::new(CoreExecutionBackend::new(
        restarted,
        rig.store.clone(),
        rig.actions.clone(),
    ));
    let after_restart = DaemonExecutions::new(DAEMON, backend)
        .create(input("execution-1"))
        .await
        .unwrap();
    assert_eq!(after_restart.agent["id"], uuid(100));
    assert_eq!(after_restart.agent["status"], "closed");
    assert_eq!(
        rig.actions.sessions.lock().unwrap().len(),
        1,
        "no second provider session"
    );
}

#[tokio::test]
async fn a_failed_create_closes_the_agent_drops_its_state_and_releases_subscriptions() {
    let rig = rig().await;
    let baseline = rig.manager.subscription_count();
    *rig.actions.fail_after_register.lock().unwrap() = true;
    let agents = DaemonExecutions::new(DAEMON, rig.backend.clone());
    let mut create = input("execution-failed");
    create.worktree = Some(paseo_hub::ExecutionWorktree {
        action: paseo_hub::WorktreeAction::BranchOff,
        worktree_name: Some("failed-prompt".into()),
        base_branch: None,
        ref_name: None,
        github_pr_number: None,
    });

    let error = agents.create(create).await.unwrap_err();

    assert_eq!(error, ExecError::other("prompt start failed"));
    assert!(rig.manager.get_agent(&uuid(100)).is_none());
    assert!(rig.store.get(&uuid(100)).await.unwrap().is_none());
    assert_eq!(
        *rig.actions.cleanups.lock().unwrap(),
        vec![Some(json!({ "path": "/wt" }))]
    );
    assert_eq!(rig.manager.subscription_count(), baseline);
    assert!(rig.actions.sessions.lock().unwrap()[0].is_closed());
}

#[tokio::test]
async fn control_goes_through_the_actions_for_owned_executions_only() {
    let rig = rig().await;
    let agents = DaemonExecutions::new(DAEMON, rig.backend.clone());
    agents.create(input("execution-1")).await.unwrap();

    agents
        .control(HubExecutionControlInput {
            request_id: "i".into(),
            execution_id: "execution-1".into(),
            action: ControlAction::Interrupt,
        })
        .await
        .unwrap();
    agents
        .control(HubExecutionControlInput {
            request_id: "a".into(),
            execution_id: "execution-1".into(),
            action: ControlAction::Archive,
        })
        .await
        .unwrap();
    agents
        .control(HubExecutionControlInput {
            request_id: "m".into(),
            execution_id: "missing".into(),
            action: ControlAction::Archive,
        })
        .await
        .unwrap();

    assert_eq!(*rig.actions.interrupts.lock().unwrap(), vec![uuid(100)]);
    assert_eq!(
        *rig.actions.archives.lock().unwrap(),
        vec![("workspace-1".to_owned(), "a".to_owned())]
    );
}
