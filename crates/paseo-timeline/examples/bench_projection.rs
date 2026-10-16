//! Microbench: `project_rows` over the 300-row bursty fixture (`bench/timeline-300.json`).
//! `cargo run --release -p paseo-timeline --example bench_projection`
use std::time::Instant;

use paseo_agent_types::AgentTimelineItem;
use paseo_timeline::{project_rows, ProjectionMode, Row};

fn main() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bench/timeline-300.json");
    let doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let rows: Vec<Row> = doc
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let item = AgentTimelineItem::from_value(&r["item"]).unwrap();
            Row::new(r["seq"].as_i64().unwrap(), r["timestamp"].as_str().unwrap(), item)
        })
        .collect();
    let n = 20_000;
    for _ in 0..2_000 {
        std::hint::black_box(project_rows(&rows, ProjectionMode::Projected));
    }
    let mut samples = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let mut out = 0;
        for _ in 0..n {
            out += project_rows(&rows, ProjectionMode::Projected).len();
        }
        std::hint::black_box(out);
        samples.push(t.elapsed().as_secs_f64() * 1e6 / n as f64);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "{{\"impl\":\"rust\",\"rows\":{},\"projected\":{},\"us_per_call_median\":{:.2}}}",
        rows.len(),
        project_rows(&rows, ProjectionMode::Projected).len(),
        samples[2]
    );
}
