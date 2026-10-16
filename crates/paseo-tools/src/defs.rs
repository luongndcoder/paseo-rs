//! Golden tool definitions: title, description and the JSON schemas the models see.
//!
//! `data/tool-defs.json` is derived from `fixtures/p4/tool-catalog/tools-list.json` by
//! `data/derive-tool-defs.py`. The schemas are snapshots of the TS output, never generated from
//! Rust types (docs/p4-core-spec.md section 7). `tests/golden.rs` re-derives the file from the
//! fixture and fails when they drift.

use std::sync::OnceLock;

use serde_json::Value;

use crate::types::ToolDef;

const TOOL_DEFS_JSON: &str = include_str!("../data/tool-defs.json");

struct Entry {
    name: String,
    title: Option<String>,
    description: String,
    input: Value,
    input_caller: Option<Value>,
    output: Option<Value>,
}

fn entries() -> &'static [Entry] {
    static ENTRIES: OnceLock<Vec<Entry>> = OnceLock::new();
    ENTRIES.get_or_init(|| {
        let Value::Array(items) = serde_json::from_str(TOOL_DEFS_JSON).expect("tool-defs.json")
        else {
            panic!("tool-defs.json is not an array");
        };
        items
            .into_iter()
            .map(|item| {
                let text = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_owned);
                Entry {
                    name: text("name").expect("name"),
                    title: text("title"),
                    description: text("description").expect("description"),
                    input: item.get("input").cloned().expect("input"),
                    input_caller: item.get("inputCaller").cloned(),
                    output: item.get("output").filter(|v| !v.is_null()).cloned(),
                }
            })
            .collect()
    })
}

/// Names of every tool in registration order.
pub fn tool_names() -> Vec<&'static str> {
    entries().iter().map(|e| e.name.as_str()).collect()
}

/// The definition of `name`. `caller` selects the agent-scoped variant of the input schema for
/// the tools whose schema depends on `callerAgentId`.
pub fn tool_def(name: &str, caller: bool) -> Option<ToolDef> {
    let entry = entries().iter().find(|e| e.name == name)?;
    let input = if caller {
        entry.input_caller.as_ref().unwrap_or(&entry.input)
    } else {
        &entry.input
    };
    Some(ToolDef {
        name: entry.name.clone(),
        title: entry.title.clone(),
        description: entry.description.clone(),
        input_schema: input.clone(),
        output_schema: entry.output.clone(),
    })
}
