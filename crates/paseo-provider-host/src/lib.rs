//! Client for the Paseo TS provider-host.
//!
//! The host exposes the TS `AgentClient`/`AgentSession` interface as JSONL-RPC over stdio.
//! This crate spawns it, gives typed calls, a per-session event stream, and a permission
//! callback trait. The wire protocol is documented in `RPC.md`.

mod adapter;
mod client;
mod config;
mod error;
mod permission;
mod session;
pub mod wire;

pub use adapter::{HostAgentClient, HostAgentSession};
pub use client::{HostCallbacks, ImportedHostSession, Pending, ProviderHostClient, SessionEvent};
pub use config::{HostConfig, InitializeOptions};
pub use error::{code, Error, Result};
pub use permission::{AllowAll, DenyAll, PermissionContext, PermissionHandler};
pub use session::{EventStream, RevertKind, Session};

/// Wire protocol version spoken by this crate.
pub const PROTOCOL_VERSION: u32 = 1;
