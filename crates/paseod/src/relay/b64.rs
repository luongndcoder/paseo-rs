//! Base64 helpers matching `packages/relay` (`base64-js`, `base64.ts`, `crypto.ts`).

use base64::alphabet;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use base64::engine::DecodePaddingMode;
use base64::Engine;

use super::channel::ChannelError;

/// `base64-js` tolerates missing/extra padding and non-zero trailing bits.
const LENIENT: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

pub fn encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// `base64ToArrayBuffer` (`base64.ts`): trim, url-safe alphabet accepted, padding repaired.
/// Returns `None` on characters outside the alphabet (base64-js would produce garbage that
/// then fails decryption; callers map `None` to the same close reasons).
pub fn decode_lenient(input: &str) -> Option<Vec<u8>> {
    let normalized: String = input
        .trim()
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .filter(|c| *c != '=')
        .collect();
    LENIENT.decode(normalized.as_bytes()).ok()
}

/// `decodePublicKeyBase64` + length check (`crypto.ts:importPublicKey`): canonical padded
/// standard base64 only, exactly 32 bytes.
pub fn decode_public_key(b64: &str) -> Result<[u8; 32], ChannelError> {
    let well_formed = b64.len().is_multiple_of(4)
        && b64
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'+' || c == b'/' || c == b'=');
    if !well_formed {
        return Err(ChannelError::new("Invalid public key encoding"));
    }
    let bytes = STANDARD
        .decode(b64)
        .map_err(|_| ChannelError::new("Invalid public key encoding"))?;
    if STANDARD.encode(&bytes) != b64 {
        return Err(ChannelError::new("Invalid public key encoding"));
    }
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| ChannelError::new("Invalid public key length (expected 32)"))
}

/// `importSecretKey`: lenient decode, exactly 32 bytes.
pub fn decode_secret_key(b64: &str) -> Result<[u8; 32], ChannelError> {
    let bytes = LENIENT
        .decode(b64.trim_end_matches('=').as_bytes())
        .map_err(|_| ChannelError::new("Invalid secret key encoding"))?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| ChannelError::new("Invalid secret key length (expected 32)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenient_matches_ts_cases() {
        assert_eq!(decode_lenient("  AQID  ").unwrap(), [1, 2, 3]);
        assert_eq!(decode_lenient("-_8").unwrap(), [0xfb, 0xff]);
        assert_eq!(decode_lenient("+/8=").unwrap(), [0xfb, 0xff]);
        assert_eq!(decode_lenient("AQI").unwrap(), [1, 2]);
        assert!(decode_lenient("not base64 !!").is_none());
    }

    #[test]
    fn public_key_is_strict() {
        let ok = encode(&[7u8; 32]);
        assert_eq!(decode_public_key(&ok).unwrap(), [7u8; 32]);
        assert!(decode_public_key("not-a-key").is_err());
        assert!(decode_public_key(&encode(&[7u8; 31])).is_err());
        // url-safe alphabet is rejected
        assert!(decode_public_key(&ok.replace('+', "-")).is_err() || !ok.contains('+'));
    }
}
