//! Server-initiated requests (approvals, questions) and the replies they expect.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandApprovalParams {
    pub item_id: String,
    /// Set when several approval callbacks belong to one item; it names the callback.
    #[serde(default)]
    pub approval_id: Option<String>,
    pub thread_id: String,
    pub turn_id: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub command_actions: Vec<Value>,
    #[serde(default)]
    pub available_decisions: Vec<Value>,
    #[serde(default)]
    pub proposed_execpolicy_amendment: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeApprovalParams {
    pub item_id: String,
    pub thread_id: String,
    pub turn_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct QuestionOption {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQuestion {
    pub id: String,
    #[serde(default)]
    pub header: String,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub is_other: bool,
    #[serde(default)]
    pub is_secret: bool,
    #[serde(default)]
    pub options: Option<Vec<QuestionOption>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolUserInputParams {
    pub item_id: String,
    pub thread_id: String,
    pub turn_id: String,
    #[serde(default)]
    pub questions: Vec<UserInputQuestion>,
    #[serde(default)]
    pub is_blocking: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpElicitationParams {
    pub thread_id: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    pub server_name: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub requested_schema: Option<Value>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub elicitation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerRequest {
    CommandApproval(CommandApprovalParams),
    FileChangeApproval(FileChangeApprovalParams),
    ToolUserInput(ToolUserInputParams),
    McpElicitation(McpElicitationParams),
    Unknown {
        method: String,
        params: Value,
    },
    Invalid {
        method: String,
        params: Value,
        error: String,
    },
}

impl ServerRequest {
    pub fn parse(method: &str, params: &Value) -> ServerRequest {
        fn de<T: serde::de::DeserializeOwned>(
            method: &str,
            params: &Value,
            wrap: impl FnOnce(T) -> ServerRequest,
        ) -> ServerRequest {
            match T::deserialize(params) {
                Ok(v) => wrap(v),
                Err(e) => ServerRequest::Invalid {
                    method: method.to_string(),
                    params: params.clone(),
                    error: e.to_string(),
                },
            }
        }
        match method {
            "item/commandExecution/requestApproval" => {
                de(method, params, ServerRequest::CommandApproval)
            }
            "item/fileChange/requestApproval" => {
                de(method, params, ServerRequest::FileChangeApproval)
            }
            // `tool/requestUserInput` is the legacy name older Codex builds still use.
            "item/tool/requestUserInput" | "tool/requestUserInput" => {
                de(method, params, ServerRequest::ToolUserInput)
            }
            "mcpServer/elicitation/request" => de(method, params, ServerRequest::McpElicitation),
            _ => ServerRequest::Unknown {
                method: method.to_string(),
                params: params.clone(),
            },
        }
    }
}

/// Decision for command / file-change approvals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalDecision {
    Accept,
    AcceptForSession,
    /// Deny; the agent continues the turn.
    Decline,
    /// Deny and interrupt the turn.
    Cancel,
}

pub fn approval_response(decision: ApprovalDecision) -> Value {
    json!({ "decision": decision })
}

/// `answers`: question id -> selected labels / free text.
pub fn tool_user_input_response(answers: &Map<String, Value>) -> Value {
    let wrapped: Map<String, Value> = answers
        .iter()
        .map(|(id, v)| {
            let list = match v {
                Value::Array(a) => Value::Array(a.clone()),
                other => Value::Array(vec![other.clone()]),
            };
            (id.clone(), json!({ "answers": list }))
        })
        .collect();
    json!({ "answers": wrapped })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ElicitationAction {
    Accept,
    Decline,
    Cancel,
}

pub fn mcp_elicitation_response(action: ElicitationAction, content: Option<Value>) -> Value {
    json!({ "action": action, "content": content, "_meta": null })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_wire_shape() {
        assert_eq!(
            approval_response(ApprovalDecision::AcceptForSession),
            json!({"decision":"acceptForSession"})
        );
        assert_eq!(
            approval_response(ApprovalDecision::Cancel),
            json!({"decision":"cancel"})
        );
    }

    #[test]
    fn parses_command_approval_with_extra_fields() {
        let p = json!({"kind":"command","threadId":"t","turnId":"u","itemId":"i","command":"ls","cwd":"/x","availableDecisions":["accept"]});
        match ServerRequest::parse("item/commandExecution/requestApproval", &p) {
            ServerRequest::CommandApproval(c) => assert_eq!(c.command.as_deref(), Some("ls")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            ServerRequest::parse("item/permissions/requestApproval", &p),
            ServerRequest::Unknown { .. }
        ));
    }
}
