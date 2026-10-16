//! Agent lifecycle operations: register, create/import, resume, reload, close, archive,
//! detach/labels, hydration/rewind, mutators, out-of-band prompts and the stored-agent loader.

pub mod archive;
pub mod close;
pub mod create;
pub mod detach_labels;
pub mod hydrate;
pub mod loader;
pub mod mutators;
pub mod oob;
pub mod register;
pub mod reload;
pub mod resume;
