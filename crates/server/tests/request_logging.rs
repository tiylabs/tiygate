#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::field_reassign_with_default
)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tokio::sync::Mutex;
use tower::ServiceExt;

use tiygate_core::{
    ExchangeCapture, HealthRegistry, PipelineEvent, RequestErrorClass, RequestEvent, RequestStatus,
    TelemetryBus,
};
use tiygate_server::config::ServerConfig;
use tiygate_server::ingress;
use tiygate_store::config::ConfigStore;

#[derive(Default)]
struct RecordingTelemetry {
    requests: Mutex<Vec<RequestEvent>>,
}

#[async_trait]
impl TelemetryBus for RecordingTelemetry {
    async fn send(&self, _event: PipelineEvent) {}

    async fn send_request_event(&self, event: RequestEvent) {
        self.requests.lock().await.push(event);
    }

    async fn send_capture(&self, _capture: ExchangeCapture) {}
}

#[tokio::test]
async fn body_limit_failure_emits_request_log_event() {
    let telemetry = Arc::new(RecordingTelemetry::default());
    let mut server_config = ServerConfig::default();
    server_config.require_api_key = false;
    server_config.max_request_body_bytes = 1;
    server_config.max_multimodal_body_bytes = 1024 * 1024;

    let app = ingress::router_with_telemetry(
        ConfigStore::default(),
        Arc::new(HealthRegistry::with_defaults()),
        &server_config,
        telemetry.clone(),
        None,
        None,
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"gpt-4o","messages":[]}"#))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    for _ in 0..20 {
        if !telemetry.requests.lock().await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let events = telemetry.requests.lock().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].status, RequestStatus::Failed);
    assert_eq!(events[0].error_class, Some(RequestErrorClass::BadRequest));
    assert_eq!(
        events[0].http_status,
        Some(StatusCode::PAYLOAD_TOO_LARGE.as_u16())
    );
}
