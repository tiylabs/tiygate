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
    extract::{ConnectInfo, Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use std::net::SocketAddr;
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
///
/// **Path-scoping.** This middleware is applied to the admin router
/// before it is `Router::merge`d into the data-plane router in
/// `tiygate_server::app::App::router`. Empirically (and per the
/// internal `ingress` ↔ `admin` trace log captured on 2026-06-13)
/// `Router::merge` in axum 0.7 propagates the inner router's
/// `Layer` to the merged router's *routing pass*, so the
/// `require_admin_token` middleware ends up being evaluated for
/// every request the merged router receives, not just the admin
/// routes. As a defense-in-depth measure we explicitly no-op for
/// any URI that does not begin with `/admin/` — i.e. the data
/// plane (`/v1/...`, `/v1beta/...`, `/v1/embeddings`, `/healthz`,
/// …) must never be bearer-gated by the admin token. The check is
/// applied **before** reading the Authorization header so an
/// unauthenticated data-plane request returns the data-plane
/// handler's response (or 404) and never leaks the
/// `admin_auth` error envelope.
pub async fn require_admin_token(
    State(state): State<AdminState>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if !is_admin_path(path) {
        // Not an admin route — let the data plane handle it (or
        // return 404 from the merged router's fallthrough). This
        // MUST happen before any auth-header inspection so we
        // never accidentally gate a non-admin route.
        return next.run(req).await;
    }

    // Public admin routes that do not require authentication.
    // `/admin/v1/info` exposes the server name + version so the
    // login page can display it before the user has a token.
    if is_public_admin_path(path) {
        return next.run(req).await;
    }

    // Brute-force protection: extract the client IP and check
    // whether it is currently locked out. A locked client gets 403
    // regardless of the token it presents — the lockout takes
    // priority over authentication so an attacker cannot probe
    // tokens during the lockout window.
    let client_id = extract_client_ip(&req);
    if state.bf_limiter.is_locked(&client_id).await {
        return error_response(
            StatusCode::FORBIDDEN,
            "too many failed attempts; try again later",
            "admin_auth",
        );
    }

    let header_value = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let token = match header_value {
        Some(v) if v.starts_with("Bearer ") => &v[7..],
        _ => {
            // Missing/malformed Authorization header counts as a
            // failed attempt.
            state.bf_limiter.record_failure(&client_id).await;
            return error_response(StatusCode::UNAUTHORIZED, "missing bearer token", "gateway");
        }
    };

    if explicit_token().is_none() {
        // Admin auth not configured — do not penalise the caller
        // with brute-force tracking (503 is a server-side config
        // issue, not an auth failure).
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "admin token not configured (set TIYGATE_ADMIN_TOKEN)",
            "gateway",
        );
    }

    if !verify_admin_token(token) {
        state.bf_limiter.record_failure(&client_id).await;
        return error_response(StatusCode::UNAUTHORIZED, "invalid admin token", "gateway");
    }

    // Successful authentication — reset the failure counters.
    state.bf_limiter.record_success(&client_id).await;
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

/// Extract the client IP from a request for brute-force tracking.
///
/// Priority:
/// 1. `X-Forwarded-For` — first IP (leftmost), the standard
///    header set by Caddy / nginx / cloud LBs.
/// 2. `X-Real-IP` — single-IP variant used by some proxies.
/// 3. `ConnectInfo<SocketAddr>` — the raw TCP peer address, only
///    available when the server was started with
///    `into_make_service_with_connect_info`.
/// 4. `"unknown"` — fallback so the limiter still provides basic
///    protection when no IP source is available.
fn extract_client_ip(req: &Request) -> String {
    if let Some(xff) = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(first) = xff.split(',').next() {
            let trimmed = first.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    if let Some(xrip) = req.headers().get("x-real-ip").and_then(|v| v.to_str().ok()) {
        let trimmed = xrip.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Some(addr) = req.extensions().get::<ConnectInfo<SocketAddr>>() {
        return addr.0.ip().to_string();
    }
    "unknown".to_string()
}

/// Returns `true` when `path` is an admin route that
/// `require_admin_token` should gate. The admin surface today lives
/// entirely under `/admin/...` (see
/// `crates/admin/src/handlers.rs::router` and the OAuth router).
///
/// We accept the exact prefix `/admin` and `/admin/...` so that
/// future top-level admin helpers (e.g. `/admin` as a redirect
/// index) can also be gated. Anything else — including the data
/// plane (`/v1/...`, `/v1beta/...`, `/v1/embeddings`, `/healthz`,
/// …) — is left to the data-plane handler chain. This is
/// defense-in-depth: the `Router::merge` call in
/// `tiygate_server::app::App::router` does not (reliably) scope a
/// layer to only the inner router's paths in axum 0.7.
fn is_admin_path(path: &str) -> bool {
    path == "/admin" || path.starts_with("/admin/")
}

/// Returns `true` for admin routes that are intentionally public
/// (no bearer token required). Currently only `/admin/v1/info`,
/// which the login page calls to display the server version before
/// the user has authenticated.
fn is_public_admin_path(path: &str) -> bool {
    path == "/admin/v1/info"
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn extract_ip_prefers_x_forwarded_for() {
        let builder = Request::builder().header("x-forwarded-for", "203.0.113.1, 10.0.0.1");
        let req = builder.body(axum::body::Body::empty()).unwrap();
        assert_eq!(extract_client_ip(&req), "203.0.113.1");
    }

    #[test]
    fn extract_ip_uses_x_real_ip_when_no_xff() {
        let builder = Request::builder().header("x-real-ip", "198.51.100.7");
        let req = builder.body(axum::body::Body::empty()).unwrap();
        assert_eq!(extract_client_ip(&req), "198.51.100.7");
    }

    #[test]
    fn extract_ip_uses_connect_info_fallback() {
        let addr: SocketAddr = "192.0.2.42:1234".parse().unwrap();
        let req = Request::builder()
            .extension(ConnectInfo(addr))
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(extract_client_ip(&req), "192.0.2.42");
    }

    #[test]
    fn extract_ip_returns_unknown_when_nothing_available() {
        let req = Request::builder().body(axum::body::Body::empty()).unwrap();
        assert_eq!(extract_client_ip(&req), "unknown");
    }

    #[test]
    fn extract_ip_ignores_empty_xff() {
        let builder = Request::builder().header("x-forwarded-for", "  , ");
        let req = builder.body(axum::body::Body::empty()).unwrap();
        assert_eq!(extract_client_ip(&req), "unknown");
    }

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

    #[test]
    fn is_admin_path_recognises_admin_routes() {
        // Positive cases — must be gated.
        assert!(is_admin_path("/admin"));
        assert!(is_admin_path("/admin/"));
        assert!(is_admin_path("/admin/v1/health"));
        assert!(is_admin_path("/admin/v1/providers"));
        assert!(is_admin_path("/admin/v1/api-keys"));
        assert!(is_admin_path("/admin/v1/api-keys/abc-123"));
        assert!(is_admin_path("/admin/v1/oauth/start"));
    }

    #[test]
    fn is_admin_path_leaves_data_plane_alone() {
        // Negative cases — must NOT be gated by require_admin_token.
        // Each of these used to leak through (regression captured
        // 2026-06-13: Gemini clients hitting
        // `/v1beta/models/...:generateContent` were getting the
        // `admin_auth` 401 envelope).
        assert!(!is_admin_path("/"));
        assert!(!is_admin_path("/v1/chat/completions"));
        assert!(!is_admin_path("/v1/messages"));
        assert!(!is_admin_path("/v1/embeddings"));
        assert!(!is_admin_path("/v1/responses"));
        assert!(!is_admin_path(
            "/v1beta/models/anthropic%2Fclaude-opus-4.8:generateContent"
        ));
        assert!(!is_admin_path("/healthz"));
        assert!(!is_admin_path("/readyz"));

        // Prefix-collision traps: a path that *contains* the
        // substring "/admin" but does not start with it must NOT
        // be gated.
        assert!(!is_admin_path("/v1/admin-tools"));
        assert!(!is_admin_path("/foo/admin"));
        assert!(!is_admin_path("/administrator"));
    }

    #[test]
    fn is_public_admin_path_only_matches_info() {
        assert!(is_public_admin_path("/admin/v1/info"));
        // Other admin routes are NOT public.
        assert!(!is_public_admin_path("/admin/v1/health"));
        assert!(!is_public_admin_path("/admin/v1/providers"));
        assert!(!is_public_admin_path("/admin/v1/settings"));
        assert!(!is_public_admin_path("/admin/v1/info/extra"));
    }
}
