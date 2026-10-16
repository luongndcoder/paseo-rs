//! Port of the xterm.js `InputHandler` (@xterm/headless 6.0.0): what a control
//! sequence or printable run does to the buffers. Function names follow the TS.

use super::buffer::Buffer;
use super::cell::*;
use super::charsets;
use super::term::Term;
use super::width::xterm_width;

/// xterm `IParams`: a value plus optional colon sub-parameters per slot.
#[derive(Clone, Debug, Default)]
pub struct Params {
    pub items: Vec<(i64, Vec<i64>)>,
}

impl Params {
    pub fn len(&self) -> usize {
        self.items.len()
    }
    /// `params.params[i]`; 0 when absent.
    pub fn get(&self, i: usize) -> i64 {
        self.items.get(i).map_or(0, |p| p.0)
    }
    /// `params.params[i] || 1`
    pub fn or1(&self, i: usize) -> usize {
        let v = self.get(i);
        if v > 0 {
            v as usize
        } else {
            1
        }
    }
    pub(super) fn has_sub(&self, i: usize) -> bool {
        self.items.get(i).is_some_and(|p| !p.1.is_empty())
    }
}

// ---- Unicode v6 join state (xterm `UnicodeService`) ------------------------------

fn char_properties(code: u32, preceding: u32) -> u32 {
    let mut width = u32::from(xterm_width(char::from_u32(code).unwrap_or('\u{FFFD}')));
    let mut should_join = width == 0 && preceding != 0;
    if should_join {
        let old = extract_width(preceding);
        if old == 0 {
            should_join = false;
        } else if old > width {
            width = old;
        }
    }
    (width & 3) << 1 | u32::from(should_join)
}

fn extract_width(state: u32) -> u32 {
    (state >> 1) & 3
}

fn extract_should_join(state: u32) -> bool {
    state & 1 != 0
}

impl Term {
    pub(super) fn row_index(&self) -> usize {
        let b = self.buf();
        b.ybase + b.y
    }

    // ---- cursor helpers ----------------------------------------------------------

    /// `_restrictCursor(maxCol)` applied to an arbitrary (possibly out-of-range) position.
    fn place_cursor(&mut self, x: i64, y: i64, max_col: usize) {
        let rows = self.rows as i64;
        let origin = self.origin;
        let b = self.buf_mut();
        b.x = x.max(0).min(max_col as i64) as usize;
        b.y = if origin {
            y.max(b.scroll_top as i64).min(b.scroll_bottom as i64) as usize
        } else {
            y.max(0).min(rows - 1) as usize
        };
    }

    pub fn restrict_cursor(&mut self, max_col: usize) {
        let (x, y) = (self.buf().x as i64, self.buf().y as i64);
        self.place_cursor(x, y, max_col);
    }

    pub(super) fn restrict(&mut self) {
        self.restrict_cursor(self.cols - 1);
    }

    /// `_setCursor`
    pub(super) fn set_cursor(&mut self, x: i64, y: i64) {
        let y = if self.origin {
            self.buf().scroll_top as i64 + y
        } else {
            y
        };
        self.place_cursor(x, y, self.cols - 1);
    }

    fn move_cursor(&mut self, dx: i64, dy: i64) {
        self.restrict();
        let (x, y) = {
            let b = self.buf();
            (b.x as i64, b.y as i64)
        };
        self.set_cursor(x + dx, y + dy);
    }

    /// OSC 0 / OSC 2: the window title. Empty (after trim) clears it, like `terminal.ts`.
    pub fn set_title(&mut self, data: &str) {
        self.title = if data.trim().is_empty() {
            None
        } else {
            Some(data.to_owned())
        };
    }

    // ---- C0 / C1 ---------------------------------------------------------------------

    pub fn line_feed(&mut self) {
        let erase = self.erase_attr();
        let rows = self.rows;
        let cols = self.cols;
        let convert_eol = self.convert_eol;
        {
            let b = self.buf_mut();
            if convert_eol {
                b.x = 0;
            }
            b.y += 1;
        }
        let (y, bottom) = {
            let b = self.buf();
            (b.y, b.scroll_bottom)
        };
        if y == bottom + 1 {
            self.buf_mut().y -= 1;
            self.scroll(erase, false);
        } else if y >= rows {
            self.buf_mut().y = rows - 1;
        } else {
            let idx = self.row_index();
            self.buf_mut().lines[idx].wrapped = false;
        }
        let b = self.buf_mut();
        if b.x >= cols {
            b.x -= 1;
        }
    }

    pub fn carriage_return(&mut self) {
        self.buf_mut().x = 0;
    }

    pub fn backspace(&mut self) {
        if !self.reverse_wraparound {
            self.restrict();
            let b = self.buf_mut();
            if b.x > 0 {
                b.x -= 1;
            }
            return;
        }
        self.restrict_cursor(self.cols);
        let cols = self.cols;
        if self.buf().x > 0 {
            self.buf_mut().x -= 1;
        } else {
            let (x, y, top, bottom) = {
                let b = self.buf();
                (b.x, b.y, b.scroll_top, b.scroll_bottom)
            };
            let idx = self.row_index();
            if x == 0 && y > top && y <= bottom && self.buf().lines[idx].wrapped {
                let b = self.buf_mut();
                b.lines[idx].wrapped = false;
                b.y -= 1;
                b.x = cols - 1;
                let line = &b.lines[b.ybase + b.y];
                if line.width(b.x) > 0 && !line.has_content(b.x) {
                    b.x -= 1;
                }
            }
        }
        self.restrict();
    }

    pub fn tab(&mut self) {
        if self.buf().x >= self.cols {
            return;
        }
        let b = self.buf_mut();
        b.x = b.next_stop(None);
    }

    pub fn shift_out(&mut self) {
        self.set_glevel(1);
    }

    pub fn shift_in(&mut self) {
        self.set_glevel(0);
    }

    pub fn set_glevel(&mut self, level: usize) {
        self.glevel = level;
        self.charset = self.charsets[level];
    }

    pub(super) fn set_gcharset(&mut self, g: usize, set: Option<u8>) {
        self.charsets[g] = set;
        if self.glevel == g {
            self.charset = set;
        }
    }

    // ---- ESC ---------------------------------------------------------------------------

    pub fn index(&mut self) {
        self.restrict();
        let erase = self.erase_attr();
        let rows = self.rows;
        self.buf_mut().y += 1;
        let (y, bottom) = {
            let b = self.buf();
            (b.y, b.scroll_bottom)
        };
        if y == bottom + 1 {
            self.buf_mut().y -= 1;
            self.scroll(erase, false);
        } else if y >= rows {
            self.buf_mut().y = rows - 1;
        }
        self.restrict();
    }

    pub fn next_line(&mut self) {
        self.buf_mut().x = 0;
        self.index();
    }

    pub fn tab_set(&mut self) {
        let b = self.buf_mut();
        let x = b.x;
        b.tabs.insert(x);
    }

    pub fn reverse_index(&mut self) {
        self.restrict();
        let erase = self.erase_attr();
        let cols = self.cols;
        let (y, top, bottom, ybase) = {
            let b = self.buf();
            (b.y, b.scroll_top, b.scroll_bottom, b.ybase)
        };
        if y == top {
            let height = bottom - top;
            let b = self.buf_mut();
            let start = ybase + y;
            b.shift_elements_down(start, height, cols);
            b.lines[start] = Buffer::blank_line(cols, erase.0, erase.1, false);
        } else {
            let (x, y) = (self.buf().x as i64, self.buf().y as i64 - 1);
            self.place_cursor(x, y, self.cols - 1);
        }
    }

    pub fn save_cursor(&mut self) {
        let (fg, bg, charset) = (self.cur_fg, self.cur_bg, self.charset);
        let b = self.buf_mut();
        b.saved_x = b.x;
        b.saved_y = b.ybase + b.y;
        b.saved_fg = fg;
        b.saved_bg = bg;
        b.saved_charset = charset;
    }

    pub fn restore_cursor(&mut self) {
        let (fg, bg, charset) = {
            let b = self.buf();
            (b.saved_fg, b.saved_bg, b.saved_charset)
        };
        {
            let b = self.buf_mut();
            b.x = b.saved_x;
            b.y = b.saved_y.saturating_sub(b.ybase);
        }
        self.cur_fg = fg;
        self.cur_bg = bg;
        // `this._charsetService.charset = this._activeBuffer.savedCharset` when set; an
        // undefined saved charset leaves the current one (the TS reads a missing private).
        if charset.is_some() {
            self.charset = charset;
        } else {
            self.charset = None;
        }
        self.restrict();
    }

    pub fn screen_alignment_pattern(&mut self) {
        let cell = Cell::from_cp(u32::from('E'), 1, self.cur_fg, self.cur_bg);
        self.set_cursor(0, 0);
        let rows = self.rows;
        let b = self.buf_mut();
        for y_offset in 0..rows {
            let row = b.ybase + b.y + y_offset;
            if let Some(line) = b.lines.get_mut(row) {
                line.fill(&cell, false);
                line.wrapped = false;
            }
        }
        self.set_cursor(0, 0);
    }

    pub fn select_default_charset(&mut self) {
        self.set_glevel(0);
        self.set_gcharset(0, None);
    }

    pub fn select_charset(&mut self, designator: u8, flag: u8) {
        if designator == b'/' {
            return;
        }
        let g = match designator {
            b'(' => 0,
            b')' | b'-' => 1,
            b'*' | b'.' => 2,
            b'+' => 3,
            _ => return,
        };
        // CHARSETS[flag] || DEFAULT_CHARSET: unknown flags (and B) mean ASCII.
        let set = if charsets::is_known(flag) && flag != b'B' {
            Some(flag)
        } else {
            None
        };
        self.set_gcharset(g, set);
    }

    // ---- print -------------------------------------------------------------------------

    /// One printable run (`InputHandler.print`).
    pub fn print(&mut self, data: &[char]) {
        let cols = self.cols;
        let rows = self.rows;
        let wraparound = self.wraparound;
        let insert_mode = self.insert_mode;
        let (cur_fg, cur_bg) = (self.cur_fg, self.cur_bg);
        let erase = self.erase_attr();
        let charset = self.charset;
        let mut preceding = self.preceding_join;

        let mut row = self.row_index();
        {
            let b = self.buf_mut();
            if b.x > 0 && !data.is_empty() && b.lines[row].width(b.x - 1) == 2 {
                let x = b.x - 1;
                b.lines[row].set_from_cp(x, 0, 1, cur_fg, cur_bg);
            }
        }

        for &ch in data {
            let mut code = u32::from(ch);
            if code < 127 {
                if let Some(set) = charset {
                    if let Some(mapped) = charsets::map(set, ch) {
                        code = u32::from(mapped);
                    }
                }
            }
            let info = char_properties(code, preceding);
            let mut ch_width = extract_width(info) as usize;
            let should_join = extract_should_join(info);
            let old_width = if should_join {
                extract_width(preceding) as usize
            } else {
                0
            };
            preceding = info;

            let x_now = self.buf().x;
            if x_now + ch_width - old_width > cols {
                if wraparound {
                    let old_row_idx = row;
                    let mut old_col = x_now - old_width;
                    let moved_end = (old_col + old_width).min(cols);
                    let moved: Vec<Cell> = if old_width > 0 && old_col < moved_end {
                        self.buf().lines[old_row_idx].cells[old_col..moved_end].to_vec()
                    } else {
                        Vec::new()
                    };
                    let mut old_row_now = Some(old_row_idx);
                    {
                        let b = self.buf_mut();
                        b.x = old_width;
                        b.y += 1;
                    }
                    let (y, bottom) = {
                        let b = self.buf();
                        (b.y, b.scroll_bottom)
                    };
                    if y == bottom + 1 {
                        self.buf_mut().y -= 1;
                        let effect = self.scroll(erase, true);
                        old_row_now = old_row_now.and_then(|i| effect.remap(i));
                    } else {
                        let b = self.buf_mut();
                        if b.y >= rows {
                            b.y = rows - 1;
                        }
                        let idx = b.ybase + b.y;
                        b.lines[idx].wrapped = true;
                    }
                    row = self.row_index();
                    if old_width > 0 {
                        let b = self.buf_mut();
                        for (i, cell) in moved.into_iter().enumerate() {
                            b.lines[row].set_cell(i, &cell);
                        }
                    }
                    if let Some(old_idx) = old_row_now {
                        let b = self.buf_mut();
                        while old_col < cols {
                            b.lines[old_idx].set_from_cp(old_col, 0, 1, cur_fg, cur_bg);
                            old_col += 1;
                        }
                    }
                } else {
                    self.buf_mut().x = cols - 1;
                    if ch_width == 2 {
                        continue;
                    }
                }
            }

            let b = self.buf_mut();
            if should_join && b.x > 0 {
                let offset = if b.lines[row].width(b.x - 1) != 0 {
                    1
                } else {
                    2
                };
                let at = b.x - offset;
                b.lines[row].add_codepoint(at, code, ch_width as u8);
                let mut delta = ch_width as isize - old_width as isize;
                loop {
                    delta -= 1;
                    if delta < 0 {
                        break;
                    }
                    let x = b.x;
                    b.lines[row].set_from_cp(x, 0, 0, cur_fg, cur_bg);
                    b.x += 1;
                }
                continue;
            }

            if insert_mode {
                let x = b.x;
                b.lines[row].insert_cells(x, ch_width - old_width, &Cell::null(cur_fg, cur_bg));
                if b.lines[row].width(cols - 1) == 2 {
                    b.lines[row].set_from_cp(cols - 1, 0, 1, cur_fg, cur_bg);
                }
            }

            let x = b.x;
            b.lines[row].set_from_cp(x, code, ch_width as u8, cur_fg, cur_bg);
            b.x += 1;
            if ch_width > 0 {
                loop {
                    ch_width -= 1;
                    if ch_width == 0 {
                        break;
                    }
                    let x = b.x;
                    b.lines[row].set_from_cp(x, 0, 0, cur_fg, cur_bg);
                    b.x += 1;
                }
            }
        }

        self.preceding_join = preceding;
        let b = self.buf_mut();
        if b.x < cols
            && !data.is_empty()
            && b.lines[row].width(b.x) == 0
            && !b.lines[row].has_content(b.x)
        {
            let x = b.x;
            b.lines[row].set_from_cp(x, 0, 1, cur_fg, cur_bg);
        }
    }

    /// CSI Ps b (REP)
    pub fn repeat_preceding_character(&mut self, p: &Params) {
        let state = self.preceding_join;
        if state == 0 {
            return;
        }
        let length = p.or1(0);
        let ch_width = extract_width(state) as usize;
        let row = self.row_index();
        let b = self.buf();
        let x = b.x.saturating_sub(ch_width);
        let text: Vec<char> = b.lines[row]
            .cells
            .get(x)
            .map(|c| c.chars().chars().collect())
            .unwrap_or_default();
        let mut data = Vec::with_capacity(text.len() * length);
        for _ in 0..length {
            data.extend_from_slice(&text);
        }
        self.print(&data);
    }

    // ---- CSI cursor ----------------------------------------------------------------------

    pub fn cursor_up(&mut self, p: &Params) {
        let diff = self.buf().y as i64 - self.buf().scroll_top as i64;
        let n = p.or1(0) as i64;
        if diff >= 0 {
            self.move_cursor(0, -diff.min(n));
        } else {
            self.move_cursor(0, -n);
        }
    }

    pub fn cursor_down(&mut self, p: &Params) {
        let diff = self.buf().scroll_bottom as i64 - self.buf().y as i64;
        let n = p.or1(0) as i64;
        if diff >= 0 {
            self.move_cursor(0, diff.min(n));
        } else {
            self.move_cursor(0, n);
        }
    }

    pub fn cursor_forward(&mut self, p: &Params) {
        self.move_cursor(p.or1(0) as i64, 0);
    }

    pub fn cursor_backward(&mut self, p: &Params) {
        self.move_cursor(-(p.or1(0) as i64), 0);
    }

    pub fn cursor_next_line(&mut self, p: &Params) {
        self.cursor_down(p);
        self.buf_mut().x = 0;
    }

    pub fn cursor_preceding_line(&mut self, p: &Params) {
        self.cursor_up(p);
        self.buf_mut().x = 0;
    }

    pub fn cursor_char_absolute(&mut self, p: &Params) {
        let y = self.buf().y as i64;
        self.set_cursor(p.or1(0) as i64 - 1, y);
    }

    pub fn cursor_position(&mut self, p: &Params) {
        let col = if p.len() >= 2 { p.or1(1) as i64 - 1 } else { 0 };
        self.set_cursor(col, p.or1(0) as i64 - 1);
    }

    pub fn line_pos_absolute(&mut self, p: &Params) {
        let x = self.buf().x as i64;
        self.set_cursor(x, p.or1(0) as i64 - 1);
    }

    pub fn v_position_relative(&mut self, p: &Params) {
        self.move_cursor(0, p.or1(0) as i64);
    }

    pub fn tab_clear(&mut self, p: &Params) {
        let b = self.buf_mut();
        match p.get(0) {
            0 => {
                let x = b.x;
                b.tabs.remove(&x);
            }
            3 => b.tabs.clear(),
            _ => {}
        }
    }

    pub fn cursor_forward_tab(&mut self, p: &Params) {
        if self.buf().x >= self.cols {
            return;
        }
        for _ in 0..p.or1(0) {
            let b = self.buf_mut();
            b.x = b.next_stop(None);
        }
    }

    pub fn cursor_backward_tab(&mut self, p: &Params) {
        if self.buf().x >= self.cols {
            return;
        }
        for _ in 0..p.or1(0) {
            let b = self.buf_mut();
            b.x = b.prev_stop(None);
        }
    }

    pub fn select_protected(&mut self, p: &Params) {
        match p.get(0) {
            1 => self.cur_bg |= BG_PROTECTED,
            0 | 2 => self.cur_bg &= !BG_PROTECTED,
            _ => {}
        }
    }
}
