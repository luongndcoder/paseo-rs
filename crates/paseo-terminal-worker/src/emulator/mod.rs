//! Headless terminal emulator. Owned by the emulator workstream.
//!
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
//!
//! # Engine choice (spike: alacritty_terminal vs vt100, then neither)
//!
//! The TS snapshot (`terminal.ts` over `@xterm/headless`) needs, per cell: color
//! *mode* (1 = 16-color palette, 2 = 256 indexed, 3 = RGB), SGR attributes, wide-char
//! continuation cells, per-row soft-wrap flags, cursor shape/blink, alt screen without
//! scrollback and bounded history. And the output has to equal xterm.js, including its
//! quirks.
//!
//! - vt100 0.16 exposes `Color::{Default, Idx(u8), Rgb}`: `ESC[31m` and `ESC[38;5;1m` both
//!   become `Idx(1)`, so mode 1 vs 2 cannot be reproduced; no cursor-shape state either.
//!   Rejected.
//! - alacritty_terminal can represent every field (`Color::Named/Indexed/Spec`, `WRAPLINE`,
//!   `WIDE_CHAR_SPACER`, DECSCUSR, alt grid, history) and was prototyped first: all
//!   hand-written scenarios could be made to pass, but a seeded differential fuzz against
//!   @xterm/headless kept finding semantic gaps because alacritty's behavior differs from
//!   xterm.js in dozens of small places (pending-wrap state on LF/TAB, wrap flags,
//!   DECSC/DECRC coordinates being absolute in xterm, resize/reflow and cursor tracking,
//!   ?47/?1047/?1048, DECSTBM, wide chars with autowrap off, Unicode v6 widths, SGR 21,
//!   ED/EL details, ...). Patching each one over alacritty's storage meant re-implementing
//!   the handler anyway.
//! - Chosen: a direct port of xterm.js's `EscapeSequenceParser`, `InputHandler`, `Buffer`,
//!   `BufferLine`, `CircularList` semantics and `BufferReflow` (all read from the
//!   @xterm/headless 6.0.0 sources; the `vte` crate was tried first but its error recovery
//!   and C1/DEL handling differ). File names map to the TS modules: `parser.rs`
//!   (EscapeSequenceParser), `decode.rs` (the UTF-8 -> string step node-pty does before
//!   xterm.js sees data), `cell.rs` (AttributeData/CellData/BufferLine), `buffer.rs` + `reflow.rs`
//!   (Buffer, BufferReflow), `term.rs` (BufferService/BufferSet/CoreService), `input.rs`
//!   (InputHandler core), `edit.rs` + `modes.rs` (the rest of InputHandler), `width.rs` (UnicodeV6), `charsets.rs` (Charsets).
//!
//! Not emulated: terminal replies (DA/DSR/DECRQM, handled by the worker like terminal.ts
//! does), mouse/focus/bracketed-paste modes (no snapshot effect), OSC colors/hyperlinks,
//! DCS, async handlers, title stack push/pop (xterm.js has none either).
//!
//! Known divergence: after some resize sequences xterm.js reports a stale `isWrapped` for the
//! row one past the grid (its circular list is read without a bounds check and keeps removed
//! rows); `Lines` models the wrap-around but not those leftover slots, so `gridWrapped` of the
//! last row can differ (about 0.3% of random resize scripts in the differential fuzz).

// The worker runtime that consumes this API lands in another workstream.
#![allow(dead_code)]

mod buffer;
mod capture;
mod cell;
mod charsets;
mod decode;
mod edit;
mod input;
mod modes;
mod parser;
mod reflow;
mod snapshot;
mod term;
#[cfg(test)]
mod tests;
mod width;

use serde::Deserialize;
use serde_json::Value;

use decode::Utf8Decoder;
use parser::Parser;
use term::Term;

/// Mirrors TS `TerminalStateSnapshotOptions`.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotOptions {
    pub scrollback_lines: Option<i64>,
    pub include_wrap_flags: Option<bool>,
}

pub struct Emulator {
    term: Term,
    parser: Parser,
    decoder: Utf8Decoder,
}

impl Emulator {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        Self {
            term: Term::new(
                usize::from(rows.max(1)),
                usize::from(cols.max(2)),
                scrollback,
            ),
            parser: Parser::new(),
            decoder: Utf8Decoder::default(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let chars = self.decoder.decode(bytes);
        self.parser.parse(&mut self.term, &chars);
    }

    /// xterm.js refuses fewer than 2 columns (the width checks assume it).
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = usize::from(rows.max(1));
        let cols = usize::from(cols.max(2));
        if rows != self.term.rows || cols != self.term.cols {
            self.term.resize(cols, rows);
        }
    }

    pub fn title(&self) -> Option<String> {
        self.term.title.clone()
    }

    /// Cursor as xterm reports it (`buffer.active.cursorY/cursorX`, 0-based); the worker
    /// answers CPR queries from it.
    pub fn cursor_position(&self) -> (usize, usize) {
        let buffer = self.term.buf();
        (buffer.y, buffer.x)
    }

    /// JSON matching TS `TerminalState` (`terminal.ts` `getState`).
    pub fn snapshot(&self, opts: &SnapshotOptions) -> Value {
        snapshot::build(&self.term, opts)
    }

    /// Streams the `snapshot` JSON into `out` (same bytes as serializing `snapshot`, with `title`
    /// replacing the emulator's own title; `None` omits it).
    pub fn write_snapshot(&self, opts: &SnapshotOptions, title: Option<&str>, out: &mut Vec<u8>) {
        snapshot::write_state(&self.term, opts, title, out);
    }

    /// Matches TS `CaptureTerminalLinesResult` (`terminal-capture.ts`).
    pub fn capture(&self, start: Option<i64>, end: Option<i64>, strip_ansi: bool) -> Value {
        capture::capture_lines(snapshot::row_texts(&self.term), start, end, strip_ansi)
    }
}
