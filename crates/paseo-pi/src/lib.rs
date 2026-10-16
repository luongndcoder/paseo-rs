//! Native Rust client for the Pi and Oh My Pi (OMP) local RPC processes.
//!
//! Layers, bottom to top:
//!
//! - [`transport`] / [`frame`]: JSONL over a child process's stdio, including OMP's chunked
//!   protocol v2 frames.
//! - [`rpc`]: request id correlation, timeouts, event fan-out.
//! - [`protocol`]: typed commands, events and messages with `Value` fallbacks.
//! - [`session`]: [`Session`], the provider-specific raw protocol layer.
//! - [`agent`]: the provider-agnostic [`AgentClient`] / [`AgentSession`] surface that mirrors
//!   `agent-sdk-types.ts`, with raw provider events (no timeline mapping).
//! - [`testing`]: a replay transport driven by recorded JSONL traffic.

pub mod agent;
pub mod error;
pub mod flavor;
pub mod frame;
pub mod launch;
pub mod protocol;
pub mod rpc;
pub mod session;
pub mod testing;
pub mod transport;

pub use agent::{AgentClient, AgentEvent, AgentSession, RpcAgentClient, RpcAgentSession};
pub use error::{Error, Result};
pub use flavor::Flavor;
pub use launch::{Launch, Options, ProtocolMode};
pub use protocol::{Command, Event};
pub use session::Session;
