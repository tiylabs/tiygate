//! Wiremock-based integration tests for the OpenAI Images endpoints.
//!
//! Validates that the gateway correctly forwards requests to upstream
//! providers in raw-body passthrough mode for both:
//!   * `/v1/images/generations` (JSON body)
//!   * `/v1/images/edits` (multipart/form-data body)
//!
//! Covers non-streaming, streaming (SSE verbatim), model override,
//! multipart raw-bytes forwarding, and error response passthrough.

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

/// Build a test app with a single OpenAI-compatible images route to a
/// wiremock upstream. The virtual model is mapped to a real upstream
/// model so model override can be tested.
fn build_images_test_app(
    upstream_url: String,
    virtual_model: &str,
    upstream_model: &str,
) -> axum::Router {
    let mut routing_table = RoutingTable::new();
    routing_table.insert(
        virtual_model.to_string(),
        vec![tiygate_core::RoutingTarget {
            provider_id: "openai".to_string(),
            model_id: upstream_model.to_string(),
            api_base: upstream_url,
            api_key: "sk-test".to_string(),
            api_protocol: ProtocolEndpoint::new(
                ProtocolSuite::OpenAiCompatible,
                "images-generations",
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
    // Tests use the legacy in-memory path (no DbConfigStore), so
    // api key resolution always returns anonymous. Disable the
    // require_api_key guard to keep these tests focused on routing
    // and passthrough behaviour.
    server_config.require_api_key = false;
    ingress::router(config_store, health, &server_config)
}

// ---------------------------------------------------------------------------
// /v1/images/generations — non-streaming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_images_generations_nonstream_passthrough() {
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/images/generations"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer sk-test",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "created": 1700000000,
            "data": [{"b64_json": "iVBORw0KGgo="}],
            "usage": {"prompt_tokens": 10, "total_tokens": 15}
        })))
        .mount(&mock_server)
        .await;

    let app = build_images_test_app(mock_server.uri(), "gpt-image-1", "gpt-image-1");

    let body = json!({
        "model": "gpt-image-1",
        "prompt": "A cute sea otter",
        "n": 1,
        "size": "1024x1024"
    });

    let request = Request::builder()
        .method("POST")
        .uri("/v1/images/generations")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body_json["data"][0]["b64_json"], json!("iVBORw0KGgo="));
    assert_eq!(body_json["usage"]["prompt_tokens"], json!(10));
}

// ---------------------------------------------------------------------------
// /v1/images/generations — model override
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_images_generations_model_override() {
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/images/generations"))
        .and(wiremock::matchers::body_partial_json(
            json!({"model": "gpt-image-1"}),
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "created": 1700000000,
            "data": [{"b64_json": "abc"}]
        })))
        .mount(&mock_server)
        .await;

    // Client sends virtual model "virtual-image", which is mapped to
    // upstream "gpt-image-1" in the routing table.
    let app = build_images_test_app(mock_server.uri(), "virtual-image", "gpt-image-1");

    let body = json!({
        "model": "virtual-image",
        "prompt": "A cute sea otter"
    });

    let request = Request::builder()
        .method("POST")
        .uri("/v1/images/generations")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// /v1/images/generations — streaming SSE verbatim passthrough
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_images_generations_streaming_passthrough() {
    let mock_server = wiremock::MockServer::start().await;

    let sse_body = concat!(
        "data: {\"type\":\"image_generation.partial_image\",\"b64_json\":\"abc\"}\n\n",
        "data: {\"type\":\"image_generation.partial_image\",\"b64_json\":\"def\"}\n\n",
        "data: {\"type\":\"image_generation.completed\",\"id\":\"img-123\"}\n\n"
    );

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/images/generations"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body.as_bytes(), "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let app = build_images_test_app(mock_server.uri(), "gpt-image-1", "gpt-image-1");

    let body = json!({
        "model": "gpt-image-1",
        "prompt": "A cute sea otter",
        "stream": true
    });

    let request = Request::builder()
        .method("POST")
        .uri("/v1/images/generations")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);
    assert!(body_str.contains("image_generation.partial_image"));
    assert!(body_str.contains("image_generation.completed"));
}

// ---------------------------------------------------------------------------
// /v1/images/generations — 429 error passthrough with Retry-After
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_images_generations_error_passthrough() {
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(
            wiremock::ResponseTemplate::new(429)
                .insert_header("retry-after", "60")
                .set_body_json(json!({
                    "error": {"message": "rate limited", "code": "rate_limit"}
                })),
        )
        .mount(&mock_server)
        .await;

    let app = build_images_test_app(mock_server.uri(), "gpt-image-1", "gpt-image-1");

    let body = json!({
        "model": "gpt-image-1",
        "prompt": "test"
    });

    let request = Request::builder()
        .method("POST")
        .uri("/v1/images/generations")
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
        Some("60")
    );
}

// ---------------------------------------------------------------------------
// /v1/images/edits — multipart non-streaming passthrough
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_images_edits_multipart_passthrough() {
    let mock_server = wiremock::MockServer::start().await;

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/images/edits"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer sk-test",
        ))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
            "created": 1700000000,
            "data": [{"b64_json": "edited_image_data"}]
        })))
        .mount(&mock_server)
        .await;

    let app = build_images_test_app(mock_server.uri(), "gpt-image-1", "gpt-image-1");

    // Build a minimal multipart/form-data body.
    let boundary = "----test_boundary_12345";
    let multipart_body = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"model\"\r\n\r\n\
         gpt-image-1\r\n\
         --{boundary}\r\n\
         Content-Disposition: form-data; name=\"prompt\"\r\n\r\n\
         Add sunglasses\r\n\
         --{boundary}\r\n\
         Content-Disposition: form-data; name=\"image\"; filename=\"source.png\"\r\n\
         Content-Type: image/png\r\n\r\n\
         PNGDATA\r\n\
         --{boundary}--\r\n"
    );

    let request = Request::builder()
        .method("POST")
        .uri("/v1/images/edits")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(multipart_body.into_bytes()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body_json["data"][0]["b64_json"], json!("edited_image_data"));
}

// ---------------------------------------------------------------------------
// /v1/images/edits — streaming SSE verbatim passthrough
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_images_edits_streaming_passthrough() {
    let mock_server = wiremock::MockServer::start().await;

    let sse_body = concat!(
        "data: {\"type\":\"image_edit.partial_image\",\"b64_json\":\"abc\"}\n\n",
        "data: {\"type\":\"image_edit.completed\",\"id\":\"edit-456\"}\n\n"
    );

    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/images/edits"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(sse_body.as_bytes(), "text/event-stream"),
        )
        .mount(&mock_server)
        .await;

    let app = build_images_test_app(mock_server.uri(), "gpt-image-1", "gpt-image-1");

    let boundary = "----test_boundary_stream";
    let multipart_body = format!(
        "--{boundary}\r\n\
         Content-Disposition: form-data; name=\"model\"\r\n\r\n\
         gpt-image-1\r\n\
         --{boundary}\r\n\
         Content-Disposition: form-data; name=\"prompt\"\r\n\r\n\
         Add sunglasses\r\n\
         --{boundary}\r\n\
         Content-Disposition: form-data; name=\"stream\"\r\n\r\n\
         true\r\n\
         --{boundary}\r\n\
         Content-Disposition: form-data; name=\"image\"; filename=\"source.png\"\r\n\
         Content-Type: image/png\r\n\r\n\
         PNGDATA\r\n\
         --{boundary}--\r\n"
    );

    let request = Request::builder()
        .method("POST")
        .uri("/v1/images/edits")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(multipart_body.into_bytes()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body_bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body_bytes);
    assert!(body_str.contains("image_edit.partial_image"));
    assert!(body_str.contains("image_edit.completed"));
}

// ---------------------------------------------------------------------------
// /v1/images/generations — 404 for unknown model
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_images_generations_unknown_model_404() {
    let app = build_images_test_app("http://unused".to_string(), "gpt-image-1", "gpt-image-1");

    let body = json!({"model": "nonexistent", "prompt": "test"});

    let request = Request::builder()
        .method("POST")
        .uri("/v1/images/generations")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
