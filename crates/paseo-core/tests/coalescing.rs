//! Port of `agent-manager-stream-coalescing.test.ts` (limits, window/ordering, boundaries,
//! lifecycle and timer safety, foreground yield, seq monotonicity). The hydration cases
//! (`bounds tool output while hydrating`, `history hydration projects immediately`) and the
//! reload halves of the timer-safety cases belong to the hydrate/reload step.

mod common;

use common::*;
use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem, ToolCallStatus};
use paseo_core::mock::MockSession;
use paseo_core::{ManagerEvent, RunOpts};
use paseo_provider_api::PromptInput;
use paseo_timeline::ProjectedEntry;
use serde_json::json;

const WINDOW: u64 = 60;
const BEFORE_WINDOW: u64 = WINDOW - 1;
const LIMIT: usize = 64 * 1024;

async fn managed(h: &Harness, id: &str) -> MockSession {
    let session = MockSession::new("codex");
    h.agent(id, &session).await;
    session
}

fn items(rows: &[ProjectedEntry]) -> Vec<AgentTimelineItem> {
    rows.iter().map(|r| r.item.clone()).collect()
}

fn covered_seqs(rows: &[ProjectedEntry]) -> Vec<i64> {
    let mut seqs: Vec<i64> = rows
        .iter()
        .flat_map(|r| {
            r.source_seq_ranges
                .iter()
                .flat_map(|range| range.start_seq..=range.end_seq)
        })
        .collect();
    seqs.sort_unstable();
    seqs.dedup();
    seqs
}

fn live_seqs(events: &[paseo_core::SharedEvent]) -> Vec<Option<i64>> {
    events.iter().map(stream_seq).collect()
}

fn some_seqs(range: std::ops::RangeInclusive<i64>) -> Vec<Option<i64>> {
    range.map(Some).collect()
}

fn text_event(e: &paseo_core::SharedEvent) -> AgentStreamEvent {
    stream_event(e).clone()
}

fn big_output() -> String {
    format!("{}{}", "a".repeat(512 * 1024), "z".repeat(512 * 1024))
}

fn limited(status: ToolCallStatus) -> AgentTimelineItem {
    tool_call("tool-1", status, &"a".repeat(LIMIT), None)
}

#[tokio::test(start_paused = true)]
async fn bounds_tool_output_before_persisting_and_streaming_it() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;
    let expected = limited(ToolCallStatus::Completed);

    session.push_event(timeline_event(
        tool_call("tool-1", ToolCallStatus::Completed, &big_output(), None),
        "codex",
        None,
    ));
    settle().await;

    assert_eq!(items(&h.rows(AGENT_IDS[0]).await), vec![expected.clone()]);
    let streamed: Vec<_> = h
        .events
        .timeline_streams(AGENT_IDS[0])
        .iter()
        .map(text_event)
        .collect();
    assert_eq!(streamed.len(), 1);
    match &streamed[0] {
        AgentStreamEvent::Timeline { item, .. } => assert_eq!(**item, expected),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn bounds_appended_tool_output_before_persisting_and_streaming_it() {
    let h = Harness::new().await;
    managed(&h, AGENT_IDS[0]).await;
    let expected = limited(ToolCallStatus::Completed);

    h.manager
        .append_timeline_item(
            AGENT_IDS[0],
            tool_call("tool-1", ToolCallStatus::Completed, &big_output(), None),
        )
        .await
        .unwrap();
    settle().await;

    assert_eq!(items(&h.rows(AGENT_IDS[0]).await), vec![expected.clone()]);
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(events.len(), 1);
    assert!(matches!(
        stream_event(&events[0]),
        AgentStreamEvent::Timeline { item, .. } if **item == expected
    ));
}

#[tokio::test(start_paused = true)]
async fn bounds_tool_output_emitted_only_to_the_live_stream() {
    let h = Harness::new().await;
    managed(&h, AGENT_IDS[0]).await;
    let expected = limited(ToolCallStatus::Completed);

    h.manager
        .emit_live_timeline_item(
            AGENT_IDS[0],
            tool_call("tool-1", ToolCallStatus::Completed, &big_output(), None),
        )
        .await
        .unwrap();
    settle().await;

    assert!(
        h.rows(AGENT_IDS[0]).await.is_empty(),
        "live-only items are not recorded"
    );
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(events.len(), 1);
    assert!(matches!(
        stream_event(&events[0]),
        AgentStreamEvent::Timeline { item, .. } if **item == expected
    ));
    assert_eq!(stream_seq(&events[0]), None);
}

#[tokio::test(start_paused = true)]
async fn bounds_failed_shell_output_carried_in_the_error() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;
    let content = big_output();
    let expected = tool_call(
        "tool-1",
        ToolCallStatus::Failed,
        "",
        Some(json!({"content": "a".repeat(LIMIT)})),
    );

    session.push_event(timeline_event(
        tool_call(
            "tool-1",
            ToolCallStatus::Failed,
            "",
            Some(json!({"content": content})),
        ),
        "codex",
        None,
    ));
    settle().await;

    assert_eq!(items(&h.rows(AGENT_IDS[0]).await), vec![expected]);
}

#[tokio::test(start_paused = true)]
async fn coalesces_a_same_tick_assistant_burst_after_the_window() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;

    for _ in 0..1000 {
        session.push_event(assistant("x", "codex", None));
    }
    settle().await;

    // Leading edge: the first chunk is out, the other 999 are buffered.
    advance(BEFORE_WINDOW).await;
    assert_eq!(
        items(&h.rows(AGENT_IDS[0]).await),
        vec![assistant_item("x")]
    );
    assert_eq!(h.events.timeline_streams(AGENT_IDS[0]).len(), 1);

    advance(1).await;
    let rows = h.rows(AGENT_IDS[0]).await;
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(rows.len(), 1);
    assert_eq!(events.len(), 2);
    assert_eq!(items(&rows), vec![assistant_item(&"x".repeat(1000))]);
    assert_eq!(text_event(&events[0]), assistant("x", "codex", None));
    assert_eq!(
        text_event(&events[1]),
        assistant(&"x".repeat(999), "codex", None)
    );
    assert_eq!(covered_seqs(&rows), vec![1, 2]);
    assert_eq!(live_seqs(&events), some_seqs(1..=2));
}

#[tokio::test(start_paused = true)]
async fn coalesces_same_tick_reasoning_chunks_after_the_window() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;

    for _ in 0..100 {
        session.push_event(reasoning("r", "codex", None));
    }
    settle().await;
    advance(WINDOW).await;

    let rows = h.rows(AGENT_IDS[0]).await;
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(
        items(&rows),
        vec![AgentTimelineItem::Reasoning {
            text: "r".repeat(100)
        }]
    );
    assert_eq!(events.len(), 2);
    assert_eq!(covered_seqs(&rows), vec![1, 2]);
    assert_eq!(live_seqs(&events), some_seqs(1..=2));
}

#[tokio::test(start_paused = true)]
async fn coalesces_running_tool_calls_without_flushing_buffered_text() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;
    let running = running_tool("running");

    for _ in 0..500 {
        session.push_event(assistant("a", "codex", None));
    }
    session.push_event(timeline_event(running.clone(), "codex", None));
    settle().await;

    // Only the leading chunk is out; the running tool call stays buffered with the text.
    assert_eq!(
        items(&h.rows(AGENT_IDS[0]).await),
        vec![assistant_item("a")]
    );
    assert_eq!(h.events.timeline_streams(AGENT_IDS[0]).len(), 1);

    advance(WINDOW).await;
    assert_eq!(
        items(&h.rows(AGENT_IDS[0]).await),
        vec![assistant_item(&"a".repeat(500)), running.clone()]
    );
    assert_eq!(
        live_seqs(&h.events.timeline_streams(AGENT_IDS[0])),
        some_seqs(1..=3)
    );

    for _ in 0..500 {
        session.push_event(assistant("b", "codex", None));
    }
    settle().await;
    // The "b" burst lands inside the window that just flushed: no leading flush, it coalesces whole.
    advance(BEFORE_WINDOW).await;
    assert_eq!(h.rows(AGENT_IDS[0]).await.len(), 2);

    advance(1).await;
    let rows = h.rows(AGENT_IDS[0]).await;
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(
        items(&rows),
        vec![
            assistant_item(&"a".repeat(500)),
            running,
            assistant_item(&"b".repeat(500)),
        ]
    );
    assert_eq!(covered_seqs(&rows), vec![1, 2, 3, 4]);
    assert_eq!(live_seqs(&events), some_seqs(1..=4));
}

#[tokio::test(start_paused = true)]
async fn coalesces_same_window_tool_call_updates_to_the_latest_snapshot() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;

    for i in 0..200 {
        session.push_event(timeline_event(
            running_tool(&format!("chunk-{i}")),
            "codex",
            None,
        ));
    }
    settle().await;

    advance(BEFORE_WINDOW).await;
    assert_eq!(
        items(&h.rows(AGENT_IDS[0]).await),
        vec![running_tool("chunk-0")]
    );
    assert_eq!(h.events.timeline_streams(AGENT_IDS[0]).len(), 1);

    advance(1).await;
    let rows = h.rows(AGENT_IDS[0]).await;
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(items(&rows), vec![running_tool("chunk-199")]);
    assert_eq!(events.len(), 2);
    assert_eq!(covered_seqs(&rows), vec![1, 2]);
    assert_eq!(live_seqs(&events), some_seqs(1..=2));
}

#[tokio::test(start_paused = true)]
async fn coalesces_interleaved_tool_calls_independently_and_preserves_first_arrival_order() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;
    let call = |id: &str, output: &str| {
        timeline_event(
            tool_call(id, ToolCallStatus::Running, output, None),
            "codex",
            None,
        )
    };

    session.push_event(call("tool-1", "one-a"));
    session.push_event(call("tool-2", "two-a"));
    session.push_event(call("tool-1", "one-b"));
    session.push_event(call("tool-2", "two-b"));
    settle().await;
    advance(WINDOW).await;

    let rows = h.rows(AGENT_IDS[0]).await;
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(
        items(&rows),
        vec![
            tool_call("tool-1", ToolCallStatus::Running, "one-b", None),
            tool_call("tool-2", ToolCallStatus::Running, "two-b", None),
        ]
    );
    assert_eq!(events.len(), 3);
    assert_eq!(covered_seqs(&rows), vec![1, 2, 3]);
    assert_eq!(live_seqs(&events), some_seqs(1..=3));
}

#[tokio::test(start_paused = true)]
async fn terminal_tool_call_statuses_flush_immediately_with_pending_text() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;

    session.push_event(assistant("before", "codex", None));
    session.push_event(timeline_event(running_tool("running"), "codex", None));
    settle().await;
    assert_eq!(
        items(&h.rows(AGENT_IDS[0]).await),
        vec![assistant_item("before")]
    );

    session.push_event(timeline_event(
        tool_call("tool-1", ToolCallStatus::Completed, "done", None),
        "codex",
        None,
    ));
    settle().await;

    let rows = h.rows(AGENT_IDS[0]).await;
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(
        items(&rows),
        vec![
            assistant_item("before"),
            tool_call("tool-1", ToolCallStatus::Completed, "done", None),
        ]
    );
    assert_eq!(events.len(), 2);
    assert_eq!(covered_seqs(&rows), vec![1, 2]);
    assert_eq!(live_seqs(&events), some_seqs(1..=2));

    advance(WINDOW).await;
    assert_eq!(h.rows(AGENT_IDS[0]).await.len(), 2);
    assert_eq!(h.events.timeline_streams(AGENT_IDS[0]).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn preserves_mixed_text_and_tool_call_order_after_coalescing() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;

    session.push_event(assistant("a", "codex", None));
    session.push_event(timeline_event(running_tool("first"), "codex", None));
    session.push_event(reasoning("r", "codex", None));
    session.push_event(timeline_event(running_tool("latest"), "codex", None));
    session.push_event(assistant("b", "codex", None));
    settle().await;
    advance(WINDOW).await;

    let rows = h.rows(AGENT_IDS[0]).await;
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(
        items(&rows),
        vec![
            assistant_item("a"),
            running_tool("latest"),
            AgentTimelineItem::Reasoning { text: "r".into() },
            assistant_item("b"),
        ]
    );
    assert_eq!(events.len(), 4);
    assert_eq!(covered_seqs(&rows), vec![1, 2, 3, 4]);
    assert_eq!(live_seqs(&events), some_seqs(1..=4));
}

#[tokio::test(start_paused = true)]
async fn preserves_assistant_and_reasoning_interleave() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;

    session.push_event(assistant("a1", "codex", None));
    session.push_event(reasoning("r1", "codex", None));
    session.push_event(reasoning("r2", "codex", None));
    session.push_event(assistant("a2", "codex", None));
    settle().await;
    advance(WINDOW).await;

    let rows = h.rows(AGENT_IDS[0]).await;
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(
        items(&rows),
        vec![
            assistant_item("a1"),
            AgentTimelineItem::Reasoning {
                text: "r1r2".into()
            },
            assistant_item("a2"),
        ]
    );
    assert_eq!(events.len(), 3);
    assert_eq!(covered_seqs(&rows), vec![1, 2, 3]);
    assert_eq!(live_seqs(&events), some_seqs(1..=3));
}

#[tokio::test(start_paused = true)]
async fn preserves_strict_alternating_assistant_reasoning_order() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;

    for i in 1..=5 {
        session.push_event(assistant(&format!("a{i}"), "codex", None));
        session.push_event(reasoning(&format!("r{i}"), "codex", None));
    }
    settle().await;
    advance(WINDOW).await;

    let rows = h.rows(AGENT_IDS[0]).await;
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    let expected: Vec<AgentTimelineItem> = (1..=5)
        .flat_map(|i| {
            [
                assistant_item(&format!("a{i}")),
                AgentTimelineItem::Reasoning {
                    text: format!("r{i}"),
                },
            ]
        })
        .collect();
    assert_eq!(items(&rows), expected);
    assert_eq!(rows.len(), 10);
    assert_eq!(events.len(), 10);
    assert_eq!(covered_seqs(&rows), (1..=10).collect::<Vec<i64>>());
    assert_eq!(live_seqs(&events), some_seqs(1..=10));
}

#[tokio::test(start_paused = true)]
async fn isolates_buffers_per_agent() {
    let h = Harness::new().await;
    let first = managed(&h, AGENT_IDS[0]).await;
    let second = managed(&h, AGENT_IDS[1]).await;

    first.push_event(assistant("a", "codex", None));
    second.push_event(assistant("b", "codex", None));
    first.push_event(assistant("a", "codex", None));
    second.push_event(assistant("b", "codex", None));
    settle().await;
    advance(WINDOW).await;

    let first_rows = h.rows(AGENT_IDS[0]).await;
    let second_rows = h.rows(AGENT_IDS[1]).await;
    assert_eq!(items(&first_rows), vec![assistant_item("aa")]);
    assert_eq!(items(&second_rows), vec![assistant_item("bb")]);
    assert_eq!(h.events.timeline_streams(AGENT_IDS[0]).len(), 2);
    assert_eq!(h.events.timeline_streams(AGENT_IDS[1]).len(), 2);
    assert_eq!(covered_seqs(&first_rows), vec![1, 2]);
    assert_eq!(covered_seqs(&second_rows), vec![1, 2]);
    assert_eq!(
        live_seqs(&h.events.timeline_streams(AGENT_IDS[0])),
        some_seqs(1..=2)
    );
    assert_eq!(
        live_seqs(&h.events.timeline_streams(AGENT_IDS[1])),
        some_seqs(1..=2)
    );
}

#[tokio::test(start_paused = true)]
async fn provider_boundaries_break_collapse() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;

    session.push_event(assistant("codex", "codex", None));
    session.push_event(assistant("claude", "claude", None));
    settle().await;
    advance(WINDOW).await;

    // The projection (provider-agnostic) merges the rows; the live stream keeps both providers.
    let rows = h.rows(AGENT_IDS[0]).await;
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(items(&rows), vec![assistant_item("codexclaude")]);
    assert_eq!(text_event(&events[0]), assistant("codex", "codex", None));
    assert_eq!(text_event(&events[1]), assistant("claude", "claude", None));
    assert_eq!(covered_seqs(&rows), vec![1, 2]);
    assert_eq!(live_seqs(&events), some_seqs(1..=2));
}

#[tokio::test(start_paused = true)]
async fn turn_id_boundaries_break_collapse() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;

    session.push_event(assistant("one", "codex", Some("turn-1")));
    session.push_event(assistant("two", "codex", Some("turn-2")));
    settle().await;
    advance(WINDOW).await;

    let rows = h.rows(AGENT_IDS[0]).await;
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(
        items(&rows),
        vec![assistant_item("one"), assistant_item("two")]
    );
    assert_eq!(
        text_event(&events[0]),
        assistant("one", "codex", Some("turn-1"))
    );
    assert_eq!(
        text_event(&events[1]),
        assistant("two", "codex", Some("turn-2"))
    );
    assert_eq!(covered_seqs(&rows), vec![1, 2]);
    assert_eq!(live_seqs(&events), some_seqs(1..=2));
}

#[tokio::test(start_paused = true)]
async fn drops_empty_text_but_preserves_whitespace_byte_exactly() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;

    for text in ["", " ", "\n\t", "done"] {
        session.push_event(assistant(text, "codex", None));
    }
    settle().await;
    advance(WINDOW).await;

    // The empty chunk is dropped before the coalescer, so " " leads the burst.
    let rows = h.rows(AGENT_IDS[0]).await;
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(items(&rows), vec![assistant_item(" \n\tdone")]);
    assert_eq!(events.len(), 2);
    assert_eq!(covered_seqs(&rows), vec![1, 2]);
    assert_eq!(live_seqs(&events), some_seqs(1..=2));
}

#[tokio::test(start_paused = true)]
async fn flushes_pending_chunks_before_terminal_lifecycle_events_by_dispatch_order() {
    for terminal in ["turn_completed", "turn_failed", "turn_canceled"] {
        let h = Harness::new().await;
        let session = managed(&h, AGENT_IDS[0]).await;
        let terminal_event = match terminal {
            "turn_completed" => turn_completed(terminal),
            "turn_failed" => turn_failed(Some(terminal), "failed"),
            _ => turn_canceled(Some(terminal), "canceled"),
        };

        session.push_event(assistant(
            &format!("{terminal}-text"),
            "codex",
            Some(terminal),
        ));
        session.push_event(terminal_event);
        settle().await;

        let streams = h.events.streams(AGENT_IDS[0]);
        let timelines = h.events.timeline_streams(AGENT_IDS[0]);
        let rows = h.rows(AGENT_IDS[0]).await;

        assert_eq!(items(&rows)[0], assistant_item(&format!("{terminal}-text")));
        // `turn_failed` adds its system error row.
        let expected_timeline_events = if terminal == "turn_failed" { 2 } else { 1 };
        assert_eq!(timelines.len(), expected_timeline_events, "{terminal}");
        assert!(matches!(
            stream_event(&streams[0]),
            AgentStreamEvent::Timeline { item, .. } if **item == assistant_item(&format!("{terminal}-text"))
        ));
        assert_eq!(stream_seq(&streams[0]), Some(1));
        let terminal_index = streams
            .iter()
            .position(|e| stream_event(e).type_name() == terminal)
            .unwrap_or_else(|| panic!("{terminal} was dispatched"));
        assert!(terminal_index > 0);
        assert_eq!(stream_seq(&streams[terminal_index]), None);

        advance(WINDOW).await;
        assert_eq!(h.rows(AGENT_IDS[0]).await.len(), expected_timeline_events);
        assert_eq!(
            h.events.timeline_streams(AGENT_IDS[0]).len(),
            expected_timeline_events
        );
    }
}

#[tokio::test(start_paused = true)]
async fn lifecycle_paths_are_timer_safe_close() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;
    session.push_event(assistant("close", "codex", None));
    settle().await;
    h.manager.close_agent(AGENT_IDS[0]).await.unwrap();
    advance(100).await;

    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(events.len(), 1);
    assert_eq!(text_event(&events[0]), assistant("close", "codex", None));
    assert_eq!(stream_seq(&events[0]), Some(1));
    assert!(h.manager.get_agent(AGENT_IDS[0]).is_none());
}

#[tokio::test(start_paused = true)]
async fn buffered_chunks_are_flushed_when_the_agent_closes() {
    // The `flushAndDiscard` half of the close path: a buffered trailing chunk is recorded and
    // dispatched before the agent leaves, and no timer fires later.
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;
    session.push_event(assistant("a", "codex", None));
    session.push_event(assistant("b", "codex", None));
    settle().await;
    assert_eq!(h.events.timeline_streams(AGENT_IDS[0]).len(), 1);

    h.manager.close_agent(AGENT_IDS[0]).await.unwrap();
    settle().await;
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(events.len(), 2);
    advance(100).await;
    assert_eq!(h.events.timeline_streams(AGENT_IDS[0]).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn old_timers_cannot_write_into_reused_agent_ids() {
    let h = Harness::new().await;
    let first = managed(&h, AGENT_IDS[0]).await;
    first.push_event(assistant("old", "codex", None));
    settle().await;
    h.manager.close_agent(AGENT_IDS[0]).await.unwrap();

    let second = managed(&h, AGENT_IDS[0]).await;
    second.push_event(assistant("new", "codex", None));
    settle().await;
    advance(WINDOW).await;

    assert_eq!(
        items(&h.rows(AGENT_IDS[0]).await),
        vec![assistant_item("oldnew")],
        "the retained timeline continues under the reused id"
    );
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    let texts: Vec<AgentTimelineItem> = events
        .iter()
        .filter_map(|e| match stream_event(e) {
            AgentStreamEvent::Timeline { item, .. } => Some((**item).clone()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec![assistant_item("old"), assistant_item("new")]);
}

#[tokio::test(start_paused = true)]
async fn flush_helper_is_idempotent_and_re_entry_safe() {
    // Idempotency: three chunks and a doubled tool call.
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;
    let tool = tool_call("tool-1", ToolCallStatus::Completed, "ok", None);
    session.push_event(assistant("a", "codex", None));
    session.push_event(assistant("b", "codex", None));
    session.push_event(assistant("c", "codex", None));
    session.push_event(timeline_event(tool.clone(), "codex", None));
    session.push_event(timeline_event(tool.clone(), "codex", None));
    settle().await;

    let rows = h.rows(AGENT_IDS[0]).await;
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(items(&rows), vec![assistant_item("abc"), tool]);
    assert_eq!(events.len(), 4);
    assert_eq!(covered_seqs(&rows), vec![1, 2, 3, 4]);
    assert_eq!(live_seqs(&events), some_seqs(1..=4));

    // Re-entry: a subscriber pushes a chunk while the first flush is delivered.
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;
    let pushed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut subscription = h
        .manager
        .subscribe(paseo_core::SubscribeOpts::default().without_replay())
        .await
        .unwrap();
    let (pushed2, session2) = (pushed.clone(), session.clone());
    tokio::spawn(async move {
        while let Some(event) = subscription.next().await {
            let is_assistant = matches!(
                &*event,
                ManagerEvent::AgentStream { event: AgentStreamEvent::Timeline { item, .. }, .. }
                    if matches!(**item, AgentTimelineItem::AssistantMessage { .. })
            );
            if is_assistant && !pushed2.swap(true, std::sync::atomic::Ordering::SeqCst) {
                session2.push_event(assistant("B", "codex", None));
            }
        }
    });

    session.push_event(assistant("A", "codex", None));
    settle().await;
    // "A" flushes on the leading edge; the chunk pushed during that flush lands in the next window.
    assert_eq!(
        items(&h.rows(AGENT_IDS[0]).await),
        vec![assistant_item("A")]
    );
    assert_eq!(h.events.timeline_streams(AGENT_IDS[0]).len(), 1);

    advance(WINDOW).await;
    let rows = h.rows(AGENT_IDS[0]).await;
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(items(&rows), vec![assistant_item("AB")]);
    assert_eq!(events.len(), 2);
    assert_eq!(covered_seqs(&rows), vec![1, 2]);
    assert_eq!(live_seqs(&events), some_seqs(1..=2));
    assert!(pushed.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn manager_flush_drains_pending_chunks_before_background_tasks() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;

    session.push_event(assistant("flush", "codex", None));
    session.push_event(assistant("-pending", "codex", None));
    settle().await;

    // "flush" left on the leading edge; "-pending" is what `flush()` drains.
    assert_eq!(
        items(&h.rows(AGENT_IDS[0]).await),
        vec![assistant_item("flush")]
    );
    h.manager.flush().await;
    settle().await;

    let rows = h.rows(AGENT_IDS[0]).await;
    assert_eq!(items(&rows), vec![assistant_item("flush-pending")]);
    assert_eq!(h.events.timeline_streams(AGENT_IDS[0]).len(), 2);

    advance(100).await;
    assert_eq!(h.rows(AGENT_IDS[0]).await.len(), 1);
    assert_eq!(h.events.timeline_streams(AGENT_IDS[0]).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn foreground_stream_yields_coalesced_timeline_events_before_terminal_events() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;
    let mut stream = h
        .manager
        .stream_agent(
            AGENT_IDS[0],
            PromptInput::from("prompt"),
            RunOpts::default(),
        )
        .await
        .unwrap();
    let first = tokio::spawn(async move {
        let first = stream.next().await;
        (stream, first)
    });
    h.manager
        .wait_for_agent_run_start(AGENT_IDS[0], None)
        .await
        .unwrap();

    session.push_event(assistant("hello ", "codex", Some("turn-1")));
    session.push_event(assistant("world", "codex", Some("turn-1")));
    session.push_event(turn_completed("turn-1"));
    settle().await;

    let (mut stream, first_event) = first.await.unwrap();
    assert_eq!(first_event.unwrap(), Some(turn_started_event("turn-1")));
    assert_eq!(
        stream.next().await.unwrap(),
        Some(assistant("hello ", "codex", Some("turn-1")))
    );
    assert_eq!(
        stream.next().await.unwrap(),
        Some(assistant("world", "codex", Some("turn-1")))
    );
    assert_eq!(stream.next().await.unwrap(), Some(turn_completed("turn-1")));
    assert_eq!(stream.next().await.unwrap(), None);
    settle().await;

    let rows = h.rows(AGENT_IDS[0]).await;
    assert_eq!(items(&rows), vec![assistant_item("hello world")]);
    let streams = h.events.streams(AGENT_IDS[0]);
    let kinds: Vec<&str> = streams
        .iter()
        .map(|e| stream_event(e).type_name())
        .collect();
    assert_eq!(
        &kinds[..4],
        ["turn_started", "timeline", "timeline", "turn_completed"]
    );
    assert!(matches!(
        stream_event(&streams[1]),
        AgentStreamEvent::Timeline { item, turn_id, .. }
            if **item == assistant_item("hello ") && turn_id.as_deref() == Some("turn-1")
    ));
}

fn turn_started_event(turn: &str) -> AgentStreamEvent {
    AgentStreamEvent::TurnStarted {
        provider: "codex".into(),
        turn_id: Some(turn.into()),
    }
}

#[tokio::test(start_paused = true)]
async fn every_collapsed_item_is_one_timeline_row_with_monotonic_seqs() {
    let h = Harness::new().await;
    let session = managed(&h, AGENT_IDS[0]).await;
    let t = Some("turn-1");

    session.push_event(assistant("a1", "codex", t));
    session.push_event(assistant("a2", "codex", t));
    session.push_event(reasoning("r1", "codex", t));
    session.push_event(reasoning("r2", "codex", t));
    session.push_event(assistant("b1", "codex", t));
    session.push_event(assistant("b2", "codex", t));
    session.push_event(turn_completed("turn-1"));
    settle().await;

    let rows = h.rows(AGENT_IDS[0]).await;
    let expected = vec![
        assistant_item("a1a2"),
        AgentTimelineItem::Reasoning {
            text: "r1r2".into(),
        },
        assistant_item("b1b2"),
    ];
    assert_eq!(items(&rows), expected);
    assert_eq!(covered_seqs(&rows), vec![1, 2, 3, 4]);
    let events = h.events.timeline_streams(AGENT_IDS[0]);
    assert_eq!(events.len(), 4);
    assert_eq!(live_seqs(&events), some_seqs(1..=4));

    advance(100).await;
    assert_eq!(items(&h.rows(AGENT_IDS[0]).await), expected);
    assert_eq!(h.events.timeline_streams(AGENT_IDS[0]).len(), 4);
}
