//! Port of the run / turn / attention / permission cases of `agent-manager.test.ts`.
//! Persisted-record assertions run against `RecordingPersister` snapshots (the store seam
//! arrives with the persistence step).

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use paseo_agent_types::{
    AgentStreamEvent, AgentTimelineItem, AgentUsage, AttentionReason, PermissionKind,
    PermissionRequest, PermissionResponse,
};
use paseo_core::mock::{boxed, Gate, MockSession, ScriptedBehavior};
use paseo_core::time::ManualClock;
use paseo_core::{
    AttentionState, Lifecycle, ManagerEvent, RegisterOptions, RunOpts, SubscribeOpts,
    WaitForAgentOptions,
};
use paseo_provider_api::{PromptInput, TurnHandle};
use serde_json::{json, Value};

const ID: &str = AGENT_IDS[0];

fn usage(input: u64) -> AgentUsage {
    AgentUsage {
        input_tokens: Some(input.into()),
        ..AgentUsage::default()
    }
}

fn permission(id: &str, name: &str, kind: PermissionKind) -> PermissionRequest {
    PermissionRequest {
        id: id.into(),
        provider: "codex".into(),
        name: name.into(),
        kind,
        title: None,
        description: None,
        input: None,
        detail: None,
        suggestions: None,
        actions: None,
        metadata: None,
    }
}

fn last_persisted(h: &Harness) -> paseo_core::ManagedAgentView {
    h.persister
        .snapshots
        .lock()
        .unwrap()
        .last()
        .expect("a snapshot was persisted")
        .0
        .clone()
}

fn persisted_harness() -> HarnessOptions {
    HarnessOptions {
        with_persister: true,
        ..HarnessOptions::default()
    }
}

/// A session whose turn `turn_id` starts by itself and ends with `terminal` once `release` opens.
fn turn_then(
    terminal: impl Fn(&str) -> AgentStreamEvent + Send + Sync + 'static,
    turn_id: &'static str,
    release: Option<Gate>,
) -> MockSession {
    let terminal = Arc::new(terminal);
    MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_start(move |session, prompt, options| {
            let (terminal, release) = (terminal.clone(), release.clone());
            boxed(async move {
                session.record_start(prompt, options);
                let task = session.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::ZERO).await;
                    task.push_event(turn_started(turn_id));
                    if let Some(gate) = release {
                        gate.wait().await;
                    }
                    task.push_event(terminal(turn_id));
                });
                Ok(TurnHandle {
                    turn_id: turn_id.into(),
                })
            })
        }),
    )
}

#[tokio::test(start_paused = true)]
async fn run_agent_persists_finished_attention_and_idle_status_without_an_external_snapshot_subscriber(
) {
    let h = Harness::with(persisted_harness()).await;
    h.agent(ID, &MockSession::auto_completing("codex")).await;

    h.manager
        .run_agent(ID, PromptInput::from("say hello"), RunOpts::default())
        .await
        .unwrap();
    h.manager.flush().await;

    let persisted = last_persisted(&h);
    assert_eq!(persisted.lifecycle, Lifecycle::Idle);
    assert!(matches!(
        persisted.attention,
        AttentionState::Required { reason: AttentionReason::Finished, timestamp_ms } if timestamp_ms > 0
    ));
}

#[tokio::test(start_paused = true)]
async fn wait_for_agent_event_does_not_resolve_idle_until_foreground_turn_is_finalized() {
    let release = Gate::new();
    let session = turn_then(turn_completed, "turn-1", Some(release.clone()));
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    let consume = h.start_background_run(ID, "hello").await;
    advance(20).await;

    let manager = h.manager.clone();
    let wait =
        tokio::spawn(async move { manager.wait_for_agent_event(ID, Default::default()).await });
    advance(50).await;
    assert!(!wait.is_finished(), "turn_completed has not arrived yet");

    release.open();
    let waited = wait.await.unwrap().unwrap();
    assert_eq!(waited.status, Lifecycle::Idle);
    consume.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn wait_for_agent_run_start_resolves_while_a_foreground_run_is_still_only_pending() {
    let h = Harness::new().await;
    h.agent(ID, &MockSession::auto_completing("codex")).await;
    let mut run = h
        .manager
        .stream_agent(ID, PromptInput::from("fast"), RunOpts::default())
        .await
        .unwrap();
    let drain = tokio::spawn(async move { while let Ok(Some(_)) = run.next().await {} });

    h.manager.wait_for_agent_run_start(ID, None).await.unwrap();
    drain.await.unwrap();
    assert_eq!(h.view(ID).lifecycle, Lifecycle::Idle);
}

#[tokio::test(start_paused = true)]
async fn wait_for_agent_run_start_ignores_a_prior_turn_error_while_the_next_run_starts() {
    let entered = Gate::new();
    let release = Gate::new();
    let (entered2, release2) = (entered.clone(), release.clone());
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_start(move |session, prompt, options| {
            let (entered, release) = (entered2.clone(), release2.clone());
            boxed(async move {
                let n = session.record_start(prompt, options);
                let turn_id = format!("turn-{n}");
                if n == 1 {
                    // Pushed synchronously, before `start_turn` returns: the events are staged.
                    session.push_event(turn_started(&turn_id));
                    session.push_event(turn_failed(Some(&turn_id), "model at capacity"));
                    return Ok(TurnHandle { turn_id });
                }
                entered.open();
                release.wait().await;
                session.push_event(turn_started(&turn_id));
                session.push_event(turn_completed(&turn_id));
                Ok(TurnHandle { turn_id })
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;

    let _ = h
        .manager
        .run_agent(ID, PromptInput::from("fail"), RunOpts::default())
        .await;
    assert_eq!(h.view(ID).lifecycle, Lifecycle::Error);

    let disposition = start_agent_run(&h.manager, ID, "resume", StartRunOptions::default())
        .await
        .unwrap();
    assert_eq!(disposition, Disposition::TurnStarted);
    let manager = h.manager.clone();
    let wait = tokio::spawn(async move { manager.wait_for_agent_run_start(ID, None).await });
    entered.wait().await;
    settle().await;
    assert!(
        !wait.is_finished(),
        "the prior error must not fail the wait"
    );

    release.open();
    wait.await.unwrap().unwrap();
    h.manager
        .wait_for_agent_event(ID, Default::default())
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn applies_live_autonomous_events_and_preserves_usage_omitted_from_completion() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    let baseline = h.events.states(ID).len();

    let turn = "autonomous-turn-1";
    session.push_event(turn_started(turn));
    settle().await;
    let running = h.view(ID);
    assert_eq!(running.lifecycle, Lifecycle::Running);
    assert_eq!(running.active_turn_id.as_deref(), Some(turn));
    assert!(running.active_turn_started_at_ms.is_some());

    let expected_usage = AgentUsage {
        input_tokens: Some(10.into()),
        context_window_max_tokens: Some(200_000.into()),
        context_window_used_tokens: Some(175.into()),
        ..AgentUsage::default()
    };
    session.push_event(AgentStreamEvent::UsageUpdated {
        provider: "codex".into(),
        usage: expected_usage.clone(),
        turn_id: Some(turn.into()),
    });
    session.push_event(timeline_event(
        assistant_item("AUTONOMOUS_PUMP_MESSAGE"),
        "codex",
        Some(turn),
    ));
    session.push_event(turn_completed(turn));
    settle().await;

    let updated = h.view(ID);
    assert_eq!(updated.lifecycle, Lifecycle::Idle);
    assert_eq!(updated.active_turn_id, None);
    assert_eq!(updated.last_usage, Some(expected_usage));
    assert!(h
        .items(ID)
        .await
        .contains(&assistant_item("AUTONOMOUS_PUMP_MESSAGE")));
    let lifecycles: Vec<Lifecycle> = h
        .events
        .states(ID)
        .iter()
        .skip(baseline)
        .map(|v| v.lifecycle)
        .collect();
    assert!(lifecycles.contains(&Lifecycle::Running));
    assert!(lifecycles.contains(&Lifecycle::Idle));
}

#[tokio::test(start_paused = true)]
async fn ignores_stale_autonomous_terminals_without_lowering_the_active_turn_lifecycle() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;

    session.push_event(AgentStreamEvent::TurnCompleted {
        provider: "codex".into(),
        usage: Some(usage(7)),
        turn_id: Some("prior-untracked-turn".into()),
    });
    session.push_event(turn_failed(Some("prior-untracked-turn"), "turn-b marker"));
    settle().await;
    let prior = h.view(ID);
    assert_eq!(prior.last_error.as_deref(), Some("turn-b marker"));
    assert_eq!(prior.last_usage, Some(usage(7)));

    session.push_event(turn_started("turn-b"));
    settle().await;
    let running = h.view(ID);
    assert_eq!(running.lifecycle, Lifecycle::Running);
    assert_eq!(running.active_turn_id.as_deref(), Some("turn-b"));

    h.manager
        .testing_mutate_agent(ID, |view| {
            view.pending_permissions.insert(
                "turn-b-permission".into(),
                permission(
                    "turn-b-permission",
                    "Current turn permission",
                    PermissionKind::Tool,
                ),
            );
        })
        .await
        .unwrap();
    let timeline_before = h.items(ID).await;

    let stale = [
        AgentStreamEvent::TurnCompleted {
            provider: "codex".into(),
            usage: Some(usage(99)),
            turn_id: Some("turn-a".into()),
        },
        turn_failed(Some("turn-a"), "late failure"),
        turn_canceled(Some("turn-a"), "late cancel"),
    ];
    for terminal in stale {
        let kind = terminal.type_name();
        let before = h.events.streams(ID).len();
        session.push_event(terminal);
        settle().await;
        // A stale terminal is still dispatched (PARITY-QUIRK(stale-terminal-dispatch)).
        assert!(h
            .events
            .streams(ID)
            .iter()
            .skip(before)
            .any(|e| stream_event(e).type_name() == kind));
        let still = h.view(ID);
        assert_eq!(still.lifecycle, Lifecycle::Running, "{kind}");
        assert_eq!(still.active_turn_id.as_deref(), Some("turn-b"), "{kind}");
        assert_eq!(still.last_error.as_deref(), Some("turn-b marker"), "{kind}");
        assert_eq!(still.last_usage, Some(usage(7)), "{kind}");
        assert!(
            still.pending_permissions.contains_key("turn-b-permission"),
            "{kind}"
        );
        assert_eq!(h.items(ID).await, timeline_before, "{kind}");
    }

    session.push_event(turn_completed("turn-b"));
    settle().await;
    let settled = h.view(ID);
    assert_eq!(settled.lifecycle, Lifecycle::Idle);
    assert_eq!(settled.active_turn_id, None);
}

#[tokio::test(start_paused = true)]
async fn preserves_terminal_fallback_when_no_active_turn_identity_was_observed() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;

    session.push_event(turn_failed(Some("untracked-turn"), "untracked failure"));
    settle().await;

    let failed = h.view(ID);
    assert_eq!(failed.lifecycle, Lifecycle::Error);
    assert_eq!(failed.last_error.as_deref(), Some("untracked failure"));
    assert!(h.items(ID).await.iter().any(|item| matches!(
        item,
        AgentTimelineItem::AssistantMessage { text, .. } if text.contains("untracked failure")
    )));
}

#[tokio::test(start_paused = true)]
async fn wait_for_agent_event_wait_for_active_resolves_for_autonomous_live_event_run() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;

    let manager = h.manager.clone();
    // Not busy yet: the wait resolves immediately, like TS where `waitForActive` needs a busy
    // agent or a foreground turn to wait on.
    let early = manager
        .wait_for_agent_event(
            ID,
            WaitForAgentOptions {
                wait_for_active: true,
                signal: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(early.status, Lifecycle::Idle);

    session.push_event(turn_started("autonomous-wait-1"));
    settle().await;
    let manager = h.manager.clone();
    let wait = tokio::spawn(async move {
        manager
            .wait_for_agent_event(
                ID,
                WaitForAgentOptions {
                    wait_for_active: true,
                    signal: None,
                },
            )
            .await
    });
    settle().await;
    assert!(!wait.is_finished());
    session.push_event(turn_completed("autonomous-wait-1"));
    let result = wait.await.unwrap().unwrap();
    assert_eq!(result.status, Lifecycle::Idle);
}

#[tokio::test(start_paused = true)]
async fn autonomous_events_arriving_during_foreground_run_are_processed_via_subscribe() {
    let release = Gate::new();
    let session = turn_then(turn_completed, "fg-turn-1", Some(release.clone()));
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    let collected = Arc::new(Mutex::new(Vec::new()));
    let mut run = h
        .manager
        .stream_agent(ID, PromptInput::from("foreground run"), RunOpts::default())
        .await
        .unwrap();
    let sink = collected.clone();
    let foreground = tokio::spawn(async move {
        while let Ok(Some(event)) = run.next().await {
            sink.lock().unwrap().push(event);
        }
    });
    h.manager.wait_for_agent_run_start(ID, None).await.unwrap();
    settle().await;

    let autonomous = "autonomous-during-fg-1";
    session.push_event(turn_started(autonomous));
    settle().await;
    let running = h.view(ID);
    assert_eq!(running.lifecycle, Lifecycle::Running);
    assert_eq!(running.active_turn_id.as_deref(), Some("fg-turn-1"));
    session.push_event(timeline_event(
        assistant_item("AUTONOMOUS_DURING_FOREGROUND"),
        "codex",
        Some(autonomous),
    ));
    session.push_event(turn_completed(autonomous));
    settle().await;

    assert!(h
        .items(ID)
        .await
        .contains(&assistant_item("AUTONOMOUS_DURING_FOREGROUND")));
    let still = h.view(ID);
    assert_eq!(still.lifecycle, Lifecycle::Running);
    assert_eq!(still.active_turn_id.as_deref(), Some("fg-turn-1"));

    release.open();
    foreground.await.unwrap();
    let events = collected.lock().unwrap().clone();
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentStreamEvent::TurnCompleted { .. })));
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentStreamEvent::Timeline { item, .. }
                if matches!(&**item, AgentTimelineItem::AssistantMessage { text, .. } if text.contains("AUTONOMOUS_DURING_FOREGROUND"))
        )),
        "the foreground stream carries only its own turn"
    );
    assert!(h
        .items(ID)
        .await
        .contains(&assistant_item("AUTONOMOUS_DURING_FOREGROUND")));
}

#[tokio::test(start_paused = true)]
async fn keeps_updated_at_monotonic_when_user_message_and_run_start_happen_in_the_same_millisecond()
{
    let clock = Arc::new(ManualClock::new(1_750_000_000_000));
    let pinned = clock.clone();
    let h = Harness::with(HarnessOptions {
        tweak: Box::new(move |o| o.clock = pinned),
        ..HarnessOptions::default()
    })
    .await;
    h.agent(ID, &MockSession::auto_completing("codex")).await;

    h.manager
        .append_timeline_item(
            ID,
            AgentTimelineItem::UserMessage {
                text: "hello".into(),
                message_id: None,
                client_message_id: None,
            },
        )
        .await
        .unwrap();
    let message_updated_at = h.view(ID).updated_at_ms;

    let mut stream = h
        .manager
        .stream_agent(ID, PromptInput::from("hello"), RunOpts::default())
        .await
        .unwrap();
    stream.next().await.unwrap();
    assert!(h.view(ID).updated_at_ms > message_updated_at);
    while stream.next().await.unwrap().is_some() {}
}

#[tokio::test(start_paused = true)]
async fn run_agent_assembles_final_text_from_trailing_assistant_chunks() {
    let expected =
        "```json\n{\"message\":\"Reserve space for archive button in sidebar agent list\"}\n```";
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_start(|session, prompt, options| {
            boxed(async move {
                let n = session.record_start(prompt, options);
                let turn_id = format!("chunked-turn-{n}");
                let task = session.clone();
                let turn = turn_id.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::ZERO).await;
                    task.push_event(turn_started(&turn));
                    task.push_event(assistant(
                        "```json\n{\"message\":\"Reserve space for archive button in side",
                        "codex",
                        Some(&turn),
                    ));
                    task.push_event(assistant("bar agent list\"}\n```", "codex", Some(&turn)));
                    task.push_event(turn_completed(&turn));
                });
                Ok(TurnHandle { turn_id })
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;

    let result = h
        .manager
        .run_agent(
            ID,
            PromptInput::from("generate commit message"),
            RunOpts::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.final_text, expected);
    assert_eq!(result.session_id, session.session_id());
}

// ---- attention ----

#[tokio::test(start_paused = true)]
async fn on_attention_is_not_called_for_internal_agents() {
    let h = Harness::new().await;
    h.agent_with(
        ID,
        &MockSession::auto_completing("codex"),
        "codex",
        RegisterOptions::default(),
        |c| {
            c.internal = Some(true);
        },
    )
    .await;

    h.manager
        .run_agent(ID, PromptInput::from("hello"), RunOpts::default())
        .await
        .unwrap();
    settle().await;

    assert!(h.hooks.attention.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn on_attention_is_not_called_for_delegated_child_agents() {
    let h = Harness::new().await;
    let mut labels = indexmap::IndexMap::new();
    labels.insert(
        "paseo.parent-agent-id".to_owned(),
        "parent-agent".to_owned(),
    );
    h.agent_with(
        ID,
        &MockSession::auto_completing("codex"),
        "codex",
        RegisterOptions {
            labels: Some(labels),
            ..RegisterOptions::default()
        },
        |_| {},
    )
    .await;

    h.manager
        .run_agent(ID, PromptInput::from("hello"), RunOpts::default())
        .await
        .unwrap();
    settle().await;

    assert!(h.hooks.attention.lock().unwrap().is_empty());
    // The attention state itself is still set (delegated agents only skip the notification).
    assert!(h.view(ID).attention.requires_attention());
}

#[tokio::test(start_paused = true)]
async fn clear_agent_attention_on_errored_agent_stays_cleared_until_a_new_error_transition() {
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
                session.record_start(prompt, options);
                let turn_id = format!("fail-turn-{attempt}");
                let task = session.clone();
                let turn = turn_id.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::ZERO).await;
                    task.push_event(turn_started(&turn));
                    task.push_event(turn_failed(Some(&turn), &format!("boom-{attempt}")));
                });
                Ok(TurnHandle { turn_id })
            })
        }),
    );
    let h = Harness::with(persisted_harness()).await;
    h.agent(ID, &session).await;

    let error = h
        .manager
        .run_agent(ID, PromptInput::from("fail once"), RunOpts::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("boom-1"));
    h.manager.flush().await;

    let after_first = h.view(ID);
    assert_eq!(after_first.lifecycle, Lifecycle::Error);
    assert!(matches!(
        after_first.attention,
        AttentionState::Required {
            reason: AttentionReason::Error,
            ..
        }
    ));
    let persisted = last_persisted(&h);
    assert_eq!(persisted.lifecycle, Lifecycle::Error);
    assert!(matches!(
        persisted.attention,
        AttentionState::Required {
            reason: AttentionReason::Error,
            ..
        }
    ));

    h.manager.clear_agent_attention(ID).await.unwrap();
    h.manager.notify_agent_state(ID).await;
    h.manager.flush().await;
    let after_clear = h.view(ID);
    assert_eq!(after_clear.lifecycle, Lifecycle::Error);
    assert_eq!(after_clear.attention, AttentionState::None);
    let persisted = last_persisted(&h);
    assert_eq!(persisted.lifecycle, Lifecycle::Error);
    assert_eq!(persisted.attention, AttentionState::None);

    let error = h
        .manager
        .run_agent(ID, PromptInput::from("fail again"), RunOpts::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("boom-2"));
    h.manager.flush().await;
    let after_second = h.view(ID);
    assert_eq!(after_second.lifecycle, Lifecycle::Error);
    assert!(matches!(
        after_second.attention,
        AttentionState::Required {
            reason: AttentionReason::Error,
            ..
        }
    ));
    assert_eq!(
        attention_reasons(&h.hooks),
        vec![AttentionReason::Error, AttentionReason::Error]
    );
}

// ---- turn_failed ----

fn failing_session(
    code: Option<&'static str>,
    diagnostic: Option<&'static str>,
    error: &'static str,
    turn: &'static str,
) -> MockSession {
    MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_start(move |session, prompt, options| {
            boxed(async move {
                session.record_start(prompt, options);
                let task = session.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::ZERO).await;
                    task.push_event(turn_started(turn));
                    task.push_event(AgentStreamEvent::TurnFailed {
                        provider: "codex".into(),
                        error: error.into(),
                        code: code.map(str::to_owned),
                        diagnostic: diagnostic.map(str::to_owned),
                        turn_id: Some(turn.into()),
                    });
                });
                Ok(TurnHandle {
                    turn_id: turn.into(),
                })
            })
        }),
    )
}

#[tokio::test(start_paused = true)]
async fn turn_failed_emits_a_system_error_assistant_timeline_message_and_keeps_error_lifecycle() {
    let h = Harness::new().await;
    h.agent(
        ID,
        &failing_session(None, None, "invalid model id", "turn-failed-1"),
    )
    .await;

    let error = h
        .manager
        .run_agent(ID, PromptInput::from("hello"), RunOpts::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("invalid model id"));

    let view = h.view(ID);
    assert_eq!(view.lifecycle, Lifecycle::Error);
    assert_eq!(view.last_error.as_deref(), Some("invalid model id"));
    let system_errors: Vec<String> = h
        .items(ID)
        .await
        .into_iter()
        .filter_map(|item| match item {
            AgentTimelineItem::AssistantMessage { text, .. } if text.contains("[System Error]") => {
                Some(text)
            }
            _ => None,
        })
        .collect();
    assert_eq!(system_errors.len(), 1);
    assert!(system_errors[0].contains("invalid model id"));
}

#[tokio::test(start_paused = true)]
async fn turn_failed_surfaces_provider_code_and_diagnostic_in_system_error_message() {
    let h = Harness::new().await;
    h.agent(
        ID,
        &failing_session(
            Some("126"),
            Some("No preset version installed for command claude"),
            "Provider execution failed",
            "turn-detailed-fail-1",
        ),
    )
    .await;

    let error = h
        .manager
        .run_agent(ID, PromptInput::from("hello"), RunOpts::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Provider execution failed"));
    assert_eq!(
        h.view(ID).last_error.as_deref(),
        Some("Provider execution failed")
    );

    let text = h
        .items(ID)
        .await
        .into_iter()
        .find_map(|item| match item {
            AgentTimelineItem::AssistantMessage { text, .. } if text.contains("[System Error]") => {
                Some(text)
            }
            _ => None,
        })
        .expect("system error row");
    assert!(text.contains("Provider execution failed"));
    assert!(text.contains("code: 126"));
    assert!(text.contains("No preset version installed for command claude"));
}

// ---- permissions ----

#[tokio::test(start_paused = true)]
async fn permission_request_notifies_once_without_forcing_unread_attention_state() {
    let release = Gate::new();
    let gate = release.clone();
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_start(move |session, prompt, options| {
            let gate = gate.clone();
            boxed(async move {
                session.record_start(prompt, options);
                let task = session.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::ZERO).await;
                    task.push_event(turn_started("turn-perm-1"));
                    task.push_event(AgentStreamEvent::PermissionRequested {
                        provider: "codex".into(),
                        request: Box::new(permission("perm-1", "Read file", PermissionKind::Tool)),
                        turn_id: Some("turn-perm-1".into()),
                    });
                    gate.wait().await;
                    task.push_event(AgentStreamEvent::PermissionResolved {
                        provider: "codex".into(),
                        request_id: "perm-1".into(),
                        resolution: PermissionResponse::Allow {
                            selected_action_id: None,
                            updated_input: None,
                            updated_permissions: None,
                        },
                        turn_id: Some("turn-perm-1".into()),
                    });
                    task.push_event(turn_completed("turn-perm-1"));
                });
                Ok(TurnHandle {
                    turn_id: "turn-perm-1".into(),
                })
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;

    let mut stream = h
        .manager
        .stream_agent(ID, PromptInput::from("permission flow"), RunOpts::default())
        .await
        .unwrap();
    stream.next().await.unwrap(); // turn_started
    stream.next().await.unwrap(); // permission_requested

    let pending = h.view(ID);
    assert_eq!(pending.pending_permissions.len(), 1);
    assert_eq!(pending.attention, AttentionState::None);

    release.open();
    while stream.next().await.unwrap().is_some() {}
    assert!(attention_reasons(&h.hooks).contains(&AttentionReason::Permission));
    assert_eq!(
        attention_reasons(&h.hooks)
            .iter()
            .filter(|r| **r == AttentionReason::Permission)
            .count(),
        1,
        "notified once"
    );
}

fn mode(id: &str, label: &str) -> paseo_agent_types::AgentMode {
    paseo_agent_types::AgentMode {
        id: id.into(),
        label: label.into(),
        description: None,
        icon: None,
        color_tier: None,
        is_unattended: None,
    }
}

#[tokio::test(start_paused = true)]
async fn respond_to_permission_updates_current_mode_id_after_plan_approval() {
    // The provider switches mode internally when the plan is approved (as claude-agent does).
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_respond(|session, _id, response| {
            boxed(async move {
                if matches!(response, PermissionResponse::Allow { .. }) {
                    session.set_mode_id(Some("acceptEdits"));
                }
                Ok(None)
            })
        }),
    );
    session.set_mode_id(Some("plan"));
    session.set_available_modes(vec![
        mode("plan", "Plan"),
        mode("acceptEdits", "Accept Edits"),
    ]);
    let h = Harness::with(persisted_harness()).await;
    let snapshot = h
        .agent_with(ID, &session, "codex", RegisterOptions::default(), |c| {
            c.mode_id = Some("plan".into());
        })
        .await;
    assert_eq!(snapshot.current_mode_id.as_deref(), Some("plan"));

    h.manager
        .testing_mutate_agent(ID, |view| {
            view.pending_permissions.insert(
                "perm-123".into(),
                permission("perm-123", "ExitPlanMode", PermissionKind::Plan),
            );
        })
        .await
        .unwrap();
    h.manager
        .respond_to_permission(
            ID,
            "perm-123",
            PermissionResponse::Allow {
                selected_action_id: None,
                updated_input: None,
                updated_permissions: None,
            },
        )
        .await
        .unwrap();

    let updated = h.view(ID);
    assert_eq!(updated.current_mode_id.as_deref(), Some("acceptEdits"));
    h.manager.flush().await;
    let persisted = last_persisted(&h);
    assert_eq!(persisted.current_mode_id.as_deref(), Some("acceptEdits"));
    // The provider-managed switch must reach config.modeId, or a reload resumes in "plan".
    assert_eq!(persisted.config.mode_id.as_deref(), Some("acceptEdits"));
}

#[tokio::test(start_paused = true)]
async fn respond_to_permission_refreshes_features_and_runtime_info_after_provider_managed_plan_approval(
) {
    let feature = |id: &str, label: &str, value: bool| json!({"type": "toggle", "id": id, "label": label, "value": value});
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_respond({
            move |session, _id, _response| {
                let features = vec![
                    feature("fast_mode", "Fast", true),
                    feature("plan_mode", "Plan", false),
                ];
                boxed(async move {
                    session.set_mode_id(Some("auto"));
                    session.set_pending_permissions(Vec::new());
                    session.set_features(features);
                    session.with_state(|s| {
                        s.runtime_info = Some(paseo_agent_types::AgentRuntimeInfo {
                            provider: "codex".into(),
                            session_id: Some(session_id_of(&session)),
                            model: Some(Some("gpt-5.4".into())),
                            thinking_option_id: None,
                            mode_id: Some(Some("auto".into())),
                            extra: Some(
                                serde_json::from_value(json!({"collaborationMode": "Code"}))
                                    .unwrap(),
                            ),
                        });
                    });
                    Ok(None)
                })
            }
        }),
    );
    session.set_mode_id(Some("auto"));
    session.set_features(vec![
        feature("fast_mode", "Fast", true),
        feature("plan_mode", "Plan", true),
    ]);
    session.set_pending_permissions(vec![permission(
        "perm-plan-1",
        "CodexPlanApproval",
        PermissionKind::Plan,
    )]);
    let h = Harness::with(persisted_harness()).await;
    h.agent(ID, &session).await;
    h.manager
        .testing_mutate_agent(ID, |view| {
            view.pending_permissions.insert(
                "perm-plan-1".into(),
                permission("perm-plan-1", "CodexPlanApproval", PermissionKind::Plan),
            );
        })
        .await
        .unwrap();

    h.manager
        .respond_to_permission(
            ID,
            "perm-plan-1",
            PermissionResponse::Allow {
                selected_action_id: Some("implement".into()),
                updated_input: None,
                updated_permissions: None,
            },
        )
        .await
        .unwrap();

    let updated = h.view(ID);
    assert!(updated.pending_permissions.is_empty());
    let expected_features = vec![
        feature("fast_mode", "Fast", true),
        feature("plan_mode", "Plan", false),
    ];
    assert_eq!(updated.features, expected_features);
    let info = updated.runtime_info.clone().expect("runtime info");
    assert_eq!(info.model, Some(Some("gpt-5.4".into())));
    assert_eq!(
        info.extra.unwrap().get("collaborationMode"),
        Some(&json!("Code"))
    );
    h.manager.flush().await;
    assert_eq!(last_persisted(&h).features, expected_features);
}

fn session_id_of(session: &MockSession) -> String {
    session.session_id().to_owned()
}

#[tokio::test(start_paused = true)]
async fn respond_to_permission_emits_refreshed_state_before_permission_resolved() {
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_respond(|session, _id, _response| {
            boxed(async move {
                // The provider resolves while the response is still in flight.
                session.push_event(AgentStreamEvent::PermissionResolved {
                    provider: "codex".into(),
                    request_id: "perm-order-1".into(),
                    resolution: PermissionResponse::Allow {
                        selected_action_id: None,
                        updated_input: None,
                        updated_permissions: None,
                    },
                    turn_id: None,
                });
                session.set_mode_id(Some("acceptEdits"));
                session.set_features(vec![
                    json!({"type": "toggle", "id": "fast_mode", "label": "Fast", "value": false}),
                ]);
                session.set_pending_permissions(Vec::new());
                Ok(None)
            })
        }),
    );
    session.set_mode_id(Some("plan"));
    session.set_features(vec![
        json!({"type": "toggle", "id": "fast_mode", "label": "Fast", "value": true}),
    ]);
    session.set_pending_permissions(vec![permission(
        "perm-order-1",
        "ExitPlanMode",
        PermissionKind::Plan,
    )]);
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    h.events.clear();

    h.manager
        .respond_to_permission(
            ID,
            "perm-order-1",
            PermissionResponse::Allow {
                selected_action_id: None,
                updated_input: None,
                updated_permissions: None,
            },
        )
        .await
        .unwrap();
    settle().await;

    let mut seen: Vec<String> = Vec::new();
    for event in h.events.all() {
        match &*event {
            ManagerEvent::AgentState { agent } if agent.id == ID => {
                let fast = agent
                    .features
                    .iter()
                    .find(|f| f.get("id") == Some(&Value::String("fast_mode".into())))
                    .and_then(|f| f.get("value").cloned());
                seen.push(format!(
                    "state:{}:{}",
                    agent.current_mode_id.clone().unwrap_or_default(),
                    fast.map_or("null".into(), |v| v.to_string())
                ));
            }
            ManagerEvent::AgentStream {
                event: AgentStreamEvent::PermissionResolved { request_id, .. },
                ..
            } => {
                seen.push(format!("resolved:{request_id}"));
            }
            _ => {}
        }
    }
    let refreshed = seen
        .iter()
        .position(|e| e == "state:acceptEdits:false")
        .expect("refreshed state");
    let resolved = seen
        .iter()
        .position(|e| e == "resolved:perm-order-1")
        .expect("resolved dispatch");
    assert!(resolved > refreshed, "{seen:?}");
}

#[tokio::test(start_paused = true)]
async fn a_second_response_to_the_same_permission_request_is_rejected_while_one_is_in_flight() {
    let entered = Gate::new();
    let release = Gate::new();
    let (entered2, release2) = (entered.clone(), release.clone());
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_respond(move |_, _, _| {
            let (entered, release) = (entered2.clone(), release2.clone());
            boxed(async move {
                entered.open();
                release.wait().await;
                Ok(None)
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    let allow = || PermissionResponse::Allow {
        selected_action_id: None,
        updated_input: None,
        updated_permissions: None,
    };

    let manager = h.manager.clone();
    let first =
        tokio::spawn(async move { manager.respond_to_permission(ID, "perm-1", allow()).await });
    entered.wait().await;
    let error = h
        .manager
        .respond_to_permission(ID, "perm-1", allow())
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "A response to this permission request is already being submitted"
    );

    release.open();
    first.await.unwrap().unwrap();
    // The claim is released: the next response goes through.
    h.manager
        .respond_to_permission(ID, "perm-1", allow())
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn turn_failed_and_turn_canceled_resolve_every_pending_permission_with_a_dispatched_denial() {
    for (terminal, message) in [
        ("turn_failed", "Turn failed"),
        ("turn_canceled", "Interrupted"),
    ] {
        let h = Harness::new().await;
        let session = MockSession::new("codex");
        h.agent(ID, &session).await;
        session.push_event(turn_started("t1"));
        session.push_event(AgentStreamEvent::PermissionRequested {
            provider: "codex".into(),
            request: Box::new(permission("p1", "Read", PermissionKind::Tool)),
            turn_id: Some("t1".into()),
        });
        settle().await;
        assert_eq!(h.view(ID).pending_permissions.len(), 1);

        session.push_event(if terminal == "turn_failed" {
            turn_failed(Some("t1"), "boom")
        } else {
            turn_canceled(Some("t1"), "stop")
        });
        settle().await;

        assert!(h.view(ID).pending_permissions.is_empty(), "{terminal}");
        let denial = h
            .events
            .streams(ID)
            .into_iter()
            .find_map(|e| match stream_event(&e) {
                AgentStreamEvent::PermissionResolved { resolution, .. } => Some(resolution.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{terminal}: denial dispatched"));
        assert!(matches!(
            denial,
            PermissionResponse::Deny { message: Some(ref m), .. } if m == message
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn subscription_replays_state_then_streams_live_events_for_wait_for_agent_event_permissions()
{
    // `waitForAgentEvent` resolves immediately on an existing pending permission and reports it.
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    session.push_event(turn_started("t1"));
    session.push_event(AgentStreamEvent::PermissionRequested {
        provider: "codex".into(),
        request: Box::new(permission("p1", "Read", PermissionKind::Tool)),
        turn_id: Some("t1".into()),
    });
    settle().await;

    let result = h
        .manager
        .wait_for_agent_event(ID, WaitForAgentOptions::default())
        .await
        .unwrap();
    assert_eq!(result.permission.map(|p| p.id), Some("p1".into()));
    assert_eq!(result.status, Lifecycle::Running);
    let _ = SubscribeOpts::default();
}
