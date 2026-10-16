//! `InputHandler` modes, cursor style and SGR handlers (@xterm/headless 6.0.0).

use super::cell::*;
use super::input::Params;
use super::term::{CursorShape, Term};

impl Term {
    // ---- modes ---------------------------------------------------------------------------------

    pub fn set_mode(&mut self, p: &Params) {
        for i in 0..p.len() {
            match p.get(i) {
                4 => self.insert_mode = true,
                20 => self.convert_eol = true,
                _ => {}
            }
        }
    }

    pub fn reset_mode(&mut self, p: &Params) {
        for i in 0..p.len() {
            match p.get(i) {
                4 => self.insert_mode = false,
                20 => self.convert_eol = false,
                _ => {}
            }
        }
    }

    pub fn set_mode_private(&mut self, p: &Params) {
        for i in 0..p.len() {
            match p.get(i) {
                2 => {
                    for g in 0..4 {
                        self.set_gcharset(g, None);
                    }
                }
                6 => {
                    self.origin = true;
                    self.set_cursor(0, 0);
                }
                7 => self.wraparound = true,
                45 => self.reverse_wraparound = true,
                25 => self.cursor_hidden = false,
                1048 => self.save_cursor(),
                1049 => {
                    self.save_cursor();
                    self.activate_alt();
                }
                47 | 1047 => self.activate_alt(),
                _ => {}
            }
        }
    }

    pub fn reset_mode_private(&mut self, p: &Params) {
        for i in 0..p.len() {
            match p.get(i) {
                6 => {
                    self.origin = false;
                    self.set_cursor(0, 0);
                }
                7 => self.wraparound = false,
                45 => self.reverse_wraparound = false,
                25 => self.cursor_hidden = true,
                1048 => self.restore_cursor(),
                v @ (1049 | 47 | 1047) => {
                    self.activate_normal();
                    if v == 1049 {
                        self.restore_cursor();
                    }
                }
                _ => {}
            }
        }
    }

    pub fn soft_reset(&mut self) {
        self.cursor_hidden = false;
        let rows = self.rows;
        {
            let b = self.buf_mut();
            b.scroll_top = 0;
            b.scroll_bottom = rows - 1;
        }
        self.cur_fg = 0;
        self.cur_bg = 0;
        // coreService.reset(): modes and DEC private modes back to defaults.
        self.insert_mode = false;
        self.wraparound = true;
        self.origin = false;
        self.reverse_wraparound = false;
        self.cursor_style = None;
        self.cursor_blink = None;
        self.charsets = [None; 4];
        self.glevel = 0;
        self.charset = None;
        let charset = self.charset;
        let b = self.buf_mut();
        b.saved_x = 0;
        b.saved_y = b.ybase;
        b.saved_fg = 0;
        b.saved_bg = 0;
        b.saved_charset = charset;
    }

    pub fn set_cursor_style(&mut self, p: &Params) {
        let param = if p.len() == 0 { 1 } else { p.get(0) };
        if param == 0 {
            self.cursor_style = None;
            self.cursor_blink = None;
        } else {
            match param {
                1 | 2 => self.cursor_style = Some(CursorShape::Block),
                3 | 4 => self.cursor_style = Some(CursorShape::Underline),
                5 | 6 => self.cursor_style = Some(CursorShape::Bar),
                _ => {}
            }
            self.cursor_blink = Some(param % 2 == 1);
        }
    }

    // ---- SGR -----------------------------------------------------------------------------------

    fn update_attr_color(color: u32, mode: i64, c1: i64, c2: i64, c3: i64) -> u32 {
        let mut color = color;
        if mode == 2 {
            color |= CM_RGB;
            color &= !RGB_MASK;
            color |=
                (((c1 & 0xFF) as u32) << 16) | (((c2 & 0xFF) as u32) << 8) | ((c3 & 0xFF) as u32);
        } else if mode == 5 {
            color &= !(CM_MASK | PCOLOR_MASK);
            color |= CM_P256 | ((c1 & 0xFF) as u32);
        }
        color
    }

    /// `_extractColor`; returns how many extra params were consumed.
    fn extract_color(&mut self, p: &Params, pos: usize) -> usize {
        let mut accu: [i64; 6] = [0, 0, -1, 0, 0, 0];
        let mut c_space = 0usize;
        let mut advance = 0usize;
        loop {
            accu[advance + c_space] = p.get(pos + advance);
            if p.has_sub(pos + advance) {
                let sub = &p.items[pos + advance].1;
                let mut i = 0usize;
                loop {
                    if accu[1] == 5 {
                        c_space = 1;
                    }
                    accu[advance + i + 1 + c_space] = sub[i];
                    i += 1;
                    if !(i < sub.len() && i + advance + 1 + c_space < accu.len()) {
                        break;
                    }
                }
                break;
            }
            if (accu[1] == 5 && advance + c_space >= 2) || (accu[1] == 2 && advance + c_space >= 5)
            {
                break;
            }
            if accu[1] != 0 {
                c_space = 1;
            }
            advance += 1;
            if !(advance + pos < p.len() && advance + c_space < accu.len()) {
                break;
            }
        }
        for slot in accu.iter_mut().skip(2) {
            if *slot == -1 {
                *slot = 0;
            }
        }
        match accu[0] {
            38 => {
                self.cur_fg =
                    Self::update_attr_color(self.cur_fg, accu[1], accu[3], accu[4], accu[5])
            }
            48 => {
                self.cur_bg =
                    Self::update_attr_color(self.cur_bg, accu[1], accu[3], accu[4], accu[5])
            }
            _ => {}
        }
        advance
    }

    pub fn char_attributes(&mut self, p: &Params) {
        if p.len() == 1 && p.get(0) == 0 {
            self.cur_fg = 0;
            self.cur_bg = 0;
            return;
        }
        let mut i = 0usize;
        while i < p.len() {
            let v = p.get(i);
            match v {
                30..=37 => {
                    self.cur_fg &= !(CM_MASK | PCOLOR_MASK);
                    self.cur_fg |= CM_P16 | (v - 30) as u32;
                }
                40..=47 => {
                    self.cur_bg &= !(CM_MASK | PCOLOR_MASK);
                    self.cur_bg |= CM_P16 | (v - 40) as u32;
                }
                90..=97 => {
                    self.cur_fg &= !(CM_MASK | PCOLOR_MASK);
                    self.cur_fg |= CM_P16 | (v - 90) as u32 | 8;
                }
                100..=107 => {
                    self.cur_bg &= !(CM_MASK | PCOLOR_MASK);
                    self.cur_bg |= CM_P16 | (v - 100) as u32 | 8;
                }
                0 => {
                    self.cur_fg = 0;
                    self.cur_bg = 0;
                }
                1 => self.cur_fg |= FG_BOLD,
                3 => self.cur_bg |= BG_ITALIC,
                4 => {
                    self.cur_fg |= FG_UNDERLINE;
                    let style = if p.has_sub(i) { p.items[i].1[0] } else { 1 };
                    let style = if style == -1 || style > 5 { 1 } else { style };
                    if style == 0 {
                        self.cur_fg &= !FG_UNDERLINE;
                    }
                }
                5 => self.cur_fg |= FG_BLINK,
                7 => self.cur_fg |= FG_INVERSE,
                8 => self.cur_fg |= FG_INVISIBLE,
                9 => self.cur_fg |= FG_STRIKETHROUGH,
                2 => self.cur_bg |= BG_DIM,
                21 => self.cur_fg |= FG_UNDERLINE,
                22 => {
                    self.cur_fg &= !FG_BOLD;
                    self.cur_bg &= !BG_DIM;
                }
                23 => self.cur_bg &= !BG_ITALIC,
                24 => self.cur_fg &= !FG_UNDERLINE,
                25 => self.cur_fg &= !FG_BLINK,
                27 => self.cur_fg &= !FG_INVERSE,
                28 => self.cur_fg &= !FG_INVISIBLE,
                29 => self.cur_fg &= !FG_STRIKETHROUGH,
                39 => self.cur_fg &= !(CM_MASK | RGB_MASK),
                49 => self.cur_bg &= !(CM_MASK | RGB_MASK),
                38 | 48 | 58 => i += self.extract_color(p, i),
                53 => self.cur_bg |= BG_OVERLINE,
                55 => self.cur_bg &= !BG_OVERLINE,
                _ => {}
            }
            i += 1;
        }
    }
}
