//! Daemon side of the E2EE channel, ported from `packages/relay/src/encrypted-channel.ts`
//! (`createDaemonChannel` + `EncryptedChannel`). Pure state machine: no IO, no timers.
//! The transport (`socket.rs`) feeds frames in and writes the returned frames out.

use bytes::Bytes;
use serde_json::Value;

use super::b64;
use super::crypto::{SharedKey, NONCE_LEN, OVERHEAD_BYTES};
use super::keypair::DaemonKeyPair;
use crate::ws::Message;

/// `REHANDSHAKE_REJECTION_CODE`.
pub const REHANDSHAKE_REJECTION_CODE: u16 = 1008;
pub const REHANDSHAKE_KEY_MISMATCH_REASON: &str = "E2EE re-handshake key mismatch";
/// Close code TS uses for decrypt/protocol failures (`transport.close(1011, err.message)`).
pub const PROTOCOL_ERROR_CODE: u16 = 1011;

/// Error whose text is surfaced to the peer as a close reason, so strings match TS exactly.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ChannelError(pub String);

impl ChannelError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// What the transport must do after feeding one inbound frame to an open channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Decrypted application message for the proxy.
    Deliver(Message),
    /// Plaintext handshake frame to send back (`e2ee_ready` re-send).
    Reply(Message),
    /// Close the transport with this code/reason.
    Close { code: u16, reason: String },
    /// Stray handshake traffic, nothing to do.
    Ignore,
}

/// A parsed, valid `e2ee_hello` (`isE2EEHelloMessage`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub key: String,
    pub binary_ciphertext: bool,
}

fn capabilities_valid(v: &Value) -> bool {
    match v.get("capabilities") {
        None => true,
        Some(Value::Object(m)) => matches!(m.get("binaryCiphertext"), None | Some(Value::Bool(_))),
        Some(_) => false,
    }
}

fn binary_capability(v: &Value) -> bool {
    v.get("capabilities")
        .and_then(|c| c.get("binaryCiphertext"))
        .and_then(Value::as_bool)
        == Some(true)
}

fn parse_hello_value(v: &Value) -> Option<Hello> {
    let obj = v.as_object()?;
    if obj.get("type")?.as_str()? != "e2ee_hello" {
        return None;
    }
    let key = obj.get("key")?.as_str()?;
    if key.trim().is_empty() || !capabilities_valid(v) {
        return None;
    }
    Some(Hello {
        key: key.to_owned(),
        binary_ciphertext: binary_capability(v),
    })
}

fn is_ready_value(v: &Value) -> bool {
    v.as_object()
        .is_some_and(|o| o.get("type").and_then(Value::as_str) == Some("e2ee_ready"))
        && capabilities_valid(v)
}

/// `{"type":"e2ee_ready"[,"capabilities":{"binaryCiphertext":true}]}` byte-for-byte as
/// `JSON.stringify` emits it (key order matters for parity, so no `serde_json::Map`).
pub fn ready_text(binary_ciphertext: bool) -> String {
    if binary_ciphertext {
        r#"{"type":"e2ee_ready","capabilities":{"binaryCiphertext":true}}"#.to_owned()
    } else {
        r#"{"type":"e2ee_ready"}"#.to_owned()
    }
}

/// `buildInvalidHelloError`.
fn invalid_hello_error(raw: &str, parsed: Option<&Value>) -> ChannelError {
    let obj = parsed.and_then(Value::as_object);
    let received_type = match obj.and_then(|o| o.get("type")) {
        Some(Value::String(s)) => s.clone(),
        None => "undefined".to_owned(),
        Some(Value::Null) => "object".to_owned(),
        Some(Value::Bool(_)) => "boolean".to_owned(),
        Some(Value::Number(_)) => "number".to_owned(),
        Some(Value::Array(_) | Value::Object(_)) => "object".to_owned(),
    };
    let has_key = obj
        .and_then(|o| o.get("key"))
        .and_then(Value::as_str)
        .is_some_and(|k| !k.trim().is_empty());
    let compact = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview = if compact.chars().count() > 160 {
        let head: String = compact.chars().take(157).collect();
        format!("{head}...")
    } else {
        compact
    };
    let preview_json = serde_json::to_string(&preview).unwrap_or_else(|_| "\"\"".to_owned());
    ChannelError::new(format!(
        "Invalid hello message (receivedType={received_type}, hasKey={has_key}, preview={preview_json})"
    ))
}

/// `createDaemonChannel` handshake step: validates the first frame, derives the key, and
/// returns the open channel plus the plaintext `e2ee_ready` reply to send.
///
/// Frames that arrive after the hello while the reply is in flight are handled in order by
/// the caller (they reach [`DaemonChannel::on_frame`]), which is equivalent to TS buffering them.
pub fn accept_hello(
    keys: &DaemonKeyPair,
    first: &Message,
) -> Result<(DaemonChannel, Message), ChannelError> {
    let text = match first {
        Message::Binary(_) => return Err(invalid_hello_error("<binary frame>", None)),
        Message::Text(t) => t,
    };
    let parsed: Value = serde_json::from_str(text).map_err(|_| invalid_hello_error(text, None))?;
    let hello =
        parse_hello_value(&parsed).ok_or_else(|| invalid_hello_error(text, Some(&parsed)))?;
    let client_public = b64::decode_public_key(&hello.key)?;
    let shared = SharedKey::derive(&keys.secret_key, &client_public)?;
    let channel = DaemonChannel {
        keys: keys.clone(),
        shared,
        binary_ciphertext: hello.binary_ciphertext,
    };
    let ready = Message::Text(ready_text(hello.binary_ciphertext));
    Ok((channel, ready))
}

/// Open E2EE channel (daemon role).
#[derive(Debug)]
pub struct DaemonChannel {
    keys: DaemonKeyPair,
    shared: SharedKey,
    binary_ciphertext: bool,
}

impl DaemonChannel {
    pub fn binary_ciphertext(&self) -> bool {
        self.binary_ciphertext
    }

    /// Handles one inbound frame on an open channel (`EncryptedChannel.handleMessage`, open state).
    /// `Err` means "close the transport with 1011 and this message".
    pub fn on_frame(&mut self, frame: Message) -> Result<Action, ChannelError> {
        if let Message::Text(text) = &frame {
            if text.trim().starts_with('{') {
                if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                    if let Some(hello) = parse_hello_value(&parsed) {
                        return self.handle_rehello(&hello);
                    }
                    if is_ready_value(&parsed) {
                        return Ok(Action::Ignore);
                    }
                    return Err(ChannelError::new(
                        "Received plaintext frame on encrypted channel",
                    ));
                }
                // Unparseable: fall through to ciphertext decoding like TS.
            }
        }
        let (bundle, plaintext_kind) = self.extract_ciphertext(frame)?;
        let plaintext = self.shared.decrypt(&bundle)?;
        decode_plaintext(plaintext, plaintext_kind).map(Action::Deliver)
    }

    fn extract_ciphertext(&self, frame: Message) -> Result<(Vec<u8>, PlaintextKind), ChannelError> {
        match (self.binary_ciphertext, frame) {
            (true, Message::Binary(b)) => Ok((b.to_vec(), PlaintextKind::Binary)),
            (true, Message::Text(t)) => {
                decode_text_ciphertext(&t).map(|b| (b, PlaintextKind::Text))
            }
            // COMPAT(binaryCiphertext): added in v0.2.3, remove legacy base64-only receive
            // mode after 2027-01-27 (mirrors `encrypted-channel.ts`).
            (false, Message::Text(t)) => {
                decode_text_ciphertext(&t).map(|b| (b, PlaintextKind::Legacy))
            }
            (false, Message::Binary(b)) => {
                let as_text = String::from_utf8_lossy(&b);
                match b64::decode_lenient(&as_text) {
                    Some(bundle) => Ok((bundle, PlaintextKind::Legacy)),
                    None => Ok((b.to_vec(), PlaintextKind::Legacy)),
                }
            }
        }
    }

    fn handle_rehello(&mut self, hello: &Hello) -> Result<Action, ChannelError> {
        let client_public = b64::decode_public_key(&hello.key)?;
        let retry_key = SharedKey::derive(&self.keys.secret_key, &client_public)?;
        if !retry_key.same_key(&self.shared) {
            return Ok(Action::Close {
                code: REHANDSHAKE_REJECTION_CODE,
                reason: REHANDSHAKE_KEY_MISMATCH_REASON.to_owned(),
            });
        }
        Ok(Action::Reply(Message::Text(ready_text(
            self.binary_ciphertext,
        ))))
    }

    /// Wire length of `encode(msg)` without encrypting (`outboundWireByteLength`).
    pub fn outbound_wire_len(&self, msg: &Message) -> usize {
        let plaintext = msg.byte_len();
        if self.binary_ciphertext && matches!(msg, Message::Binary(_)) {
            plaintext + OVERHEAD_BYTES
        } else {
            base64_wire_len(plaintext)
        }
    }

    /// `EncryptedChannel.send`: encrypts and picks the wire representation.
    pub fn encode(&self, msg: &Message) -> Message {
        self.encode_with(msg, |plain| self.shared.encrypt(plain))
    }

    /// Deterministic variant for vectors: caller supplies the nonce.
    pub fn encode_with_nonce(&self, msg: &Message, nonce: &[u8; NONCE_LEN]) -> Message {
        self.encode_with(msg, |plain| self.shared.encrypt_with_nonce(nonce, plain))
    }

    fn encode_with(&self, msg: &Message, enc: impl FnOnce(&[u8]) -> Vec<u8>) -> Message {
        match msg {
            Message::Binary(b) => {
                let bundle = enc(b);
                if self.binary_ciphertext {
                    Message::Binary(Bytes::from(bundle))
                } else {
                    Message::Text(b64::encode(&bundle))
                }
            }
            Message::Text(t) => Message::Text(b64::encode(&enc(t.as_bytes()))),
        }
    }
}

/// `base64EncryptedWireByteLength`.
pub fn base64_wire_len(plaintext_bytes: usize) -> usize {
    4 * (plaintext_bytes + OVERHEAD_BYTES).div_ceil(3)
}

#[derive(Clone, Copy)]
enum PlaintextKind {
    Binary,
    Text,
    Legacy,
}

/// Text ciphertext -> bundle. TS's `base64-js` never throws on junk, it just yields bytes that
/// fail later, so a junk string is mapped to the error it would have produced.
fn decode_text_ciphertext(text: &str) -> Result<Vec<u8>, ChannelError> {
    match b64::decode_lenient(text) {
        Some(b) => Ok(b),
        None => {
            let approx_bytes = text.trim().len().div_ceil(4) * 3;
            if approx_bytes < NONCE_LEN {
                Err(ChannelError::new("Ciphertext bundle too short"))
            } else {
                Err(ChannelError::new("Decryption failed"))
            }
        }
    }
}

fn decode_plaintext(bytes: Vec<u8>, kind: PlaintextKind) -> Result<Message, ChannelError> {
    match kind {
        PlaintextKind::Binary => Ok(Message::Binary(Bytes::from(bytes))),
        PlaintextKind::Text => String::from_utf8(bytes)
            .map(Message::Text)
            .map_err(|_| ChannelError::new("The encoded data was not valid for encoding utf-8")),
        PlaintextKind::Legacy => Ok(match String::from_utf8(bytes) {
            Ok(s) => Message::Text(s),
            Err(e) => Message::Binary(Bytes::from(e.into_bytes())),
        }),
    }
}
