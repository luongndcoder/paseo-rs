//! Temporary plain-text stand-in for the real emulator (same signatures as the contract in
//! `emulator/mod.rs`). Tracks no cursor/attributes; collects printable text into lines.

use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SnapshotOptions {
    pub scrollback_lines: Option<usize>,
    pub include_wrap_flags: Option<bool>,
}

pub struct Emulator {
    rows: u16,
    cols: u16,
    lines: Vec<String>,
    title: Option<String>,
    esc: u8, // 0 ground, 1 esc, 2 csi, 3 osc
    osc: String,
    scrollback: usize,
}

impl Emulator {
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        Self {
            rows,
            cols,
            lines: vec![String::new()],
            title: None,
            esc: 0,
            osc: String::new(),
            scrollback,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for ch in String::from_utf8_lossy(bytes).chars() {
            match self.esc {
                1 => {
                    self.esc = match ch {
                        '[' => 2,
                        ']' => {
                            self.osc.clear();
                            3
                        }
                        _ => 0,
                    }
                }
                2 => {
                    if ('@'..='~').contains(&ch) {
                        self.esc = 0;
                    }
                }
                3 => {
                    if ch == '\u{7}' || ch == '\\' {
                        self.esc = 0;
                        let osc = std::mem::take(&mut self.osc);
                        let osc = osc.trim_end_matches('\u{1b}');
                        if let Some(t) = osc.strip_prefix("0;").or_else(|| osc.strip_prefix("2;")) {
                            self.title = Some(t.to_owned());
                        }
                    } else {
                        self.osc.push(ch);
                    }
                }
                _ => match ch {
                    '\u{1b}' => self.esc = 1,
                    '\n' => self.lines.push(String::new()),
                    '\r' => {}
                    c if !c.is_control() => {
                        if let Some(l) = self.lines.last_mut() {
                            l.push(c);
                        }
                    }
                    _ => {}
                },
            }
        }
        let max = self.scrollback + self.rows as usize;
        if self.lines.len() > max {
            let drop = self.lines.len() - max;
            self.lines.drain(..drop);
        }
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
    }

    pub fn title(&self) -> Option<String> {
        self.title.clone()
    }

    pub fn snapshot(&self, _opts: &SnapshotOptions) -> Value {
        let blank_row: Vec<Value> = (0..self.cols).map(|_| json!({"char":" "})).collect();
        let grid: Vec<Value> = (0..self.rows).map(|_| json!(blank_row)).collect();
        json!({
            "rows": self.rows,
            "cols": self.cols,
            "grid": grid,
            "scrollback": [],
            "cursor": {"row": 0, "col": 0},
        })
    }

    pub fn cursor_position(&self) -> (usize, usize) {
        (0, 0)
    }

    pub fn write_snapshot(&self, opts: &SnapshotOptions, title: Option<&str>, out: &mut Vec<u8>) {
        let mut state = self.snapshot(opts);
        if let (Some(title), Some(obj)) = (title, state.as_object_mut()) {
            obj.insert("title".into(), json!(title));
        }
        let _ = serde_json::to_writer(out, &state);
    }

    pub fn capture(&self, start: Option<i64>, end: Option<i64>, _strip_ansi: bool) -> Value {
        let total = self.lines.len() as i64;
        let resolve = |v: Option<i64>, default: i64| -> i64 {
            match v {
                None => default,
                Some(n) if n < 0 => (total + n).max(0),
                Some(n) => n.min(total - 1),
            }
        };
        let s = resolve(start, 0);
        let e = resolve(end, total - 1);
        let lines: Vec<&str> = if s > e {
            vec![]
        } else {
            self.lines[s as usize..=e as usize]
                .iter()
                .map(|l| l.trim_end())
                .collect()
        };
        json!({"lines": lines, "totalLines": total})
    }
}
