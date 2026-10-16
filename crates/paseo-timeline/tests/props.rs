//! Property tests: incremental projection equals batch projection, re-projection is idempotent,
//! pages are consistent with the window, and the coalescer preserves text bytes.

use std::time::{Duration, Instant};

use paseo_agent_types::{AgentStreamEvent, AgentTimelineItem};
use paseo_timeline::coalescer::Coalescer;
use paseo_timeline::{
    project_entries, project_rows, select_page_from_rows, Direction, Projection, ProjectionMode,
    Row,
};
use proptest::prelude::*;
use serde_json::{json, Value};

#[derive(Debug, Clone)]
enum Op {
    User(u8),
    Assistant(Option<u8>, u8),
    Reasoning(u8),
    Tool(u8, bool, u8),
    Plugin(u8, u8),
}

fn op_strategy() -> impl Strategy<Value = (Op, Option<u8>)> {
    let turn = prop_oneof![Just(None), Just(Some(1u8)), Just(Some(2u8))];
    let op = prop_oneof![
        any::<u8>().prop_map(Op::User),
        (
            prop_oneof![Just(None), Just(Some(1u8)), Just(Some(2u8))],
            any::<u8>()
        )
            .prop_map(|(m, t)| Op::Assistant(m, t)),
        any::<u8>().prop_map(Op::Reasoning),
        (0u8..3, any::<bool>(), any::<u8>()).prop_map(|(c, done, o)| Op::Tool(c, done, o)),
        (0u8..2, any::<u8>()).prop_map(|(i, d)| Op::Plugin(i, d)),
    ];
    (op, turn)
}

fn to_rows(ops: &[(Op, Option<u8>)]) -> Vec<Row> {
    ops.iter()
        .enumerate()
        .map(|(i, (op, turn))| {
            let item: Value = match op {
                Op::User(t) => json!({"type": "user_message", "text": format!("u{t}")}),
                Op::Assistant(m, t) => {
                    let mut v = json!({"type": "assistant_message", "text": format!("a{t}")});
                    if let Some(m) = m {
                        v["messageId"] = json!(format!("m{m}"));
                    }
                    v
                }
                Op::Reasoning(t) => json!({"type": "reasoning", "text": format!("r{t}")}),
                Op::Tool(c, done, o) => json!({
                    "type": "tool_call", "callId": format!("c{c}"), "name": "shell",
                    "status": if *done { "completed" } else { "running" }, "error": null,
                    "detail": {"type": "unknown", "input": {"c": c}, "output": if *done { json!(o) } else { Value::Null }}
                }),
                Op::Plugin(id, d) => json!({
                    "type": "plugin", "id": format!("p{id}"), "pluginId": "pl", "kind": "k",
                    "version": 1, "data": {"d": d}
                }),
            };
            Row {
                turn_id: turn.map(|t| format!("t{t}")),
                ..Row::new(i as i64 + 1, "2026-01-01T00:00:00.000Z", AgentTimelineItem::from_value(&item).unwrap())
            }
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]

    #[test]
    fn incremental_projection_equals_batch(ops in proptest::collection::vec(op_strategy(), 0..40)) {
        let rows = to_rows(&ops);
        let mut inc = Projection::new();
        for r in &rows { inc.append(r); }
        let batch = project_rows(&rows, ProjectionMode::Projected);
        prop_assert_eq!(inc.rows(), batch.as_slice());
    }

    #[test]
    fn reprojection_is_idempotent(ops in proptest::collection::vec(op_strategy(), 0..40)) {
        let rows = to_rows(&ops);
        let once = project_rows(&rows, ProjectionMode::Projected);
        let twice = project_entries(&once, ProjectionMode::Projected);
        prop_assert_eq!(once, twice);
    }

    #[test]
    fn tail_page_covers_the_newest_seq_and_pages_chain(
        ops in proptest::collection::vec(op_strategy(), 1..40),
        limit in 0usize..6,
    ) {
        let rows = to_rows(&ops);
        let max = rows.len() as i64;
        let tail = select_page_from_rows(&rows, None, Direction::Tail, None, Some(limit));
        prop_assert_eq!(tail.end_seq, Some(max));
        prop_assert!(!tail.has_newer);
        // Walking `before` pages from the end never repeats an entry's seq_start.
        let mut seen = std::collections::HashSet::new();
        let mut cursor = max + 1;
        let step = limit.max(1);
        for _ in 0..(rows.len() + 2) {
            let p = select_page_from_rows(&rows, None, Direction::Before, Some(cursor), Some(step));
            if p.entries.is_empty() { break; }
            for e in &p.entries { prop_assert!(seen.insert(e.seq_start)); }
            let Some(start) = p.start_seq else { break };
            cursor = start;
            if !p.has_older { break; }
        }
        let all = project_rows(&rows, ProjectionMode::Projected);
        prop_assert_eq!(seen.len(), all.len());
    }

    #[test]
    fn after_paging_from_zero_visits_every_source_seq(
        ops in proptest::collection::vec(op_strategy(), 1..40),
        limit in 1usize..5,
    ) {
        let rows = to_rows(&ops);
        let max = rows.len() as i64;
        let mut cursor = 0i64;
        let mut guard = 0;
        while cursor < max {
            let p = select_page_from_rows(&rows, None, Direction::After, Some(cursor), Some(limit));
            let end = p.end_seq;
            prop_assert!(end.is_some_and(|e| e > cursor), "cursor must advance: {cursor} -> {end:?}");
            cursor = end.unwrap();
            guard += 1;
            prop_assert!(guard <= rows.len() + 2);
        }
    }
}

fn text_event(text: &str, turn: u8) -> AgentStreamEvent {
    AgentStreamEvent::from_value(&json!({
        "type": "timeline", "provider": "codex", "turnId": format!("t{turn}"),
        "item": {"type": "assistant_message", "text": text}
    }))
    .unwrap()
}

proptest! {
    #[test]
    fn coalescer_preserves_text_bytes_and_turn_order(
        chunks in proptest::collection::vec(("[ -~\\n]{0,6}", 0u8..2, 0u64..90), 1..60),
    ) {
        let base = Instant::now();
        let mut c = Coalescer::default();
        let mut now_ms = 0u64;
        let mut out = Vec::new();
        let mut expect_per_turn: Vec<(u8, String)> = Vec::new();
        for (text, turn, dt) in &chunks {
            now_ms += dt;
            let now = base + Duration::from_millis(now_ms);
            while let Some(d) = c.next_deadline() {
                if d > now { break; }
                out.extend(c.tick(d));
            }
            out.extend(c.handle("a", &text_event(text, *turn), now));
            if text.is_empty() { continue; }
            match expect_per_turn.last_mut() {
                Some((t, s)) if t == turn => s.push_str(text),
                _ => expect_per_turn.push((*turn, text.clone())),
            }
        }
        out.extend(c.flush_all(base + Duration::from_millis(now_ms + 1)));
        // Same bytes in the same order, regrouped only within a turn run.
        let mut got: Vec<(u8, String)> = Vec::new();
        for f in &out {
            let turn: u8 = f.turn_id.as_ref().unwrap()[1..].parse().unwrap();
            let AgentTimelineItem::AssistantMessage { text, .. } = &f.item else { panic!() };
            match got.last_mut() {
                Some((t, s)) if *t == turn => s.push_str(text),
                _ => got.push((turn, text.clone())),
            }
        }
        prop_assert_eq!(got, expect_per_turn);
        prop_assert_eq!(c.pending_timers(), 0);
    }
}
