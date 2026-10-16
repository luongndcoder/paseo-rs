//! Port of the timeline read / projection cases of `agent-manager.test.ts` that do not need
//! hydration, reload or rewind (`fetchTimeline` windows, rows fallback, streamed chunk retention,
//! provider `user_message` recording, submitted prompt identity, system envelope filtering).

mod common;

use common::*;
use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem};
use paseo_core::mock::{boxed, Gate, MockSession, ScriptedBehavior};
use paseo_core::RunOpts;
use paseo_provider_api::{PromptInput, TurnHandle};
use paseo_timeline::store::{Cursor, FetchOptions};
use paseo_timeline::{CollapseKind, Direction, SeqRange};

const ID: &str = AGENT_IDS[0];

fn assistant_with_id(text: &str, message_id: &str) -> AgentTimelineItem {
    AgentTimelineItem::AssistantMessage {
        text: text.into(),
        message_id: Some(message_id.into()),
    }
}

async fn setup(item_count: usize) -> Harness {
    let h = Harness::new().await;
    h.agent(ID, &MockSession::new("codex")).await;
    let names = ["first", "second", "third", "fourth", "fifth"];
    for name in names.iter().take(item_count) {
        h.manager
            .append_timeline_item(ID, assistant_with_id(name, name))
            .await
            .unwrap();
    }
    h
}

fn fetch(direction: Direction, cursor: Option<(&str, i64)>, limit: usize) -> FetchOptions {
    FetchOptions {
        direction: Some(direction),
        cursor: cursor.map(|(epoch, seq)| Cursor {
            epoch: epoch.into(),
            seq,
        }),
        limit: Some(limit),
    }
}

#[tokio::test(start_paused = true)]
async fn fetch_timeline_returns_a_bounded_reset_window_when_cursor_epoch_is_stale() {
    let h = setup(3).await;
    let baseline = h
        .manager
        .fetch_timeline(ID, fetch(Direction::Tail, None, 2))
        .await
        .unwrap();
    assert_eq!(baseline.rows.len(), 2);

    let result = h
        .manager
        .fetch_timeline(
            ID,
            fetch(
                Direction::After,
                Some(("stale-epoch", baseline.rows.last().unwrap().seq())),
                1,
            ),
        )
        .await
        .unwrap();
    assert!(result.reset);
    assert!(result.stale_cursor);
    assert!(!result.gap);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0].seq(), 3);
    assert!(result.has_older);

    let older = h
        .manager
        .fetch_timeline(
            ID,
            fetch(
                Direction::Before,
                Some((&result.epoch, result.rows[0].seq())),
                1,
            ),
        )
        .await
        .unwrap();
    assert!(!older.reset);
    assert_eq!(older.rows.len(), 1);
    assert_eq!(older.rows[0].seq(), 2);
    assert!(older.has_older);
}

#[tokio::test(start_paused = true)]
async fn get_timeline_rows_falls_back_to_the_in_memory_timeline_when_no_durable_store_is_configured(
) {
    let h = Harness::new().await;
    h.agent(ID, &MockSession::new("codex")).await;
    h.manager
        .append_timeline_item(ID, assistant_item("row one"))
        .await
        .unwrap();
    h.manager
        .append_timeline_item(ID, assistant_item("row two"))
        .await
        .unwrap();

    let rows = h.rows(ID).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!((row.seq(), row.seq_start, row.seq_end), (2, 1, 2));
    assert_eq!(
        row.source_seq_ranges,
        vec![SeqRange {
            start_seq: 1,
            end_seq: 2
        }]
    );
    assert_eq!(row.collapsed, vec![CollapseKind::AssistantMerge]);
    assert_eq!(row.item, assistant_item("row onerow two"));
    assert!(!row.timestamp.is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_manager_keeps_committed_history_behind_its_own_api() {
    let h = Harness::new().await;
    h.agent(ID, &MockSession::new("codex")).await;
    let user = AgentTimelineItem::UserMessage {
        text: "hello boundary".into(),
        message_id: Some("msg-boundary-1".into()),
        client_message_id: None,
    };
    h.manager
        .append_timeline_item(ID, user.clone())
        .await
        .unwrap();
    h.manager
        .append_timeline_item(ID, assistant_item("history stays behind manager"))
        .await
        .unwrap();

    assert_eq!(
        h.manager.get_timeline(ID).await.unwrap(),
        vec![user, assistant_item("history stays behind manager")]
    );
    let fetched = h
        .manager
        .fetch_timeline(ID, fetch(Direction::Tail, None, 0))
        .await
        .unwrap();
    assert_eq!(
        fetched.rows.iter().map(|r| r.seq()).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[tokio::test(start_paused = true)]
async fn streams_coalesced_assistant_chunks_and_retains_the_projected_message() {
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_start(|session, prompt, options| {
            boxed(async move {
                let n = session.record_start(prompt, options);
                let turn = format!("turn-{n}");
                let task = session.clone();
                let task_turn = turn.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::ZERO).await;
                    task.push_event(turn_started(&task_turn));
                    task.push_event(assistant("final ", "codex", Some(&task_turn)));
                    task.push_event(assistant("reply", "codex", Some(&task_turn)));
                    task.push_event(turn_completed(&task_turn));
                });
                Ok(TurnHandle { turn_id: turn })
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    h.events.clear();

    let mut stream = h
        .manager
        .stream_agent(ID, PromptInput::from("hello"), RunOpts::default())
        .await
        .unwrap();
    while stream.next().await.unwrap().is_some() {}
    settle().await;

    // The leading edge ships "final " alone; "reply" follows on the trailing window. History
    // retains their complete projected message.
    let streamed: Vec<(i64, String, String)> = h
        .events
        .streams(ID)
        .iter()
        .filter_map(|e| match &**e {
            paseo_core::ManagerEvent::AgentStream {
                event: AgentStreamEvent::Timeline { item, .. },
                seq,
                epoch,
                ..
            } => match &**item {
                AgentTimelineItem::AssistantMessage { text, .. } => {
                    Some((seq.unwrap(), text.clone(), epoch.clone().unwrap()))
                }
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(streamed.len(), 2);
    assert_eq!((streamed[0].0, streamed[0].1.as_str()), (1, "final "));
    assert_eq!((streamed[1].0, streamed[1].1.as_str()), (2, "reply"));
    assert_eq!(
        h.manager.get_timeline(ID).await.unwrap(),
        vec![assistant_item("final reply")]
    );
    let fetched = h
        .manager
        .fetch_timeline(ID, fetch(Direction::Tail, None, 0))
        .await
        .unwrap();
    assert_eq!(fetched.rows.len(), 1);
    assert_eq!((fetched.rows[0].seq_start, fetched.rows[0].seq_end), (1, 2));
    assert_eq!(streamed[0].2, fetched.epoch);
}

#[tokio::test(start_paused = true)]
async fn fetch_timeline_supports_older_history_pagination_with_before_seq() {
    let h = setup(5).await;
    let epoch = h
        .manager
        .fetch_timeline(ID, FetchOptions::default())
        .await
        .unwrap()
        .epoch;
    let result = h
        .manager
        .fetch_timeline(ID, fetch(Direction::Before, Some((&epoch, 5)), 2))
        .await
        .unwrap();
    assert_eq!(
        result.rows.iter().map(|r| r.seq()).collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert!(result.has_older);
    assert!(result.has_newer);
}

#[tokio::test(start_paused = true)]
async fn committed_history_is_not_trimmed() {
    let h = setup(3).await;
    let fetched = h
        .manager
        .fetch_timeline(ID, fetch(Direction::Tail, None, 0))
        .await
        .unwrap();
    assert_eq!(fetched.rows.len(), 3);
    assert_eq!((fetched.window.min_seq, fetched.window.max_seq), (1, 3));
}

#[tokio::test(start_paused = true)]
async fn provider_user_message_is_recorded_from_the_live_stream() {
    // Pushed before `start_turn` returns (staged, then replayed after the accept block): a
    // provider `user_message` without client identity has no canonical row to dedupe against.
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_start(|session, prompt, options| {
            boxed(async move {
                session.record_start(prompt, options);
                let turn = "turn-unexpected-1";
                session.push_event(turn_started(turn));
                session.push_event(timeline_event(
                    AgentTimelineItem::UserMessage {
                        text: "continuation prompt".into(),
                        message_id: None,
                        client_message_id: None,
                    },
                    "codex",
                    Some(turn),
                ));
                session.push_event(assistant("continuation reply", "codex", Some(turn)));
                session.push_event(turn_completed(turn));
                Ok(TurnHandle {
                    turn_id: turn.into(),
                })
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;

    h.manager
        .run_agent(ID, PromptInput::from("do something"), RunOpts::default())
        .await
        .unwrap();

    assert_eq!(
        user_message_texts(&h.items(ID).await),
        vec!["continuation prompt".to_owned()]
    );
}

#[tokio::test(start_paused = true)]
async fn canonical_submitted_prompt_keeps_wire_identity_when_the_provider_echo_arrives_late() {
    // The rewind half of the TS case belongs to the rewind step.
    let allow_echo = Gate::new();
    let gate = allow_echo.clone();
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_start(move |session, prompt, options| {
            let gate = gate.clone();
            boxed(async move {
                let text = match &prompt {
                    PromptInput::Text(t) => t.clone(),
                    PromptInput::Blocks(_) => String::new(),
                };
                let client_message_id = options
                    .as_ref()
                    .and_then(|o| o.get("clientMessageId"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                session.record_start(prompt, options);
                let turn = "turn-submitted-user-message";
                let task = session.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::ZERO).await;
                    task.push_event(turn_started(turn));
                    task.push_event(assistant(
                        "output before provider echo",
                        "codex",
                        Some(turn),
                    ));
                    gate.wait().await;
                    task.push_event(timeline_event(
                        AgentTimelineItem::UserMessage {
                            text,
                            message_id: Some("provider-message-1".into()),
                            client_message_id,
                        },
                        "codex",
                        Some(turn),
                    ));
                    task.push_event(turn_completed(turn));
                });
                Ok(TurnHandle {
                    turn_id: turn.into(),
                })
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;
    h.events.clear();

    let manager = h.manager.clone();
    let run = tokio::spawn(async move {
        manager
            .run_agent(
                ID,
                PromptInput::from("hello from composer"),
                RunOpts::with_client_message_id("msg-client-1"),
            )
            .await
    });
    // TS admits the run synchronously inside `runAgent`; let the spawned task do the same.
    settle().await;
    h.manager.wait_for_agent_run_start(ID, None).await.unwrap();
    allow_echo.open();
    let result = run.await.unwrap().unwrap();
    assert!(!result.canceled);
    settle().await;

    let kinds: Vec<String> = h
        .events
        .streams(ID)
        .iter()
        .zip(h.events.streams(ID).iter())
        .map(|(e, _)| match &**e {
            paseo_core::ManagerEvent::AgentStream {
                event: AgentStreamEvent::Timeline { item, .. },
                seq,
                ..
            } => {
                format!("{}@{}", item.type_name(), seq.unwrap())
            }
            paseo_core::ManagerEvent::AgentStream { event, .. } => event.type_name().to_owned(),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "turn_started",
            "user_message@1",
            "assistant_message@2",
            "turn_completed"
        ]
    );

    let rows = h
        .manager
        .fetch_timeline(ID, fetch(Direction::Tail, None, 20))
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
    assert!(matches!(
        &rows[0].item,
        AgentTimelineItem::UserMessage { text, message_id: Some(m), client_message_id: Some(c) }
            if text == "hello from composer" && m == "msg-client-1" && c == "msg-client-1"
    ));
    assert_eq!(rows[1].item, assistant_item("output before provider echo"));
    assert_eq!(
        rows[1].turn_id.as_deref(),
        Some("turn-submitted-user-message")
    );
}

#[tokio::test(start_paused = true)]
async fn user_messages_wrapping_a_paseo_system_envelope_are_not_added_to_the_timeline() {
    let session = MockSession::with_behavior(
        "codex",
        ScriptedBehavior::new().on_start(|session, prompt, options| {
            boxed(async move {
                session.record_start(prompt, options);
                let turn = "turn-fake-codex";
                let task = session.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::ZERO).await;
                    task.push_event(turn_started(turn));
                    for text in [
                        paseo_timeline::agent_message::format_system_notification_prompt(
                            "child finished",
                        ),
                        "plain user message".to_owned(),
                    ] {
                        task.push_event(timeline_event(
                            AgentTimelineItem::UserMessage {
                                text,
                                message_id: None,
                                client_message_id: None,
                            },
                            "codex",
                            Some(turn),
                        ));
                    }
                    task.push_event(turn_completed(turn));
                });
                Ok(TurnHandle {
                    turn_id: turn.into(),
                })
            })
        }),
    );
    let h = Harness::new().await;
    h.agent(ID, &session).await;

    h.manager
        .run_agent(ID, PromptInput::from("do something"), RunOpts::default())
        .await
        .unwrap();

    assert_eq!(
        user_message_texts(&h.items(ID).await),
        vec!["plain user message".to_owned()]
    );
}
