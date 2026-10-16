//! Single import point for the emulator. Compiles against the contract in `emulator/mod.rs`;
//! while that module is still a stub the `stub-emulator` feature (default) supplies a private
//! plain-text stand-in so the runtime builds and runs.

#[cfg(not(feature = "stub-emulator"))]
pub use crate::emulator::{Emulator, SnapshotOptions};
#[cfg(feature = "stub-emulator")]
pub use crate::stub_emulator::{Emulator, SnapshotOptions};

use serde_json::Value;

/// Normalizes `Emulator::snapshot` output to a `TerminalState`: the contract says the JSON matches
/// "TerminalState / TerminalStateSnapshot", so accept either `{state, ..}` or the bare state.
pub fn into_terminal_state(snapshot: Value) -> Value {
    if snapshot.get("grid").is_none() {
        if let Some(state) = snapshot.get("state") {
            return state.clone();
        }
    }
    snapshot
}
