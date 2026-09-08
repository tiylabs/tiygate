//! Wiremock-based provider integration tests.
//!
//! Validates that the gateway correctly forwards requests to upstream
//! providers, decodes the response, and applies auth headers / passthrough
//! behavior. Uses `wiremock` to stand in for the upstream API.
//!
//! Phase 1 acceptance criterion: 3 providers × happy-path coverage.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::field_reassign_with_default
)]

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tiygate_auth::provider_oauth::OAuthTokenCache;
use tiygate_core::provider::oauth::{
    OAuthEgressProfile, OAuthTargetConfig, TokenRequestStyle, UpstreamTransport,
};
use tiygate_core::quota::InMemoryQuota;
use tiygate_core::{HealthRegistry, ProtocolEndpoint, ProtocolSuite, RoutingTable};
use tiygate_server::config::ServerConfig;
use tiygate_server::ingress;
use tiygate_store::config::ConfigStore;
use tiygate_store::config_store::DbConfigStore;
use tiygate_store::db;
use tiygate_store::models::{AuthMode, RouteTarget};
use tower::ServiceExt;

/// Build a test app with a single OpenAI-compatible route to a wiremock upstream.
fn build_openai_test_app(upstream_url: String, model: &str) -> axum::Router {
    {
        let mut cfg = ServerConfig::default();
        cfg.require_api_key = false;
        build_test_app_with_config(upstream_url, model, cfg)
    }
}

/// Build a test app with a single OpenAI-compatible route to a wiremock upstream
/// and a custom server config (used by the concurrency overflow test).
fn build_test_app_with_config(
    upstream_url: String,
    model: &str,
    server_config: ServerConfig,
) -> axum::Router {
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
    ingress::router(config_store, health, &server_config)
}

/// Build a test app with a single Anthropic Messages route to a wiremock upstream.
fn build_anthropic_test_app(upstream_url: String, model: &str) -> axum::Router {
    let mut server_config = ServerConfig::default();
    server_config.require_api_key = false;
    build_anthropic_test_app_with_config(upstream_url, model, server_config)
}

/// Build a test app with a single Anthropic Messages route and custom config.
fn build_anthropic_test_app_with_config(
    upstream_url: String,
    model: &str,
    server_config: ServerConfig,
) -> axum::Router {
    let mut routing_table = RoutingTable::new();
    routing_table.insert(
        model.to_string(),
        vec![tiygate_core::RoutingTarget {
            provider_id: "anthropic".to_string(),
            model_id: "claude-3-5-sonnet-20241022".to_string(),
            api_base: upstream_url,
            api_key: "sk-ant-test".to_string(),
            api_protocol: ProtocolEndpoint::new(
                ProtocolSuite::AnthropicMessages,
                "messages",
                "2023-06-01",
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
    ingress::router(config_store, health, &server_config)
}

/// Build an Anthropic Messages ingress app whose target uses the Codex OAuth
/// Responses profile. The access token is seeded by the test before sending a
/// request, so no token endpoint is needed for this data-plane regression.
fn build_anthropic_ingress_codex_egress_app(
    upstream_url: String,
    model: &str,
    provider_id: &str,
) -> axum::Router {
    let mut routing_table = RoutingTable::new();
    routing_table.insert(
        model.to_string(),
        vec![tiygate_core::RoutingTarget {
            provider_id: provider_id.to_string(),
            model_id: model.to_string(),
            api_base: upstream_url,
            api_key: String::new(),
            api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiResponses, "responses", "v1"),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
            oauth: Some(OAuthTargetConfig {
                upstream_transport: UpstreamTransport::Http,
                egress_profile: OAuthEgressProfile::OpenAiCodex,
                token_url: "https://example.test/token".to_string(),
                client_id: "codex-test-client".to_string(),
                client_secret: None,
                refresh_token: "refresh-token".to_string(),
                scopes: Vec::new(),
                token_request_style: TokenRequestStyle::Form,
                authorization_header: None,
                authorization_prefix: None,
                extra_headers: Vec::new(),
                account_id: None,
            }),
        }],
    );

    let config_store = ConfigStore::with_routing_table(routing_table);
    let health = Arc::new(HealthRegistry::with_defaults());
    let mut server_config = ServerConfig::default();
    server_config.require_api_key = false;
    ingress::router(config_store, health, &server_config)
}

/// Build a test app for a generic OpenAI-compatible provider (custom base URL + key).
fn build_openai_compatible_test_app(
    upstream_url: String,
    model: &str,
    api_key: &str,
) -> axum::Router {
    let mut routing_table = RoutingTable::new();
    routing_table.insert(
        model.to_string(),
        vec![tiygate_core::RoutingTarget {
            provider_id: "openai-compatible".to_string(),
            model_id: model.to_string(),
            api_base: upstream_url,
            api_key: api_key.to_string(),
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

#[tokio::test]
async fn test_happy_path_openai_chat_completion() {
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer sk-test",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Hello from wiremock!"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        })))
        .mount(&mock_server)
        .await;

    let app = build_openai_test_app(mock_server.uri(), "gpt-4o");

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
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);
    assert!(body_str.contains("Hello from wiremock!"));
    assert!(body_str.contains("\"prompt_tokens\":10"));
}

#[tokio::test]
async fn test_nonstream_timeout_is_configurable_and_can_be_disabled() {
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(1_100))
                .set_body_json(json!({
                    "id": "chatcmpl-slow",
                    "object": "chat.completion",
                    "created": 1700000000,
                    "model": "gpt-4o",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "completed"},
                        "finish_reason": "stop"
                    }]
                })),
        )
        .mount(&mock_server)
        .await;

    let body = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "wait"}]
    });

    let mut timeout_config = ServerConfig::default();
    timeout_config.require_api_key = false;
    timeout_config.upstream_nonstream_timeout_secs = 1;
    let timed_app = build_test_app_with_config(mock_server.uri(), "gpt-4o", timeout_config);
    let timed_request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let timed_response = timed_app.oneshot(timed_request).await.unwrap();
    assert_eq!(timed_response.status(), StatusCode::GATEWAY_TIMEOUT);
    let timed_body = axum::body::to_bytes(timed_response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&timed_body).contains("upstream non-stream request timeout"));

    let mut disabled_config = ServerConfig::default();
    disabled_config.require_api_key = false;
    disabled_config.upstream_nonstream_timeout_secs = 0;
    let unlimited_app = build_test_app_with_config(mock_server.uri(), "gpt-4o", disabled_config);
    let unlimited_request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let unlimited_response = unlimited_app.oneshot(unlimited_request).await.unwrap();
    assert_eq!(unlimited_response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_nonstream_timeout_covers_messages_embeddings_responses_and_gemini() {
    let mock_server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(1_100))
                .set_body_json(json!({"id": "slow"})),
        )
        .mount(&mock_server)
        .await;

    let mut config = ServerConfig::default();
    config.require_api_key = false;
    config.upstream_nonstream_timeout_secs = 1;

    let messages_app = build_anthropic_test_app_with_config(
        mock_server.uri(),
        "claude-3-5-sonnet-20241022",
        config.clone(),
    );
    let messages_request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "claude-3-5-sonnet-20241022",
                "max_tokens": 32,
                "messages": [{"role": "user", "content": "wait"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    assert_eq!(
        messages_app
            .oneshot(messages_request)
            .await
            .unwrap()
            .status(),
        StatusCode::GATEWAY_TIMEOUT
    );

    let embeddings_app = build_test_app_with_config(mock_server.uri(), "gpt-4o", config.clone());
    let embeddings_request = Request::builder()
        .method("POST")
        .uri("/v1/embeddings")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "gpt-4o",
                "input": "wait"
            }))
            .unwrap(),
        ))
        .unwrap();
    assert_eq!(
        embeddings_app
            .oneshot(embeddings_request)
            .await
            .unwrap()
            .status(),
        StatusCode::GATEWAY_TIMEOUT
    );

    let responses_app = build_responses_ingress_openai_egress_app_with_config(
        mock_server.uri(),
        "gpt-4o",
        config.clone(),
    );
    let responses_request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({"model": "gpt-4o", "input": "wait"})).unwrap(),
        ))
        .unwrap();
    assert_eq!(
        responses_app
            .oneshot(responses_request)
            .await
            .unwrap()
            .status(),
        StatusCode::GATEWAY_TIMEOUT
    );

    let gemini_app =
        build_gemini_ingress_openai_egress_app_with_config(mock_server.uri(), "gemini-pro", config);
    let gemini_request = Request::builder()
        .method("POST")
        .uri("/v1beta/models/gemini-pro/generateContent")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "contents": [{"role": "user", "parts": [{"text": "wait"}]}]
            }))
            .unwrap(),
        ))
        .unwrap();
    assert_eq!(
        gemini_app.oneshot(gemini_request).await.unwrap().status(),
        StatusCode::GATEWAY_TIMEOUT
    );
}

#[tokio::test]
async fn test_happy_path_anthropic_messages() {
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/messages"))
        .and(wiremock::matchers::header("x-api-key", "sk-ant-test"))
        .and(wiremock::matchers::header(
            "anthropic-version",
            "2023-06-01",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_test_123",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "Hi from Anthropic wiremock!"}],
            "model": "claude-3-5-sonnet-20241022",
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 12,
                "output_tokens": 7
            }
        })))
        .mount(&mock_server)
        .await;

    let app = build_anthropic_test_app(mock_server.uri(), "claude-3-5-sonnet-20241022");

    let body = json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);
    assert!(body_str.contains("Hi from Anthropic wiremock!"));
    assert!(body_str.contains("\"role\":\"assistant\""));
}

#[tokio::test]
async fn test_anthropic_messages_to_codex_responses_strips_unsupported_metadata() {
    let mock_server = wiremock::MockServer::start().await;
    let provider_id = "issue-52-codex-metadata";
    let cache = OAuthTokenCache::global();
    cache.invalidate_provider(provider_id);
    cache.seed_tokens(
        provider_id,
        "__provider__",
        "access-token",
        "refresh-token",
        Some(Duration::from_secs(300)),
    );

    let upstream_sse = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_issue_52\",\"object\":\"response\",\"status\":\"in_progress\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_issue_52\",\"object\":\"response\",\"status\":\"completed\",\"model\":\"gpt-5.6-luna\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello from Codex\"}]}],\"usage\":{\"input_tokens\":3,\"output_tokens\":4,\"total_tokens\":7}}}\n\n"
    );
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/responses"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer access-token",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(upstream_sse),
        )
        .mount(&mock_server)
        .await;

    let app =
        build_anthropic_ingress_codex_egress_app(mock_server.uri(), "gpt-5.6-luna", provider_id);
    let body = json!({
        "model": "gpt-5.6-luna",
        "max_tokens": 32000,
        "temperature": 0.2,
        "top_p": 0.8,
        "stop_sequences": ["DONE"],
        "messages": [{"role": "user", "content": "Hello"}],
        "metadata": {"user_id": "{\"device_id\":\"device-issue-52\",\"session_id\":\"issue-52-session\"}"}
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response_body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let response_json: serde_json::Value = serde_json::from_slice(&response_body).unwrap();
    assert_eq!(response_json["type"], "message");
    assert_eq!(response_json["content"][0]["text"], "Hello from Codex");

    let requests = mock_server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let upstream_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(upstream_body["model"], "gpt-5.6-luna");
    assert_eq!(upstream_body["stream"], true);
    assert_eq!(upstream_body["instructions"], "");
    assert_eq!(upstream_body["store"], false);
    assert!(
        upstream_body["prompt_cache_key"]
            .as_str()
            .is_some_and(|key| uuid::Uuid::parse_str(key).is_ok()),
        "Claude session should produce a stable Codex prompt_cache_key: {upstream_body}"
    );
    assert!(upstream_body.get("metadata").is_none());
    assert!(upstream_body.get("max_output_tokens").is_none());
    assert!(upstream_body.get("temperature").is_none());
    assert!(upstream_body.get("top_p").is_none());
    assert!(upstream_body.get("stop").is_none());

    cache.invalidate_provider(provider_id);
}

#[tokio::test]
async fn test_happy_path_openai_compatible_provider() {
    // Simulate a generic OpenAI-compatible provider (e.g. DeepSeek, Moonshot, Ollama)
    // by configuring a custom base URL and key.
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer custom-key-abc",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "compat-test-1",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "custom-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok from custom provider"},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 3,
                "completion_tokens": 4,
                "total_tokens": 7
            }
        })))
        .mount(&mock_server)
        .await;

    let app = build_openai_compatible_test_app(mock_server.uri(), "custom-model", "custom-key-abc");

    let body = json!({
        "model": "custom-model",
        "messages": [{"role": "user", "content": "ping"}]
    });

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);
    assert!(body_str.contains("ok from custom provider"));
}

#[tokio::test]
async fn test_429_propagates_with_retry_after() {
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(
            wiremock::ResponseTemplate::new(429)
                .insert_header("retry-after", "30")
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-limit", "100")
                .set_body_json(json!({
                    "error": {"message": "rate limited", "code": "rate_limit"}
                })),
        )
        .mount(&mock_server)
        .await;

    let app = build_openai_test_app(mock_server.uri(), "gpt-4o");

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
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap()),
        Some("30")
    );
    // x-ratelimit-remaining should be passthrough'd
    assert_eq!(
        response
            .headers()
            .get("x-ratelimit-remaining")
            .map(|v| v.to_str().unwrap()),
        Some("0")
    );
}

#[tokio::test]
async fn test_413_payload_too_large() {
    // Build app with a very small body limit
    let mut server_config = ServerConfig::default();
    server_config.require_api_key = false;
    server_config.max_request_body_bytes = 100; // 100 bytes
    let config_store = ConfigStore::default();
    let health = Arc::new(HealthRegistry::with_defaults());
    let app = ingress::router(config_store, health, &server_config);

    // Build a valid JSON body > 100 bytes
    let big_body = format!(
        r#"{{"model":"gpt-4o","messages":[{{"role":"user","content":"{}"}}]}}"#,
        "x".repeat(200)
    );
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(big_body))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    // tower-http's RequestBodyLimitLayer returns 413 for bodies over the limit
    let status = response.status();
    assert!(
        status == StatusCode::PAYLOAD_TOO_LARGE || status == StatusCode::BAD_REQUEST,
        "expected 413 or 400, got {}",
        status
    );
}

#[tokio::test]
async fn test_404_for_unknown_model() {
    let app = build_openai_test_app("http://unused".to_string(), "gpt-4o");
    let body = json!({
        "model": "nonexistent-model",
        "messages": [{"role": "user", "content": "Hi"}]
    });

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Concurrency semaphore: when `max_inflight=1` is configured and we
/// hold the only permit, a second concurrent request must be rejected
/// with 503 + Retry-After (queue full path) once the bounded queue
/// depth is exhausted. The test fires N concurrent requests against a
/// slow upstream (via the chat path that needs to resolve a route and
/// hit a wiremock that delays 200ms), and asserts that at least one
/// returns 503.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrency_overflow_returns_503() {
    // Slow upstream so the first permit stays held long enough for
    // the queue to fill.
    let mock_server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(300))
                .set_body_json(json!({
                    "id": "slow-1",
                    "object": "chat.completion",
                    "created": 1700000000,
                    "model": "gpt-4o",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "slow ok"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                })),
        )
        .mount(&mock_server)
        .await;

    // max_inflight=1 + max_queue_depth=0 + acquire_timeout=0 ⇒ any
    // second concurrent request is rejected with 503.
    let mut server_config = ServerConfig::default();
    server_config.require_api_key = false;
    server_config.max_inflight_requests = 1;
    server_config.max_queue_depth = 0;
    server_config.acquire_timeout_secs = 0;
    let app = build_test_app_with_config(mock_server.uri(), "gpt-4o", server_config);

    // Fire 8 concurrent requests; most should observe 503 because
    // the first holds the only permit for ~300ms.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            let body = json!({
                "model": "gpt-4o",
                "messages": [{"role": "user", "content": "hi"}]
            });
            let req = Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();
            app.oneshot(req).await.unwrap().status()
        }));
    }
    let mut saw_503 = false;
    for h in handles {
        let status = h.await.expect("task panicked");
        if status == StatusCode::SERVICE_UNAVAILABLE {
            saw_503 = true;
        }
    }
    assert!(
        saw_503,
        "expected at least one 503 under max_inflight=1 + max_queue_depth=0"
    );
}

/// Build a test app with multi-target fallback. The first target gets
/// 5xx, the second target gets 200. With default retry policy, the
/// gateway must transfer to the second target and return 200.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_multi_target_fallback_5xx_transfers() {
    let primary = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("upstream is down"))
        .mount(&primary)
        .await;

    let secondary = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "fallback-1",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "fallback ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&secondary)
        .await;

    // Build app with two targets on the same model.
    let mut routing_table = RoutingTable::new();
    routing_table.insert(
        "gpt-4o".to_string(),
        vec![
            tiygate_core::RoutingTarget {
                provider_id: "primary".to_string(),
                model_id: "gpt-4o".to_string(),
                api_base: primary.uri(),
                api_key: "sk-1".to_string(),
                api_protocol: ProtocolEndpoint::new(
                    ProtocolSuite::OpenAiCompatible,
                    "chat-completions",
                    "v1",
                ),
                account_label: Some("primary".to_string()),
                api_key_override: None,
                api_base_override: None,
                weight: 1.0,
                oauth: None,
            },
            tiygate_core::RoutingTarget {
                provider_id: "secondary".to_string(),
                model_id: "gpt-4o".to_string(),
                api_base: secondary.uri(),
                api_key: "sk-2".to_string(),
                api_protocol: ProtocolEndpoint::new(
                    ProtocolSuite::OpenAiCompatible,
                    "chat-completions",
                    "v1",
                ),
                account_label: Some("secondary".to_string()),
                api_key_override: None,
                api_base_override: None,
                weight: 1.0,
                oauth: None,
            },
        ],
    );
    let config_store = ConfigStore::with_routing_table(routing_table);
    let health = Arc::new(HealthRegistry::with_defaults());
    let app = {
        let mut cfg = ServerConfig::default();
        cfg.require_api_key = false;
        ingress::router(config_store, health, &cfg)
    };

    let body = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    // Must be 200 (the secondary target served the request) — 500
    // would mean the gateway did not transfer to the next target.
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);
    assert!(body_str.contains("fallback ok"));
}

#[tokio::test]
async fn test_streaming_chat_completion_forwards_done_frame() {
    // Streaming path: when the upstream sends its own protocol-native
    // terminal frame (`data: [DONE]`), the gateway must forward it
    // verbatim to the downstream — and must NOT duplicate it. This
    // exercises the `drive_upstream_stream` verbatim passthrough where the
    // upstream provides a faithful terminator.
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer sk-test",
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
                ),
        )
        .mount(&mock_server)
        .await;

    let app = build_openai_test_app(mock_server.uri(), "gpt-4o");

    let body = json!({
        "model": "gpt-4o",
        "stream": true,
        "messages": [{"role": "user", "content": "Hi"}]
    });

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);
    // The real frame from the upstream must be present.
    assert!(
        body_str.contains("\"content\":\"hi\""),
        "expected upstream frame, got: {body_str}"
    );
    // The upstream's own terminal frame must be forwarded ...
    assert!(
        body_str.contains("data: [DONE]"),
        "expected upstream's terminal frame to be forwarded, got: {body_str}"
    );
    // ... exactly once (no gateway-synthesized duplicate).
    assert_eq!(
        body_str.matches("[DONE]").count(),
        1,
        "expected exactly one [DONE] frame, got: {body_str}"
    );
}

#[tokio::test]
async fn test_streaming_error_frame_injects_end_marker() {
    // When the upstream returns HTTP 200 with an SSE error frame (e.g.
    // service_unavailable_error) and then closes the stream WITHOUT a
    // natural terminal frame (`data: [DONE]`), the gateway must inject
    // the protocol-native end marker so the client SDK does not time
    // out waiting for it.
    let mock_server = wiremock::MockServer::start().await;

    let sse_error = "data: {\"error\":{\"type\":\"service_unavailable_error\",\"message\":\"Service unavailable\"}}\n\n";

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_error),
        )
        .mount(&mock_server)
        .await;

    let app = build_openai_test_app(mock_server.uri(), "gpt-4o");

    let body = json!({
        "model": "gpt-4o",
        "stream": true,
        "messages": [{"role": "user", "content": "Hi"}]
    });

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);
    // The upstream error frame must be forwarded.
    assert!(
        body_str.contains("service_unavailable_error"),
        "expected upstream error frame, got: {body_str}"
    );
    // The gateway must inject `data: [DONE]` so the client SDK can
    // close the stream cleanly.
    assert!(
        body_str.contains("data: [DONE]"),
        "expected gateway-injected end marker, got: {body_str}"
    );
}

#[tokio::test]
async fn test_passthrough_forwards_raw_body_verbatim() {
    // PassThrough contract: when the target's protocol suite matches the
    // ingress suite and the codec declares Passthrough, the gateway
    // forwards the original request body bytes verbatim to the upstream
    // (no IR round-trip). wiremock captures the received body and we
    // assert it is byte-for-byte equal to what the client sent —
    // including a custom upstream-only field that the OpenAI codec
    // would drop on re-serialization.
    use std::sync::{Arc as StdArc, Mutex as StdMutex};
    let mock_server = wiremock::MockServer::start().await;

    let captured: StdArc<StdMutex<Option<Vec<u8>>>> = StdArc::new(StdMutex::new(None));
    let captured_clone = captured.clone();

    // Marker that the OpenAI encode_request would drop on re-serialize
    // (the OpenAI IR does not model `x_custom_upstream_field`).
    let raw_body_marker = "x_custom_upstream_field_MARKER_42";
    // A body that contains the marker literally — the gateway must
    // forward this byte-for-byte. If it re-serialises via the codec,
    // the marker is dropped and the mock won't match.
    let forwarded_body = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "ping"}],
        "x_custom_upstream_field": raw_body_marker
    });
    let expected_body_str = serde_json::to_string(&forwarded_body).unwrap();

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer sk-test",
        ))
        .and(wiremock::matchers::body_string(expected_body_str.clone()))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "passthrough-1",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&mock_server)
        .await;

    let app = build_openai_test_app(mock_server.uri(), "gpt-4o");

    let body_bytes = expected_body_str.into_bytes();
    let _ = captured_clone; // mark used; body assertion is via the regex matcher

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body_bytes))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // The regex matcher on the mock verifies the upstream received the
    // raw marker field byte-for-byte. If the gateway had re-serialised
    // via the codec, the field would be dropped and the mock would
    // respond 404 (no matching request body).
}

/// The gateway must rewrite the client's *virtual* model name to the
/// routing target's real upstream `model_id` before forwarding. The
/// mock only matches when the request body's `model` equals the real
/// upstream model id ("gpt-4o"), not the virtual name ("my-smart-model").
#[tokio::test]
async fn test_virtual_model_rewritten_to_upstream_model_id() {
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .and(wiremock::matchers::body_partial_json(json!({
            "model": "gpt-4o"
        })))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&mock_server)
        .await;

    // Virtual model name is "my-smart-model"; the route maps it to the
    // real upstream model id "gpt-4o" (see build_openai_test_app).
    let app = build_openai_test_app(mock_server.uri(), "my-smart-model");

    let body = json!({
        "model": "my-smart-model",
        "messages": [{"role": "user", "content": "Hi"}]
    });

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    // If the gateway forwarded the virtual name "my-smart-model", the
    // body_partial_json("gpt-4o") matcher would fail and wiremock would
    // return 404. A 200 proves the model id was rewritten.
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_streaming_rewrites_model_without_double_data() {
    // Regression: the gateway used to re-wrap each upstream SSE frame in
    // an axum `Event::default().data(...)`, producing a corrupt
    // double-`data:` prefix (`data: data: {...}`) and a duplicate
    // terminal frame. Model normalization intentionally reserializes
    // JSON data lines, but must preserve framing and the single
    // upstream terminator.
    let mock_server = wiremock::MockServer::start().await;

    // Faithful OpenAI-style SSE body: two delta frames plus the
    // upstream's own `data: [DONE]` terminator.
    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"he\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\ndata: [DONE]\n\n";

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&mock_server)
        .await;

    let app = build_openai_test_app(mock_server.uri(), "virtual/chat");
    let body = json!({
        "model": "virtual/chat",
        "stream": true,
        "messages": [{"role": "user", "content": "Hi"}]
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
        "SSE response must set content-type: text/event-stream"
    );

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);

    assert!(
        body_str.contains("\"model\":\"virtual/chat\""),
        "gateway must expose the requested virtual model, got: {body_str:?}"
    );
    // Defensive: no double `data:` prefix anywhere.
    assert!(
        !body_str.contains("data: data:"),
        "double data: prefix leaked: {body_str:?}"
    );
    // Defensive: exactly one `[DONE]` (no duplicate terminal frame).
    assert_eq!(
        body_str.matches("[DONE]").count(),
        1,
        "expected exactly one [DONE] frame, got: {body_str:?}"
    );
}

#[tokio::test]
async fn test_streaming_no_synthetic_done_when_upstream_omits_it() {
    // New behavior: when the upstream closes cleanly WITHOUT its own
    // terminal frame, the gateway must NOT synthesize a success `[DONE]`.
    // Fabricating a terminator turns a recoverable "incomplete stream"
    // (which clients detect on EOF and retry) into a corrupt "successful"
    // response — especially dangerous when the upstream was cut mid-frame.
    // The gateway forwards upstream bytes verbatim and ends at EOF,
    // leaving the client to decide whether the stream was complete.
    let mock_server = wiremock::MockServer::start().await;

    let sse_body = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body),
        )
        .mount(&mock_server)
        .await;

    let app = build_openai_test_app(mock_server.uri(), "gpt-4o");
    let body = json!({
        "model": "gpt-4o",
        "stream": true,
        "messages": [{"role": "user", "content": "Hi"}]
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);

    assert!(
        body_str.contains("\"model\":\"gpt-4o\""),
        "gateway must include the requested model in each rewritten chunk, got: {body_str:?}"
    );
    assert!(
        !body_str.contains("[DONE]"),
        "gateway must NOT synthesize a [DONE] the upstream never sent, got: {body_str:?}"
    );
    assert!(
        !body_str.contains("data: data:"),
        "double data: prefix leaked: {body_str:?}"
    );
}

/// Build a test app whose ingress entrypoint is OpenAI chat-completions but
/// whose single route targets an Anthropic Messages upstream (cross-protocol).
fn build_chat_ingress_anthropic_egress_app(upstream_url: String, model: &str) -> axum::Router {
    let mut routing_table = RoutingTable::new();
    routing_table.insert(
        model.to_string(),
        vec![tiygate_core::RoutingTarget {
            provider_id: "anthropic".to_string(),
            model_id: "claude-3-5-sonnet-20241022".to_string(),
            api_base: upstream_url,
            api_key: "sk-ant-test".to_string(),
            api_protocol: ProtocolEndpoint::new(
                ProtocolSuite::AnthropicMessages,
                "messages",
                "2023-06-01",
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
    {
        let mut cfg = ServerConfig::default();
        cfg.require_api_key = false;
        ingress::router(config_store, health, &cfg)
    }
}

/// Build a test app whose ingress entrypoint is Anthropic Messages but whose
/// single route targets an OpenAI chat-completions upstream (cross-protocol).
fn build_messages_ingress_openai_egress_app(upstream_url: String, model: &str) -> axum::Router {
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
    {
        let mut cfg = ServerConfig::default();
        cfg.require_api_key = false;
        ingress::router(config_store, health, &cfg)
    }
}

#[tokio::test]
async fn test_streaming_chat_ingress_anthropic_egress_transcodes_to_openai() {
    // A /v1/chat/completions (stream) request routed to an Anthropic upstream
    // must be re-encoded: the client speaks OpenAI, so the gateway must decode
    // the Anthropic SSE and emit OpenAI `chat.completion.chunk` + `[DONE]`.
    let mock_server = wiremock::MockServer::start().await;

    // Anthropic-native upstream SSE stream.
    let upstream_sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude\",\"content\":[]}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/messages"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(upstream_sse),
        )
        .mount(&mock_server)
        .await;

    let app = build_chat_ingress_anthropic_egress_app(mock_server.uri(), "gpt-4o");
    let body = json!({
        "model": "gpt-4o",
        "stream": true,
        "messages": [{"role": "user", "content": "Hi"}]
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);

    // Client must see OpenAI wire format, not Anthropic events.
    assert!(
        body_str.contains("chat.completion.chunk"),
        "expected OpenAI chunks, got: {body_str}"
    );
    assert!(
        body_str.contains("Hello") && body_str.contains(" there"),
        "expected text deltas, got: {body_str}"
    );
    assert!(
        body_str.contains("data: [DONE]"),
        "expected OpenAI terminator, got: {body_str}"
    );
    assert!(
        !body_str.contains("event: message_start"),
        "Anthropic event frames leaked to OpenAI client: {body_str}"
    );
}

#[tokio::test]
async fn test_streaming_messages_ingress_openai_egress_transcodes_to_anthropic() {
    // A /v1/messages (stream) request routed to an OpenAI upstream must be
    // re-encoded into Anthropic SSE events for the client.
    let mock_server = wiremock::MockServer::start().await;

    let upstream_sse = concat!(
        "data: {\"id\":\"chatcmpl_1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl_1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl_1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(upstream_sse),
        )
        .mount(&mock_server)
        .await;

    let app = build_messages_ingress_openai_egress_app(mock_server.uri(), "claude-3-5-sonnet");
    let body = json!({
        "model": "claude-3-5-sonnet",
        "stream": true,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "Hi"}]
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);

    // Client must see Anthropic wire format.
    assert!(
        body_str.contains("event: content_block_delta") && body_str.contains("text_delta"),
        "expected Anthropic content_block_delta, got: {body_str}"
    );
    assert!(
        body_str.contains("Hi") && body_str.contains(" world"),
        "expected text deltas, got: {body_str}"
    );
    assert!(
        body_str.contains("message_stop"),
        "expected Anthropic terminator, got: {body_str}"
    );
    assert!(
        !body_str.contains("chat.completion.chunk"),
        "OpenAI chunk format leaked to Anthropic client: {body_str}"
    );
    assert!(
        !body_str.contains("data: [DONE]"),
        "OpenAI [DONE] terminator leaked to Anthropic client: {body_str}"
    );
}

#[tokio::test]
async fn test_streaming_large_tool_use_anthropic_egress_not_truncated() {
    // Regression guard for the "update_plan" mid-stream truncation seen in
    // production: a very large Anthropic tool_use stream (the model emits a
    // big JSON argument as thousands of `input_json_delta` frames) routed to
    // an OpenAI chat-completions client must be transcoded *in full* — the
    // gateway itself must never truncate a large/long stream. This stands in
    // for the real failing path (ingress OpenAI chat-completions, egress
    // Anthropic Messages, transcode) and proves the truncation is upstream
    // network behavior, not a gateway frame/byte ceiling.
    let mock_server = wiremock::MockServer::start().await;

    // Build > 1500 frames of tool_use input_json_delta plus the surrounding
    // Anthropic envelope. Each delta carries a unique marker so we can assert
    // the *last* frame survived end-to-end (no early cut).
    const DELTA_FRAMES: usize = 1600;
    let mut sse = String::new();
    sse.push_str("event: message_start\n");
    sse.push_str(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_big\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude\",\"content\":[]}}\n\n",
    );
    // Open a tool_use content block (index 0).
    sse.push_str("event: content_block_start\n");
    sse.push_str(
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_big\",\"name\":\"update_plan\",\"input\":{}}}\n\n",
    );
    for i in 0..DELTA_FRAMES {
        // partial_json chunk with a per-frame marker `#<i>;` so the assertions
        // can locate the first and last fragments.
        sse.push_str("event: content_block_delta\n");
        sse.push_str(&format!(
            "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"#{i};\"}}}}\n\n"
        ));
    }
    sse.push_str("event: content_block_stop\n");
    sse.push_str("data: {\"type\":\"content_block_stop\",\"index\":0}\n\n");
    sse.push_str("event: message_delta\n");
    sse.push_str(
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":4096}}\n\n",
    );
    sse.push_str("event: message_stop\n");
    sse.push_str("data: {\"type\":\"message_stop\"}\n\n");

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/messages"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&mock_server)
        .await;

    let app = build_chat_ingress_anthropic_egress_app(mock_server.uri(), "gpt-4o");
    let body = json!({
        "model": "gpt-4o",
        "stream": true,
        "messages": [{"role": "user", "content": "Make a big plan"}]
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // Allow a generous cap: 1600 frames of OpenAI chunks are well under 16 MiB.
    let body_bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);

    // The client speaks OpenAI: tool_call argument deltas, not Anthropic frames.
    assert!(
        body_str.contains("chat.completion.chunk"),
        "expected OpenAI chunks, got first 500 bytes: {}",
        &body_str.chars().take(500).collect::<String>()
    );
    // First fragment marker must be present.
    assert!(
        body_str.contains("#0;"),
        "expected first tool-arg fragment #0; in transcoded output"
    );
    // The LAST fragment marker must survive — this is the core anti-truncation
    // assertion. If the gateway cut the stream early this fails.
    let last_marker = format!("#{};", DELTA_FRAMES - 1);
    assert!(
        body_str.contains(&last_marker),
        "expected LAST tool-arg fragment {last_marker} to survive end-to-end; \
         gateway truncated the large stream"
    );
    // The natural OpenAI terminator must be emitted (clean completion, NOT a
    // gateway-injected error frame).
    assert!(
        body_str.contains("data: [DONE]"),
        "expected clean OpenAI [DONE] terminator on a fully-forwarded large stream"
    );
    assert!(
        !body_str.contains("upstream stream truncated by gateway"),
        "gateway must NOT inject a truncation error on a complete upstream stream: {}",
        &body_str.chars().rev().take(500).collect::<String>()
    );
}

/// Build a chat-ingress / anthropic-egress app with a custom ServerConfig
/// (used by the slow-stream timeout regression test, which needs a short
/// `request_read_timeout_secs` to prove streaming is no longer capped by it).
fn build_chat_ingress_anthropic_egress_app_with_config(
    upstream_url: String,
    model: &str,
    server_config: ServerConfig,
) -> axum::Router {
    let mut routing_table = RoutingTable::new();
    routing_table.insert(
        model.to_string(),
        vec![tiygate_core::RoutingTarget {
            provider_id: "anthropic".to_string(),
            model_id: "claude-3-5-sonnet-20241022".to_string(),
            api_base: upstream_url,
            api_key: "sk-ant-test".to_string(),
            api_protocol: ProtocolEndpoint::new(
                ProtocolSuite::AnthropicMessages,
                "messages",
                "2023-06-01",
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
    ingress::router(config_store, health, &server_config)
}

/// A minimal raw-TCP SSE server that writes an Anthropic-native stream
/// frame-by-frame with a per-frame delay, then closes the connection
/// cleanly (EOF). Returns the bound `http://127.0.0.1:<port>` base URL.
///
/// Unlike wiremock (which buffers the whole body and applies a single
/// `set_delay`), this drip-feeds frames so the *total* stream duration can
/// deliberately exceed the gateway's `request_read_timeout` while each
/// inter-frame gap stays well under the SSE idle window — exactly the
/// "slow but healthy large stream" shape that the production `update_plan`
/// truncation exhibited.
async fn spawn_slow_anthropic_sse_server(
    delta_frames: usize,
    per_frame_delay: std::time::Duration,
) -> String {
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // Accept a single connection (the test makes one request).
        let (mut sock, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => return,
        };
        // Drain the request head (we don't need to parse it — just read
        // until we've consumed the blank line that ends the headers, then
        // ignore the body for this test's purposes).
        let mut buf = [0u8; 4096];
        let _ = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await;

        // Send the HTTP response head with a chunked-free, connection-close
        // streaming body. We use HTTP/1.1 + `Connection: close` and just
        // write the SSE bytes directly, closing the socket to signal EOF.
        let head = "HTTP/1.1 200 OK\r\n\
             content-type: text/event-stream\r\n\
             cache-control: no-cache\r\n\
             connection: close\r\n\r\n";
        if sock.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        let _ = sock.flush().await;

        // message_start + tool_use content_block_start.
        let prelude = "event: message_start\n\
             data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_slow\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude\",\"content\":[]}}\n\n\
             event: content_block_start\n\
             data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_slow\",\"name\":\"update_plan\",\"input\":{}}}\n\n";
        if sock.write_all(prelude.as_bytes()).await.is_err() {
            return;
        }
        let _ = sock.flush().await;

        for i in 0..delta_frames {
            let frame = format!(
                "event: content_block_delta\n\
                 data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"#{i};\"}}}}\n\n"
            );
            if sock.write_all(frame.as_bytes()).await.is_err() {
                return;
            }
            let _ = sock.flush().await;
            tokio::time::sleep(per_frame_delay).await;
        }

        let tail = "event: content_block_stop\n\
             data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
             event: message_delta\n\
             data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":4096}}\n\n\
             event: message_stop\n\
             data: {\"type\":\"message_stop\"}\n\n";
        let _ = sock.write_all(tail.as_bytes()).await;
        let _ = sock.flush().await;
        // Drop closes the socket → clean EOF for the gateway.
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn test_slow_large_stream_not_capped_by_request_read_timeout() {
    // Regression test for the production `update_plan` truncation: streaming
    // upstream requests must NOT be killed by `request_read_timeout` (which
    // bounds reqwest's *entire* request lifecycle, including reading the whole
    // SSE body). We deliberately set `request_read_timeout_secs = 2` and drive
    // a stream whose total duration is ~5s (200 frames × 25ms) — far longer
    // than 2s — while each inter-frame gap (25ms) is tiny vs. the 120s idle
    // window. Before the fix this aborted mid-stream with `operation timed
    // out`; after the fix the whole stream is forwarded and transcoded.
    const DELTA_FRAMES: usize = 200;
    let per_frame = std::time::Duration::from_millis(25); // ~5s total > 2s read timeout
    let upstream = spawn_slow_anthropic_sse_server(DELTA_FRAMES, per_frame).await;

    let mut cfg = ServerConfig::default();
    cfg.require_api_key = false;
    cfg.request_read_timeout_secs = 2; // the old (buggy) cap would fire at 2s
                                       // Keep the SSE idle window at its default 120s so the only thing that
                                       // could (wrongly) kill the stream is the request_read_timeout.
    let app = build_chat_ingress_anthropic_egress_app_with_config(upstream, "gpt-4o", cfg);

    let body = json!({
        "model": "gpt-4o",
        "stream": true,
        "messages": [{"role": "user", "content": "Make a big plan slowly"}]
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);

    // First and LAST fragments must both survive: the stream ran ~5s, well
    // past the 2s request_read_timeout, and must NOT have been truncated.
    assert!(
        body_str.contains("#0;"),
        "expected first fragment; stream was cut before any data?"
    );
    let last_marker = format!("#{};", DELTA_FRAMES - 1);
    assert!(
        body_str.contains(&last_marker),
        "expected LAST fragment {last_marker} to survive a slow (~5s) stream; \
         request_read_timeout wrongly capped the streaming body"
    );
    assert!(
        body_str.contains("data: [DONE]"),
        "expected clean OpenAI terminator on a fully-forwarded slow stream"
    );
    assert!(
        !body_str.contains("operation timed out")
            && !body_str.contains("upstream stream truncated by gateway"),
        "gateway must not inject a timeout/truncation error on a healthy slow stream: {}",
        &body_str.chars().rev().take(400).collect::<String>()
    );
}

/// Build an app whose ingress is OpenAI Responses but whose route targets an
/// OpenAI chat-completions upstream (cross-protocol).
fn build_responses_ingress_openai_egress_app(upstream_url: String, model: &str) -> axum::Router {
    let mut config = ServerConfig::default();
    config.require_api_key = false;
    build_responses_ingress_openai_egress_app_with_config(upstream_url, model, config)
}

/// Build a Responses ingress app with an OpenAI chat-completions egress and
/// a caller-supplied server configuration.
fn build_responses_ingress_openai_egress_app_with_config(
    upstream_url: String,
    model: &str,
    config: ServerConfig,
) -> axum::Router {
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
    ingress::router(config_store, health, &config)
}

/// Build an app whose ingress is Google Gemini but whose route targets an
/// OpenAI chat-completions upstream (cross-protocol).
fn build_gemini_ingress_openai_egress_app(upstream_url: String, model: &str) -> axum::Router {
    let mut config = ServerConfig::default();
    config.require_api_key = false;
    build_gemini_ingress_openai_egress_app_with_config(upstream_url, model, config)
}

/// Build a Gemini ingress app with an OpenAI chat-completions egress and a
/// caller-supplied server configuration.
fn build_gemini_ingress_openai_egress_app_with_config(
    upstream_url: String,
    model: &str,
    config: ServerConfig,
) -> axum::Router {
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
    ingress::router(config_store, health, &config)
}

#[tokio::test]
async fn test_streaming_responses_ingress_openai_egress_transcodes_to_responses() {
    // A /v1/responses (stream) request routed to an OpenAI chat-completions
    // upstream must be re-encoded into Responses SSE events for the client.
    let mock_server = wiremock::MockServer::start().await;
    let upstream_sse = concat!(
        "data: {\"id\":\"chatcmpl_1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl_1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" there\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl_1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(upstream_sse),
        )
        .mount(&mock_server)
        .await;

    let app = build_responses_ingress_openai_egress_app(mock_server.uri(), "gpt-4o");
    let body = json!({
        "model": "gpt-4o",
        "stream": true,
        "input": "Hi"
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);

    // Client must see Responses wire format, not OpenAI chunks.
    assert!(
        body_str.contains("response.output_text.delta"),
        "expected Responses text delta events, got: {body_str}"
    );
    assert!(
        body_str.contains("Hi") && body_str.contains(" there"),
        "expected text deltas, got: {body_str}"
    );
    assert!(
        body_str.contains("data: [DONE]"),
        "expected Responses terminator, got: {body_str}"
    );
    assert!(
        !body_str.contains("chat.completion.chunk"),
        "OpenAI chunk format leaked to Responses client: {body_str}"
    );
}

#[tokio::test]
async fn test_nonstream_responses_ingress_openai_egress_transcodes_to_responses() {
    // Non-streaming /v1/responses routed to an OpenAI upstream: the OpenAI
    // chat.completion body must be re-encoded into a Responses object.
    let mock_server = wiremock::MockServer::start().await;
    let upstream_body = json!({
        "id": "chatcmpl_xyz",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "Hello from OpenAI"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
    });
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(upstream_body))
        .mount(&mock_server)
        .await;

    let app = build_responses_ingress_openai_egress_app(mock_server.uri(), "gpt-4o");
    let body = json!({"model": "gpt-4o", "input": "Hi"});
    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    // The client must see a Responses object, not a chat.completion.
    assert_eq!(v["object"], "response", "expected Responses object: {v}");
    let text = serde_json::to_string(&v).unwrap();
    assert!(
        text.contains("Hello from OpenAI"),
        "expected re-encoded text in Responses body: {text}"
    );
}

#[tokio::test]
async fn test_streaming_gemini_ingress_openai_egress_transcodes_to_gemini() {
    // A Gemini generateContent (stream) request routed to an OpenAI upstream
    // must be re-encoded into Gemini SSE chunks for the client.
    let mock_server = wiremock::MockServer::start().await;
    let upstream_sse = concat!(
        "data: {\"id\":\"chatcmpl_1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl_1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl_1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(upstream_sse),
        )
        .mount(&mock_server)
        .await;

    let app = build_gemini_ingress_openai_egress_app(mock_server.uri(), "gemini-pro");
    let body = json!({
        "_stream": true,
        "contents": [{"role": "user", "parts": [{"text": "Hi"}]}]
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1beta/models/gemini-pro/generateContent")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);

    // Client must see Gemini wire format (candidates/parts), not OpenAI chunks.
    assert!(
        body_str.contains("candidates") && body_str.contains("\"text\""),
        "expected Gemini candidates/parts, got: {body_str}"
    );
    assert!(
        body_str.contains("Hi") && body_str.contains(" world"),
        "expected text deltas, got: {body_str}"
    );
    assert!(
        !body_str.contains("chat.completion.chunk"),
        "OpenAI chunk format leaked to Gemini client: {body_str}"
    );
    assert!(
        !body_str.contains("data: [DONE]"),
        "OpenAI [DONE] terminator leaked to Gemini client: {body_str}"
    );
}

#[tokio::test]
async fn test_nonstream_gemini_ingress_openai_egress_transcodes_to_gemini() {
    // Non-streaming Gemini generateContent routed to an OpenAI upstream: the
    // OpenAI body must be re-encoded into a Gemini generateContent response.
    let mock_server = wiremock::MockServer::start().await;
    let upstream_body = json!({
        "id": "chatcmpl_xyz",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "Hello from OpenAI"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
    });
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(upstream_body))
        .mount(&mock_server)
        .await;

    let app = build_gemini_ingress_openai_egress_app(mock_server.uri(), "gemini-pro");
    let body = json!({
        "contents": [{"role": "user", "parts": [{"text": "Hi"}]}]
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1beta/models/gemini-pro/generateContent")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    // The client must see a Gemini object (candidates[].content.parts).
    let text = serde_json::to_string(&v).unwrap();
    assert!(
        v["candidates"].is_array(),
        "expected Gemini candidates array: {text}"
    );
    assert!(
        text.contains("Hello from OpenAI"),
        "expected re-encoded text in Gemini body: {text}"
    );
}

/// Build a same-protocol Responses app (Responses ingress → Responses upstream).
fn build_responses_same_protocol_app(upstream_url: String, model: &str) -> axum::Router {
    let mut routing_table = RoutingTable::new();
    routing_table.insert(
        model.to_string(),
        vec![tiygate_core::RoutingTarget {
            provider_id: "openai".to_string(),
            model_id: "gpt-4o".to_string(),
            api_base: upstream_url,
            api_key: "sk-test".to_string(),
            api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiResponses, "responses", "v1"),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
            oauth: None,
        }],
    );
    let config_store = ConfigStore::with_routing_table(routing_table);
    let health = Arc::new(HealthRegistry::with_defaults());
    {
        let mut cfg = ServerConfig::default();
        cfg.require_api_key = false;
        ingress::router(config_store, health, &cfg)
    }
}

/// Build a same-protocol Gemini app (Gemini ingress → Gemini upstream).
fn build_gemini_same_protocol_app(upstream_url: String, model: &str) -> axum::Router {
    let mut routing_table = RoutingTable::new();
    routing_table.insert(
        model.to_string(),
        vec![tiygate_core::RoutingTarget {
            provider_id: "google".to_string(),
            model_id: "gemini-pro".to_string(),
            api_base: upstream_url,
            api_key: "sk-gemini".to_string(),
            api_protocol: ProtocolEndpoint::new(
                ProtocolSuite::GoogleGemini,
                "generateContent",
                "v1beta",
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
    {
        let mut cfg = ServerConfig::default();
        cfg.require_api_key = false;
        ingress::router(config_store, health, &cfg)
    }
}

#[tokio::test]
async fn test_nonstream_responses_same_protocol_passthrough() {
    // Same-protocol Responses→Responses: the upstream body is forwarded
    // verbatim (no cross-protocol re-encode) after the refactor.
    let mock_server = wiremock::MockServer::start().await;
    let upstream_body = json!({
        "id": "resp_abc",
        "object": "response",
        "status": "completed",
        "output": [{
            "type": "message",
            "content": [{"type": "output_text", "text": "Same protocol ok"}]
        }]
    });
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/responses"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(upstream_body))
        .mount(&mock_server)
        .await;

    let app = build_responses_same_protocol_app(mock_server.uri(), "gpt-4o");
    let body = json!({"model": "gpt-4o", "input": "Hi"});
    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(
        v["id"], "resp_abc",
        "same-protocol body should pass through: {v}"
    );
    assert!(serde_json::to_string(&v)
        .unwrap()
        .contains("Same protocol ok"));
}

#[tokio::test]
async fn test_responses_namespace_adaptive_flat_retry_and_learning() {
    let mock_server = wiremock::MockServer::start().await;
    let native_body = json!({
        "model": "gpt-4o",
        "input": "Hi",
        "tools": [{
            "type": "namespace",
            "namespace": "collaboration",
            "tools": [{
                "type": "function",
                "name": "spawn_agent",
                "parameters": {"type": "object"}
            }]
        }]
    });

    // The target rejects the native namespace declaration once. The gateway
    // must classify this as a target capability mismatch, flatten and retry
    // the same request, then remember the target-local flat dialect.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/responses"))
        .and(wiremock::matchers::body_json(native_body.clone()))
        .respond_with(wiremock::ResponseTemplate::new(400).set_body_json(json!({
            "error": {
                "type": "invalid_request_error",
                "code": "unknown_parameter",
                "param": "tools[0].type",
                "message": "unknown parameter tools[0].type"
            }
        })))
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/responses"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp-flat",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": "call-1",
                "name": "collaboration__spawn_agent",
                "arguments": "{}"
            }]
        })))
        .with_priority(10)
        .mount(&mock_server)
        .await;

    let app = build_responses_same_protocol_app(mock_server.uri(), "gpt-4o");
    let request_body = serde_json::to_vec(&native_body).unwrap();
    for _ in 0..2 {
        let request = Request::builder()
            .method("POST")
            .uri("/v1/responses")
            .header("content-type", "application/json")
            .body(Body::from(request_body.clone()))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["output"][0]["namespace"], "collaboration");
        assert_eq!(value["output"][0]["name"], "spawn_agent");
    }

    let requests = mock_server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        3,
        "one native rejection + two flat requests"
    );
    let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    let third: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(first["tools"][0]["type"], "namespace");
    assert_eq!(second["tools"][0]["name"], "collaboration__spawn_agent");
    assert_eq!(third["tools"][0]["name"], "collaboration__spawn_agent");
}

#[tokio::test]
async fn test_responses_ordinary_400_does_not_trigger_flat_retry() {
    let mock_server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/responses"))
        .respond_with(wiremock::ResponseTemplate::new(400).set_body_json(json!({
            "error": {
                "type": "invalid_request_error",
                "code": "invalid_request_error",
                "param": "temperature",
                "message": "temperature is not accepted"
            }
        })))
        .mount(&mock_server)
        .await;
    let app = build_responses_same_protocol_app(mock_server.uri(), "gpt-4o");
    let body = json!({
        "model": "gpt-4o",
        "input": "Hi",
        "temperature": 0.2,
        "tools": [{
            "type": "namespace",
            "namespace": "collaboration",
            "tools": [{"type": "function", "name": "spawn_agent"}]
        }]
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(mock_server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_responses_namespace_flat_sse_restores_namespace() {
    let mock_server = wiremock::MockServer::start().await;
    let native_body = json!({
        "model": "gpt-4o",
        "stream": true,
        "input": "Hi",
        "tools": [{
            "type": "namespace",
            "namespace": "collaboration",
            "tools": [{"type": "function", "name": "spawn_agent"}]
        }]
    });
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/responses"))
        .and(wiremock::matchers::body_json(native_body.clone()))
        .respond_with(wiremock::ResponseTemplate::new(400).set_body_json(json!({
            "error": {
                "code": "unsupported_parameter",
                "param": "tools[0].tools",
                "message": "namespace tools are unsupported"
            }
        })))
        .with_priority(1)
        .up_to_n_times(1)
        .mount(&mock_server)
        .await;
    let sse = concat!(
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"name\":\"collaboration__spawn_agent\",\"call_id\":\"call-1\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"name\":\"collaboration__spawn_agent\",\"call_id\":\"call-1\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"function_call\",\"name\":\"collaboration__spawn_agent\"}]}}\n\n"
    );
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/responses"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .with_priority(10)
        .mount(&mock_server)
        .await;

    let app = build_responses_same_protocol_app(mock_server.uri(), "gpt-4o");
    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&native_body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("\"namespace\":\"collaboration\""));
    assert!(text.contains("\"name\":\"spawn_agent\""));
    assert!(!text.contains("collaboration__spawn_agent"));
}

#[tokio::test]
async fn test_responses_tool_mismatch_falls_back_to_next_target() {
    let primary = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(400).set_body_json(json!({
            "error": {
                "code": "unsupported_parameter",
                "param": "tools[0].type",
                "message": "namespace tools unsupported"
            }
        })))
        .mount(&primary)
        .await;
    let secondary = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp-secondary",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "secondary"}]
            }]
        })))
        .mount(&secondary)
        .await;

    let mut routing_table = RoutingTable::new();
    routing_table.insert(
        "gpt-4o".to_string(),
        vec![
            tiygate_core::RoutingTarget {
                provider_id: "primary".to_string(),
                model_id: "gpt-4o".to_string(),
                api_base: primary.uri(),
                api_key: "sk-primary".to_string(),
                api_protocol: ProtocolEndpoint::new(
                    ProtocolSuite::OpenAiResponses,
                    "responses",
                    "v1",
                ),
                account_label: Some("primary".to_string()),
                api_key_override: None,
                api_base_override: None,
                weight: 2.0,
                oauth: None,
            },
            tiygate_core::RoutingTarget {
                provider_id: "secondary".to_string(),
                model_id: "gpt-4o".to_string(),
                api_base: secondary.uri(),
                api_key: "sk-secondary".to_string(),
                api_protocol: ProtocolEndpoint::new(
                    ProtocolSuite::OpenAiResponses,
                    "responses",
                    "v1",
                ),
                account_label: Some("secondary".to_string()),
                api_key_override: None,
                api_base_override: None,
                weight: 1.0,
                oauth: None,
            },
        ],
    );
    let config_store = ConfigStore::with_routing_table(routing_table);
    let health = Arc::new(HealthRegistry::with_defaults());
    let mut cfg = ServerConfig::default();
    cfg.require_api_key = false;
    cfg.routing_strategy = tiygate_core::RoutingStrategyName::Priority;
    let app = ingress::router(config_store, health, &cfg);
    let body = json!({
        "model": "gpt-4o",
        "input": "Hi",
        "tools": [{
            "type": "namespace",
            "namespace": "collaboration",
            "tools": [{"type": "function", "name": "spawn_agent"}]
        }]
    });
    let request = Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body_bytes);
    assert!(text.contains("secondary"));
    assert_eq!(primary.received_requests().await.unwrap().len(), 2);
    assert_eq!(secondary.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_nonstream_gemini_same_protocol_passthrough() {
    // Same-protocol Gemini→Gemini: the upstream body is forwarded verbatim.
    let mock_server = wiremock::MockServer::start().await;
    let upstream_body = json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "Gemini same protocol"}]},
            "finishReason": "STOP"
        }],
        "usageMetadata": {"promptTokenCount": 2, "candidatesTokenCount": 3, "totalTokenCount": 5}
    });
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(
            "/v1beta/models/gemini-pro:generateContent",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(upstream_body))
        .mount(&mock_server)
        .await;

    let app = build_gemini_same_protocol_app(mock_server.uri(), "gemini-pro");
    let body = json!({"contents": [{"role": "user", "parts": [{"text": "Hi"}]}]});
    let request = Request::builder()
        .method("POST")
        .uri("/v1beta/models/gemini-pro/generateContent")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(
        v["candidates"].is_array(),
        "same-protocol Gemini body should pass through: {v}"
    );
    assert!(serde_json::to_string(&v)
        .unwrap()
        .contains("Gemini same protocol"));
}

// ---------------------------------------------------------------------------
// require_api_key toggle — verify the auth gate rejects anonymous
// requests when enabled and passes them through when disabled.
//
// These tests use the legacy in-memory path (no DbConfigStore), so
// `find_api_key_by_secret` always returns `Ok(None)` → the outcome
// is `UnknownCredential` for any credential and `NoCredential` when
// no header is supplied.
// ---------------------------------------------------------------------------

fn build_app_with_require_api_key(
    upstream_url: String,
    model: &str,
    require_api_key: bool,
) -> axum::Router {
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
    let mut cfg = ServerConfig::default();
    cfg.require_api_key = require_api_key;
    ingress::router(config_store, health, &cfg)
}

#[tokio::test]
async fn test_require_api_key_rejects_missing_credential() {
    let mock_server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&mock_server)
        .await;

    let app = build_app_with_require_api_key(mock_server.uri(), "gpt-4o", true);

    // No Authorization header → 401
    let body = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]});
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_require_api_key_rejects_unknown_credential() {
    let mock_server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&mock_server)
        .await;

    let app = build_app_with_require_api_key(mock_server.uri(), "gpt-4o", true);

    // Unknown credential → 401 (in-memory path always returns None)
    let body = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]});
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", "Bearer sk-unknown")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_require_api_key_disabled_allows_anonymous() {
    let mock_server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .mount(&mock_server)
        .await;

    let app = build_app_with_require_api_key(mock_server.uri(), "gpt-4o", false);

    // No credential, but require_api_key=false → upstream is hit, 200
    let body = json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]});
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_api_key_model_access_rejects_before_upstream() {
    let mock_server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "model": "gpt-4o",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })))
        .expect(0)
        .mount(&mock_server)
        .await;

    let pool = db::open_pool("sqlite::memory:").await.expect("pool");
    db::run_migrations(&pool).await.expect("migrations");
    let store = Arc::new(DbConfigStore::new(pool, None));
    store.refresh().await.expect("refresh");
    store
        .upsert_provider(
            "openai",
            "OpenAI",
            "openai",
            &mock_server.uri(),
            "",
            Some("sk-upstream"),
            AuthMode::ApiKey,
            None,
            json!({}),
            true,
        )
        .await
        .expect("provider");
    store
        .upsert_route(
            "route-blocked",
            "blocked-model",
            &[RouteTarget {
                provider_id: "openai".to_string(),
                model_id: "gpt-4o".to_string(),
                weight: 1.0,
                enabled: true,
                account_label: None,
                api_key_override: None,
                api_base_override: None,
            }],
            None,
            None,
            true,
        )
        .await
        .expect("route");
    store
        .create_api_key(
            "scoped",
            "tg-scoped-chat",
            json!({"requests_per_minute": 1}),
            Some(vec!["allowed-model".to_string()]),
        )
        .await
        .expect("create key");
    let cfg = ServerConfig {
        require_api_key: true,
        ..Default::default()
    };
    let app = ingress::router_with_telemetry_full(
        store.config_store(),
        Arc::new(HealthRegistry::with_defaults()),
        &cfg,
        Arc::new(tiygate_server::telemetry::ChannelTelemetryBus::spawn(
            Arc::new(tiygate_store::log_sink::stdout::StdoutSink::new()),
            64,
        )),
        Some(InMemoryQuota::new()),
        None,
        Some(store),
        None,
    );

    let body = json!({"model": "blocked-model", "messages": [{"role": "user", "content": "hi"}]});
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("authorization", "Bearer tg-scoped-chat")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .expect("body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(value["error"]["type"], json!("permission_error"));
    assert_eq!(value["error"]["code"], json!("model_access_denied"));

    // The rejected request still consumes request quota, so repeated denied
    // traffic cannot bypass the API key's ingress rate limit.
    let body =
        json!({"model": "blocked-model", "messages": [{"role": "user", "content": "again"}]});
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("authorization", "Bearer tg-scoped-chat")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

/// Regression: when the upstream does not return response headers
/// within the TTFB timeout, the streaming branch must return 504
/// (Gateway Timeout) with a `DeadlineExceeded` error class instead
/// of hanging indefinitely. Before the fix, the streaming branch had
/// no TTFB protection — `client.execute().await` could block forever.
#[tokio::test]
async fn test_streaming_ttfb_timeout_returns_504() {
    let mock_server = wiremock::MockServer::start().await;

    // The upstream delays 3s before sending response headers. With
    // `upstream_ttfb_timeout_secs = 1`, the gateway should time out
    // after ~1s and return 504.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_delay(std::time::Duration::from_secs(3))
                .set_body_string("data: {\"choices\":[]}\n\ndata: [DONE]\n\n"),
        )
        .mount(&mock_server)
        .await;

    let mut server_config = ServerConfig::default();
    server_config.require_api_key = false;
    server_config.upstream_ttfb_timeout_secs = 1;
    let app = build_test_app_with_config(mock_server.uri(), "gpt-4o", server_config);

    let body = json!({
        "model": "gpt-4o",
        "stream": true,
        "messages": [{"role": "user", "content": "Hi"}]
    });

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let start = std::time::Instant::now();
    let response = app.oneshot(request).await.unwrap();
    let elapsed = start.elapsed();

    // Must return 504, not 200 or 500.
    assert_eq!(
        response.status(),
        StatusCode::GATEWAY_TIMEOUT,
        "expected 504 Gateway Timeout, got {}",
        response.status()
    );

    // Must time out after ~1s, not wait for the 3s upstream delay.
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "TTFB timeout should fire after ~1s, took {:?}",
        elapsed
    );

    // The error body should contain a server_error type (OpenAI
    // protocol-native mapping for DeadlineExceeded).
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);
    assert!(
        body_str.contains("error"),
        "expected error body, got: {body_str}"
    );
}

/// Regression: when `upstream_ttfb_timeout_secs = 0` (disabled), the
/// streaming branch must behave as before — no TTFB timeout is applied.
/// This ensures backward compatibility when the setting is not configured.
#[tokio::test]
async fn test_streaming_ttfb_timeout_disabled_when_zero() {
    let mock_server = wiremock::MockServer::start().await;

    // A small delay that would NOT trigger a 1s TTFB timeout, but
    // confirms the path works when ttfb_timeout is 0 (disabled).
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n",
                ),
        )
        .mount(&mock_server)
        .await;

    let mut server_config = ServerConfig::default();
    server_config.require_api_key = false;
    server_config.upstream_ttfb_timeout_secs = 0; // disabled
    let app = build_test_app_with_config(mock_server.uri(), "gpt-4o", server_config);

    let body = json!({
        "model": "gpt-4o",
        "stream": true,
        "messages": [{"role": "user", "content": "Hi"}]
    });

    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);
    assert!(
        body_str.contains("data: [DONE]"),
        "expected stream to complete normally, got: {body_str}"
    );
}
