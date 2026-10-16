//! Exact JSON equality with the WP0 timeline goldens (`fixtures/p4/timeline`).

mod common;

use common::{load_json, load_jsonl};
use paseo_agent_types::AgentTimelineItem;
use paseo_timeline::paging::{
    select_items_by_projected_limit, select_page_from_rows, select_window,
};
use paseo_timeline::{
    build_timeline_prompt_index, project_entries, project_rows, AppendOptions, Cursor, Direction,
    FetchOptions, ProjectedEntry, ProjectionMode, Row, SeedOptions, TimelineState,
};
use serde_json::{json, Value};

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(String::from)
}

fn item(v: &Value) -> AgentTimelineItem {
    AgentTimelineItem::from_value(v).unwrap_or_else(|e| panic!("item {v}: {e}"))
}

fn seed_of(t: &Value) -> SeedOptions {
    let mut seed = SeedOptions {
        epoch: Some(s(t, "epoch").expect("epoch")),
        timestamp: s(t, "initTimestamp"),
        ..Default::default()
    };
    if let Some(sd) = t.get("seed").filter(|v| !v.is_null()) {
        if let Some(items) = sd.get("items").and_then(Value::as_array) {
            seed.items = items.iter().map(item).collect();
        }
        if let Some(rows) = sd.get("rows").and_then(Value::as_array) {
            seed.rows = rows.iter().map(|r| Row::from_value(r).unwrap()).collect();
        }
        seed.next_seq = sd.get("nextSeq").and_then(Value::as_i64);
    }
    seed
}

fn build(t: &Value) -> TimelineState {
    let mut state = TimelineState::initialize(seed_of(t));
    for step in t["steps"].as_array().unwrap() {
        assert_eq!(step["op"], "append");
        let row = state.append(
            item(&step["item"]),
            AppendOptions {
                timestamp: s(step, "timestamp"),
                provider_message_id: s(step, "providerMessageId"),
                turn_id: s(step, "turnId"),
            },
        );
        assert_eq!(
            row.to_value(),
            step["returnedRow"],
            "returned row of {}",
            t["name"]
        );
    }
    for e in t["enrich"].as_array().unwrap() {
        let got = state.enrich_submitted_user_message(
            e["clientMessageId"].as_str().unwrap(),
            e["providerMessageId"].as_str().unwrap(),
        );
        assert_eq!(
            got.map_or(Value::Null, |r| r.to_row_value()),
            e["result"],
            "enrich result of {}",
            t["name"]
        );
    }
    state
}

#[test]
fn store_timelines_match_ts() {
    let doc = load_json("store-timelines.json");
    let mut count = 0;
    for t in doc["timelines"].as_array().unwrap() {
        let name = t["name"].as_str().unwrap();
        let state = build(t);
        let exp = &t["expected"];
        let w = state.window();
        assert_eq!(
            json!({"minSeq": w.min_seq, "maxSeq": w.max_seq, "nextSeq": w.next_seq}),
            exp["window"],
            "{name} window"
        );
        assert_eq!(
            Value::Array(state.items().iter().map(|i| i.to_value()).collect()),
            exp["items"],
            "{name} items"
        );
        assert_eq!(
            Value::Array(
                state
                    .rows()
                    .iter()
                    .map(ProjectedEntry::to_row_value)
                    .collect()
            ),
            exp["projectedRows"],
            "{name} projectedRows"
        );
        assert_eq!(
            state.last_item().map_or(Value::Null, |i| i.to_value()),
            exp["lastItem"],
            "{name} lastItem"
        );
        assert_eq!(
            state
                .last_assistant_message()
                .map_or(Value::Null, |m| json!(m)),
            exp["lastAssistantMessage"],
            "{name} lastAssistantMessage"
        );
        assert_eq!(json!(state.epoch()), exp["epoch"], "{name} epoch");
        for m in exp["submittedUserMessages"].as_array().unwrap() {
            let got = state.submitted_user_message(m["clientMessageId"].as_str().unwrap());
            assert_eq!(
                got.map_or(Value::Null, |r| r.to_row_value()),
                m["row"],
                "{name} submitted {}",
                m["clientMessageId"]
            );
        }
        // Re-projection of projected rows is idempotent.
        assert_eq!(
            state.reproject(),
            state.rows(),
            "{name}: reprojection must be idempotent"
        );
        count += 1;
    }
    assert_eq!(count, 17);
}

#[test]
fn store_fetch_cases_match_ts() {
    let doc = load_json("store-timelines.json");
    let states: std::collections::HashMap<String, TimelineState> = doc["timelines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["name"].as_str().unwrap().to_string(), build(t)))
        .collect();
    let cases = load_jsonl("store-fetch-cases.jsonl");
    for (i, case) in cases.iter().enumerate() {
        let name = case["timeline"].as_str().unwrap();
        let o = &case["options"];
        let options = FetchOptions {
            direction: o
                .get("direction")
                .and_then(Value::as_str)
                .map(|d| Direction::parse(d).unwrap()),
            cursor: o.get("cursor").map(|c| Cursor {
                epoch: c["epoch"].as_str().unwrap().to_string(),
                seq: c["seq"].as_i64().unwrap(),
            }),
            limit: o.get("limit").and_then(Value::as_u64).map(|l| l as usize),
        };
        let got = states[name].fetch(&options).to_value();
        assert_eq!(got, case["result"], "fetch case {i} ({name}) options {o}");
    }
    assert_eq!(cases.len(), 2410);
}

fn entries_json(entries: &[ProjectedEntry]) -> Value {
    Value::Array(entries.iter().map(ProjectedEntry::to_value).collect())
}

#[test]
fn projection_cases_match_ts() {
    let cases = load_jsonl("projection-cases.jsonl");
    assert_eq!(cases.len(), 17);
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let rows: Vec<Row> = case["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| Row::from_value(r).unwrap())
            .collect();

        assert_eq!(
            entries_json(&project_rows(&rows, ProjectionMode::Canonical)),
            case["canonical"],
            "{name} canonical"
        );
        let projected = project_rows(&rows, ProjectionMode::Projected);
        assert_eq!(
            entries_json(&projected),
            case["projected"],
            "{name} projected"
        );
        assert_eq!(
            entries_json(&project_entries(&projected, ProjectionMode::Projected)),
            case["reprojected"],
            "{name} reprojected"
        );

        for w in case["windows"].as_array().unwrap() {
            let dir = Direction::parse(w["direction"].as_str().unwrap()).unwrap();
            let sel = select_window(&rows, dir, w["limit"].as_u64().unwrap() as usize);
            let got = json!({
                "projectedEntries": entries_json(&sel.projected_entries),
                "selectedRows": sel.selected_rows.iter().map(Row::to_value).collect::<Vec<_>>(),
                "minSeq": sel.min_seq,
                "maxSeq": sel.max_seq,
            });
            assert_eq!(
                got, w["result"],
                "{name} window {} {}",
                w["direction"], w["limit"]
            );
        }

        for p in case["pages"].as_array().unwrap() {
            let dir = Direction::parse(p["direction"].as_str().unwrap()).unwrap();
            let sel = select_page_from_rows(
                &rows,
                None,
                dir,
                p["cursorSeq"].as_i64(),
                Some(p["limit"].as_u64().unwrap() as usize),
            );
            let got = json!({
                "entries": entries_json(&sel.entries),
                "startSeq": sel.start_seq,
                "endSeq": sel.end_seq,
                "hasOlder": sel.has_older,
                "hasNewer": sel.has_newer,
            });
            assert_eq!(
                got, p["result"],
                "{name} page {} limit {} cursor {}",
                p["direction"], p["limit"], p["cursorSeq"]
            );
        }

        let items: Vec<AgentTimelineItem> = rows.iter().map(|r| r.item.clone()).collect();
        for sel in case["itemSelections"].as_array().unwrap() {
            let dir = Direction::parse(sel["direction"].as_str().unwrap()).unwrap();
            let got = select_items_by_projected_limit(
                &items,
                dir,
                sel["limit"].as_u64().unwrap() as usize,
            );
            let got = json!({
                "items": got.items.iter().map(|i| i.to_value()).collect::<Vec<_>>(),
                "totalProjected": got.total_projected,
                "shownProjected": got.shown_projected,
            });
            assert_eq!(
                got, sel["result"],
                "{name} itemSelection {} {}",
                sel["direction"], sel["limit"]
            );
        }
    }
}

#[test]
fn prompt_index_matches_ts() {
    let doc = load_json("prompt-index.json");
    let rows: Vec<Row> = doc["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| Row::from_value(r).unwrap())
        .collect();
    let index = build_timeline_prompt_index(doc["epoch"].as_str().unwrap(), &rows);
    assert_eq!(index.to_value(), doc["result"]);
}
