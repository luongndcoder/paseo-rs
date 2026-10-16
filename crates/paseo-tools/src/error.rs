//! Errors of tool execution.

/// A tool handler failure (`throw new Error(message)` in TS). Over MCP it becomes an
/// `isError` result whose text is the message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ToolError(pub String);

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Failure of `ToolCatalog::execute_tool`, with the messages the TS catalog throws.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecuteError {
    /// `Paseo tool not found: <name>`.
    #[error("Paseo tool not found: {0}")]
    NotFound(String),
    /// `ZodError.message`: the pretty-printed issue list.
    #[error("{0}")]
    InvalidInput(String),
    #[error("{0}")]
    Tool(String),
}

impl From<ToolError> for ExecuteError {
    fn from(error: ToolError) -> Self {
        Self::Tool(error.0)
    }
}
