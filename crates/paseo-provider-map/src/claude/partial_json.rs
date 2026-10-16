//! Incremental JSON object parser for streamed tool input (`claude/partial-json.ts`).
//!
//! Indexes are in `char`s rather than UTF-16 units; only the `complete` flag and the parsed value
//! leave this module, and neither depends on the unit.

use paseo_tool_detail::js::is_js_whitespace;
use serde_json::{Map, Value};

struct Parsed {
    value: Value,
    next: usize,
    complete: bool,
}

fn skip_ws(input: &[char], mut i: usize) -> usize {
    while i < input.len() && is_js_whitespace(input[i]) {
        i += 1;
    }
    i
}

struct Str {
    value: String,
    next: usize,
    complete: bool,
}

fn parse_string(input: &[char], index: usize) -> Option<Str> {
    if input.get(index) != Some(&'"') {
        return None;
    }
    let mut i = index + 1;
    let mut value = String::new();
    let incomplete = |value: String| Str {
        value,
        next: input.len(),
        complete: false,
    };
    while i < input.len() {
        let c = input[i];
        if c == '"' {
            return Some(Str {
                value,
                next: i + 1,
                complete: true,
            });
        }
        if c == '\\' {
            let Some(&esc) = input.get(i + 1) else {
                return Some(incomplete(value));
            };
            match esc {
                '"' | '\\' | '/' => value.push(esc),
                'b' => value.push('\u{8}'),
                'f' => value.push('\u{c}'),
                'n' => value.push('\n'),
                'r' => value.push('\r'),
                't' => value.push('\t'),
                'u' => {
                    let Some(unit) = hex4(input, i + 2) else {
                        return Some(incomplete(value));
                    };
                    if (0xD800..0xDC00).contains(&unit) {
                        let low = match (input.get(i + 6), input.get(i + 7)) {
                            (Some('\\'), Some('u')) => {
                                hex4(input, i + 8).filter(|l| (0xDC00..0xE000).contains(l))
                            }
                            _ => None,
                        };
                        if let Some(low) = low {
                            let cp =
                                0x10000 + ((unit as u32 - 0xD800) << 10) + (low as u32 - 0xDC00);
                            value.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                            i += 12;
                            continue;
                        }
                        // PARITY-QUIRK(claude-partial-json-lone-surrogate): JS keeps the lone high
                        // surrogate; it travels as the js_str carrier char. A lone low surrogate
                        // has no carrier and becomes U+FFFD.
                        value.push(
                            paseo_agent_types::js_str::lone_high_surrogate(unit)
                                .unwrap_or('\u{fffd}'),
                        );
                    } else {
                        value.push(char::from_u32(unit as u32).unwrap_or('\u{fffd}'));
                    }
                    i += 6;
                    continue;
                }
                _ => return Some(incomplete(value)),
            }
            i += 2;
            continue;
        }
        value.push(c);
        i += 1;
    }
    Some(incomplete(value))
}

fn hex4(input: &[char], at: usize) -> Option<u16> {
    let slice = input.get(at..at + 4)?;
    if !slice.iter().all(char::is_ascii_hexdigit) {
        return None;
    }
    let s: String = slice.iter().collect();
    u16::from_str_radix(&s, 16).ok()
}

fn starts_with_at(input: &[char], at: usize, lit: &str) -> bool {
    lit.chars()
        .enumerate()
        .all(|(k, c)| input.get(at + k) == Some(&c))
}

fn parse_literal(input: &[char], i: usize) -> Option<Parsed> {
    for (lit, v) in [
        ("true", Value::Bool(true)),
        ("false", Value::Bool(false)),
        ("null", Value::Null),
    ] {
        if starts_with_at(input, i, lit) {
            return Some(Parsed {
                value: v,
                next: i + lit.len(),
                complete: true,
            });
        }
    }
    None
}

fn parse_number(input: &[char], i: usize) -> Option<Parsed> {
    // -?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?
    let mut j = i;
    if input.get(j) == Some(&'-') {
        j += 1;
    }
    match input.get(j) {
        Some('0') => j += 1,
        Some(c) if c.is_ascii_digit() => {
            while input.get(j).is_some_and(char::is_ascii_digit) {
                j += 1;
            }
        }
        _ => return None,
    }
    if input.get(j) == Some(&'.') && input.get(j + 1).is_some_and(char::is_ascii_digit) {
        j += 1;
        while input.get(j).is_some_and(char::is_ascii_digit) {
            j += 1;
        }
    }
    if matches!(input.get(j), Some('e' | 'E')) {
        let mut k = j + 1;
        if matches!(input.get(k), Some('+' | '-')) {
            k += 1;
        }
        if input.get(k).is_some_and(char::is_ascii_digit) {
            while input.get(k).is_some_and(char::is_ascii_digit) {
                k += 1;
            }
            j = k;
        }
    }
    let text: String = input[i..j].iter().collect();
    let f: f64 = text.parse().ok()?;
    Some(Parsed {
        value: Value::Number(paseo_tool_detail::num_from_f64(f)),
        next: j,
        complete: true,
    })
}

fn parse_array(input: &[char], index: usize) -> Option<Parsed> {
    if input.get(index) != Some(&'[') {
        return None;
    }
    let mut values = Vec::new();
    let mut i = index + 1;
    let done = |values: Vec<Value>, next: usize, complete: bool| {
        Some(Parsed {
            value: Value::Array(values),
            next,
            complete,
        })
    };
    while i <= input.len() {
        i = skip_ws(input, i);
        match input.get(i) {
            Some(']') => return done(values, i + 1, true),
            None => return done(values, input.len(), false),
            _ => {}
        }
        let Some(parsed) = parse_value(input, i) else {
            return done(values, i, false);
        };
        values.push(parsed.value);
        i = skip_ws(input, parsed.next);
        if !parsed.complete {
            return done(values, i, false);
        }
        match input.get(i) {
            Some(',') => i += 1,
            Some(']') => return done(values, i + 1, true),
            None => return done(values, input.len(), false),
            _ => return done(values, i, false),
        }
    }
    done(values, input.len(), false)
}

fn parse_object(input: &[char], index: usize) -> Option<Parsed> {
    if input.get(index) != Some(&'{') {
        return None;
    }
    let mut value = Map::new();
    let mut i = index + 1;
    let done = |value: Map<String, Value>, next: usize, complete: bool| {
        Some(Parsed {
            value: Value::Object(value),
            next,
            complete,
        })
    };
    while i <= input.len() {
        i = skip_ws(input, i);
        match input.get(i) {
            Some('}') => return done(value, i + 1, true),
            None => return done(value, input.len(), false),
            _ => {}
        }
        let key = parse_string(input, i);
        let Some(key) = key.filter(|k| k.complete) else {
            return done(value, i, false);
        };
        i = skip_ws(input, key.next);
        if input.get(i) != Some(&':') {
            return done(value, i, false);
        }
        i = skip_ws(input, i + 1);
        let Some(member) = parse_value(input, i) else {
            return done(value, i, false);
        };
        if !member.complete {
            return done(value, skip_ws(input, member.next), false);
        }
        value.insert(key.value, member.value);
        i = skip_ws(input, member.next);
        match input.get(i) {
            Some(',') => i += 1,
            Some('}') => return done(value, i + 1, true),
            None => return done(value, input.len(), false),
            _ => return done(value, i, false),
        }
    }
    done(value, input.len(), false)
}

fn parse_value(input: &[char], index: usize) -> Option<Parsed> {
    let i = skip_ws(input, index);
    let c = *input.get(i)?;
    match c {
        '"' => parse_string(input, i).map(|s| Parsed {
            value: Value::String(s.value),
            next: s.next,
            complete: s.complete,
        }),
        '{' => parse_object(input, i),
        '[' => parse_array(input, i),
        't' | 'f' | 'n' => parse_literal(input, i),
        '-' | '0'..='9' => parse_number(input, i),
        _ => None,
    }
}

/// `parsePartialJsonObject`: the object parsed so far and whether the whole input was consumed.
pub fn parse_partial_json_object(input: &str) -> Option<(Map<String, Value>, bool)> {
    let chars: Vec<char> = input.chars().collect();
    let start = skip_ws(&chars, 0);
    let parsed = parse_object(&chars, start)?;
    let complete = parsed.complete && skip_ws(&chars, parsed.next) == chars.len();
    match parsed.value {
        Value::Object(m) => Some((m, complete)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_partial_objects() {
        let (v, done) = parse_partial_json_object(r#"{"a": "he"#).unwrap();
        assert_eq!(Value::Object(v), json!({}));
        assert!(!done);
        let (v, done) = parse_partial_json_object(r#"{"a": 1, "b": [1, 2], "c": "x"}"#).unwrap();
        assert_eq!(Value::Object(v), json!({"a": 1, "b": [1, 2], "c": "x"}));
        assert!(done);
        let (v, done) = parse_partial_json_object(r#"{"a": 1, "b": "par"#).unwrap();
        assert_eq!(Value::Object(v), json!({"a": 1}));
        assert!(!done);
        assert!(parse_partial_json_object("[1]").is_none());
    }
}
