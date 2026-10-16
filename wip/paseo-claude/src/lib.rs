//! Native Rust client for the Claude Code CLI.
//!
//! Spawns `claude` with stream-json stdio and speaks its control protocol, replacing what
//! `@anthropic-ai/claude-agent-sdk` does. Wire types live in [`protocol`] (see `PROTOCOL.md`).
//!
//! ```no_run
//! # async fn demo() -> paseo_claude::Result<()> {
//! use futures_core::Stream;
//! use paseo_claude::{ClaudeOptions, SdkMessage, Session};
//!
//! let mut session = Session::start(ClaudeOptions::new().model("haiku"))?;
//! session.send_user_message("Say hi")?;
//! while let Some(msg) = session.next_message().await {
//!     if let SdkMessage::Result(_) = msg? {
//!         break;
//!     }
//! }
//! session.close().await;
//! # Ok(()) }
//! ```

pub mod callbacks;
mod dispatch;
pub mod error;
mod mcp_bridge;
pub mod memory;
pub mod message;
pub mod options;
pub mod process;
pub mod protocol;
pub mod session;
pub mod transport;

#[cfg(test)]
mod tests;

pub use callbacks::{
    CanUseTool, ElicitationHandler, HookCallback, McpCallContext, McpTool, McpToolResult,
    PermissionDecision, SdkMcpServer,
};
pub use error::{BoxError, Error, Result};
pub use memory::{InMemoryPeer, InMemoryTransport};
pub use message::{SdkMessage, UserInput};
pub use options::{ClaudeOptions, McpServerSpec, Prepared, SystemPrompt, SDK_VERSION};
pub use process::{ProcessTransport, SpawnConfig};
pub use session::{MessageStream, Session, SessionHandle};
pub use transport::{ExitInfo, Transport, TransportEvent, TransportIo};
