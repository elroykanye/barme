//! Envelope encryption for secrets at rest.
//!
//! Secret keys can't be hashed: the S3 door verifies AWS SigV4, which is a
//! symmetric HMAC, so the server must recover the raw secret to check a
//! signature. Instead they're encrypted on disk with a master key (the KEK) and
//! decrypted into memory only when needed. This keeps the key store free of
//! plaintext secrets while preserving S3 compatibility — the same tradeoff AWS
//! and MinIO make.
//!
//! AES-256-GCM with a fresh random 96-bit nonce per encryption. The stored form
//! is `hex(nonce || ciphertext||tag)`. GCM authenticates on decrypt, so a wrong
//! master key or a tampered ciphertext fails loudly rather than yielding garbage.

use crate::{Result, StoreError};
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};

const NONCE_LEN: usize = 12; // 96-bit nonce, the GCM standard

/// A master key wrapped as an AES-256-GCM cipher. Cheap to clone conceptually,
/// but held once in the key store. `Send + Sync` so the store can be shared.
pub struct Cipher(Aes256Gcm);

impl Cipher {
    /// Build from a 32-byte master key.
    pub fn new(key: &[u8; 32]) -> Self {
        Cipher(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)))
    }

    /// Encrypt a secret, returning `hex(nonce || ciphertext)`.
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        getrandom::getrandom(&mut nonce_bytes)
            .map_err(|e| StoreError::Crypto(format!("nonce: {e}")))?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .0
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|_| StoreError::Crypto("encrypt failed".into()))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(hex::encode(out))
    }

    /// Decrypt what `encrypt` produced. Fails on a wrong master key, a tampered
    /// ciphertext, or malformed input — all reported vaguely on purpose.
    pub fn decrypt(&self, encoded: &str) -> Result<String> {
        let raw = hex::decode(encoded).map_err(|_| StoreError::Crypto("bad encoding".into()))?;
        if raw.len() <= NONCE_LEN {
            return Err(StoreError::Crypto("ciphertext too short".into()));
        }
        let (nonce_bytes, ciphertext) = raw.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);
        let plaintext = self
            .0
            .decrypt(nonce, ciphertext)
            .map_err(|_| StoreError::Crypto("decrypt failed (wrong master key?)".into()))?;
        String::from_utf8(plaintext).map_err(|_| StoreError::Crypto("secret not utf-8".into()))
    }
}

impl std::fmt::Debug for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key material.
        f.write_str("Cipher(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let c = Cipher::new(&[7u8; 32]);
        let enc = c.encrypt("super-secret-key").unwrap();
        assert_ne!(enc, "super-secret-key");
        assert!(!enc.contains("secret")); // no plaintext leaks into the encoding
        assert_eq!(c.decrypt(&enc).unwrap(), "super-secret-key");
    }

    #[test]
    fn fresh_nonce_each_time() {
        let c = Cipher::new(&[1u8; 32]);
        // Same plaintext, different ciphertext — proves the nonce isn't reused.
        assert_ne!(c.encrypt("x").unwrap(), c.encrypt("x").unwrap());
    }

    #[test]
    fn wrong_key_fails_to_decrypt() {
        let enc = Cipher::new(&[1u8; 32]).encrypt("secret").unwrap();
        assert!(matches!(
            Cipher::new(&[2u8; 32]).decrypt(&enc),
            Err(StoreError::Crypto(_))
        ));
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let c = Cipher::new(&[3u8; 32]);
        let mut enc = c.encrypt("secret").unwrap();
        // Flip the last hex nibble; GCM's tag must catch it.
        let last = enc.pop().unwrap();
        enc.push(if last == 'a' { 'b' } else { 'a' });
        assert!(c.decrypt(&enc).is_err());
    }
}
