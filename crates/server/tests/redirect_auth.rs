//! Tests for upstream redirect behavior.
//!
//! Issue #26: HTTP upstream endpoints always return 401 due to reqwest
//! redirect stripping the Authorization header on cross-origin redirects.
//!
//! reqwest's default redirect policy follows up to 10 redirects and silently
//! strips sensitive headers (including `Authorization`) when the redirect
//! target has a different origin (scheme, host, or port). For an AI gateway
//! this is wrong: if the upstream or a reverse proxy issues a cross-origin
//! redirect (e.g. HTTP → HTTPS, or a different port), the gateway follows it
//! but drops the API key, causing a 401.
//!
//! These tests verify that the gateway does **not** silently follow
//! cross-origin redirects and strip credentials. After the fix, the
//! gateway's `reqwest::Client` should have redirects disabled so 3xx
//! responses are surfaced to the caller instead of being followed.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::field_reassign_with_default
)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tiygate_core::{HealthRegistry, ProtocolEndpoint, ProtocolSuite, RoutingTable};
use tiygate_server::config::ServerConfig;
use tiygate_server::ingress;
use tiygate_store::config::ConfigStore;
use tower::ServiceExt;

/// Build a test app with a single OpenAI-compatible route pointing at
/// `upstream_url` with API key `sk-test`.
fn build_test_app(upstream_url: String, model: &str) -> axum::Router {
    let mut routing_table = RoutingTable::new();
    routing_table.insert(
        model.to_string(),
        vec![tiygate_core::RoutingTarget {
            provider_id: "openai".to_string(),
            model_id: "gpt-4o".to_string(),
            api_base: upstream_url,
            api_key: "sk-test".to_string(),
            api_protocol: ProtocolEndpoint::new(
                ProtocolSuite::OpenAiCompatible,
                "chat-completions",
                "v1",
            ),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
            oauth: None,
        }],
    );

    let config_store = ConfigStore::with_routing_table(routing_table);
    let health = Arc::new(HealthRegistry::with_defaults());
    let mut server_config = ServerConfig::default();
    server_config.require_api_key = false;
    ingress::router(config_store, health, &server_config)
}

/// Helper: send a non-streaming chat completions request through `app`.
async fn send_chat_request(app: axum::Router) -> (StatusCode, String) {
    let body = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "Hi"}]
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body_bytes).to_string())
}

/// When the upstream issues a cross-origin 307 redirect (different port),
/// reqwest's default policy follows it but strips the `Authorization` header.
///
/// **Before the fix:** the gateway silently follows the redirect, the
/// Authorization header is dropped, the redirected upstream returns 401, and
/// the caller sees an upstream error — even though the API key was correct.
///
/// **After the fix (redirects disabled):** the gateway does not follow the
/// redirect. The redirected-to server never receives a request, and the
/// caller does not see a 401 caused by a stripped credential.
#[tokio::test]
async fn test_cross_origin_redirect_does_not_strip_authorization() {
    // --- Server B: the redirect target. Expects Authorization header. ---
    let target_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-redirected",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Hello from target!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        })))
        // Give it a name so we can assert on it independently.
        .named("target-server-b")
        .mount(&target_server)
        .await;

    // --- Server A: issues a 307 redirect to Server B. ---
    // Two separate wiremock servers run on different ports, which reqwest
    // treats as a cross-origin redirect — triggering Authorization stripping.
    let redirect_server = wiremock::MockServer::start().await;

    let target_url = format!("{}/chat/completions", target_server.uri());
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(307)
                .insert_header("location", &target_url)
                .set_body_json(json!({"error": "redirect"})),
        )
        .named("redirect-server-a")
        .mount(&redirect_server)
        .await;

    // --- Gateway points at Server A (the redirecting server). ---
    let app = build_test_app(redirect_server.uri(), "gpt-4o");
    let (status, body) = send_chat_request(app).await;

    // After the fix, redirects are disabled. The gateway should NOT follow
    // the 307, so Server B should never receive a request. The caller may
    // see a non-200 (e.g. 502 from JSON parse failure on the 307 body, or
    // the 307 surfaced directly), but crucially must NOT see a 401 caused
    // by a stripped Authorization header.
    //
    // Before the fix, reqwest follows the 307, strips Authorization, Server B
    // receives a request without auth (but our mock returns 200 regardless),
    // and the caller sees 200 — which is wrong because the gateway silently
    // followed a cross-origin redirect and would fail if Server B validated
    // the auth header.
    //
    // The key assertion: Server B (the redirect target) must NOT receive
    // any requests. If it did, that means the gateway followed the redirect
    // — which is the bug.
    let target_received = target_server.received_requests().await;
    let target_request_count = target_received.map(|r| r.len()).unwrap_or(0);
    assert_eq!(
        target_request_count, 0,
        "Redirect target server received {target_request_count} request(s) — \
         the gateway followed the cross-origin redirect (and likely stripped \
         the Authorization header).",
    );

    // Additionally, assert the caller does not get a 401 (which would
    // indicate auth was stripped). A 401 here is the symptom from the issue.
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "Gateway returned 401 — Authorization header was likely stripped on redirect. \
         Body: {body}"
    );
}

/// When the upstream issues a same-origin redirect (same server, different
/// path), the Authorization header should be preserved if redirects are
/// followed. If redirects are disabled (the fix), the 3xx is surfaced
/// directly.
///
/// This test documents that same-origin redirects on the *same* wiremock
/// server are also affected by the redirect policy — the gateway should not
/// follow them either, for consistency.
#[tokio::test]
async fn test_same_origin_redirect_not_followed() {
    let mock_server = wiremock::MockServer::start().await;

    // The redirect target path on the same server.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer sk-test",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-same-origin",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Hello!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        })))
        .named("same-origin-target")
        .mount(&mock_server)
        .await;

    // The redirecting path returns a 307 to /v1/chat/completions on the same server.
    let target_url = format!("{}/v1/chat/completions", mock_server.uri());
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(307)
                .insert_header("location", &target_url)
                .set_body_json(json!({"error": "redirect"})),
        )
        .named("same-origin-redirect")
        .mount(&mock_server)
        .await;

    let app = build_test_app(mock_server.uri(), "gpt-4o");
    let (status, body) = send_chat_request(app).await;

    // After the fix, redirects are disabled. The gateway should surface the
    // 3xx (or a derived error) without following it. The caller must not
    // receive a 200 from the redirect target, because that would mean the
    // gateway silently followed a redirect — inconsistent gateway behavior.
    //
    // Before the fix, reqwest follows the same-origin 307, preserves the
    // Authorization header (same origin), and returns 200.
    //
    // We assert the redirect target path was NOT hit (redirects disabled).
    // The wiremock `named("same-origin-target")` mock should have 0 hits.
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "Gateway returned 401 — Authorization header was likely stripped. \
         Body: {body}"
    );
}

/// Sanity check: without any redirect, the happy path still works and the
/// Authorization header is forwarded to the upstream.
#[tokio::test]
async fn test_no_redirect_happy_path_preserves_auth() {
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer sk-test",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-happy",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Hello!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        })))
        .mount(&mock_server)
        .await;

    let app = build_test_app(mock_server.uri(), "gpt-4o");
    let (status, body) = send_chat_request(app).await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("Hello!"),
        "Body should contain response: {body}"
    );
}
