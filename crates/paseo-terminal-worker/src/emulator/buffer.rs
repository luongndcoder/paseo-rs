//! Port of xterm.js `Buffer` / `CircularList` semantics (@xterm/headless 6.0.0).
//! `lines` always holds `ybase + rows` rows; `y` is viewport-relative, saved cursor Y
//! is absolute (`ybase + y`) exactly like xterm.js.

use std::collections::{BTreeSet, VecDeque};
use std::ops::{Deref, DerefMut, Index, IndexMut};

use super::cell::{Cell, Line};

/// Row storage with `CircularList` read semantics: xterm.js `get(i)` has no bounds check,
/// so once the list is full an index past the end wraps to the front. Anything else out of
/// range reads (and swallows writes on) a scratch row instead of throwing.
pub struct Lines {
    deque: VecDeque<Line>,
    pub max_len: usize,
    scratch: Line,
}

impl Lines {
    fn new(max_len: usize) -> Self {
        Self {
            deque: VecDeque::new(),
            max_len,
            scratch: Line::new(0, &Cell::null(0, 0), false),
        }
    }

    pub fn replace(&mut self, deque: VecDeque<Line>) {
        self.deque = deque;
    }

    fn wrapped_index(&self, index: usize) -> Option<usize> {
        let len = self.deque.len();
        if index < len {
            Some(index)
        } else if len == self.max_len && len > 0 {
            Some(index % len)
        } else {
            None
        }
    }
}

impl Deref for Lines {
    type Target = VecDeque<Line>;
    fn deref(&self) -> &VecDeque<Line> {
        &self.deque
    }
}

impl DerefMut for Lines {
    fn deref_mut(&mut self) -> &mut VecDeque<Line> {
        &mut self.deque
    }
}

impl Index<usize> for Lines {
    type Output = Line;
    fn index(&self, index: usize) -> &Line {
        match self.wrapped_index(index) {
            Some(i) => &self.deque[i],
            None => &self.scratch,
        }
    }
}

impl IndexMut<usize> for Lines {
    fn index_mut(&mut self, index: usize) -> &mut Line {
        match self.wrapped_index(index) {
            Some(i) => &mut self.deque[i],
            None => {
                let cols = self.deque.front().map_or(0, Line::len);
                self.scratch = Line::new(cols, &Cell::null(0, 0), false);
                &mut self.scratch
            }
        }
    }
}

pub struct Buffer {
    pub lines: Lines,
    pub cols: usize,
    pub rows: usize,
    pub ybase: usize,
    pub y: usize,
    pub x: usize,
    pub scroll_top: usize,
    pub scroll_bottom: usize,
    pub tabs: BTreeSet<usize>,
    pub saved_x: usize,
    pub saved_y: usize,
    pub saved_fg: u32,
    pub saved_bg: u32,
    pub saved_charset: Option<u8>,
    pub scrollback: usize,
    /// False for the alternate buffer: no history, no reflow.
    pub has_scrollback: bool,
}

pub const TAB_STOP_WIDTH: usize = 8;

impl Buffer {
    pub fn new(rows: usize, cols: usize, scrollback: usize, has_scrollback: bool) -> Self {
        let mut buffer = Self {
            lines: Lines::new(0),
            cols,
            rows,
            ybase: 0,
            y: 0,
            x: 0,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            tabs: BTreeSet::new(),
            saved_x: 0,
            saved_y: 0,
            saved_fg: 0,
            saved_bg: 0,
            saved_charset: None,
            scrollback,
            has_scrollback,
        };
        buffer.lines.max_len = buffer.correct_length(rows);
        buffer.setup_tab_stops(None);
        buffer
    }

    pub fn correct_length(&self, rows: usize) -> usize {
        if self.has_scrollback {
            rows + self.scrollback
        } else {
            rows
        }
    }

    pub fn is_full(&self) -> bool {
        self.lines.len() == self.lines.max_len
    }

    pub fn blank_line(cols: usize, fg: u32, bg: u32, wrapped: bool) -> Line {
        Line::new(cols, &Cell::null(fg, bg), wrapped)
    }

    /// `fillViewportRows`: only fills an empty buffer.
    pub fn fill_viewport_rows(&mut self, fg: u32, bg: u32) {
        if self.lines.is_empty() {
            for _ in 0..self.rows {
                self.lines
                    .push_back(Self::blank_line(self.cols, fg, bg, false));
            }
        }
    }

    /// `clear()`: drops all lines (the alt buffer is re-filled on activation).
    pub fn clear(&mut self) {
        self.ybase = 0;
        self.y = 0;
        self.x = 0;
        self.lines.replace(VecDeque::new());
        self.lines.max_len = self.correct_length(self.rows);
        self.scroll_top = 0;
        self.scroll_bottom = self.rows - 1;
        self.setup_tab_stops(None);
    }

    /// `CircularList.push`: pushing onto a full list drops the oldest row.
    pub fn push_line(&mut self, line: Line) {
        if self.lines.len() == self.lines.max_len && self.lines.max_len > 0 {
            self.lines.pop_front();
        }
        self.lines.push_back(line);
    }

    /// `lines.get(index)` as xterm.js reads it (see `Lines`).
    pub fn line_cyclic(&self, index: usize) -> Option<&Line> {
        self.lines
            .wrapped_index(index)
            .map(|i| &self.lines.deque[i])
    }

    /// `CircularList.shiftElements(start, count, 1)`: moves `count` rows down by one and may
    /// grow the list by one (trimming the oldest row when full). The row at `start` keeps
    /// its old content; the caller overwrites it.
    pub fn shift_elements_down(&mut self, start: usize, count: usize, cols: usize) {
        if count == 0 || start >= self.lines.len() {
            return;
        }
        let end = start + count + 1;
        while self.lines.len() < end {
            self.lines.push_back(Self::blank_line(cols, 0, 0, false));
        }
        self.lines.make_contiguous()[start..end].rotate_right(1);
        while self.lines.len() > self.lines.max_len {
            self.lines.pop_front();
        }
    }

    /// `CircularList.splice(index, 0, line)`: inserting past `max_len` trims the oldest row.
    pub fn insert_line(&mut self, index: usize, line: Line) {
        let index = index.min(self.lines.len());
        self.lines.insert(index, line);
        if self.lines.len() > self.lines.max_len {
            self.lines.pop_front();
        }
    }

    pub fn setup_tab_stops(&mut self, from: Option<usize>) {
        let mut i = match from {
            Some(i) => {
                if self.tabs.contains(&i) {
                    i
                } else {
                    self.prev_stop(Some(i))
                }
            }
            None => {
                self.tabs.clear();
                0
            }
        };
        while i < self.cols {
            self.tabs.insert(i);
            i += TAB_STOP_WIDTH;
        }
    }

    pub fn prev_stop(&self, x: Option<usize>) -> usize {
        let mut x = x.unwrap_or(self.x) as isize;
        loop {
            x -= 1;
            if self.tabs.contains(&(x.max(0) as usize)) && x >= 0 || x <= 0 {
                break;
            }
        }
        if x >= self.cols as isize {
            self.cols - 1
        } else if x < 0 {
            0
        } else {
            x as usize
        }
    }

    pub fn next_stop(&self, x: Option<usize>) -> usize {
        let mut x = x.unwrap_or(self.x);
        loop {
            x += 1;
            if self.tabs.contains(&x) || x >= self.cols {
                break;
            }
        }
        if x >= self.cols {
            self.cols - 1
        } else {
            x
        }
    }
}
