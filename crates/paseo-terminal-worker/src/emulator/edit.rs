//! `InputHandler` erase / insert / delete / scroll handlers (@xterm/headless 6.0.0).

use super::buffer::Buffer;
use super::cell::*;
use super::input::Params;
use super::term::Term;

impl Term {
    // ---- CSI erase / insert / delete -------------------------------------------------------

    fn erase_in_buffer_line(
        &mut self,
        y: usize,
        start: usize,
        end: usize,
        clear_wrap: bool,
        protect: bool,
    ) {
        let fill = self.null_cell_erase();
        let b = self.buf_mut();
        let idx = b.ybase + y;
        let line = &mut b.lines[idx];
        line.replace_cells(start, end, &fill, protect);
        if clear_wrap {
            line.wrapped = false;
        }
    }

    fn reset_buffer_line(&mut self, y: usize, protect: bool) {
        let fill = self.null_cell_erase();
        let b = self.buf_mut();
        let idx = b.ybase + y;
        if let Some(line) = b.lines.get_mut(idx) {
            line.fill(&fill, protect);
            line.wrapped = false;
        }
    }

    pub fn erase_in_display(&mut self, p: &Params, protect: bool) {
        self.restrict_cursor(self.cols);
        let cols = self.cols;
        let rows = self.rows;
        match p.get(0) {
            0 => {
                let (y, x) = (self.buf().y, self.buf().x);
                self.erase_in_buffer_line(y, x, cols, x == 0, protect);
                for j in y + 1..rows {
                    self.reset_buffer_line(j, protect);
                }
            }
            1 => {
                let (y, x) = (self.buf().y, self.buf().x);
                self.erase_in_buffer_line(y, 0, x + 1, true, protect);
                if x + 1 >= cols {
                    // xterm.js indexes `j + 1` without `ybase`; kept as is.
                    if let Some(line) = self.buf_mut().lines.get_mut(y + 1) {
                        line.wrapped = false;
                    }
                }
                for j in (0..y).rev() {
                    self.reset_buffer_line(j, protect);
                }
            }
            2 => {
                for j in (0..rows).rev() {
                    self.reset_buffer_line(j, protect);
                }
            }
            3 => {
                let b = self.buf_mut();
                let size = b.lines.len().saturating_sub(rows);
                if size > 0 {
                    b.lines.drain(..size);
                    b.ybase = b.ybase.saturating_sub(size);
                }
            }
            _ => {}
        }
    }

    pub fn erase_in_line(&mut self, p: &Params, protect: bool) {
        self.restrict_cursor(self.cols);
        let cols = self.cols;
        let (y, x) = (self.buf().y, self.buf().x);
        match p.get(0) {
            0 => self.erase_in_buffer_line(y, x, cols, x == 0, protect),
            1 => self.erase_in_buffer_line(y, 0, x + 1, false, protect),
            2 => self.erase_in_buffer_line(y, 0, cols, true, protect),
            _ => {}
        }
    }

    pub fn insert_lines(&mut self, p: &Params) {
        self.restrict();
        let n = p.or1(0);
        let (y, top, bottom, ybase) = {
            let b = self.buf();
            (b.y, b.scroll_top, b.scroll_bottom, b.ybase)
        };
        if y > bottom || y < top {
            return;
        }
        let (fg, bg) = self.erase_attr();
        let cols = self.cols;
        let rows = self.rows;
        let row = ybase + y;
        let bottom_offset = rows - 1 - bottom;
        let bottom_abs = rows - 1 + ybase - bottom_offset + 1;
        let b = self.buf_mut();
        for _ in 0..n {
            b.lines.remove(bottom_abs - 1);
            b.insert_line(row, Buffer::blank_line(cols, fg, bg, false));
        }
        b.x = 0;
    }

    pub fn delete_lines(&mut self, p: &Params) {
        self.restrict();
        let n = p.or1(0);
        let (y, top, bottom, ybase) = {
            let b = self.buf();
            (b.y, b.scroll_top, b.scroll_bottom, b.ybase)
        };
        if y > bottom || y < top {
            return;
        }
        let (fg, bg) = self.erase_attr();
        let cols = self.cols;
        let rows = self.rows;
        let row = ybase + y;
        let j = rows - 1 + ybase - (rows - 1 - bottom);
        let b = self.buf_mut();
        for _ in 0..n {
            b.lines.remove(row);
            b.insert_line(j, Buffer::blank_line(cols, fg, bg, false));
        }
        b.x = 0;
    }

    pub fn insert_chars(&mut self, p: &Params) {
        self.restrict();
        let fill = self.null_cell_erase();
        let n = p.or1(0);
        let b = self.buf_mut();
        let (x, row) = (b.x, b.ybase + b.y);
        if let Some(line) = b.lines.get_mut(row) {
            line.insert_cells(x, n, &fill);
        }
    }

    pub fn delete_chars(&mut self, p: &Params) {
        self.restrict();
        let fill = self.null_cell_erase();
        let n = p.or1(0);
        let b = self.buf_mut();
        let (x, row) = (b.x, b.ybase + b.y);
        if let Some(line) = b.lines.get_mut(row) {
            line.delete_cells(x, n, &fill);
        }
    }

    pub fn erase_chars(&mut self, p: &Params) {
        self.restrict();
        let fill = self.null_cell_erase();
        let n = p.or1(0);
        let b = self.buf_mut();
        let (x, row) = (b.x, b.ybase + b.y);
        if let Some(line) = b.lines.get_mut(row) {
            line.replace_cells(x, x + n, &fill, false);
        }
    }

    pub fn scroll_up(&mut self, p: &Params) {
        let (fg, bg) = self.erase_attr();
        let cols = self.cols;
        let b = self.buf_mut();
        for _ in 0..p.or1(0) {
            b.lines.remove(b.ybase + b.scroll_top);
            let at = b.ybase + b.scroll_bottom;
            b.insert_line(at, Buffer::blank_line(cols, fg, bg, false));
        }
    }

    pub fn scroll_down(&mut self, p: &Params) {
        let cols = self.cols;
        let b = self.buf_mut();
        for _ in 0..p.or1(0) {
            b.lines.remove(b.ybase + b.scroll_bottom);
            let at = b.ybase + b.scroll_top;
            b.insert_line(at, Buffer::blank_line(cols, 0, 0, false));
        }
    }

    fn column_op(&mut self, p: &Params, op: impl Fn(&mut Line, usize, usize, &Cell)) {
        let (y, top, bottom, ybase, x) = {
            let b = self.buf();
            (b.y, b.scroll_top, b.scroll_bottom, b.ybase, b.x)
        };
        if y > bottom || y < top {
            return;
        }
        let fill = self.null_cell_erase();
        let n = p.or1(0);
        let b = self.buf_mut();
        for row in top..=bottom {
            let line = &mut b.lines[ybase + row];
            op(line, x, n, &fill);
            line.wrapped = false;
        }
    }

    pub fn scroll_left(&mut self, p: &Params) {
        self.column_op(p, |line, _, n, fill| line.delete_cells(0, n, fill));
    }

    pub fn scroll_right(&mut self, p: &Params) {
        self.column_op(p, |line, _, n, fill| line.insert_cells(0, n, fill));
    }

    pub fn insert_columns(&mut self, p: &Params) {
        self.column_op(p, |line, x, n, fill| line.insert_cells(x, n, fill));
    }

    pub fn delete_columns(&mut self, p: &Params) {
        self.column_op(p, |line, x, n, fill| line.delete_cells(x, n, fill));
    }

    pub fn set_scroll_region(&mut self, p: &Params) {
        let rows = self.rows as i64;
        let top = if p.get(0) > 0 { p.get(0) } else { 1 };
        let mut bottom = if p.len() < 2 { rows } else { p.get(1) };
        if p.len() < 2 || bottom > rows || bottom == 0 {
            bottom = rows;
        }
        if bottom > top {
            let b = self.buf_mut();
            b.scroll_top = (top - 1) as usize;
            b.scroll_bottom = (bottom - 1) as usize;
            self.set_cursor(0, 0);
        }
    }
}
