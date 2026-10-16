//! `terminal.ts` `getState` / `extractCell` equivalent over the xterm-port buffers.

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
