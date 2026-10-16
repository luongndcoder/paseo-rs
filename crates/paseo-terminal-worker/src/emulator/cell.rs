//! Cell and row storage, a port of xterm.js `AttributeData`, `CellData` and `BufferLine`
//! (@xterm/headless 6.0.0). fg/bg use xterm's packed layout so SGR handling and the
//! snapshot read the same bits xterm.js does.

pub const CM_MASK: u32 = 0x300_0000;
pub const CM_P16: u32 = 0x100_0000;
pub const CM_P256: u32 = 0x200_0000;
pub const CM_RGB: u32 = 0x300_0000;
pub const RGB_MASK: u32 = 0xFF_FFFF;
pub const PCOLOR_MASK: u32 = 0xFF;

pub const FG_INVERSE: u32 = 0x400_0000;
pub const FG_BOLD: u32 = 0x800_0000;
pub const FG_UNDERLINE: u32 = 0x1000_0000;
pub const FG_BLINK: u32 = 0x2000_0000;
pub const FG_INVISIBLE: u32 = 0x4000_0000;
pub const FG_STRIKETHROUGH: u32 = 0x8000_0000;

pub const BG_ITALIC: u32 = 0x400_0000;
pub const BG_DIM: u32 = 0x800_0000;
pub const BG_PROTECTED: u32 = 0x2000_0000;
pub const BG_OVERLINE: u32 = 0x4000_0000;

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    /// Code point; 0 means empty (or the second half of a wide glyph when `width == 0`).
    pub cp: u32,
    /// Base char plus combining chars, set once a combining char joined the cell.
    pub combined: Option<Box<str>>,
    pub width: u8,
    pub fg: u32,
    pub bg: u32,
}

impl Cell {
    /// `getNullCell(attr)`: empty cell, width 1.
    pub fn null(fg: u32, bg: u32) -> Self {
        Self {
            cp: 0,
            combined: None,
            width: 1,
            fg,
            bg,
        }
    }

    pub fn from_cp(cp: u32, width: u8, fg: u32, bg: u32) -> Self {
        Self {
            cp,
            combined: None,
            width,
            fg,
            bg,
        }
    }

    pub fn has_content(&self) -> bool {
        self.cp != 0 || self.combined.is_some()
    }

    /// `getChars()`: the glyph text, empty for empty cells.
    pub fn chars(&self) -> String {
        if let Some(text) = &self.combined {
            return text.to_string();
        }
        char::from_u32(self.cp)
            .filter(|_| self.cp != 0)
            .map(String::from)
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug)]
pub struct Line {
    pub cells: Vec<Cell>,
    /// xterm `isWrapped`: this row continues the previous row.
    pub wrapped: bool,
}

impl Line {
    pub fn new(cols: usize, fill: &Cell, wrapped: bool) -> Self {
        Self {
            cells: vec![fill.clone(); cols],
            wrapped,
        }
    }

    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn width(&self, i: usize) -> u8 {
        self.cells.get(i).map_or(0, |c| c.width)
    }

    pub fn has_content(&self, i: usize) -> bool {
        self.cells.get(i).is_some_and(Cell::has_content)
    }

    fn is_protected(&self, i: usize) -> bool {
        self.cells.get(i).is_some_and(|c| c.bg & BG_PROTECTED != 0)
    }

    pub fn trimmed_length(&self) -> usize {
        self.cells
            .iter()
            .enumerate()
            .rev()
            .find(|(_, cell)| cell.has_content())
            .map_or(0, |(i, cell)| i + usize::from(cell.width))
    }

    pub fn set_cell(&mut self, i: usize, cell: &Cell) {
        if let Some(slot) = self.cells.get_mut(i) {
            *slot = cell.clone();
        }
    }

    pub fn set_from_cp(&mut self, i: usize, cp: u32, width: u8, fg: u32, bg: u32) {
        if let Some(slot) = self.cells.get_mut(i) {
            *slot = Cell::from_cp(cp, width, fg, bg);
        }
    }

    pub fn add_codepoint(&mut self, i: usize, cp: u32, width: u8) {
        let Some(cell) = self.cells.get_mut(i) else {
            return;
        };
        let extra = char::from_u32(cp).unwrap_or('\u{FFFD}');
        if let Some(text) = &mut cell.combined {
            let mut owned = text.to_string();
            owned.push(extra);
            *text = owned.into_boxed_str();
        } else if cell.cp != 0 {
            let base = char::from_u32(cell.cp).unwrap_or('\u{FFFD}');
            cell.combined = Some(format!("{base}{extra}").into_boxed_str());
            cell.cp = 0;
        } else {
            cell.cp = cp;
            cell.width = 1;
        }
        if width != 0 {
            cell.width = width;
        }
    }

    pub fn insert_cells(&mut self, pos: usize, n: usize, fill: &Cell) {
        let len = self.cells.len();
        if len == 0 {
            return;
        }
        let pos = pos % len;
        if pos > 0 && self.width(pos - 1) == 2 {
            self.set_cell(pos - 1, &Cell::null(fill.fg, fill.bg));
        }
        if n < len - pos {
            for i in (0..len - pos - n).rev() {
                let moved = self.cells[pos + i].clone();
                self.cells[pos + n + i] = moved;
            }
            for i in 0..n {
                self.cells[pos + i] = fill.clone();
            }
        } else {
            for i in pos..len {
                self.cells[i] = fill.clone();
            }
        }
        if self.width(len - 1) == 2 {
            self.set_cell(len - 1, &Cell::null(fill.fg, fill.bg));
        }
    }

    pub fn delete_cells(&mut self, pos: usize, n: usize, fill: &Cell) {
        let len = self.cells.len();
        if len == 0 {
            return;
        }
        let pos = pos % len;
        if n < len - pos {
            for i in 0..len - pos - n {
                let moved = self.cells[pos + n + i].clone();
                self.cells[pos + i] = moved;
            }
            for i in len - n..len {
                self.cells[i] = fill.clone();
            }
        } else {
            for i in pos..len {
                self.cells[i] = fill.clone();
            }
        }
        if pos > 0 && self.width(pos - 1) == 2 {
            self.set_cell(pos - 1, &Cell::null(fill.fg, fill.bg));
        }
        if self.width(pos) == 0 && !self.has_content(pos) {
            self.set_cell(pos, &Cell::null(fill.fg, fill.bg));
        }
    }

    pub fn replace_cells(&mut self, start: usize, end: usize, fill: &Cell, respect_protect: bool) {
        let len = self.cells.len();
        let mut start = start;
        if respect_protect {
            if start > 0 && self.width(start - 1) == 2 && !self.is_protected(start - 1) {
                self.set_cell(start - 1, &Cell::null(fill.fg, fill.bg));
            }
            if end < len && end > 0 && self.width(end - 1) == 2 && !self.is_protected(end) {
                self.set_cell(end, &Cell::null(fill.fg, fill.bg));
            }
            while start < end && start < len {
                if !self.is_protected(start) {
                    self.cells[start] = fill.clone();
                }
                start += 1;
            }
            return;
        }
        if start > 0 && self.width(start - 1) == 2 {
            self.set_cell(start - 1, &Cell::null(fill.fg, fill.bg));
        }
        if end < len && end > 0 && self.width(end - 1) == 2 {
            self.set_cell(end, &Cell::null(fill.fg, fill.bg));
        }
        while start < end && start < len {
            self.cells[start] = fill.clone();
            start += 1;
        }
    }

    pub fn fill(&mut self, fill: &Cell, respect_protect: bool) {
        for i in 0..self.cells.len() {
            if !respect_protect || !self.is_protected(i) {
                self.cells[i] = fill.clone();
            }
        }
    }

    pub fn resize(&mut self, cols: usize, fill: &Cell) {
        self.cells.resize(cols, fill.clone());
    }
}
