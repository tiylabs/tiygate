//! Encryption utilities for API key storage (AES-256-GCM).
//! Phase 1: Placeholder — keys stored in env vars only.

/// Placeholder encryption module. Full AES-GCM encryption will be
/// implemented in Phase 4 (Productization).
pub struct KeyEncryption;

impl Default for KeyEncryption {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyEncryption {
    pub fn new() -> Self {
        Self
    }
}
