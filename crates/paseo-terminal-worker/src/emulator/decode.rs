//! Incremental lossy UTF-8 decoder: what node-pty's `StringDecoder` does before the TS
//! terminal hands a string to xterm.js. Invalid bytes become U+FFFD (maximal-subpart
//! replacement), an incomplete tail waits for the next chunk.

#[derive(Default)]
pub struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub fn decode(&mut self, bytes: &[u8]) -> Vec<char> {
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(bytes);
        let mut out = Vec::with_capacity(input.len());
        let mut rest: &[u8] = &input;
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    out.extend(valid.chars());
                    break;
                }
                Err(error) => {
                    let (valid, tail) = rest.split_at(error.valid_up_to());
                    // SAFETY-free: `valid` is exactly the prefix `from_utf8` accepted.
                    out.extend(String::from_utf8_lossy(valid).chars());
                    match error.error_len() {
                        Some(len) => {
                            out.push('\u{FFFD}');
                            rest = &tail[len..];
                        }
                        None => {
                            self.pending = tail.to_vec();
                            break;
                        }
                    }
                }
            }
        }
        out
    }
}
