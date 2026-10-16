//! `registerSession` (`agent-manager.ts:3568-3681`): turn a provider session into a live agent.
//!
//! Order matters. Startup history is read BEFORE the agent is published (a failure closes the
//! session and registers nothing). After publication: previous status, imported rows,
//! `refresh_runtime_info(emit=false)`, persist with the title, emit state (skipped when
//! `publish_when_ready`), `refresh_session_state`, lifecycle `idle`, touch unless `restoring`,
//! persist, emit, and only then `subscribe()`, so an adapter that creates its channel lazily
//! loses pre-subscribe events as in TS (PARITY-QUIRK(pre-subscribe-events-lost)).

use std::sync::atomic::Ordering;
use std::sync::Arc;

use indexmap::IndexMap;
use paseo_agent_types::{AgentTimelineItem, AgentUsage, PersistenceHandle};
use paseo_provider_api::{AgentSession, AgentSessionConfig};
use paseo_store::AgentOwner;
use paseo_timeline::store::SeedOptions;
use paseo_timeline::{project_agent_message, Row, TimelineState};
use tracing::warn;

use crate::agent_actor::hydrate::collect_history;
use crate::agent_actor::{attach_persistence_cwd, spawn_actor, ActorHandle, SpawnParams};
use crate::bridge::SessionRef;
use crate::error::{CoreError, Result};
use crate::hooks::PersistOptions;
use crate::ids::validate_agent_id;
use crate::manager::AgentManager;
use crate::model::{AttentionState, Broadcast, Lifecycle, ManagedAgentView};
use crate::time::iso8601_from_millis;

/// Options of `registerSession`.
#[derive(Debug, Clone, Default)]
pub struct RegisterOptions {
    pub created_at_ms: Option<i64>,
    pub updated_at_ms: Option<i64>,
    pub last_user_message_at_ms: Option<i64>,
    pub labels: Option<IndexMap<String, String>>,
    pub timeline: Vec<AgentTimelineItem>,
    pub timeline_rows: Vec<Row>,
    pub timeline_next_seq: Option<i64>,
    pub persistence: Option<PersistenceHandle>,
    pub history_primed: Option<bool>,
    pub last_usage: Option<AgentUsage>,
    pub last_error: Option<String>,
    pub attention: AttentionState,
    /// Bringing a known agent back: timestamps and attention come from what was recorded, and
    /// installing the session is not activity.
    pub restoring: bool,
    pub initial_title: Option<String>,
    pub publish_when_ready: bool,
    pub workspace_id: Option<String>,
    pub owner: Option<AgentOwner>,
}

impl AgentManager {
    pub async fn register_session(
        &self,
        session: Box<dyn AgentSession>,
        config: AgentSessionConfig,
        agent_id: &str,
        options: RegisterOptions,
    ) -> Result<Arc<ManagedAgentView>> {
        let _tracked = self.inner.registrations.track();
        let session: SessionRef = Arc::from(session);
        let mut registered = false;
        let result = self
            .register_session_inner(&session, config, agent_id, options, &mut registered)
            .await;
        if result.is_err() && !registered {
            if let Err(err) = session.close().await {
                warn!(%err, "Failed to close unregistered agent session");
            }
        }
        result
    }

    async fn register_session_inner(
        &self,
        session: &SessionRef,
        config: AgentSessionConfig,
        agent_id: &str,
        options: RegisterOptions,
        registered: &mut bool,
    ) -> Result<Arc<ManagedAgentView>> {
        self.assert_accepting_registrations()?;
        let id = validate_agent_id(agent_id, "registerSession")?;
        if self.inner.registry.get(&id).is_some() {
            return Err(CoreError::msg(format!("Agent with id {id} already exists")));
        }
        let title = self
            .resolve_initial_persisted_title(&id, &config, &options)
            .await?;
        let now = self.clock().now_ms();
        let history_primed = options
            .history_primed
            .unwrap_or_else(|| self.inner.registry.has_retained_timeline(&id));
        let initial_timeline = session.initial_timeline().to_vec();
        // Startup history is read BEFORE the agent is published: a provider failure leaves the
        // session unregistered, so the caller closes it and nothing is registered.
        let startup_history = if !initial_timeline.is_empty() && !history_primed {
            Some(collect_history(session.stream_history().await?))
        } else {
            None
        };
        self.assert_accepting_registrations()?;
        let retained = self.inner.registry.take_retained_timeline(&id);
        let timeline = build_timeline(retained, &options, now);
        let persistence = {
            let described = session.describe_persistence().await.ok().flatten();
            attach_persistence_cwd(options.persistence.clone().or(described), &config.cwd)
        };
        let view = ManagedAgentView {
            id: id.clone(),
            provider: config.provider.clone(),
            cwd: config.cwd.clone(),
            workspace_id: options.workspace_id.clone(),
            owner: options.owner.clone(),
            capabilities: session.capabilities().clone(),
            internal: config.internal.unwrap_or(false),
            config,
            runtime_info: None,
            created_at_ms: options.created_at_ms.unwrap_or(now),
            updated_at_ms: options.updated_at_ms.unwrap_or(now),
            available_modes: Vec::new(),
            features: Vec::new(),
            current_mode_id: None,
            pending_permissions: IndexMap::new(),
            pending_replacement: false,
            persistence,
            history_primed,
            last_user_message_at_ms: options.last_user_message_at_ms,
            active_turn_id: None,
            active_turn_started_at_ms: None,
            last_usage: options.last_usage.clone(),
            last_error: options.last_error.clone(),
            attention: options.attention.clone(),
            lifecycle: Lifecycle::Initializing,
            active_foreground_turn_id: None,
            labels: options.labels.clone().unwrap_or_default(),
            has_run: false,
            pending_run_start: None,
        };

        let handle = self.publish_actor(view, session.clone(), timeline);
        *registered = true;

        if !initial_timeline.is_empty() {
            handle
                .ask(move |a| {
                    match startup_history {
                        // Legacy and imported chats need their existing history before the
                        // startup rows, which the replay already carries.
                        Some(history) => a.apply_prime_history(history, &Broadcast::Off),
                        None => {
                            for item in initial_timeline {
                                if let Some(item) = project_agent_message(&item) {
                                    a.record_timeline(item, Default::default());
                                }
                            }
                        }
                    }
                    a.refresh_session_persistence();
                })
                .await?;
        }
        self.refresh_runtime_info(&handle, false).await;
        self.assert_registration_active(&handle)?;
        self.persist_snapshot(
            &handle,
            PersistOptions {
                title: Some(title),
                internal: None,
            },
        )
        .await?;
        self.assert_registration_active(&handle)?;
        if !options.publish_when_ready {
            handle.ask(|a| a.emit_state(false)).await?;
        }
        self.refresh_session_state(&handle, false).await;
        self.assert_registration_active(&handle)?;
        let restoring = options.restoring;
        handle
            .ask(move |a| {
                a.view.lifecycle = Lifecycle::Idle;
                // Stamping over a restored timestamp would rewrite the workspace's "last used".
                if !restoring {
                    a.touch_updated_at();
                }
            })
            .await?;
        self.persist_snapshot(&handle, PersistOptions::default())
            .await?;
        self.assert_registration_active(&handle)?;
        handle
            .ask(|a| {
                a.emit_state(false);
                a.subscribe_to_session();
                Arc::new(a.snapshot())
            })
            .await
    }

    fn publish_actor(
        &self,
        view: ManagedAgentView,
        session: SessionRef,
        timeline: TimelineState,
    ) -> ActorHandle {
        let id = view.id.clone();
        let generation = self.inner.next_generation.fetch_add(1, Ordering::SeqCst);
        let handle = spawn_actor(SpawnParams {
            view,
            session,
            timeline,
            coalesce_window: self.inner.options.coalesce_window,
            ctx: self.inner.ctx.clone(),
            generation,
        });
        self.inner
            .registry
            .agents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, handle.clone());
        handle
    }

    /// `resolveInitialPersistedTitle`: an existing record's title, else the trimmed config
    /// title, else the caller's fallback.
    async fn resolve_initial_persisted_title(
        &self,
        id: &str,
        config: &AgentSessionConfig,
        options: &RegisterOptions,
    ) -> Result<Option<String>> {
        if let Some(persister) = &self.inner.ctx.persister {
            if let Some(existing) = persister.stored_title(id).await.map_err(CoreError::msg)? {
                return Ok(existing);
            }
        }
        let explicit = config
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_owned);
        Ok(explicit.or_else(|| options.initial_title.clone()))
    }
}

/// Timeline seed: explicit items/rows/next seq, else the retained timeline of a previous
/// registration of this id, else a fresh one.
fn build_timeline(
    retained: Option<TimelineState>,
    options: &RegisterOptions,
    now: i64,
) -> TimelineState {
    let explicit = !options.timeline.is_empty()
        || !options.timeline_rows.is_empty()
        || options.timeline_next_seq.is_some();
    if explicit {
        let stamp = options
            .updated_at_ms
            .or(options.created_at_ms)
            .unwrap_or(now);
        return TimelineState::initialize(SeedOptions {
            items: options.timeline.clone(),
            rows: options.timeline_rows.clone(),
            next_seq: options.timeline_next_seq,
            timestamp: Some(iso8601_from_millis(stamp)),
            ..SeedOptions::default()
        });
    }
    retained.unwrap_or_else(|| {
        TimelineState::initialize(SeedOptions {
            timestamp: Some(iso8601_from_millis(now)),
            ..SeedOptions::default()
        })
    })
}
