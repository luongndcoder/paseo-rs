//! Wire types for the Pi / OMP RPC.
//!
//! Every type keeps unknown fields in an `extra` map and every tagged union has an `Other(Value)`
//! arm, so a frame this crate does not model survives parse -> serialize unchanged (up to
//! `null` vs absent, which the protocol treats as the same).

#[macro_use]
mod tagged;
pub mod commands;
pub mod events;
pub mod types;

pub use commands::Command;
pub use events::{AssistantMessageEvent, Event};
pub use types::*;
