//! File explorer, directory suggestions, file transfer framing and upload.
//!
//! Port of the TypeScript daemon modules `file-explorer`, `file-download`,
//! `file-upload`, `utils/directory-suggestions` and the protocol
//! `binary-frames` file-transfer codec. The API is synchronous; an edge crate
//! wraps calls in `spawn_blocking` and wires them to the WebSocket protocol.
//! Wire types in [`wire`] serialize to the exact JSON the TS daemon emits.
//!
//! Unix only: revisions use `dev:ino:size:mtimeNs` and reads use `O_NOFOLLOW`.

#[cfg(not(unix))]
compile_error!("paseo-files supports unix targets only");

pub mod binary_frames;
pub mod collation;
pub mod content_disposition;
pub mod error;
pub mod explorer;
pub mod explorer_entries;
pub mod git;
pub mod handlers;
pub mod path_utils;
pub mod suggestions;
pub mod text_match;
pub mod time;
pub mod token_store;
pub mod upload;
pub mod wire;

pub use error::ExplorerError;
