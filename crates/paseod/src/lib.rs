//! `paseod`: the Rust edge in front of the TS Paseo daemon (plan P2).
//!
//! Read `EDGE-SPEC.md` first. Module map:
//!
//! - [`config`]   edge configuration (env + read-only persisted config)
//! - [`internal`] the edge <-> TS backend contract (headers, lifecycle lines, env names)
//! - [`routing`]  `OwnedTypes` + `peek_route`, the dispatch hook for domains that move to Rust
//! - [`core_routes`] which message types the Rust core claims under `PASEO_CORE=rust`
//! - [`client_registry`] sockets <-> core sessions, the `Dispatcher` that feeds them
//! - [`internal_rpc`] `internal.delivery.*` between the core and TS-resident controllers
//! - [`core_runtime`] builds the core services from the edge config
//! - [`parent`]   supervisor liveness over stdin (EOF or shutdown line stops the edge)
//! - [`backend`]  spawn/supervise the TS worker, readiness
//! - [`ws`]       physical client sockets, per-socket internal proxy, lease, high-water
//! - [`relay`]    outbound relay transport + E2EE (feeds [`ws`] like a client socket)
//! - [`trusted`]  trusted proxy set mirroring the daemon's `trustedProxies`
//! - [`http`]     axum router: `/ws` upgrade, static web UI, reverse proxy to TS
//! - [`agent_mcp`] in-process `/mcp/agents` (tool catalog over MCP) under `PASEO_CORE=rust`

pub mod agent_mcp;
pub mod backend;
pub mod client_registry;
pub mod config;
pub mod core_routes;
pub mod core_runtime;
pub mod error;
pub mod http;
pub mod internal;
pub mod internal_rpc;
pub mod parent;
pub mod relay;
pub mod routing;
pub mod trusted;
pub mod ws;
