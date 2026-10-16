//! Port of the steer / replace / cancel cases of `agent-manager.test.ts` (WP8 group
//! "Steer/replace/cancel"), driven through `MockSession` with a paused clock.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem};
use paseo_core::mock::{boxed, Gate, MockSession, ScriptedBehavior};
use paseo_core::{
    AgentManager, CancelOutcome, Lifecycle, ManagerEvent, RunOpts, SteerRunOpts, SubscribeOpts,
};
use paseo_provider_api::{PromptInput, ProviderError, SteerResult, TurnHandle};
use paseo_timeline::store::FetchOptions;

const ID: &str = AGENT_IDS[0];

fn steer_opts(client_message_id: &str) -> SteerRunOpts {
    SteerRunOpts {
        run: RunOpts::with_client_message_id(client_message_id),
        ..SteerRunOpts::default()
    }
}

async fn all_rows(h: &Harness) -> Vec<paseo_timeline::ProjectedEntry> {
    h.manager
        .fetch_timeline(
            ID,
            FetchOptions {
                limit: Some(0),
                ..FetchOptions::default()
            },
        )
        .await
        .unwrap()
        .rows
}

#[tokio::test(start_paused = true)]
async fn unavailable_steer_interrupts_once_and_starts_one_replacement_turn() {
    let h = Harness::new().await;
    let (session, mode) = steering_session("codex");
    *mode.lock().unwrap() = SteerMode::Unavailable;
    start_and_steer_through_manager(&h, &session, "codex", true).await;

    assert_eq!(session.interrupt_count(), 1);
    assert_eq!(session.start_count(), 2);
    assert!(has_user_message(&h.items(ID).await, "replacement-client"));
}

#[tokio::test(start_paused = true)]
async fn orders_an_accepted_steer_before_output_emitted_while_acknowledgement_is_pending() {
    let entered = Gate::new();
    let release = Gate::new();
    let (entered2, release2) = (entered.clone(), release.clone());
    let session = MockSession::with_behavior(
        "codex",
        steering_behavior_with(move |session, options| {
            let (entered, release) = (entered2.clone(), release2.clone());
            boxed(async move {
                session.with_state(|s| s.steer_count += 1);
                assert_eq!(options.expected_turn_id, "active-turn-1");
                entered.open();
                release.wait().await;
                Ok(SteerResult::Accepted)
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    let consume = h.start_background_run(ID, "initial").await;

    let manager = h.manager.clone();
    let steer = tokio::spawn(async move {
        manager
            .steer_agent_run(ID, PromptInput::from("hello"), steer_opts("hello-client"))
            .await
    });
    entered.wait().await;
    session.push_event(timeline_event(
        assistant_item("terminal output"),
        "codex",
        Some("active-turn-1"),
    ));
    session.push_event(turn_completed("active-turn-1"));
    settle().await;
    assert!(
        !h.items(ID)
            .await
            .contains(&assistant_item("terminal output")),
        "output emitted during the steer await is held on the barrier"
    );

    release.open();
    assert_eq!(steer.await.unwrap().unwrap(), SteerResult::Accepted);
    consume.await.unwrap();
    let rows: Vec<_> = all_rows(&h)
        .await
        .into_iter()
        .filter(|r| {
            matches!(&r.item, AgentTimelineItem::UserMessage { text, .. } if text == "hello")
                || r.item == assistant_item("terminal output")
        })
        .collect();
    assert_eq!(rows.len(), 2);
    assert!(
        matches!(&rows[0].item, AgentTimelineItem::UserMessage { text, .. } if text == "hello")
    );
    assert_eq!(rows[0].turn_id.as_deref(), Some("active-turn-1"));
    assert_eq!(rows[1].item, assistant_item("terminal output"));
    assert_eq!(rows[1].turn_id.as_deref(), Some("active-turn-1"));
}

/// `SteeringTestSession` with only the steer call replaced.
fn steering_behavior_with(
    steer: impl Fn(
            MockSession,
            paseo_provider_api::SteerOptions,
        ) -> paseo_core::mock::BoxFut<paseo_provider_api::Result<SteerResult>>
        + Send
        + Sync
        + 'static,
) -> ScriptedBehavior {
    ScriptedBehavior::new()
        .on_start(|session, prompt, options| {
            boxed(async move {
                let n = session.record_start(prompt, options);
                let turn_id = format!("active-turn-{n}");
                session.push_event_soon(provider_event_turn_started("codex", &turn_id));
                Ok(TurnHandle { turn_id })
            })
        })
        .on_interrupt(|session| {
            boxed(async move {
                session.with_state(|s| s.interrupt_count += 1);
                let turn = format!("active-turn-{}", session.start_count());
                session.push_event(turn_canceled(Some(&turn), "interrupted"));
                Ok(())
            })
        })
        .on_steer(move |session, _prompt, options| steer(session, options))
}

#[tokio::test(start_paused = true)]
async fn orders_buffered_pre_steer_output_before_an_immediately_accepted_steer() {
    let h = Harness::with(HarnessOptions {
        tweak: Box::new(|o| o.coalesce_window = Duration::from_secs(60)),
        ..HarnessOptions::default()
    })
    .await;
    let (session, _) = steering_session("codex");
    h.agent(ID, &session).await;
    h.start_background_run(ID, "initial").await;

    session.push_event(timeline_event(
        assistant_item("pre-steer output"),
        "codex",
        Some("active-turn-1"),
    ));
    let result = h
        .manager
        .steer_agent_run(ID, PromptInput::from("hello"), steer_opts("hello-client"))
        .await
        .unwrap();
    assert_eq!(result, SteerResult::Accepted);

    let rows: Vec<_> = all_rows(&h)
        .await
        .into_iter()
        .filter(|r| {
            r.item == assistant_item("pre-steer output")
                || matches!(&r.item, AgentTimelineItem::UserMessage { text, .. } if text == "hello")
        })
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].item, assistant_item("pre-steer output"));
    assert_eq!(rows[0].turn_id.as_deref(), Some("active-turn-1"));
    assert!(
        matches!(&rows[1].item, AgentTimelineItem::UserMessage { text, .. } if text == "hello")
    );
    assert_eq!(rows[1].turn_id.as_deref(), Some("active-turn-1"));
}

#[tokio::test(start_paused = true)]
async fn orders_a_concurrent_replacement_after_a_pending_accepted_steer() {
    let entered = Gate::new();
    let release = Gate::new();
    let (entered2, release2) = (entered.clone(), release.clone());
    let session = MockSession::with_behavior(
        "codex",
        steering_behavior_with(move |session, _options| {
            let (entered, release) = (entered2.clone(), release2.clone());
            boxed(async move {
                session.with_state(|s| s.steer_count += 1);
                entered.open();
                release.wait().await;
                Ok(SteerResult::Accepted)
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    let consume_initial = h.start_background_run(ID, "initial").await;

    let manager = h.manager.clone();
    let steer = tokio::spawn(async move {
        manager
            .steer_agent_run(ID, PromptInput::from("hello"), steer_opts("hello-client"))
            .await
    });
    entered.wait().await;
    let manager = h.manager.clone();
    let replacement = tokio::spawn(async move {
        manager
            .replace_agent_run(
                ID,
                PromptInput::from("replacement"),
                RunOpts::with_client_message_id("replacement-client"),
            )
            .await
    });
    settle().await;
    assert_eq!(
        session.interrupt_count(),
        0,
        "the replacement waits for the foreground lane"
    );

    release.open();
    assert_eq!(steer.await.unwrap().unwrap(), SteerResult::Accepted);
    let mut replacement_run = replacement.await.unwrap().unwrap();
    tokio::spawn(async move { while let Ok(Some(_)) = replacement_run.next().await {} });
    consume_initial.await.unwrap();
    settle().await;

    assert_eq!(session.start_count(), 2);
    assert_eq!(session.interrupt_count(), 1);
    let users: Vec<(String, Option<String>)> = all_rows(&h)
        .await
        .into_iter()
        .filter_map(|r| match r.item {
            AgentTimelineItem::UserMessage {
                text,
                client_message_id,
                ..
            } => Some((text, client_message_id)),
            _ => None,
        })
        .collect();
    assert_eq!(
        users,
        vec![
            ("hello".to_owned(), Some("hello-client".to_owned())),
            (
                "replacement".to_owned(),
                Some("replacement-client".to_owned())
            ),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn does_not_replace_a_newer_foreground_turn_after_unavailable_steer_fallback_is_admitted() {
    let (mut hooks, _log) = RecordingHooks::new();
    hooks.hold_fallback = true;
    hooks.fallback_release = Gate::new();
    let (entered, release) = (
        hooks.fallback_entered.clone(),
        hooks.fallback_release.clone(),
    );
    let h = Harness::with(HarnessOptions {
        hooks: Some(hooks),
        ..HarnessOptions::default()
    })
    .await;
    let (session, mode) = steering_session("codex");
    *mode.lock().unwrap() = SteerMode::Unavailable;
    h.agent(ID, &session).await;
    let consume_a = h.start_background_run(ID, "A").await;

    let manager = h.manager.clone();
    let send = tokio::spawn(async move {
        start_agent_run(
            &manager,
            ID,
            "hello",
            StartRunOptions {
                replace_running: true,
                steer: true,
                run: RunOpts::with_client_message_id("hello-client"),
                ..StartRunOptions::default()
            },
        )
        .await
    });
    entered.wait().await;
    session.push_event(turn_completed("active-turn-1"));
    consume_a.await.unwrap();
    let consume_b = h.start_background_run(ID, "B").await;
    release.open();

    let error = send.await.unwrap().unwrap_err();
    assert_eq!(
        error.to_string(),
        "Active turn changed before steering could be delivered"
    );
    assert_eq!(
        h.view(ID).active_foreground_turn_id.as_deref(),
        Some("active-turn-2")
    );
    assert_eq!(session.interrupt_count(), 0);
    assert!(!session
        .start_prompts()
        .contains(&PromptInput::from("hello")));
    assert!(!has_user_message(&h.items(ID).await, "hello-client"));
    h.manager.close_agent(ID).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_millis(100), consume_b).await;
}

#[tokio::test(start_paused = true)]
async fn steers_a_tracked_autonomous_turn_without_creating_a_replacement_run() {
    let h = Harness::new().await;
    let (session, _) = steering_session("claude");
    h.agent_with(ID, &session, "claude", Default::default(), |_| {})
        .await;
    session.push_event(provider_event_turn_started("claude", "active-turn-0"));
    settle().await;
    assert_eq!(h.view(ID).active_turn_id.as_deref(), Some("active-turn-0"));

    let result = start_agent_run(
        &h.manager,
        ID,
        "autonomous follow-up",
        StartRunOptions {
            steer: true,
            run: RunOpts::with_client_message_id("autonomous-follow-up-client"),
            ..StartRunOptions::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(result, Disposition::Steered);
    assert_eq!(session.steer_count(), 1);
    assert_eq!(session.interrupt_count(), 0);
    let view = h.view(ID);
    assert_eq!(view.active_turn_id.as_deref(), Some("active-turn-0"));
    assert_eq!(view.active_foreground_turn_id, None);
    assert!(h.items(ID).await.iter().any(|item| matches!(
        item,
        AgentTimelineItem::UserMessage { text, client_message_id: Some(c), .. }
            if text == "autonomous follow-up" && c == "autonomous-follow-up-client"
    )));
}

#[tokio::test(start_paused = true)]
async fn isolated_rewind_falls_back_from_steering_to_the_normal_replacement_path() {
    let h = Harness::new().await;
    let (session, mode) = steering_session("claude");
    *mode.lock().unwrap() = SteerMode::Unavailable;
    let id = start_and_steer_through_manager(&h, &session, "claude", true).await;

    start_agent_run(
        &h.manager,
        id,
        "/rewind submitted-message-id",
        StartRunOptions {
            replace_running: true,
            steer: true,
            run: RunOpts::with_client_message_id("rewind-client"),
            ..StartRunOptions::default()
        },
    )
    .await
    .unwrap();
    h.manager.wait_for_agent_run_start(id, None).await.unwrap();

    assert_eq!(session.interrupt_count(), 2);
    assert!(session
        .start_prompts()
        .contains(&PromptInput::from("/rewind submitted-message-id")));
    assert!(has_user_message(&h.items(id).await, "rewind-client"));
}

#[tokio::test(start_paused = true)]
async fn missing_steer_operation_interrupts_once_and_starts_one_replacement_turn() {
    let h = Harness::new().await;
    let session = unsupported_steering_session();
    let id = start_and_steer_through_manager(&h, &session, "codex", true).await;

    assert_eq!(session.interrupt_count(), 1);
    assert_eq!(session.start_count(), 2);
    assert!(has_user_message(&h.items(id).await, "replacement-client"));
}

#[tokio::test(start_paused = true)]
async fn ambiguous_steer_failure_leaves_the_active_turn_untouched() {
    let h = Harness::new().await;
    let (session, mode) = steering_session("codex");
    *mode.lock().unwrap() = SteerMode::Fail("connection lost after delivery".into());
    h.agent(ID, &session).await;
    h.start_background_run(ID, "initial").await;

    let error = start_agent_run(
        &h.manager,
        ID,
        "must not resend",
        StartRunOptions {
            replace_running: true,
            steer: true,
            run: RunOpts::with_client_message_id("ambiguous-client"),
            ..StartRunOptions::default()
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("connection lost after delivery"));
    assert_eq!(session.interrupt_count(), 0);
    assert_eq!(session.start_count(), 1);
    assert!(!has_user_message(&h.items(ID).await, "ambiguous-client"));
}

#[tokio::test(start_paused = true)]
async fn steering_records_concurrent_early_echoes_as_canonical_submitted_prompts() {
    let h = Harness::new().await;
    let (session, _) = steering_session("codex");
    h.agent(ID, &session).await;
    h.start_background_run(ID, "start").await;

    let (one, two) = tokio::join!(
        h.manager
            .steer_agent_run(ID, PromptInput::from("one"), steer_opts("client-one")),
        h.manager
            .steer_agent_run(ID, PromptInput::from("two"), steer_opts("client-two")),
    );
    assert_eq!(one.unwrap(), SteerResult::Accepted);
    assert_eq!(two.unwrap(), SteerResult::Accepted);
    settle().await;

    let users: Vec<(Option<String>, Option<String>)> = h
        .items(ID)
        .await
        .into_iter()
        .filter_map(|item| match item {
            AgentTimelineItem::UserMessage {
                message_id,
                client_message_id,
                ..
            } => Some((message_id, client_message_id)),
            _ => None,
        })
        .collect();
    for id in ["client-one", "client-two"] {
        let matching: Vec<_> = users
            .iter()
            .filter(|(_, client)| client.as_deref() == Some(id))
            .collect();
        assert_eq!(matching.len(), 1, "{id} is recorded once");
        assert_eq!(
            matching[0].0.as_deref(),
            Some(id),
            "messageId is the clientMessageId"
        );
    }
}

// ---- cancel ----

/// `ControlledInterruptSession`: the turn id is fixed, `interrupt` runs a custom behavior.
fn controlled_interrupt_session(
    turn_id: &'static str,
    interrupt: impl Fn(MockSession) -> paseo_core::mock::BoxFut<paseo_provider_api::Result<()>>
        + Send
        + Sync
        + 'static,
) -> MockSession {
    MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new()
            .on_start(move |session, prompt, options| {
                boxed(async move {
                    session.record_start(prompt, options);
                    session.push_event_soon(provider_event_turn_started("codex", turn_id));
                    Ok(TurnHandle {
                        turn_id: turn_id.into(),
                    })
                })
            })
            .on_interrupt(move |session| {
                session.with_state(|s| s.interrupt_count += 1);
                interrupt(session)
            }),
    )
}

async fn controlled_harness(session: &MockSession) -> Harness {
    let h = Harness::with(HarnessOptions {
        tweak: Box::new(|o| o.interrupt_session_timeout = Duration::from_millis(10)),
        ..HarnessOptions::default()
    })
    .await;
    h.agent(ID, session).await;
    h
}

async fn wait_for_lifecycle(manager: &AgentManager, id: &str, lifecycle: Lifecycle) {
    let mut sub = manager
        .subscribe(SubscribeOpts::for_agent(id).without_replay())
        .await
        .unwrap();
    while let Some(event) = sub.next().await {
        if matches!(&*event, ManagerEvent::AgentState { agent } if agent.lifecycle == lifecycle) {
            return;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn cancel_agent_run_preserves_running_state_when_the_provider_interrupt_hangs() {
    let session = controlled_interrupt_session("hanging-interrupt-turn", |_| {
        boxed(async { std::future::pending().await })
    });
    let h = controlled_harness(&session).await;
    session.push_event(provider_event_turn_started(
        "codex",
        "hanging-interrupt-turn",
    ));
    wait_for_lifecycle(&h.manager, ID, Lifecycle::Running).await;

    let outcome = h.manager.cancel_agent_run(ID).await.unwrap();
    assert_eq!(outcome, CancelOutcome::Refused);
    assert_eq!(session.interrupt_count(), 1);
    assert_eq!(h.view(ID).lifecycle, Lifecycle::Running);
}

#[tokio::test(start_paused = true)]
async fn cancel_agent_run_preserves_the_active_turn_when_the_provider_rejects_the_interrupt() {
    let session = controlled_interrupt_session("provider-still-active-turn", |_| {
        boxed(async {
            Err(ProviderError::provider(
                "A foreground turn is already active",
            ))
        })
    });
    let h = controlled_harness(&session).await;
    h.start_background_run(ID, "exercise cancellation").await;

    assert_eq!(
        h.manager.cancel_agent_run(ID).await.unwrap(),
        CancelOutcome::Refused
    );
    let view = h.view(ID);
    assert_eq!(view.lifecycle, Lifecycle::Running);
    assert_eq!(
        view.active_foreground_turn_id.as_deref(),
        Some("provider-still-active-turn")
    );
    session.push_event(turn_completed("provider-still-active-turn"));
    settle().await;
}

#[tokio::test(start_paused = true)]
async fn cancel_agent_run_succeeds_when_the_foreground_turn_finishes_before_the_provider_rejects_the_interrupt(
) {
    let manager_slot: Arc<Mutex<Option<Arc<AgentManager>>>> = Arc::default();
    let slot = manager_slot.clone();
    let session = controlled_interrupt_session("naturally-completed-turn", move |session| {
        let manager = slot.lock().unwrap().clone().expect("manager");
        boxed(async move {
            let mut sub = manager
                .subscribe(SubscribeOpts::for_agent(ID).without_replay())
                .await
                .unwrap();
            session.push_event(turn_completed("naturally-completed-turn"));
            while let Some(event) = sub.next().await {
                if matches!(&*event, ManagerEvent::AgentState { agent } if agent.lifecycle == Lifecycle::Idle)
                {
                    break;
                }
            }
            Err(ProviderError::provider("turn already completed"))
        })
    });
    let h = controlled_harness(&session).await;
    *manager_slot.lock().unwrap() = Some(h.manager.clone());
    h.start_background_run(ID, "exercise cancellation").await;

    assert_eq!(
        h.manager.cancel_agent_run(ID).await.unwrap(),
        CancelOutcome::Settled
    );
    let view = h.view(ID);
    assert_eq!(view.lifecycle, Lifecycle::Idle);
    assert_eq!(view.active_foreground_turn_id, None);
}

#[tokio::test(start_paused = true)]
async fn cancel_agent_run_succeeds_when_the_provider_queues_completion_before_rejecting_the_interrupt(
) {
    let session = controlled_interrupt_session("queued-completion-turn", |session| {
        boxed(async move {
            session.push_event(turn_completed("queued-completion-turn"));
            Err(ProviderError::provider("turn already completed"))
        })
    });
    let h = controlled_harness(&session).await;
    h.start_background_run(ID, "exercise cancellation").await;

    assert_eq!(
        h.manager.cancel_agent_run(ID).await.unwrap(),
        CancelOutcome::Settled
    );
    let view = h.view(ID);
    assert_eq!(view.lifecycle, Lifecycle::Idle);
    assert_eq!(view.active_foreground_turn_id, None);
}

#[tokio::test(start_paused = true)]
async fn cancel_agent_run_waits_for_an_acknowledged_autonomous_interrupt_to_settle() {
    let interrupted = Gate::new();
    let signal = interrupted.clone();
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_interrupt(move |session| {
            session.with_state(|s| s.interrupt_count += 1);
            signal.open();
            boxed(async { Ok(()) })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    session.push_event(provider_event_turn_started("codex", "autonomous-cancel-1"));
    wait_for_lifecycle(&h.manager, ID, Lifecycle::Running).await;
    let before = h.view(ID);
    assert_eq!(before.lifecycle, Lifecycle::Running);
    assert_eq!(before.active_foreground_turn_id, None);

    let manager = h.manager.clone();
    let cancel = tokio::spawn(async move { manager.cancel_agent_run(ID).await });
    interrupted.wait().await;
    settle().await;
    assert!(!cancel.is_finished(), "cancel waits for the run to settle");
    assert_eq!(session.interrupt_count(), 1);

    session.push_event(turn_canceled(Some("autonomous-cancel-1"), "interrupted"));
    assert_eq!(cancel.await.unwrap().unwrap(), CancelOutcome::Settled);
    assert_eq!(h.view(ID).lifecycle, Lifecycle::Idle);
}

#[tokio::test(start_paused = true)]
async fn failed_replacement_cancellation_preserves_an_autonomous_running_state() {
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_interrupt(|_| {
            boxed(async {
                Err(ProviderError::provider(
                    "provider still owns the autonomous turn",
                ))
            })
        }),
    );
    let h = controlled_harness(&session).await;
    session.push_event(provider_event_turn_started("codex", "autonomous-replace-1"));
    wait_for_lifecycle(&h.manager, ID, Lifecycle::Running).await;

    let error = h
        .manager
        .replace_agent_run(
            ID,
            PromptInput::from("replacement prompt"),
            RunOpts::default(),
        )
        .await
        .err()
        .expect("replace is refused");
    assert_eq!(
        error.to_string(),
        format!(
            "Cannot replace agent {ID} because its active run cancellation was not acknowledged"
        )
    );
    let view = h.view(ID);
    assert_eq!(view.lifecycle, Lifecycle::Running);
    assert_eq!(view.active_foreground_turn_id, None);
}

// ---- replace ----

#[tokio::test(start_paused = true)]
async fn replace_agent_run_does_not_emit_idle_or_resolve_waiters_between_interrupted_and_replacement_runs(
) {
    let allow_first_end = Gate::new();
    let allow_second_end = Gate::new();
    let (first_gate, second_gate, interrupt_gate) = (
        allow_first_end.clone(),
        allow_second_end.clone(),
        allow_first_end.clone(),
    );
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new()
            .on_start(move |session, prompt, options| {
                let (first, second) = (first_gate.clone(), second_gate.clone());
                boxed(async move {
                    let n = session.record_start(prompt, options);
                    let turn_id = format!("turn-{n}");
                    let task_session = session.clone();
                    let task_turn = turn_id.clone();
                    tokio::spawn(async move {
                        task_session.push_event(turn_started(&task_turn));
                        if n == 1 {
                            first.wait().await;
                            task_session.push_event(turn_canceled(Some(&task_turn), "interrupted"));
                        } else {
                            second.wait().await;
                            task_session.push_event(turn_completed(&task_turn));
                        }
                    });
                    Ok(TurnHandle { turn_id })
                })
            })
            .on_interrupt(move |_| {
                interrupt_gate.open();
                boxed(async { Ok(()) })
            }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    // The TS test subscribes after `createAgent`: skip the registration frames.
    let baseline = h.events.states(ID).len();
    let first_drain = h.start_background_run(ID, "first run").await;

    let manager = h.manager.clone();
    let wait =
        tokio::spawn(async move { manager.wait_for_agent_event(ID, Default::default()).await });
    settle().await;
    let mut second = h
        .manager
        .replace_agent_run(ID, PromptInput::from("second run"), RunOpts::default())
        .await
        .unwrap();
    let second_drain = tokio::spawn(async move { while let Ok(Some(_)) = second.next().await {} });
    h.manager.wait_for_agent_run_start(ID, None).await.unwrap();

    advance(50).await;
    assert!(
        !wait.is_finished(),
        "the waiter must not resolve between runs"
    );

    let lifecycles: Vec<Lifecycle> = h
        .events
        .states(ID)
        .iter()
        .skip(baseline)
        .map(|v| v.lifecycle)
        .collect();
    let running: Vec<usize> = lifecycles
        .iter()
        .enumerate()
        .filter(|(_, l)| **l == Lifecycle::Running)
        .map(|(i, _)| i)
        .collect();
    assert!(running.len() >= 2);
    assert!(
        !lifecycles[..running[1]].contains(&Lifecycle::Idle),
        "no idle frame before the replacement runs: {lifecycles:?}"
    );

    allow_second_end.open();
    let waited = wait.await.unwrap().unwrap();
    assert_eq!(waited.status, Lifecycle::Idle);
    first_drain.await.unwrap();
    second_drain.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn replace_agent_run_stays_running_when_a_stale_old_terminal_arrives_before_the_replacement_turn_is_current(
) {
    let second_start_entered = Gate::new();
    let interrupt_started = Gate::new();
    let allow_interrupt = Gate::new();
    let allow_second_start = Gate::new();
    let allow_second_turn = Gate::new();
    let gates = (
        second_start_entered.clone(),
        interrupt_started.clone(),
        allow_interrupt.clone(),
        allow_second_start.clone(),
        allow_second_turn.clone(),
    );
    let (g1, g2, g3, g4, g5) = gates;
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new()
            .on_start(move |session, prompt, options| {
                let (entered, allow_start, allow_turn) = (g1.clone(), g4.clone(), g5.clone());
                boxed(async move {
                    let n = session.record_start(prompt, options);
                    let turn_id = format!("turn-{n}");
                    if n == 1 {
                        session.push_event_soon(turn_started(&turn_id));
                        return Ok(TurnHandle { turn_id });
                    }
                    entered.open();
                    allow_start.wait().await;
                    let task_session = session.clone();
                    let task_turn = turn_id.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::ZERO).await;
                        task_session.push_event(turn_started(&task_turn));
                        allow_turn.wait().await;
                        task_session.push_event(turn_completed(&task_turn));
                    });
                    Ok(TurnHandle { turn_id })
                })
            })
            .on_interrupt(move |session| {
                let (started, allow) = (g2.clone(), g3.clone());
                boxed(async move {
                    started.open();
                    allow.wait().await;
                    session.push_event(turn_canceled(Some("turn-1"), "Interrupted"));
                    Ok(())
                })
            }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    let mut first = h
        .manager
        .stream_agent(
            ID,
            PromptInput::from("first run"),
            RunOpts::with_client_message_id("first-client-message"),
        )
        .await
        .unwrap();
    let first_drain = tokio::spawn(async move { while let Ok(Some(_)) = first.next().await {} });
    h.manager.wait_for_agent_run_start(ID, None).await.unwrap();

    let updates_before = h.events.states(ID).len();
    let before_updated_at = h.view(ID).updated_at_ms;
    let manager = h.manager.clone();
    let second_run = tokio::spawn(async move {
        manager
            .replace_agent_run(
                ID,
                PromptInput::from("replacement run"),
                RunOpts::with_client_message_id("replacement-client-message"),
            )
            .await
    });
    interrupt_started.wait().await;
    settle().await;
    let replacement_updates: Vec<_> = h
        .events
        .states(ID)
        .into_iter()
        .skip(updates_before)
        .collect();
    assert!(replacement_updates
        .iter()
        .any(|v| v.lifecycle == Lifecycle::Running && v.updated_at_ms > before_updated_at));
    assert!(replacement_updates
        .iter()
        .all(|v| v.lifecycle != Lifecycle::Idle));
    allow_interrupt.open();

    let mut second = second_run.await.unwrap().unwrap();
    let second_drain = tokio::spawn(async move { while let Ok(Some(_)) = second.next().await {} });
    second_start_entered.wait().await;

    let gap = h.view(ID);
    assert!(gap.pending_replacement);
    assert_eq!(gap.active_foreground_turn_id, None);
    assert_eq!(gap.lifecycle, Lifecycle::Running);

    let manager = h.manager.clone();
    let replacement_start =
        tokio::spawn(async move { manager.wait_for_agent_run_start(ID, None).await });
    advance(50).await;
    assert!(
        !replacement_start.is_finished(),
        "the gap is not a started run"
    );

    session.push_event(turn_completed("turn-1"));
    settle().await;
    assert_eq!(h.view(ID).lifecycle, Lifecycle::Running);
    assert_eq!(
        h.events.states(ID).last().unwrap().lifecycle,
        Lifecycle::Running
    );
    assert!(h
        .events
        .states(ID)
        .iter()
        .skip(updates_before)
        .all(|v| v.lifecycle != Lifecycle::Idle));

    allow_second_start.open();
    replacement_start.await.unwrap().unwrap();
    session.push_event(turn_completed("turn-1"));
    settle().await;
    let tail = h
        .manager
        .fetch_timeline(
            ID,
            FetchOptions {
                limit: Some(0),
                ..FetchOptions::default()
            },
        )
        .await
        .unwrap();
    let replacement_row = tail
        .rows
        .iter()
        .find(|r| matches!(&r.item, AgentTimelineItem::UserMessage { client_message_id: Some(c), .. } if c == "replacement-client-message"))
        .expect("replacement prompt is recorded");
    assert!(matches!(
        &replacement_row.item,
        AgentTimelineItem::UserMessage { message_id: Some(id), .. } if id == "replacement-client-message"
    ));
    allow_second_turn.open();
    first_drain.await.unwrap();
    second_drain.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn replace_agent_run_succeeds_when_foreground_turn_terminal_event_is_never_delivered() {
    let allow_second_end = Gate::new();
    let gate = allow_second_end.clone();
    // The first turn never emits a terminal event and `interrupt` produces none either.
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_start(move |session, prompt, options| {
            let gate = gate.clone();
            boxed(async move {
                let n = session.record_start(prompt, options);
                let turn_id = format!("turn-{n}");
                let task_session = session.clone();
                let task_turn = turn_id.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::ZERO).await;
                    task_session.push_event(turn_started(&task_turn));
                    if n != 1 {
                        gate.wait().await;
                        task_session.push_event(turn_completed(&task_turn));
                    }
                });
                Ok(TurnHandle { turn_id })
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    let first_drain = h.start_background_run(ID, "hanging prompt").await;
    let before = h.view(ID);
    assert_eq!(before.lifecycle, Lifecycle::Running);
    assert_eq!(before.active_foreground_turn_id.as_deref(), Some("turn-1"));

    // The cancel times out after 2 s (no terminal event), then force-cancels the stale turn.
    let mut second = h
        .manager
        .replace_agent_run(
            ID,
            PromptInput::from("replacement prompt"),
            RunOpts::default(),
        )
        .await
        .unwrap();
    let collected = Arc::new(Mutex::new(Vec::new()));
    let sink = collected.clone();
    let second_drain = tokio::spawn(async move {
        while let Ok(Some(event)) = second.next().await {
            sink.lock().unwrap().push(event);
        }
    });
    h.manager.wait_for_agent_run_start(ID, None).await.unwrap();
    allow_second_end.open();
    second_drain.await.unwrap();
    first_drain.await.unwrap();

    let events = collected.lock().unwrap().clone();
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentStreamEvent::TurnCompleted { .. })));
    assert_eq!(session.interrupt_count(), 1);
    assert_eq!(h.view(ID).lifecycle, Lifecycle::Idle);
}

// ---- pending run ----

#[tokio::test(start_paused = true)]
async fn stream_agent_clears_pending_run_when_start_turn_fails_before_a_turn_id_exists() {
    let attempts = Arc::new(Mutex::new(0u32));
    let counter = attempts.clone();
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_start(move |session, prompt, options| {
            let attempt = {
                let mut n = counter.lock().unwrap();
                *n += 1;
                *n
            };
            boxed(async move {
                if attempt == 1 {
                    return Err(ProviderError::provider(
                        "Invalid request: missing field `text`",
                    ));
                }
                let n = session.record_start(prompt, options);
                let turn_id = format!("turn-{n}");
                let soon = session.clone();
                let (started, completed) = (turn_started(&turn_id), turn_completed(&turn_id));
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::ZERO).await;
                    soon.push_event(started);
                    soon.push_event(completed);
                });
                Ok(TurnHandle { turn_id })
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;

    let error = h
        .manager
        .run_agent(
            ID,
            PromptInput::from("fail before turn id"),
            RunOpts::with_client_message_id("rejected-prompt"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Invalid request: missing field `text`");
    assert_eq!(
        h.items(ID).await,
        vec![
            AgentTimelineItem::UserMessage {
                text: "fail before turn id".into(),
                message_id: Some("rejected-prompt".into()),
                client_message_id: Some("rejected-prompt".into()),
            },
            assistant_item("[System Error] Invalid request: missing field `text`"),
        ]
    );

    let second = h
        .manager
        .run_agent(ID, PromptInput::from("second turn"), RunOpts::default())
        .await
        .unwrap();
    assert!(!second.session_id.is_empty());
    assert!(!second.canceled);
}

#[tokio::test(start_paused = true)]
async fn acknowledged_cancellation_settles_a_pending_run_before_it_has_a_turn_id() {
    let entered = Gate::new();
    let release = Gate::new();
    let (entered2, release2) = (entered.clone(), release.clone());
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_start(move |_, _, _| {
            let (entered, release) = (entered2.clone(), release2.clone());
            boxed(async move {
                entered.open();
                release.wait().await;
                Err(ProviderError::provider("released canceled start"))
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    let manager = h.manager.clone();
    let first_run = tokio::spawn(async move {
        manager
            .run_agent(ID, PromptInput::from("start a turn"), RunOpts::default())
            .await
    });
    entered.wait().await;

    assert_eq!(h.view(ID).lifecycle, Lifecycle::Idle);
    assert!(h.manager.has_in_flight_run(ID));

    assert_eq!(
        h.manager.cancel_agent_run(ID).await.unwrap(),
        CancelOutcome::Settled
    );
    settle().await;
    assert!(!h.manager.has_in_flight_run(ID));
    assert_eq!(h.view(ID).lifecycle, Lifecycle::Idle);

    release.open();
    let error = first_run.await.unwrap().unwrap_err();
    assert_eq!(error.to_string(), "released canceled start");
    assert_eq!(h.view(ID).lifecycle, Lifecycle::Idle);
}
