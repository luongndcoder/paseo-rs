//! Tests of the WP8 step A foundation itself: event routing priority and staging, admission,
//! attention edges, persistence seams, the shutdown barrier, close, the retained timeline and
//! the published read views (decision Q17: one test per TS caller that could observe mid-await
//! state).

mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use paseo_agent_types::{
    AgentStreamEvent, AgentTimelineItem, AttentionReason, PermissionKind, PermissionRequest,
};
use paseo_core::mock::{boxed, Gate, MockSession, ScriptedBehavior};
use paseo_core::{
    AttentionState, CoreError, Lifecycle, ManagedAgentView, ManagerEvent, PersistOptions,
    RegisterOptions, RunOpts, SnapshotPersister, SubscribeOpts,
};
use paseo_provider_api::{PromptInput, ProviderError, TurnHandle};
use paseo_timeline::store::FetchOptions;

const ID: &str = AGENT_IDS[0];

fn permission(id: &str) -> PermissionRequest {
    PermissionRequest {
        id: id.into(),
        provider: "codex".into(),
        name: "Read".into(),
        kind: PermissionKind::Tool,
        title: None,
        description: None,
        input: None,
        detail: None,
        suggestions: None,
        actions: None,
        metadata: None,
    }
}

fn permission_requested(id: &str, turn: &str) -> AgentStreamEvent {
    AgentStreamEvent::PermissionRequested {
        provider: "codex".into(),
        request: Box::new(permission(id)),
        turn_id: Some(turn.into()),
    }
}

// ---- admission and staging ----

#[tokio::test(start_paused = true)]
async fn admission_fails_while_a_run_slot_is_occupied() {
    let h = Harness::new().await;
    h.agent(ID, &MockSession::new("codex")).await;

    let _first = h
        .manager
        .stream_agent(ID, PromptInput::from("one"), RunOpts::default())
        .await
        .unwrap();
    let error = h
        .manager
        .stream_agent(ID, PromptInput::from("two"), RunOpts::default())
        .await
        .err()
        .expect("second admission is refused");
    assert_eq!(
        error.to_string(),
        format!("Agent {ID} already has an active run")
    );
    assert!(
        h.manager.has_in_flight_run(ID),
        "an unpolled pending run counts as in flight"
    );
}

#[tokio::test(start_paused = true)]
async fn admission_clears_the_previous_error_and_unknown_agents_are_rejected() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    session.push_event(turn_failed(Some("t0"), "old failure"));
    settle().await;
    assert_eq!(h.view(ID).last_error.as_deref(), Some("old failure"));

    let _stream = h
        .manager
        .stream_agent(ID, PromptInput::from("again"), RunOpts::default())
        .await
        .unwrap();
    assert_eq!(h.view(ID).last_error, None);

    let missing = AGENT_IDS[3];
    let error = h
        .manager
        .stream_agent(missing, PromptInput::from("x"), RunOpts::default())
        .await
        .err()
        .unwrap();
    assert_eq!(error.to_string(), format!("Unknown agent '{missing}'"));
    let error = h
        .manager
        .stream_agent("nope", PromptInput::from("x"), RunOpts::default())
        .await
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "requireAgent: agentId must be a UUID");
}

#[tokio::test(start_paused = true)]
async fn dropping_an_unpolled_stream_settles_its_pending_run() {
    // DEVIATION(stream-drop-settles-run), decision Q18.
    let h = Harness::new().await;
    h.agent(ID, &MockSession::new("codex")).await;
    let stream = h
        .manager
        .stream_agent(ID, PromptInput::from("never polled"), RunOpts::default())
        .await
        .unwrap();
    assert!(h.manager.has_in_flight_run(ID));

    drop(stream);
    settle().await;
    assert!(!h.manager.has_in_flight_run(ID));
    assert_eq!(h.view(ID).lifecycle, Lifecycle::Idle);
    h.manager
        .stream_agent(ID, PromptInput::from("next"), RunOpts::default())
        .await
        .expect("the agent is not locked");
}

#[tokio::test(start_paused = true)]
async fn cancelling_a_stream_start_midway_does_not_leave_the_agent_locked() {
    let (entered, release) = (Gate::new(), Gate::new());
    let (entered2, release2) = (entered.clone(), release.clone());
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_start(move |session, prompt, options| {
            let (entered, release) = (entered2.clone(), release2.clone());
            boxed(async move {
                session.record_start(prompt, options);
                entered.open();
                release.wait().await;
                Ok(TurnHandle {
                    turn_id: "turn-late".into(),
                })
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    let mut stream = h
        .manager
        .stream_agent(ID, PromptInput::from("x"), RunOpts::default())
        .await
        .unwrap();
    let task = tokio::spawn(async move { stream.next().await });
    entered.wait().await;
    assert!(h.manager.has_in_flight_run(ID));

    task.abort();
    let _ = task.await;
    settle().await;
    assert!(
        !h.manager.has_in_flight_run(ID),
        "dropping the half-started stream settles its run"
    );
    release.open();
    h.manager
        .stream_agent(ID, PromptInput::from("y"), RunOpts::default())
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn abandoning_a_started_stream_drops_its_waiter_and_the_provider_terminal_still_finalizes() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    let mut stream = h
        .manager
        .stream_agent(ID, PromptInput::from("x"), RunOpts::default())
        .await
        .unwrap();
    assert_eq!(
        stream.next().await.unwrap().unwrap().type_name(),
        "turn_started"
    );
    assert_eq!(h.view(ID).lifecycle, Lifecycle::Running);

    drop(stream);
    settle().await;
    session.push_event(assistant("late", "codex", Some("turn-1")));
    session.push_event(turn_completed("turn-1"));
    settle().await;

    let view = h.view(ID);
    assert_eq!(view.lifecycle, Lifecycle::Idle);
    assert_eq!(view.active_foreground_turn_id, None);
    assert!(!h.manager.has_in_flight_run(ID));
    assert!(h.items(ID).await.contains(&assistant_item("late")));
}

#[tokio::test(start_paused = true)]
async fn events_staged_under_a_pending_run_are_dropped_when_it_settles_without_starting() {
    // PARITY-QUIRK(rewind-staged-events-dropped): rewind parks a pending run as a lock.
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    let stream = h
        .manager
        .stream_agent(ID, PromptInput::from("parked"), RunOpts::default())
        .await
        .unwrap();

    session.push_event(assistant("staged text", "codex", None));
    settle().await;
    assert!(h.items(ID).await.is_empty(), "staged, not processed");
    drop(stream);
    settle().await;
    assert!(h.items(ID).await.is_empty(), "never replayed");
    assert!(h.events.timeline_streams(ID).is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_accept_block_orders_turn_started_before_the_prompt_and_reuses_the_staged_echo() {
    // Everything below is pushed before `start_turn` returns, so the actor stages it.
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_start(|session, prompt, options| {
            boxed(async move {
                session.record_start(prompt, options);
                session.push_event(turn_started("turn-1"));
                session.push_event(AgentStreamEvent::Timeline {
                    item: Box::new(AgentTimelineItem::UserMessage {
                        text: "hello".into(),
                        message_id: Some("provider-msg-1".into()),
                        client_message_id: Some("client-1".into()),
                    }),
                    provider: "codex".into(),
                    turn_id: Some("turn-1".into()),
                    timestamp: None,
                });
                session.push_event(assistant("answer", "codex", Some("turn-1")));
                session.push_event(turn_completed("turn-1"));
                Ok(TurnHandle {
                    turn_id: "turn-1".into(),
                })
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    h.events.clear();

    h.manager
        .run_agent(
            ID,
            PromptInput::from("hello"),
            RunOpts::with_client_message_id("client-1"),
        )
        .await
        .unwrap();
    settle().await;

    let rows = h.rows(ID).await;
    assert_eq!(
        rows.len(),
        2,
        "one canonical prompt, the echo is not a second row"
    );
    assert!(matches!(
        &rows[0].item,
        AgentTimelineItem::UserMessage { message_id: Some(m), client_message_id: Some(c), .. }
            if m == "client-1" && c == "client-1"
    ));
    assert_eq!(
        rows[0].provider_message_id.as_deref(),
        Some("provider-msg-1")
    );
    assert_eq!(rows[0].turn_id.as_deref(), Some("turn-1"));
    assert_eq!(rows[1].item, assistant_item("answer"));

    let kinds: Vec<String> = h
        .events
        .streams(ID)
        .iter()
        .map(|e| match stream_event(e) {
            AgentStreamEvent::Timeline { item, .. } => format!("timeline:{}", item.type_name()),
            other => other.type_name().to_owned(),
        })
        .collect();
    let started: Vec<_> = kinds
        .iter()
        .enumerate()
        .filter(|(_, k)| *k == "turn_started")
        .collect();
    assert_eq!(
        started.len(),
        1,
        "the provider's own start for the foreground turn is suppressed: {kinds:?}"
    );
    let prompt_at = kinds
        .iter()
        .position(|k| k == "timeline:user_message")
        .unwrap();
    assert!(
        started[0].0 < prompt_at,
        "turn_started precedes the canonical prompt: {kinds:?}"
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "timeline:user_message")
            .count(),
        1
    );
}

// ---- attention ----

#[tokio::test(start_paused = true)]
async fn attention_is_edge_triggered_and_the_emitted_snapshot_already_has_it() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    h.events.clear();

    session.push_event(turn_started("t1"));
    session.push_event(turn_completed("t1"));
    settle().await;
    assert_eq!(attention_reasons(&h.hooks), vec![AttentionReason::Finished]);
    let idle_state = h
        .events
        .states(ID)
        .into_iter()
        .find(|v| v.lifecycle == Lifecycle::Idle)
        .expect("idle state");
    assert!(
        matches!(
            idle_state.attention,
            AttentionState::Required {
                reason: AttentionReason::Finished,
                ..
            }
        ),
        "attention is set before the snapshot is cloned"
    );

    // Level is not edge: more idle emits, even after clearing, do not notify again.
    h.manager.notify_agent_state(ID).await;
    h.manager.clear_agent_attention(ID).await.unwrap();
    h.manager.notify_agent_state(ID).await;
    settle().await;
    assert_eq!(attention_reasons(&h.hooks), vec![AttentionReason::Finished]);
    assert_eq!(h.view(ID).attention, AttentionState::None);
}

#[tokio::test(start_paused = true)]
async fn entering_error_notifies_once_and_staying_in_error_does_not_repeat() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;

    session.push_event(turn_failed(Some("t1"), "boom"));
    settle().await;
    assert_eq!(attention_reasons(&h.hooks), vec![AttentionReason::Error]);
    h.manager.clear_agent_attention(ID).await.unwrap();
    h.manager.notify_agent_state(ID).await;
    settle().await;
    assert_eq!(attention_reasons(&h.hooks), vec![AttentionReason::Error]);
    assert_eq!(h.view(ID).lifecycle, Lifecycle::Error);
}

#[tokio::test(start_paused = true)]
async fn only_the_first_pending_permission_notifies() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    session.push_event(turn_started("t1"));
    session.push_event(permission_requested("p1", "t1"));
    session.push_event(permission_requested("p2", "t1"));
    settle().await;
    assert_eq!(
        attention_reasons(&h.hooks),
        vec![AttentionReason::Permission]
    );
    assert_eq!(h.view(ID).pending_permissions.len(), 2);
    assert_eq!(
        h.view(ID).attention,
        AttentionState::None,
        "permission never sets attention state"
    );

    for id in ["p1", "p2"] {
        session.push_event(AgentStreamEvent::PermissionResolved {
            provider: "codex".into(),
            request_id: id.into(),
            resolution: paseo_agent_types::PermissionResponse::Deny {
                selected_action_id: None,
                message: None,
                interrupt: None,
            },
            turn_id: Some("t1".into()),
        });
    }
    session.push_event(permission_requested("p3", "t1"));
    settle().await;
    assert_eq!(
        attention_reasons(&h.hooks),
        vec![AttentionReason::Permission, AttentionReason::Permission],
        "an empty map notifies again"
    );
}

#[tokio::test(start_paused = true)]
async fn mark_agent_unread_only_applies_to_a_finished_read_agent() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;

    h.manager.mark_agent_unread(ID).await.unwrap();
    assert!(matches!(
        h.view(ID).attention,
        AttentionState::Required {
            reason: AttentionReason::Finished,
            ..
        }
    ));
    let error = h.manager.mark_agent_unread(ID).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("Agent is no longer finished and read: {ID}")
    );

    h.manager.clear_agent_attention(ID).await.unwrap();
    session.push_event(turn_started("t1"));
    settle().await;
    assert!(
        h.manager.mark_agent_unread(ID).await.is_err(),
        "a running agent cannot be marked unread"
    );
}

// ---- persistence seam ----

#[tokio::test(start_paused = true)]
async fn explicit_persists_do_not_double_write_and_internal_agents_never_persist() {
    let h = Harness::with(HarnessOptions {
        with_persister: true,
        ..HarnessOptions::default()
    })
    .await;
    let session = MockSession::new("codex");
    h.agent_with(ID, &session, "codex", RegisterOptions::default(), |c| {
        c.title = Some("  Keep  ".into())
    })
    .await;
    h.manager.flush().await;
    let registered = h.persister.snapshots.lock().unwrap().clone();
    assert_eq!(
        registered.len(),
        2,
        "persist before the first emit, and after going idle"
    );
    assert_eq!(
        registered[0].1.title,
        Some(Some("Keep".to_owned())),
        "trimmed config title"
    );
    assert_eq!(registered[1].1, PersistOptions::default());

    session.push_event(turn_started("t1"));
    session.push_event(turn_completed("t1"));
    settle().await;
    h.manager.flush().await;
    let before = h.persister.snapshots.lock().unwrap().len();
    h.manager.clear_agent_attention(ID).await.unwrap();
    h.manager.flush().await;
    assert_eq!(
        h.persister.snapshots.lock().unwrap().len(),
        before + 1,
        "clear persists explicitly and then emits with persist=false"
    );

    let internal = MockSession::new("codex");
    h.agent_with(
        AGENT_IDS[1],
        &internal,
        "codex",
        RegisterOptions::default(),
        |c| c.internal = Some(true),
    )
    .await;
    internal.push_event(turn_started("t1"));
    internal.push_event(turn_completed("t1"));
    settle().await;
    h.manager.flush().await;
    assert!(
        h.persister
            .snapshots
            .lock()
            .unwrap()
            .iter()
            .all(|(v, _)| v.id != AGENT_IDS[1]),
        "internal agents never persist"
    );
}

struct StoredTitlePersister;

#[async_trait]
impl SnapshotPersister for StoredTitlePersister {
    async fn persist(
        &self,
        _agent: &ManagedAgentView,
        _options: PersistOptions,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn stored_title(&self, _agent_id: &str) -> Result<Option<Option<String>>, String> {
        Ok(Some(Some("Stored title".into())))
    }
}

#[tokio::test(start_paused = true)]
async fn an_existing_stored_title_wins_over_the_config_title() {
    let recorded = Arc::new(RecordingPersister::default());
    struct Both(Arc<RecordingPersister>);
    #[async_trait]
    impl SnapshotPersister for Both {
        async fn persist(
            &self,
            agent: &ManagedAgentView,
            options: PersistOptions,
        ) -> Result<(), String> {
            self.0.persist(agent, options).await
        }
        async fn stored_title(&self, id: &str) -> Result<Option<Option<String>>, String> {
            StoredTitlePersister.stored_title(id).await
        }
    }
    let both = Arc::new(Both(recorded.clone()));
    let h = Harness::with(HarnessOptions {
        tweak: Box::new(move |o| o.persister = Some(both)),
        ..HarnessOptions::default()
    })
    .await;
    h.agent_with(
        ID,
        &MockSession::new("codex"),
        "codex",
        RegisterOptions::default(),
        |c| c.title = Some("Config title".into()),
    )
    .await;
    assert_eq!(
        recorded.snapshots.lock().unwrap()[0].1.title,
        Some(Some("Stored title".to_owned()))
    );
}

// ---- registration, shutdown, close ----

#[tokio::test(start_paused = true)]
async fn registration_emits_initializing_then_idle_and_rejects_duplicates() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    let lifecycles: Vec<Lifecycle> = h.events.states(ID).iter().map(|v| v.lifecycle).collect();
    assert_eq!(lifecycles, vec![Lifecycle::Initializing, Lifecycle::Idle]);

    let duplicate = MockSession::new("codex");
    let error = h
        .manager
        .register_session(
            Box::new(duplicate.clone()),
            paseo_provider_api::AgentSessionConfig::new("codex", "/tmp"),
            ID,
            RegisterOptions::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("Agent with id {ID} already exists")
    );
    assert!(
        duplicate.is_closed(),
        "a session that never registered is closed"
    );

    let error = h
        .manager
        .register_session(
            Box::new(MockSession::new("codex")),
            paseo_provider_api::AgentSessionConfig::new("codex", "/tmp"),
            "not-a-guid",
            RegisterOptions::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "registerSession: agentId must be a UUID");
}

#[tokio::test(start_paused = true)]
async fn publish_when_ready_skips_the_first_emit() {
    let h = Harness::new().await;
    h.agent_with(
        ID,
        &MockSession::new("codex"),
        "codex",
        RegisterOptions {
            publish_when_ready: true,
            ..RegisterOptions::default()
        },
        |_| {},
    )
    .await;
    let lifecycles: Vec<Lifecycle> = h.events.states(ID).iter().map(|v| v.lifecycle).collect();
    assert_eq!(lifecycles, vec![Lifecycle::Idle]);
}

#[tokio::test(start_paused = true)]
async fn restoring_does_not_touch_the_restored_timestamps() {
    let h = Harness::new().await;
    h.agent_with(
        ID,
        &MockSession::new("codex"),
        "codex",
        RegisterOptions {
            restoring: true,
            created_at_ms: Some(1_000),
            updated_at_ms: Some(2_000),
            ..RegisterOptions::default()
        },
        |_| {},
    )
    .await;
    let view = h.view(ID);
    assert_eq!((view.created_at_ms, view.updated_at_ms), (1_000, 2_000));
}

#[tokio::test(start_paused = true)]
async fn prepare_for_shutdown_rejects_new_registrations_and_closes_their_sessions() {
    let h = Harness::new().await;
    h.manager.prepare_for_shutdown();
    let session = MockSession::new("codex");
    let error = h
        .manager
        .register_session(
            Box::new(session.clone()),
            paseo_provider_api::AgentSessionConfig::new("codex", "/tmp"),
            ID,
            RegisterOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, CoreError::ShuttingDown));
    assert_eq!(error.to_string(), "Agent manager is shutting down");
    assert!(session.is_closed());
    assert!(h.manager.get_agent(ID).is_none());
}

struct GatedPersister {
    entered: Gate,
    release: Gate,
}

#[async_trait]
impl SnapshotPersister for GatedPersister {
    async fn persist(
        &self,
        _agent: &ManagedAgentView,
        _options: PersistOptions,
    ) -> Result<(), String> {
        self.entered.open();
        self.release.wait().await;
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn a_registration_that_crosses_the_shutdown_barrier_fails_and_flush_for_shutdown_joins_it() {
    let (entered, release) = (Gate::new(), Gate::new());
    let persister = Arc::new(GatedPersister {
        entered: entered.clone(),
        release: release.clone(),
    });
    let h = Harness::with(HarnessOptions {
        tweak: Box::new(move |o| o.persister = Some(persister)),
        ..HarnessOptions::default()
    })
    .await;
    let manager = h.manager.clone();
    let session = MockSession::new("codex");
    let registering = tokio::spawn({
        let session = session.clone();
        async move {
            manager
                .register_session(
                    Box::new(session),
                    paseo_provider_api::AgentSessionConfig::new("codex", "/tmp"),
                    ID,
                    RegisterOptions {
                        history_primed: Some(true),
                        ..RegisterOptions::default()
                    },
                )
                .await
        }
    });
    entered.wait().await;
    h.manager.prepare_for_shutdown();

    let manager = h.manager.clone();
    let flush = tokio::spawn(async move { manager.flush_for_shutdown().await });
    settle().await;
    assert!(
        !flush.is_finished(),
        "flush_for_shutdown joins the in-flight registration"
    );

    release.open();
    let error = registering.await.unwrap().unwrap_err();
    assert!(matches!(error, CoreError::ShuttingDown), "{error}");
    flush.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn concurrent_close_calls_share_one_in_flight_close() {
    let entered = Gate::new();
    let release = Gate::new();
    let (entered2, release2) = (entered.clone(), release.clone());
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_close(move |session| {
            session.with_state(|s| s.closed = true);
            let (entered, release) = (entered2.clone(), release2.clone());
            boxed(async move {
                entered.open();
                release.wait().await;
                Ok(())
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;

    let first = tokio::spawn({
        let manager = h.manager.clone();
        async move { manager.close_agent(ID).await }
    });
    entered.wait().await;
    let second = tokio::spawn({
        let manager = h.manager.clone();
        async move { manager.close_agent(ID).await }
    });
    settle().await;
    assert!(!second.is_finished());
    let waiter = tokio::spawn({
        let manager = h.manager.clone();
        async move { manager.wait_for_agent_close(ID).await }
    });
    settle().await;
    assert!(
        !waiter.is_finished(),
        "wait_for_agent_close waits for the close"
    );

    release.open();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    waiter.await.unwrap();
    assert!(h.manager.get_agent(ID).is_none());
    // A later close of the gone agent is a no-op.
    h.manager.close_agent(ID).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_failed_session_close_keeps_the_runtime_and_the_next_attempt_retries() {
    let attempts = Arc::new(std::sync::Mutex::new(0u32));
    let counter = attempts.clone();
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_close(move |_| {
            let attempt = {
                let mut n = counter.lock().unwrap();
                *n += 1;
                *n
            };
            boxed(async move {
                if attempt == 1 {
                    Err(ProviderError::provider("close rejected"))
                } else {
                    Ok(())
                }
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    h.events.clear();

    let error = h.manager.close_agent(ID).await.unwrap_err();
    assert_eq!(error.to_string(), "close rejected");
    settle().await;
    assert!(h.manager.get_agent(ID).is_some(), "the runtime is retained");
    assert!(
        h.events
            .states(ID)
            .iter()
            .all(|v| v.lifecycle != Lifecycle::Closed),
        "no closed snapshot is published"
    );

    h.manager.close_agent(ID).await.unwrap();
    assert!(h.manager.get_agent(ID).is_none());
}

#[tokio::test(start_paused = true)]
async fn close_cancels_foreground_waiters_with_the_close_reason_and_emits_the_closed_state() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    let mut stream = h
        .manager
        .stream_agent(ID, PromptInput::from("long"), RunOpts::default())
        .await
        .unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.type_name(), "turn_started");
    h.events.clear();

    h.manager.close_agent(ID).await.unwrap();
    let canceled = stream
        .next()
        .await
        .unwrap()
        .expect("the waiter is cancelled, not dropped");
    assert!(matches!(
        canceled,
        AgentStreamEvent::TurnCanceled { ref reason, ref turn_id, .. }
            if reason == "agent closed" && turn_id.as_deref() == Some("turn-1")
    ));
    assert_eq!(stream.next().await.unwrap(), None);
    settle().await;

    let closed = h
        .events
        .states(ID)
        .into_iter()
        .last()
        .expect("closed state");
    assert_eq!(closed.lifecycle, Lifecycle::Closed);
    assert_eq!(closed.active_foreground_turn_id, None);
    assert!(h
        .hooks
        .plugin_emits
        .lock()
        .unwrap()
        .iter()
        .any(|(hook, _)| hook == "agent.closed"));
    let error = h
        .manager
        .stream_agent(ID, PromptInput::from("late"), RunOpts::default())
        .await
        .err()
        .unwrap();
    assert_eq!(error.to_string(), format!("Unknown agent '{ID}'"));
}

#[tokio::test(start_paused = true)]
async fn the_timeline_survives_close_and_a_new_registration_continues_the_same_epoch() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    h.manager
        .append_timeline_item(ID, assistant_item("first"))
        .await
        .unwrap();
    let before = h
        .manager
        .fetch_timeline(ID, FetchOptions::default())
        .await
        .unwrap();
    h.manager.close_agent(ID).await.unwrap();

    // `history_primed` defaults to "a timeline already exists", so no history is re-read.
    let again = MockSession::new("codex");
    h.manager
        .register_session(
            Box::new(again),
            paseo_provider_api::AgentSessionConfig::new("codex", "/tmp/paseo-core-test"),
            ID,
            RegisterOptions::default(),
        )
        .await
        .unwrap();
    assert!(h.view(ID).history_primed);
    let (_, seq) = h
        .manager
        .append_timeline_item(ID, assistant_item("second"))
        .await
        .map(|(s, e)| (e, s))
        .unwrap();
    let after = h
        .manager
        .fetch_timeline(ID, FetchOptions::default())
        .await
        .unwrap();
    assert_eq!(after.epoch, before.epoch);
    assert_eq!(seq, 2, "seq continues");
}

#[tokio::test(start_paused = true)]
async fn the_shell_command_hook_fires_for_completed_remote_git_commands() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    let shell = |command: &str, status| {
        let detail = paseo_agent_types::ToolCallDetail::Shell {
            command: command.into(),
            cwd: None,
            output: None,
            exit_code: None,
        };
        AgentTimelineItem::tool_call(paseo_agent_types::ToolCallItem::new(
            "c", "shell", status, detail,
        ))
    };
    use paseo_agent_types::ToolCallStatus as S;
    session.push_event(timeline_event(
        shell("git push origin main", S::Running),
        "codex",
        None,
    ));
    session.push_event(timeline_event(
        shell("git status", S::Completed),
        "codex",
        None,
    ));
    session.push_event(timeline_event(
        shell("gh pr merge 7", S::Completed),
        "codex",
        None,
    ));
    settle().await;
    assert_eq!(
        *h.hooks.workspace_cwds.lock().unwrap(),
        vec!["/tmp/paseo-core-test".to_owned()]
    );
}

// ---- Q17: published reads ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reads_see_an_awaited_mutation_immediately() {
    // TS callers read `getAgent` right after awaiting a mutation (`startAgentRun` reads
    // `hasInFlightRun` after `streamAgent`; sessions read state after `clearAgentAttention`).
    // The actor publishes its view before it answers, so the read can never be stale, even with
    // the caller on another thread.
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    for round in 0..50 {
        let stream = h
            .manager
            .stream_agent(ID, PromptInput::from("x"), RunOpts::default())
            .await
            .unwrap();
        assert!(
            h.manager.has_in_flight_run(ID),
            "round {round}: admission is visible"
        );
        drop(stream);
        for _ in 0..50 {
            if !h.manager.has_in_flight_run(ID) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(!h.manager.has_in_flight_run(ID));

        h.manager.mark_agent_unread(ID).await.unwrap();
        assert!(h.view(ID).attention.requires_attention(), "round {round}");
        h.manager.clear_agent_attention(ID).await.unwrap();
        assert!(!h.view(ID).attention.requires_attention(), "round {round}");
    }
}

#[tokio::test(start_paused = true)]
async fn start_agent_run_decides_replace_from_a_read_that_includes_the_pending_admission() {
    // `startOrReplaceRun`: `replaceRunning && hasInFlightRun(agentId)` right after another caller
    // admitted a run that has not started its turn yet.
    let h = Harness::new().await;
    let (session, _) = steering_session("codex");
    h.agent(ID, &session).await;
    let _pending = h
        .manager
        .stream_agent(ID, PromptInput::from("pending"), RunOpts::default())
        .await
        .unwrap();
    assert!(h.manager.has_in_flight_run(ID));

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        start_agent_run(
            &h.manager,
            ID,
            "replace it",
            StartRunOptions {
                replace_running: true,
                ..StartRunOptions::default()
            },
        ),
    )
    .await;
    // The replace path cancels the pending run (the unpolled stream never gets a turn id), so the
    // new run is admitted instead of failing with "already has an active run".
    assert!(
        matches!(outcome, Ok(Ok(Disposition::TurnStarted))),
        "{outcome:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn wait_for_agent_run_start_judges_the_state_at_registration_not_a_later_read() {
    // The run starts and finishes while the waiter is being set up: TS (synchronous) sees the
    // pending run, then the started state event, and resolves. A later state read would see an
    // idle agent with no run and report "run finished before starting".
    let h = Harness::new().await;
    h.agent(ID, &MockSession::auto_completing("codex")).await;
    let mut run = h
        .manager
        .stream_agent(ID, PromptInput::from("fast"), RunOpts::default())
        .await
        .unwrap();
    let drain = tokio::spawn(async move { while let Ok(Some(_)) = run.next().await {} });
    h.manager
        .wait_for_agent_run_start(ID, None)
        .await
        .expect("resolves on the started frame");
    drain.await.unwrap();

    // With no run at all the wait fails immediately.
    let error = h
        .manager
        .wait_for_agent_run_start(ID, None)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), format!("Agent {ID} has no pending run"));
}

#[tokio::test(start_paused = true)]
async fn wait_for_agent_event_reports_a_permission_raised_while_it_waits() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    session.push_event(turn_started("t1"));
    settle().await;

    let manager = h.manager.clone();
    let wait =
        tokio::spawn(async move { manager.wait_for_agent_event(ID, Default::default()).await });
    settle().await;
    assert!(!wait.is_finished());
    session.push_event(permission_requested("p1", "t1"));
    let result = wait.await.unwrap().unwrap();
    assert_eq!(result.permission.map(|p| p.id), Some("p1".into()));
}

#[tokio::test(start_paused = true)]
async fn a_wait_is_aborted_by_its_signal() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    session.push_event(turn_started("t1"));
    settle().await;

    let signal = paseo_core::AbortSignal::new();
    let manager = h.manager.clone();
    let task_signal = signal.clone();
    let wait = tokio::spawn(async move {
        manager
            .wait_for_agent_event(
                ID,
                paseo_core::WaitForAgentOptions {
                    signal: Some(task_signal),
                    wait_for_active: false,
                },
            )
            .await
    });
    settle().await;
    signal.abort("caller gave up");
    let error = wait.await.unwrap().unwrap_err();
    assert!(
        matches!(error, CoreError::Aborted(ref m) if m == "caller gave up"),
        "{error}"
    );

    // A running agent satisfies the start wait at once, before the signal is looked at.
    let aborted = paseo_core::AbortSignal::new();
    aborted.abort("early");
    h.manager
        .wait_for_agent_run_start(ID, Some(aborted.clone()))
        .await
        .unwrap();

    // With an idle agent and a pending run the abort check runs first.
    let idle = Harness::new().await;
    idle.agent(ID, &MockSession::new("codex")).await;
    let _pending = idle
        .manager
        .stream_agent(ID, PromptInput::from("x"), RunOpts::default())
        .await
        .unwrap();
    let error = idle
        .manager
        .wait_for_agent_run_start(ID, Some(aborted))
        .await
        .unwrap_err();
    assert!(
        matches!(error, CoreError::Aborted(ref m) if m == "early"),
        "{error}"
    );
}

#[tokio::test(start_paused = true)]
async fn out_of_order_subscribers_all_see_the_same_event_order() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    let a = collect(&h.manager, SubscribeOpts::for_agent(ID).without_replay()).await;
    let b = collect(&h.manager, SubscribeOpts::default().without_replay()).await;
    for i in 0..20 {
        session.push_event(turn_started(&format!("t{i}")));
        session.push_event(turn_completed(&format!("t{i}")));
    }
    settle().await;
    let kinds = |log: &EventLog| -> Vec<String> {
        log.all()
            .iter()
            .map(|e| match &**e {
                ManagerEvent::AgentState { agent } => format!("state:{:?}", agent.lifecycle),
                ManagerEvent::AgentStream { event, .. } => event.type_name().to_owned(),
                _ => "other".into(),
            })
            .collect()
    };
    assert_eq!(kinds(&a), kinds(&b));
    assert!(a.len() >= 80);
}

#[tokio::test(start_paused = true)]
async fn a_flooding_provider_cannot_starve_commands() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    for i in 0..2000 {
        session.push_event(timeline_event(
            tool_call(
                &format!("call-{i}"),
                paseo_agent_types::ToolCallStatus::Completed,
                "ok",
                None,
            ),
            "codex",
            None,
        ));
    }

    // The command is served after a bounded streak of events, long before the flood is drained.
    let rows_at_command = h.rows(ID).await.len();
    assert!(
        rows_at_command < 2000,
        "the command waited for the whole flood: {rows_at_command}"
    );
    settle().await;
    assert_eq!(h.rows(ID).await.len(), 2000, "and nothing is lost");
}
