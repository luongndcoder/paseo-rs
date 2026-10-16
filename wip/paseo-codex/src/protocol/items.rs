//! Thread items (`item/started`, `item/completed`, `thread/read` turns).
//!
//! An item keeps its full JSON in [`ThreadItem::raw`] and adds a typed view for the variants this
//! crate models. A type the crate does not know (or a payload that does not fit the typed view)
//! leaves `known` empty rather than failing the whole notification.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadItem {
    pub raw: Value,
    pub known: Option<KnownItem>,
}

impl ThreadItem {
    pub fn id(&self) -> Option<&str> {
        self.raw.get("id").and_then(Value::as_str)
    }

    /// The wire `type` string, e.g. `agentMessage`, `commandExecution`.
    pub fn item_type(&self) -> Option<&str> {
        self.raw.get("type").and_then(Value::as_str)
    }
}

impl Serialize for ThreadItem {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.raw.serialize(s)
    }
}

impl<'de> Deserialize<'de> for ThreadItem {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(d)?;
        let known = KnownItem::deserialize(&raw).ok();
        Ok(ThreadItem { raw, known })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum KnownItem {
    UserMessage {
        id: String,
        #[serde(default)]
        client_id: Option<String>,
        #[serde(default)]
        content: Vec<Value>,
    },
    AgentMessage {
        id: String,
        #[serde(default)]
        text: String,
        #[serde(default)]
        phase: Option<String>,
    },
    Reasoning {
        id: String,
        #[serde(default)]
        summary: Vec<String>,
        #[serde(default)]
        content: Vec<String>,
    },
    Plan {
        id: String,
        #[serde(default)]
        text: String,
    },
    CommandExecution {
        id: String,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        aggregated_output: Option<String>,
        #[serde(default)]
        exit_code: Option<i64>,
        #[serde(default)]
        duration_ms: Option<i64>,
        #[serde(default)]
        process_id: Option<String>,
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        command_actions: Vec<Value>,
    },
    FileChange {
        id: String,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        changes: Vec<Value>,
    },
    McpToolCall {
        id: String,
        #[serde(default)]
        server: Option<String>,
        #[serde(default)]
        tool: Option<String>,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        arguments: Value,
        #[serde(default)]
        result: Option<Value>,
        #[serde(default)]
        error: Option<Value>,
        #[serde(default)]
        duration_ms: Option<i64>,
    },
    WebSearch {
        id: String,
        #[serde(default)]
        query: Option<String>,
    },
    ContextCompaction {
        id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_and_unknown_items() {
        let item: ThreadItem = serde_json::from_value(json!({
            "type":"commandExecution","id":"e1","command":"ls","cwd":"/x","status":"inProgress",
            "commandActions":[{"type":"unknown","command":"ls"}],"aggregatedOutput":null,"exitCode":null
        }))
        .unwrap();
        assert_eq!(item.id(), Some("e1"));
        assert!(matches!(
            item.known,
            Some(KnownItem::CommandExecution { .. })
        ));

        let item: ThreadItem = serde_json::from_value(json!({"type":"holoDeck","id":"h"})).unwrap();
        assert!(item.known.is_none());
        assert_eq!(item.item_type(), Some("holoDeck"));
        assert_eq!(
            serde_json::to_value(&item).unwrap(),
            json!({"type":"holoDeck","id":"h"})
        );
    }
}
