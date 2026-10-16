//! JSONL frame decoding, including OMP protocol v2 chunked frames (`rpc_chunk`).

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::{Map, Value};

pub const V2_FRAME_BYTES: u64 = 1024 * 1024;
pub const V2_REASSEMBLED_BYTES: u64 = 64 * 1024 * 1024;
const V2_CHUNK_BYTES: usize = 256 * 1024;
const V2_BASE64_CHARS: usize = V2_CHUNK_BYTES.div_ceil(3) * 4;
const V2_MAX_CHUNKS: u64 = V2_REASSEMBLED_BYTES.div_ceil(V2_CHUNK_BYTES as u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameProblem {
    InvalidJson,
    InvalidChunk,
    InvalidBase64,
    OutOfOrderChunk,
    ChunkLengthMismatch,
    InvalidChunkPayload,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decoded {
    /// Blank line, a non-object value, or a chunk that is not the last one yet.
    Nothing,
    Frame(Map<String, Value>),
    Problem(FrameProblem),
}

/// Whether a `ready` frame offers the chunked v2 transport with the sizes this client supports.
pub fn supports_protocol_v2(ready: &Value) -> bool {
    ready.get("type").and_then(Value::as_str) == Some("ready")
        && ready
            .get("supportedProtocolVersions")
            .and_then(Value::as_array)
            .is_some_and(|v| v.iter().any(|x| x.as_u64() == Some(2)))
        && ready.get("maxFrameBytes").and_then(Value::as_u64) == Some(V2_FRAME_BYTES)
        && ready
            .get("maxReassembledFrameBytes")
            .and_then(Value::as_u64)
            == Some(V2_REASSEMBLED_BYTES)
}

struct Assembly {
    id: String,
    count: u64,
    byte_length: u64,
    parts: Vec<Vec<u8>>,
    bytes: u64,
}

struct ChunkHeader<'a> {
    id: &'a str,
    index: u64,
    count: u64,
    byte_length: u64,
    data: &'a str,
}

fn chunk_header(frame: &Map<String, Value>) -> Option<ChunkHeader<'_>> {
    let id = frame.get("chunkId")?.as_str()?;
    let index = frame.get("index")?.as_u64()?;
    let count = frame.get("count")?.as_u64()?;
    let byte_length = frame.get("byteLength")?.as_u64()?;
    let data = frame.get("data")?.as_str()?;
    if id.is_empty()
        || id.len() > 128
        || !(2..=V2_MAX_CHUNKS).contains(&count)
        || index >= count
        || !(V2_FRAME_BYTES..=V2_REASSEMBLED_BYTES).contains(&byte_length)
        || data.is_empty()
    {
        return None;
    }
    Some(ChunkHeader {
        id,
        index,
        count,
        byte_length,
        data,
    })
}

fn chunk_bytes(data: &str) -> Option<Vec<u8>> {
    if data.len() > V2_BASE64_CHARS {
        return None;
    }
    let bytes = STANDARD.decode(data).ok()?;
    if bytes.len() > V2_CHUNK_BYTES || STANDARD.encode(&bytes) != data {
        return None;
    }
    Some(bytes)
}

/// Stateful per-connection decoder: feed it one line at a time.
#[derive(Default)]
pub struct FrameDecoder {
    assembly: Option<Assembly>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn consume_line(&mut self, line: &str) -> Decoded {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.trim().is_empty() {
            return Decoded::Nothing;
        }
        let Ok(parsed) = serde_json::from_str::<Value>(line) else {
            return Decoded::Problem(FrameProblem::InvalidJson);
        };
        let Value::Object(frame) = parsed else {
            return Decoded::Nothing;
        };
        if frame.get("type").and_then(Value::as_str) != Some("rpc_chunk") {
            return Decoded::Frame(frame);
        }
        self.consume_chunk(&frame)
    }

    fn drop_assembly(&mut self, problem: FrameProblem) -> Decoded {
        self.assembly = None;
        Decoded::Problem(problem)
    }

    fn consume_chunk(&mut self, frame: &Map<String, Value>) -> Decoded {
        let Some(header) = chunk_header(frame) else {
            return self.drop_assembly(FrameProblem::InvalidChunk);
        };
        let Some(bytes) = chunk_bytes(header.data) else {
            return self.drop_assembly(FrameProblem::InvalidBase64);
        };
        if self.assembly.is_none() {
            if header.index != 0 {
                return Decoded::Problem(FrameProblem::OutOfOrderChunk);
            }
            self.assembly = Some(Assembly {
                id: header.id.to_string(),
                count: header.count,
                byte_length: header.byte_length,
                parts: Vec::new(),
                bytes: 0,
            });
        }
        let assembly = self.assembly.as_mut().expect("assembly was just set");
        if assembly.id != header.id
            || assembly.count != header.count
            || assembly.byte_length != header.byte_length
            || assembly.parts.len() as u64 != header.index
        {
            return self.drop_assembly(FrameProblem::OutOfOrderChunk);
        }
        assembly.bytes += bytes.len() as u64;
        assembly.parts.push(bytes);
        if assembly.bytes > assembly.byte_length {
            return self.drop_assembly(FrameProblem::ChunkLengthMismatch);
        }
        if (assembly.parts.len() as u64) < assembly.count {
            return Decoded::Nothing;
        }
        let assembly = self.assembly.take().expect("assembly is present");
        if assembly.bytes != assembly.byte_length {
            return Decoded::Problem(FrameProblem::ChunkLengthMismatch);
        }
        let joined: Vec<u8> = assembly.parts.concat();
        let Ok(text) = String::from_utf8(joined) else {
            return Decoded::Problem(FrameProblem::InvalidChunkPayload);
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(frame)) => Decoded::Frame(frame),
            _ => Decoded::Problem(FrameProblem::InvalidChunkPayload),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunk(id: &str, index: u64, count: u64, byte_length: u64, bytes: &[u8]) -> String {
        json!({"type":"rpc_chunk","chunkId":id,"index":index,"count":count,
               "byteLength":byte_length,"data":STANDARD.encode(bytes)})
        .to_string()
    }

    #[test]
    fn plain_frames_pass_through_and_garbage_is_reported() {
        let mut d = FrameDecoder::new();
        assert!(matches!(
            d.consume_line(r#"{"type":"agent_start"}"#),
            Decoded::Frame(_)
        ));
        assert_eq!(d.consume_line("   "), Decoded::Nothing);
        assert_eq!(d.consume_line("[1]"), Decoded::Nothing);
        assert_eq!(
            d.consume_line("{nope"),
            Decoded::Problem(FrameProblem::InvalidJson)
        );
    }

    #[test]
    fn reassembles_chunks() {
        // The decoder only accepts byteLength >= 1 MiB, so pad a real payload.
        let payload = format!(
            r#"{{"type":"response","id":"req_1","pad":"{}"}}"#,
            "x".repeat(V2_FRAME_BYTES as usize)
        );
        let bytes = payload.as_bytes();
        let (a, b) = bytes.split_at(V2_CHUNK_BYTES);
        let (b, c) = b.split_at(V2_CHUNK_BYTES);
        let total = bytes.len() as u64;
        let mut d = FrameDecoder::new();
        // 1 MiB + header needs 5 chunks of 256 KiB; use exactly the ones we need.
        let rest: Vec<&[u8]> = c.chunks(V2_CHUNK_BYTES).collect();
        let count = 2 + rest.len() as u64;
        assert_eq!(
            d.consume_line(&chunk("c1", 0, count, total, a)),
            Decoded::Nothing
        );
        assert_eq!(
            d.consume_line(&chunk("c1", 1, count, total, b)),
            Decoded::Nothing
        );
        let mut last = Decoded::Nothing;
        for (i, part) in rest.iter().enumerate() {
            last = d.consume_line(&chunk("c1", 2 + i as u64, count, total, part));
        }
        match last {
            Decoded::Frame(f) => assert_eq!(f["id"], "req_1"),
            other => panic!("expected frame, got {other:?}"),
        }
    }

    #[test]
    fn out_of_order_chunk_resets() {
        let mut d = FrameDecoder::new();
        let line = chunk("c1", 1, 4, V2_FRAME_BYTES, b"abc");
        assert_eq!(
            d.consume_line(&line),
            Decoded::Problem(FrameProblem::OutOfOrderChunk)
        );
    }

    #[test]
    fn v2_ready_detection() {
        let ready = json!({"type":"ready","supportedProtocolVersions":[1,2],
            "maxFrameBytes":V2_FRAME_BYTES,"maxReassembledFrameBytes":V2_REASSEMBLED_BYTES});
        assert!(supports_protocol_v2(&ready));
        assert!(!supports_protocol_v2(&json!({"type":"ready"})));
    }
}
