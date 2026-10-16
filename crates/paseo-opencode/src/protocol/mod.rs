//! Wire types for the OpenCode 1.x HTTP + SSE API (the `/session`, `/global/event` surface).
//!
//! Every type keeps unknown fields in `extra` and every union has an `Unknown` fallback holding the
//! original JSON, so `parse -> serialize` is lossless and a newer server never causes a hard error.
//! See `PROTOCOL.md` at the crate root.

pub mod event;
pub mod requests;
pub mod types;

pub use event::{Event, EventPayload, GlobalEvent};
pub use requests::*;
pub use types::*;
