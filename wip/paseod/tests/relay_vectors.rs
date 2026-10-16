//! Cross-checks the Rust relay crypto/channel against vectors produced by the real TS packages
//! (`tests/relay-vectors/generate.mts` drives `packages/relay` + `packages/protocol`).

use bytes::Bytes;
use paseod::relay::b64;
use paseod::relay::channel::{accept_hello, Action};
use paseod::relay::control::build_relay_url;
use paseod::relay::crypto::SharedKey;
use paseod::relay::keypair::parse_keypair;
use paseod::relay::DaemonKeyPair;
use paseod::ws::Message;
use serde_json::Value;

fn load(name: &str) -> Value {
    let path = format!(
        "{}/tests/relay-vectors/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).expect("vector file")).expect("json")
}

fn hex(s: &str) -> Vec<u8> {
    assert_eq!(s.len() % 2, 0);
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn key32(s: &str) -> [u8; 32] {
    hex(s).try_into().unwrap()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap()
}

fn frame(v: &Value) -> Message {
    match s(&v["kind"]) {
        "text" => Message::Text(s(&v["text"]).to_owned()),
        "binary" => Message::Binary(Bytes::from(hex(s(&v["hex"])))),
        other => panic!("kind {other}"),
    }
}

#[test]
fn shared_key_and_bundles_match_tweetnacl() {
    let v = load("crypto");
    let daemon_secret = key32(s(&v["daemon"]["secretKeyHex"]));
    let daemon_public = key32(s(&v["daemon"]["publicKeyHex"]));
    let client_secret = key32(s(&v["client"]["secretKeyHex"]));
    let client_public = key32(s(&v["client"]["publicKeyHex"]));

    let shared = SharedKey::derive(&daemon_secret, &client_public).unwrap();
    let shared_rev = SharedKey::derive(&client_secret, &daemon_public).unwrap();
    assert!(shared.same_key(&shared_rev));

    for case in v["cases"].as_array().unwrap() {
        let name = s(&case["name"]);
        let plaintext = hex(s(&case["plaintextHex"]));
        // Rust decrypts what the real TS `encrypt` produced (random nonce inside).
        let real = hex(s(&case["realBundleHex"]));
        assert_eq!(
            shared.decrypt(&real).unwrap(),
            plaintext,
            "{name}: decrypt real bundle"
        );
        // base64 text form decodes to the same bundle.
        assert_eq!(
            b64::decode_lenient(s(&case["realBundleBase64"])).unwrap(),
            real,
            "{name}"
        );
        // Rust encrypt with TS's nonce yields identical bytes.
        let nonce: [u8; 24] = hex(s(&case["fixedNonceHex"])).try_into().unwrap();
        let ours = shared.encrypt_with_nonce(&nonce, &plaintext);
        assert_eq!(
            ours,
            hex(s(&case["fixedBundleHex"])),
            "{name}: encrypt parity"
        );
        // And the reverse key direction decrypts our output.
        assert_eq!(shared_rev.decrypt(&ours).unwrap(), plaintext);
    }
}

#[test]
fn negative_vectors_fail_like_ts() {
    let v = load("crypto");
    let shared = SharedKey::derive(
        &key32(s(&v["daemon"]["secretKeyHex"])),
        &key32(s(&v["client"]["publicKeyHex"])),
    )
    .unwrap();
    let neg = &v["negative"];
    assert_eq!(
        shared
            .decrypt(&hex(s(&neg["tamperedBundleHex"])))
            .unwrap_err()
            .0,
        "Decryption failed"
    );
    assert_eq!(
        shared
            .decrypt(&hex(s(&neg["shortBundleHex"])))
            .unwrap_err()
            .0,
        "Ciphertext bundle too short"
    );
    let zero = b64::decode_public_key(s(&neg["zeroPublicKeyB64"])).unwrap();
    assert_eq!(
        SharedKey::derive(&key32(s(&v["daemon"]["secretKeyHex"])), &zero)
            .unwrap_err()
            .0,
        "Invalid peer public key"
    );
    assert!(b64::decode_public_key(s(&neg["nonCanonicalPublicKeyB64"])).is_err());
    assert!(b64::decode_public_key(s(&neg["shortPublicKeyB64"])).is_err());
    assert_eq!(
        v["daemon"]["publicKeyB64"],
        b64::encode(&key32(s(&v["daemon"]["publicKeyHex"])))
    );
}

#[test]
fn lenient_base64_matches_ts() {
    for case in load("crypto")["base64"]["lenient"].as_array().unwrap() {
        assert_eq!(
            hex(s(&case["check"])),
            b64::decode_lenient(s(&case["input"])).unwrap(),
            "{}",
            case["input"]
        );
    }
}

#[test]
fn keypair_file_parses() {
    let v = load("keypair-file");
    let kp = parse_keypair(s(&v["fileContents"])).unwrap();
    assert_eq!(kp.public_key, key32(s(&v["publicKeyHex"])));
    assert_eq!(kp.secret_key, key32(s(&v["secretKeyHex"])));
    assert!(parse_keypair(&s(&v["fileContents"]).replace("\"v\": 2", "\"v\": 1")).is_err());
    assert!(parse_keypair("{}").is_err());
}

#[test]
fn relay_urls_match_ts() {
    for case in load("urls").as_array().unwrap() {
        let got = build_relay_url(
            s(&case["endpoint"]),
            case["useTls"].as_bool().unwrap(),
            s(&case["serverId"]),
            case["connectionId"].as_str(),
        )
        .unwrap();
        assert_eq!(got, s(&case["expected"]));
    }
}

fn daemon_keys(t: &Value) -> DaemonKeyPair {
    DaemonKeyPair {
        public_key: b64::decode_public_key(s(&t["daemon"]["publicKeyB64"])).unwrap(),
        secret_key: b64::decode_secret_key(s(&t["daemon"]["secretKeyB64"])).unwrap(),
    }
}

/// One observable outcome at the daemon boundary, same shape as the TS `events`/`received`.
#[derive(Debug, PartialEq)]
enum Seen {
    Wire(Message),
    Close(u16, String),
    App(Message),
}

#[test]
fn transcripts_replay_identically() {
    let t = load("transcripts");
    let keys = daemon_keys(&t);
    for sc in t["scenarios"].as_array().unwrap() {
        let name = s(&sc["name"]);
        let inputs = sc["inputs"].as_array().unwrap();
        let (mut channel, ready) =
            accept_hello(&keys, &frame(&inputs[0])).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            channel.binary_ciphertext(),
            sc["binaryCiphertext"].as_bool().unwrap(),
            "{name}"
        );

        let mut wire_and_close = vec![Seen::Wire(ready)];
        let mut app = Vec::new();
        for input in &inputs[1..] {
            match channel.on_frame(frame(input)) {
                Ok(Action::Deliver(m)) => app.push(Seen::App(m)),
                Ok(Action::Reply(m)) => wire_and_close.push(Seen::Wire(m)),
                Ok(Action::Close { code, reason }) => {
                    wire_and_close.push(Seen::Close(code, reason))
                }
                Ok(Action::Ignore) => {}
                Err(e) => wire_and_close.push(Seen::Close(1011, e.0)),
            }
        }

        let expected_events: Vec<Seen> = sc["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| match s(&e["t"]) {
                "wire" => Seen::Wire(frame(&e["frame"])),
                "close" => Seen::Close(
                    e["code"].as_u64().unwrap() as u16,
                    s(&e["reason"]).to_owned(),
                ),
                other => panic!("{other}"),
            })
            .collect();
        assert_eq!(wire_and_close, expected_events, "{name}: wire/close events");

        let expected_app: Vec<Seen> = sc["received"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| Seen::App(frame(f)))
            .collect();
        assert_eq!(app, expected_app, "{name}: delivered plaintext");

        // Outbound: re-encrypt each TS plaintext with TS's nonce and compare wire bytes.
        for out in sc["outbound"].as_array().unwrap() {
            let plain = frame(&out["app"]);
            let nonce: [u8; 24] = hex(s(&out["nonceHex"])).try_into().unwrap();
            assert_eq!(
                channel.encode_with_nonce(&plain, &nonce),
                frame(&out["wire"]),
                "{name}: outbound parity"
            );
            assert_eq!(
                channel.outbound_wire_len(&plain),
                match frame(&out["wire"]) {
                    Message::Text(t) => t.len(),
                    Message::Binary(b) => b.len(),
                },
                "{name}: outbound_wire_len"
            );
        }
    }
}

#[test]
fn invalid_hello_errors_match_ts() {
    let t = load("transcripts");
    let keys = daemon_keys(&t);
    for case in t["invalidHello"].as_array().unwrap() {
        let err = match accept_hello(&keys, &frame(&case["input"])) {
            Ok(_) => panic!("{} accepted", case["label"]),
            Err(e) => e,
        };
        assert_eq!(err.0, s(&case["error"]), "{}", case["label"]);
    }
}

/// Reverse direction: dumps Rust-produced frames so `verify-rust-output.mts` can decrypt them with
/// the real TS `decrypt`. Only runs when `RELAY_RUST_OUT` names a file.
#[test]
fn dump_rust_frames_for_ts_verification() {
    let Ok(out) = std::env::var("RELAY_RUST_OUT") else {
        return;
    };
    let t = load("transcripts");
    let keys = daemon_keys(&t);
    let sc = &t["scenarios"][0];
    let (channel, _) = accept_hello(&keys, &frame(&sc["inputs"][0])).unwrap();
    let samples = [
        Message::Text("rust says héllo 🌍".into()),
        Message::Binary(Bytes::from_static(&[0, 1, 2, 254, 255])),
        Message::Text(String::new()),
    ];
    let frames: Vec<Value> = samples
        .iter()
        .map(|m| {
            let (plain_kind, plain_hex) = match m {
                Message::Text(t) => ("text", t.bytes().map(|b| format!("{b:02x}")).collect::<String>()),
                Message::Binary(b) => ("binary", b.iter().map(|b| format!("{b:02x}")).collect::<String>()),
            };
            let wire = match channel.encode(m) {
                Message::Text(t) => serde_json::json!({"kind":"text","text":t}),
                Message::Binary(b) => serde_json::json!({"kind":"binary","hex":b.iter().map(|b| format!("{b:02x}")).collect::<String>()}),
            };
            serde_json::json!({"plainKind":plain_kind,"plainHex":plain_hex,"wire":wire})
        })
        .collect();
    std::fs::write(out, serde_json::to_string(&frames).unwrap()).unwrap();
}
