//! Native Rust client for Agent Client Protocol (ACP) agents.
//!
//! Layers, bottom up:
//! - [`protocol`]: wire types and JSON-RPC framing, hand-rolled to match `@agentclientprotocol/sdk` 0.17.
//! - [`transport`] / [`connection`]: stdio process transport (with [`tee`] recording) and a request multiplexer.
//! - [`client`] / [`session`]: provider-agnostic surface (`AcpClient`, `AcpSession`).
//! - [`providers`]: per-agent profiles (Copilot, Cursor, Kimi, Kiro, Trae, generic).
//! - [`replay`]: fixture-driven fake agent for tests.

pub mod agent;
pub mod client;
pub mod connection;
pub mod derive;
pub mod error;
pub mod events;
pub mod host;
pub mod launch;
pub mod permission;
pub mod profile;
pub mod protocol;
pub mod providers;
pub mod replay;
pub mod session;
mod session_settings;
pub mod tee;
pub mod transport;
pub mod util;

pub use client::AcpClient;
pub use error::{Error, Result};
pub use events::SessionEvent;
pub use profile::{AgentProfile, ProviderHooks};
pub use session::AcpSession;
