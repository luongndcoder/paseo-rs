//! Shared fakes of the paseo-hub integration tests: the TS `HubRelationshipHarness` pieces
//! that do not need a daemon (remote, clock, session host, execution backend).
#![allow(dead_code, unused_imports)]

mod backend;
pub mod http;
mod remote;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use paseo_authz::Permission;
use paseo_hub::{
    AttachOptions, DaemonExecutions, HubClock, HubExecutionAgents, HubHandle, HubLogger,
    HubOptions, HubRelationshipActor, HubSessionHost, HubSocket, LogLevel, RetryPolicy,
    ScheduledTask,
};

pub use backend::{FakeBackend, Operation};
pub use remote::FakeRemote;

pub const NOW: &str = "2026-07-13T00:00:00.000Z";

pub async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    for _ in 0..4000 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Lets spawned tasks run until nothing else is ready.
pub async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(5)).await;
}

// ---- clock ------------------------------------------------------------------------------

struct ManualTask {
    cancelled: Arc<AtomicBool>,
    task: Option<Box<dyn FnOnce() + Send>>,
}

struct ManualHandle(Arc<AtomicBool>);

impl ScheduledTask for ManualHandle {
    fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// TS `ManualRelationshipClock`: tasks run only when the test says so; it is also the retry
/// policy and records the attempt numbers it was asked about (delay is always 0).
pub struct ManualClock {
    tasks: Mutex<Vec<ManualTask>>,
    attempts: Mutex<Vec<u32>>,
    delays: Mutex<Vec<u64>>,
    now_ms: Mutex<i64>,
}

impl ManualClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            tasks: Mutex::new(Vec::new()),
            attempts: Mutex::new(Vec::new()),
            delays: Mutex::new(Vec::new()),
            now_ms: Mutex::new(1_783_900_800_000), // 2026-07-13T00:00:00.000Z
        })
    }

    pub fn pending_tasks(&self) -> usize {
        self.tasks
            .lock()
            .unwrap()
            .iter()
            .filter(|t| !t.cancelled.load(Ordering::SeqCst))
            .count()
    }

    pub fn scheduled_attempts(&self) -> Vec<u32> {
        self.attempts.lock().unwrap().clone()
    }

    pub fn scheduled_delays(&self) -> Vec<u64> {
        self.delays.lock().unwrap().clone()
    }

    pub fn run_next(&self) {
        let mut task = {
            let mut tasks = self.tasks.lock().unwrap();
            assert!(!tasks.is_empty(), "No Hub reconnect is scheduled");
            tasks.remove(0)
        };
        assert!(
            !task.cancelled.load(Ordering::SeqCst),
            "No Hub reconnect is scheduled"
        );
        (task.task.take().unwrap())();
    }
}

impl HubClock for ManualClock {
    fn now_ms(&self) -> i64 {
        *self.now_ms.lock().unwrap()
    }

    fn schedule(&self, delay_ms: u64, task: Box<dyn FnOnce() + Send>) -> Box<dyn ScheduledTask> {
        self.delays.lock().unwrap().push(delay_ms);
        let cancelled = Arc::new(AtomicBool::new(false));
        self.tasks.lock().unwrap().push(ManualTask {
            cancelled: cancelled.clone(),
            task: Some(task),
        });
        Box::new(ManualHandle(cancelled))
    }
}

impl RetryPolicy for ManualClock {
    fn delay(&self, attempt: u32) -> u64 {
        self.attempts.lock().unwrap().push(attempt);
        0
    }
}

// ---- logger -----------------------------------------------------------------------------

#[derive(Default)]
pub struct RecordingLogger {
    entries: Mutex<Vec<(LogLevel, String, Option<String>)>>,
}

impl RecordingLogger {
    pub fn messages(&self) -> Vec<String> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .map(|(_, m, _)| m.clone())
            .collect()
    }

    pub fn all_text(&self) -> String {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .map(|(_, m, d)| format!("{m} {}", d.clone().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl HubLogger for RecordingLogger {
    fn log(&self, level: LogLevel, message: &str, detail: Option<&str>) {
        self.entries
            .lock()
            .unwrap()
            .push((level, message.to_owned(), detail.map(str::to_owned)));
    }
}

// ---- session host -----------------------------------------------------------------------

pub struct Attached {
    pub options: AttachOptions,
}

/// Records what the actor hands to the session layer and builds `DaemonExecutions` over the
/// shared fake backend.
pub struct FakeHost {
    pub backend: Arc<FakeBackend>,
    pub attached: Mutex<Vec<Attached>>,
    /// Sockets stay open until the test ends, like a session that owns them.
    held_sockets: Mutex<Vec<Box<dyn HubSocket>>>,
    pub permission_updates: Mutex<Vec<(String, Vec<Permission>)>>,
    pub created_agents: Mutex<Vec<(String, Arc<DaemonExecutions>)>>,
}

impl FakeHost {
    pub fn new(backend: Arc<FakeBackend>) -> Arc<Self> {
        Arc::new(Self {
            backend,
            attached: Mutex::new(Vec::new()),
            held_sockets: Mutex::new(Vec::new()),
            permission_updates: Mutex::new(Vec::new()),
            created_agents: Mutex::new(Vec::new()),
        })
    }

    pub fn attach_count(&self) -> usize {
        self.attached.lock().unwrap().len()
    }

    pub fn attached_permissions(&self) -> Vec<Vec<Permission>> {
        self.attached
            .lock()
            .unwrap()
            .iter()
            .map(|a| a.options.permissions.clone())
            .collect()
    }

    pub fn latest_agents(&self) -> Arc<DaemonExecutions> {
        self.created_agents
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .1
            .clone()
    }

    pub fn latest_attached_agents(&self) -> Arc<dyn HubExecutionAgents> {
        self.attached
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .options
            .agents
            .clone()
    }
}

#[async_trait]
impl HubSessionHost for FakeHost {
    async fn attach_socket(
        &self,
        socket: Box<dyn HubSocket>,
        options: AttachOptions,
    ) -> Result<(), String> {
        self.held_sockets.lock().unwrap().push(socket);
        self.attached.lock().unwrap().push(Attached { options });
        Ok(())
    }

    fn update_attached_permissions(&self, principal_id: &str, permissions: &[Permission]) {
        self.permission_updates
            .lock()
            .unwrap()
            .push((principal_id.to_owned(), permissions.to_vec()));
    }

    fn create_execution_agents(&self, daemon_id: &str) -> Arc<dyn HubExecutionAgents> {
        let agents = Arc::new(DaemonExecutions::new(daemon_id, self.backend.clone()));
        self.created_agents
            .lock()
            .unwrap()
            .push((daemon_id.to_owned(), agents.clone()));
        agents
    }
}

// ---- rig --------------------------------------------------------------------------------

/// The `$PASEO_HOME` of a rig: a temp dir, or a directory somebody else owns.
pub struct Home {
    path: PathBuf,
    _temp: Option<tempfile::TempDir>,
}

impl Home {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub struct Rig {
    pub home: Home,
    pub handle: HubHandle,
    pub remote: Arc<FakeRemote>,
    pub clock: Arc<ManualClock>,
    pub host: Arc<FakeHost>,
    pub logger: Arc<RecordingLogger>,
    pub ceremony: Option<paseo_hub::Ceremony>,
}

pub fn options(
    home: &Path,
    remote: &Arc<FakeRemote>,
    clock: &Arc<ManualClock>,
    host: &Arc<FakeHost>,
    logger: &Arc<RecordingLogger>,
    ceremony: &Option<paseo_hub::Ceremony>,
) -> HubOptions {
    let ceremony = ceremony.clone();
    HubOptions {
        paseo_home: home.to_path_buf(),
        hostname: "fixture-host".into(),
        server_id: "server-fixture".into(),
        daemon_public_key: "public-key-fixture".into(),
        logger: logger.clone(),
        remote: remote.clone(),
        clock: clock.clone(),
        retry_policy: clock.clone(),
        create_ceremony: ceremony.map(|c| {
            Box::new(move || c.clone()) as Box<dyn Fn() -> paseo_hub::Ceremony + Send + Sync>
        }),
        host: host.clone(),
    }
}

impl Rig {
    pub async fn start() -> Self {
        Self::with_home(tempfile::tempdir().unwrap()).await
    }

    pub async fn with_home(home: tempfile::TempDir) -> Self {
        Self::build(
            Home {
                path: home.path().to_path_buf(),
                _temp: Some(home),
            },
            None,
        )
    }

    /// A rig over a directory the caller owns (the crash tests share it with a child process).
    pub async fn in_dir(path: &Path) -> Self {
        Self::build(
            Home {
                path: path.to_path_buf(),
                _temp: None,
            },
            None,
        )
    }

    pub async fn with_ceremony(ceremony: paseo_hub::Ceremony) -> Self {
        let home = tempfile::tempdir().unwrap();
        Self::build(
            Home {
                path: home.path().to_path_buf(),
                _temp: Some(home),
            },
            Some(ceremony),
        )
    }

    fn build(home: Home, ceremony: Option<paseo_hub::Ceremony>) -> Self {
        let remote = FakeRemote::new();
        let clock = ManualClock::new();
        let host = FakeHost::new(FakeBackend::new());
        let logger = Arc::new(RecordingLogger::default());
        let handle = HubRelationshipActor::spawn(options(
            home.path(),
            &remote,
            &clock,
            &host,
            &logger,
            &ceremony,
        ))
        .expect("spawn actor");
        Self {
            home,
            handle,
            remote,
            clock,
            host,
            logger,
            ceremony,
        }
    }

    pub fn relationship_path(&self) -> PathBuf {
        self.home.path().join("hub-relationship.json")
    }

    /// The persisted record, if any.
    pub fn file(&self) -> Option<serde_json::Value> {
        std::fs::read_to_string(self.relationship_path())
            .ok()
            .map(|t| serde_json::from_str(&t).unwrap())
    }

    pub fn quarantined_files(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.home.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("hub-relationship.invalid-"))
            })
            .collect()
    }

    /// `restartDaemon`: stop the actor and start a fresh one on the same home and fakes.
    pub async fn restart(&mut self) {
        self.handle.stop().await.unwrap();
        self.handle = self.respawn();
        self.handle.start().await.unwrap();
    }

    /// A new actor on the same home without starting it (`startStoppedDaemon`).
    pub fn respawn(&self) -> HubHandle {
        HubRelationshipActor::spawn(options(
            self.home.path(),
            &self.remote,
            &self.clock,
            &self.host,
            &self.logger,
            &self.ceremony,
        ))
        .expect("spawn actor")
    }

    pub async fn connect(&self, token: &str) -> paseo_hub::HubRelationshipStatus {
        self.handle
            .connect(connect_input(token, "https://hub.test"))
            .await
            .expect("connect")
    }
}

pub fn connect_input(token: &str, hub_url: &str) -> paseo_hub::ConnectInput {
    paseo_hub::ConnectInput {
        hub_url: hub_url.into(),
        token: token.into(),
        permissions: vec!["hub.execute".into()],
    }
}

pub fn perms(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}
