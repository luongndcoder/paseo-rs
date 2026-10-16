//! Lone-surrogate carrier for TS parity (PARITY-QUIRK(surrogate-truncation)).
//!
//! TS cuts strings by UTF-16 code unit, so a cut inside a surrogate pair leaves a lone high
//! surrogate, and `JSON.stringify` writes it as `\ud83d`. A Rust `String` cannot hold one. A lone
//! high surrogate `0xD800 + n` (n < 1024) is carried as the Plane 16 private-use scalar
//! `U+10F800 + n` (KNOWN COLLISION: a genuine U+10F800..U+10FBFF scalar is indistinguishable; see spec section 17) and [`to_js_json_string`] writes it back as the
//! `\udXXX` escape. Everything else in the pipeline treats the carrier as an ordinary character
//! that counts as ONE UTF-16 unit (see [`utf16_len`]).

use serde_json::Value;
use std::borrow::Cow;

const CARRIER_BASE: u32 = 0x10F800;
const CARRIER_END: u32 = CARRIER_BASE + 1023;

fn hex4(chars: &[char]) -> Option<u16> {
    if chars.len() < 4 {
        return None;
    }
    let s: String = chars[..4].iter().collect();
    u16::from_str_radix(&s, 16).ok()
}

/// Replaces lone-surrogate `\uXXXX` escapes. Lone high surrogate becomes its carrier char,
/// lone low surrogate becomes U+FFFD (PARITY-QUIRK(lone-low-surrogate): the carrier covers
/// high surrogates only, which is all a UTF-16 end-of-string slice produces).
pub fn decode_lone_surrogates(raw: &str) -> Cow<'_, str> {
    if !raw.contains("\\u") {
        return Cow::Borrowed(raw);
    }
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::with_capacity(raw.len());
    let mut changed = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\\' || i + 1 >= chars.len() {
            out.push(c);
            i += 1;
            continue;
        }
        if chars[i + 1] != 'u' {
            out.push(c);
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        let unit = hex4(&chars[i + 2..]);
        match unit {
            Some(u @ 0xD800..=0xDBFF) => {
                let low = (chars.get(i + 6) == Some(&'\\') && chars.get(i + 7) == Some(&'u'))
                    .then(|| hex4(&chars[(i + 8).min(chars.len())..]))
                    .flatten()
                    .is_some_and(|l| (0xDC00..=0xDFFF).contains(&l));
                if low {
                    out.extend(&chars[i..i + 12]);
                    i += 12;
                } else {
                    out.push(lone_high_surrogate(u).unwrap_or('\u{FFFD}'));
                    changed = true;
                    i += 6;
                }
            }
            Some(0xDC00..=0xDFFF) => {
                out.push('\u{FFFD}');
                changed = true;
                i += 6;
            }
            _ => {
                out.extend(&chars[i..(i + 2).min(chars.len())]);
                i += 2;
            }
        }
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(raw)
    }
}

/// Lenient `JSON.parse`: accepts lone-surrogate escapes (see [`decode_lone_surrogates`]).
pub fn parse_js_json(raw: &str) -> serde_json::Result<Value> {
    serde_json::from_str(&decode_lone_surrogates(raw))
}


/// The carrier character for a lone high surrogate code unit (`0xD800..=0xDBFF`).
pub fn lone_high_surrogate(unit: u16) -> Option<char> {
    if (0xD800..=0xDBFF).contains(&unit) {
        char::from_u32(CARRIER_BASE + (unit as u32 - 0xD800))
    } else {
        None
    }
}

/// The surrogate code unit a carrier character stands for.
pub fn carrier_unit(c: char) -> Option<u16> {
    let cp = c as u32;
    (CARRIER_BASE..=CARRIER_END)
        .contains(&cp)
        .then(|| (0xD800 + (cp - CARRIER_BASE)) as u16)
}

/// UTF-16 length with carriers counted as one unit (`String.prototype.length`).
pub fn utf16_len(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if carrier_unit(c).is_some() {
                1
            } else {
                c.len_utf16()
            }
        })
        .sum()
}

/// UTF-16 code units with carriers expanded to their lone surrogate.
pub fn utf16_units(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u16; 2];
    for c in s.chars() {
        match carrier_unit(c) {
            Some(u) => out.push(u),
            None => out.extend_from_slice(c.encode_utf16(&mut buf)),
        }
    }
    out
}

/// `JSON.stringify(value)` for a value that may contain carriers: carriers become `\udXXX`
/// escapes (lowercase hex, as V8 writes them). Other characters follow `serde_json`.
pub fn to_js_json_string(v: &Value) -> String {
    let s = serde_json::to_string(v).expect("Value serializes");
    escape_carriers(&s)
}

pub fn escape_carriers(s: &str) -> String {
    if !s.chars().any(|c| carrier_unit(c).is_some()) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match carrier_unit(c) {
            Some(u) => out.push_str(&format!("\\u{u:04x}")),
            None => out.push(c),
        }
    }
    out
}
