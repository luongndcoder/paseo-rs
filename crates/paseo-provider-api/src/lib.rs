//! Provider seam of the Paseo core: object-safe [`AgentClient`] / [`AgentSession`] traits,
//! the single-consumer [`EventStream`], and the shared [`run_provider_turn`].
//!
//! Implemented by `paseo-provider-host` (TS provider host) and by native adapters.

mod error;
mod events;
mod run;
mod traits;
mod types;

pub use error::{ProviderError, Result};
pub use events::{event_channel, EventSender, EventStream, DEFAULT_EVENT_CAPACITY};
pub use run::{
    append_or_replace_growing, replace_with_assistant_message, run_provider_turn, run_session_turn,
    FinalTextReducer,
};
pub use traits::{AgentClient, AgentSession, ToolCallHandler};
pub use types::*;
