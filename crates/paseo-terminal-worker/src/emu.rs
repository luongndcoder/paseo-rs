//! Single import point for the emulator. Compiles against the contract in `emulator/mod.rs`;
//! while that module is still a stub the `stub-emulator` feature (default) supplies a private
//! plain-text stand-in so the runtime builds and runs.

#[cfg(not(feature = "stub-emulator"))]
pub use crate::emulator::{Emulator, SnapshotOptions};
#[cfg(feature = "stub-emulator")]
pub use crate::stub_emulator::{Emulator, SnapshotOptions};
