//! JavaScript-semantics helpers. The TS code leans on UTF-16 string lengths, `String.trim`,
//! `\s`, `JSON.stringify` and number formatting; byte-for-byte parity needs the same behavior.

use serde_json::Value;

/// JS `WhiteSpace` + `LineTerminator` (what `trim()` and `\s` match). Differs from Rust's
/// `char::is_whitespace`: includes U+FEFF, excludes U+0085.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}' | '\u{a}' | '\u{b}' | '\u{c}' | '\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// `String.prototype.trim`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// `str.length` (UTF-16 code units; lone-surrogate carriers count as one).
pub fn utf16_len(s: &str) -> usize {
    paseo_agent_types::js_str::utf16_len(s)
}

/// `str.slice(0, n)` by UTF-16 code units.
///
/// PARITY-QUIRK(surrogate-truncation): a cut inside a surrogate pair leaves a lone high
/// surrogate, as in TS. It is carried as a private-use scalar (see `paseo_agent_types::js_str`)
/// and written as `\udXXX` by `to_js_json_string`.
pub fn utf16_prefix(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = if paseo_agent_types::js_str::carrier_unit(c).is_some() {
            1
        } else {
            c.len_utf16()
        };
        if used + w <= n {
            out.push(c);
            used += w;
        } else {
            if used < n && w == 2 {
                let mut buf = [0u16; 2];
                c.encode_utf16(&mut buf);
                out.push(paseo_agent_types::js_str::lone_high_surrogate(buf[0]).unwrap());
            }
            break;
        }
    }
    out
}

/// ASCII-only lowercase copy; byte offsets are preserved so callers can search it and slice the
/// original.
pub fn ascii_lower(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// `Number.prototype.toString()` for a finite f64.
pub fn js_number_to_string(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    if !x.is_finite() {
        return if x.is_nan() {
            "NaN".into()
        } else if x > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    let neg = x < 0.0;
    let sci = format!("{:e}", x.abs()); // shortest round-trip digits, e.g. "1.2345e2"
    let (mant, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp + 1;
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        let e = n - 1;
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if e < 0 { '-' } else { '+' });
        out.push_str(&e.abs().to_string());
    }
    out
}

fn number_to_string(n: &serde_json::Number) -> String {
    js_number_to_string(n.as_f64().unwrap_or(0.0))
}

fn write_value(v: &Value, indent: Option<&str>, depth: usize, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number_to_string(n)),
        Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap()),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if let Some(ind) = indent {
                    out.push('\n');
                    out.push_str(&ind.repeat(depth + 1));
                }
                write_value(x, indent, depth + 1, out);
            }
            if let Some(ind) = indent {
                out.push('\n');
                out.push_str(&ind.repeat(depth));
            }
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                if let Some(ind) = indent {
                    out.push('\n');
                    out.push_str(&ind.repeat(depth + 1));
                }
                out.push_str(&serde_json::to_string(k).unwrap());
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(x, indent, depth + 1, out);
            }
            if let Some(ind) = indent {
                out.push('\n');
                out.push_str(&ind.repeat(depth));
            }
            out.push('}');
        }
    }
}

/// `JSON.stringify(value)`.
pub fn json_stringify(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, None, 0, &mut out);
    out
}

/// `JSON.stringify(value, null, 2)`.
pub fn json_stringify_pretty(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, Some("  "), 0, &mut out);
    out
}

/// `String(value)` for the primitives the callers need; arrays/objects follow JS defaults.
pub fn js_to_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => number_to_string(n),
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|x| match x {
                Value::Null => String::new(),
                other => js_to_string(other),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `hashText` from `tool-call-mapper-utils.ts`: 32-bit rolling hash over UTF-16 code units,
/// `Math.abs(hash).toString(36)`.
pub fn hash_text(value: &str) -> String {
    let mut hash: i32 = 0;
    for unit in paseo_agent_types::js_str::utf16_units(value) {
        hash = hash
            .wrapping_shl(5)
            .wrapping_sub(hash)
            .wrapping_add(unit as i32);
    }
    to_base36(hash.unsigned_abs() as u64)
}

fn to_base36(mut n: u64) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(b"0123456789abcdefghijklmnopqrstuvwxyz"[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap()
}

/// JS truthiness for an `Option<&str>`-style "non-empty string" check.
pub fn non_empty(s: &str) -> Option<&str> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub fn is_integer(n: &serde_json::Number) -> bool {
    n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
}

pub fn is_safe_integer(n: &serde_json::Number) -> bool {
    n.as_f64()
        .is_some_and(|f| f.fract() == 0.0 && f.abs() <= 9007199254740991.0)
}
