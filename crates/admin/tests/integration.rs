//! End-to-end integration tests for the Admin API surface.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::json;
use tiygate_store::config_store::DbConfigStore;
use tiygate_store::db;
use tiygate_store::encryption::KeyEncryption;
use tiygate_store::models::AuthMode;
use tower::ServiceExt;

use tiygate_admin::{build_router, AdminState};
use tiygate_core::EventSink;

async fn boot_no_auth() -> (
    axum::Router,
    Arc<DbConfigStore>,
    Arc<tiygate_store::db::DbPool>,
) {
    let pool = Arc::new(db::open_pool("sqlite::memory:").await.expect("pool"));
    db::run_migrations(pool.sqlite()).await.expect("migrate");
    let key_bytes: [u8; 32] = std::iter::repeat(0xabu8)
        .take(32)
        .collect::<Vec<u8>>()
        .try_into()
        .unwrap();
    let encryption = Some(Arc::new(KeyEncryption::from_bytes(key_bytes)));
    let store = Arc::new(DbConfigStore::new((*pool).clone(), encryption));
    store.refresh().await.expect("refresh");
    let state = AdminState::new(store.clone(), pool.clone(), None);
    let router = tiygate_admin::build_router_with_auth(state, false);
    (router, store, pool)
}

async fn boot_with_auth() -> (
    axum::Router,
    Arc<DbConfigStore>,
    Arc<tiygate_store::db::DbPool>,
) {
    let (_router, store, pool) = boot_no_auth().await;
    let state = AdminState::new(store.clone(), pool.clone(), None);
    let router = build_router(state);
    (router, store, pool)
}

fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

// ---- Acceptance #1: Admin CRUD + propagation to routing table ----

#[tokio::test]
async fn acceptance_1_admin_crud_propagates_to_routing_table() {
    let (router, store, _pool) = boot_no_auth().await;

    // Create a provider.
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/providers",
            json!({
                "id": "openai",
                "name": "OpenAI",
                "vendor": "openai",
                "api_base": "https://api.openai.com/v1",
                "api_key": "sk-test",
                "auth_mode": "api_key",
            }),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Create a route that uses the provider.
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/routes",
            json!({
                "virtual_model": "gpt-4o",
                "targets": [{
                    "provider_id": "openai",
                    "model_id": "gpt-4o",
                    "weight": 1.0
                }]
            }),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The data-plane ConfigStore must see the new route.
    let cs = store.config_store();
    assert!(cs.routing_table.routes.contains_key("gpt-4o"));
    let targets = &cs.routing_table.routes["gpt-4o"];
    assert_eq!(targets[0].provider_id, "openai");
    // The api key on the routing-table target must be the cleartext
    // we supplied at admin-time, so the data plane can forward
    // it to the upstream call. (No master key is configured in
    // this test — `cleartext-fallback` mode is exercised.)
    assert_eq!(targets[0].api_key, "sk-test");
}

#[tokio::test]
async fn acceptance_1_get_provider_returns_redacted_secrets() {
    let (router, _store, _pool) = boot_no_auth().await;
    let _ = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/providers",
            json!({
                "id": "p1",
                "name": "p1",
                "vendor": "openai",
                "api_base": "https://x",
                "api_key": "super-secret-key",
            }),
        ))
        .await
        .expect("post");
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/providers/p1")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("get");
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    let redacted = body["encrypted_api_key"].as_str().unwrap();
    assert!(redacted.starts_with("[encrypted:"));
    // The cleartext secret must NOT be present in the response.
    assert!(!body.to_string().contains("super-secret-key"));
}

#[tokio::test]
async fn acceptance_1_provider_secret_is_encrypted_at_rest() {
    let (router, _store, pool) = boot_no_auth().await;
    let _ = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/providers",
            json!({
                "id": "p1",
                "name": "p1",
                "vendor": "openai",
                "api_base": "https://x",
                "api_key": "sk-cleartext-must-not-appear",
            }),
        ))
        .await
        .expect("post");
    let direct: (String,) =
        sqlx::query_as("SELECT encrypted_api_key FROM providers WHERE id = 'p1'")
            .fetch_one(pool.sqlite())
            .await
            .expect("direct query");
    assert!(
        !direct.0.contains("sk-cleartext-must-not-appear"),
        "encrypted column leaked the cleartext: {}",
        direct.0
    );
    assert!(direct.0.len() > 20);
}

#[tokio::test]
async fn acceptance_1_with_master_key_decrypts_into_routing_table() {
    // Boot a DbConfigStore with a real AES-256-GCM master key.
    let pool = Arc::new(db::open_pool("sqlite::memory:").await.expect("pool"));
    db::run_migrations(pool.sqlite()).await.expect("migrate");
    let key_bytes: [u8; 32] = std::iter::repeat(0x42u8)
        .take(32)
        .collect::<Vec<u8>>()
        .try_into()
        .unwrap();
    let encryption = Some(Arc::new(KeyEncryption::from_bytes(key_bytes)));
    let store = Arc::new(DbConfigStore::new((*pool).clone(), encryption));
    store.refresh().await.expect("initial refresh");

    // Create a provider with an api key. The DbConfigStore
    // encrypts it at write time and decrypts it on every refresh.
    store
        .upsert_provider(
            "openai",
            "OpenAI",
            "openai",
            "https://api.openai.com/v1",
            Some("sk-decrypted-cleartext"),
            AuthMode::ApiKey,
            None,
            serde_json::json!({}),
            true,
        )
        .await
        .expect("upsert provider");

    // The data-plane ConfigStore must carry the cleartext.
    let cs = store.config_store();
    let snapshot = cs
        .snapshot()
        .expect("snapshot must be present after refresh");
    let p = snapshot
        .providers
        .get("openai")
        .expect("openai provider must be in the snapshot");
    assert_eq!(
        p.api_key_cleartext.as_deref(),
        Some("sk-decrypted-cleartext"),
        "the cleartext must be populated on refresh when a master key is configured"
    );
}

// ---- Acceptance #2: migrations run + status reports ----

#[tokio::test]
async fn acceptance_2_migrate_creates_tables_and_status_reports() {
    let (_router, _store, pool) = boot_no_auth().await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _migrations")
        .fetch_one(pool.sqlite())
        .await
        .expect("count");
    assert!(
        count >= 2,
        "expected both config + log migrations, got {count}"
    );
    // Both sequences should be recorded.
    let seqs: Vec<String> = sqlx::query_scalar("SELECT sequence FROM _migrations")
        .fetch_all(pool.sqlite())
        .await
        .expect("seqs");
    assert!(seqs.contains(&"config".to_string()));
    assert!(seqs.contains(&"log".to_string()));
}

// ---- Acceptance #3: OltpSink + aggregate query ----

#[tokio::test]
async fn acceptance_3_stats_aggregate_uses_oltp_log_table() {
    use tiygate_core::telemetry::{LatencyBreakdown, RequestEvent};
    use tiygate_core::Usage;
    let (_router, _store, pool) = boot_no_auth().await;
    let sink = tiygate_store::log_sink::oltp::OltpSink::new(pool.clone());
    let make = |id: &str, model: &str, status: &str| RequestEvent {
        request_id: id.to_string(),
        timestamp: chrono::Utc::now(),
        virtual_model: model.to_string(),
        resolved_provider: Some("openai".to_string()),
        resolved_model: Some(model.to_string()),
        account_label: None,
        trace_id: None,
        span_id: None,
        traceparent: None,
        ingress_protocol: "openai/chat-completions/v1".to_string(),
        egress_protocol: Some("openai/chat-completions/v1".to_string()),
        lossy: false,
        cache_hit: None,
        status: status.to_string(),
        error_class: None,
        http_status: Some(200),
        error_source: None,
        latency_ms: LatencyBreakdown {
            total_ms: 1,
            upstream_ms: 1,
            queue_ms: 0,
        },
        ttfb_ms: None,
        tokens: Some(Usage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
            ..Default::default()
        }),
        cost: None,
        api_key_id: None,
        client_ip: None,
        user_agent: None,
        raw_envelope: None,
    };
    sink.write_request_event(&make("r1", "gpt-4o", "ok"))
        .await
        .expect("write1");
    sink.write_request_event(&make("r2", "gpt-4o-mini", "ok"))
        .await
        .expect("write2");

    let now = chrono::Utc::now().to_rfc3339();
    let earlier = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let by_model = tiygate_store::log_sink::oltp::aggregate_by_model(pool.as_ref(), &earlier, &now)
        .await
        .expect("agg");
    let names: Vec<&str> = by_model.iter().map(|b| b.bucket.as_str()).collect();
    assert!(names.contains(&"gpt-4o"));
    assert!(names.contains(&"gpt-4o-mini"));
}

#[tokio::test]
async fn acceptance_3_stats_by_provider_endpoint() {
    use tiygate_core::telemetry::{LatencyBreakdown, RequestEvent};
    let (router, _store, pool) = boot_no_auth().await;
    let sink = tiygate_store::log_sink::oltp::OltpSink::new(pool.clone());
    let ev = RequestEvent {
        request_id: "r-x".to_string(),
        timestamp: chrono::Utc::now(),
        virtual_model: "gpt-4o".to_string(),
        resolved_provider: Some("openai".to_string()),
        resolved_model: Some("gpt-4o".to_string()),
        account_label: None,
        trace_id: None,
        span_id: None,
        traceparent: None,
        ingress_protocol: "openai/chat-completions/v1".to_string(),
        egress_protocol: Some("openai/chat-completions/v1".to_string()),
        lossy: false,
        cache_hit: None,
        status: "ok".to_string(),
        error_class: None,
        http_status: Some(200),
        error_source: None,
        latency_ms: LatencyBreakdown::default(),
        ttfb_ms: None,
        tokens: None,
        cost: None,
        api_key_id: Some("k1".to_string()),
        client_ip: None,
        user_agent: None,
        raw_envelope: None,
    };
    sink.write_request_event(&ev).await.expect("write");
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/stats/by-provider")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let buckets = body["buckets"].as_array().unwrap();
    assert!(!buckets.is_empty());
    assert_eq!(buckets[0]["bucket"], "openai");
}

// ---- Acceptance #4: config and log are separate tables ----

#[tokio::test]
async fn acceptance_4_config_and_log_live_in_separate_tables() {
    let (_router, _store, pool) = boot_no_auth().await;
    // Both tables must exist.
    let has_providers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='providers'",
    )
    .fetch_one(pool.sqlite())
    .await
    .expect("providers");
    let has_request_logs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='request_logs'",
    )
    .fetch_one(pool.sqlite())
    .await
    .expect("logs");
    assert!(has_providers > 0, "providers table missing");
    assert!(has_request_logs > 0, "request_logs table missing");
}

// ---- Acceptance #5: retention cleans up old rows ----

#[tokio::test]
async fn acceptance_5_retention_deletes_expired_rows() {
    let (_router, _store, pool) = boot_no_auth().await;
    let old_ts = (chrono::Utc::now() - chrono::Duration::days(60)).to_rfc3339();
    sqlx::query(
        "INSERT INTO request_logs (request_id, ts, virtual_model, ingress_protocol, status) \
         VALUES ('old-1', ?1, 'm', 'openai/chat-completions/v1', 'ok')",
    )
    .bind(&old_ts)
    .execute(pool.sqlite())
    .await
    .expect("insert");
    let deleted = tiygate_store::retention::cleanup_once(pool.as_ref(), 30)
        .await
        .expect("cleanup");
    assert_eq!(deleted, 1);
}

// ---- Acceptance #6: encryption round-trip ----

#[tokio::test]
async fn acceptance_6_provider_secret_decrypts_round_trip() {
    let encryption = KeyEncryption::from_bytes([0xab; 32]);
    let blob = tiygate_store::keys::encrypt_api_key(&encryption, "sk-original").expect("enc");
    let plain = tiygate_store::keys::decrypt_api_key(&encryption, &blob).expect("dec");
    assert_eq!(plain, "sk-original");
}

// ---- Acceptance #10: Admin API bearer auth ----

#[tokio::test]
#[serial_test::serial]
async fn acceptance_10_admin_auth_rejects_missing_token() {
    // Per-thread override is the only way to set the token in
    // parallel tests; setting the env var races with other tests
    // that observe `TIYGATE_ADMIN_TOKEN` at request time.
    tiygate_admin::auth::set_test_admin_token_for_current_thread(Some("correct-horse"));
    let (router, _store, _pool) = boot_with_auth().await;
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/health")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    tiygate_admin::auth::set_test_admin_token_for_current_thread(None);
}

#[tokio::test]
#[serial_test::serial]
async fn acceptance_10_admin_auth_accepts_matching_token() {
    tiygate_admin::auth::set_test_admin_token_for_current_thread(Some("correct-horse"));
    let (router, _store, _pool) = boot_with_auth().await;
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/health")
                .header(header::AUTHORIZATION, "Bearer correct-horse")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "expected 200 OK with valid bearer"
    );
    tiygate_admin::auth::set_test_admin_token_for_current_thread(None);
}

#[tokio::test]
#[serial_test::serial]
async fn acceptance_10_admin_auth_rejects_wrong_token() {
    tiygate_admin::auth::set_test_admin_token_for_current_thread(Some("correct-horse"));
    let (router, _store, _pool) = boot_with_auth().await;
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/health")
                .header(header::AUTHORIZATION, "Bearer wrong-horse")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    tiygate_admin::auth::set_test_admin_token_for_current_thread(None);
}

// ---- OAuth admin routes (Phase 4 §4.5) ----
//
// These tests verify the OAuth admin routes are wired (i.e. the
// router contains them and they return reasonable error codes
// for the no-credential case). They do not exercise a live
// OAuth provider — the applier test coverage lives in
// `crates/providers/src/oauth.rs` (start / exchange / refresh).

#[tokio::test]
async fn oauth_routes_are_registered() {
    // `GET /admin/v1/oauth/callback` must reject requests
    // without a `state` query param (we surface a 400 from
    // the missing-state branch via serde's Query extractor).
    let (router, _store, _pool) = boot_no_auth().await;
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/oauth/callback?code=foo") // no `state`
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("resp");
    // Missing `state` is a 4xx from the axum Query extractor
    // (the handler signature requires both `code` and `state`).
    assert!(
        resp.status().is_client_error(),
        "expected 4xx for missing state, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn oauth_start_rejects_unknown_provider() {
    let (router, _store, _pool) = boot_no_auth().await;
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/v1/oauth/start")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({"provider_id": "nonexistent"})).expect("encode"),
                ))
                .expect("req"),
        )
        .await
        .expect("resp");
    // The handler returns 404 for "provider not found".
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn oauth_callback_rejects_unknown_state() {
    // A well-formed callback with a `state` that was never
    // minted via `start` is rejected as 400 (CSRF protection).
    let (router, _store, _pool) = boot_no_auth().await;
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/oauth/callback?code=foo&state=ghost")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn api_key_quota_patch_and_single_get() {
    // Create a key, then PATCH its quota and confirm the GET reflects
    // the new quota while the status stays `active` (i.e. PATCH does
    // not collide with the PUT disable verb).
    let (router, _store, _pool) = boot_no_auth().await;

    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/api-keys",
            json!({ "name": "agent-q", "quota": { "requests_per_minute": 10 } }),
        ))
        .await
        .expect("create");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // PATCH the quota.
    let resp = router
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/admin/v1/api-keys/{id}"),
            json!({ "quota": { "requests_per_minute": 99, "tokens_per_day": 5000 } }),
        ))
        .await
        .expect("patch");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let patched: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(patched["quota"]["requests_per_minute"], json!(99));
    assert_eq!(patched["quota"]["tokens_per_day"], json!(5000));
    assert_eq!(patched["status"], json!("active"));

    // Single-key GET returns the updated quota plus a `usage` map
    // (empty when no live quota counter is wired into the state).
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/admin/v1/api-keys/{id}"))
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("get");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let got: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(got["id"], json!(id));
    assert_eq!(got["quota"]["requests_per_minute"], json!(99));
    assert!(got["usage"].is_object());

    // GET on an unknown id is a 404.
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/api-keys/does-not-exist")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("get-missing");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
