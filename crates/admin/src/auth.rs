//! Bearer-token authentication middleware for the Admin API.
//!
//! Phase 4 (产品化) keeps the auth surface deliberately small: a
//! single bearer token supplied via the `Authorization: Bearer …`
//! header, compared in constant time to `TIYGATE_ADMIN_TOKEN`. The
//! intent is "single operator" / "small team" deployments, where
//! any holder of the admin token is fully trusted. RBAC, audit
//! actors, and per-key scoping are explicitly out of scope per the
//! design doc §4.5.
//!
//! The middleware also accepts a per-thread override via
//! [`set_test_admin_token_for_current_thread`]. This is the
//! integration test escape hatch: it avoids the env-var race that
//! `cargo test` exposes when multiple tests set the same env
//! variable concurrently. Production code never sets it; the env
//! var is the source of truth.

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use subtle::ConstantTimeEq;

use crate::state::AdminState;

/// Verify a bearer token against the configured admin token.
/// Returns `false` if neither the env var nor a thread-local
/// override is set.
pub fn verify_admin_token(token: &str) -> bool {
    let Some(expected) = explicit_token() else {
        return false;
    };
    // Constant-time comparison: both sides must be the same length,
    // otherwise the `subtle::Choice::ct_eq` returns 0 without
    // leaking length information.
    if token.len() != expected.len() {
        return false;
    }
    token.as_bytes().ct_eq(expected.as_bytes()).into()
}

fn explicit_token() -> Option<String> {
    thread_local_admin_token().or_else(read_admin_token_from_env)
}

fn read_admin_token_from_env() -> Option<String> {
    std::env::var("TIYGATE_ADMIN_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
}

thread_local! {
    /// Per-thread override used by integration tests to avoid the
    /// env-var race that `cargo test` exposes. Production code
    /// never sets this.
    static TEST_ADMIN_TOKEN: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

fn thread_local_admin_token() -> Option<String> {
    TEST_ADMIN_TOKEN.with(|t| t.borrow().clone())
}

/// Set the per-thread admin token override (test-only).
#[doc(hidden)]
pub fn set_test_admin_token_for_current_thread(token: Option<&str>) {
    TEST_ADMIN_TOKEN.with(|t| {
        *t.borrow_mut() = token.map(str::to_string);
    });
}

/// Middleware that requires a valid admin bearer token. Returns
/// `401 Unauthorized` when the token is missing or invalid, and
/// `503 Service Unavailable` when admin auth has not been
/// configured at all.
pub async fn require_admin_token(
    State(_state): State<AdminState>,
    req: Request,
    next: Next,
) -> Response {
    let header_value = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let token = match header_value {
        Some(v) if v.starts_with("Bearer ") => &v[7..],
        _ => {
            return error_response(StatusCode::UNAUTHORIZED, "missing bearer token", "gateway");
        }
    };

    if explicit_token().is_none() {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "admin token not configured (set TIYGATE_ADMIN_TOKEN)",
            "gateway",
        );
    }

    if !verify_admin_token(token) {
        return error_response(StatusCode::UNAUTHORIZED, "invalid admin token", "gateway");
    }

    next.run(req).await
}

fn error_response(status: StatusCode, message: &str, source: &str) -> Response {
    let body = Json(serde_json::json!({
        "error": {
            "message": message,
            "type": "admin_auth",
            "source": source,
        }
    }));
    (status, body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_returns_false_when_unconfigured() {
        set_test_admin_token_for_current_thread(None);
        std::env::remove_var("TIYGATE_ADMIN_TOKEN");
        assert!(!verify_admin_token("anything"));
    }

    #[test]
    fn verify_returns_true_with_matching_token() {
        set_test_admin_token_for_current_thread(Some("topsecret"));
        assert!(verify_admin_token("topsecret"));
        set_test_admin_token_for_current_thread(None);
    }

    #[test]
    fn verify_returns_false_with_mismatched_token() {
        set_test_admin_token_for_current_thread(Some("topsecret"));
        assert!(!verify_admin_token("wrong"));
        assert!(!verify_admin_token("topsecre"));
        assert!(!verify_admin_token("topsecret1"));
        set_test_admin_token_for_current_thread(None);
    }
}
