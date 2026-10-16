//! Newline-delimited JSON framing.

/// Splits a byte stream into lines on `\n` only.
///
/// `node:readline` (and `str::lines` consumers that also split on U+2028 / U+2029) cut a JSON-RPC
/// message in half, because Codex emits those characters raw inside JSON strings. Splitting on the
/// byte `0x0A` and decoding whole lines keeps both that and multi-byte characters that straddle a
/// chunk boundary intact.
#[derive(Debug, Default)]
pub struct LineDecoder {
    buf: Vec<u8>,
}

impl LineDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk and return every line it completed. Blank lines are dropped and a trailing
    /// `\r` is stripped.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        let mut rest = chunk;
        while let Some(pos) = rest.iter().position(|b| *b == b'\n') {
            self.buf.extend_from_slice(&rest[..pos]);
            rest = &rest[pos + 1..];
            if let Some(line) = self.take_line() {
                lines.push(line);
            }
        }
        self.buf.extend_from_slice(rest);
        lines
    }

    /// Flush an unterminated final line (end of stream).
    pub fn finish(&mut self) -> Option<String> {
        self.take_line()
    }

    fn take_line(&mut self) -> Option<String> {
        let mut bytes = std::mem::take(&mut self.buf);
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        let line = String::from_utf8_lossy(&bytes).into_owned();
        if line.trim().is_empty() {
            None
        } else {
            Some(line)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_across_chunks_and_keeps_line_separators_inside_strings() {
        let mut d = LineDecoder::new();
        let msg = "{\"a\":\"x\u{2028}y\u{2029}z\"}";
        let bytes = format!("{msg}\n{{\"b\":1}}\r\n\n{{\"c\"").into_bytes();
        // split inside the multi-byte U+2028 sequence
        let cut = bytes.iter().position(|b| *b == 0xE2).unwrap() + 1;
        let mut lines = d.push(&bytes[..cut]);
        assert!(lines.is_empty());
        lines.extend(d.push(&bytes[cut..]));
        assert_eq!(lines, vec![msg.to_string(), "{\"b\":1}".to_string()]);
        assert_eq!(d.finish().as_deref(), Some("{\"c\""));
        assert_eq!(d.finish(), None);
    }
}
