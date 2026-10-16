//! Small zod-shaped readers over `serde_json::Value` plus JS string helpers shared by the Pi
//! mapper modules. Readers return `None` where the zod schema would fail to parse.

use paseo_agent_types::Map;
use paseo_tool_detail::js::js_trim;
use serde_json::Value;

/// `z.string()`
pub(super) fn req_str<'a>(m: &'a Map, k: &str) -> Option<&'a str> {
    m.get(k)?.as_str()
}

/// `z.string().optional()`: `Some(None)` when absent, `None` when present but not a string.
pub(super) fn opt_str<'a>(m: &'a Map, k: &str) -> Option<Option<&'a str>> {
    match m.get(k) {
        None => Some(None),
        Some(Value::String(s)) => Some(Some(s)),
        _ => None,
    }
}

/// `z.string().trim().min(1)`: the parsed value is the trimmed string.
pub(super) fn req_trim<'a>(m: &'a Map, k: &str) -> Option<&'a str> {
    let t = js_trim(m.get(k)?.as_str()?);
    (!t.is_empty()).then_some(t)
}

/// `z.string().trim().min(1).optional()`
pub(super) fn opt_trim<'a>(m: &'a Map, k: &str) -> Option<Option<&'a str>> {
    match m.get(k) {
        None => Some(None),
        Some(_) => req_trim(m, k).map(Some),
    }
}

/// `z.boolean().optional()`
pub(super) fn opt_bool(m: &Map, k: &str) -> Option<Option<bool>> {
    match m.get(k) {
        None => Some(None),
        Some(Value::Bool(b)) => Some(Some(*b)),
        _ => None,
    }
}

pub(super) fn req_bool(m: &Map, k: &str) -> Option<bool> {
    m.get(k)?.as_bool()
}

pub(super) fn is_int(v: &Value) -> bool {
    match v {
        Value::Number(n) => {
            n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        _ => false,
    }
}

/// `z.number().int()`
pub(super) fn req_int(m: &Map, k: &str) -> Option<i64> {
    let v = m.get(k)?;
    is_int(v).then(|| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))?
}

pub(super) fn req_array<'a>(m: &'a Map, k: &str) -> Option<&'a Vec<Value>> {
    m.get(k)?.as_array()
}

/// JS truthiness of an optional string field (`undefined` and `""` are falsy).
pub(super) fn truthy_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// `optionalString(value)` from `agent.ts`.
pub(super) fn optional_string(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
}

/// `/\bword\b/i` over ASCII words (JS `\b` treats `[A-Za-z0-9_]` as word characters).
pub(super) fn has_word_ci(haystack: &str, word: &str) -> bool {
    let h = haystack.to_lowercase();
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(pos) = h[from..].find(word) {
        let start = from + pos;
        let end = start + word.len();
        let before_ok = !h[..start].chars().next_back().is_some_and(is_word);
        let after_ok = !h[end..].chars().next().is_some_and(is_word);
        if before_ok && after_ok {
            return true;
        }
        from = start + h[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// `/\boptional\b|\bskip\b/i.test(placeholder ?? "")`
pub(super) fn is_optional_input_placeholder(placeholder: Option<&str>) -> bool {
    let p = placeholder.unwrap_or("");
    has_word_ci(p, "optional") || has_word_ci(p, "skip")
}

/// JS template-literal interpolation of an optional JSON value (`undefined` -> `"undefined"`).
pub(super) fn js_interp(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(v) => paseo_tool_detail::js::js_to_string(v),
    }
}

/// `strip-ansi` (ansi-regex 5): CSI/OSC escape sequences.
///
/// DEVIATION(pi-strip-ansi-approximate): a hand-written matcher for the ansi-regex 5 pattern
/// (no regex engine), exercised on CSI and BEL-terminated OSC sequences only.
pub(super) fn strip_ansi(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if matches!(chars[i], '\u{1b}' | '\u{9b}') {
            if let Some(end) = match_ansi(&chars, i + 1) {
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn is_prefix_char(c: char) -> bool {
    matches!(c, '[' | ']' | '(' | ')' | '#' | ';' | '?')
}

fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '/' | '#' | '&' | '.' | ':' | '=' | '?' | '%' | '@' | '~' | '_'
        )
}

fn is_final_byte(c: char) -> bool {
    c.is_ascii_digit()
        || matches!(c, 'A'..='P' | 'R'..='T' | 'Z' | 'c' | 'f'..='n' | 'q'..='u' | 'y' | '=' | '>' | '<' | '~')
}

/// Regex-style match after the ESC char, trying every prefix length from longest to shortest.
fn match_ansi(c: &[char], start: usize) -> Option<usize> {
    let mut max = start;
    while max < c.len() && is_prefix_char(c[max]) {
        max += 1;
    }
    for p in (start..=max).rev() {
        if let Some(e) = match_osc(c, p).or_else(|| match_csi(c, p)) {
            return Some(e);
        }
    }
    None
}

/// `(?:(?:;[-a-zA-Z\d\/#&.:=?%@~_]+)*|[a-zA-Z\d]+(?:;[-a-zA-Z\d\/#&.:=?%@~_]*)*)?\u0007`
fn match_osc(c: &[char], p: usize) -> Option<usize> {
    let bel = |i: usize| (c.get(i) == Some(&'\u{7}')).then_some(i + 1);
    // alt A: (;url+)* then BEL
    let mut i = p;
    loop {
        if c.get(i) == Some(&';') {
            let mut j = i + 1;
            while j < c.len() && (is_url_char(c[j]) || c[j] == '-') {
                j += 1;
            }
            if j > i + 1 {
                i = j;
                continue;
            }
        }
        break;
    }
    if let Some(e) = bel(i) {
        return Some(e);
    }
    // alt B: [a-zA-Z\d]+ (;url*)* then BEL
    let mut j = p;
    while j < c.len() && c[j].is_ascii_alphanumeric() {
        j += 1;
    }
    if j > p {
        loop {
            if c.get(j) == Some(&';') {
                j += 1;
                while j < c.len() && is_url_char(c[j]) {
                    j += 1;
                }
                continue;
            }
            break;
        }
        return bel(j);
    }
    None
}

/// `(?:\d{1,4}(?:;\d{0,4})*)?[\dA-PR-TZcf-nq-uy=><~]`
fn match_csi(c: &[char], p: usize) -> Option<usize> {
    let digits = |from: usize, max: usize| {
        let mut j = from;
        while j < c.len() && j - from < max && c[j].is_ascii_digit() {
            j += 1;
        }
        j
    };
    let d = digits(p, 4);
    if d > p {
        let mut j = d;
        while c.get(j) == Some(&';') {
            j = digits(j + 1, 4);
        }
        // greedy run first, then shorter runs for backtracking
        if let Some(&f) = c.get(j) {
            if is_final_byte(f) {
                return Some(j + 1);
            }
        }
    }
    let f = *c.get(p)?;
    is_final_byte(f).then_some(p + 1)
}
