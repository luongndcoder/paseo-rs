//! Tool result and definition types (`agent/tools/types.ts`).

use serde_json::{json, Map, Value};

/// `PaseoToolResult`. `structured_content: Some(Value::Null)` is a defined `null`, distinct from
/// `None` (absent).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolResult {
    pub content: Vec<Value>,
    pub structured_content: Option<Value>,
    pub is_error: Option<bool>,
}

impl ToolResult {
    /// `{content: [], structuredContent}`: the shape nearly every tool returns; the model-visible
    /// text is synthesized from the structured content.
    pub fn structured(value: Value) -> Self {
        Self {
            content: Vec::new(),
            structured_content: Some(value),
            is_error: None,
        }
    }

    /// The error result the MCP SDK builds for a thrown error (`createToolError`).
    pub fn error_text(message: impl Into<String>) -> Self {
        Self {
            content: vec![json!({"type": "text", "text": message.into()})],
            structured_content: None,
            is_error: Some(true),
        }
    }

    /// JSON as `toMcpToolResult` emits it: `content`, `structuredContent`, `isError`.
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("content".into(), Value::Array(self.content.clone()));
        if let Some(structured) = &self.structured_content {
            map.insert("structuredContent".into(), structured.clone());
        }
        if let Some(is_error) = self.is_error {
            map.insert("isError".into(), Value::Bool(is_error));
        }
        Value::Object(map)
    }
}

/// `PaseoToolDefinition` minus the handler: what `tools/list` and native host tools expose.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub title: Option<String>,
    pub description: String,
    /// The `inputSchema` as `serializePaseoToolInputParameters` returns it (golden snapshot).
    pub input_schema: Value,
    /// `outputSchema` as JSON schema; `None` for tools without one (browser tools).
    pub output_schema: Option<Value>,
}
