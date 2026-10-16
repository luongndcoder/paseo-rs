//! Tool result text synthesis and schema serialization (`tools/paseo-tool-serialization.ts`,
//! the `tools/list` shape of the MCP SDK).

use serde_json::{json, Map, Value};

use crate::types::{ToolDef, ToolResult};

const DRAFT_07: &str = "http://json-schema.org/draft-07/schema#";

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// `formatStructuredContentForModel`: per array key `<k>_count=N`, `<k>_ids=a,b` when every item
/// has a non-empty string `id`, a blank line, then the pretty JSON.
fn format_structured_content_for_model(structured: &Value) -> String {
    let Value::Object(record) = structured else {
        return pretty(structured);
    };
    let mut summary: Vec<String> = Vec::new();
    for (key, value) in record {
        let Value::Array(items) = value else {
            continue;
        };
        summary.push(format!("{key}_count={}", items.len()));
        let ids: Vec<&str> = items
            .iter()
            .filter_map(|item| match item {
                Value::Object(map) => map.get("id").and_then(Value::as_str),
                _ => None,
            })
            .filter(|id| !id.is_empty())
            .collect();
        if ids.len() == items.len() && !ids.is_empty() {
            summary.push(format!("{key}_ids={}", ids.join(",")));
        }
    }
    let json = pretty(structured);
    if summary.is_empty() {
        json
    } else {
        format!("{}\n\n{json}", summary.join("\n"))
    }
}

/// `addModelVisibleStructuredContent`: results with structured content and no content blocks get a
/// text block so models that only read `content` still see the data.
pub fn add_model_visible_structured_content(result: ToolResult) -> ToolResult {
    let Some(structured) = &result.structured_content else {
        return result;
    };
    if !result.content.is_empty() {
        return result;
    }
    let text = format_structured_content_for_model(structured);
    ToolResult {
        content: vec![json!({"type": "text", "text": text})],
        ..result
    }
}

/// `serializePaseoToolInputParameters`.
pub fn serialize_tool_input_parameters(tool: &ToolDef) -> Value {
    match &tool.input_schema {
        Value::Object(_) => tool.input_schema.clone(),
        _ => json!({"type": "object", "properties": {}}),
    }
}

/// The `inputSchema` the MCP SDK serves in `tools/list`: the same schema with `$schema` placed
/// before `additionalProperties`, or last when there is none (the SDK converts the zod object
/// itself, so key order differs from `serializePaseoToolInputParameters`).
pub fn mcp_input_schema(schema: &Value) -> Value {
    let Value::Object(source) = schema else {
        return json!({"type": "object", "properties": {}, "$schema": DRAFT_07});
    };
    let mut out = Map::new();
    for (key, value) in source {
        if key == "$schema" {
            continue;
        }
        if key == "additionalProperties" {
            out.insert("$schema".into(), json!(DRAFT_07));
        }
        out.insert(key.clone(), value.clone());
    }
    if !out.contains_key("$schema") {
        out.insert("$schema".into(), json!(DRAFT_07));
    }
    Value::Object(out)
}

/// One entry of the MCP `tools/list` result. `execution.taskSupport` is `forbidden` because the
/// SDK stamps it on tools registered without task support.
pub fn mcp_tool_listing(tool: &ToolDef) -> Value {
    let mut map = Map::new();
    map.insert("name".into(), json!(tool.name));
    if let Some(title) = &tool.title {
        map.insert("title".into(), json!(title));
    }
    map.insert("description".into(), json!(tool.description));
    map.insert(
        "inputSchema".into(),
        mcp_input_schema(&serialize_tool_input_parameters(tool)),
    );
    map.insert("execution".into(), json!({"taskSupport": "forbidden"}));
    Value::Object(map)
}
