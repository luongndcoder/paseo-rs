//! Headless terminal emulator. Owned by the emulator workstream.
//! Contract consumed by the worker runtime (do not change signatures without updating both sides):
//!
//! pub struct Emulator;
//! impl Emulator {
//!     pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self;
//!     pub fn feed(&mut self, bytes: &[u8]);
//!     pub fn resize(&mut self, rows: u16, cols: u16);
//!     pub fn title(&self) -> Option<String>;
//!     /// JSON matching TS `TerminalState` / `TerminalStateSnapshot` exactly.
//!     pub fn snapshot(&self, opts: &SnapshotOptions) -> serde_json::Value;
//!     /// Matches TS `CaptureTerminalLinesResult` (terminal-capture.ts).
//!     pub fn capture(&self, start: Option<i64>, end: Option<i64>, strip_ansi: bool) -> serde_json::Value;
//! }
//! #[derive(Default, serde::Deserialize)] pub struct SnapshotOptions { /* mirrors TS TerminalStateSnapshotOptions */ }
