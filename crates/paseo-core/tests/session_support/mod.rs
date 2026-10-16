//! Harness of the session-layer tests: a session registry over a real manager, `AgentStore` and
//! registries in a temp dir, and sockets that behave like the edge's writer (they enforce the
//! delivery gate and record what a client would receive).
#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use paseo_core::delivery::{Frame, Outbound, OutboundSink, SourceGate, SourceId};
use paseo_core::mock::MockClient;
use paseo_core::session::caps::cap;
use paseo_core::session::{AttachRequest, Attached};
use paseo_core::session::{
    ClientHello, NoSessionHooks, SessionHandle, SessionHooks, SessionRegistry, SessionServices,
};
use paseo_core::time::{Clock, ManualClock};
use paseo_core::{
    AgentManager, ManagerOptions, ProviderDefinition, ProviderRegistryUpdate, StorePersister,
};
use paseo_directory::DirectorySync;
use paseo_provider_api::AgentClient;
use paseo_store::workspace_registry::WorkspaceKind;
use paseo_store::{
    AgentStore, ProjectKind, ProjectRecord, ProjectRegistry, StoredAgentRecord, WorkspaceRecord,
    WorkspaceRegistry,
};
use serde_json::Value;

pub const GENERATION: &str = "golden-generation";

pub struct Env {
    pub dir: tempfile::TempDir,
    pub store: Arc<AgentStore>,
    pub workspaces: Arc<WorkspaceRegistry>,
    pub projects: Arc<ProjectRegistry>,
    pub manager: Arc<AgentManager>,
    pub directory: Arc<DirectorySync>,
    pub registry: SessionRegistry,
    pub services: SessionServices,
    pub clock: Arc<ManualClock>,
}

pub struct EnvOptions {
    pub grace: Duration,
    pub hooks: Arc<dyn SessionHooks>,
}

impl Default for EnvOptions {
    fn default() -> Self {
        Self {
            grace: Duration::from_secs(90),
            hooks: Arc::new(NoSessionHooks),
        }
    }
}

pub fn project_record() -> ProjectRecord {
    ProjectRecord::new(
        "prj_0000000000000001",
        "/work/repo",
        ProjectKind::Git,
        "repo",
        "2026-03-01T00:00:00.000Z",
        "2026-03-01T00:00:00.000Z",
    )
}

pub fn workspace_record(id: &str, cwd: &str) -> WorkspaceRecord {
    let mut ws = WorkspaceRecord::new(
        id,
        "prj_0000000000000001",
        cwd,
        WorkspaceKind::LocalCheckout,
        "main",
        "2026-03-01T00:00:00.000Z",
        "2026-03-01T00:00:00.000Z",
    );
    ws.branch = Some("main".into());
    ws.worktree_root = Some(cwd.into());
    ws
}

impl Env {
    pub async fn new() -> Self {
        Self::with(EnvOptions::default()).await
    }

    pub async fn with(options: EnvOptions) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(AgentStore::new(dir.path().join("agents")));
        let workspaces = Arc::new(WorkspaceRegistry::new(dir.path().join("workspaces.json")));
        let projects = Arc::new(ProjectRegistry::new(dir.path().join("projects.json")));
        let mut providers = ProviderRegistryUpdate::default();
        for provider in ["claude", "codex", "opencode"] {
            providers.clients.insert(
                provider.into(),
                Arc::new(MockClient::new(provider)) as Arc<dyn AgentClient>,
            );
            providers
                .definitions
                .insert(provider.into(), ProviderDefinition::enabled());
        }
        let clock = Arc::new(ManualClock::new(1_772_366_400_000));
        let manager = AgentManager::new(ManagerOptions {
            store: Some(store.clone()),
            persister: Some(Arc::new(StorePersister::new(store.clone()))),
            providers,
            clock: clock.clone() as Arc<dyn Clock>,
            ..ManagerOptions::default()
        });
        let directory = Arc::new(DirectorySync::new(GENERATION));
        let services = SessionServices {
            manager: manager.clone(),
            store: store.clone(),
            workspaces: workspaces.clone(),
            projects: projects.clone(),
            directory: directory.clone(),
            hooks: options.hooks,
            clock: clock.clone() as Arc<dyn Clock>,
        };
        let registry = SessionRegistry::with_grace(services.clone(), options.grace);
        Self {
            dir,
            store,
            workspaces,
            projects,
            manager,
            directory,
            registry,
            services,
            clock,
        }
    }

    pub async fn seed_workspace(&self) {
        self.projects.upsert(project_record()).await.unwrap();
        self.workspaces
            .upsert(workspace_record("ws_1", "/work/repo"), None)
            .await
            .unwrap();
    }

    pub async fn put_agent(&self, record: StoredAgentRecord) {
        self.store.upsert(record).await.unwrap();
    }

    /// A socket attached as `client_id` with the given capabilities.
    pub async fn connect(&self, client_id: &str, caps: &[&str]) -> Socket {
        self.connect_with(client_id, caps, None).await
    }

    pub async fn connect_with(
        &self,
        client_id: &str,
        caps: &[&str],
        app_version: Option<&str>,
    ) -> Socket {
        let source = self.registry.next_source_id();
        let wire = Arc::new(SocketWire::default());
        let attached = self
            .registry
            .attach(AttachRequest {
                principal: "owner".into(),
                client_id: client_id.into(),
                source,
                hello: ClientHello {
                    capabilities: paseo_core::session::caps::ClientCaps::from_names(
                        caps.iter().copied(),
                    ),
                    app_version: app_version.map(str::to_owned),
                },
                out: wire.clone(),
            })
            .await
            .expect("attach");
        wire.gate.set(attached.gate.clone()).ok();
        Socket {
            source,
            wire,
            attached,
            registry: self.registry.clone(),
        }
    }
}

pub fn modern() -> [&'static str; 1] {
    [cap::OWNED_SUBSCRIPTIONS]
}

#[derive(Default)]
pub struct SocketWire {
    pub gate: OnceLock<Arc<SourceGate>>,
    pub frames: Mutex<Vec<Value>>,
    pub binary: Mutex<Vec<Vec<u8>>>,
    pub dropped: AtomicUsize,
}

impl OutboundSink for SocketWire {
    fn send(&self, out: Outbound) {
        let permitted = self.gate.get().is_none_or(|gate| gate.permits(&out));
        if !permitted {
            self.dropped.fetch_add(1, Ordering::SeqCst);
            return;
        }
        match &out.body {
            Frame::Text(text) => {
                let envelope: Value = serde_json::from_str(text).expect("json frame");
                assert_eq!(
                    envelope["type"], "session",
                    "outbound frames are session envelopes"
                );
                self.frames
                    .lock()
                    .unwrap()
                    .push(envelope["message"].clone());
            }
            Frame::Binary(bytes) => self.binary.lock().unwrap().push(bytes.to_vec()),
        }
    }
}

pub struct Socket {
    pub source: SourceId,
    pub wire: Arc<SocketWire>,
    pub attached: Attached,
    pub registry: SessionRegistry,
}

impl Socket {
    pub fn handle(&self) -> &SessionHandle {
        &self.attached.handle
    }

    pub async fn send(&self, message: Value) {
        assert!(self.registry.inbound(self.source, message).await);
    }

    pub fn frames(&self) -> Vec<Value> {
        self.wire.frames.lock().unwrap().clone()
    }

    pub fn frames_of(&self, ty: &str) -> Vec<Value> {
        self.frames()
            .into_iter()
            .filter(|m| m["type"] == ty)
            .collect()
    }

    pub fn types(&self) -> Vec<String> {
        self.frames()
            .iter()
            .map(|m| m["type"].as_str().unwrap().to_owned())
            .collect()
    }

    pub fn dropped(&self) -> usize {
        self.wire.dropped.load(Ordering::SeqCst)
    }

    pub async fn detach(&self) {
        self.registry.detach(self.source).await;
    }

    /// The first frame of type `ty` whose `payload.requestId` is `request_id`.
    pub async fn response(&self, ty: &str, request_id: &str) -> Value {
        wait_for(|| {
            self.frames()
                .into_iter()
                .find(|m| m["type"] == ty && m["payload"]["requestId"] == request_id)
        })
        .await
    }

    pub async fn wait_for_type(&self, ty: &str, count: usize) -> Vec<Value> {
        wait_for(|| {
            let found = self.frames_of(ty);
            (found.len() >= count).then_some(found)
        })
        .await
    }
}

/// Polls `f` until it yields a value (5 s cap). Works under real and paused time.
pub async fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> T {
    for _ in 0..1000 {
        if let Some(v) = f() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("condition not met within 5 s");
}

/// Lets queued work run, then asserts nothing further arrives.
pub async fn settle() {
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

pub fn stored(id: &str, updated_at: &str) -> StoredAgentRecord {
    let mut record = StoredAgentRecord::new(id, "codex", "/work/repo", "2026-03-01T10:00:00.000Z");
    record.workspace_id = Some("ws_1".into());
    record.updated_at = updated_at.into();
    record.last_activity_at = Some(updated_at.into());
    record.title = paseo_store::Nullable::Value(format!("Agent {id}"));
    record
}
