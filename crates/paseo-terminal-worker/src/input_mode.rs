//! Port of packages/protocol/src/terminal-input-mode.ts (TerminalInputModeTracker).

use regex::Regex;
use std::sync::OnceLock;

const ESC: char = '\u{1b}';

fn csi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\x1b\[(?:([<>=?]?)([0-9;]*)u|\?([0-9;]*)([hl]))").expect("valid regex")
    })
}

fn incomplete_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1b\[[<>=?]?[0-9;]*$").expect("valid regex"))
}

fn first_param(params: &str) -> Option<u32> {
    let first = params.split(';').next()?;
    if first.is_empty() || !first.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    first.parse().ok()
}

fn second_param(params: &str) -> Option<u32> {
    let second = params.split(';').nth(1)?;
    if second.is_empty() || !second.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    second.parse().ok()
}

#[derive(Default)]
pub struct InputModeTracker {
    kitty_flags: u32,
    win32_input_mode: bool,
    application_cursor_keys: bool,
    bracketed_paste: bool,
    kitty_stack: Vec<u32>,
    pending: String,
}

impl InputModeTracker {
    /// Returns responses that must be written back to the PTY.
    pub fn feed(&mut self, data: &str) -> Vec<String> {
        let mut responses = Vec::new();
        if data.is_empty() {
            return responses;
        }
        let text = format!("{}{}", std::mem::take(&mut self.pending), data);
        let mut consumed_until = 0;
        for caps in csi_re().captures_iter(&text) {
            let whole = caps.get(0).expect("group 0");
            consumed_until = whole.end();
            if let Some(fin) = caps.get(4) {
                let params = caps.get(3).map_or("", |m| m.as_str());
                self.apply_private_mode(params, fin.as_str());
                continue;
            }
            let prefix = caps.get(1).map_or("", |m| m.as_str());
            let params = caps.get(2).map_or("", |m| m.as_str());
            if let Some(resp) = self.apply_kitty(prefix, params) {
                responses.push(resp);
            }
        }
        let tail = &text[consumed_until..];
        if let Some(start) = tail.rfind(&format!("{ESC}[")) {
            let pending = &tail[start..];
            if incomplete_re().is_match(pending) {
                self.pending = pending.to_owned();
            }
        }
        responses
    }

    pub fn preamble(&self) -> String {
        let mut out = String::new();
        if self.kitty_flags > 0 {
            out.push_str(&format!("\x1b[={};1u", self.kitty_flags));
        }
        if self.win32_input_mode {
            out.push_str("\x1b[?9001h");
        }
        if self.application_cursor_keys {
            out.push_str("\x1b[?1h");
        }
        if self.bracketed_paste {
            out.push_str("\x1b[?2004h");
        }
        out
    }

    fn apply_kitty(&mut self, prefix: &str, params: &str) -> Option<String> {
        match prefix {
            ">" => {
                self.kitty_stack.push(self.kitty_flags);
                self.kitty_flags = first_param(params).unwrap_or(1);
            }
            "=" => {
                let mode = second_param(params).unwrap_or(1);
                self.kitty_flags = if mode == 0 {
                    0
                } else {
                    first_param(params).unwrap_or(0)
                };
            }
            "<" => {
                let count = first_param(params).unwrap_or(1).max(1);
                for _ in 0..count {
                    self.kitty_flags = self.kitty_stack.pop().unwrap_or(0);
                }
            }
            "?" => return Some(format!("\x1b[?{}u", self.kitty_flags)),
            _ => {}
        }
        None
    }

    fn apply_private_mode(&mut self, params: &str, fin: &str) {
        let on = fin == "h";
        for p in params.split(';') {
            match p.parse::<u32>() {
                Ok(9001) => self.win32_input_mode = on,
                Ok(1) => self.application_cursor_keys = on,
                Ok(2004) => self.bracketed_paste = on,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_modes_and_builds_preamble() {
        let mut t = InputModeTracker::default();
        assert!(t.feed("\x1b[>1u\x1b[?2004h\x1b[?1h").is_empty());
        assert_eq!(t.preamble(), "\x1b[=1;1u\x1b[?1h\x1b[?2004h");
        t.feed("\x1b[?2004l\x1b[<u");
        assert_eq!(t.preamble(), "\x1b[?1h");
    }

    #[test]
    fn answers_kitty_query_and_handles_split_sequence() {
        let mut t = InputModeTracker::default();
        t.feed("\x1b[>5u");
        assert_eq!(t.feed("\x1b[?u"), vec!["\x1b[?5u".to_string()]);
        t.feed("\x1b[?90");
        t.feed("01h");
        assert!(t.preamble().contains("\x1b[?9001h"));
    }
}
