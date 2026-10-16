//! Typed views of server notifications.
//!
//! [`ServerNotification::parse`] never fails: a method this crate does not model becomes
//! [`ServerNotification::Unknown`], and a modelled method whose payload does not fit becomes
//! [`ServerNotification::Invalid`]. Both keep the original `params`.

use serde::Deserialize;
use serde_json::Value;

use super::items::ThreadItem;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thread {
    pub id: String,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub forked_from_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub preview: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_provider: Option<String>,
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub updated_at: Option<i64>,
    #[serde(default)]
    pub status: Option<Value>,
    #[serde(default)]
    pub turns: Vec<Turn>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
    InProgress,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnError {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub codex_error_info: Option<Value>,
    #[serde(default)]
    pub additional_details: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: String,
    pub status: TurnStatus,
    #[serde(default)]
    pub error: Option<TurnError>,
    #[serde(default)]
    pub items: Vec<ThreadItem>,
    #[serde(default)]
    pub items_view: Option<String>,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub completed_at: Option<i64>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageBreakdown {
    #[serde(default)]
    pub total_tokens: i64,
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub cached_input_tokens: i64,
    #[serde(default)]
    pub cache_write_input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub reasoning_output_tokens: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTokenUsage {
    #[serde(default)]
    pub total: TokenUsageBreakdown,
    #[serde(default)]
    pub last: TokenUsageBreakdown,
    #[serde(default)]
    pub model_context_window: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    #[serde(default)]
    pub step: String,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorInfo {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub codex_error_info: Option<Value>,
    #[serde(default)]
    pub additional_details: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerNotification {
    ThreadStarted {
        thread: Thread,
    },
    ThreadStatusChanged {
        thread_id: String,
        status: Value,
    },
    ThreadSettingsUpdated {
        thread_id: String,
        settings: Value,
    },
    ThreadTokenUsageUpdated {
        thread_id: String,
        turn_id: Option<String>,
        token_usage: ThreadTokenUsage,
    },
    ThreadCompacted {
        thread_id: String,
        turn_id: Option<String>,
    },
    TurnStarted {
        thread_id: String,
        turn: Turn,
    },
    TurnCompleted {
        thread_id: String,
        turn: Turn,
    },
    TurnPlanUpdated {
        thread_id: String,
        turn_id: String,
        explanation: Option<String>,
        plan: Vec<PlanStep>,
    },
    TurnDiffUpdated {
        thread_id: String,
        turn_id: String,
        diff: String,
    },
    ItemStarted {
        thread_id: String,
        turn_id: String,
        item: ThreadItem,
    },
    ItemCompleted {
        thread_id: String,
        turn_id: String,
        item: ThreadItem,
    },
    AgentMessageDelta {
        thread_id: String,
        turn_id: String,
        item_id: String,
        delta: String,
    },
    ReasoningSummaryTextDelta {
        thread_id: String,
        turn_id: String,
        item_id: String,
        delta: String,
        summary_index: i64,
    },
    ReasoningTextDelta {
        thread_id: String,
        turn_id: String,
        item_id: String,
        delta: String,
        content_index: i64,
    },
    CommandExecutionOutputDelta {
        thread_id: String,
        turn_id: String,
        item_id: String,
        delta: String,
    },
    FileChangeOutputDelta {
        thread_id: String,
        turn_id: String,
        item_id: String,
        delta: String,
    },
    TerminalInteraction {
        thread_id: String,
        turn_id: String,
        item_id: String,
        process_id: Option<String>,
        stdin: Option<String>,
    },
    ServerRequestResolved {
        thread_id: String,
        request_id: super::RequestId,
    },
    McpServerStartupStatusUpdated {
        thread_id: Option<String>,
        name: String,
        status: String,
        error: Option<String>,
    },
    Error {
        thread_id: Option<String>,
        turn_id: Option<String>,
        error: ErrorInfo,
        will_retry: bool,
    },
    Warning {
        thread_id: Option<String>,
        message: String,
    },
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

// Wire payload shapes. Kept private; the public enum above is the stable surface.
mod wire {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ThreadStarted {
        pub thread: Thread,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ThreadStatus {
        pub thread_id: String,
        #[serde(default)]
        pub status: Value,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ThreadSettings {
        pub thread_id: String,
        #[serde(default)]
        pub thread_settings: Value,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct TokenUsage {
        pub thread_id: String,
        #[serde(default)]
        pub turn_id: Option<String>,
        pub token_usage: ThreadTokenUsage,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Compacted {
        pub thread_id: String,
        #[serde(default)]
        pub turn_id: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct TurnEvent {
        pub thread_id: String,
        pub turn: Turn,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct PlanUpdated {
        pub thread_id: String,
        pub turn_id: String,
        #[serde(default)]
        pub explanation: Option<String>,
        #[serde(default)]
        pub plan: Vec<PlanStep>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct DiffUpdated {
        pub thread_id: String,
        pub turn_id: String,
        #[serde(default)]
        pub diff: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ItemEvent {
        pub thread_id: String,
        pub turn_id: String,
        pub item: ThreadItem,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Delta {
        pub thread_id: String,
        pub turn_id: String,
        pub item_id: String,
        #[serde(default)]
        pub delta: String,
        #[serde(default)]
        pub summary_index: i64,
        #[serde(default)]
        pub content_index: i64,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Terminal {
        pub thread_id: String,
        pub turn_id: String,
        pub item_id: String,
        #[serde(default)]
        pub process_id: Option<String>,
        #[serde(default)]
        pub stdin: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Resolved {
        pub thread_id: String,
        pub request_id: super::super::RequestId,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct McpStatus {
        #[serde(default)]
        pub thread_id: Option<String>,
        pub name: String,
        pub status: String,
        #[serde(default)]
        pub error: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct ErrorEvent {
        #[serde(default)]
        pub thread_id: Option<String>,
        #[serde(default)]
        pub turn_id: Option<String>,
        pub error: ErrorInfo,
        #[serde(default)]
        pub will_retry: bool,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct WarningEvent {
        #[serde(default)]
        pub thread_id: Option<String>,
        #[serde(default)]
        pub message: String,
    }
}

fn de<T: serde::de::DeserializeOwned>(params: &Value) -> Result<T, String> {
    T::deserialize(params).map_err(|e| e.to_string())
}

impl ServerNotification {
    pub fn parse(method: &str, params: &Value) -> ServerNotification {
        match Self::try_parse(method, params) {
            Ok(Some(n)) => n,
            Ok(None) => ServerNotification::Unknown {
                method: method.to_string(),
                params: params.clone(),
            },
            Err(error) => ServerNotification::Invalid {
                method: method.to_string(),
                params: params.clone(),
                error,
            },
        }
    }

    fn try_parse(method: &str, p: &Value) -> Result<Option<ServerNotification>, String> {
        use ServerNotification as N;
        Ok(Some(match method {
            "thread/started" => N::ThreadStarted {
                thread: de::<wire::ThreadStarted>(p)?.thread,
            },
            "thread/status/changed" => {
                let w = de::<wire::ThreadStatus>(p)?;
                N::ThreadStatusChanged {
                    thread_id: w.thread_id,
                    status: w.status,
                }
            }
            "thread/settings/updated" => {
                let w = de::<wire::ThreadSettings>(p)?;
                N::ThreadSettingsUpdated {
                    thread_id: w.thread_id,
                    settings: w.thread_settings,
                }
            }
            "thread/tokenUsage/updated" => {
                let w = de::<wire::TokenUsage>(p)?;
                N::ThreadTokenUsageUpdated {
                    thread_id: w.thread_id,
                    turn_id: w.turn_id,
                    token_usage: w.token_usage,
                }
            }
            "thread/compacted" => {
                let w = de::<wire::Compacted>(p)?;
                N::ThreadCompacted {
                    thread_id: w.thread_id,
                    turn_id: w.turn_id,
                }
            }
            "turn/started" => {
                let w = de::<wire::TurnEvent>(p)?;
                N::TurnStarted {
                    thread_id: w.thread_id,
                    turn: w.turn,
                }
            }
            "turn/completed" => {
                let w = de::<wire::TurnEvent>(p)?;
                N::TurnCompleted {
                    thread_id: w.thread_id,
                    turn: w.turn,
                }
            }
            "turn/plan/updated" => {
                let w = de::<wire::PlanUpdated>(p)?;
                N::TurnPlanUpdated {
                    thread_id: w.thread_id,
                    turn_id: w.turn_id,
                    explanation: w.explanation,
                    plan: w.plan,
                }
            }
            "turn/diff/updated" => {
                let w = de::<wire::DiffUpdated>(p)?;
                N::TurnDiffUpdated {
                    thread_id: w.thread_id,
                    turn_id: w.turn_id,
                    diff: w.diff,
                }
            }
            "item/started" => {
                let w = de::<wire::ItemEvent>(p)?;
                N::ItemStarted {
                    thread_id: w.thread_id,
                    turn_id: w.turn_id,
                    item: w.item,
                }
            }
            "item/completed" => {
                let w = de::<wire::ItemEvent>(p)?;
                N::ItemCompleted {
                    thread_id: w.thread_id,
                    turn_id: w.turn_id,
                    item: w.item,
                }
            }
            "item/agentMessage/delta" => {
                let w = de::<wire::Delta>(p)?;
                N::AgentMessageDelta {
                    thread_id: w.thread_id,
                    turn_id: w.turn_id,
                    item_id: w.item_id,
                    delta: w.delta,
                }
            }
            "item/reasoning/summaryTextDelta" => {
                let w = de::<wire::Delta>(p)?;
                N::ReasoningSummaryTextDelta {
                    thread_id: w.thread_id,
                    turn_id: w.turn_id,
                    item_id: w.item_id,
                    delta: w.delta,
                    summary_index: w.summary_index,
                }
            }
            "item/reasoning/textDelta" => {
                let w = de::<wire::Delta>(p)?;
                N::ReasoningTextDelta {
                    thread_id: w.thread_id,
                    turn_id: w.turn_id,
                    item_id: w.item_id,
                    delta: w.delta,
                    content_index: w.content_index,
                }
            }
            "item/commandExecution/outputDelta" => {
                let w = de::<wire::Delta>(p)?;
                N::CommandExecutionOutputDelta {
                    thread_id: w.thread_id,
                    turn_id: w.turn_id,
                    item_id: w.item_id,
                    delta: w.delta,
                }
            }
            "item/fileChange/outputDelta" => {
                let w = de::<wire::Delta>(p)?;
                N::FileChangeOutputDelta {
                    thread_id: w.thread_id,
                    turn_id: w.turn_id,
                    item_id: w.item_id,
                    delta: w.delta,
                }
            }
            "item/commandExecution/terminalInteraction" => {
                let w = de::<wire::Terminal>(p)?;
                N::TerminalInteraction {
                    thread_id: w.thread_id,
                    turn_id: w.turn_id,
                    item_id: w.item_id,
                    process_id: w.process_id,
                    stdin: w.stdin,
                }
            }
            "serverRequest/resolved" => {
                let w = de::<wire::Resolved>(p)?;
                N::ServerRequestResolved {
                    thread_id: w.thread_id,
                    request_id: w.request_id,
                }
            }
            "mcpServer/startupStatus/updated" => {
                let w = de::<wire::McpStatus>(p)?;
                N::McpServerStartupStatusUpdated {
                    thread_id: w.thread_id,
                    name: w.name,
                    status: w.status,
                    error: w.error,
                }
            }
            "error" => {
                let w = de::<wire::ErrorEvent>(p)?;
                N::Error {
                    thread_id: w.thread_id,
                    turn_id: w.turn_id,
                    error: w.error,
                    will_retry: w.will_retry,
                }
            }
            "warning" => {
                let w = de::<wire::WarningEvent>(p)?;
                N::Warning {
                    thread_id: w.thread_id,
                    message: w.message,
                }
            }
            _ => return Ok(None),
        }))
    }

    /// Thread the notification belongs to, when it names one. Used to separate the root thread
    /// from sub-agent threads.
    pub fn thread_id(&self) -> Option<&str> {
        use ServerNotification as N;
        match self {
            N::ThreadStarted { thread } => Some(&thread.id),
            N::ThreadStatusChanged { thread_id, .. }
            | N::ThreadSettingsUpdated { thread_id, .. }
            | N::ThreadTokenUsageUpdated { thread_id, .. }
            | N::ThreadCompacted { thread_id, .. }
            | N::TurnStarted { thread_id, .. }
            | N::TurnCompleted { thread_id, .. }
            | N::TurnPlanUpdated { thread_id, .. }
            | N::TurnDiffUpdated { thread_id, .. }
            | N::ItemStarted { thread_id, .. }
            | N::ItemCompleted { thread_id, .. }
            | N::AgentMessageDelta { thread_id, .. }
            | N::ReasoningSummaryTextDelta { thread_id, .. }
            | N::ReasoningTextDelta { thread_id, .. }
            | N::CommandExecutionOutputDelta { thread_id, .. }
            | N::FileChangeOutputDelta { thread_id, .. }
            | N::TerminalInteraction { thread_id, .. }
            | N::ServerRequestResolved { thread_id, .. } => Some(thread_id),
            N::McpServerStartupStatusUpdated { thread_id, .. }
            | N::Error { thread_id, .. }
            | N::Warning { thread_id, .. } => thread_id.as_deref(),
            N::Unknown { .. } | N::Invalid { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_and_invalid_keep_params() {
        let p = json!({"x":1});
        assert!(matches!(
            ServerNotification::parse("hook/started", &p),
            ServerNotification::Unknown { .. }
        ));
        match ServerNotification::parse("turn/started", &p) {
            ServerNotification::Invalid { params, .. } => assert_eq!(params, p),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_turn_completed() {
        let p = json!({"threadId":"t","turn":{"id":"u","items":[],"itemsView":"notLoaded","status":"interrupted","error":null,"startedAt":1,"completedAt":2,"durationMs":3}});
        match ServerNotification::parse("turn/completed", &p) {
            ServerNotification::TurnCompleted { thread_id, turn } => {
                assert_eq!(thread_id, "t");
                assert_eq!(turn.status, TurnStatus::Interrupted);
            }
            other => panic!("{other:?}"),
        }
    }
}
