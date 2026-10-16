//! Ports of the history-hydration, rewind, out-of-band and timeline-replacement cases of
//! `agent-manager.test.ts`.
//!
//! The TS cases that read committed rows through `durableTimelineStore` have no durable store
//! to read here (non-goal): their in-memory halves are ported.

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use paseo_agent_types::{
    AgentStreamEvent, AgentTimelineItem, ProviderSubagentInputEvent, ProviderSubagentStatus,
};
use paseo_core::mock::{boxed, Gate, MockClient, MockSession, ScriptedBehavior};
use paseo_core::{
    CreateAgentOptions, HydrateOpts, ManagerEvent, ReloadOptions, RewindMode, RunOpts,
    StartRunOptions,
};
use paseo_provider_api::{AgentSessionConfig, ProviderError, TurnHandle};
use serde_json::json;

async fn within<T>(work: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), work)
        .await
        .expect("timed out")
}

fn history_client(history: Vec<AgentStreamEvent>) -> MockClient {
    let client = MockClient::new("codex");
    client.on_create(move |req| {
        let history = history.clone();
        boxed(async move {
            Ok(MockSession::builder("codex")
                .config(&req.config)
                .auto_completing()
                .behavior(ScriptedBehavior::new().on_history(move |_| {
                    let history = history.clone();
                    boxed(async move { Ok(history) })
                }))
                .build())
        })
    });
    client
}

fn user_event(text: &str, message_id: &str) -> AgentStreamEvent {
    timeline_event(
        AgentTimelineItem::UserMessage {
            text: text.into(),
            message_id: Some(message_id.into()),
            client_message_id: None,
        },
        "codex",
        None,
    )
}

fn force() -> HydrateOpts {
    HydrateOpts {
        force: true,
        ..HydrateOpts::default()
    }
}

// ---- the in-memory halves of the durable-store cases ----

#[tokio::test]
async fn appended_items_are_visible_in_the_rows_without_a_durable_store() {
    let lc = Lc::new().await;
    let agent = lc.create().await;
    let (seq, epoch) = lc
        .manager
        .append_timeline_item(&agent.id, assistant_item("stored"))
        .await
        .unwrap();
    lc.flush().await;
    assert_eq!(seq, 1);
    assert_eq!(lc.items(&agent.id).await, vec![assistant_item("stored")]);
    assert_eq!(
        lc.manager
            .fetch_timeline(&agent.id, Default::default())
            .await
            .unwrap()
            .epoch,
        epoch
    );
}

#[tokio::test]
async fn refreshing_an_agent_replaces_the_timeline_instead_of_appending_a_second_copy() {
    let history = vec![
        timeline_event(
            AgentTimelineItem::UserMessage {
                text: "continue".into(),
                message_id: None,
                client_message_id: None,
            },
            "codex",
            None,
        ),
        assistant("done", "codex", None),
    ];
    let client = history_client(history.clone());
    client.on_resume(move |req| {
        let history = history.clone();
        boxed(async move {
            let session = MockSession::builder("codex")
                .auto_completing()
                .behavior(ScriptedBehavior::new().on_history(move |_| {
                    let history = history.clone();
                    boxed(async move { Ok(history) })
                }))
                .build();
            session.set_persistence(req.handle.clone());
            Ok(session)
        })
    });
    let lc = Lc::with_client(client).await;
    let agent = lc.create().await;

    // What `session.handleRefreshAgentRequest` does for a loaded agent, twice over.
    for _ in 0..2 {
        lc.manager
            .reload_agent_session(
                &agent.id,
                None,
                ReloadOptions {
                    rehydrate_from_disk: true,
                },
            )
            .await
            .unwrap();
        lc.manager
            .hydrate_timeline_from_provider(
                &agent.id,
                HydrateOpts {
                    broadcast: true.into(),
                    ..HydrateOpts::default()
                },
            )
            .await
            .unwrap();
        lc.manager.flush().await;
    }

    assert_eq!(
        lc.items(&agent.id).await,
        vec![
            AgentTimelineItem::UserMessage {
                text: "continue".into(),
                message_id: None,
                client_message_id: None
            },
            assistant_item("done")
        ]
    );
}

#[tokio::test]
async fn a_failed_history_replay_leaves_the_timeline_untouched_and_a_retry_replaces_it() {
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let behavior = |fail: Arc<std::sync::atomic::AtomicBool>| {
        ScriptedBehavior::new().on_history(move |_| {
            let fail = fail.clone();
            boxed(async move {
                if fail.load(Ordering::SeqCst) {
                    return Err(ProviderError::provider("history stream closed"));
                }
                Ok(vec![
                    timeline_event(
                        AgentTimelineItem::UserMessage {
                            text: "continue".into(),
                            message_id: None,
                            client_message_id: None,
                        },
                        "codex",
                        None,
                    ),
                    assistant("done", "codex", None),
                ])
            })
        })
    };
    let client = MockClient::new("codex");
    {
        let fail = fail.clone();
        client.on_create(move |req| {
            let fail = fail.clone();
            boxed(async move {
                Ok(MockSession::builder("codex")
                    .config(&req.config)
                    .auto_completing()
                    .behavior(behavior(fail))
                    .build())
            })
        });
    }
    {
        let fail = fail.clone();
        client.on_resume(move |req| {
            let fail = fail.clone();
            boxed(async move {
                let session = MockSession::builder("codex")
                    .auto_completing()
                    .behavior(behavior(fail))
                    .build();
                session.set_persistence(req.handle.clone());
                Ok(session)
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let agent = lc.create().await;
    let rehydrate = ReloadOptions {
        rehydrate_from_disk: true,
    };
    let broadcast = || HydrateOpts {
        broadcast: true.into(),
        ..HydrateOpts::default()
    };
    lc.manager
        .reload_agent_session(&agent.id, None, rehydrate)
        .await
        .unwrap();
    lc.manager
        .hydrate_timeline_from_provider(&agent.id, broadcast())
        .await
        .unwrap();
    assert_eq!(lc.items(&agent.id).await.len(), 2);

    fail.store(true, Ordering::SeqCst);
    lc.manager
        .reload_agent_session(&agent.id, None, rehydrate)
        .await
        .unwrap();
    let error = lc
        .manager
        .hydrate_timeline_from_provider(&agent.id, broadcast())
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "history stream closed");
    assert!(
        lc.items(&agent.id).await.is_empty(),
        "no partial copy was committed"
    );
    assert!(!lc.manager.get_agent(&agent.id).unwrap().history_primed);

    // A retry once the provider recovers replaces that history rather than stacking on it.
    fail.store(false, Ordering::SeqCst);
    lc.manager
        .hydrate_timeline_from_provider(&agent.id, broadcast())
        .await
        .unwrap();
    assert_eq!(lc.items(&agent.id).await.len(), 2);
}

// ---- provider children ----

fn child_upsert(id: &str, status: ProviderSubagentStatus, title: Option<&str>) -> AgentStreamEvent {
    AgentStreamEvent::ProviderSubagent {
        provider: "codex".into(),
        event: Box::new(ProviderSubagentInputEvent::Upsert {
            id: id.into(),
            title: title.map(|t| Some(t.to_owned())),
            description: None,
            status: Some(status),
            tool_call_id: None,
            cwd: None,
            subtitle: None,
            parent_subagent_id: None,
            timestamp: None,
        }),
    }
}

#[tokio::test]
async fn hydrate_timeline_from_provider_restores_and_broadcasts_provider_children_from_session_history(
) {
    let client = history_client(vec![child_upsert(
        "restored-child",
        ProviderSubagentStatus::Completed,
        Some("Restored child"),
    )]);
    let lc = Lc::with_client(client).await;
    let snapshot = lc.create().await;
    let events = collect(
        &lc.manager,
        paseo_core::SubscribeOpts::for_agent(snapshot.id.clone()).without_replay(),
    )
    .await;

    lc.manager
        .hydrate_timeline_from_provider(
            &snapshot.id,
            HydrateOpts {
                force: true,
                broadcast: true.into(),
                ..HydrateOpts::default()
            },
        )
        .await
        .unwrap();

    let children = lc.manager.list_provider_subagents(&snapshot.id).unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].id, "restored-child");
    assert_eq!(children[0].parent_agent_id, snapshot.id);
    assert_eq!(children[0].title.as_deref(), Some("Restored child"));
    assert_eq!(children[0].status, ProviderSubagentStatus::Completed);
    assert_eq!(lc.manager.list_provider_subagent_activity().len(), 1);
    lc.sync().await;
    assert!(events.all().iter().any(|e| matches!(
        &**e,
        ManagerEvent::ProviderSubagent {
            event: paseo_core::provider_subagents::SubagentStoreEvent::Upsert { subagent },
            ..
        } if subagent.id == "restored-child"
    )));
}

#[tokio::test]
async fn force_provider_hydration_removes_children_absent_from_current_history() {
    let lc = Lc::new().await;
    let snapshot = lc.create().await;
    lc.client.created_sessions()[0].push_event(child_upsert(
        "removed-by-rewind",
        ProviderSubagentStatus::Completed,
        None,
    ));
    within(async {
        while lc
            .manager
            .list_provider_subagents(&snapshot.id)
            .unwrap()
            .is_empty()
        {
            settle().await;
        }
    })
    .await;
    let events = collect(
        &lc.manager,
        paseo_core::SubscribeOpts::for_agent(snapshot.id.clone()).without_replay(),
    )
    .await;

    lc.manager
        .hydrate_timeline_from_provider(
            &snapshot.id,
            HydrateOpts {
                force: true,
                broadcast: true.into(),
                ..HydrateOpts::default()
            },
        )
        .await
        .unwrap();

    assert!(lc
        .manager
        .list_provider_subagents(&snapshot.id)
        .unwrap()
        .is_empty());
    lc.sync().await;
    assert!(events.all().iter().any(|e| matches!(
        &**e,
        ManagerEvent::ProviderSubagent {
            event: paseo_core::provider_subagents::SubagentStoreEvent::Remove { subagent_id, parent_agent_id },
            ..
        } if subagent_id == "removed-by-rewind" && *parent_agent_id == snapshot.id
    )));
}

// ---- history content ----

#[tokio::test]
async fn hydrate_timeline_preserves_assistant_chunk_reasoning_and_tool_timeline_history() {
    let tool = tool_call(
        "call-history-1",
        paseo_agent_types::ToolCallStatus::Completed,
        "hi\n",
        None,
    );
    let client = history_client(vec![
        assistant("chunk one ", "codex", None),
        assistant("chunk two", "codex", None),
        reasoning("internal", "codex", None),
        timeline_event(tool.clone(), "codex", None),
        assistant("final answer", "codex", None),
    ]);
    let lc = Lc::with_client(client).await;
    let snapshot = lc.create().await;

    lc.manager
        .hydrate_timeline_from_provider(&snapshot.id, force())
        .await
        .unwrap();

    assert_eq!(
        lc.items(&snapshot.id).await,
        vec![
            assistant_item("chunk one chunk two"),
            AgentTimelineItem::Reasoning {
                text: "internal".into()
            },
            tool,
            assistant_item("final answer"),
        ]
    );
}

#[tokio::test]
async fn hydrate_timeline_preserves_reasoning_between_assistant_chunks() {
    let client = history_client(vec![
        assistant("before reasoning ", "codex", None),
        reasoning("internal step", "codex", None),
        assistant("after reasoning", "codex", None),
    ]);
    let lc = Lc::with_client(client).await;
    let snapshot = lc.create().await;
    lc.manager
        .hydrate_timeline_from_provider(&snapshot.id, force())
        .await
        .unwrap();
    assert_eq!(
        lc.items(&snapshot.id).await,
        vec![
            assistant_item("before reasoning "),
            AgentTimelineItem::Reasoning {
                text: "internal step".into()
            },
            assistant_item("after reasoning"),
        ]
    );
}

#[tokio::test]
async fn hydrate_timeline_keeps_provider_user_message_items_when_no_canonical_user_history_exists()
{
    let client = history_client(vec![
        user_event("hello from user", "msg_history_1"),
        assistant("hi there", "codex", None),
        user_event("second question", "msg_history_2"),
        assistant("second answer", "codex", None),
    ]);
    let lc = Lc::with_client(client).await;
    let snapshot = lc.create().await;
    lc.manager
        .hydrate_timeline_from_provider(&snapshot.id, force())
        .await
        .unwrap();
    let items = lc.items(&snapshot.id).await;
    assert_eq!(user_message_texts(&items).len(), 2);
    assert_eq!(
        items
            .iter()
            .filter(|i| matches!(i, AgentTimelineItem::AssistantMessage { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn hydrate_timeline_preserves_provider_replay_timestamps_and_marks_missing_ones_untrusted() {
    let stamped = AgentStreamEvent::Timeline {
        item: Box::new(AgentTimelineItem::UserMessage {
            text: "hello".into(),
            message_id: Some("msg_history_1".into()),
            client_message_id: None,
        }),
        provider: "codex".into(),
        turn_id: None,
        timestamp: Some("2026-05-01T10:00:00.000Z".into()),
    };
    let client = history_client(vec![
        stamped,
        assistant("no original timestamp", "codex", None),
    ]);
    let lc = Lc::with_client(client).await;
    let snapshot = lc.create().await;
    lc.manager
        .hydrate_timeline_from_provider(&snapshot.id, force())
        .await
        .unwrap();

    let rows = lc
        .manager
        .fetch_timeline(
            &snapshot.id,
            paseo_timeline::FetchOptions {
                limit: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].timestamp, "2026-05-01T10:00:00.000Z");
    assert!(
        matches!(&rows[0].item, AgentTimelineItem::UserMessage { text, message_id: Some(m), .. } if text == "hello" && m == "msg_history_1")
    );
    assert!(!rows[1].timestamp.is_empty());
    assert_ne!(rows[1].timestamp, rows[0].timestamp);
}

#[tokio::test]
async fn user_message_events_wrapping_a_paseo_system_envelope_are_not_restored_during_history_replay(
) {
    let client = history_client(vec![
        user_event(
            &paseo_timeline::agent_message::format_system_notification_prompt("schedule fired"),
            "msg_history_envelope",
        ),
        user_event("real user message", "msg_history_real"),
        assistant("reply", "codex", None),
    ]);
    let lc = Lc::with_client(client).await;
    let snapshot = lc.create().await;
    lc.manager
        .hydrate_timeline_from_provider(&snapshot.id, force())
        .await
        .unwrap();
    assert_eq!(
        user_message_texts(&lc.items(&snapshot.id).await),
        vec!["real user message"]
    );
}

#[tokio::test]
async fn force_hydration_mints_a_new_epoch_and_a_prime_keeps_the_epoch() {
    let client = history_client(vec![assistant("history", "codex", None)]);
    let lc = Lc::with_client(client).await;
    // A resumed agent is not primed: the first hydration appends to the current epoch.
    let resumed = lc
        .manager
        .resume_agent_from_persistence(
            handle("codex", "epochs", &lc.cwd()),
            Some(obj(json!({"cwd": lc.cwd()}))),
            None,
            Default::default(),
            None,
        )
        .await
        .unwrap();
    lc.client
        .on_resume(|_| boxed(async { Ok(MockSession::new("codex")) }));
    let epoch = |id: &str| {
        let manager = lc.manager.clone();
        let id = id.to_owned();
        async move {
            manager
                .fetch_timeline(&id, Default::default())
                .await
                .unwrap()
                .epoch
        }
    };
    let before = epoch(&resumed.id).await;
    // The default resume session has no scripted history, so prime with the mock's own.
    lc.client.created_sessions()[0].set_history(vec![assistant("history", "codex", None)]);
    lc.manager
        .hydrate_timeline_from_provider(&resumed.id, Default::default())
        .await
        .unwrap();
    assert_eq!(
        epoch(&resumed.id).await,
        before,
        "prime appends within the epoch"
    );
    assert_eq!(lc.items(&resumed.id).await, vec![assistant_item("history")]);

    lc.manager
        .hydrate_timeline_from_provider(&resumed.id, force())
        .await
        .unwrap();
    assert_ne!(epoch(&resumed.id).await, before, "force starts a new epoch");
    assert_eq!(lc.items(&resumed.id).await, vec![assistant_item("history")]);
}

#[tokio::test]
async fn a_deferred_broadcast_is_read_when_the_hydration_applies() {
    let lc = Lc::new().await;
    let resumed = lc
        .manager
        .resume_agent_from_persistence(
            handle("codex", "deferred", &lc.cwd()),
            Some(obj(json!({"cwd": lc.cwd()}))),
            None,
            Default::default(),
            None,
        )
        .await
        .unwrap();
    lc.client.created_sessions()[0].set_history(vec![assistant("late", "codex", None)]);
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    lc.events.clear();
    lc.manager
        .hydrate_timeline_from_provider(
            &resumed.id,
            HydrateOpts {
                broadcast: paseo_core::Broadcast::Deferred(flag.clone()),
                ..HydrateOpts::default()
            },
        )
        .await
        .unwrap();
    lc.sync().await;
    assert!(
        lc.events.timeline_streams(&resumed.id).is_empty(),
        "flag was off at apply time"
    );

    let second = lc
        .manager
        .resume_agent_from_persistence(
            handle("codex", "deferred-2", &lc.cwd()),
            Some(obj(json!({"cwd": lc.cwd()}))),
            None,
            Default::default(),
            None,
        )
        .await
        .unwrap();
    lc.client.created_sessions()[1].set_history(vec![assistant("late", "codex", None)]);
    flag.store(true, Ordering::SeqCst);
    lc.manager
        .hydrate_timeline_from_provider(
            &second.id,
            HydrateOpts {
                broadcast: paseo_core::Broadcast::Deferred(flag),
                ..HydrateOpts::default()
            },
        )
        .await
        .unwrap();
    lc.sync().await;
    assert_eq!(lc.events.timeline_streams(&second.id).len(), 1);
}

// ---- rewind ----

fn rewind_session(
    config: &AgentSessionConfig,
    reverted: &Arc<Mutex<Vec<(String, String)>>>,
    echo_gate: Option<Gate>,
) -> MockSession {
    let (reverted, starts) = (reverted.clone(), Arc::new(AtomicU32::new(0)));
    MockSession::builder("codex")
        .config(config)
        .capability("supportsRewindFiles", true)
        .capability("supportsRewindConversation", true)
        .behavior(
            ScriptedBehavior::new()
                .on_start(move |session, prompt, options| {
                    let (echo_gate, starts) = (echo_gate.clone(), starts.clone());
                    boxed(async move {
                        let text = match &prompt {
                            paseo_provider_api::PromptInput::Text(t) => t.clone(),
                            paseo_provider_api::PromptInput::Blocks(_) => String::new(),
                        };
                        let client_message_id = options
                            .as_ref()
                            .and_then(|o| o.get("clientMessageId"))
                            .and_then(|v| v.as_str())
                            .map(str::to_owned);
                        session.record_start(prompt, options);
                        let nth = starts.fetch_add(1, Ordering::SeqCst);
                        let turn_id = "turn-submitted-user-message".to_owned();
                        let (soon, turn) = (session.clone(), turn_id.clone());
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::ZERO).await;
                            soon.push_event(provider_event_turn_started("codex", &turn));
                            soon.push_event(assistant(
                                "output before provider echo",
                                "codex",
                                Some(&turn),
                            ));
                            if let Some(gate) = &echo_gate {
                                gate.wait().await;
                            }
                            let _ = nth;
                            soon.push_event(timeline_event(
                                AgentTimelineItem::UserMessage {
                                    text,
                                    message_id: Some("provider-message-1".into()),
                                    client_message_id,
                                },
                                "codex",
                                Some(&turn),
                            ));
                            soon.push_event(turn_completed(&turn));
                        });
                        Ok(TurnHandle { turn_id })
                    })
                })
                .on_interrupt(|session| {
                    boxed(async move {
                        session.with_state(|s| s.interrupt_count += 1);
                        session.push_event(turn_canceled(
                            Some("turn-submitted-user-message"),
                            "interrupted",
                        ));
                        Ok(())
                    })
                })
                .on_revert(move |_, kind, message_id| {
                    let reverted = reverted.clone();
                    boxed(async move {
                        reverted.lock().unwrap().push((kind, message_id));
                        Ok(())
                    })
                }),
        )
        .build()
}

#[tokio::test]
async fn canonical_submitted_prompt_keeps_wire_identity_while_rewind_resolves_provider_identity() {
    let client = MockClient::new("codex");
    let reverted: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let echo = Gate::new();
    {
        let (reverted, echo) = (reverted.clone(), echo.clone());
        client.on_create(move |req| {
            let (reverted, echo) = (reverted.clone(), echo.clone());
            boxed(async move { Ok(rewind_session(&req.config, &reverted, Some(echo))) })
        });
    }
    let lc = Lc::with_client(client).await;
    let snapshot = lc.create().await;

    let run = {
        let (manager, id) = (lc.manager.clone(), snapshot.id.clone());
        tokio::spawn(async move {
            manager
                .run_agent(
                    &id,
                    "hello from composer".into(),
                    RunOpts::with_client_message_id("msg-client-1"),
                )
                .await
        })
    };
    settle().await;
    lc.manager
        .wait_for_agent_run_start(&snapshot.id, None)
        .await
        .unwrap();

    let error = lc
        .manager
        .rewind(&snapshot.id, "msg-client-1", RewindMode::Files)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Cannot rewind before the provider acknowledges the submitted prompt"
    );
    assert_eq!(lc.client.created_sessions()[0].interrupt_count(), 0);

    echo.open();
    let outcome = within(run).await.unwrap().unwrap();
    assert!(!outcome.canceled);
    lc.sync().await;

    let kinds: Vec<String> = lc
        .events
        .streams(&snapshot.id)
        .iter()
        .map(|e| match stream_event(e) {
            AgentStreamEvent::Timeline { item, .. } => {
                format!("{}@{:?}", item.type_name(), stream_seq(e))
            }
            other => other.type_name().to_owned(),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "turn_started",
            "user_message@Some(1)",
            "assistant_message@Some(2)",
            "turn_completed"
        ]
    );
    let user = lc
        .events
        .timeline_streams(&snapshot.id)
        .into_iter()
        .find_map(|e| match stream_event(&e) {
            AgentStreamEvent::Timeline { item, .. } => match &**item {
                AgentTimelineItem::UserMessage {
                    text,
                    client_message_id,
                    message_id,
                } => Some((text.clone(), client_message_id.clone(), message_id.clone())),
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    assert_eq!(
        user,
        (
            "hello from composer".into(),
            Some("msg-client-1".into()),
            Some("msg-client-1".into())
        )
    );

    let rows = lc
        .manager
        .fetch_timeline(
            &snapshot.id,
            paseo_timeline::FetchOptions {
                limit: Some(20),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].seq(), 1);
    assert_eq!(
        rows[0].provider_message_id.as_deref(),
        Some("provider-message-1")
    );
    assert_eq!(
        rows[0].turn_id.as_deref(),
        Some("turn-submitted-user-message")
    );
    assert_eq!(rows[1].seq(), 2);
    assert_eq!(rows[1].item, assistant_item("output before provider echo"));

    lc.manager
        .rewind(&snapshot.id, "msg-client-1", RewindMode::Files)
        .await
        .unwrap();
    lc.manager
        .rewind(&snapshot.id, "provider-native-message", RewindMode::Files)
        .await
        .unwrap();
    assert_eq!(
        *reverted.lock().unwrap(),
        vec![
            ("files".to_owned(), "provider-message-1".to_owned()),
            ("files".to_owned(), "provider-native-message".to_owned())
        ]
    );
}

#[tokio::test]
async fn rewind_conversation_re_reads_history_and_announces_the_replacement() {
    let client = MockClient::new("codex");
    let reverted: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    {
        let reverted = reverted.clone();
        client.on_create(move |req| {
            let reverted = reverted.clone();
            boxed(async move {
                let session = rewind_session(&req.config, &reverted, None);
                session.set_history(vec![assistant("rewound history", "codex", None)]);
                Ok(session)
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let agent = lc.create().await;
    lc.manager
        .append_timeline_item(&agent.id, assistant_item("before rewind"))
        .await
        .unwrap();
    let old_epoch = lc
        .manager
        .fetch_timeline(&agent.id, Default::default())
        .await
        .unwrap()
        .epoch;
    lc.events.clear();

    lc.manager
        .rewind(&agent.id, "target", RewindMode::Conversation)
        .await
        .unwrap();

    assert_eq!(
        reverted.lock().unwrap()[0],
        ("conversation".to_owned(), "target".to_owned())
    );
    assert_eq!(
        lc.items(&agent.id).await,
        vec![assistant_item("rewound history")]
    );
    let new_epoch = lc
        .manager
        .fetch_timeline(&agent.id, Default::default())
        .await
        .unwrap()
        .epoch;
    assert_ne!(new_epoch, old_epoch);
    lc.sync().await;
    let replacements: Vec<_> = lc
        .events
        .all()
        .into_iter()
        .filter_map(|e| match &*e {
            ManagerEvent::TimelineReplacement { agent_id, epoch } => {
                Some((agent_id.clone(), epoch.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(replacements, vec![(agent.id.clone(), new_epoch)]);
    assert!(
        lc.events.timeline_streams(&agent.id).is_empty(),
        "rewind publishes the replacement, not the rows (broadcastTimeline: false)"
    );
    assert!(!lc.events.states(&agent.id).is_empty());
}

#[tokio::test]
async fn rewind_requires_the_matching_capability_and_never_leaves_the_run_slot_parked() {
    let lc = Lc::new().await;
    let agent = lc.create().await;
    let error = lc
        .manager
        .rewind(&agent.id, "m", RewindMode::Both)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Provider does not support rewinding both"
    );
    // The failed rewind released its lock: a prompt is admitted again.
    lc.manager
        .run_agent(&agent.id, "after".into(), Default::default())
        .await
        .unwrap();
}

#[tokio::test]
async fn rewind_parks_the_run_slot_so_a_concurrent_prompt_is_refused() {
    let client = MockClient::new("codex");
    let revert_started = Gate::new();
    let revert_allowed = Gate::new();
    {
        let (started, allowed) = (revert_started.clone(), revert_allowed.clone());
        client.on_create(move |req| {
            let (started, allowed) = (started.clone(), allowed.clone());
            boxed(async move {
                Ok(MockSession::builder("codex")
                    .config(&req.config)
                    .auto_completing()
                    .capability("supportsRewindFiles", true)
                    .behavior(ScriptedBehavior::new().on_revert(move |_, _, _| {
                        let (started, allowed) = (started.clone(), allowed.clone());
                        boxed(async move {
                            started.open();
                            allowed.wait().await;
                            Ok(())
                        })
                    }))
                    .build())
            })
        });
    }
    let lc = Lc::with_client(client).await;
    let agent = lc.create().await;
    let rewinding = {
        let (manager, id) = (lc.manager.clone(), agent.id.clone());
        tokio::spawn(async move { manager.rewind(&id, "m", RewindMode::Files).await })
    };
    within(revert_started.wait()).await;

    let error = lc
        .manager
        .stream_agent(&agent.id, "concurrent".into(), Default::default())
        .await
        .err()
        .expect("admission is refused");
    assert_eq!(
        error.to_string(),
        format!("Agent {} already has an active run", agent.id)
    );
    assert!(lc.manager.has_in_flight_run(&agent.id));

    revert_allowed.open();
    within(rewinding).await.unwrap().unwrap();
    assert!(!lc.manager.has_in_flight_run(&agent.id));
}

// ---- out of band ----

fn out_of_band_client(handled: &'static str) -> MockClient {
    let client = MockClient::new("codex");
    client.on_create(move |req| {
        boxed(async move {
            Ok(MockSession::builder("codex")
                .config(&req.config)
                .auto_completing()
                .behavior(
                    ScriptedBehavior::new().on_out_of_band(move |session, prompt| {
                        boxed(async move {
                            if prompt != paseo_provider_api::PromptInput::from(handled) {
                                return Ok(false);
                            }
                            session.push_event(assistant("Handled by the daemon", "codex", None));
                            Ok(true)
                        })
                    }),
                )
                .build())
        })
    });
    client
}

#[tokio::test]
async fn authoritative_timeline_records_a_daemon_handled_submitted_prompt_before_its_output() {
    let lc = Lc::with_client(out_of_band_client("/handled")).await;
    let snapshot = lc.create().await;
    let events = collect(
        &lc.manager,
        paseo_core::SubscribeOpts::for_agent(snapshot.id.clone()).without_replay(),
    )
    .await;

    let disposition = lc
        .manager
        .start_agent_run(
            &snapshot.id,
            "/handled".into(),
            StartRunOptions {
                run: RunOpts::with_client_message_id("msg-client-daemon-handled"),
                ..StartRunOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(disposition, paseo_core::PromptDisposition::OutOfBand);
    lc.manager.flush().await;
    lc.sync().await;

    let expected = vec![
        AgentTimelineItem::UserMessage {
            text: "/handled".into(),
            message_id: None,
            client_message_id: Some("msg-client-daemon-handled".into()),
        },
        assistant_item("Handled by the daemon"),
    ];
    let streamed: Vec<_> = events
        .timeline_streams(&snapshot.id)
        .iter()
        .filter_map(|e| match stream_event(e) {
            AgentStreamEvent::Timeline { item, .. } => Some((**item).clone()),
            _ => None,
        })
        .collect();
    assert_eq!(streamed, expected);
    let rows: Vec<_> = lc
        .manager
        .fetch_timeline(
            &snapshot.id,
            paseo_timeline::FetchOptions {
                limit: Some(20),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .rows
        .into_iter()
        .map(|r| r.item)
        .collect();
    assert_eq!(rows, expected);
    assert_eq!(
        lc.client.created_sessions()[0].start_count(),
        0,
        "no turn was started"
    );
}

#[tokio::test]
async fn an_unhandled_prompt_starts_a_normal_turn_and_a_failed_out_of_band_command_leaves_an_error_row(
) {
    let lc = Lc::with_client(out_of_band_client("/handled")).await;
    let snapshot = lc.create().await;
    let disposition = lc
        .manager
        .start_agent_run(
            &snapshot.id,
            "plain prompt".into(),
            StartRunOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(disposition, paseo_core::PromptDisposition::TurnStarted);
    lc.manager
        .wait_for_agent_event(&snapshot.id, Default::default())
        .await
        .unwrap();
    assert_eq!(lc.client.created_sessions()[0].start_count(), 1);

    let failing = MockClient::new("codex");
    failing.on_create(|req| {
        boxed(async move {
            Ok(MockSession::builder("codex")
                .config(&req.config)
                .behavior(ScriptedBehavior::new().on_out_of_band(|_, _| {
                    boxed(async { Err(ProviderError::provider("command exploded")) })
                }))
                .build())
        })
    });
    let lc = Lc::with_client(failing).await;
    let agent = lc.create().await;
    assert!(lc
        .manager
        .try_run_out_of_band(&agent.id, "/x".into(), RunOpts::default())
        .await
        .unwrap());
    assert_eq!(
        lc.items(&agent.id).await,
        vec![assistant_item("[Error] command exploded")]
    );
}

#[allow(dead_code)]
fn _config(_: &AgentSessionConfig, _: CreateAgentOptions) {}
