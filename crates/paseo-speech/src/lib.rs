//! Speech for the Paseo daemon, ported from `packages/server/src/server/{speech,dictation}`.

pub mod audio;
pub mod config;
pub mod dictation;
pub mod dictation_debug;
pub mod error;
pub mod provider;
pub mod providers;
pub mod readiness;
pub mod resampler;
pub mod types;

pub use error::{Result, SpeechError};
