//! Wiremock-based provider integration tests.
//!
//! Validates that the gateway correctly forwards requests to upstream
//! providers, decodes the response, and applies auth headers / passthrough
//! behavior. Uses `wiremock` to stand in for the upstream API.
//!
//! Phase 1 acceptance criterion: 3 providers × happy-path coverage.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tiygate_core::{HealthRegistry, ProtocolEndpoint, ProtocolSuite, RoutingTable};
use tiygate_server::config::ServerConfig;
use tiygate_server::ingress;
use tiygate_store::config::ConfigStore;
use tower::ServiceExt;

/// Build a test app with a single OpenAI-compatible route to a wiremock upstream.
fn build_openai_test_app(upstream_url: String, model: &str) -> axum::Router {
    build_test_app_with_config(upstream_url, model, ServerConfig::default())
}

/// Build a test app with a single OpenAI-compatible route to a wiremock upstream
/// and a custom server config (used by the concurrency overflow test).
fn build_test_app_with_config(
    upstream_url: String,
    model: &str,
    server_config: ServerConfig,
) -> axum::Router {
    let mut routing_table = RoutingTable::new();
    routing_table.routes.insert(
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
        }],
    );

    let config_store = ConfigStore::with_routing_table(routing_table);
    let health = Arc::new(HealthRegistry::with_defaults());
    ingress::router(config_store, health, &server_config)
}

/// Build a test app with a single Anthropic Messages route to a wiremock upstream.
fn build_anthropic_test_app(upstream_url: String, model: &str) -> axum::Router {
    let mut routing_table = RoutingTable::new();
    routing_table.routes.insert(
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
        }],
    );

    let config_store = ConfigStore::with_routing_table(routing_table);
    let health = Arc::new(HealthRegistry::with_defaults());
    let server_config = ServerConfig::default();
    ingress::router(config_store, health, &server_config)
}

/// Build a test app for a generic OpenAI-compatible provider (custom base URL + key).
fn build_openai_compatible_test_app(
    upstream_url: String,
    model: &str,
    api_key: &str,
) -> axum::Router {
    let mut routing_table = RoutingTable::new();
    routing_table.routes.insert(
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
        }],
    );

    let config_store = ConfigStore::with_routing_table(routing_table);
    let health = Arc::new(HealthRegistry::with_defaults());
    let server_config = ServerConfig::default();
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
    routing_table.routes.insert(
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
            },
        ],
    );
    let config_store = ConfigStore::with_routing_table(routing_table);
    let health = Arc::new(HealthRegistry::with_defaults());
    let app = ingress::router(config_store, health, &ServerConfig::default());

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
    // Streaming path: the gateway must forward a chunked OpenAI-style
    // SSE response to the downstream and append the protocol-native
    // end frame (`data: [DONE]`) when the upstream closes naturally.
    // This exercises the new `drive_upstream_stream` bridge end-to-end
    // — keepalive is on a longer cadence (default 30s) and the test
    // body is small enough that the stream closes before any
    // keepalive is emitted, so we can assert on the done-frame
    // passthrough only.
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
                .set_body_string("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n"),
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
    // And the protocol-native end frame the gateway emits on natural
    // upstream close must also be present.
    assert!(
        body_str.contains("data: [DONE]"),
        "expected protocol-native end frame, got: {body_str}"
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
async fn test_streaming_forwards_sse_bytes_verbatim_no_double_data() {
    // Regression: the gateway used to re-wrap each upstream SSE frame in
    // an axum `Event::default().data(...)`, producing a corrupt
    // double-`data:` prefix (`data: data: {...}`) and a duplicate
    // terminal frame. The fix forwards upstream bytes verbatim and
    // dedups the gateway end frame against the upstream's own
    // terminator. This test asserts the exact wire bytes.
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

    // The bytes must be forwarded verbatim — identical to what the
    // upstream sent (gateway adds nothing because the upstream already
    // terminated with `data: [DONE]`).
    assert_eq!(
        body_str, sse_body,
        "gateway must forward upstream SSE bytes verbatim, got: {body_str:?}"
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
async fn test_streaming_appends_done_when_upstream_omits_it() {
    // When the upstream closes without its own terminal frame, the
    // gateway must append exactly one protocol-native `data: [DONE]`.
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

    let expected = format!("{sse_body}data: [DONE]\n\n");
    assert_eq!(
        body_str, expected,
        "gateway must append exactly one [DONE], got: {body_str:?}"
    );
    assert!(
        !body_str.contains("data: data:"),
        "double data: prefix leaked: {body_str:?}"
    );
}

