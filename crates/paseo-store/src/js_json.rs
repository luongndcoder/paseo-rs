//! JSON read/write with JS lone-surrogate parity.
//!
//! `JSON.parse` accepts `"\ud83d"` (a lone surrogate escape, which a UTF-16 slice in TS can
//! produce) and `JSON.stringify` writes one back; `serde_json` rejects it. Reads map a lone high
//! surrogate to the `paseo_agent_types::js_str` carrier scalar and writes turn carriers back into
//! `\udXXX` escapes, so both runtimes keep reading the same files.

use paseo_agent_types::js_str::{escape_carriers, lone_high_surrogate};
use paseo_fs_atomic::{write_file_atomic, FsyncPolicy};
use serde::{de::DeserializeOwned, Serialize};
use std::borrow::Cow;
use std::path::Path;

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

pub fn from_js_json_str<T: DeserializeOwned>(raw: &str) -> serde_json::Result<T> {
    serde_json::from_str(&decode_lone_surrogates(raw))
}

/// `JSON.stringify(value, null, 2)` with carriers written as `\udXXX`.
pub fn write_js_json_atomic<T: Serialize>(
    path: &Path,
    value: &T,
    policy: FsyncPolicy,
) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    write_file_atomic(path, escape_carriers(&text).as_bytes(), policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn lone_high_round_trips() {
        let v: Value = from_js_json_str(r#"{"t":"a\ud83d"}"#).unwrap();
        let s = v["t"].as_str().unwrap();
        assert_eq!(paseo_agent_types::js_str::utf16_len(s), 2);
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.json");
        write_js_json_atomic(&p, &v, FsyncPolicy::default()).unwrap();
        assert!(std::fs::read_to_string(&p).unwrap().contains(r"a\ud83d"));
    }

    #[test]
    fn valid_pairs_and_escaped_backslashes_untouched() {
        let raw = r#"{"t":"😀 \\ud83d"}"#;
        let v: Value = from_js_json_str(raw).unwrap();
        assert_eq!(v["t"].as_str().unwrap(), "\u{1F600} \\ud83d");
    }
}
