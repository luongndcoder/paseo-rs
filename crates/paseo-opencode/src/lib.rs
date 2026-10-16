//! Native Rust client for the OpenCode server (HTTP + SSE).
//!
//! Layers, bottom to top:
//! - `transport` / `http` / `sse`: bytes on the wire (`Transport` trait, reqwest implementation).
//! - `protocol` + `client`: typed OpenCode REST API and `/global/event` events (raw provider layer).
//! - `server` / `manager` / `events`: `opencode serve` lifecycle and the reconnecting event hub.
//! - `agent`: the provider-agnostic `AgentClient` / `AgentSession` surface built on the above.
//! - `replay`: a `Transport` that serves recorded traffic, used by the golden tests.

pub mod agent;
pub mod client;
pub mod error;
pub mod events;
pub mod http;
pub mod manager;
pub mod options;
pub mod protocol;
pub mod replay;
pub mod server;
pub mod sse;
pub mod transport;

pub use agent::{AgentClient, AgentSession, OpenCodeAgentClient};
pub use client::RawClient;
pub use error::{Error, Result};
pub use options::OpenCodeOptions;
