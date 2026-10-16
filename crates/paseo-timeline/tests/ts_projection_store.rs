//! Ports of the TS unit tests: `timeline-projection.test.ts`, `agent-timeline-store.test.ts`,
//! `timeline-prompt-index.test.ts`, `agent-timeline-content.test.ts` (cases not already covered
//! one-to-one by the WP0 goldens, plus the explicit assertions of the TS tests).

use paseo_agent_types::AgentTimelineItem;
use paseo_timeline::paging::{select_page_from_rows, select_window};
use paseo_timeline::{
    project_rows, AppendOptions, CollapseKind, Cursor, Direction, FetchOptions, ProjectionMode,
    Row, SeedOptions, SeqRange, TimelineState,
};
use serde_json::{json, Value};

const TS: &str = "2026-02-13T00:00:00.000Z";

fn item(v: Value) -> AgentTimelineItem {
    AgentTimelineItem::from_value(&v).unwrap()
}
fn asst(seq: i64, text: &str) -> Row {
    Row::new(
        seq,
        TS,
        item(json!({"type": "assistant_message", "text": text})),
    )
}
fn asst_id(seq: i64, text: &str, id: &str) -> Row {
    Row::new(
        seq,
        TS,
        item(json!({"type": "assistant_message", "text": text, "messageId": id})),
    )
}
fn user(seq: i64, text: &str) -> Row {
    Row::new(seq, TS, item(json!({"type": "user_message", "text": text})))
}
fn tool_json(call: &str, status: &str, output: Value) -> Value {
    json!({"type": "tool_call", "callId": call, "name": "shell", "status": status, "error": null,
           "detail": {"type": "unknown", "input": {"cmd": "pwd"}, "output": output}})
}
fn tool(seq: i64, call: &str, status: &str) -> Row {
    let output = if status == "completed" {
        json!({"stdout": "/tmp"})
    } else {
        Value::Null
    };
    Row::new(seq, TS, item(tool_json(call, status, output)))
}
fn seqs(rows: &[Row]) -> Vec<i64> {
    rows.iter().map(|r| r.seq).collect()
}
fn r(a: i64, b: i64) -> SeqRange {
    SeqRange {
        start_seq: a,
        end_seq: b,
    }
}

// ---- projectTimelineRows ----

#[test]
fn merges_adjacent_assistant_chunks() {
    let rows = vec![asst(1, "Hel"), asst(2, "lo"), user(3, "next")];
    let p = project_rows(&rows, ProjectionMode::Projected);
    assert_eq!(p.len(), 2);
    assert_eq!(
        p[0].item.to_value(),
        json!({"type": "assistant_message", "text": "Hello"})
    );
    assert_eq!((p[0].seq_start, p[0].seq_end), (1, 2));
    assert_eq!(p[0].source_seq_ranges, vec![r(1, 2)]);
    assert!(p[0].collapsed.contains(&CollapseKind::AssistantMerge));
}

#[test]
fn message_id_rules() {
    let same = project_rows(
        &[asst_id(1, "Hel", "msg-1"), asst_id(2, "lo", "msg-1")],
        ProjectionMode::Projected,
    );
    assert_eq!(same.len(), 1);
    assert_eq!(
        same[0].item.to_value(),
        json!({"type": "assistant_message", "text": "Hello", "messageId": "msg-1"})
    );
    let differ = project_rows(
        &[asst_id(1, "A.", "msg-1"), asst_id(2, "B.", "msg-2")],
        ProjectionMode::Projected,
    );
    assert_eq!(differ.len(), 2);
    // An undefined messageId on the incoming chunk merges; the first id is kept.
    let undefined = project_rows(
        &[asst_id(1, "A", "m"), asst(2, "B")],
        ProjectionMode::Projected,
    );
    assert_eq!(undefined.len(), 1);
    assert_eq!(undefined[0].item.to_value()["messageId"], "m");
    // An explicit id on the incoming chunk after an id-less one breaks the merge.
    assert_eq!(
        project_rows(
            &[asst(1, "A"), asst_id(2, "B", "m")],
            ProjectionMode::Projected
        )
        .len(),
        2
    );
}

#[test]
fn merges_adjacent_reasoning_chunks() {
    let reasoning =
        |seq, text: &str| Row::new(seq, TS, item(json!({"type": "reasoning", "text": text})));
    let rows = vec![
        reasoning(1, "Step "),
        reasoning(2, "by step"),
        asst(3, "done"),
    ];
    let p = project_rows(&rows, ProjectionMode::Projected);
    assert_eq!(p.len(), 2);
    assert_eq!(
        p[0].item.to_value(),
        json!({"type": "reasoning", "text": "Step by step"})
    );
    assert!(p[0].collapsed.contains(&CollapseKind::ReasoningMerge));
}

#[test]
fn collapses_tool_lifecycle_with_exact_ranges() {
    let rows = vec![
        tool(1, "call_1", "running"),
        asst(2, "working"),
        tool(3, "call_1", "completed"),
    ];
    let p = project_rows(&rows, ProjectionMode::Projected);
    assert_eq!(p.len(), 2);
    let t = p[0].item.as_tool_call().unwrap();
    assert_eq!(
        (t.status.as_str(), t.call_id.as_str()),
        ("completed", "call_1")
    );
    assert_eq!(p[0].source_seq_ranges, vec![r(1, 1), r(3, 3)]);
    assert!(p[0].collapsed.contains(&CollapseKind::ToolLifecycle));
}

#[test]
fn replaces_plugin_rows_with_same_identity_across_turns() {
    let plugin = |seq, turn: &str, status: &str| Row {
        turn_id: Some(turn.into()),
        ..Row::new(
            seq,
            TS,
            item(
                json!({"type": "plugin", "id": "review-1", "pluginId": "review", "kind": "review",
                        "version": 1, "data": {"status": status}}),
            ),
        )
    };
    let rows = vec![
        plugin(1, "turn-1", "running"),
        plugin(2, "turn-2", "complete"),
    ];
    let p = project_rows(&rows, ProjectionMode::Projected);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].item, rows[1].item);
    assert_eq!(p[0].turn_id.as_deref(), Some("turn-2"));
    assert_eq!(p[0].source_seq_ranges, vec![r(1, 2)]);
    assert!(p[0].collapsed.contains(&CollapseKind::Identity));
    assert_eq!((p[0].seq_start, p[0].seq_end), (1, 2));
}

#[test]
fn canonical_mode_is_unchanged() {
    let rows = vec![asst(10, "A"), asst(11, "B")];
    let p = project_rows(&rows, ProjectionMode::Canonical);
    assert_eq!(p.len(), 2);
    assert!(p.iter().all(|e| e.collapsed.is_empty()));
}

#[test]
fn tool_call_with_different_turn_starts_a_new_row() {
    let with_turn = |seq, turn: &str| Row {
        turn_id: Some(turn.into()),
        ..tool(seq, "c", "running")
    };
    let p = project_rows(
        &[with_turn(1, "t1"), with_turn(2, "t2")],
        ProjectionMode::Projected,
    );
    assert_eq!(p.len(), 2);
}

// ---- selectTimelineWindowByProjectedLimit ----

#[test]
fn window_tail_after_before() {
    let rows = vec![
        asst(1, "Hel"),
        asst(2, "lo"),
        user(3, "next"),
        asst(4, "Wor"),
        asst(5, "ld"),
    ];
    let s = select_window(&rows, Direction::Tail, 1);
    assert_eq!((s.min_seq, s.max_seq), (Some(4), Some(5)));
    assert_eq!(seqs(&s.selected_rows), vec![4, 5]);
    assert_eq!(s.projected_entries.len(), 1);
    assert_eq!(s.projected_entries[0].item.to_value()["text"], "World");

    let rows = vec![asst(10, "A"), asst(11, "B"), user(12, "u1"), user(13, "u2")];
    let s = select_window(&rows, Direction::After, 2);
    assert_eq!((s.min_seq, s.max_seq), (Some(10), Some(12)));
    assert_eq!(seqs(&s.selected_rows), vec![10, 11, 12]);

    let rows = vec![asst(1, "a"), asst(2, "b"), user(3, "u1"), user(4, "u2")];
    let s = select_window(&rows, Direction::Before, 1);
    assert_eq!((s.min_seq, s.max_seq), (Some(4), Some(4)));
    assert_eq!(s.projected_entries[0].item.to_value()["text"], "u2");
}

#[test]
fn window_non_monotonic_tool_seq_end_and_overlap_expansion() {
    let rows = vec![
        tool(1, "call_1", "running"),
        asst(2, "working"),
        tool(3, "call_1", "completed"),
    ];
    let s = select_window(&rows, Direction::Tail, 2);
    assert_eq!(s.projected_entries.len(), 2);
    assert_eq!((s.min_seq, s.max_seq), (Some(1), Some(3)));
    assert_eq!(seqs(&s.selected_rows), vec![1, 2, 3]);

    let rows = vec![
        tool(1, "call_1", "running"),
        asst(2, "work"),
        asst(3, "ing"),
        tool(4, "call_1", "completed"),
    ];
    let s = select_window(&rows, Direction::After, 1);
    assert_eq!((s.min_seq, s.max_seq), (Some(1), Some(4)));
    assert_eq!(seqs(&s.selected_rows), vec![1, 2, 3, 4]);
    let types: Vec<_> = s
        .projected_entries
        .iter()
        .map(|e| e.item.type_name())
        .collect();
    assert_eq!(types, vec!["tool_call", "assistant_message"]);
}

#[test]
fn window_repeated_running_tool_is_one_item() {
    let mut rows: Vec<Row> = (0..6).map(|i| asst(i + 1, &format!("old {i}"))).collect();
    for i in 0..20 {
        rows.push(Row::new(
            i + 7,
            TS,
            item(tool_json("call_1", "running", json!({"progress": i}))),
        ));
    }
    let s = select_window(&rows, Direction::Tail, 100);
    let tools: Vec<_> = s
        .projected_entries
        .iter()
        .filter(|e| e.item.as_tool_call().is_some())
        .collect();
    assert_eq!(tools.len(), 1);
    assert!(tools[0].collapsed.contains(&CollapseKind::ToolLifecycle));
    assert_eq!(s.projected_entries.len(), 2);
}

// ---- selectProjectedTimelinePage ----

fn page(
    rows: &[Row],
    d: Direction,
    cursor: Option<i64>,
    limit: usize,
) -> paseo_timeline::PageSelection {
    select_page_from_rows(rows, None, d, cursor, Some(limit))
}

#[test]
fn page_tail_returns_full_projected_items() {
    let mut rows = vec![user(1, "go")];
    rows.extend((0..120).map(|i| tool(i + 2, "call_1", "running")));
    let p = page(&rows, Direction::Tail, None, 100);
    let types: Vec<_> = p.entries.iter().map(|e| e.item.type_name()).collect();
    assert_eq!(types, vec!["user_message", "tool_call"]);
    assert_eq!(p.entries[1].source_seq_ranges, vec![r(2, 121)]);
    assert_eq!(
        (p.start_seq, p.end_seq, p.has_newer),
        (Some(1), Some(121), false)
    );
}

#[test]
fn page_after_includes_full_tool_when_only_update_is_new() {
    let rows = vec![
        tool(10, "call_1", "running"),
        asst(11, "working"),
        tool(250, "call_1", "completed"),
    ];
    let p = page(&rows, Direction::After, Some(249), 100);
    assert_eq!(p.entries.len(), 1);
    assert_eq!((p.entries[0].seq_start, p.entries[0].seq_end), (10, 250));
    assert_eq!(p.entries[0].source_seq_ranges, vec![r(10, 10), r(250, 250)]);
    assert_eq!((p.start_seq, p.end_seq), (Some(250), Some(250)));
}

#[test]
fn page_after_cursor_advances_only_through_contiguous_seqs() {
    let mut rows = vec![tool(1, "call_1", "running")];
    rows.extend((0..498).map(|i| user(i + 2, &format!("middle {}", i + 2))));
    rows.push(tool(500, "call_1", "completed"));
    rows.extend((0..101).map(|i| user(i + 501, &format!("later {}", i + 501))));
    let p = page(&rows, Direction::After, Some(0), 100);
    assert_eq!(p.entries[0].item.type_name(), "tool_call");
    assert!(!p
        .entries
        .iter()
        .any(|e| e.item.type_name() == "user_message" && e.seq_start == 101));
    assert_eq!(p.end_seq, Some(100));
    assert!(p.has_newer);
}

#[test]
fn page_after_limit_counts_projected_entries() {
    let mut rows: Vec<Row> = (0..200).map(|i| asst(i + 1, "x")).collect();
    rows.extend((0..150).map(|i| user(i + 201, &format!("message {}", i + 1))));
    let p = page(&rows, Direction::After, Some(0), 100);
    assert_eq!(p.entries.len(), 100);
    assert_eq!(p.entries[0].item.to_value()["text"], "x".repeat(200));
    assert_eq!(p.entries[0].collapsed, vec![CollapseKind::AssistantMerge]);
    let last = p.entries.last().unwrap();
    assert_eq!(
        (last.seq_start, last.item.to_value()["text"].clone()),
        (299, json!("message 99"))
    );
    assert_eq!(
        (p.start_seq, p.end_seq, p.has_newer),
        (Some(1), Some(299), true)
    );
}

#[test]
fn page_before_limit_never_repeats_a_merged_assistant() {
    let mut rows: Vec<Row> = (0..80).map(|i| asst(i + 1, "x")).collect();
    rows.extend((0..4).map(|i| user(i + 81, &format!("message {}", i + 1))));
    let newest = page(&rows, Direction::Before, Some(85), 2);
    let middle = page(&rows, Direction::Before, newest.start_seq, 2);
    let oldest = page(&rows, Direction::Before, middle.start_seq, 2);
    let exhausted = page(&rows, Direction::Before, oldest.start_seq, 2);
    let summary = |p: &paseo_timeline::PageSelection| {
        (
            p.entries
                .iter()
                .map(|e| e.item.type_name())
                .collect::<Vec<_>>(),
            p.start_seq,
            p.end_seq,
            p.has_older,
        )
    };
    assert_eq!(
        summary(&newest),
        (vec!["user_message"; 2], Some(83), Some(84), true)
    );
    assert_eq!(
        summary(&middle),
        (vec!["user_message"; 2], Some(81), Some(82), true)
    );
    assert_eq!(
        summary(&oldest),
        (vec!["assistant_message"], Some(1), Some(80), false)
    );
    assert!(exhausted.entries.is_empty());
}

#[test]
fn page_before_places_wide_tool_on_anchor_page_and_tail_includes_it() {
    let mut rows = vec![tool(1, "call_1", "running")];
    rows.extend((0..498).map(|i| user(i + 2, &format!("middle {}", i + 2))));
    rows.push(tool(500, "call_1", "completed"));
    let newest = page(&rows, Direction::Before, Some(500), 100);
    let anchored = page(&rows, Direction::Before, Some(101), 100);
    assert_eq!(newest.entries.len(), 100);
    assert!(!newest
        .entries
        .iter()
        .any(|e| e.item.as_tool_call().is_some()));
    assert_eq!(
        (newest.start_seq, newest.end_seq, newest.has_older),
        (Some(400), Some(499), true)
    );
    assert_eq!(anchored.entries.len(), 100);
    assert_eq!(
        (anchored.entries[0].seq_start, anchored.entries[0].seq_end),
        (1, 500)
    );
    assert_eq!(
        (anchored.start_seq, anchored.end_seq, anchored.has_older),
        (Some(1), Some(100), false)
    );

    let mut rows = vec![tool(1, "call_1", "running")];
    rows.extend((0..499).map(|i| user(i + 2, &format!("middle {}", i + 2))));
    rows.push(tool(501, "call_1", "completed"));
    let tail = page(&rows, Direction::Tail, None, 100);
    assert!(tail.entries.iter().any(|e| e.item.as_tool_call().is_some()));
    assert_eq!(tail.end_seq, Some(501));
}

// ---- InMemoryAgentTimelineStore ----

fn seeded(rows: Vec<Row>, next_seq: Option<i64>, epoch: &str) -> TimelineState {
    TimelineState::initialize(SeedOptions {
        rows,
        next_seq,
        epoch: Some(epoch.into()),
        ..Default::default()
    })
}

fn state(epoch: &str) -> TimelineState {
    TimelineState::initialize(SeedOptions {
        epoch: Some(epoch.into()),
        ..Default::default()
    })
}

fn append(s: &mut TimelineState, v: Value) -> Row {
    s.append(
        item(v),
        AppendOptions {
            timestamp: Some(TS.into()),
            ..Default::default()
        },
    )
}

#[test]
fn before_cursor_overshoot_clamps_to_tail_window() {
    let s = seeded(
        vec![
            asst_id(5, "five", "five"),
            asst_id(6, "six", "six"),
            asst_id(7, "seven", "seven"),
        ],
        Some(8),
        "epoch-1",
    );
    let res = s.fetch(&FetchOptions {
        direction: Some(Direction::Before),
        cursor: Some(Cursor {
            epoch: "epoch-1".into(),
            seq: 100,
        }),
        limit: Some(2),
    });
    assert!(!res.reset && !res.stale_cursor && !res.gap);
    assert_eq!(
        (res.window.min_seq, res.window.max_seq, res.window.next_seq),
        (5, 7, 8)
    );
    assert!(res.has_older && !res.has_newer);
    assert_eq!(
        res.rows.iter().map(|r| r.seq()).collect::<Vec<_>>(),
        vec![6, 7]
    );
}

#[test]
fn after_cursor_behind_retained_history_resets_to_tail() {
    let s = seeded(
        vec![
            asst_id(5, "five", "five"),
            asst_id(6, "six", "six"),
            asst_id(7, "seven", "seven"),
        ],
        Some(8),
        "epoch-1",
    );
    let res = s.fetch(&FetchOptions {
        direction: Some(Direction::After),
        cursor: Some(Cursor {
            epoch: "epoch-1".into(),
            seq: 1,
        }),
        limit: Some(2),
    });
    assert!(res.reset && res.gap && !res.stale_cursor);
    assert_eq!(res.direction, Direction::After);
    assert_eq!(
        res.rows.iter().map(|r| r.seq()).collect::<Vec<_>>(),
        vec![6, 7]
    );
}

#[test]
fn streaming_tool_updates_retain_one_row() {
    let mut s = state("e");
    for seq in 1..=2000i64 {
        let row = append(
            &mut s,
            json!({"type": "tool_call", "callId": "child", "name": "task", "status": "running", "error": null,
                   "detail": {"type": "plain_text", "label": "Child", "text": "x".repeat(seq as usize * 4)}}),
        );
        assert_eq!(row.seq, seq);
    }
    let res = s.fetch(&FetchOptions {
        limit: Some(0),
        ..Default::default()
    });
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.window.max_seq, 2000);
}

#[test]
fn mid_message_cursor_catches_up_with_complete_message() {
    let mut s = state("e");
    append(
        &mut s,
        json!({"type": "assistant_message", "messageId": "m", "text": "A"}),
    );
    append(
        &mut s,
        json!({"type": "assistant_message", "messageId": "m", "text": "B"}),
    );
    let res = s.fetch(&FetchOptions {
        direction: Some(Direction::After),
        cursor: Some(Cursor {
            epoch: "e".into(),
            seq: 1,
        }),
        ..Default::default()
    });
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].item.to_value()["text"], "AB");
    assert_eq!((res.rows[0].seq_start, res.rows[0].seq_end), (1, 2));
    assert_eq!(res.rows[0].source_seq_ranges, vec![r(1, 2)]);
}

#[test]
fn fetched_coverage_is_a_value_snapshot() {
    let mut s = state("e");
    append(&mut s, json!({"type": "assistant_message", "text": "A"}));
    let before = s.fetch(&FetchOptions::default());
    append(&mut s, json!({"type": "assistant_message", "text": "B"}));
    assert_eq!(before.rows[0].item.to_value()["text"], "A");
    assert_eq!(before.rows[0].seq_end, 1);
    let after = s.fetch(&FetchOptions::default());
    assert_eq!(after.rows[0].item.to_value()["text"], "AB");
    assert_eq!(after.rows[0].source_seq_ranges, vec![r(1, 2)]);
}

#[test]
fn seeding_projected_history_preserves_source_positions() {
    let mut a = state("a");
    let tool = |status: &str| {
        json!({"type": "tool_call", "callId": "t", "name": "shell", "status": status, "error": null,
               "detail": {"type": "plain_text", "label": "work"}})
    };
    append(&mut a, tool("running"));
    append(
        &mut a,
        json!({"type": "assistant_message", "text": "Answer"}),
    );
    append(&mut a, tool("completed"));
    let mut b = TimelineState::initialize(SeedOptions {
        projected_rows: a.rows().to_vec(),
        epoch: Some("b".into()),
        ..Default::default()
    });
    let next = append(&mut b, json!({"type": "user_message", "text": "next"}));
    assert_eq!(next.seq, 4);
    let starts: Vec<i64> = b
        .fetch(&FetchOptions::default())
        .rows
        .iter()
        .map(|r| r.seq_start)
        .collect();
    assert_eq!(starts, vec![1, 2, 4]);
}

#[test]
fn last_assistant_message_does_not_join_different_messages() {
    let mut s = state("a");
    append(
        &mut s,
        json!({"type": "assistant_message", "messageId": "first", "text": "First"}),
    );
    append(
        &mut s,
        json!({"type": "assistant_message", "messageId": "second", "text": "Sec"}),
    );
    append(
        &mut s,
        json!({"type": "assistant_message", "messageId": "second", "text": "ond"}),
    );
    assert_eq!(s.last_assistant_message(), Some("Second"));
}

#[test]
fn transitive_tool_updates_in_contiguous_tail() {
    let mut s = state("a");
    let tool = |call: &str, status: &str| {
        json!({"type": "tool_call", "callId": call, "name": "shell", "status": status, "error": null,
               "detail": {"type": "plain_text", "label": "work"}})
    };
    append(&mut s, tool("first", "running"));
    append(&mut s, tool("second", "running"));
    append(&mut s, json!({"type": "user_message", "text": "Continue"}));
    append(&mut s, tool("first", "completed"));
    append(
        &mut s,
        json!({"type": "assistant_message", "text": "Answer"}),
    );
    append(&mut s, tool("second", "completed"));
    let tail = s.fetch(&FetchOptions {
        limit: Some(1),
        ..Default::default()
    });
    let starts: Vec<i64> = tail.rows.iter().map(|r| r.seq_start).collect();
    assert_eq!(starts, vec![1, 2, 3, 5]);
    assert_eq!(
        (tail.start_seq, tail.end_seq, tail.has_older),
        (Some(1), Some(6), false)
    );
}

#[test]
fn empty_window_and_stale_epoch() {
    let s = state("e");
    let w = s.window();
    assert_eq!((w.min_seq, w.max_seq, w.next_seq), (0, 0, 1));
    let mut s = s;
    append(&mut s, json!({"type": "user_message", "text": "hi"}));
    let res = s.fetch(&FetchOptions {
        direction: Some(Direction::After),
        cursor: Some(Cursor {
            epoch: "old".into(),
            seq: 1,
        }),
        ..Default::default()
    });
    assert!(res.reset && res.stale_cursor && !res.gap);
    assert_eq!(res.rows.len(), 1);
    // A fresh epoch is minted when none is seeded.
    assert_ne!(
        TimelineState::initialize(SeedOptions::default()).epoch(),
        TimelineState::initialize(SeedOptions::default()).epoch()
    );
}

#[test]
fn empty_turn_and_provider_ids_are_dropped_like_ts() {
    let mut s = state("e");
    let row = s.append(
        item(json!({"type": "user_message", "text": "x"})),
        AppendOptions {
            timestamp: Some(TS.into()),
            turn_id: Some(String::new()),
            provider_message_id: Some(String::new()),
        },
    );
    assert!(row.turn_id.is_none() && row.provider_message_id.is_none());
}

// ---- prompt index / content ----

#[test]
fn prompt_preview_bounds_and_skips_assistant_rows() {
    let rows = vec![
        user(1, &format!("  {}  ", "a b\n\tc".repeat(60))),
        asst(2, "not a prompt"),
        user(3, "short   text"),
    ];
    let idx = paseo_timeline::build_timeline_prompt_index("e", &rows);
    assert_eq!(idx.prompts.len(), 2);
    assert_eq!(idx.prompts[0].preview.encode_utf16().count(), 120);
    assert!(idx.prompts[0].preview.ends_with('…'));
    assert_eq!(idx.prompts[1].preview, "short text");
    assert_eq!(idx.prompts[1].seq, 3);
}

#[test]
fn plain_text_truncation_notice_stays_within_budget() {
    let long = item(
        json!({"type": "tool_call", "callId": "c", "name": "x", "status": "completed", "error": null,
        "detail": {"type": "plain_text", "text": "a".repeat(70_000)}}),
    );
    let out = paseo_timeline::limits::limit_item_content(long).to_value();
    let text = out["detail"]["text"].as_str().unwrap();
    assert_eq!(text.len(), 64 * 1024);
    assert!(text.ends_with("\n\n[Content truncated]"));
}
