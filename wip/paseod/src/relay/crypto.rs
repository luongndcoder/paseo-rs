//! NaCl box primitives, byte-compatible with `packages/relay/src/crypto.ts` (tweetnacl).
//!
//! Key agreement is X25519 followed by HSalsa20 (`nacl.box.before`), encryption is
//! XSalsa20-Poly1305. Bundle layout is `[nonce(24)][tag(16) ciphertext]`.

use crypto_box::aead::generic_array::GenericArray;
use crypto_box::aead::Aead;
use crypto_box::{PublicKey, SalsaBox, SecretKey};
use rand::RngCore;

use super::channel::ChannelError;

pub const NONCE_LEN: usize = 24;
/// Nonce + Poly1305 tag (`ENCRYPTED_PAYLOAD_OVERHEAD_BYTES`).
pub const OVERHEAD_BYTES: usize = 40;

/// Result of `deriveSharedKey`.
pub struct SharedKey {
    cipher: SalsaBox,
    /// Raw X25519 output; only used for constant-time equality (re-handshake check).
    dh: [u8; 32],
}

impl std::fmt::Debug for SharedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedKey(<redacted>)")
    }
}

impl SharedKey {
    /// Mirrors `deriveSharedKey`: rejects an all-zero X25519 result ("Invalid peer public key").
    pub fn derive(our_secret: &[u8; 32], peer_public: &[u8; 32]) -> Result<Self, ChannelError> {
        let dh = x25519_dalek::x25519(*our_secret, *peer_public);
        if dh.iter().fold(0u8, |acc, b| acc | b) == 0 {
            return Err(ChannelError::new("Invalid peer public key"));
        }
        let cipher = SalsaBox::new(
            &PublicKey::from(*peer_public),
            &SecretKey::from(*our_secret),
        );
        Ok(Self { cipher, dh })
    }

    /// Constant-time comparison (`keysEqual`).
    pub fn same_key(&self, other: &SharedKey) -> bool {
        self.dh
            .iter()
            .zip(other.dh.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }

    /// `encrypt` with a random nonce.
    pub fn encrypt(&self, plaintext: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        self.encrypt_with_nonce(&nonce, plaintext)
    }

    /// Deterministic variant for test vectors.
    pub fn encrypt_with_nonce(&self, nonce: &[u8; NONCE_LEN], plaintext: &[u8]) -> Vec<u8> {
        let ct = self
            .cipher
            .encrypt(GenericArray::from_slice(nonce), plaintext)
            .expect("xsalsa20poly1305 encryption cannot fail for in-memory buffers");
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(nonce);
        out.extend_from_slice(&ct);
        out
    }

    /// `decrypt`; error strings match TS because they become close reasons.
    pub fn decrypt(&self, bundle: &[u8]) -> Result<Vec<u8>, ChannelError> {
        if bundle.len() < NONCE_LEN {
            return Err(ChannelError::new("Ciphertext bundle too short"));
        }
        let (nonce, ct) = bundle.split_at(NONCE_LEN);
        self.cipher
            .decrypt(GenericArray::from_slice(nonce), ct)
            .map_err(|_| ChannelError::new("Decryption failed"))
    }
}
