//! Port of the xterm.js `EscapeSequenceParser` (@xterm/headless 6.0.0): the VT500
//! transition table, parameter/OSC/DCS handling and its error recovery. Input is code
//! points (xterm.js decodes the PTY string before parsing), so C1 characters
//! U+0080-U+009F act as controls exactly like they do there.

use std::sync::OnceLock;

use super::input::Params;
use super::term::Term;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Ground = 0,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    SosPmApcString,
    OscString,
    DcsEntry,
    DcsParam,
    DcsIgnore,
    DcsIntermediate,
    DcsPassthrough,
}

const STATES: [State; 14] = [
    State::Ground,
    State::Escape,
    State::EscapeIntermediate,
    State::CsiEntry,
    State::CsiParam,
    State::CsiIntermediate,
    State::CsiIgnore,
    State::SosPmApcString,
    State::OscString,
    State::DcsEntry,
    State::DcsParam,
    State::DcsIgnore,
    State::DcsIntermediate,
    State::DcsPassthrough,
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Ignore,
    Error,
    Print,
    Execute,
    OscStart,
    OscPut,
    OscEnd,
    CsiDispatch,
    Param,
    Collect,
    EscDispatch,
    Clear,
    DcsHook,
    DcsPut,
    DcsUnhook,
}

const NON_ASCII_PRINTABLE: u32 = 0xA0;
const MAX_VALUE: i64 = 0x7FFF_FFFF;

type Table = Vec<(Action, State)>;

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(build_table)
}

fn build_table() -> Table {
    use Action as A;
    use State as S;
    let mut table: Table = vec![(A::Error, S::Ground); 14 * 256];
    let mut add_many = |codes: &[u32], state: State, action: Action, next: State| {
        for &code in codes {
            table[(state as usize) << 8 | code as usize] = (action, next);
        }
    };
    let r = |start: u32, end: u32| -> Vec<u32> { (start..end).collect() };

    let printables = r(0x20, 0x7f);
    let mut executables = r(0x00, 0x18);
    executables.push(0x19);
    executables.extend(r(0x1c, 0x20));

    add_many(&printables, S::Ground, A::Print, S::Ground);
    for state in STATES {
        add_many(&[0x18, 0x1a, 0x99, 0x9a], state, A::Execute, S::Ground);
        add_many(&r(0x80, 0x90), state, A::Execute, S::Ground);
        add_many(&r(0x90, 0x98), state, A::Execute, S::Ground);
        add_many(&[0x9c], state, A::Ignore, S::Ground);
        add_many(&[0x1b], state, A::Clear, S::Escape);
        add_many(&[0x9d], state, A::OscStart, S::OscString);
        add_many(&[0x98, 0x9e, 0x9f], state, A::Ignore, S::SosPmApcString);
        add_many(&[0x9b], state, A::Clear, S::CsiEntry);
        add_many(&[0x90], state, A::Clear, S::DcsEntry);
    }
    add_many(&executables, S::Ground, A::Execute, S::Ground);
    add_many(&executables, S::Escape, A::Execute, S::Escape);
    add_many(&[0x7f], S::Escape, A::Ignore, S::Escape);
    add_many(&executables, S::OscString, A::Ignore, S::OscString);
    add_many(&executables, S::CsiEntry, A::Execute, S::CsiEntry);
    add_many(&[0x7f], S::CsiEntry, A::Ignore, S::CsiEntry);
    add_many(&executables, S::CsiParam, A::Execute, S::CsiParam);
    add_many(&[0x7f], S::CsiParam, A::Ignore, S::CsiParam);
    add_many(&executables, S::CsiIgnore, A::Execute, S::CsiIgnore);
    add_many(
        &executables,
        S::CsiIntermediate,
        A::Execute,
        S::CsiIntermediate,
    );
    add_many(&[0x7f], S::CsiIntermediate, A::Ignore, S::CsiIntermediate);
    add_many(
        &executables,
        S::EscapeIntermediate,
        A::Execute,
        S::EscapeIntermediate,
    );
    add_many(
        &[0x7f],
        S::EscapeIntermediate,
        A::Ignore,
        S::EscapeIntermediate,
    );
    add_many(&[0x5d], S::Escape, A::OscStart, S::OscString);
    add_many(&printables, S::OscString, A::OscPut, S::OscString);
    add_many(&[0x7f], S::OscString, A::OscPut, S::OscString);
    add_many(
        &[0x9c, 0x1b, 0x18, 0x1a, 0x07],
        S::OscString,
        A::OscEnd,
        S::Ground,
    );
    add_many(&r(0x1c, 0x20), S::OscString, A::Ignore, S::OscString);
    add_many(&[0x58, 0x5e, 0x5f], S::Escape, A::Ignore, S::SosPmApcString);
    add_many(&printables, S::SosPmApcString, A::Ignore, S::SosPmApcString);
    add_many(
        &executables,
        S::SosPmApcString,
        A::Ignore,
        S::SosPmApcString,
    );
    add_many(&[0x9c], S::SosPmApcString, A::Ignore, S::Ground);
    add_many(&[0x7f], S::SosPmApcString, A::Ignore, S::SosPmApcString);
    add_many(&[0x5b], S::Escape, A::Clear, S::CsiEntry);
    add_many(&r(0x40, 0x7f), S::CsiEntry, A::CsiDispatch, S::Ground);
    add_many(&r(0x30, 0x3c), S::CsiEntry, A::Param, S::CsiParam);
    add_many(
        &[0x3c, 0x3d, 0x3e, 0x3f],
        S::CsiEntry,
        A::Collect,
        S::CsiParam,
    );
    add_many(&r(0x30, 0x3c), S::CsiParam, A::Param, S::CsiParam);
    add_many(&r(0x40, 0x7f), S::CsiParam, A::CsiDispatch, S::Ground);
    add_many(
        &[0x3c, 0x3d, 0x3e, 0x3f],
        S::CsiParam,
        A::Ignore,
        S::CsiIgnore,
    );
    add_many(&r(0x20, 0x40), S::CsiIgnore, A::Ignore, S::CsiIgnore);
    add_many(&[0x7f], S::CsiIgnore, A::Ignore, S::CsiIgnore);
    add_many(&r(0x40, 0x7f), S::CsiIgnore, A::Ignore, S::Ground);
    add_many(&r(0x20, 0x30), S::CsiEntry, A::Collect, S::CsiIntermediate);
    add_many(
        &r(0x20, 0x30),
        S::CsiIntermediate,
        A::Collect,
        S::CsiIntermediate,
    );
    add_many(&r(0x30, 0x40), S::CsiIntermediate, A::Ignore, S::CsiIgnore);
    add_many(
        &r(0x40, 0x7f),
        S::CsiIntermediate,
        A::CsiDispatch,
        S::Ground,
    );
    add_many(&r(0x20, 0x30), S::CsiParam, A::Collect, S::CsiIntermediate);
    add_many(&r(0x20, 0x30), S::Escape, A::Collect, S::EscapeIntermediate);
    add_many(
        &r(0x20, 0x30),
        S::EscapeIntermediate,
        A::Collect,
        S::EscapeIntermediate,
    );
    add_many(
        &r(0x30, 0x7f),
        S::EscapeIntermediate,
        A::EscDispatch,
        S::Ground,
    );
    add_many(&r(0x30, 0x50), S::Escape, A::EscDispatch, S::Ground);
    add_many(&r(0x51, 0x58), S::Escape, A::EscDispatch, S::Ground);
    add_many(&[0x59, 0x5a, 0x5c], S::Escape, A::EscDispatch, S::Ground);
    add_many(&r(0x60, 0x7f), S::Escape, A::EscDispatch, S::Ground);
    add_many(&[0x50], S::Escape, A::Clear, S::DcsEntry);
    add_many(&executables, S::DcsEntry, A::Ignore, S::DcsEntry);
    add_many(&[0x7f], S::DcsEntry, A::Ignore, S::DcsEntry);
    add_many(&r(0x1c, 0x20), S::DcsEntry, A::Ignore, S::DcsEntry);
    add_many(&r(0x20, 0x30), S::DcsEntry, A::Collect, S::DcsIntermediate);
    add_many(&r(0x30, 0x3c), S::DcsEntry, A::Param, S::DcsParam);
    add_many(
        &[0x3c, 0x3d, 0x3e, 0x3f],
        S::DcsEntry,
        A::Collect,
        S::DcsParam,
    );
    add_many(&executables, S::DcsIgnore, A::Ignore, S::DcsIgnore);
    add_many(&r(0x20, 0x80), S::DcsIgnore, A::Ignore, S::DcsIgnore);
    add_many(&r(0x1c, 0x20), S::DcsIgnore, A::Ignore, S::DcsIgnore);
    add_many(&executables, S::DcsParam, A::Ignore, S::DcsParam);
    add_many(&[0x7f], S::DcsParam, A::Ignore, S::DcsParam);
    add_many(&r(0x1c, 0x20), S::DcsParam, A::Ignore, S::DcsParam);
    add_many(&r(0x30, 0x3c), S::DcsParam, A::Param, S::DcsParam);
    add_many(
        &[0x3c, 0x3d, 0x3e, 0x3f],
        S::DcsParam,
        A::Ignore,
        S::DcsIgnore,
    );
    add_many(&r(0x20, 0x30), S::DcsParam, A::Collect, S::DcsIntermediate);
    add_many(
        &executables,
        S::DcsIntermediate,
        A::Ignore,
        S::DcsIntermediate,
    );
    add_many(&[0x7f], S::DcsIntermediate, A::Ignore, S::DcsIntermediate);
    add_many(
        &r(0x1c, 0x20),
        S::DcsIntermediate,
        A::Ignore,
        S::DcsIntermediate,
    );
    add_many(
        &r(0x20, 0x30),
        S::DcsIntermediate,
        A::Collect,
        S::DcsIntermediate,
    );
    add_many(&r(0x30, 0x40), S::DcsIntermediate, A::Ignore, S::DcsIgnore);
    add_many(
        &r(0x40, 0x7f),
        S::DcsIntermediate,
        A::DcsHook,
        S::DcsPassthrough,
    );
    add_many(&r(0x40, 0x7f), S::DcsParam, A::DcsHook, S::DcsPassthrough);
    add_many(&r(0x40, 0x7f), S::DcsEntry, A::DcsHook, S::DcsPassthrough);
    add_many(
        &executables,
        S::DcsPassthrough,
        A::DcsPut,
        S::DcsPassthrough,
    );
    add_many(&printables, S::DcsPassthrough, A::DcsPut, S::DcsPassthrough);
    add_many(&[0x7f], S::DcsPassthrough, A::Ignore, S::DcsPassthrough);
    add_many(
        &[0x1b, 0x9c, 0x18, 0x1a],
        S::DcsPassthrough,
        A::DcsUnhook,
        S::Ground,
    );
    add_many(&[NON_ASCII_PRINTABLE], S::Ground, A::Print, S::Ground);
    add_many(
        &[NON_ASCII_PRINTABLE],
        S::OscString,
        A::OscPut,
        S::OscString,
    );
    add_many(
        &[NON_ASCII_PRINTABLE],
        S::CsiIgnore,
        A::Ignore,
        S::CsiIgnore,
    );
    add_many(
        &[NON_ASCII_PRINTABLE],
        S::DcsIgnore,
        A::Ignore,
        S::DcsIgnore,
    );
    add_many(
        &[NON_ASCII_PRINTABLE],
        S::DcsPassthrough,
        A::DcsPut,
        S::DcsPassthrough,
    );
    table
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OscState {
    Start,
    Id,
    Payload,
    Abort,
}

/// xterm `Params` plus the limits/rejection flags of its digit accumulation.
struct ParamBuilder {
    params: Params,
    sub_total: usize,
    reject_digits: bool,
    reject_sub_digits: bool,
    digit_is_sub: bool,
}

const MAX_PARAMS: usize = 32;
const MAX_SUBPARAMS: usize = 32;

impl ParamBuilder {
    fn new() -> Self {
        let mut builder = Self {
            params: Params::default(),
            sub_total: 0,
            reject_digits: false,
            reject_sub_digits: false,
            digit_is_sub: false,
        };
        builder.add_param(0);
        builder
    }

    fn clear(&mut self) {
        self.params.items.clear();
        self.sub_total = 0;
        self.reject_digits = false;
        self.reject_sub_digits = false;
        self.digit_is_sub = false;
        self.add_param(0);
    }

    fn add_param(&mut self, value: i64) {
        self.digit_is_sub = false;
        if self.params.items.len() >= MAX_PARAMS {
            self.reject_digits = true;
            return;
        }
        self.params.items.push((value.min(MAX_VALUE), Vec::new()));
    }

    fn add_sub_param(&mut self, value: i64) {
        self.digit_is_sub = true;
        if self.params.items.is_empty() {
            return;
        }
        if self.reject_digits || self.sub_total >= MAX_SUBPARAMS {
            self.reject_sub_digits = true;
            return;
        }
        if let Some(last) = self.params.items.last_mut() {
            last.1.push(value.min(MAX_VALUE));
            self.sub_total += 1;
        }
    }

    fn add_digit(&mut self, digit: i64) {
        let length = if self.digit_is_sub {
            self.sub_total
        } else {
            self.params.items.len()
        };
        if self.reject_digits || length == 0 || (self.digit_is_sub && self.reject_sub_digits) {
            return;
        }
        let slot: Option<&mut i64> = if self.digit_is_sub {
            self.params
                .items
                .iter_mut()
                .rev()
                .find_map(|p| p.1.last_mut())
        } else {
            self.params.items.last_mut().map(|p| &mut p.0)
        };
        if let Some(cur) = slot {
            *cur = if *cur != -1 {
                (*cur * 10 + digit).min(MAX_VALUE)
            } else {
                digit
            };
        }
    }
}

pub struct Parser {
    state: State,
    params: ParamBuilder,
    collect: Vec<u8>,
    osc_state: OscState,
    osc_id: i64,
    osc_data: String,
}

impl Parser {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            params: ParamBuilder::new(),
            collect: Vec::new(),
            osc_state: OscState::Start,
            osc_id: -1,
            osc_data: String::new(),
        }
    }

    fn clear(&mut self) {
        self.params.clear();
        self.collect.clear();
    }

    fn osc_reset(&mut self) {
        self.osc_state = OscState::Start;
        self.osc_id = -1;
        self.osc_data.clear();
    }

    fn osc_put(&mut self, chars: &[char]) {
        let mut start = 0usize;
        if self.osc_state == OscState::Abort {
            return;
        }
        if self.osc_state == OscState::Id {
            while start < chars.len() {
                let code = u32::from(chars[start]);
                start += 1;
                if code == 0x3b {
                    self.osc_state = OscState::Payload;
                    break;
                }
                if !(0x30..=0x39).contains(&code) {
                    self.osc_state = OscState::Abort;
                    return;
                }
                if self.osc_id == -1 {
                    self.osc_id = 0;
                }
                self.osc_id = self.osc_id * 10 + i64::from(code) - 48;
            }
        }
        if self.osc_state == OscState::Payload && start < chars.len() {
            self.osc_data.extend(&chars[start..]);
        }
    }

    fn osc_end(&mut self, term: &mut Term, success: bool) {
        if self.osc_state == OscState::Start {
            return;
        }
        if self.osc_state != OscState::Abort && success {
            // Only OSC 0 and 2 change state the snapshot can see (the window title).
            if self.osc_id == 0 || self.osc_id == 2 {
                term.set_title(&self.osc_data);
            }
        }
        self.osc_reset();
    }

    /// `EscapeSequenceParser.parse`.
    pub fn parse(&mut self, term: &mut Term, data: &[char]) {
        let table = table();
        let length = data.len();
        let mut i = 0usize;
        while i < length {
            let mut code = u32::from(data[i]);
            let index = (self.state as usize) << 8
                | (if code < 0xa0 {
                    code
                } else {
                    NON_ASCII_PRINTABLE
                }) as usize;
            let (action, mut next) = table[index];
            match action {
                Action::Print => {
                    let mut j = i + 1;
                    loop {
                        if j >= length {
                            break;
                        }
                        let c = u32::from(data[j]);
                        if c < 0x20 || (c > 0x7e && c < NON_ASCII_PRINTABLE) {
                            break;
                        }
                        j += 1;
                    }
                    term.print(&data[i..j]);
                    i = j - 1;
                }
                Action::Execute => {
                    execute(term, code);
                    term.preceding_join = 0;
                }
                Action::Ignore => {}
                Action::Error => {}
                Action::CsiDispatch => {
                    csi_dispatch(term, &self.collect, &self.params.params, code);
                    term.preceding_join = 0;
                }
                Action::Param => {
                    loop {
                        match code {
                            0x3b => self.params.add_param(0),
                            0x3a => self.params.add_sub_param(-1),
                            _ => self.params.add_digit(i64::from(code) - 48),
                        }
                        i += 1;
                        if i < length {
                            code = u32::from(data[i]);
                            if code > 0x2f && code < 0x3c {
                                continue;
                            }
                        }
                        break;
                    }
                    i -= 1;
                }
                Action::Collect => {
                    self.collect.push(code as u8);
                }
                Action::EscDispatch => {
                    esc_dispatch(term, &self.collect, code as u8);
                    term.preceding_join = 0;
                }
                Action::Clear => {
                    self.clear();
                }
                Action::DcsHook => {}
                Action::DcsPut => {
                    let mut j = i + 1;
                    loop {
                        if j >= length {
                            break;
                        }
                        let c = u32::from(data[j]);
                        if c == 0x18
                            || c == 0x1a
                            || c == 0x1b
                            || (c > 0x7f && c < NON_ASCII_PRINTABLE)
                        {
                            break;
                        }
                        j += 1;
                    }
                    i = j - 1;
                }
                Action::DcsUnhook => {
                    if code == 0x1b {
                        next = State::Escape;
                    }
                    self.clear();
                    term.preceding_join = 0;
                }
                Action::OscStart => {
                    self.osc_reset();
                    self.osc_state = OscState::Id;
                }
                Action::OscPut => {
                    let mut j = i + 1;
                    loop {
                        if j >= length {
                            break;
                        }
                        let c = u32::from(data[j]);
                        if c < 0x20 || (c > 0x7f && c < NON_ASCII_PRINTABLE) {
                            break;
                        }
                        j += 1;
                    }
                    self.osc_put(&data[i..j]);
                    i = j - 1;
                }
                Action::OscEnd => {
                    // OSC with an id but no `;` still announces START and END (empty payload).
                    if self.osc_state == OscState::Id {
                        self.osc_state = OscState::Payload;
                    }
                    self.osc_end(term, code != 0x18 && code != 0x1a);
                    if code == 0x1b {
                        next = State::Escape;
                    }
                    self.clear();
                    term.preceding_join = 0;
                }
            }
            self.state = next;
            i += 1;
        }
    }
}

fn execute(term: &mut Term, code: u32) {
    match code {
        0x0a..=0x0c => term.line_feed(),
        0x0d => term.carriage_return(),
        0x08 => term.backspace(),
        0x09 => term.tab(),
        0x0e => term.shift_out(),
        0x0f => term.shift_in(),
        0x84 => term.index(),
        0x85 => term.next_line(),
        0x88 => term.tab_set(),
        _ => {}
    }
}

fn csi_dispatch(t: &mut Term, collect: &[u8], p: &Params, code: u32) {
    let (prefix, inter): (Option<u8>, &[u8]) = match collect.first() {
        Some(&b @ (b'<' | b'=' | b'>' | b'?')) => (Some(b), &collect[1..]),
        _ => (None, collect),
    };
    let Some(action) = char::from_u32(code) else {
        return;
    };
    match (prefix, inter, action) {
        (None, [], '@') => t.insert_chars(p),
        (None, [b' '], '@') => t.scroll_left(p),
        (None, [], 'A') => t.cursor_up(p),
        (None, [b' '], 'A') => t.scroll_right(p),
        (None, [], 'B') => t.cursor_down(p),
        (None, [], 'C') => t.cursor_forward(p),
        (None, [], 'D') => t.cursor_backward(p),
        (None, [], 'E') => t.cursor_next_line(p),
        (None, [], 'F') => t.cursor_preceding_line(p),
        (None, [], 'G') => t.cursor_char_absolute(p),
        (None, [], 'H') => t.cursor_position(p),
        (None, [], 'I') => t.cursor_forward_tab(p),
        (None, [], 'J') => t.erase_in_display(p, false),
        (Some(b'?'), [], 'J') => t.erase_in_display(p, true),
        (None, [], 'K') => t.erase_in_line(p, false),
        (Some(b'?'), [], 'K') => t.erase_in_line(p, true),
        (None, [], 'L') => t.insert_lines(p),
        (None, [], 'M') => t.delete_lines(p),
        (None, [], 'P') => t.delete_chars(p),
        (None, [], 'S') => t.scroll_up(p),
        (None, [], 'T') => t.scroll_down(p),
        (None, [], 'X') => t.erase_chars(p),
        (None, [], 'Z') => t.cursor_backward_tab(p),
        (None, [], '`') => t.cursor_char_absolute(p),
        (None, [], 'a') => t.cursor_forward(p),
        (None, [], 'b') => t.repeat_preceding_character(p),
        (None, [], 'd') => t.line_pos_absolute(p),
        (None, [], 'e') => t.v_position_relative(p),
        (None, [], 'f') => t.cursor_position(p),
        (None, [], 'g') => t.tab_clear(p),
        (None, [], 'h') => t.set_mode(p),
        (Some(b'?'), [], 'h') => t.set_mode_private(p),
        (None, [], 'l') => t.reset_mode(p),
        (Some(b'?'), [], 'l') => t.reset_mode_private(p),
        (None, [], 'm') => t.char_attributes(p),
        (None, [b'!'], 'p') => t.soft_reset(),
        (None, [b' '], 'q') => t.set_cursor_style(p),
        (None, [], 'r') => t.set_scroll_region(p),
        (None, [], 's') => t.save_cursor(),
        (None, [], 'u') => t.restore_cursor(),
        (None, [b'\''], '}') => t.insert_columns(p),
        (None, [b'\''], '~') => t.delete_columns(p),
        (None, [b'"'], 'q') => t.select_protected(p),
        _ => {}
    }
}

fn esc_dispatch(t: &mut Term, collect: &[u8], code: u8) {
    match (collect, code) {
        ([], b'7') => t.save_cursor(),
        ([], b'8') => t.restore_cursor(),
        ([], b'D') => t.index(),
        ([], b'E') => t.next_line(),
        ([], b'H') => t.tab_set(),
        ([], b'M') => t.reverse_index(),
        ([], b'c') => t.full_reset(),
        ([], b'n') => t.set_glevel(2),
        ([], b'o') | ([], b'|') => t.set_glevel(3),
        ([], b'}') => t.set_glevel(2),
        ([], b'~') => t.set_glevel(1),
        ([b'%'], b'@') | ([b'%'], b'G') => t.select_default_charset(),
        ([b'#'], b'8') => t.screen_alignment_pattern(),
        ([designator @ (b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/')], flag)
            if super::charsets::is_known(flag) =>
        {
            t.select_charset(*designator, flag);
        }
        _ => {}
    }
}
