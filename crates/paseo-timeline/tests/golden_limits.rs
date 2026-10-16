//! `content-limits.json`: 64 KiB UTF-16 cuts (including astral straddles) and plugin data size.

mod common;

use common::load_json;
use paseo_agent_types::js_str::utf16_units;
use paseo_agent_types::AgentTimelineItem;
use paseo_timeline::limits::{
    assert_plugin_timeline_data_size, limit_item_content, prepare_item_for_record,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// `{ "$gen": { parts: [{unit, count}] } }` placeholders become real strings.
fn expand(v: &Value) -> Value {
    match v {
        Value::Object(m) if m.len() == 1 && m.contains_key("$gen") => {
            let mut s = String::new();
            for p in m["$gen"]["parts"].as_array().unwrap() {
                s.push_str(
                    &p["unit"]
                        .as_str()
                        .unwrap()
                        .repeat(p["count"].as_u64().unwrap() as usize),
                );
            }
            Value::String(s)
        }
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), expand(v))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(expand).collect()),
        other => other.clone(),
    }
}

/// Strings over 1024 UTF-16 units become `{ "$str": { length16, sha256Utf16le, tail16Hex } }`.
fn digest(v: &Value) -> Value {
    match v {
        Value::String(s) => {
            let units = utf16_units(s);
            if units.len() <= 1024 {
                return v.clone();
            }
            let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
            let sha: String = Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let tail: Vec<String> = units[units.len() - 8..]
                .iter()
                .map(|u| format!("{u:04x}"))
                .collect();
            json!({"$str": {"length16": units.len(), "sha256Utf16le": sha, "tail16Hex": tail}})
        }
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), digest(v))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(digest).collect()),
        other => other.clone(),
    }
}

#[test]
fn limit_cases_match_ts() {
    let doc = load_json("content-limits.json");
    assert_eq!(doc["cap"], 65536);
    let cases = doc["limitCases"].as_array().unwrap();
    assert_eq!(cases.len(), 19);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let Ok(input) = AgentTimelineItem::from_value(&expand(&case["input"])) else {
            // A non-string shell `output` is unrepresentable in the typed item; TS leaves it as is.
            assert_eq!(name, "shell_output_not_string");
            assert_eq!(case["input"], case["output"]);
            continue;
        };
        let out = limit_item_content(input.clone());
        assert_eq!(digest(&out.to_value()), case["output"], "{name}");
        if case["outputIsSameReference"] == true {
            assert_eq!(out, input, "{name}: untouched item must be unchanged");
        }
    }
}

#[test]
fn plugin_size_cases_match_ts() {
    let doc = load_json("content-limits.json");
    let cases = doc["pluginCases"].as_array().unwrap();
    assert_eq!(cases.len(), 6);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let data = expand(&case["data"]);
        let got = assert_plugin_timeline_data_size(&data);
        match (&case["result"]["ok"], got) {
            (Value::Bool(true), Ok(())) => {}
            (Value::Bool(false), Err(msg)) => {
                assert_eq!(json!(msg), case["result"]["error"]["message"], "{name}")
            }
            (want, got) => panic!("{name}: want ok={want}, got {got:?}"),
        }
        let item = AgentTimelineItem::Plugin {
            id: "i".into(),
            plugin_id: "p".into(),
            kind: "k".into(),
            version: 1.into(),
            data,
        };
        assert_eq!(
            prepare_item_for_record(item).is_ok(),
            case["result"]["ok"] == true,
            "{name}"
        );
    }
}
