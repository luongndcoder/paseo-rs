#[path = "../../../../../src/emulator/mod.rs"]
mod emulator;

use base64::Engine;
use serde_json::Value;
use std::io::Read;

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    // Input: one scenario JSON per line; output: one JSON array of results per line.
    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let g: Value = serde_json::from_str(line).unwrap();
        let mut e = emulator::Emulator::new(
            g["rows"].as_u64().unwrap() as u16,
            g["cols"].as_u64().unwrap() as u16,
            g["scrollback"].as_u64().unwrap() as usize,
        );
        let mut out = Vec::new();
        for step in g["steps"].as_array().unwrap() {
            if let Some(d) = step.get("feed") {
                let b = base64::engine::general_purpose::STANDARD.decode(d.as_str().unwrap()).unwrap();
                e.feed(&b);
            } else if let Some(s) = step.get("resize") {
                e.resize(s[0].as_u64().unwrap() as u16, s[1].as_u64().unwrap() as u16);
            } else if let Some(o) = step.get("snapshot") {
                let opts: emulator::SnapshotOptions = serde_json::from_value(o.clone()).unwrap();
                out.push(e.snapshot(&opts));
            }
        }
        println!("{}", serde_json::to_string(&out).unwrap());
    }
}
