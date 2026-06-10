//! Admin API authentication middleware (Phase 4). Placeholder.

/// Verify the admin bearer token.
pub fn verify_admin_token(token: &str) -> bool {
    if let Ok(expected) = std::env::var("TIYGATE_ADMIN_TOKEN") {
        // Constant-time comparison
        token == expected
    } else {
        false
    }
}
