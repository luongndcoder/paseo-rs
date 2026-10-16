//! Agent updates service (`session/agent-updates/agent-updates-service.ts`): filtered agent
//! snapshots and live `agent_update` frames for independent observers.
//!
//! Rules kept from TS:
//! - A subscription registers (bootstrapping) BEFORE its snapshot is read; updates that arrive
//!   meanwhile are buffered last-write-wins per agent and replayed by [`AgentUpdates::flush_bootstrapped`]
//!   after the response went out. A buffered upsert strictly older than the snapshot's revision
//!   is dropped (equal revisions replay); removals always replay.
//! - Provider visibility is checked when an update is buffered and again at replay, through the
//!   owning source's capabilities.
//! - An agent that no longer matches the filter, or whose workspace resolves to no project,
//!   produces a `remove`.
//! - Live updates run on a per-agent serialized job chain; the agent payload is snapshotted
//!   SYNCHRONOUSLY before the job is enqueued, enrichment (title, archive state from the store)
//!   happens inside the job. Failures are logged and swallowed.
//! - MCP credentials never reach a payload (`to_agent_payload` projects neither `config` nor
//!   persistence `mcpServers`).
//!
//! Emit callbacks run while the service state is locked, so they must not block or call back in.

mod filter;

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use indexmap::IndexMap;
use paseo_store::time::parse_js_date;
use paseo_store::StoredAgentRecord;
use serde_json::{json, Map, Value};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::agent_payload::to_agent_payload;
use crate::model::ManagedAgentView;

pub use filter::{matches_agent_updates_filter, AgentUpdatesFilter};

/// Session services the updates service reads and writes through.
#[async_trait]
pub trait AgentUpdatesDeps: Send + Sync + 'static {
    /// Default sink of subscriptions that bring no emitter of their own.
    fn emit(&self, _message: Value) {}

    /// Workspace visibility (`AgentDirectory.includes`): background workspaces hide their agents
    /// from readers that did not ask for them.
    async fn includes_workspace(&self, _agent: &Value, _include_background: bool) -> bool {
        true
    }

    /// `enrichAgentPayload`: title and `archivedAt` from the store.
    async fn enrich_agent_payload(&self, payload: Value) -> Result<Value, String>;

    fn build_stored_agent_payload(&self, record: &StoredAgentRecord) -> Result<Value, String>;

    /// `isProviderVisibleToClient` for subscriptions without their own predicate.
    fn is_provider_visible_to_client(&self, _provider: &str) -> bool {
        true
    }

    async fn build_project_placement(&self, workspace_id: &str) -> Result<Option<Value>, String>;

    async fn emit_workspace_update(&self, workspace_id: &str) -> Result<(), String>;

    /// `directorySync.sequenceAgentUpdate`. `entry` is `{agent, project}` or `None` for removals.
    fn sequence_agent_update(
        &self,
        payload: Value,
        entry: Option<Value>,
        agent_id: &str,
        include_sequence: bool,
        include_background: bool,
    ) -> Value;

    fn log_error(&self, context: &str, error: &str) {
        tracing::error!(error, "{context}");
    }
}

pub type EmitFn = Arc<dyn Fn(Value) + Send + Sync>;
pub type VisibleFn = Arc<dyn Fn(&str) -> bool + Send + Sync>;

pub struct BeginSubscription {
    pub subscription_id: String,
    pub is_provider_visible: Option<VisibleFn>,
    pub emit: Option<EmitFn>,
    pub filter: Option<AgentUpdatesFilter>,
    pub sync_enabled: bool,
}

impl BeginSubscription {
    pub fn new(subscription_id: impl Into<String>) -> Self {
        Self {
            subscription_id: subscription_id.into(),
            is_provider_visible: None,
            emit: None,
            filter: None,
            sync_enabled: false,
        }
    }

    pub fn filter(mut self, filter: AgentUpdatesFilter) -> Self {
        self.filter = Some(filter);
        self
    }

    pub fn sync(mut self, enabled: bool) -> Self {
        self.sync_enabled = enabled;
        self
    }

    pub fn emit(mut self, emit: EmitFn) -> Self {
        self.emit = Some(emit);
        self
    }

    pub fn provider_visible(mut self, visible: VisibleFn) -> Self {
        self.is_provider_visible = Some(visible);
        self
    }
}

struct Sub {
    /// Distinguishes a re-registered subscription from the one an in-flight job started with.
    generation: u64,
    emit: EmitFn,
    sync_enabled: bool,
    filter: Option<AgentUpdatesFilter>,
    is_provider_visible: VisibleFn,
    bootstrapping: bool,
    pending: IndexMap<String, Value>,
}

#[derive(Clone)]
struct Observer {
    id: String,
    generation: u64,
    sync_enabled: bool,
    filter: Option<AgentUpdatesFilter>,
    is_provider_visible: VisibleFn,
}

struct TailEntry {
    token: u64,
    done: watch::Receiver<bool>,
}

#[derive(Default)]
struct State {
    subs: IndexMap<String, Sub>,
    tails: HashMap<String, TailEntry>,
    next_generation: u64,
    next_token: u64,
}

struct Inner {
    deps: Arc<dyn AgentUpdatesDeps>,
    state: Mutex<State>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Completion of an enqueued update. Awaiting is optional; the work runs regardless.
pub struct UpdateDone(Option<JoinHandle<()>>);

impl UpdateDone {
    fn ready() -> Self {
        Self(None)
    }

    pub async fn wait(self) {
        if let Some(handle) = self.0 {
            let _ = handle.await;
        }
    }
}

/// Sends `done` when dropped, so a panicking job never wedges the chain behind it.
struct DoneGuard(watch::Sender<bool>);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

#[derive(Clone)]
pub struct AgentUpdates {
    inner: Arc<Inner>,
}

fn target_id(update: &Value) -> String {
    let key = if update.get("kind").and_then(Value::as_str) == Some("remove") {
        update.get("agentId")
    } else {
        update.get("agent").and_then(|a| a.get("id"))
    };
    key.and_then(Value::as_str).unwrap_or_default().to_owned()
}

impl AgentUpdates {
    pub fn new(deps: Arc<dyn AgentUpdatesDeps>) -> Self {
        Self {
            inner: Arc::new(Inner {
                deps,
                state: Mutex::new(State::default()),
            }),
        }
    }

    fn sequence(
        &self,
        observer: &Observer,
        payload: Value,
        agent: Option<&Value>,
        project: Option<&Value>,
        agent_id: &str,
    ) -> Value {
        let entry = match (agent, project) {
            (Some(agent), Some(project)) => {
                let mut map = Map::new();
                map.insert("agent".into(), agent.clone());
                map.insert("project".into(), project.clone());
                Some(Value::Object(map))
            }
            _ => None,
        };
        self.inner.deps.sequence_agent_update(
            payload,
            entry,
            agent_id,
            observer.sync_enabled,
            observer
                .filter
                .as_ref()
                .and_then(|f| f.include_background)
                .unwrap_or(false),
        )
    }

    /// `bufferOrEmit`.
    fn buffer_or_emit(&self, subscription_id: &str, generation: u64, payload: Value) {
        let mut state = self.inner.lock();
        let Some(sub) = state.subs.get_mut(subscription_id) else {
            return;
        };
        if sub.generation != generation {
            return;
        }
        if payload.get("kind").and_then(Value::as_str) == Some("upsert") {
            let provider = payload
                .get("agent")
                .and_then(|a| a.get("provider"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !(sub.is_provider_visible)(provider) {
                return;
            }
        }
        if sub.bootstrapping {
            sub.pending.insert(target_id(&payload), payload);
            return;
        }
        (sub.emit)(json!({ "type": "agent_update", "payload": payload }));
    }

    /// Registers a subscription in the bootstrapping state. Replaces one with the same id.
    pub fn begin_subscription(&self, input: BeginSubscription) {
        let deps = self.inner.deps.clone();
        let mut state = self.inner.lock();
        state.next_generation += 1;
        let generation = state.next_generation;
        let default_emit: EmitFn = {
            let deps = deps.clone();
            Arc::new(move |message| deps.emit(message))
        };
        let default_visible: VisibleFn = Arc::new(move |p| deps.is_provider_visible_to_client(p));
        state.subs.insert(
            input.subscription_id,
            Sub {
                generation,
                emit: input.emit.unwrap_or(default_emit),
                sync_enabled: input.sync_enabled,
                filter: input.filter,
                is_provider_visible: input.is_provider_visible.unwrap_or(default_visible),
                bootstrapping: true,
                pending: IndexMap::new(),
            },
        );
    }

    /// Ends bootstrapping and replays what was buffered, dropping upserts strictly older than the
    /// snapshot the client received. No-op for a subscription that is gone or already flushed.
    pub fn flush_bootstrapped(
        &self,
        subscription_id: &str,
        snapshot_updated_at_by_agent_id: Option<&HashMap<String, i64>>,
    ) {
        // Replay under the lock so a concurrent `buffer_or_emit` cannot emit a newer update ahead
        // of an older buffered one (TS flushes synchronously on one thread).
        let mut state = self.inner.lock();
        let Some(sub) = state.subs.get_mut(subscription_id) else {
            return;
        };
        if !sub.bootstrapping {
            return;
        }
        sub.bootstrapping = false;
        let pending = std::mem::take(&mut sub.pending);
        for (_, payload) in pending {
            if payload.get("kind").and_then(Value::as_str) == Some("upsert") {
                let agent = payload.get("agent");
                let id = agent
                    .and_then(|a| a.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let snapshot_at = snapshot_updated_at_by_agent_id.and_then(|m| m.get(id));
                let updated_at = agent
                    .and_then(|a| a.get("updatedAt"))
                    .and_then(Value::as_str)
                    .and_then(parse_js_date);
                // `Date.parse(..) < snapshotUpdatedAt`: NaN compares false, so it replays.
                if let (Some(snapshot_at), Some(updated_at)) = (snapshot_at, updated_at) {
                    if updated_at < *snapshot_at {
                        continue;
                    }
                }
                let provider = agent
                    .and_then(|a| a.get("provider"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !(sub.is_provider_visible)(provider) {
                    continue;
                }
            }
            (sub.emit)(json!({ "type": "agent_update", "payload": payload }));
        }
    }

    pub fn clear_subscription(&self, subscription_id: &str) {
        self.inner.lock().subs.shift_remove(subscription_id);
    }

    pub fn has_subscription(&self) -> bool {
        !self.inner.lock().subs.is_empty()
    }

    pub fn dispose(&self) {
        self.inner.lock().subs.clear();
    }

    fn observers(&self, only: Option<&std::collections::HashSet<String>>) -> Vec<Observer> {
        self.inner
            .lock()
            .subs
            .iter()
            .filter(|(id, _)| only.is_none_or(|set| set.contains(*id)))
            .map(|(id, sub)| Observer {
                id: id.clone(),
                generation: sub.generation,
                sync_enabled: sub.sync_enabled,
                filter: sub.filter.clone(),
                is_provider_visible: sub.is_provider_visible.clone(),
            })
            .collect()
    }

    /// Would any (selected) subscription list this live agent? Attention eligibility follows the
    /// active directory subscription filter.
    pub async fn includes_live_agent(
        &self,
        agent: &ManagedAgentView,
        subscription_ids: Option<&std::collections::HashSet<String>>,
    ) -> Result<bool, String> {
        let observers = self.observers(subscription_ids);
        if observers.is_empty() {
            return Ok(false);
        }
        let deps = &self.inner.deps;
        let payload = deps.enrich_agent_payload(to_agent_payload(agent)).await?;
        let project = match workspace_id_of(&payload) {
            Some(ws) => deps.build_project_placement(ws).await?,
            None => None,
        };
        let mut visible_workspace = Vec::with_capacity(observers.len());
        for observer in &observers {
            visible_workspace.push(
                deps.includes_workspace(
                    &payload,
                    observer
                        .filter
                        .as_ref()
                        .and_then(|f| f.include_background)
                        .unwrap_or(false),
                )
                .await,
            );
        }
        let provider = payload
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(project) = project else {
            return Ok(false);
        };
        Ok(observers.iter().zip(visible_workspace).any(|(o, visible)| {
            visible
                && (o.is_provider_visible)(provider)
                && matches_agent_updates_filter(&payload, &project, o.filter.as_ref())
        }))
    }

    /// `publishPayload`: every observer gets an upsert, or a remove when the agent is not (or no
    /// longer) in its view.
    async fn publish_payload(&self, payload: &Value) -> Result<(), String> {
        let observers = self.observers(None);
        if observers.is_empty() {
            return Ok(());
        }
        let deps = &self.inner.deps;
        let project = match workspace_id_of(payload) {
            Some(ws) => deps.build_project_placement(ws).await?,
            None => None,
        };
        let agent_id = payload
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        for observer in observers {
            let include_background = observer
                .filter
                .as_ref()
                .and_then(|f| f.include_background)
                .unwrap_or(false);
            let matches = match &project {
                Some(project) => {
                    deps.includes_workspace(payload, include_background).await
                        && matches_agent_updates_filter(payload, project, observer.filter.as_ref())
                }
                None => false,
            };
            let update = match (&project, matches) {
                (Some(project), true) => {
                    json!({ "kind": "upsert", "agent": payload, "project": project })
                }
                _ => json!({ "kind": "remove", "agentId": agent_id }),
            };
            // PARITY-QUIRK(remove-update-still-observes-agent): a `remove` for an agent that does not
            // match this observer still hands `{agent, project}` to the sequencer, which records the
            // agent in the shared directory; only a missing project or workspace drops it.
            let sequenced = self.sequence(
                &observer,
                update,
                Some(payload),
                project.as_ref(),
                &agent_id,
            );
            self.buffer_or_emit(&observer.id, observer.generation, sequenced);
        }
        Ok(())
    }

    /// `emitStoredRecord`: publishes a stored-only agent and returns its payload. Errors from
    /// the payload build or the placement lookup propagate.
    pub async fn emit_stored_record(&self, record: &StoredAgentRecord) -> Result<Value, String> {
        let payload = self.inner.deps.build_stored_agent_payload(record)?;
        self.publish_payload(&payload).await?;
        Ok(payload)
    }

    async fn emit_live_agent_update(&self, payload: Value) {
        let result: Result<(), String> = async {
            if self.has_subscription() {
                let enriched = self
                    .inner
                    .deps
                    .enrich_agent_payload(payload.clone())
                    .await?;
                self.publish_payload(&enriched).await?;
            }
            if let Some(ws) = workspace_id_of(&payload) {
                self.inner.deps.emit_workspace_update(ws).await?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            self.inner
                .deps
                .log_error("Failed to emit agent update", &error);
        }
    }

    /// Runs `job` after every earlier job of `agent_id` finished (a failure is not propagated).
    fn enqueue(
        &self,
        agent_id: &str,
        job: impl Future<Output = ()> + Send + 'static,
    ) -> UpdateDone {
        let (tx, rx) = watch::channel(false);
        let (token, previous) = {
            let mut state = self.inner.lock();
            state.next_token += 1;
            let token = state.next_token;
            let previous = state
                .tails
                .insert(agent_id.to_owned(), TailEntry { token, done: rx });
            (token, previous)
        };
        let inner = self.inner.clone();
        let agent_id = agent_id.to_owned();
        let handle = tokio::spawn(async move {
            let _done = DoneGuard(tx);
            if let Some(previous) = previous {
                let mut done = previous.done;
                let _ = done.wait_for(|d| *d).await;
            }
            job.await;
            let mut state = inner.lock();
            if state.tails.get(&agent_id).is_some_and(|t| t.token == token) {
                state.tails.remove(&agent_id);
            }
        });
        UpdateDone(Some(handle))
    }

    /// `forwardLiveAgent`. The payload is snapshotted now, in the caller's step.
    pub fn forward_live_agent(&self, agent: &ManagedAgentView) -> UpdateDone {
        if !self.has_subscription() {
            let Some(workspace_id) = agent.workspace_id.clone().filter(|w| !w.is_empty()) else {
                return UpdateDone::ready();
            };
            let this = self.clone();
            return self.enqueue(&agent.id, async move {
                if let Err(error) = this.inner.deps.emit_workspace_update(&workspace_id).await {
                    this.inner
                        .deps
                        .log_error("Failed to emit workspace update", &error);
                }
            });
        }
        let payload = to_agent_payload(agent);
        let this = self.clone();
        self.enqueue(&agent.id, async move {
            this.emit_live_agent_update(payload).await;
        })
    }

    /// `removeAgent`: a `remove` for every subscription, after the agent's queued updates.
    pub fn remove_agent(&self, agent_id: &str) -> UpdateDone {
        let this = self.clone();
        let id = agent_id.to_owned();
        self.enqueue(agent_id, async move {
            for observer in this.observers(None) {
                let update = json!({ "kind": "remove", "agentId": id });
                let sequenced = this.sequence(&observer, update, None, None, &id);
                this.buffer_or_emit(&observer.id, observer.generation, sequenced);
            }
        })
    }
}

fn workspace_id_of(payload: &Value) -> Option<&str> {
    payload
        .get("workspaceId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}
