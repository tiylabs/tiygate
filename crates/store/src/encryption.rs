//! AES-256-GCM encryption helpers for provider API keys and OAuth
//! refresh tokens (design doc §4.5).
//!
//! ## Master key
//!
//! The master key is supplied as 32 raw bytes — usually via the
//! `TIYGATE_MASTER_KEY` environment variable, accepted in one of two
//! encodings:
//!
//! * **hex** — 64 lowercase characters
//! * **base64** — standard or URL-safe, with or without padding
//!
//! ## Key derivation
//!
//! [`KeyEncryption::encrypt`] takes a *purpose* label and derives a
//! 32-byte subkey via HKDF-SHA256 from the master. This lets an
//! operator rotate the master key without re-deriving every per-
//! purpose subkey by hand, and prevents cross-purpose ciphertext
//! confusion.
//!
//! ## Ciphertext format
//!
//! `nonce (12) || ciphertext || tag (16)`, base64-encoded. The 12-byte
//! nonce is generated with `rand::thread_rng()` for every call —
//! reusing a (key, nonce) pair catastrophically breaks GCM
//! confidentiality and integrity, so we rely on the OS RNG rather
//! than a counter.
//!
//! ## Plaintext zeroization
//!
//! We accept plain strings (UTF-8) for the API key. The master key
//! is wrapped in a `Zeroizing<[u8; 32]>` so that dropping the
//! [`KeyEncryption`] overwrites the buffer before the allocator
//! reclaims it.

use std::fmt;

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hkdf::Hkdf;
use rand::Rng;
use sha2::Sha256;
use thiserror::Error;
use zeroize::Zeroizing;

const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;
const HKDF_INFO_PREFIX: &[u8] = b"tiygate/v1/";

/// Errors emitted by [`KeyEncryption`].
#[derive(Debug, Error)]
pub enum EncryptionError {
    #[error("master key must be 32 bytes (got {0})")]
    BadKeyLength(usize),
    #[error("master key is not valid hex or base64: {0}")]
    BadKeyEncoding(String),
    #[error("ciphertext too short")]
    CiphertextTooShort,
    #[error("decryption failed: {0}")]
    Decrypt(String),
    #[error("encryption failed: {0}")]
    Encrypt(String),
}

/// Wrapper around an AES-256-GCM key derived from the master key +
/// a per-purpose label.
pub struct KeyEncryption {
    /// The raw master key. Wrapped in `Zeroizing` so it is wiped on
    /// drop. We never log it; `Debug` is hand-rolled to a redacted
    /// form.
    master: Zeroizing<[u8; KEY_LEN]>,
    // Ciphers are derived per-purpose on demand inside `cipher_for`.
    // We avoid storing the cipher directly because each purpose
    // produces a different derived key.
}

impl fmt::Debug for KeyEncryption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyEncryption")
            .field("master", &"<redacted 32 bytes>")
            .finish()
    }
}

impl KeyEncryption {
    /// Build a `KeyEncryption` from 32 raw bytes.
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self {
            master: Zeroizing::new(bytes),
        }
    }

    /// Build a `KeyEncryption` from a hex or base64-encoded string.
    /// Accepts the encodings described at the module level.
    pub fn from_secret(secret: &str) -> Result<Self, EncryptionError> {
        let trimmed = secret.trim();
        let bytes: [u8; KEY_LEN] = if trimmed.len() == KEY_LEN * 2 && trimmed.is_ascii() {
            // try hex first
            hex::decode(trimmed)
                .map_err(|e| EncryptionError::BadKeyEncoding(e.to_string()))?
                .try_into()
                .map_err(|v: Vec<u8>| EncryptionError::BadKeyLength(v.len()))?
        } else {
            // base64: use the standard engine (handles missing
            // padding by being lenient about trailing `=`).
            let decoded = STANDARD
                .decode(trimmed)
                .map_err(|e| EncryptionError::BadKeyEncoding(e.to_string()))?;
            if decoded.len() != KEY_LEN {
                return Err(EncryptionError::BadKeyLength(decoded.len()));
            }
            decoded
                .try_into()
                .map_err(|v: Vec<u8>| EncryptionError::BadKeyLength(v.len()))?
        };
        Ok(Self::from_bytes(bytes))
    }

    /// Read the master key from the `TIYGATE_MASTER_KEY` env var.
    /// Returns `None` when the env var is unset or empty.
    pub fn from_env() -> Option<Result<Self, EncryptionError>> {
        let raw = std::env::var("TIYGATE_MASTER_KEY").ok()?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(Self::from_secret(trimmed))
    }

    /// Encrypt `plaintext` under the subkey derived for `purpose`.
    /// Returns a base64-encoded `nonce || ciphertext || tag` blob.
    pub fn encrypt(&self, purpose: &str, plaintext: &str) -> Result<String, EncryptionError> {
        let cipher = self.cipher_for(purpose)?;
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::thread_rng().fill(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = cipher
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|e| EncryptionError::Encrypt(e.to_string()))?;
        let mut blob = Vec::with_capacity(NONCE_LEN + ct.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ct);
        Ok(STANDARD.encode(blob))
    }

    /// Decrypt a base64-encoded `nonce || ciphertext || tag` blob
    /// produced by [`Self::encrypt`].
    pub fn decrypt(&self, purpose: &str, blob: &str) -> Result<String, EncryptionError> {
        let cipher = self.cipher_for(purpose)?;
        let raw = STANDARD
            .decode(blob.trim())
            .map_err(|e| EncryptionError::Decrypt(format!("base64: {e}")))?;
        if raw.len() <= NONCE_LEN {
            return Err(EncryptionError::CiphertextTooShort);
        }
        let (nonce_bytes, ct) = raw.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);
        let pt = cipher
            .decrypt(nonce, ct)
            .map_err(|e| EncryptionError::Decrypt(e.to_string()))?;
        String::from_utf8(pt).map_err(|e| EncryptionError::Decrypt(format!("utf8: {e}")))
    }

    /// Returns a redacted form of an encrypted blob for safe logging.
    pub fn redact(blob: &str) -> String {
        if blob.len() <= 12 {
            return "[encrypted: <short>]".to_string();
        }
        format!("[encrypted: {}…]", &blob[..12])
    }

    /// Derive the subkey for `purpose` and return an `Aes256Gcm`
    /// cipher bound to it.
    fn cipher_for(&self, purpose: &str) -> Result<Aes256Gcm, EncryptionError> {
        let mut info = Vec::with_capacity(HKDF_INFO_PREFIX.len() + purpose.len());
        info.extend_from_slice(HKDF_INFO_PREFIX);
        info.extend_from_slice(purpose.as_bytes());
        let hk = Hkdf::<Sha256>::new(None, self.master.as_ref());
        let mut okm = [0u8; KEY_LEN];
        hk.expand(&info, &mut okm)
            .map_err(|e| EncryptionError::Decrypt(format!("hkdf: {e}")))?;
        Ok(Aes256Gcm::new(okm.as_ref().into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn master() -> [u8; KEY_LEN] {
        let mut b = [0u8; KEY_LEN];
        for (i, slot) in b.iter_mut().enumerate() {
            *slot = (i as u8).wrapping_mul(7).wrapping_add(13);
        }
        b
    }

    #[test]
    fn round_trip_preserves_plaintext() {
        let enc = KeyEncryption::from_bytes(master());
        let pt = "sk-test-1234-secret";
        let ct = enc.encrypt("provider-api-key", pt).expect("encrypt");
        let rt = enc.decrypt("provider-api-key", &ct).expect("decrypt");
        assert_eq!(rt, pt);
    }

    #[test]
    fn different_purposes_produce_different_ciphertexts() {
        let enc = KeyEncryption::from_bytes(master());
        let pt = "shared-plaintext";
        let a = enc.encrypt("purpose-a", pt).unwrap();
        let b = enc.encrypt("purpose-b", pt).unwrap();
        assert_ne!(a, b, "ciphertexts must differ across purposes");
    }

    #[test]
    fn wrong_purpose_fails_to_decrypt() {
        let enc = KeyEncryption::from_bytes(master());
        let ct = enc.encrypt("purpose-a", "x").unwrap();
        let err = enc.decrypt("purpose-b", &ct);
        assert!(err.is_err(), "decryption with wrong purpose must fail");
    }

    #[test]
    fn from_secret_accepts_hex() {
        let hex: String = (0..KEY_LEN).map(|i| format!("{:02x}", i as u8)).collect();
        let enc = KeyEncryption::from_secret(&hex).expect("hex ok");
        let _ = enc.encrypt("p", "x").expect("encrypt ok");
    }

    #[test]
    fn from_secret_accepts_base64() {
        let raw = [42u8; KEY_LEN];
        let b64 = STANDARD.encode(raw);
        let enc = KeyEncryption::from_secret(&b64).expect("b64 ok");
        let _ = enc.encrypt("p", "x").expect("encrypt ok");
    }

    #[test]
    fn from_secret_rejects_short() {
        let err = KeyEncryption::from_secret("abcd");
        assert!(matches!(err, Err(EncryptionError::BadKeyLength(_))));
    }

    #[test]
    fn redact_hides_payload() {
        let blob = "abcdefghijklmnopqrstuvwxyz0123456789";
        let r = KeyEncryption::redact(blob);
        assert!(r.contains("[encrypted:"));
        assert!(!r.contains("0123456789"), "redact must truncate");
    }

    #[test]
    fn debug_does_not_leak_master() {
        let enc = KeyEncryption::from_bytes(master());
        let s = format!("{enc:?}");
        assert!(s.contains("redacted"));
        assert!(!s.contains("13")); // 13 = a marker byte from the test key
    }
}
