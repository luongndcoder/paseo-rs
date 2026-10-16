//! Native Rust client for the Codex CLI app-server (`codex app-server`, JSON-RPC over stdio).
//!
//! Layers, bottom up:
//!
//! - [`jsonl`] / [`transport`]: line framing and the process (or replay) transport.
//! - [`protocol`]: wire types, with a `serde_json::Value` fallback for anything unmodelled.
//! - [`rpc`]: request/response multiplexer over a transport.
//! - [`agent`]: provider-agnostic `AgentClient` / `AgentSession` surface and raw events.
//! - [`CodexClient`] / [`CodexSession`]: the Codex implementation of that surface.
//!
//! Timeline / tool-call mapping is deliberately absent: sessions emit raw provider events.

pub mod agent;
pub mod catalog;
pub mod client;
pub mod error;
pub mod jsonl;
pub mod options;
pub mod protocol;
pub mod rpc;
pub mod session;
pub mod transport;

pub use agent::{AgentClient, AgentEvent, AgentSession};
pub use client::CodexClient;
pub use error::{Error, Result};
pub use options::CodexOptions;
pub use session::CodexSession;
