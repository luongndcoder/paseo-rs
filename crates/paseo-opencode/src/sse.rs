//! Incremental Server-Sent-Events parser (WHATWG `text/event-stream`).

use crate::transport::SseFrame;

#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    id: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds bytes; returns every frame completed by them.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseFrame> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            let Some(pos) = self.buf.iter().position(|b| *b == b'\n' || *b == b'\r') else {
                break;
            };
            // A lone trailing \r may be the first half of \r\n; wait for more input.
            if self.buf[pos] == b'\r' && pos + 1 == self.buf.len() {
                break;
            }
            let mut end = pos + 1;
            if self.buf[pos] == b'\r' && self.buf.get(pos + 1) == Some(&b'\n') {
                end += 1;
            }
            let line = String::from_utf8_lossy(&self.buf[..pos]).into_owned();
            self.buf.drain(..end);
            if let Some(frame) = self.line(&line) {
                frames.push(frame);
            }
        }
        frames
    }

    fn line(&mut self, line: &str) -> Option<SseFrame> {
        if line.is_empty() {
            if self.data.is_empty() && self.event.is_none() && self.id.is_none() {
                return None;
            }
            let frame = SseFrame {
                event: self.event.take(),
                id: self.id.take(),
                data: self.data.join("\n"),
            };
            self.data.clear();
            return Some(frame);
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "data" => self.data.push(value.to_owned()),
            "event" => self.event = Some(value.to_owned()),
            "id" => self.id = Some(value.to_owned()),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frames_across_chunk_boundaries() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: {\"a\":").is_empty());
        let frames = p.push(b"1}\n\ndata: second\r\n\r\n: comment\nevent: x\ndata: a\ndata: b\n\n");
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].data, "{\"a\":1}");
        assert_eq!(frames[1].data, "second");
        assert_eq!(frames[2].event.as_deref(), Some("x"));
        assert_eq!(frames[2].data, "a\nb");
    }

    #[test]
    fn crlf_split_across_chunks() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: x\r").is_empty());
        let frames = p.push(b"\n\r\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "x");
    }
}
