//! Incremental UTF-8 decoder. node-pty hands the TS worker decoded strings: incomplete trailing
//! sequences are held back and invalid bytes become U+FFFD.

#[derive(Default)]
pub struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);
        let mut out = String::with_capacity(buf.len());
        let mut rest: &[u8] = &buf;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    break;
                }
                Err(e) => {
                    let (valid, after) = rest.split_at(e.valid_up_to());
                    // SAFETY-free: valid prefix is checked by from_utf8.
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match e.error_len() {
                        None => {
                            self.pending = after.to_vec();
                            break;
                        }
                        Some(n) => {
                            out.push('\u{FFFD}');
                            rest = &after[n..];
                        }
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_back_split_sequences() {
        let mut d = Utf8Decoder::default();
        let bytes = "é世".as_bytes();
        assert_eq!(d.decode(&bytes[..1]), "");
        assert_eq!(d.decode(&bytes[1..3]), "é");
        assert_eq!(d.decode(&bytes[3..]), "世");
    }

    #[test]
    fn replaces_invalid_bytes() {
        let mut d = Utf8Decoder::default();
        assert_eq!(d.decode(b"a\xffb"), "a\u{FFFD}b");
    }
}
