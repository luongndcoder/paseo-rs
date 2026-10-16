//! Port of `session/agent-updates/agent-updates-service.test.ts`. Every dependency is an injected
//! in-memory fake, so each test fully controls the (agent, project, filter) triple the service
//! reasons about and asserts the emitted `agent_update` payloads.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use indexmap::IndexMap;
use paseo_agent_types::PersistenceHandle;
use paseo_core::agent_updates::{
    matches_agent_updates_filter, AgentUpdates, AgentUpdatesDeps, AgentUpdatesFilter,
    BeginSubscription,
};
use paseo_core::{Lifecycle, ManagedAgentView};
use paseo_directory::DirectorySync;
use paseo_store::StoredAgentRecord;
use serde_json::{json, Map, Value};
use tokio::sync::oneshot;

fn agent_payload(id: &str) -> AgentPayloadBuilder {
    AgentPayloadBuilder {
        id: id.into(),
        workspace_id: None,
        provider: "codex".into(),
        status: "running".into(),
        updated_at: "2026-03-01T12:00:00.000Z".into(),
        archived_at: None,
        labels: json!({}),
        requires_attention: false,
        effective_thinking: None,
        thinking: None,
        internal: false,
    }
}

struct AgentPayloadBuilder {
    id: String,
    workspace_id: Option<String>,
    provider: String,
    status: String,
    updated_at: String,
    archived_at: Option<String>,
    labels: Value,
    requires_attention: bool,
    effective_thinking: Option<String>,
    thinking: Option<String>,
    internal: bool,
}

impl AgentPayloadBuilder {
    fn workspace(mut self, id: &str) -> Self {
        self.workspace_id = Some(id.into());
        self
    }
    fn provider(mut self, p: &str) -> Self {
        self.provider = p.into();
        self
    }
    fn status(mut self, s: &str) -> Self {
        self.status = s.into();
        self
    }
    fn updated_at(mut self, s: &str) -> Self {
        self.updated_at = s.into();
        self
    }
    fn archived(mut self, s: &str) -> Self {
        self.archived_at = Some(s.into());
        self
    }
    fn labels(mut self, l: Value) -> Self {
        self.labels = l;
        self
    }
    fn attention(mut self, v: bool) -> Self {
        self.requires_attention = v;
        self
    }
    fn effective_thinking(mut self, v: &str) -> Self {
        self.effective_thinking = Some(v.into());
        self
    }
    fn internal(mut self) -> Self {
        self.internal = true;
        self
    }
    fn build(self) -> Value {
        let mut p = Map::new();
        p.insert("id".into(), self.id.clone().into());
        p.insert("provider".into(), self.provider.clone().into());
        p.insert("cwd".into(), "/tmp/repo".into());
        if let Some(ws) = &self.workspace_id {
            p.insert("workspaceId".into(), ws.clone().into());
        }
        if self.internal {
            p.insert("internal".into(), true.into());
        }
        p.insert("model".into(), Value::Null);
        p.insert(
            "thinkingOptionId".into(),
            self.thinking.clone().map_or(Value::Null, Value::String),
        );
        p.insert(
            "effectiveThinkingOptionId".into(),
            self.effective_thinking
                .clone()
                .map_or(Value::Null, Value::String),
        );
        p.insert("createdAt".into(), self.updated_at.clone().into());
        p.insert("updatedAt".into(), self.updated_at.clone().into());
        p.insert("lastUserMessageAt".into(), Value::Null);
        p.insert("status".into(), self.status.clone().into());
        p.insert("labels".into(), self.labels.clone());
        p.insert("requiresAttention".into(), self.requires_attention.into());
        p.insert(
            "archivedAt".into(),
            self.archived_at.map_or(Value::Null, Value::String),
        );
        Value::Object(p)
    }
}

fn project(key: &str) -> Value {
    json!({
        "projectKey": key,
        "projectName": "repo",
        "workspaceName": "main",
        "checkout": {
            "cwd": "/tmp/repo", "isGit": false, "currentBranch": null, "remoteUrl": null,
            "worktreeRoot": null, "isPaseoOwnedWorktree": false, "mainRepoRoot": null,
        },
    })
}

enum Build {
    Gated(oneshot::Receiver<Value>),
    Ready(Value),
}

type VisibleFn = Box<dyn Fn(&str) -> bool + Send + Sync>;

struct Fake {
    emitted: Mutex<Vec<Value>>,
    workspace_updates: Mutex<Vec<String>>,
    logged: Mutex<Vec<String>>,
    payloads: Mutex<HashMap<String, Value>>,
    queued: Mutex<VecDeque<Build>>,
    projects: Mutex<HashMap<String, Option<Value>>>,
    visible: Mutex<Option<VisibleFn>>,
    fail_with: Mutex<Option<String>>,
    use_projected: Mutex<bool>,
    directory: DirectorySync,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            emitted: Mutex::default(),
            workspace_updates: Mutex::default(),
            logged: Mutex::default(),
            payloads: Mutex::default(),
            queued: Mutex::default(),
            projects: Mutex::default(),
            visible: Mutex::default(),
            fail_with: Mutex::default(),
            use_projected: Mutex::default(),
            directory: DirectorySync::new("test-generation"),
        })
    }
}

#[async_trait]
impl AgentUpdatesDeps for Fake {
    fn emit(&self, message: Value) {
        self.emitted.lock().unwrap().push(message);
    }

    async fn enrich_agent_payload(&self, payload: Value) -> Result<Value, String> {
        if *self.use_projected.lock().unwrap() {
            return Ok(payload);
        }
        let queued = self.queued.lock().unwrap().pop_front();
        match queued {
            Some(Build::Ready(v)) => return Ok(v),
            Some(Build::Gated(rx)) => return rx.await.map_err(|e| e.to_string()),
            None => {}
        }
        if let Some(error) = self.fail_with.lock().unwrap().clone() {
            return Err(error);
        }
        let id = payload["id"].as_str().unwrap().to_owned();
        self.payloads
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("no payload registered for {id}"))
    }

    fn build_stored_agent_payload(&self, record: &StoredAgentRecord) -> Result<Value, String> {
        self.payloads
            .lock()
            .unwrap()
            .get(&record.id)
            .cloned()
            .ok_or_else(|| format!("no payload registered for {}", record.id))
    }

    fn is_provider_visible_to_client(&self, provider: &str) -> bool {
        self.visible
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|f| f(provider))
    }

    async fn build_project_placement(&self, workspace_id: &str) -> Result<Option<Value>, String> {
        Ok(self
            .projects
            .lock()
            .unwrap()
            .get(workspace_id)
            .cloned()
            .flatten())
    }

    async fn emit_workspace_update(&self, workspace_id: &str) -> Result<(), String> {
        self.workspace_updates
            .lock()
            .unwrap()
            .push(workspace_id.to_owned());
        Ok(())
    }

    fn sequence_agent_update(
        &self,
        payload: Value,
        entry: Option<Value>,
        agent_id: &str,
        include_sequence: bool,
        include_background: bool,
    ) -> Value {
        self.directory.sequence_agent_update(
            payload,
            entry,
            agent_id,
            include_sequence,
            include_background,
        )
    }

    fn log_error(&self, _context: &str, error: &str) {
        self.logged.lock().unwrap().push(error.to_owned());
    }
}

struct Harness {
    fake: Arc<Fake>,
    service: AgentUpdates,
}

impl Harness {
    fn new() -> Self {
        let fake = Fake::new();
        Self {
            service: AgentUpdates::new(fake.clone()),
            fake,
        }
    }

    fn register(&self, payload: Value) -> Value {
        self.register_with(payload, Some(project("proj-1")))
    }

    fn register_with(&self, payload: Value, project: Option<Value>) -> Value {
        self.fake
            .payloads
            .lock()
            .unwrap()
            .insert(payload["id"].as_str().unwrap().to_owned(), payload.clone());
        if let Some(ws) = payload.get("workspaceId").and_then(Value::as_str) {
            self.fake
                .projects
                .lock()
                .unwrap()
                .insert(ws.to_owned(), project);
        }
        payload
    }

    fn begin(&self, id: &str, filter: AgentUpdatesFilter) {
        self.service
            .begin_subscription(BeginSubscription::new(id).filter(filter));
    }

    fn begin_open(&self, id: &str) {
        self.begin(id, AgentUpdatesFilter::default());
        self.service.flush_bootstrapped(id, None);
    }

    fn updates(&self) -> Vec<Value> {
        self.fake
            .emitted
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m["type"] == "agent_update")
            .map(|m| m["payload"].clone())
            .collect()
    }

    /// `managed(id)`: a live agent view built from the registered payload.
    fn managed(&self, id: &str) -> ManagedAgentView {
        let payload = self
            .fake
            .payloads
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_else(|| agent_payload(id).build());
        let mut view = ManagedAgentView::for_test(id);
        view.provider = payload["provider"].as_str().unwrap().to_owned();
        view.config.provider = view.provider.clone();
        view.workspace_id = payload
            .get("workspaceId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        view.lifecycle = match payload["status"].as_str().unwrap() {
            "running" => Lifecycle::Running,
            "idle" => Lifecycle::Idle,
            "error" => Lifecycle::Error,
            "initializing" => Lifecycle::Initializing,
            _ => Lifecycle::Closed,
        };
        let labels: IndexMap<String, String> = payload["labels"]
            .as_object()
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        view.labels = labels;
        view
    }

    fn stored(&self, id: &str) -> StoredAgentRecord {
        StoredAgentRecord::new(id, "codex", "/tmp/repo", "2026-03-01T12:00:00.000Z")
    }
}

async fn yield_a_lot() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

fn filter() -> AgentUpdatesFilter {
    AgentUpdatesFilter::default()
}

// ---- matchesAgentUpdatesFilter ----

#[test]
fn no_filter_matches() {
    assert!(matches_agent_updates_filter(
        &agent_payload("a").build(),
        &project("proj-1"),
        None
    ));
}

#[test]
fn label_match_vs_mismatch() {
    let agent = agent_payload("a")
        .labels(json!({"surface": "voice"}))
        .build();
    let with = |value: &str| AgentUpdatesFilter {
        labels: Some(
            [("surface".to_owned(), value.to_owned())]
                .into_iter()
                .collect(),
        ),
        ..filter()
    };
    assert!(matches_agent_updates_filter(
        &agent,
        &project("proj-1"),
        Some(&with("voice"))
    ));
    assert!(!matches_agent_updates_filter(
        &agent,
        &project("proj-1"),
        Some(&with("cli"))
    ));
}

#[test]
fn archived_agents_are_excluded_unless_include_archived() {
    let agent = agent_payload("a")
        .archived("2026-03-02T00:00:00.000Z")
        .build();
    assert!(!matches_agent_updates_filter(
        &agent,
        &project("proj-1"),
        Some(&filter())
    ));
    let include = AgentUpdatesFilter {
        include_archived: Some(true),
        ..filter()
    };
    assert!(matches_agent_updates_filter(
        &agent,
        &project("proj-1"),
        Some(&include)
    ));
}

#[test]
fn internal_agents_are_excluded_unless_include_internal() {
    let agent = agent_payload("a").internal().build();
    assert!(!matches_agent_updates_filter(
        &agent,
        &project("proj-1"),
        None
    ));
    assert!(!matches_agent_updates_filter(
        &agent,
        &project("proj-1"),
        Some(&filter())
    ));
    let include = AgentUpdatesFilter {
        include_internal: Some(true),
        ..filter()
    };
    assert!(matches_agent_updates_filter(
        &agent,
        &project("proj-1"),
        Some(&include)
    ));
}

#[test]
fn thinking_option_filter_compares_the_resolved_option() {
    let high = agent_payload("a").effective_thinking("high").build();
    let with = |value: &str| AgentUpdatesFilter {
        thinking_option_id: Some(Some(value.to_owned())),
        ..filter()
    };
    assert!(matches_agent_updates_filter(
        &high,
        &project("proj-1"),
        Some(&with("high"))
    ));
    assert!(!matches_agent_updates_filter(
        &high,
        &project("proj-1"),
        Some(&with("low"))
    ));
    // Absent means "do not filter on thinking option".
    assert!(matches_agent_updates_filter(
        &high,
        &project("proj-1"),
        Some(&filter())
    ));
    // A null filter value asks for "no thinking option".
    let none = AgentUpdatesFilter {
        thinking_option_id: Some(None),
        ..filter()
    };
    assert!(!matches_agent_updates_filter(
        &high,
        &project("proj-1"),
        Some(&none)
    ));
    assert!(matches_agent_updates_filter(
        &agent_payload("b").build(),
        &project("proj-1"),
        Some(&none)
    ));
}

#[test]
fn status_filter() {
    let agent = agent_payload("a").status("running").build();
    let with = |status: &str| AgentUpdatesFilter {
        statuses: Some(vec![status.to_owned()]),
        ..filter()
    };
    assert!(matches_agent_updates_filter(
        &agent,
        &project("proj-1"),
        Some(&with("running"))
    ));
    assert!(!matches_agent_updates_filter(
        &agent,
        &project("proj-1"),
        Some(&with("closed"))
    ));
}

#[test]
fn requires_attention_filter() {
    let agent = agent_payload("a").attention(true).build();
    let with = |value: bool| AgentUpdatesFilter {
        requires_attention: Some(value),
        ..filter()
    };
    assert!(matches_agent_updates_filter(
        &agent,
        &project("proj-1"),
        Some(&with(true))
    ));
    assert!(!matches_agent_updates_filter(
        &agent,
        &project("proj-1"),
        Some(&with(false))
    ));
}

#[test]
fn project_keys_filter_ignoring_blank_entries() {
    let agent = agent_payload("a").build();
    let keys = |keys: &[&str]| AgentUpdatesFilter {
        project_keys: Some(keys.iter().map(|k| (*k).to_owned()).collect()),
        ..filter()
    };
    assert!(matches_agent_updates_filter(
        &agent,
        &project("proj-1"),
        Some(&keys(&["proj-1"]))
    ));
    assert!(!matches_agent_updates_filter(
        &agent,
        &project("proj-2"),
        Some(&keys(&["proj-1"]))
    ));
    // A whitespace-only key is trimmed away, leaving no constraint.
    assert!(matches_agent_updates_filter(
        &agent,
        &project("proj-2"),
        Some(&keys(&["  "]))
    ));
}

// ---- forwardLiveAgent ----

#[tokio::test]
async fn emits_snapshots_for_one_agent_in_forward_call_order() {
    let h = Harness::new();
    h.begin_open("sub");
    let idle = agent_payload("a").workspace("ws-1").status("idle").build();
    let running = agent_payload("a")
        .workspace("ws-1")
        .status("running")
        .build();
    h.register(running.clone());
    let (release, gate) = oneshot::channel();
    {
        let mut queued = h.fake.queued.lock().unwrap();
        queued.push_back(Build::Gated(gate));
        queued.push_back(Build::Ready(running));
    }
    let first = h.service.forward_live_agent(&h.managed("a"));
    let second = h.service.forward_live_agent(&h.managed("a"));
    yield_a_lot().await;
    assert!(
        h.updates().is_empty(),
        "the first build is still pending, the second waits behind it"
    );
    release.send(idle).unwrap();
    first.wait().await;
    second.wait().await;
    let statuses: Vec<String> = h
        .updates()
        .iter()
        .map(|u| u["agent"]["status"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(statuses, vec!["idle", "running"]);
}

#[tokio::test]
async fn emits_removal_after_an_earlier_queued_upsert_for_the_same_agent() {
    let h = Harness::new();
    h.begin_open("sub");
    let running = agent_payload("a")
        .workspace("ws-1")
        .status("running")
        .build();
    h.register(running.clone());
    let (release, gate) = oneshot::channel();
    h.fake.queued.lock().unwrap().push_back(Build::Gated(gate));
    let forward = h.service.forward_live_agent(&h.managed("a"));
    let removal = h.service.remove_agent("a");
    yield_a_lot().await;
    release.send(running).unwrap();
    forward.wait().await;
    removal.wait().await;
    let updates = h.updates();
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0]["kind"], "upsert");
    assert_eq!(updates[0]["agent"]["id"], "a");
    assert_eq!(updates[0]["project"], project("proj-1"));
    assert_eq!(updates[1], json!({"kind": "remove", "agentId": "a"}));
}

#[tokio::test]
async fn emits_an_upsert_for_a_matching_agent_and_updates_its_workspace() {
    let h = Harness::new();
    h.begin_open("sub");
    h.register(agent_payload("a").workspace("ws-1").build());
    h.service.forward_live_agent(&h.managed("a")).wait().await;
    let updates = h.updates();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["kind"], "upsert");
    assert_eq!(updates[0]["project"], project("proj-1"));
    assert_eq!(*h.fake.workspace_updates.lock().unwrap(), vec!["ws-1"]);
}

#[tokio::test]
async fn never_projects_mcp_credentials_into_ordinary_client_updates() {
    let h = Harness::new();
    h.begin_open("sub");
    h.register(agent_payload("a").workspace("ws-1").build());
    *h.fake.use_projected.lock().unwrap() = true;
    let mut agent = h.managed("a");
    let servers = json!({
        "hub": {
            "type": "http",
            "url": "https://hub.test/mcp/executions/execution-1",
            "headers": { "Authorization": "Bearer execution-secret" },
        },
    });
    agent.config.mcp_servers = Some(servers.clone());
    let mut metadata = Map::new();
    metadata.insert("conversationId".into(), "conversation-a".into());
    metadata.insert("mcpServers".into(), servers);
    agent.persistence = Some(PersistenceHandle {
        provider: "codex".into(),
        session_id: "session-a".into(),
        native_handle: None,
        metadata: Some(metadata),
    });
    h.service.forward_live_agent(&agent).wait().await;
    let updates = h.updates();
    assert_eq!(updates.len(), 1);
    let update = &updates[0];
    assert_eq!(update["kind"], "upsert");
    assert_eq!(update["agent"]["id"], "a");
    let agent = update["agent"].as_object().unwrap();
    assert!(!agent.contains_key("config"));
    assert!(!agent.contains_key("mcpServers"));
    assert_eq!(
        update["agent"]["persistence"]["metadata"],
        json!({"conversationId": "conversation-a"})
    );
    assert!(!update.to_string().contains("execution-secret"));
}

#[tokio::test]
async fn emits_a_remove_when_the_agents_workspace_resolves_to_no_project() {
    let h = Harness::new();
    h.begin_open("sub");
    h.register_with(agent_payload("a").workspace("ws-1").build(), None);
    h.service.forward_live_agent(&h.managed("a")).wait().await;
    assert_eq!(h.updates(), vec![json!({"kind": "remove", "agentId": "a"})]);
}

#[tokio::test]
async fn emits_a_remove_when_the_agent_does_not_match_the_filter() {
    let h = Harness::new();
    h.begin(
        "sub",
        AgentUpdatesFilter {
            statuses: Some(vec!["closed".into()]),
            ..filter()
        },
    );
    h.service.flush_bootstrapped("sub", None);
    h.register(
        agent_payload("a")
            .workspace("ws-1")
            .status("running")
            .build(),
    );
    h.service.forward_live_agent(&h.managed("a")).wait().await;
    assert_eq!(h.updates(), vec![json!({"kind": "remove", "agentId": "a"})]);
}

#[tokio::test]
async fn with_no_subscription_emits_no_agent_update_but_still_updates_the_workspace() {
    let h = Harness::new();
    h.register(agent_payload("a").workspace("ws-1").build());
    h.service.forward_live_agent(&h.managed("a")).wait().await;
    assert!(h.updates().is_empty());
    assert_eq!(*h.fake.workspace_updates.lock().unwrap(), vec!["ws-1"]);
}

#[tokio::test]
async fn drops_an_upsert_whose_provider_is_not_visible_to_the_client() {
    let h = Harness::new();
    h.begin_open("sub");
    *h.fake.visible.lock().unwrap() = Some(Box::new(|_| false));
    // The default visibility is read when the subscription is registered.
    h.service
        .begin_subscription(BeginSubscription::new("hidden"));
    h.service.flush_bootstrapped("hidden", None);
    h.service.clear_subscription("sub");
    h.register(agent_payload("a").workspace("ws-1").provider("pi").build());
    h.service.forward_live_agent(&h.managed("a")).wait().await;
    assert!(h.updates().is_empty());
    // The workspace-update tail fires regardless of visibility.
    assert_eq!(*h.fake.workspace_updates.lock().unwrap(), vec!["ws-1"]);
}

#[tokio::test]
async fn swallows_and_logs_a_build_error_without_throwing() {
    let h = Harness::new();
    h.begin_open("sub");
    *h.fake.fail_with.lock().unwrap() = Some("boom".into());
    h.service.forward_live_agent(&h.managed("a")).wait().await;
    assert_eq!(h.fake.logged.lock().unwrap().len(), 1);
    assert!(h.updates().is_empty());
}

// ---- emitStoredRecord ----

#[tokio::test]
async fn emit_stored_record_returns_the_payload_and_emits_an_upsert_when_matching() {
    let h = Harness::new();
    h.begin_open("sub");
    h.register(agent_payload("a").workspace("ws-1").build());
    let payload = h.service.emit_stored_record(&h.stored("a")).await.unwrap();
    assert_eq!(payload["id"], "a");
    let updates = h.updates();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["kind"], "upsert");
}

#[tokio::test]
async fn emit_stored_record_emits_a_remove_when_no_project_resolves() {
    let h = Harness::new();
    h.begin_open("sub");
    h.register_with(agent_payload("a").workspace("ws-1").build(), None);
    h.service.emit_stored_record(&h.stored("a")).await.unwrap();
    assert_eq!(h.updates(), vec![json!({"kind": "remove", "agentId": "a"})]);
}

#[tokio::test]
async fn emit_stored_record_emits_a_remove_when_the_record_no_longer_matches_the_filter() {
    let h = Harness::new();
    h.begin(
        "sub",
        AgentUpdatesFilter {
            include_archived: Some(false),
            ..filter()
        },
    );
    h.service.flush_bootstrapped("sub", None);
    h.register(
        agent_payload("a")
            .workspace("ws-1")
            .archived("2026-03-02T00:00:00.000Z")
            .build(),
    );
    h.service.emit_stored_record(&h.stored("a")).await.unwrap();
    assert_eq!(h.updates(), vec![json!({"kind": "remove", "agentId": "a"})]);
}

#[tokio::test]
async fn emit_stored_record_returns_the_payload_but_emits_nothing_without_a_subscription() {
    let h = Harness::new();
    h.register(agent_payload("a").workspace("ws-1").build());
    let payload = h.service.emit_stored_record(&h.stored("a")).await.unwrap();
    assert_eq!(payload["id"], "a");
    assert!(h.updates().is_empty());
}

// ---- bootstrap buffering ----

#[tokio::test]
async fn buffers_updates_while_bootstrapping_and_replays_them_on_flush() {
    let h = Harness::new();
    h.begin("sub", filter());
    h.register(agent_payload("a").workspace("ws-1").build());
    h.service.forward_live_agent(&h.managed("a")).wait().await;
    assert!(h.updates().is_empty(), "buffered, not emitted yet");
    h.service.flush_bootstrapped("sub", None);
    let updates = h.updates();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["agent"]["id"], "a");
}

#[tokio::test]
async fn keeps_only_the_latest_buffered_update_per_agent() {
    let h = Harness::new();
    h.begin("sub", filter());
    h.register(
        agent_payload("a")
            .workspace("ws-1")
            .status("running")
            .build(),
    );
    h.service.forward_live_agent(&h.managed("a")).wait().await;
    h.register(
        agent_payload("a")
            .workspace("ws-1")
            .status("closed")
            .build(),
    );
    h.service.forward_live_agent(&h.managed("a")).wait().await;
    h.service.flush_bootstrapped("sub", None);
    let updates = h.updates();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["agent"]["status"], "closed");
}

fn ms(iso: &str) -> i64 {
    paseo_store::time::parse_js_date(iso).unwrap()
}

#[tokio::test]
async fn skips_a_buffered_upsert_that_is_older_than_the_snapshot() {
    let h = Harness::new();
    h.begin("sub", filter());
    h.register(
        agent_payload("stale")
            .workspace("ws-1")
            .updated_at("2026-03-01T00:00:00.000Z")
            .build(),
    );
    h.service
        .forward_live_agent(&h.managed("stale"))
        .wait()
        .await;
    h.register(
        agent_payload("fresh")
            .workspace("ws-1")
            .updated_at("2026-03-05T00:00:00.000Z")
            .build(),
    );
    h.service
        .forward_live_agent(&h.managed("fresh"))
        .wait()
        .await;
    let snapshot: HashMap<String, i64> = [
        ("stale".to_owned(), ms("2026-03-02T00:00:00.000Z")), // snapshot newer: drop
        ("fresh".to_owned(), ms("2026-03-02T00:00:00.000Z")), // update newer: keep
    ]
    .into_iter()
    .collect();
    h.service.flush_bootstrapped("sub", Some(&snapshot));
    let updates = h.updates();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["agent"]["id"], "fresh");
}

#[tokio::test]
async fn replays_a_buffered_live_update_with_the_same_revision_as_the_snapshot() {
    let h = Harness::new();
    h.begin("sub", filter());
    h.register(
        agent_payload("equal-revision")
            .workspace("ws-1")
            .status("running")
            .updated_at("2026-03-02T00:00:00.000Z")
            .build(),
    );
    h.service
        .forward_live_agent(&h.managed("equal-revision"))
        .wait()
        .await;
    let snapshot: HashMap<String, i64> =
        [("equal-revision".to_owned(), ms("2026-03-02T00:00:00.000Z"))]
            .into_iter()
            .collect();
    h.service.flush_bootstrapped("sub", Some(&snapshot));
    let updates = h.updates();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["agent"]["id"], "equal-revision");
    assert_eq!(updates[0]["agent"]["status"], "running");
}

#[tokio::test]
async fn a_removed_agent_is_always_replayed_even_against_a_snapshot() {
    let h = Harness::new();
    h.begin("sub", filter());
    h.service.remove_agent("a").wait().await;
    assert!(h.updates().is_empty(), "buffered");
    let snapshot: HashMap<String, i64> = [("a".to_owned(), ms("2030-01-01T00:00:00.000Z"))]
        .into_iter()
        .collect();
    h.service.flush_bootstrapped("sub", Some(&snapshot));
    assert_eq!(h.updates(), vec![json!({"kind": "remove", "agentId": "a"})]);
}

#[tokio::test]
async fn does_not_buffer_an_upsert_for_a_provider_that_is_not_visible() {
    let h = Harness::new();
    h.service
        .begin_subscription(BeginSubscription::new("sub").provider_visible(Arc::new(|_| false)));
    h.register(agent_payload("a").workspace("ws-1").provider("pi").build());
    h.service.forward_live_agent(&h.managed("a")).wait().await;
    h.service.flush_bootstrapped("sub", None);
    assert!(h.updates().is_empty());
}

#[tokio::test]
async fn visibility_is_checked_again_at_replay() {
    let h = Harness::new();
    let visible = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let gate = visible.clone();
    h.service
        .begin_subscription(
            BeginSubscription::new("sub").provider_visible(Arc::new(move |_| {
                gate.load(std::sync::atomic::Ordering::SeqCst)
            })),
        );
    h.register(agent_payload("a").workspace("ws-1").provider("pi").build());
    h.service.forward_live_agent(&h.managed("a")).wait().await;
    // The client's capabilities changed while the snapshot was being built.
    visible.store(false, std::sync::atomic::Ordering::SeqCst);
    h.service.flush_bootstrapped("sub", None);
    assert!(h.updates().is_empty());
}

#[tokio::test]
async fn sequenced_subscriptions_stamp_generation_and_seq() {
    let h = Harness::new();
    h.service
        .begin_subscription(BeginSubscription::new("sub").sync(true));
    h.service.flush_bootstrapped("sub", None);
    h.register(agent_payload("a").workspace("ws-1").build());
    h.service.forward_live_agent(&h.managed("a")).wait().await;
    let updates = h.updates();
    assert_eq!(updates[0]["generation"], "test-generation");
    assert_eq!(updates[0]["seq"], 1);
}

// ---- subscription lifecycle ----

#[tokio::test]
async fn attention_eligibility_follows_the_active_directory_subscription_filter() {
    let h = Harness::new();
    h.register(
        agent_payload("matching")
            .workspace("ws-1")
            .labels(json!({"team": "a"}))
            .build(),
    );
    h.register(
        agent_payload("excluded")
            .workspace("ws-2")
            .labels(json!({"team": "b"}))
            .build(),
    );
    assert!(!h
        .service
        .includes_live_agent(&h.managed("matching"), None)
        .await
        .unwrap());
    h.begin(
        "sub",
        AgentUpdatesFilter {
            labels: Some([("team".to_owned(), "a".to_owned())].into_iter().collect()),
            ..filter()
        },
    );
    assert!(h
        .service
        .includes_live_agent(&h.managed("matching"), None)
        .await
        .unwrap());
    assert!(!h
        .service
        .includes_live_agent(&h.managed("excluded"), None)
        .await
        .unwrap());
    // Restricting to other subscription ids excludes it.
    let other: HashSet<String> = ["nope".to_owned()].into_iter().collect();
    assert!(!h
        .service
        .includes_live_agent(&h.managed("matching"), Some(&other))
        .await
        .unwrap());
    h.service.clear_subscription("sub");
    assert!(!h
        .service
        .includes_live_agent(&h.managed("matching"), None)
        .await
        .unwrap());
}

#[tokio::test]
async fn flush_bootstrapped_is_a_no_op_for_a_stale_subscription_id() {
    let h = Harness::new();
    h.begin("sub", filter());
    h.register(agent_payload("a").workspace("ws-1").build());
    h.service.forward_live_agent(&h.managed("a")).wait().await;
    h.service.flush_bootstrapped("other-sub", None);
    assert!(h.updates().is_empty(), "still buffering");
    h.service.flush_bootstrapped("sub", None);
    assert_eq!(h.updates().len(), 1);
    // A second flush replays nothing.
    h.service.flush_bootstrapped("sub", None);
    assert_eq!(h.updates().len(), 1);
}

#[test]
fn clear_subscription_only_clears_the_named_subscription() {
    let h = Harness::new();
    h.begin("sub", filter());
    h.service.clear_subscription("other-sub");
    assert!(h.service.has_subscription());
    h.service.clear_subscription("sub");
    assert!(!h.service.has_subscription());
}

#[test]
fn dispose_drops_the_subscription() {
    let h = Harness::new();
    h.begin("sub", filter());
    assert!(h.service.has_subscription());
    h.service.dispose();
    assert!(!h.service.has_subscription());
}

#[tokio::test]
async fn remove_agent_is_a_no_op_without_a_subscription() {
    let h = Harness::new();
    h.service.remove_agent("a").wait().await;
    assert!(h.updates().is_empty());
}

#[tokio::test]
async fn remove_agent_emits_a_remove_for_a_live_subscription() {
    let h = Harness::new();
    h.begin_open("sub");
    h.service.remove_agent("a").wait().await;
    assert_eq!(h.updates(), vec![json!({"kind": "remove", "agentId": "a"})]);
}

#[tokio::test]
async fn an_idle_session_skips_agent_hydration_while_preserving_workspace_updates() {
    let h = Harness::new();
    let mut agent = h.managed("unobserved");
    agent.workspace_id = Some("workspace".into());
    // No payload is registered: attempting to hydrate this agent would fail.
    h.service.forward_live_agent(&agent).wait().await;
    assert!(h.fake.logged.lock().unwrap().is_empty());
    assert!(h.updates().is_empty());
    assert_eq!(*h.fake.workspace_updates.lock().unwrap(), vec!["workspace"]);
}
