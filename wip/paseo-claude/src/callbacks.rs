//! Host callbacks the CLI can call back into: tool permission, hooks, in-process MCP servers,
//! MCP elicitation.

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::BoxError;
use crate::protocol::{
    CanUseToolRequest, ElicitationRequest, HookInput, HookJsonOutput, PermissionUpdate,
};

/// Answer to a `can_use_tool` request.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionDecision {
    Allow {
        /// `None` echoes the original tool input (the CLI is only known to accept allows
        /// that carry `updatedInput`).
        updated_input: Option<Value>,
        updated_permissions: Option<Vec<PermissionUpdate>>,
    },
    Deny {
        message: String,
        /// Also interrupt the running turn.
        interrupt: bool,
    },
}

impl PermissionDecision {
    pub fn allow() -> Self {
        Self::Allow {
            updated_input: None,
            updated_permissions: None,
        }
    }

    pub fn allow_with_input(updated_input: Value) -> Self {
        Self::Allow {
            updated_input: Some(updated_input),
            updated_permissions: None,
        }
    }

    pub fn deny(message: impl Into<String>) -> Self {
        Self::Deny {
            message: message.into(),
            interrupt: false,
        }
    }

    pub fn deny_and_interrupt(message: impl Into<String>) -> Self {
        Self::Deny {
            message: message.into(),
            interrupt: true,
        }
    }
}

/// `canUseTool`. `cancel` fires when the CLI withdraws the request (`control_cancel_request`)
/// or the session closes; the future is dropped right after.
#[async_trait]
pub trait CanUseTool: Send + Sync + 'static {
    async fn can_use_tool(
        &self,
        request: CanUseToolRequest,
        cancel: CancellationToken,
    ) -> Result<PermissionDecision, BoxError>;
}

/// A hook registered at `initialize` under an opaque callback id.
#[async_trait]
pub trait HookCallback: Send + Sync + 'static {
    /// Return `HookJsonOutput::default()` (`{}`) for a no-op.
    async fn call(
        &self,
        input: HookInput,
        tool_use_id: Option<String>,
        cancel: CancellationToken,
    ) -> Result<HookJsonOutput, BoxError>;
}

/// MCP tool descriptor (`tools/list`).
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    /// JSON Schema object.
    pub input_schema: Value,
    /// MCP `annotations` (e.g. `{"readOnlyHint": true}`).
    pub annotations: Option<Value>,
    /// MCP `execution`. `None` advertises `{"taskSupport": "forbidden"}`, which is what the
    /// TS SDK's MCP server library emits for every tool (golden fixture 05).
    pub execution: Option<Value>,
}

impl McpTool {
    pub fn new(name: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            description: None,
            input_schema,
            annotations: None,
            execution: None,
        }
    }

    pub fn description(mut self, d: impl Into<String>) -> Self {
        self.description = Some(d.into());
        self
    }
}

/// `tools/call` result.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct McpToolResult {
    /// MCP content blocks, e.g. `{"type":"text","text":"..."}`.
    pub content: Vec<Value>,
    pub is_error: bool,
    pub structured_content: Option<Value>,
}

impl McpToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![serde_json::json!({"type": "text", "text": text.into()})],
            ..Self::default()
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::text(text)
        }
    }
}

/// Context of a `tools/call`.
#[derive(Debug, Clone)]
pub struct McpCallContext {
    /// `params._meta["claudecode/toolUseId"]`.
    pub tool_use_id: Option<String>,
    pub cancel: CancellationToken,
}

/// In-process MCP server answering the CLI's tunnelled JSON-RPC.
#[async_trait]
pub trait SdkMcpServer: Send + Sync + 'static {
    fn name(&self) -> &str;

    fn version(&self) -> &str {
        "1.0.0"
    }

    async fn list_tools(&self) -> Result<Vec<McpTool>, BoxError>;

    /// An `Err` is reported to the model as an `isError` tool result.
    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        ctx: McpCallContext,
    ) -> Result<McpToolResult, BoxError>;
}

/// MCP elicitation (a server asking the user for input). Return the raw MCP `ElicitResult`:
/// `{"action": "accept"|"decline"|"cancel", "content": {...}}`. Without a handler the session
/// answers `{"action": "decline"}`.
#[async_trait]
pub trait ElicitationHandler: Send + Sync + 'static {
    async fn elicit(
        &self,
        request: ElicitationRequest,
        cancel: CancellationToken,
    ) -> Result<Value, BoxError>;
}
