#![allow(dead_code)]
//! Shared fixture helpers for the WP0 timeline goldens.

use std::path::PathBuf;

use paseo_agent_types::js_str::lone_high_surrogate;
use serde_json::Value;

pub fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/p4/timeline")
        .join(name)
}

pub fn load_json(name: &str) -> Value {
    let text = std::fs::read_to_string(fixture_path(name))
        .unwrap_or_else(|e| panic!("read fixture {name}: {e}"));
    parse_js_json(&text)
}

pub fn load_jsonl(name: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(fixture_path(name))
        .unwrap_or_else(|e| panic!("read fixture {name}: {e}"));
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(parse_js_json)
        .collect()
}

/// Parse JSON that may contain a lone `\udXXX` high-surrogate escape (TS `JSON.stringify` output
/// after a UTF-16 cut). `serde_json` rejects those, so each lone high surrogate is replaced by the
/// carrier scalar used by `paseo_agent_types::js_str` before parsing.
pub fn parse_js_json(text: &str) -> Value {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let hex4 = |at: usize| -> Option<u16> {
        let s = text.get(at..at + 4)?;
        u16::from_str_radix(s, 16).ok()
    };
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'u' {
                if let Some(unit) = hex4(i + 2) {
                    if (0xD800..=0xDBFF).contains(&unit) {
                        let paired = text.get(i + 6..i + 8) == Some("\\u")
                            && hex4(i + 8).is_some_and(|l| (0xDC00..=0xDFFF).contains(&l));
                        if !paired {
                            out.push(lone_high_surrogate(unit).expect("high surrogate"));
                            i += 6;
                            continue;
                        }
                    }
                }
            }
            // Keep the escape pair verbatim so `\\u` is never mistaken for `\u`.
            out.push_str(&text[i..i + 2]);
            i += 2;
            continue;
        }
        let c = text[i..].chars().next().expect("char");
        out.push(c);
        i += c.len_utf8();
    }
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("parse json: {e}"))
}
