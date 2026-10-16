//! Gap fillers of the WP8 acceptance cross-check: invariants of the spec that no ported TS case
//! pins down on its own. `docs/p4-core-spec.md` section 16 maps every invariant to its tests.

mod common;

use common::*;
use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem, PersistenceHandle};
use paseo_core::mock::MockSession;
use paseo_core::{CreateAgentOptions, Lifecycle, RunOpts};
use paseo_provider_api::PromptInput;
use serde_json::json;

const ID: &str = AGENT_IDS[0];

/// Every `[System Error] ...` row. Projection merges adjacent assistant rows, so split the
/// merged text back apart.
fn system_errors(items: &[AgentTimelineItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match item {
            AgentTimelineItem::AssistantMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .flat_map(|text| text.split("[System Error]").skip(1))
        .map(|rest| format!("[System Error]{rest}"))
        .collect()
}

// Invariant 6: the `[System Error]` row is skipped when it equals the last row.
#[tokio::test(start_paused = true)]
async fn an_identical_system_error_is_not_appended_twice_in_a_row() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;

    session.push_event(turn_failed(Some("t1"), "same failure"));
    settle().await;
    session.push_event(turn_failed(Some("t2"), "same failure"));
    settle().await;
    assert_eq!(system_errors(&h.items(ID).await).len(), 1);

    session.push_event(turn_failed(Some("t3"), "another failure"));
    settle().await;
    session.push_event(turn_failed(Some("t4"), "same failure"));
    settle().await;
    assert_eq!(
        system_errors(&h.items(ID).await).len(),
        3,
        "only a repeat of the LAST row is dropped"
    );
}

// Invariant 6: an empty failure message falls back to `Provider run failed`.
#[tokio::test(start_paused = true)]
async fn an_empty_failure_message_reads_provider_run_failed() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    session.push_event(turn_failed(Some("t1"), "  "));
    settle().await;
    assert_eq!(
        system_errors(&h.items(ID).await),
        ["[System Error] Provider run failed"]
    );
}

// Invariant 5: a non-foreground failure puts the agent in `error`, the next completion clears
// `lastError` and merges usage without clearing it.
#[tokio::test(start_paused = true)]
async fn an_autonomous_failure_sets_error_and_a_later_completion_clears_it() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;

    session.push_event(turn_started("auto-1"));
    settle().await;
    assert_eq!(h.view(ID).lifecycle, Lifecycle::Running);
    session.push_event(turn_failed(Some("auto-1"), "autonomous boom"));
    settle().await;
    let view = h.view(ID);
    assert_eq!(view.lifecycle, Lifecycle::Error);
    assert_eq!(view.last_error.as_deref(), Some("autonomous boom"));

    session.push_event(turn_started("auto-2"));
    settle().await;
    session.push_event(turn_completed("auto-2"));
    settle().await;
    let view = h.view(ID);
    assert_eq!(view.lifecycle, Lifecycle::Idle);
    assert_eq!(view.last_error, None);
}

// Invariant 19: `hasInFlightRun` is lifecycle running, a foreground turn or any run slot.
#[tokio::test(start_paused = true)]
async fn has_in_flight_run_follows_lifecycle_slot_and_foreground_turn() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    h.agent(ID, &session).await;
    assert!(!h.manager.has_in_flight_run(ID));

    // Lifecycle `running` from an autonomous turn.
    session.push_event(turn_started("auto-1"));
    settle().await;
    assert!(h.manager.has_in_flight_run(ID));
    session.push_event(turn_completed("auto-1"));
    settle().await;
    assert!(!h.manager.has_in_flight_run(ID));

    // A run slot occupied by an admitted but unpolled stream.
    let stream = h
        .manager
        .stream_agent(ID, PromptInput::from("hi"), RunOpts::default())
        .await
        .unwrap();
    assert!(h.manager.has_in_flight_run(ID));
    assert!(h.view(ID).has_run);
    drop(stream);
    settle().await;
    assert!(!h.manager.has_in_flight_run(ID));
}

// Invariant 12 / PARITY-QUIRK(pre-subscribe-events-lost).
#[tokio::test(start_paused = true)]
async fn events_a_session_emits_before_registration_are_lost() {
    let h = Harness::new().await;
    let session = MockSession::new("codex");
    session.push_event(assistant("too early", "codex", None));
    h.agent(ID, &session).await;
    settle().await;
    assert!(h.items(ID).await.is_empty());
    session.push_event(assistant("on time", "codex", None));
    settle().await;
    assert_eq!(h.items(ID).await.len(), 1);
}

// Invariant 4: the persistence handle is refreshed from `describe_persistence` with the cwd.
#[tokio::test(start_paused = true)]
async fn finalizing_a_turn_refreshes_persistence_and_attaches_the_cwd() {
    let h = Harness::new().await;
    let session = MockSession::builder("codex").auto_completing().build();
    h.agent(ID, &session).await;
    session.set_persistence(PersistenceHandle {
        provider: "codex".into(),
        session_id: "fresh-session".into(),
        native_handle: None,
        metadata: None,
    });
    h.manager
        .run_agent(ID, PromptInput::from("go"), RunOpts::default())
        .await
        .unwrap();
    let persistence = h.view(ID).persistence.clone().expect("persistence");
    assert_eq!(persistence.session_id, "fresh-session");
    assert_eq!(
        persistence.metadata.as_ref().and_then(|m| m.get("cwd")),
        Some(&json!("/tmp/paseo-core-test"))
    );
}

// Invariant 15 / PARITY-QUIRK(cascade-abort-on-first-failure): the first failing child aborts
// the cascade and later children stay unarchived.
#[tokio::test]
async fn the_cascade_stops_at_the_first_failing_child() {
    let lc = Lc::new().await;
    let parent = lc.create().await;
    let mut ids = Vec::new();
    for title in ["Child one", "Child two"] {
        let mut config = lc.config();
        config.title = Some(title.into());
        let options = CreateAgentOptions {
            labels: Some(
                [("paseo.parent-agent-id".to_owned(), parent.id.clone())]
                    .into_iter()
                    .collect(),
            ),
            ..CreateAgentOptions::default()
        };
        ids.push(lc.create_with(config, options).await.id.clone());
    }
    lc.flush().await;
    // The cascade visits stored records in `list` order: break the first child's file.
    let order: Vec<String> = lc
        .store
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|record| record.id)
        .filter(|id| ids.contains(id))
        .collect();
    let (first, second) = (&order[0], &order[1]);
    let path = lc
        .dir
        .path()
        .join("agents")
        .join(paseo_store::project_dir_name_from_cwd(&lc.cwd()))
        .join(format!("{first}.json"));
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();

    assert!(lc.manager.archive_agent(&parent.id).await.is_err());
    let second_record = lc.record(second).await.expect("record");
    assert!(
        !second_record.is_archived(),
        "the child after the failure was skipped"
    );
}

#[allow(dead_code)]
fn _unused(_: AgentStreamEvent) {}
