//! Requests this client sends. The RPC layer adds the `id`.

use serde::Serialize;
use serde_json::Value;

use super::types::ImageContent;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Command {
    Prompt {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        images: Option<Vec<ImageContent>>,
    },
    Steer {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        images: Option<Vec<ImageContent>>,
    },
    ClearQueue,
    Compact {
        #[serde(skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
    },
    SetAutoCompaction {
        enabled: bool,
    },
    Abort,
    GetState,
    GetMessages,
    GetAvailableModels,
    SetModel {
        provider: String,
        model_id: String,
    },
    SetThinkingLevel {
        level: String,
    },
    GetSessionStats,
    /// Pi.
    GetCommands,
    /// OMP.
    GetAvailableCommands,
    // ---- OMP only ----
    NegotiateProtocol {
        protocol_version: u64,
    },
    SetFastMode {
        enabled: bool,
    },
    SetSubagentSubscription {
        level: String,
    },
    SetHostTools {
        tools: Vec<Value>,
    },
    Branch {
        entry_id: String,
    },
    GetBranchMessages,
    Handoff {
        #[serde(skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
    },
}

impl Command {
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("Command always serializes")
    }

    /// Wire name, e.g. `get_state`.
    pub fn name(&self) -> String {
        self.to_value()["type"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serializes_like_the_ts_client() {
        assert_eq!(
            Command::SetModel {
                provider: "nvidia".into(),
                model_id: "z-ai/glm".into()
            }
            .to_value(),
            json!({"type":"set_model","provider":"nvidia","modelId":"z-ai/glm"})
        );
        assert_eq!(
            Command::Prompt {
                message: "hi".into(),
                images: Some(vec![ImageContent::new("AAAA", "image/png")])
            }
            .to_value(),
            json!({"type":"prompt","message":"hi",
                   "images":[{"type":"image","data":"AAAA","mimeType":"image/png"}]})
        );
        assert_eq!(
            Command::ClearQueue.to_value(),
            json!({"type":"clear_queue"})
        );
        assert_eq!(
            Command::NegotiateProtocol {
                protocol_version: 2
            }
            .to_value(),
            json!({"type":"negotiate_protocol","protocolVersion":2})
        );
        assert_eq!(
            Command::Compact {
                custom_instructions: None
            }
            .to_value(),
            json!({"type":"compact"})
        );
    }
}
