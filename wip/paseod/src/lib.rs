//! `paseod`: the Rust edge in front of the TS Paseo daemon (plan P2).
//!
//! Read `EDGE-SPEC.md` first. Module map:
//!
//! - [`config`]   edge configuration (env + read-only persisted config)
//! - [`internal`] the edge <-> TS backend contract (headers, lifecycle lines, env names)
//! - [`routing`]  `OwnedTypes` + `peek_route`, the dispatch hook for domains that move to Rust
//! - [`backend`]  spawn/supervise the TS worker, readiness
//! - [`ws`]       physical client sockets, per-socket internal proxy, lease, high-water
//! - [`relay`]    outbound relay transport + E2EE (feeds [`ws`] like a client socket)
//! - [`http`]     axum router: `/ws` upgrade, static web UI, reverse proxy to TS

pub mod backend;
pub mod config;
pub mod error;
pub mod http;
pub mod internal;
pub mod relay;
pub mod routing;
pub mod ws;
