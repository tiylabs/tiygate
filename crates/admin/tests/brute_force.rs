//! Integration tests for the admin brute-force protection.
//!
//! These tests exercise the in-memory limiter through the admin
//! middleware, verifying the lockout state machine end-to-end:
//! 3 failures → 403 lockout, success resets counters, and locked
//! clients get 403 even with a valid token.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use tower::ServiceExt;

use tiygate_admin::{build_router, AdminState, BruteForceConfig, InMemoryBruteForceLimiter};
use tiygate_store::config_store::DbConfigStore;
use tiygate_store::db;
use tiygate_store::encryption::KeyEncryption;

/// Boot a router with auth enabled and a fast brute-force config
/// (3 failures, 1s lockout) so tests do not block on real timeouts.
async fn boot_with_fast_bf() -> axum::Router {
    let pool = Arc::new(db::open_pool("sqlite::memory:").await.expect("pool"));
    db::run_migrations(&pool).await.expect("migrate");
    let key_bytes: [u8; 32] = std::iter::repeat_n(0xabu8, 32)
        .collect::<Vec<u8>>()
        .try_into()
        .unwrap();
    let encryption = Some(Arc::new(KeyEncryption::from_bytes(key_bytes)));
    let store = Arc::new(DbConfigStore::new((*pool).clone(), encryption));
    store.refresh().await.expect("refresh");
    let fast_config = BruteForceConfig {
        max_failures: 3,
        lockout_secs: 1,
        max_lockouts: 3,
        escalated_lockout_secs: 2,
    };
    let state = AdminState::new(store, pool, None)
        .with_bf_limiter(Arc::new(InMemoryBruteForceLimiter::new(fast_config)));
    build_router(state)
}

fn admin_request(auth: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri("/admin/v1/health");
    if let Some(token) = auth {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::empty()).expect("request")
}

#[tokio::test]
#[serial_test::serial]
async fn bf_three_failures_then_locked() {
    tiygate_admin::auth::set_test_admin_token_for_current_thread(Some("secret"));
    let router = boot_with_fast_bf().await;

    // First two wrong-token attempts → 401.
    for i in 0..2 {
        let resp = router
            .clone()
            .oneshot(admin_request(Some("wrong")))
            .await
            .expect("response");
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "attempt {i} should be 401"
        );
    }

    // Third wrong-token attempt → still 401 (the failure that
    // triggers the lockout is itself a 401, not a 403; the lockout
    // applies to *subsequent* requests).
    let resp = router
        .clone()
        .oneshot(admin_request(Some("wrong")))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // Fourth attempt — even with the correct token — → 403 because
    // the IP is now locked out.
    let resp = router
        .clone()
        .oneshot(admin_request(Some("secret")))
        .await
        .expect("response");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "locked client must get 403 even with valid token"
    );

    tiygate_admin::auth::set_test_admin_token_for_current_thread(None);
}

#[tokio::test]
#[serial_test::serial]
async fn bf_correct_token_resets_failure_count() {
    tiygate_admin::auth::set_test_admin_token_for_current_thread(Some("secret"));
    let router = boot_with_fast_bf().await;

    // Two failures (below the threshold).
    for _ in 0..2 {
        let _ = router
            .clone()
            .oneshot(admin_request(Some("wrong")))
            .await
            .expect("response");
    }

    // A successful auth resets the counter.
    let resp = router
        .clone()
        .oneshot(admin_request(Some("secret")))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    // Now two more failures should NOT lock (counter was reset).
    for _ in 0..2 {
        let resp = router
            .clone()
            .oneshot(admin_request(Some("wrong")))
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
    // The next valid request should still succeed (not locked).
    let resp = router
        .clone()
        .oneshot(admin_request(Some("secret")))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    tiygate_admin::auth::set_test_admin_token_for_current_thread(None);
}

#[tokio::test]
#[serial_test::serial]
async fn bf_missing_token_counts_as_failure() {
    tiygate_admin::auth::set_test_admin_token_for_current_thread(Some("secret"));
    let router = boot_with_fast_bf().await;

    // Three requests with no Authorization header → 401 each, and
    // the third triggers the lockout.
    for i in 0..3 {
        let resp = router
            .clone()
            .oneshot(admin_request(None))
            .await
            .expect("response");
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "missing-token attempt {i} should be 401"
        );
    }

    // Fourth attempt is locked → 403.
    let resp = router
        .clone()
        .oneshot(admin_request(Some("secret")))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    tiygate_admin::auth::set_test_admin_token_for_current_thread(None);
}

#[tokio::test]
#[serial_test::serial]
async fn bf_lockout_expires_and_allows_retry() {
    tiygate_admin::auth::set_test_admin_token_for_current_thread(Some("secret"));
    let router = boot_with_fast_bf().await;

    // Trigger lockout with 3 failures.
    for _ in 0..3 {
        let _ = router
            .clone()
            .oneshot(admin_request(Some("wrong")))
            .await
            .expect("response");
    }
    // Confirmed locked.
    let resp = router
        .clone()
        .oneshot(admin_request(Some("secret")))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Wait for the 1s lockout to expire.
    tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;

    // After expiry, a valid token works again.
    let resp = router
        .clone()
        .oneshot(admin_request(Some("secret")))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    tiygate_admin::auth::set_test_admin_token_for_current_thread(None);
}

#[tokio::test]
#[serial_test::serial]
async fn bf_503_not_configured_does_not_count_as_failure() {
    // When admin auth is not configured (no token set), requests
    // get 503 and must NOT count toward brute-force lockout.
    tiygate_admin::auth::set_test_admin_token_for_current_thread(None);
    let router = boot_with_fast_bf().await;

    for _ in 0..5 {
        let resp = router
            .clone()
            .oneshot(admin_request(Some("anything")))
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // Even after 5 "attempts" the limiter should not have locked
    // anyone. Verify by configuring a token and authenticating.
    tiygate_admin::auth::set_test_admin_token_for_current_thread(Some("secret"));
    let resp = router
        .clone()
        .oneshot(admin_request(Some("secret")))
        .await
        .expect("response");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "503 path must not trigger brute-force lockout"
    );

    tiygate_admin::auth::set_test_admin_token_for_current_thread(None);
}
