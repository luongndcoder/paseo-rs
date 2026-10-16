//! Tool calls replayed against the TS catalog's recorded behavior (`tests/data/behavior-vectors.json`,
//! generator `gen-behavior-vectors.mts`): the same JSON-described stubs on both sides, the response
//! and the recorded stub calls compared as compact JSON (so key order counts).

mod common;

use common::profile::ProfileCore;
use paseo_tools::{RuntimeCtx, ToolCatalogFactory};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap()
}

#[tokio::test]
async fn tool_behavior_matches_the_ts_catalog() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/behavior-vectors.json"
    ))
    .unwrap();
    let vectors: Value = serde_json::from_str(&text).unwrap();
    let profiles = vectors["profiles"].as_object().unwrap();
    let cases = vectors["cases"].as_array().unwrap();
    let mut failures = Vec::new();
    for case in cases {
        let profile = &profiles[case["profile"].as_str().unwrap()];
        let broker = case
            .get("brokerPayload")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"ok": true, "result": {"command": "back"}}));
        let core = Arc::new(ProfileCore::new(profile, broker));
        let runtime = RuntimeCtx {
            caller_agent_id: core.caller_id(),
            ..RuntimeCtx::default()
        };
        let catalog = ToolCatalogFactory::new(core.clone()).build(runtime).await;
        let response = catalog
            .call_tool_for_mcp(
                case["tool"].as_str().unwrap(),
                Some(&case["args"]),
                CancellationToken::new(),
            )
            .await;
        let label = format!("{} {}", case["name"], compact(&case["args"]));
        if compact(&response) != compact(&case["response"]) {
            failures.push(format!(
                "{label}\n  got  {}\n  want {}",
                compact(&response),
                compact(&case["response"])
            ));
        }
        let calls = Value::Array(core.calls());
        if compact(&calls) != compact(&case["sideEffects"]) {
            failures.push(format!(
                "{label} (calls)\n  got  {}\n  want {}",
                compact(&calls),
                compact(&case["sideEffects"])
            ));
        }
    }
    assert!(cases.len() > 400, "{} cases", cases.len());
    assert!(
        failures.is_empty(),
        "{} of {} cases differ; first {}:\n{}",
        failures.len(),
        cases.len(),
        failures.len().min(12),
        failures
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
