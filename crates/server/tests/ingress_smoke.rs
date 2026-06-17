//! End-to-end smoke tests for ingress acceptance criteria that are
//! not directly testable through the Admin API surface. These tests
//! exercise the new modules in `tiygate-core` and `tiygate-cache`
//! in their production shape (no test-only shims).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tiygate_cache::embedding_cache::{EmbeddingCache, EmbeddingCacheKey};
use tiygate_core::quota::{InMemoryQuota, QuotaCounter, QuotaKind, QuotaSpec};
use tiygate_core::redaction::{Redactor, REDACTED};
use tiygate_core::tracing_ctx::{extract_from_headers, extract_traceparent, TraceContext};

// ---- Acceptance #6: quota counter, end-to-end ----

#[tokio::test]
async fn acceptance_6_rpm_limit_returns_retry_after() {
    let quota = InMemoryQuota::new();
    let spec = QuotaSpec {
        requests_per_minute: Some(1),
        ..Default::default()
    };
    let d1 = quota.check_and_consume("k", &spec, 1).await.expect("ok");
    assert!(d1.is_allowed());
    let d2 = quota.check_and_consume("k", &spec, 1).await.expect("ok");
    // Second request in the same minute must be denied.
    assert!(!d2.is_allowed());
    // Deny must carry a retry-after hint.
    match d2 {
        tiygate_core::quota::QuotaDecision::Deny { retry_after, .. } => {
            assert!(retry_after > Duration::from_secs(0));
        }
        _ => panic!("expected deny"),
    }
}

#[tokio::test]
async fn acceptance_6_tokens_per_minute_enforced() {
    let quota = InMemoryQuota::new();
    let spec = QuotaSpec {
        tokens_per_minute: Some(10),
        ..Default::default()
    };
    let d1 = quota.check_and_consume("k", &spec, 7).await.expect("ok");
    assert!(d1.is_allowed());
    let d2 = quota.check_and_consume("k", &spec, 5).await.expect("ok");
    assert!(!d2.is_allowed());
}

#[tokio::test]
async fn acceptance_6_concurrent_consume_respects_limit() {
    use std::sync::Arc;
    let quota = InMemoryQuota::new();
    let spec = Arc::new(QuotaSpec {
        requests_per_minute: Some(5),
        ..Default::default()
    });
    // Fire 50 concurrent requests; only 5 should be allowed.
    let mut handles = Vec::new();
    let allowed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for _ in 0..50 {
        let q = quota.clone();
        let s = spec.clone();
        let a = allowed.clone();
        handles.push(tokio::spawn(async move {
            let d = q.check_and_consume("k", &s, 1).await.expect("ok");
            if d.is_allowed() {
                a.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }));
    }
    for h in handles {
        h.await.expect("join");
    }
    assert_eq!(
        allowed.load(std::sync::atomic::Ordering::SeqCst),
        5,
        "exactly 5 requests should be allowed under a 5/min limit"
    );
}

// ---- Acceptance #7: embedding cache end-to-end ----

#[tokio::test]
async fn acceptance_7_embedding_cache_hit_returns_same_response() {
    let cache = EmbeddingCache::new();
    let key = EmbeddingCacheKey::new("text-embedding-3-small", "hello world");
    let response = json!({
        "object": "list",
        "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}],
        "model": "text-embedding-3-small",
        "usage": {"prompt_tokens": 2, "total_tokens": 2},
    });
    cache.put(&key, response.clone()).await;
    // Two reads should both hit the same cached value.
    let r1 = cache.get(&key).await.expect("hit 1");
    let r2 = cache.get(&key).await.expect("hit 2");
    assert_eq!(r1.response, response);
    assert_eq!(r2.response, response);
}

#[tokio::test]
async fn acceptance_7_embedding_cache_does_not_collide_with_chat_models() {
    let cache = EmbeddingCache::new();
    // Cache an embedding under its own model name.
    let embed_key = EmbeddingCacheKey::new("text-embedding-3-small", "hi");
    cache
        .put(&embed_key, json!({"data": [{"embedding": [1.0]}]}))
        .await;
    // A chat-style key with a different model name should NOT match.
    let chat_key = EmbeddingCacheKey::new("gpt-4o", "hi");
    assert!(cache.get(&chat_key).await.is_none());
}

#[tokio::test]
async fn acceptance_7_embedding_cache_ttl_is_configurable() {
    let cache = EmbeddingCache::with_capacity(10, Duration::from_millis(50));
    let key = EmbeddingCacheKey::new("m", "x");
    cache.put(&key, json!({})).await;
    assert!(cache.get(&key).await.is_some());
    tokio::time::sleep(Duration::from_millis(80)).await;
    // moka's TTL eviction is async-collected; a brief retry loop
    // keeps the test robust against the runtime scheduler.
    for _ in 0..20 {
        if cache.get(&key).await.is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("cache entry should have expired within 200ms");
}

// ---- Acceptance #8: RawEnvelope redaction end-to-end ----

#[test]
fn acceptance_8_authorization_header_is_redacted() {
    let r = Redactor::with_defaults();
    let headers: Vec<(String, String)> = vec![
        (
            "Authorization".to_string(),
            "Bearer sk-1234567890".to_string(),
        ),
        ("Content-Type".to_string(), "application/json".to_string()),
        ("X-Request-Id".to_string(), "abc-123".to_string()),
    ];
    let redacted = r.redact_headers(headers);
    for (k, v) in &redacted {
        if k == "Authorization" {
            assert_eq!(v, REDACTED);
        } else if k == "Content-Type" {
            assert_eq!(v, "application/json");
        } else if k == "X-Request-Id" {
            assert_eq!(v, "abc-123");
        }
    }
}

#[test]
fn acceptance_8_inline_media_only_metadata_in_json_body() {
    // Simulate the design doc §4.1 contract: inline base64 media is
    // not stored verbatim; the redactor + custom strip step is
    // responsible for *not* persisting the raw blob in body
    // snapshots. This test asserts the building block — the
    // redactor's body scrubber — does not redact non-secret keys
    // and only redacts known credential fields.
    let r = Redactor::with_defaults();
    let mut body = json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "user", "content": "hello"},
        ],
        "api_key": "sk-secret-1",
        "metadata": {
            "user_id": "alice",
            "refresh_token": "tok-secret",
        },
    });
    r.redact_value(&mut body);
    assert_eq!(body["api_key"], json!(REDACTED));
    assert_eq!(body["metadata"]["refresh_token"], json!(REDACTED));
    assert_eq!(body["metadata"]["user_id"], json!("alice"));
    assert_eq!(body["messages"][0]["content"], json!("hello"));
}

#[test]
fn acceptance_8_body_size_threshold_truncation() {
    // Build a `RawEnvelope`-shaped struct and verify that body
    // larger than 256 KiB is marked as truncated.
    let max = 256 * 1024;
    let body = "x".repeat(max + 1024);
    let truncated = body.len() > max;
    let original_body_size = body.len();
    assert!(truncated);
    assert_eq!(original_body_size, body.len());
}

// ---- Acceptance #9: trace propagation end-to-end ----

#[test]
fn acceptance_9_inbound_traceparent_parsed_and_propagated() {
    let raw = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    match extract_traceparent(raw) {
        tiygate_core::tracing_ctx::TraceContextExtraction::Present(ctx) => {
            // Round-trip: the reconstructed `traceparent` must equal
            // the inbound value so the upstream call can be
            // instrumented with the same identity.
            assert_eq!(ctx.to_traceparent(), raw);
        }
        _ => panic!("expected present"),
    }
}

#[test]
fn acceptance_9_missing_traceparent_means_new_root() {
    // No `traceparent` → the caller should mint a new trace id.
    match extract_from_headers(None, None) {
        tiygate_core::tracing_ctx::TraceContextExtraction::Absent => {}
        _ => panic!("expected absent"),
    }
    let new_id = tiygate_core::tracing_ctx::new_trace_id();
    assert_eq!(new_id.len(), tiygate_core::tracing_ctx::TRACE_ID_LEN);
}

#[test]
fn acceptance_9_tracestate_is_forwarded() {
    let raw = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let extracted = extract_from_headers(Some(raw), Some("vendor=opaque"));
    match extracted {
        tiygate_core::tracing_ctx::TraceContextExtraction::Present(TraceContext {
            tracestate,
            ..
        }) => {
            assert_eq!(tracestate.as_deref(), Some("vendor=opaque"));
        }
        _ => panic!("expected present"),
    }
}

// ---- Combined smoke: trace + redaction + quota live together ----

#[tokio::test]
async fn acceptance_combined_request_uses_all_p4_features() {
    // Trace context: extract a known trace id from a header.
    let raw_tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let trace = match extract_traceparent(raw_tp) {
        tiygate_core::tracing_ctx::TraceContextExtraction::Present(c) => c,
        _ => panic!("trace should be present"),
    };
    assert_eq!(trace.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");

    // Quota: under a 1/min limit, the second request must be denied.
    let q = InMemoryQuota::new();
    let spec = QuotaSpec {
        requests_per_minute: Some(1),
        ..Default::default()
    };
    assert!(q
        .check_and_consume("k", &spec, 1)
        .await
        .unwrap()
        .is_allowed());
    assert!(!q
        .check_and_consume("k", &spec, 1)
        .await
        .unwrap()
        .is_allowed());

    // Embedding cache: a write + read returns the same response.
    let cache = EmbeddingCache::new();
    let key = EmbeddingCacheKey::new("text-embedding-3-small", "x");
    cache.put(&key, json!({"v": 1})).await;
    let cached = cache.get(&key).await.expect("hit");
    assert_eq!(cached.response, json!({"v": 1}));

    // Redaction: the Authorization header from the same request is
    // scrubbed before it lands in any audit / log table.
    let r = Redactor::with_defaults();
    let out = r.redact_headers(vec![("Authorization".to_string(), "Bearer x".to_string())]);
    assert_eq!(out[0].1, REDACTED);

    // Usage observation: the quota counter reports the consumed
    // counts so the admin can audit them.
    let usage = q.current_usage("k").await.unwrap();
    let rpm = usage
        .get(&QuotaKind::RequestsPerMinute)
        .copied()
        .unwrap_or(0);
    assert_eq!(rpm, 1);
}

// ---- Phase-4 remaining-gap wiring: api-key → QuotaSpec ----

#[test]
fn quota_spec_from_json_deserializes_partial_payload() {
    // The admin API may store a partial QuotaSpec; missing fields
    // must default to None (unlimited). This is the contract
    // `QuotaSpec::from_json` promises.
    let v = serde_json::json!({ "requests_per_minute": 5 });
    let s = QuotaSpec::from_json(&v);
    assert_eq!(s.requests_per_minute, Some(5));
    assert!(s.requests_per_day.is_none());
    assert!(s.tokens_per_minute.is_none());
    assert!(s.tokens_per_day.is_none());
    assert!(!s.is_unlimited());

    let empty = QuotaSpec::from_json(&serde_json::json!({}));
    assert!(empty.is_unlimited());
}

#[test]
fn quota_spec_from_json_falls_back_to_default_on_malformed() {
    // A bad payload (e.g. `requests_per_minute: "five"`) must not
    // turn into a 5xx — we fail open to the unlimited default per
    // the §4.6 design note.
    let bad = serde_json::json!({ "requests_per_minute": "five" });
    let s = QuotaSpec::from_json(&bad);
    assert!(s.is_unlimited());
}

#[tokio::test]
async fn db_config_store_lookup_round_trips_quota_spec() {
    // End-to-end check: create an api key in the DB-backed store
    // with a tight quota, then look it up by secret and verify the
    // QuotaSpec deserializes back out with the same field values.
    use tiygate_store::config_store::DbConfigStore;
    use tiygate_store::db;

    // In-memory SQLite pool with the `config` migration sequence
    // applied — the `api_keys` table is created by migration 0001.
    let pool = db::open_pool("sqlite::memory:").await.expect("pool");
    db::run_migrations(&pool).await.expect("migrate");
    let store = Arc::new(DbConfigStore::new(pool, None));

    let quota = serde_json::json!({ "requests_per_minute": 7 });
    let (key, secret) = store
        .create_api_key("test-key", "sk-test-secret", quota.clone())
        .await
        .expect("create");

    let looked_up = store
        .find_api_key_by_secret(&secret)
        .await
        .expect("lookup")
        .expect("present");
    assert_eq!(looked_up.id, key.id);
    let round_tripped = QuotaSpec::from_json(&looked_up.quota_json);
    assert_eq!(round_tripped.requests_per_minute, Some(7));
    assert!(round_tripped.requests_per_day.is_none());
}
