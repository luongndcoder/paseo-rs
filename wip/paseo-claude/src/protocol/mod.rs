//! Wire types for the Claude Code stream-json stdio protocol. See `PROTOCOL.md`.
//!
//! Design: typed where Paseo needs it, lossless everywhere. Unknown fields land in `extra`
//! maps, unknown tags in `Unknown(Value)` variants, and [`Line::parse`] reserializes to the
//! exact input JSON.

pub mod argv;
pub mod content;
pub mod control;
pub mod hooks;
pub mod line;
pub mod lossless;
pub mod mcp;
pub mod messages;
pub mod permission;
pub mod responses;
pub mod stream;

#[cfg(test)]
mod tests;

pub use argv::{build_argv, ArgvOptions, ThinkingConfig};
pub use content::{ApiMessage, ContentBlock, ImageSource, MessageParam, UserContent};
pub use control::{
    AgentDefinition, CanUseToolRequest, ControlCancelRequest, ControlRequest, ControlRequestBody,
    ControlResponse, ControlResponseBody, ElicitationRequest, ErrorResponse, InitializeRequest,
    SuccessResponse,
};
pub use hooks::{HookCallbackMatcher, HookEvent, HookInput, HookInputBase, HookJsonOutput};
pub use line::Line;
pub use lossless::{parse_lossless, Extra, OpenEnum};
pub use mcp::{McpConfigFile, McpServerConfig, McpServerStatus};
pub use messages::{
    AssistantMessage, CommandLifecycle, InitMessage, RateLimitEvent, ResultMessage, StreamEvent,
    SystemMessage, ToolProgress, UserMessage,
};
pub use permission::{
    PermissionBehavior, PermissionDestination, PermissionMode, PermissionResult, PermissionUpdate,
};
pub use responses::{parse_response, InitializeResponse, InterruptResponse, RewindFilesResult};
pub use stream::{Delta, StreamEventKind};
