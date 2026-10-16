//! `terminal.ts` `getState` / `extractCell` equivalent over the xterm-port buffers.
//!
//! `build` produces a `Value` (tests, small callers); `write_state` streams the same JSON, byte for
//! byte what `serde_json` prints for `build`'s result (keys sorted, same escaping), without the tree.

use serde_json::{json, Map, Value};

use super::cell::*;
use super::term::{CursorShape, Term};
use super::SnapshotOptions;

fn color_parts(packed: u32) -> Option<(u32, u32)> {
    let mode = (packed & CM_MASK) >> 24;
    if mode == 0 {
        return None;
    }
    let value = if mode == 3 {
        packed & RGB_MASK
    } else {
        packed & PCOLOR_MASK
    };
    Some((mode, value))
}

/// Text xterm reports for a cell: continuation cells are empty, empty cells a space.
pub fn cell_text(cell: &Cell) -> String {
    if cell.width == 0 {
        return String::new();
    }
    let chars = cell.chars();
    if chars.is_empty() {
        " ".to_owned()
    } else {
        chars
    }
}

fn cell_json(cell: &Cell) -> Value {
    let mut map = Map::new();
    map.insert("char".into(), Value::String(cell_text(cell)));
    if let Some((mode, value)) = color_parts(cell.fg) {
        map.insert("fg".into(), json!(value));
        map.insert("fgMode".into(), json!(mode));
    }
    if let Some((mode, value)) = color_parts(cell.bg) {
        map.insert("bg".into(), json!(value));
        map.insert("bgMode".into(), json!(mode));
    }
    map.insert("bold".into(), json!(cell.fg & FG_BOLD != 0));
    map.insert("italic".into(), json!(cell.bg & BG_ITALIC != 0));
    map.insert("underline".into(), json!(cell.fg & FG_UNDERLINE != 0));
    map.insert("dim".into(), json!(cell.bg & BG_DIM != 0));
    map.insert("inverse".into(), json!(cell.fg & FG_INVERSE != 0));
    map.insert(
        "strikethrough".into(),
        json!(cell.fg & FG_STRIKETHROUGH != 0),
    );
    Value::Object(map)
}

fn row_json(term: &Term, index: usize) -> Value {
    let buf = term.buf();
    let cells: Vec<Value> = (0..term.cols)
        .map(
            |col| match buf.line_cyclic(index).and_then(|line| line.cells.get(col)) {
                Some(cell) => cell_json(cell),
                None => json!({ "char": " " }),
            },
        )
        .collect();
    Value::Array(cells)
}

/// `buffer.getLine(index + 1)?.isWrapped` (see `Buffer::line_cyclic`).
fn continues_to_next(term: &Term, index: usize) -> bool {
    term.buf()
        .line_cyclic(index + 1)
        .is_some_and(|line| line.wrapped)
}

fn scrollback_start(history: usize, limit: Option<i64>) -> usize {
    match limit {
        // Math.max(0, history - limit); a negative limit yields an empty range.
        Some(limit) => (history as i64 - limit).clamp(0, history as i64) as usize,
        None => 0,
    }
}

fn cursor_json(term: &Term) -> Value {
    let buf = term.buf();
    let mut map = Map::new();
    map.insert("row".into(), json!(buf.y));
    map.insert("col".into(), json!(buf.x));
    if term.cursor_hidden {
        map.insert("hidden".into(), json!(true));
    }
    if let Some(shape) = term.cursor_style {
        let name = match shape {
            CursorShape::Block => "block",
            CursorShape::Underline => "underline",
            CursorShape::Bar => "bar",
        };
        map.insert("style".into(), json!(name));
    }
    if let Some(blink) = term.cursor_blink {
        map.insert("blink".into(), json!(blink));
    }
    Value::Object(map)
}

pub fn build(term: &Term, opts: &SnapshotOptions) -> Value {
    let buf = term.buf();
    let history = buf.ybase;
    let start = scrollback_start(history, opts.scrollback_lines);

    let scrollback: Vec<Value> = (start..history).map(|i| row_json(term, i)).collect();
    let grid: Vec<Value> = (0..term.rows)
        .map(|r| row_json(term, history + r))
        .collect();

    let mut state = Map::new();
    state.insert("rows".into(), json!(term.rows));
    state.insert("cols".into(), json!(term.cols));
    state.insert("grid".into(), Value::Array(grid));
    state.insert("scrollback".into(), Value::Array(scrollback));
    state.insert("cursor".into(), cursor_json(term));
    if let Some(title) = term.title.as_ref().filter(|t| !t.is_empty()) {
        state.insert("title".into(), Value::String(title.clone()));
    }
    if opts.include_wrap_flags == Some(true) {
        let grid_wrapped: Vec<Value> = (0..term.rows)
            .map(|r| json!(continues_to_next(term, history + r)))
            .collect();
        let scrollback_wrapped: Vec<Value> = (start..history)
            .map(|i| json!(continues_to_next(term, i)))
            .collect();
        state.insert("gridWrapped".into(), Value::Array(grid_wrapped));
        state.insert("scrollbackWrapped".into(), Value::Array(scrollback_wrapped));
    }
    Value::Object(state)
}

/// Untrimmed text of every row (scrollback first), as `cellsToPlainText` sees it
/// before `trimEnd`.
pub fn row_texts(term: &Term) -> Vec<String> {
    let buf = term.buf();
    (0..buf.ybase + term.rows)
        .map(|index| {
            (0..term.cols)
                .map(
                    |col| match buf.line_cyclic(index).and_then(|line| line.cells.get(col)) {
                        Some(cell) => cell_text(cell),
                        None => " ".to_owned(),
                    },
                )
                .collect::<String>()
        })
        .collect()
}

fn push_u32(out: &mut Vec<u8>, mut n: u32) {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&buf[i..]);
}

fn push_bool(out: &mut Vec<u8>, v: bool) {
    out.extend_from_slice(if v { b"true" } else { b"false" });
}

/// Appends `s` as a JSON string body (no quotes) with serde_json's escaping.
fn push_escaped(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let esc: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            0x08 => b"\\b",
            0x0c => b"\\f",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0..=0x1f => {
                out.extend_from_slice(&bytes[start..i]);
                out.extend_from_slice(b"\\u00");
                out.push(b"0123456789abcdef"[usize::from(b >> 4)]);
                out.push(b"0123456789abcdef"[usize::from(b & 0xf)]);
                start = i + 1;
                continue;
            }
            _ => continue,
        };
        out.extend_from_slice(&bytes[start..i]);
        out.extend_from_slice(esc);
        start = i + 1;
    }
    out.extend_from_slice(&bytes[start..]);
}

/// Cell JSON is `{<bg,bgMode,>bold,char,dim,<fg,fgMode,>inverse,italic,strikethrough,underline}`;
/// everything but `char` depends only on (fg, bg), so the two halves are cached per attribute pair.
struct CellWriter {
    key: Option<(u32, u32)>,
    head: Vec<u8>,
    tail: Vec<u8>,
}

impl CellWriter {
    fn new() -> Self {
        Self {
            key: None,
            head: Vec::with_capacity(64),
            tail: Vec::with_capacity(128),
        }
    }

    fn prepare(&mut self, fg: u32, bg: u32) {
        if self.key == Some((fg, bg)) {
            return;
        }
        self.key = Some((fg, bg));
        let (head, tail) = (&mut self.head, &mut self.tail);
        head.clear();
        tail.clear();
        head.push(b'{');
        if let Some((mode, value)) = color_parts(bg) {
            head.extend_from_slice(b"\"bg\":");
            push_u32(head, value);
            head.extend_from_slice(b",\"bgMode\":");
            push_u32(head, mode);
            head.push(b',');
        }
        head.extend_from_slice(b"\"bold\":");
        push_bool(head, fg & FG_BOLD != 0);
        head.extend_from_slice(b",\"char\":\"");
        tail.extend_from_slice(b"\",\"dim\":");
        push_bool(tail, bg & BG_DIM != 0);
        tail.push(b',');
        if let Some((mode, value)) = color_parts(fg) {
            tail.extend_from_slice(b"\"fg\":");
            push_u32(tail, value);
            tail.extend_from_slice(b",\"fgMode\":");
            push_u32(tail, mode);
            tail.push(b',');
        }
        tail.extend_from_slice(b"\"inverse\":");
        push_bool(tail, fg & FG_INVERSE != 0);
        tail.extend_from_slice(b",\"italic\":");
        push_bool(tail, bg & BG_ITALIC != 0);
        tail.extend_from_slice(b",\"strikethrough\":");
        push_bool(tail, fg & FG_STRIKETHROUGH != 0);
        tail.extend_from_slice(b",\"underline\":");
        push_bool(tail, fg & FG_UNDERLINE != 0);
        tail.push(b'}');
    }

    fn write(&mut self, out: &mut Vec<u8>, cell: &Cell) {
        self.prepare(cell.fg, cell.bg);
        out.extend_from_slice(&self.head);
        if cell.width != 0 {
            if let Some(text) = &cell.combined {
                if text.is_empty() {
                    out.push(b' ');
                } else {
                    push_escaped(out, text);
                }
            } else {
                match char::from_u32(cell.cp).filter(|_| cell.cp != 0) {
                    None => out.push(b' '),
                    Some(c) if c.is_ascii() && c >= ' ' && c != '"' && c != '\\' => {
                        out.push(c as u8);
                    }
                    Some(c) => {
                        let mut tmp = [0u8; 4];
                        push_escaped(out, c.encode_utf8(&mut tmp));
                    }
                }
            }
        }
        out.extend_from_slice(&self.tail);
    }
}

fn write_row(out: &mut Vec<u8>, cw: &mut CellWriter, term: &Term, index: usize) {
    out.push(b'[');
    let line = term.buf().line_cyclic(index);
    for col in 0..term.cols {
        if col > 0 {
            out.push(b',');
        }
        match line.and_then(|l| l.cells.get(col)) {
            Some(cell) => cw.write(out, cell),
            None => out.extend_from_slice(b"{\"char\":\" \"}"),
        }
    }
    out.push(b']');
}

fn write_bools(out: &mut Vec<u8>, range: std::ops::Range<usize>, term: &Term) {
    out.push(b'[');
    for (n, i) in range.enumerate() {
        if n > 0 {
            out.push(b',');
        }
        push_bool(out, continues_to_next(term, i));
    }
    out.push(b']');
}

/// Streams the terminal state JSON. `title` replaces whatever title the emulator holds
/// (the worker owns the title), omitted when `None`.
pub fn write_state(term: &Term, opts: &SnapshotOptions, title: Option<&str>, out: &mut Vec<u8>) {
    let history = term.buf().ybase;
    let start = scrollback_start(history, opts.scrollback_lines);
    let wrap = opts.include_wrap_flags == Some(true);
    let mut cw = CellWriter::new();
    // A plain cell is about 100 bytes of JSON; one up-front allocation avoids regrowth copies.
    out.reserve((term.rows + history - start) * term.cols * 100 + 1024);

    out.extend_from_slice(b"{\"cols\":");
    push_u32(out, term.cols as u32);
    out.extend_from_slice(b",\"cursor\":");
    write_cursor(out, term);
    out.extend_from_slice(b",\"grid\":[");
    for r in 0..term.rows {
        if r > 0 {
            out.push(b',');
        }
        write_row(out, &mut cw, term, history + r);
    }
    out.push(b']');
    if wrap {
        out.extend_from_slice(b",\"gridWrapped\":");
        write_bools(out, history..history + term.rows, term);
    }
    out.extend_from_slice(b",\"rows\":");
    push_u32(out, term.rows as u32);
    out.extend_from_slice(b",\"scrollback\":[");
    for (n, i) in (start..history).enumerate() {
        if n > 0 {
            out.push(b',');
        }
        write_row(out, &mut cw, term, i);
    }
    out.push(b']');
    if wrap {
        out.extend_from_slice(b",\"scrollbackWrapped\":");
        write_bools(out, start..history, term);
    }
    if let Some(title) = title {
        out.extend_from_slice(b",\"title\":\"");
        push_escaped(out, title);
        out.push(b'"');
    }
    out.push(b'}');
}

fn write_cursor(out: &mut Vec<u8>, term: &Term) {
    let buf = term.buf();
    out.extend_from_slice(b"{");
    if let Some(blink) = term.cursor_blink {
        out.extend_from_slice(b"\"blink\":");
        push_bool(out, blink);
        out.push(b',');
    }
    out.extend_from_slice(b"\"col\":");
    push_u32(out, buf.x as u32);
    if term.cursor_hidden {
        out.extend_from_slice(b",\"hidden\":true");
    }
    out.extend_from_slice(b",\"row\":");
    push_u32(out, buf.y as u32);
    if let Some(shape) = term.cursor_style {
        out.extend_from_slice(b",\"style\":\"");
        out.extend_from_slice(match shape {
            CursorShape::Block => b"block".as_slice(),
            CursorShape::Underline => b"underline",
            CursorShape::Bar => b"bar",
        });
        out.push(b'"');
    }
    out.push(b'}');
}
