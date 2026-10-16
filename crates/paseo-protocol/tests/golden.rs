//! Golden conformance: every fixture frame must survive parse -> serialize unchanged.
//! Fixture lines are `{"dir":"out"|"in","frame":{..}}` (out = client to daemon); a bare frame
//! object is accepted too. Vacuous pass (with a note) when `fixtures/` has no `.jsonl` files.
use paseo_protocol::{peek_route, Direction, Frame};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::PathBuf};

#[test]
fn fixtures_roundtrip() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    if files.is_empty() {
        println!("golden: no fixtures/*.jsonl found; passing vacuously");
        return;
    }

    let (mut typed, mut fallback, mut total, mut binary) = (0usize, 0usize, 0usize, 0usize);
    let mut untyped_types: BTreeMap<String, usize> = BTreeMap::new();
    let mut dir_mismatch = Vec::new();

    for file in files {
        let text = fs::read_to_string(&file).unwrap();
        for (i, line) in text
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
        {
            let at = format!("{}:{}", file.file_name().unwrap().to_string_lossy(), i + 1);
            let entry: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{at}: {e}"));
            if entry.get("binary_b64").is_some() {
                // Binary frames are routed by opcode, not JSON; nothing to round-trip here.
                binary += 1;
                continue;
            }
            let (frame_value, wire_dir) = match entry.get("frame") {
                Some(f) if entry.get("dir").is_some() => {
                    (f.clone(), entry["dir"].as_str().map(String::from))
                }
                _ => (entry.clone(), None),
            };
            let bytes = serde_json::to_vec(&frame_value).unwrap();

            let frame = Frame::from_slice(&bytes).unwrap_or_else(|e| panic!("{at}: {e}"));
            let reserialized: Value = serde_json::from_slice(&frame.to_vec()).unwrap();
            assert_eq!(
                reserialized, frame_value,
                "{at}: parse -> serialize changed the frame"
            );
            let via_serde: Value =
                serde_json::from_str(&serde_json::to_string(&frame).unwrap()).unwrap();
            assert_eq!(
                via_serde, frame_value,
                "{at}: Serialize impl changed the frame"
            );

            // peek_route must agree with the full parse.
            let route = peek_route(&bytes);
            let expect_type = frame_value.get("type").and_then(Value::as_str);
            assert_eq!(
                route.as_ref().map(|r| r.type_.as_str()),
                expect_type,
                "{at}: route type"
            );
            if expect_type == Some("session") {
                let inner = frame_value["message"]["type"].as_str();
                assert_eq!(
                    route.as_ref().and_then(|r| r.session_type.as_deref()),
                    inner,
                    "{at}: route session_type"
                );
            }

            total += 1;
            match frame.typed() {
                Some(t) => {
                    typed += 1;
                    let wire_is_out = wire_dir.as_deref() == Some("out");
                    let ok = match wire_dir.as_deref() {
                        Some(_) => (t.direction == Direction::Inbound) == wire_is_out,
                        None => true,
                    };
                    if !ok {
                        dir_mismatch.push(format!("{at}: {:?} {:?}", t.scope, t.type_));
                    }
                }
                None => {
                    fallback += 1;
                    let key = match route {
                        Some(r) => match r.session_type {
                            Some(s) => format!("session/{s}"),
                            None => r.type_,
                        },
                        None => "<no type>".into(),
                    };
                    *untyped_types.entry(key).or_default() += 1;
                }
            }
        }
    }
    println!("golden: {total} JSON frames, {typed} typed, {fallback} Value fallback, {binary} binary skipped");
    for (ty, n) in &untyped_types {
        println!("golden: fallback {ty} x{n}");
    }
    assert!(
        dir_mismatch.is_empty(),
        "direction mismatches: {dir_mismatch:#?}"
    );
}
