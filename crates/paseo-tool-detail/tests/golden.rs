//! Golden replay against WP0 mapper vectors. Skips (with a note) when the fixture dir is
//! missing.
//!
//! Assumed format (WP0 had not landed when this was written): `fixtures/p4/**/*.jsonl` (any file
//! whose name contains `mapper`), one JSON object per line:
//! `{"fn": "<TS export name>", "args": [...], "expected": <json>}`. `expected` may also be named
//! `output`/`result`. Unknown `fn` names are counted and reported, not failed.

use paseo_agent_types::{AgentTimelineItem, ToolCallItem};
use paseo_tool_detail::detail::{claude, codex, opencode};
use paseo_tool_detail::*;
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

fn arg(args: &[Value], i: usize) -> Option<&Value> {
    args.get(i)
}

fn run(name: &str, args: &[Value]) -> Option<Value> {
    Some(match name {
        "normalizeToolCallStatus" => {
            let raw = arg(args, 0).and_then(Value::as_str);
            let s = normalize_tool_call_status(raw, arg(args, 1), arg(args, 2));
            Value::String(s.as_str().into())
        }
        "coerceToolCallId" => Value::String(coerce_tool_call_id(arg(args, 0)?)),
        "deriveClaudeToolDetail" => {
            claude::derive_claude_tool_detail(arg(args, 0)?.as_str()?, arg(args, 1), arg(args, 2))
                .to_value()
        }
        "deriveCodexToolDetail" => {
            let p = arg(args, 0)?;
            codex::derive_codex_tool_detail(
                p.get("name")?.as_str()?,
                p.get("input"),
                p.get("output"),
                p.get("cwd").and_then(Value::as_str),
            )
            .to_value()
        }
        "deriveOpencodeToolDetail" => opencode::derive_opencode_tool_detail(
            arg(args, 0)?.as_str()?,
            arg(args, 1),
            arg(args, 2),
            arg(args, 3),
            arg(args, 4).and_then(Value::as_object),
        )
        .to_value(),
        "mergeToolCallItems" => {
            let a = ToolCallItem::from_value(arg(args, 0)?).ok()?;
            let b = ToolCallItem::from_value(arg(args, 1)?).ok()?;
            merge_tool_call_items(&a, &b).to_value()
        }
        "limitAgentTimelineItemContent" => {
            let item = AgentTimelineItem::from_value(arg(args, 0)?).ok()?;
            limit_item_content(item).to_value()
        }
        _ => return None,
    })
}

#[test]
fn mapper_vectors_match_ts() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/p4");
    if !root.is_dir() {
        println!(
            "golden: {} missing; skipping (WP0 fixtures not produced yet)",
            root.display()
        );
        return;
    }
    let mut files = Vec::new();
    collect(&root, &mut files);
    files.sort();
    if files.is_empty() {
        println!("golden: no *mapper*.jsonl under fixtures/p4; skipping");
        return;
    }
    let (mut ok, mut skipped) = (0usize, 0usize);
    let mut failures = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        for (i, line) in text
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
        {
            let v: Value = serde_json::from_str(line).unwrap();
            let (Some(name), Some(args)) = (v["fn"].as_str(), v["args"].as_array()) else {
                skipped += 1;
                continue;
            };
            let expected = ["expected", "output", "result"]
                .iter()
                .find_map(|k| v.get(*k));
            match (run(name, args), expected) {
                (Some(got), Some(want)) if &got == want => ok += 1,
                (Some(got), Some(want)) => failures.push(format!(
                    "{}:{} {name}\n  got  {got}\n  want {want}",
                    f.display(),
                    i + 1
                )),
                _ => skipped += 1,
            }
        }
    }
    println!(
        "golden: {ok} ok, {skipped} skipped, {} failed",
        failures.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
