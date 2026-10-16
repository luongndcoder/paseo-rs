//! Golden replay of `serializeAgentStreamEvent` vectors from WP0 (`fixtures/p4/**/*mapper*.jsonl`,
//! lines `{"fn":"serializeAgentStreamEvent","args":[event],"expected":payload|null}`).
//! Skips with a note when the fixtures are missing.

use paseo_agent_types::{to_wire, AgentStreamEvent};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "jsonl")
            && p.file_name()
                .is_some_and(|n| n.to_string_lossy().contains("mapper"))
        {
            out.push(p);
        }
    }
}

#[test]
fn wire_vectors_match_ts() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/p4");
    let mut files = Vec::new();
    collect(&root, &mut files);
    if files.is_empty() {
        println!("golden: no fixtures under {}; skipping", root.display());
        return;
    }
    let (mut ok, mut skipped) = (0, 0);
    for f in files {
        for line in std::fs::read_to_string(&f)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
        {
            let v: Value = paseo_agent_types::js_str::parse_js_json(line).unwrap();
            if v["fn"] != "serializeAgentStreamEvent" {
                skipped += 1;
                continue;
            }
            let ev = AgentStreamEvent::from_value(&v["args"][0]);
            let got = ev
                .ok()
                .and_then(|e| to_wire(&e))
                .map(|w| w.into_value())
                .unwrap_or(Value::Null);
            assert_eq!(got, v["expected"], "{}: {line}", f.display());
            ok += 1;
        }
    }
    println!("golden: {ok} ok, {skipped} skipped");
}
