//! Escape-sequence scanner for the control traffic the TS worker handles through xterm.js parser
//! hooks: DA1, DSR 5/6, DECDSR 6, OSC 10/11/12 colour queries, and OSC 633 command-finished.
//! The emulator never sees responses; the runtime writes them to the PTY. State survives chunk
//! boundaries.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    Da1,
    Dsr5,
    Dsr6,
    DecDsr6,
    OscColorQuery(u8),
    CommandFinished(Option<i64>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Byte offset in the scanned chunk just past the terminating byte of the sequence.
    pub end: usize,
    pub control: Control,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum St {
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
    Str,
    StrEsc,
}

const OSC_CAP: usize = 4096;

pub struct Scanner {
    st: St,
    csi: String,
    csi_has_intermediate: bool,
    osc: String,
}

impl Default for Scanner {
    fn default() -> Self {
        Self {
            st: St::Ground,
            csi: String::new(),
            csi_has_intermediate: false,
            osc: String::new(),
        }
    }
}

/// `D` or `D;<int>` (the only shape our shell integration emits).
pub fn parse_command_finished(data: &str) -> Option<Control> {
    let parts: Vec<&str> = data.split(';').collect();
    if parts[0] != "D" {
        return None;
    }
    if parts.len() == 1 {
        return Some(Control::CommandFinished(None));
    }
    if parts.len() != 2 {
        return None;
    }
    let p = parts[1];
    let digits = p.strip_prefix('-').unwrap_or(p);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    p.parse().ok().map(|n| Control::CommandFinished(Some(n)))
}

impl Scanner {
    pub fn feed(&mut self, s: &str) -> Vec<Found> {
        let mut found = Vec::new();
        for (i, ch) in s.char_indices() {
            let end = i + ch.len_utf8();
            let mut emit = |control: Control| found.push(Found { end, control });
            match self.st {
                St::Ground => {
                    if ch == '\u{1b}' {
                        self.st = St::Esc;
                    }
                }
                St::Esc => match ch {
                    '[' => {
                        self.st = St::Csi;
                        self.csi.clear();
                        self.csi_has_intermediate = false;
                    }
                    ']' => {
                        self.st = St::Osc;
                        self.osc.clear();
                    }
                    'P' | '_' | '^' | 'X' => self.st = St::Str,
                    '\u{1b}' => {}
                    c if ('\u{20}'..='\u{2f}').contains(&c) => {}
                    _ => self.st = St::Ground,
                },
                St::Csi => match ch {
                    '\u{1b}' => self.st = St::Esc,
                    '\u{18}' | '\u{1a}' => self.st = St::Ground,
                    c if ('\u{30}'..='\u{3f}').contains(&c) => {
                        if self.csi_has_intermediate {
                            // Malformed; keep consuming until a final byte.
                        } else if self.csi.len() < 64 {
                            self.csi.push(c);
                        }
                    }
                    c if ('\u{20}'..='\u{2f}').contains(&c) => self.csi_has_intermediate = true,
                    c if ('\u{40}'..='\u{7e}').contains(&c) => {
                        self.st = St::Ground;
                        if !self.csi_has_intermediate {
                            match (self.csi.as_str(), c) {
                                ("" | "0", 'c') => emit(Control::Da1),
                                ("5", 'n') => emit(Control::Dsr5),
                                ("6", 'n') => emit(Control::Dsr6),
                                ("?6", 'n') => emit(Control::DecDsr6),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                },
                St::Osc | St::OscEsc => {
                    let after_esc = self.st == St::OscEsc;
                    if after_esc && ch == '\\' || (!after_esc && (ch == '\u{7}' || ch == '\u{9c}'))
                    {
                        self.st = St::Ground;
                        if let Some(c) = self.finish_osc() {
                            emit(c);
                        }
                    } else if ch == '\u{1b}' {
                        self.st = St::OscEsc;
                    } else if after_esc {
                        // ESC not followed by `\`: abort OSC, reprocess as an escape sequence.
                        self.st = St::Esc;
                        match ch {
                            '[' => {
                                self.st = St::Csi;
                                self.csi.clear();
                                self.csi_has_intermediate = false;
                            }
                            ']' => {
                                self.st = St::Osc;
                                self.osc.clear();
                            }
                            _ => self.st = St::Ground,
                        }
                    } else if ch == '\u{18}' || ch == '\u{1a}' {
                        self.st = St::Ground;
                    } else if self.osc.len() < OSC_CAP {
                        self.osc.push(ch);
                    }
                }
                St::Str => match ch {
                    '\u{1b}' => self.st = St::StrEsc,
                    '\u{18}' | '\u{1a}' | '\u{9c}' => self.st = St::Ground,
                    _ => {}
                },
                St::StrEsc => {
                    self.st = if ch == '\\' { St::Ground } else { St::Str };
                }
            }
        }
        found
    }

    fn finish_osc(&mut self) -> Option<Control> {
        let osc = std::mem::take(&mut self.osc);
        let (code, data) = osc.split_once(';').unwrap_or((osc.as_str(), ""));
        match code {
            "10" | "11" | "12" if data.trim() == "?" => Some(Control::OscColorQuery(
                code.parse().expect("matched numeric code"),
            )),
            "633" => parse_command_finished(data),
            _ => None,
        }
    }
}

/// xterm.js-compatible colour query answers (terminal.ts TERMINAL_OSC_COLOR_QUERY_RESPONSES).
pub fn osc_color_response(code: u8) -> Option<String> {
    let rgb = match code {
        10 | 12 => "rgb:e6e6/e6e6/e6e6",
        11 => "rgb:0b0b/0b0b/0b0b",
        _ => return None,
    };
    Some(format!("\x1b]{code};{rgb}\x1b\\"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controls(s: &mut Scanner, chunk: &str) -> Vec<Control> {
        s.feed(chunk).into_iter().map(|f| f.control).collect()
    }

    #[test]
    fn detects_queries() {
        let mut s = Scanner::default();
        assert_eq!(
            controls(&mut s, "a\x1b[c\x1b[0c\x1b[5n\x1b[6n\x1b[?6n\x1b[1;2H"),
            vec![
                Control::Da1,
                Control::Da1,
                Control::Dsr5,
                Control::Dsr6,
                Control::DecDsr6
            ]
        );
    }

    #[test]
    fn handles_split_chunks_and_offsets() {
        let mut s = Scanner::default();
        assert!(s.feed("x\x1b[").is_empty());
        let f = s.feed("6nyy");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].end, 2);
    }

    #[test]
    fn osc_color_and_command_finished() {
        let mut s = Scanner::default();
        assert_eq!(
            controls(
                &mut s,
                "\x1b]11;?\x1b\\\x1b]10;?\x07\x1b]633;D;0\x07\x1b]633;D\x07\x1b]633;D;x\x07\x1b]11;rgb:1/2/3\x07"
            ),
            vec![
                Control::OscColorQuery(11),
                Control::OscColorQuery(10),
                Control::CommandFinished(Some(0)),
                Control::CommandFinished(None),
            ]
        );
        assert_eq!(
            controls(&mut s, "\x1b]633;D;-1\x07"),
            vec![Control::CommandFinished(Some(-1))]
        );
    }

    #[test]
    fn ignores_sequences_inside_dcs() {
        let mut s = Scanner::default();
        assert!(controls(&mut s, "\x1bP1$r\x1b[c\x1b\\").is_empty());
        assert_eq!(controls(&mut s, "\x1b[c"), vec![Control::Da1]);
    }

    #[test]
    fn color_responses() {
        assert_eq!(
            osc_color_response(11).unwrap(),
            "\x1b]11;rgb:0b0b/0b0b/0b0b\x1b\\"
        );
    }
}
