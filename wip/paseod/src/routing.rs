//! Routing: the dispatch hook that lets domains move from TS to Rust one at a time.
//!
//! P2 ships an EMPTY [`OwnedTypes`]: every frame is [`Route::Forward`] and bytes pass
//! through untouched. The peek must never allocate the frame body or re-serialize it.
//!
//! Wire shape peeked (see `packages/protocol/src/messages.ts`):
//! - text frame `{"type":"session","message":{"type":"<t>", ...}}`  -> route key `<t>`
//! - text frame `{"type":"<t>", ...}` for non-session envelopes     -> route key `<t>`
//! - binary frame: first byte is the opcode (`terminal.ts`, `file-transfer.ts`)

use std::collections::HashMap;

/// A domain that has moved to Rust. Add variants as domains land (P3+).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Domain {
    /// Reserved so the enum is non-empty; never registered in P2.
    #[doc(hidden)]
    Placeholder,
}

/// Binary frame families by opcode (first byte). Mirrors `TerminalStreamOpcode` and `FileTransferOpcode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryFamily {
    /// 0x01..=0x05: Output, Input, Resize, Snapshot, Restore.
    TerminalStream,
    /// 0x10..=0x12: FileBegin, FileChunk, FileEnd.
    FileTransfer,
    /// Any other first byte, or an empty frame. Always forwarded.
    Unknown,
}

pub fn binary_family(frame: &[u8]) -> BinaryFamily {
    match frame.first() {
        Some(0x01..=0x05) => BinaryFamily::TerminalStream,
        Some(0x10..=0x12) => BinaryFamily::FileTransfer,
        _ => BinaryFamily::Unknown,
    }
}

/// Decision for one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Send the original bytes to the TS backend unchanged.
    Forward,
    /// A Rust domain handles it.
    Owned(Domain),
}

/// Routing table. Message types and binary families map to the owning [`Domain`].
#[derive(Debug, Default, Clone)]
pub struct OwnedTypes {
    types: HashMap<&'static str, Domain>,
    binary: HashMap<BinaryFamilyKey, Domain>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum BinaryFamilyKey {
    Terminal,
    File,
}

impl OwnedTypes {
    /// The P2 table: nothing owned, pure passthrough.
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.types.is_empty() && self.binary.is_empty()
    }

    /// Claims a text message type (inner `session.message.type` or top-level `type`).
    pub fn claim_type(&mut self, message_type: &'static str, domain: Domain) {
        self.types.insert(message_type, domain);
    }

    /// Claims a whole binary family.
    pub fn claim_binary(&mut self, family: BinaryFamily, domain: Domain) {
        match family {
            BinaryFamily::TerminalStream => {
                self.binary.insert(BinaryFamilyKey::Terminal, domain);
            }
            BinaryFamily::FileTransfer => {
                self.binary.insert(BinaryFamilyKey::File, domain);
            }
            BinaryFamily::Unknown => {}
        }
    }

    fn lookup_type(&self, t: &str) -> Option<Domain> {
        self.types.get(t).copied()
    }

    fn lookup_binary(&self, family: BinaryFamily) -> Option<Domain> {
        match family {
            BinaryFamily::TerminalStream => self.binary.get(&BinaryFamilyKey::Terminal).copied(),
            BinaryFamily::FileTransfer => self.binary.get(&BinaryFamilyKey::File).copied(),
            BinaryFamily::Unknown => None,
        }
    }
}

/// Borrowed view of an inbound frame.
#[derive(Debug, Clone, Copy)]
pub enum FrameRef<'a> {
    Text(&'a str),
    Binary(&'a [u8]),
}

#[derive(serde::Deserialize)]
struct PeekEnvelope<'a> {
    #[serde(borrow, default, rename = "type")]
    kind: Option<&'a str>,
    #[serde(borrow, default)]
    message: Option<PeekInner<'a>>,
}

#[derive(serde::Deserialize)]
struct PeekInner<'a> {
    #[serde(borrow, default, rename = "type")]
    kind: Option<&'a str>,
}

/// Returns the routing key of a text frame: `session.message.type` for session
/// envelopes, else the top-level `type`. `None` when the frame is not JSON or has no type.
pub fn peek_type(text: &str) -> Option<&str> {
    let env: PeekEnvelope<'_> = serde_json::from_str(text).ok()?;
    match env.kind? {
        "session" => env.message.and_then(|m| m.kind),
        other => Some(other),
    }
}

#[derive(serde::Deserialize)]
struct PeekStatusEnvelope<'a> {
    #[serde(borrow, default, rename = "type")]
    kind: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow, default)]
    message: Option<PeekStatusMessage<'a>>,
}

#[derive(serde::Deserialize)]
struct PeekStatusMessage<'a> {
    #[serde(borrow, default, rename = "type")]
    kind: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow, default)]
    payload: Option<PeekStatusPayload<'a>>,
}

#[derive(serde::Deserialize)]
struct PeekStatusPayload<'a> {
    #[serde(borrow, default)]
    status: Option<std::borrow::Cow<'a, str>>,
}

/// True when `text` is the top-level application ping `{"type":"ping"}` (any key order or
/// whitespace). Cheap substring precheck first, so ordinary frames never get parsed.
pub fn is_top_level_ping(text: &str) -> bool {
    if text.len() > 256 || !text.contains("\"ping\"") {
        return false;
    }
    serde_json::from_str::<PeekEnvelope<'_>>(text)
        .ok()
        .and_then(|e| e.kind)
        .is_some_and(|k| k == "ping")
}

/// True when `text` is the outbound `server_info` status frame
/// (`session.message.type == "status"` and `payload.status == "server_info"`).
pub fn is_server_info(text: &str) -> bool {
    if !text.contains("server_info") {
        return false;
    }
    let Ok(env) = serde_json::from_str::<PeekStatusEnvelope<'_>>(text) else {
        return false;
    };
    env.kind.as_deref() == Some("session")
        && env.message.is_some_and(|m| {
            m.kind.as_deref() == Some("status")
                && m.payload
                    .and_then(|p| p.status)
                    .is_some_and(|s| s == "server_info")
        })
}

/// Decides where one inbound frame goes. Never fails: anything unparseable forwards,
/// so TS stays the single authority on invalid-frame handling.
pub fn peek_route(table: &OwnedTypes, frame: FrameRef<'_>) -> Route {
    if table.is_empty() {
        return Route::Forward;
    }
    let owner = match frame {
        FrameRef::Text(text) => peek_type(text).and_then(|t| table.lookup_type(t)),
        FrameRef::Binary(bytes) => table.lookup_binary(binary_family(bytes)),
    };
    owner.map_or(Route::Forward, Route::Owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_table_forwards_everything() {
        let t = OwnedTypes::empty();
        assert_eq!(
            peek_route(&t, FrameRef::Text("{\"type\":\"ping\"}")),
            Route::Forward
        );
        assert_eq!(peek_route(&t, FrameRef::Binary(&[0x01, 2])), Route::Forward);
        assert_eq!(peek_route(&t, FrameRef::Text("not json")), Route::Forward);
    }

    #[test]
    fn peeks_session_inner_type_and_top_level_type() {
        assert_eq!(
            peek_type(
                r#"{"type":"session","message":{"type":"fetch_agents_request","requestId":"1"}}"#
            ),
            Some("fetch_agents_request")
        );
        assert_eq!(peek_type(r#"{"type":"ping"}"#), Some("ping"));
        assert_eq!(peek_type(r#"{"nope":1}"#), None);
        assert_eq!(peek_type("garbage"), None);
    }

    #[test]
    fn owned_type_routes_to_domain() {
        let mut t = OwnedTypes::empty();
        t.claim_type("fetch_agents_request", Domain::Placeholder);
        let hit = r#"{"type":"session","message":{"type":"fetch_agents_request"}}"#;
        let miss = r#"{"type":"session","message":{"type":"other"}}"#;
        assert_eq!(
            peek_route(&t, FrameRef::Text(hit)),
            Route::Owned(Domain::Placeholder)
        );
        assert_eq!(peek_route(&t, FrameRef::Text(miss)), Route::Forward);
    }

    #[test]
    fn detects_ping_and_server_info() {
        assert!(is_top_level_ping(r#"{"type":"ping"}"#));
        assert!(is_top_level_ping(r#"{ "type" : "ping" }"#));
        assert!(!is_top_level_ping(
            r#"{"type":"session","message":{"type":"ping"}}"#
        ));
        assert!(!is_top_level_ping(r#"{"type":"pong"}"#));
        let si = r#"{"type":"session","message":{"type":"status","payload":{"status":"server_info","serverId":"x"}}}"#;
        assert!(is_server_info(si));
        assert!(!is_server_info(
            r#"{"type":"session","message":{"type":"status","payload":{"status":"agent_created"}}}"#
        ));
        assert!(!is_server_info("server_info"));
    }

    #[test]
    fn binary_families_by_opcode() {
        assert_eq!(binary_family(&[0x03, 0]), BinaryFamily::TerminalStream);
        assert_eq!(binary_family(&[0x11]), BinaryFamily::FileTransfer);
        assert_eq!(binary_family(&[0x7f]), BinaryFamily::Unknown);
        assert_eq!(binary_family(&[]), BinaryFamily::Unknown);
        let mut t = OwnedTypes::empty();
        t.claim_binary(BinaryFamily::FileTransfer, Domain::Placeholder);
        assert_eq!(
            peek_route(&t, FrameRef::Binary(&[0x10])),
            Route::Owned(Domain::Placeholder)
        );
        assert_eq!(peek_route(&t, FrameRef::Binary(&[0x01])), Route::Forward);
    }
}
