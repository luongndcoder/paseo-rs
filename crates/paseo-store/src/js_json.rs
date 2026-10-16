//! JSON read/write with JS lone-surrogate parity.
//!
//! `JSON.parse` accepts `"\ud83d"` (a lone surrogate escape, which a UTF-16 slice in TS can
//! produce) and `JSON.stringify` writes one back; `serde_json` rejects it. Reads map a lone high
//! surrogate to the `paseo_agent_types::js_str` carrier scalar and writes turn carriers back into
//! `\udXXX` escapes, so both runtimes keep reading the same files.

use paseo_agent_types::js_str::{decode_lone_surrogates, escape_carriers};
use paseo_fs_atomic::{write_file_atomic, FsyncPolicy};
use serde::{de::DeserializeOwned, Serialize};
use std::path::Path;

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
