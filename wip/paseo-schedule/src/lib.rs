//! Scheduled agents for the Paseo daemon.
//!
//! * [`cron`]: five-field cron parsing and next-run semantics identical to the TS daemon.
//! * [`store`]: `PASEO_HOME/schedules/<id>.json` files, same JSON schema, atomic writes.
//! * [`service`]: create/update/pause/resume, due-run claiming, run bookkeeping, recovery.
//! * [`host`]: the trait the edge implements to launch agents (this crate never spawns one).
//! * [`rpc`]: `schedule/*` request/response wire shapes and the request handler.

// Wire enums mirror the TS unions; boxing would leak into the public API for no gain.
#![allow(clippy::large_enum_variant)]

pub mod atomic;
pub mod cadence;
pub mod cron;
pub mod error;
pub mod host;
pub mod logger;
pub mod rpc;
pub mod service;
pub mod store;
pub mod time;
pub mod types;

pub use error::{Result, ScheduleError};
pub use host::ScheduleAgentHost;
pub use rpc::{ScheduleOutbound, ScheduleRequest, ScheduleSession};
pub use service::{ScheduleService, ScheduleServiceOptions};
pub use store::ScheduleStore;
pub use types::*;
