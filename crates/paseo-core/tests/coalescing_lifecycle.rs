//! The lifecycle halves of `agent-manager-stream-coalescing.test.ts`: history hydration limits
//! and projection, and the close / reload / id-reuse timer-safety cases. Real time (the
//! lifecycle calls touch the file system), so only loose waits are asserted.

mod common;

use std::time::Duration;

use common::*;
use paseo_agent_types::{AgentTimelineItem, ToolCallStatus};
use paseo_core::mock::MockClient;
use paseo_core::{CreateAgentOptions, HydrateOpts, ReloadOptions};

const COALESCE_WINDOW: Duration = Duration::from_millis(60);

async fn lc() -> Lc {
    Lc::with(
        MockClient::new("codex"),
        LcOptions {
            store: false,
            ..LcOptions::default()
        },
    )
    .await
}

fn force() -> HydrateOpts {
    HydrateOpts {
        force: true,
        ..HydrateOpts::default()
    }
}

fn assistant_texts(events: &[paseo_core::SharedEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match stream_event(e) {
            paseo_agent_types::AgentStreamEvent::Timeline { item, .. } => match &**item {
                AgentTimelineItem::AssistantMessage { text, .. } => Some(text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn bounds_tool_output_while_hydrating_provider_history() {
    let lc = lc().await;
    let agent = lc.create().await;
    let output = format!("{}{}", "a".repeat(512 * 1024), "z".repeat(512 * 1024));
    lc.client.created_sessions()[0].set_history(vec![timeline_event(
        tool_call("tool-1", ToolCallStatus::Completed, &output, None),
        "codex",
        None,
    )]);

    lc.manager
        .hydrate_timeline_from_provider(&agent.id, force())
        .await
        .unwrap();

    assert_eq!(
        lc.items(&agent.id).await,
        vec![tool_call(
            "tool-1",
            ToolCallStatus::Completed,
            &"a".repeat(64 * 1024),
            None
        )]
    );
}

#[tokio::test]
async fn history_hydration_projects_immediately_without_waiting_for_a_streaming_window() {
    let lc = lc().await;
    let agent = lc.create().await;
    lc.client.created_sessions()[0].set_history(vec![
        assistant("a1", "codex", None),
        assistant("a2", "codex", None),
        reasoning("r1", "codex", None),
        reasoning("r2", "codex", None),
    ]);

    lc.manager
        .hydrate_timeline_from_provider(&agent.id, force())
        .await
        .unwrap();

    let rows = lc.rows(&agent.id).await;
    assert_eq!(
        rows.iter().map(|r| r.item.clone()).collect::<Vec<_>>(),
        vec![
            assistant_item("a1a2"),
            AgentTimelineItem::Reasoning {
                text: "r1r2".into()
            }
        ]
    );
    assert_eq!(
        rows.iter()
            .flat_map(|r| r.source_seq_ranges.iter().map(|s| (s.start_seq, s.end_seq)))
            .collect::<Vec<_>>(),
        vec![(1, 2), (3, 4)]
    );
    lc.sync().await;
    assert!(lc.events.timeline_streams(&agent.id).is_empty());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(lc.rows(&agent.id).await.len(), 2);
}

#[tokio::test]
async fn lifecycle_paths_are_timer_safe() {
    // close
    let lc_close = lc().await;
    let agent = lc_close.create().await;
    lc_close.client.created_sessions()[0].push_event(assistant("close", "codex", None));
    settle().await;
    lc_close.manager.close_agent(&agent.id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    lc_close.sync().await;
    let streams = lc_close.events.timeline_streams(&agent.id);
    assert_eq!(streams.len(), 1);
    assert_eq!(stream_seq(&streams[0]), Some(1));
    assert_eq!(assistant_texts(&streams), ["close"]);

    // reload
    let lc_reload = lc().await;
    let agent = lc_reload.create().await;
    lc_reload.client.created_sessions()[0].push_event(assistant("reload", "codex", None));
    settle().await;
    lc_reload
        .manager
        .reload_agent_session(&agent.id, None, ReloadOptions::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    lc_reload.sync().await;
    assert_eq!(
        lc_reload.items(&agent.id).await,
        vec![assistant_item("reload")]
    );
    assert_eq!(lc_reload.events.timeline_streams(&agent.id).len(), 1);

    // deletion (close, then the agent is gone)
    let lc_delete = lc().await;
    let agent = lc_delete.create().await;
    lc_delete.client.created_sessions()[0].push_event(assistant("delete", "codex", None));
    settle().await;
    lc_delete.manager.close_agent(&agent.id).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    lc_delete.sync().await;
    assert!(lc_delete.manager.get_agent(&agent.id).is_none());
    assert_eq!(lc_delete.events.timeline_streams(&agent.id).len(), 1);
}

#[tokio::test]
async fn old_timers_cannot_write_into_reused_agent_ids() {
    let reload = lc().await;
    let agent = reload.create().await;
    reload.client.created_sessions()[0].push_event(assistant("before-reload", "codex", None));
    settle().await;
    reload
        .manager
        .reload_agent_session(&agent.id, None, ReloadOptions::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    reload.sync().await;
    assert_eq!(
        reload.items(&agent.id).await,
        vec![assistant_item("before-reload")]
    );
    assert_eq!(reload.events.timeline_streams(&agent.id).len(), 1);

    let reuse = lc().await;
    let id = AGENT_IDS[0];
    let first = reuse
        .manager
        .create_agent(reuse.config(), Some(id), CreateAgentOptions::default())
        .await
        .unwrap();
    reuse.client.created_sessions()[0].push_event(assistant("old", "codex", None));
    settle().await;
    reuse.manager.close_agent(&first.id).await.unwrap();

    let next_dir = tempfile::tempdir().unwrap();
    let mut config = reuse.config();
    config.cwd = next_dir.path().to_string_lossy().into_owned();
    let second = reuse
        .manager
        .create_agent(config, Some(id), CreateAgentOptions::default())
        .await
        .unwrap();
    reuse.client.created_sessions()[1].push_event(assistant("new", "codex", None));
    settle().await;
    tokio::time::sleep(COALESCE_WINDOW + Duration::from_millis(40)).await;
    reuse.sync().await;

    assert_eq!(reuse.items(&second.id).await, vec![assistant_item("new")]);
    let streams = reuse.events.timeline_streams(&first.id);
    assert_eq!(assistant_texts(&streams), ["old", "new"]);
}
