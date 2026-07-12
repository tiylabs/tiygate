//! Higher-level wrappers around [`crate::encryption::KeyEncryption`].
//!
//! These are the functions the rest of the gateway uses to encrypt
//! and decrypt provider API keys. They are deliberately tiny so the
//! auth-mode branching lives in one place and so that switching
//! purposes (provider API key vs. OAuth refresh token) does not
//! require changes in any other file.

use crate::encryption::{EncryptionError, KeyEncryption};

/// Purpose label used to derive the subkey for a provider's static
/// API key. Distinct from the OAuth purpose so an operator can
/// rotate one without invalidating the other.
pub const PURPOSE_PROVIDER_API_KEY: &str = "provider-api-key";
/// Purpose label used to derive the subkey for OAuth refresh tokens.
pub const PURPOSE_OAUTH_REFRESH_TOKEN: &str = "oauth-refresh-token";
/// Purpose label used for short-lived OAuth access tokens cached in the
/// shared database. Keeping it distinct from refresh-token metadata prevents
/// ciphertext from being replayed across credential classes.
pub const PURPOSE_OAUTH_ACCESS_TOKEN: &str = "oauth-access-token";
/// Purpose label used to derive the subkey for sensitive settings
/// stored in the `settings` table (e.g. S3 access keys).
pub const PURPOSE_SETTINGS: &str = "settings";

/// Encrypt a provider API key. Returns the base64-encoded blob
/// suitable for storing in the `providers.encrypted_api_key` column.
pub fn encrypt_api_key(enc: &KeyEncryption, api_key: &str) -> Result<String, EncryptionError> {
    enc.encrypt(PURPOSE_PROVIDER_API_KEY, api_key)
}

/// Decrypt a provider API key.
pub fn decrypt_api_key(enc: &KeyEncryption, blob: &str) -> Result<String, EncryptionError> {
    enc.decrypt(PURPOSE_PROVIDER_API_KEY, blob)
}

/// Encrypt an OAuth refresh token JSON blob.
pub fn encrypt_oauth_meta(enc: &KeyEncryption, meta_json: &str) -> Result<String, EncryptionError> {
    enc.encrypt(PURPOSE_OAUTH_REFRESH_TOKEN, meta_json)
}

/// Decrypt an OAuth refresh token JSON blob.
pub fn decrypt_oauth_meta(enc: &KeyEncryption, blob: &str) -> Result<String, EncryptionError> {
    enc.decrypt(PURPOSE_OAUTH_REFRESH_TOKEN, blob)
}

/// Encrypt an OAuth access token for the shared token-state table.
pub fn encrypt_oauth_access_token(
    enc: &KeyEncryption,
    access_token: &str,
) -> Result<String, EncryptionError> {
    enc.encrypt(PURPOSE_OAUTH_ACCESS_TOKEN, access_token)
}

/// Decrypt an OAuth access token from the shared token-state table.
pub fn decrypt_oauth_access_token(
    enc: &KeyEncryption,
    blob: &str,
) -> Result<String, EncryptionError> {
    enc.decrypt(PURPOSE_OAUTH_ACCESS_TOKEN, blob)
}

/// Encrypt a sensitive setting value (e.g. an S3 access key).
pub fn encrypt_settings(enc: &KeyEncryption, value: &str) -> Result<String, EncryptionError> {
    enc.encrypt(PURPOSE_SETTINGS, value)
}

/// Decrypt a sensitive setting value.
pub fn decrypt_settings(enc: &KeyEncryption, blob: &str) -> Result<String, EncryptionError> {
    enc.decrypt(PURPOSE_SETTINGS, blob)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn enc() -> KeyEncryption {
        let mut b = [0u8; 32];
        for (i, slot) in b.iter_mut().enumerate() {
            *slot = (i as u8).wrapping_add(7);
        }
        KeyEncryption::from_bytes(b)
    }

    #[test]
    fn api_key_round_trip() {
        let e = enc();
        let ct = encrypt_api_key(&e, "sk-test").unwrap();
        let pt = decrypt_api_key(&e, &ct).unwrap();
        assert_eq!(pt, "sk-test");
    }

    #[test]
    fn oauth_meta_round_trip() {
        let e = enc();
        let meta = r#"{"refresh_token":"abc","expires_in":3600}"#;
        let ct = encrypt_oauth_meta(&e, meta).unwrap();
        let pt = decrypt_oauth_meta(&e, &ct).unwrap();
        assert_eq!(pt, meta);
    }

    #[test]
    fn purposes_are_isolated() {
        let e = enc();
        let api = encrypt_api_key(&e, "sk-test").unwrap();
        // OAuth key derived from the same plaintext must not decrypt
        // the API-key blob.
        let err = decrypt_oauth_meta(&e, &api);
        assert!(err.is_err());

        let access = encrypt_oauth_access_token(&e, "access-token").unwrap();
        assert_eq!(
            decrypt_oauth_access_token(&e, &access).unwrap(),
            "access-token"
        );
        assert!(decrypt_oauth_meta(&e, &access).is_err());
    }
}
