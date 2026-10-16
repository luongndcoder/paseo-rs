//! File-transfer binary frames (`FileBegin`, `FileChunk`, `FileEnd`).
//!
//! Layout: `opcode(1) | requestIdLen(1) | requestId | body`. `FileBegin` body is
//! `metadataLen(u16 BE) | JSON metadata`; `FileChunk` body is raw bytes;
//! `FileEnd` has no body.

use serde::Serialize;
use serde_json::Value;
use std::fmt;

pub const FILE_BEGIN: u8 = 0x10;
pub const FILE_CHUNK: u8 = 0x11;
pub const FILE_END: u8 = 0x12;

/// Terminal stream opcodes. Terminal frames are not decoded here.
pub const TERMINAL_OPCODES: [u8; 5] = [0x01, 0x02, 0x03, 0x04, 0x05];

/// Download chunk size: 256 KiB.
pub const FILE_EXPLORER_STREAM_CHUNK_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum FileEncoding {
    #[serde(rename = "utf-8")]
    Utf8,
    #[serde(rename = "binary")]
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileBeginMetadata {
    pub mime: String,
    pub size: u64,
    pub encoding: FileEncoding,
    pub modified_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileTransferFrame {
    Begin {
        request_id: String,
        metadata: FileBeginMetadata,
    },
    Chunk {
        request_id: String,
        payload: Vec<u8>,
    },
    End {
        request_id: String,
    },
}

impl FileTransferFrame {
    pub fn request_id(&self) -> &str {
        match self {
            FileTransferFrame::Begin { request_id, .. }
            | FileTransferFrame::Chunk { request_id, .. }
            | FileTransferFrame::End { request_id } => request_id,
        }
    }

    pub fn opcode(&self) -> u8 {
        match self {
            FileTransferFrame::Begin { .. } => FILE_BEGIN,
            FileTransferFrame::Chunk { .. } => FILE_CHUNK,
            FileTransferFrame::End { .. } => FILE_END,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    RequestIdRequired,
    RequestIdTooLong,
    MetadataTooLong,
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FrameError::RequestIdRequired => "File transfer requestId is required",
            FrameError::RequestIdTooLong => "File transfer requestId is too long",
            FrameError::MetadataTooLong => "FileBegin metadata is too long",
        })
    }
}

impl std::error::Error for FrameError {}

fn encode_request_id(request_id: &str) -> Result<&[u8], FrameError> {
    let bytes = request_id.as_bytes();
    if bytes.is_empty() {
        return Err(FrameError::RequestIdRequired);
    }
    if bytes.len() > 0xff {
        return Err(FrameError::RequestIdTooLong);
    }
    Ok(bytes)
}

pub fn encode_file_transfer_frame(frame: &FileTransferFrame) -> Result<Vec<u8>, FrameError> {
    let request_id = encode_request_id(frame.request_id())?;
    match frame {
        FileTransferFrame::Begin { metadata, .. } => {
            let json = serde_json::to_vec(metadata).expect("metadata serializes");
            if json.len() > 0xffff {
                return Err(FrameError::MetadataTooLong);
            }
            let mut out = Vec::with_capacity(4 + request_id.len() + json.len());
            out.push(FILE_BEGIN);
            out.push(request_id.len() as u8);
            out.extend_from_slice(request_id);
            out.extend_from_slice(&(json.len() as u16).to_be_bytes());
            out.extend_from_slice(&json);
            Ok(out)
        }
        FileTransferFrame::Chunk { payload, .. } => {
            let mut out = Vec::with_capacity(2 + request_id.len() + payload.len());
            out.push(FILE_CHUNK);
            out.push(request_id.len() as u8);
            out.extend_from_slice(request_id);
            out.extend_from_slice(payload);
            Ok(out)
        }
        FileTransferFrame::End { .. } => {
            let mut out = Vec::with_capacity(2 + request_id.len());
            out.push(FILE_END);
            out.push(request_id.len() as u8);
            out.extend_from_slice(request_id);
            Ok(out)
        }
    }
}

pub fn encode_begin(request_id: &str, metadata: FileBeginMetadata) -> Result<Vec<u8>, FrameError> {
    encode_file_transfer_frame(&FileTransferFrame::Begin {
        request_id: request_id.to_string(),
        metadata,
    })
}

pub fn encode_chunk(request_id: &str, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    encode_file_transfer_frame(&FileTransferFrame::Chunk {
        request_id: request_id.to_string(),
        payload: payload.to_vec(),
    })
}

pub fn encode_end(request_id: &str) -> Result<Vec<u8>, FrameError> {
    encode_file_transfer_frame(&FileTransferFrame::End {
        request_id: request_id.to_string(),
    })
}

pub fn decode_file_transfer_frame(bytes: &[u8]) -> Option<FileTransferFrame> {
    if bytes.len() < 2 {
        return None;
    }
    let opcode = bytes[0];
    if !matches!(opcode, FILE_BEGIN | FILE_CHUNK | FILE_END) {
        return None;
    }
    let id_len = bytes[1] as usize;
    if id_len == 0 || id_len > bytes.len() - 2 {
        return None;
    }
    let request_id = String::from_utf8_lossy(&bytes[2..2 + id_len]).into_owned();
    let body = &bytes[2 + id_len..];

    match opcode {
        FILE_BEGIN => {
            if body.len() < 2 {
                return None;
            }
            let meta_len = u16::from_be_bytes([body[0], body[1]]) as usize;
            if meta_len != body.len() - 2 {
                return None;
            }
            let value: Value = serde_json::from_slice(&body[2..]).ok()?;
            let metadata = parse_metadata(&value)?;
            Some(FileTransferFrame::Begin {
                request_id,
                metadata,
            })
        }
        FILE_CHUNK => Some(FileTransferFrame::Chunk {
            request_id,
            payload: body.to_vec(),
        }),
        _ => {
            if !body.is_empty() {
                return None;
            }
            Some(FileTransferFrame::End { request_id })
        }
    }
}

/// Mirrors `FileBeginMetadataSchema`; unknown keys are dropped.
fn parse_metadata(value: &Value) -> Option<FileBeginMetadata> {
    let obj = value.as_object()?;
    let mime = obj.get("mime")?.as_str()?;
    if mime.is_empty() {
        return None;
    }
    let size_f = obj.get("size")?.as_f64()?;
    if !size_f.is_finite()
        || size_f.fract() != 0.0
        || !(0.0..=9_007_199_254_740_991.0).contains(&size_f)
    {
        return None;
    }
    let encoding = match obj.get("encoding")?.as_str()? {
        "utf-8" => FileEncoding::Utf8,
        "binary" => FileEncoding::Binary,
        _ => return None,
    };
    let modified_at = obj.get("modifiedAt")?.as_str()?.to_string();
    let optional_string = |key: &str| -> Option<Option<String>> {
        match obj.get(key) {
            None => Some(None),
            Some(Value::String(s)) => Some(Some(s.clone())),
            Some(_) => None,
        }
    };
    Some(FileBeginMetadata {
        mime: mime.to_string(),
        size: size_f as u64,
        encoding,
        modified_at,
        revision: optional_string("revision")?,
        file_name: optional_string("fileName")?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryFrame {
    /// A terminal stream frame; the bytes are passed through undecoded.
    Terminal(Vec<u8>),
    FileTransfer(FileTransferFrame),
}

/// Route a binary WebSocket message by opcode.
pub fn decode_binary_frame(bytes: &[u8]) -> Option<BinaryFrame> {
    match *bytes.first()? {
        op if TERMINAL_OPCODES.contains(&op) => Some(BinaryFrame::Terminal(bytes.to_vec())),
        FILE_BEGIN | FILE_CHUNK | FILE_END => {
            decode_file_transfer_frame(bytes).map(BinaryFrame::FileTransfer)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> FileBeginMetadata {
        FileBeginMetadata {
            mime: "image/png".into(),
            size: 6,
            encoding: FileEncoding::Binary,
            modified_at: "2026-05-02T00:00:00.000Z".into(),
            revision: None,
            file_name: None,
        }
    }

    #[test]
    fn opcodes_do_not_overlap_terminal() {
        for op in [FILE_BEGIN, FILE_CHUNK, FILE_END] {
            assert!(!TERMINAL_OPCODES.contains(&op));
        }
    }

    #[test]
    fn file_begin_layout_and_roundtrip() {
        let encoded = encode_begin("req-1", meta()).unwrap();
        let json = br#"{"mime":"image/png","size":6,"encoding":"binary","modifiedAt":"2026-05-02T00:00:00.000Z"}"#;
        assert_eq!(encoded[0], FILE_BEGIN);
        assert_eq!(encoded[1], 5);
        assert_eq!(&encoded[2..7], b"req-1");
        assert_eq!(
            u16::from_be_bytes([encoded[7], encoded[8]]) as usize,
            json.len()
        );
        assert_eq!(&encoded[9..], json);
        assert_eq!(
            decode_file_transfer_frame(&encoded),
            Some(FileTransferFrame::Begin {
                request_id: "req-1".into(),
                metadata: meta()
            })
        );
    }

    #[test]
    fn begin_metadata_serializes_revision_and_file_name_in_schema_order() {
        let mut m = meta();
        m.revision = Some("1:2:6:7".into());
        m.file_name = Some("a.png".into());
        let encoded = encode_begin("r", m).unwrap();
        let text = String::from_utf8(encoded[5..].to_vec()).unwrap();
        assert_eq!(
            text,
            r#"{"mime":"image/png","size":6,"encoding":"binary","modifiedAt":"2026-05-02T00:00:00.000Z","revision":"1:2:6:7","fileName":"a.png"}"#
        );
    }

    #[test]
    fn file_chunk_layout_and_roundtrip() {
        let payload = [0u8, 1, 2, 253, 254, 255];
        let encoded = encode_chunk("req-1", &payload).unwrap();
        assert_eq!(encoded[0], FILE_CHUNK);
        assert_eq!(encoded[1], 5);
        assert_eq!(&encoded[2..7], b"req-1");
        assert_eq!(&encoded[7..], &payload);
        assert_eq!(
            decode_file_transfer_frame(&encoded),
            Some(FileTransferFrame::Chunk {
                request_id: "req-1".into(),
                payload: payload.to_vec()
            })
        );
    }

    #[test]
    fn file_end_is_prefix_only() {
        let encoded = encode_end("req-1").unwrap();
        assert_eq!(encoded, [&[FILE_END, 5][..], b"req-1"].concat());
        assert_eq!(
            decode_file_transfer_frame(&encoded),
            Some(FileTransferFrame::End {
                request_id: "req-1".into()
            })
        );
    }

    #[test]
    fn rejects_malformed_metadata_but_ignores_unknown_fields() {
        let mut bad = meta();
        bad.size = 1;
        let mut raw = encode_begin("req-1", bad).unwrap();
        // size -1 cannot be built through the typed API; patch the JSON text.
        let json = br#"{"mime":"image/png","size":-1,"encoding":"binary","modifiedAt":"x"}"#;
        raw.truncate(7);
        raw.extend_from_slice(&(json.len() as u16).to_be_bytes());
        raw.extend_from_slice(json);
        assert_eq!(decode_file_transfer_frame(&raw), None);

        let json = br#"{"mime":"image/png","size":1,"encoding":"binary","modifiedAt":"2026-05-02T00:00:00.000Z","extra":true}"#;
        let mut raw = vec![FILE_BEGIN, 5];
        raw.extend_from_slice(b"req-1");
        raw.extend_from_slice(&(json.len() as u16).to_be_bytes());
        raw.extend_from_slice(json);
        let mut expected = meta();
        expected.size = 1;
        assert_eq!(
            decode_file_transfer_frame(&raw),
            Some(FileTransferFrame::Begin {
                request_id: "req-1".into(),
                metadata: expected
            })
        );
    }

    #[test]
    fn rejects_malformed_prefixes_and_tails() {
        assert_eq!(decode_file_transfer_frame(&[FILE_END, 0]), None);
        assert_eq!(decode_file_transfer_frame(&[FILE_END, 6, 1]), None);
        let mut tail = encode_end("req-1").unwrap();
        tail.push(1);
        assert_eq!(decode_file_transfer_frame(&tail), None);
        assert_eq!(decode_file_transfer_frame(&[FILE_END]), None);
        assert_eq!(decode_file_transfer_frame(&[0x01, 1, b'a']), None);
    }

    #[test]
    fn encode_rejects_bad_request_ids() {
        assert_eq!(encode_end("").unwrap_err(), FrameError::RequestIdRequired);
        assert_eq!(
            encode_end(&"x".repeat(256)).unwrap_err(),
            FrameError::RequestIdTooLong
        );
    }

    #[test]
    fn demux_routes_by_opcode() {
        let chunk = encode_chunk("req-upload", b"hello").unwrap();
        assert_eq!(
            decode_binary_frame(&chunk),
            Some(BinaryFrame::FileTransfer(FileTransferFrame::Chunk {
                request_id: "req-upload".into(),
                payload: b"hello".to_vec()
            }))
        );
        let terminal = [0x02, 0, 7, b'l', b's'];
        assert_eq!(
            decode_binary_frame(&terminal),
            Some(BinaryFrame::Terminal(terminal.to_vec()))
        );
        assert_eq!(decode_binary_frame(&[0xff, 0]), None);
        assert_eq!(decode_binary_frame(&[]), None);
    }
}
