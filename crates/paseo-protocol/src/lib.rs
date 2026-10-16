//! Paseo wire protocol types.
//!
//! `generated` holds per-message structs produced by typify from the zod-derived JSON Schema
//! (see README). `Frame` is the lossless entry point: it keeps the original JSON value and records
//! whether a typed struct exists and accepts it. Unknown variants and fields never error.
#[rustfmt::skip]
pub mod generated;

mod frame;
mod route;

pub use frame::{Direction, Frame, Scope, Typed};
pub use route::{peek_route, Route};
