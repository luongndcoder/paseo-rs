//! Terminal core: xterm.js `BufferService` + `BufferSet` + `CoreService` state, and the
//! input handler in `input.rs` mutating it. Everything mirrors @xterm/headless 6.0.0.

use super::buffer::Buffer;
use super::cell::Cell;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

/// How rows moved after `scroll`, so a caller holding a row index can follow its row.
pub struct ScrollEffect {
    trimmed_front: usize,
    moved_up: Option<(usize, usize)>,
    replaced: Option<usize>,
}

impl ScrollEffect {
    pub fn remap(&self, index: usize) -> Option<usize> {
        if self.replaced == Some(index) {
            return None;
        }
        if let Some((from, to)) = self.moved_up {
            if index >= from && index <= to {
                return Some(index - 1);
            }
        }
        index.checked_sub(self.trimmed_front)
    }
}

pub struct Term {
    pub cols: usize,
    pub rows: usize,
    pub scrollback: usize,
    pub normal: Buffer,
    pub alt: Buffer,
    pub alt_active: bool,

    // InputHandler state
    pub cur_fg: u32,
    pub cur_bg: u32,
    pub charsets: [Option<u8>; 4],
    pub glevel: usize,
    pub charset: Option<u8>,
    pub preceding_join: u32,

    // CoreService modes
    pub insert_mode: bool,
    pub wraparound: bool,
    pub origin: bool,
    pub reverse_wraparound: bool,
    pub cursor_hidden: bool,
    pub cursor_style: Option<CursorShape>,
    pub cursor_blink: Option<bool>,
    /// Option set through ANSI mode 20 (`convertEol`).
    pub convert_eol: bool,

    pub title: Option<String>,
}

impl Term {
    pub fn new(rows: usize, cols: usize, scrollback: usize) -> Self {
        let mut normal = Buffer::new(rows, cols, scrollback, true);
        normal.fill_viewport_rows(0, 0);
        let alt = Buffer::new(rows, cols, scrollback, false);
        Self {
            cols,
            rows,
            scrollback,
            normal,
            alt,
            alt_active: false,
            cur_fg: 0,
            cur_bg: 0,
            charsets: [None; 4],
            glevel: 0,
            charset: None,
            preceding_join: 0,
            insert_mode: false,
            wraparound: true,
            origin: false,
            reverse_wraparound: false,
            cursor_hidden: false,
            cursor_style: None,
            cursor_blink: None,
            convert_eol: false,
            title: None,
        }
    }

    pub fn buf(&self) -> &Buffer {
        if self.alt_active {
            &self.alt
        } else {
            &self.normal
        }
    }

    pub fn buf_mut(&mut self) -> &mut Buffer {
        if self.alt_active {
            &mut self.alt
        } else {
            &mut self.normal
        }
    }

    /// `_eraseAttrData()`: erase cells keep the background color, nothing else.
    pub fn erase_attr(&self) -> (u32, u32) {
        (0, self.cur_bg & 0x03FF_FFFF)
    }

    pub fn null_cell_erase(&self) -> Cell {
        let (fg, bg) = self.erase_attr();
        Cell::null(fg, bg)
    }

    /// `BufferService.scroll`: scroll the active buffer up one row, creating a blank row.
    pub fn scroll(&mut self, erase: (u32, u32), is_wrapped: bool) -> ScrollEffect {
        let cols = self.cols;
        let buffer = self.buf_mut();
        let new_line = Buffer::blank_line(cols, erase.0, erase.1, is_wrapped);
        let top_row = buffer.ybase + buffer.scroll_top;
        let bottom_row = buffer.ybase + buffer.scroll_bottom;
        let mut effect = ScrollEffect {
            trimmed_front: 0,
            moved_up: None,
            replaced: None,
        };

        if buffer.scroll_top == 0 {
            let will_be_trimmed = buffer.is_full();
            if bottom_row == buffer.lines.len() - 1 {
                if will_be_trimmed {
                    buffer.lines.pop_front();
                    effect.trimmed_front = 1;
                }
                buffer.lines.push_back(new_line);
            } else {
                buffer.insert_line(bottom_row + 1, new_line);
                if will_be_trimmed {
                    effect.trimmed_front = 1;
                }
            }
            if !will_be_trimmed {
                buffer.ybase += 1;
            }
        } else {
            let height = bottom_row - top_row + 1;
            if height > 1 && bottom_row < buffer.lines.len() {
                buffer.lines.make_contiguous()[top_row..=bottom_row].rotate_left(1);
                effect.moved_up = Some((top_row + 1, bottom_row));
            }
            buffer.lines[bottom_row] = new_line;
            effect.replaced = Some(top_row);
        }
        effect
    }

    pub fn activate_alt(&mut self) {
        if self.alt_active {
            return;
        }
        let (fg, bg) = self.erase_attr();
        self.alt.fill_viewport_rows(fg, bg);
        self.alt.x = self.normal.x;
        self.alt.y = self.normal.y;
        self.alt_active = true;
    }

    pub fn activate_normal(&mut self) {
        if !self.alt_active {
            return;
        }
        self.normal.x = self.alt.x;
        self.normal.y = self.alt.y;
        self.alt.clear();
        self.alt_active = false;
    }

    /// `BufferService.resize` + `BufferSet.resize`.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        self.normal.resize(cols, rows);
        self.alt.resize(cols, rows);
        self.normal.setup_tab_stops(Some(cols));
        self.alt.setup_tab_stops(Some(cols));
    }

    /// `CoreTerminal.reset()` (ESC c): fresh buffers and modes. The cursor hidden flag
    /// is not part of `CoreService.reset()` and survives, like in xterm.js.
    pub fn full_reset(&mut self) {
        self.cur_fg = 0;
        self.cur_bg = 0;
        self.normal = Buffer::new(self.rows, self.cols, self.scrollback, true);
        self.normal.fill_viewport_rows(0, 0);
        self.alt = Buffer::new(self.rows, self.cols, self.scrollback, false);
        self.alt_active = false;
        self.charsets = [None; 4];
        self.glevel = 0;
        self.charset = None;
        self.insert_mode = false;
        self.wraparound = true;
        self.origin = false;
        self.reverse_wraparound = false;
        self.cursor_style = None;
        self.cursor_blink = None;
        self.preceding_join = 0;
    }
}
