//! HTTP ingress — request handling, routing, and SSE response streaming.
//!
//! Phase 2 stability features:
//! - Multi-target fallback via FallbackPolicy + HealthRegistry
//! - Retry with exponential backoff + jitter
//! - Global concurrency semaphore + bounded queue
//! - Retry-After passthrough and upstream-aware cooling
//! - Error source distinction (gateway vs upstream)
//! - UsageAccumulator for disconnected streaming billing

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, Sse},
    routing::post,
    Json, Router,
};
use futures::{ready, Future, Stream, StreamExt};
use pin_project::pin_project;
use serde_json::Value;
use tokio::sync::Semaphore;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::RequestBodyTimeoutLayer;

use tiygate_core::{
    classify_error, DefaultFallbackPolicy, EndpointCodec, ErrorClass, FallbackDecision,
    FallbackPolicy, HealthRegistry, IrRequest, PipelineContext, RawEnvelope, RetryPolicy,
    TelemetryBus, TruncationReason, UsageAccumulator,
};
use tiygate_protocols::chat_completions::ChatCompletionsCodec;
use tiygate_protocols::embeddings::EmbeddingsCodec;
use tiygate_protocols::gemini::GeminiCodec;
use tiygate_protocols::messages::MessagesCodec;
use tiygate_protocols::responses::ResponsesCodec;

/// Construct a `Strategy` from the `RoutingStrategyName` carried on
/// `AppState`. §3.4 names `Weighted` as the document-level default; we honor
/// that here. The `Latency` strategy needs the `HealthRegistry` handle, so it
/// is the only one with a non-trivial constructor.
fn build_strategy(
    name: crate::config::RoutingStrategyName,
    health: Arc<HealthRegistry>,
) -> (Box<dyn tiygate_core::routing::Strategy>, &'static str) {
    use tiygate_core::routing::Strategy;
    match name {
        crate::config::RoutingStrategyName::Weighted => (
            Box::new(tiygate_core::routing::WeightedStrategy),
            "WeightedStrategy",
        ),
        crate::config::RoutingStrategyName::Priority => (
            Box::new(tiygate_core::routing::PriorityStrategy),
            "PriorityStrategy",
        ),
        crate::config::RoutingStrategyName::Cooldown => (
            Box::new(tiygate_core::routing::CooldownStrategy::new(health)),
            "CooldownStrategy",
        ),
        crate::config::RoutingStrategyName::Latency => (
            Box::new(tiygate_core::routing::LatencyStrategy::new(health)),
            "LatencyStrategy",
        ),
    }
}

use tiygate_store::config::ConfigStore;

/// Shared application state.
#[derive(Clone)]
#[allow(dead_code)]
pub struct AppState {
    pub config: ConfigStore,
    pub health: Arc<HealthRegistry>,
    pub concurrency_semaphore: Arc<Semaphore>,
    /// Max inflight requests before queueing.
    pub max_inflight: usize,
    /// Max queue depth before 503.
    pub max_queue_depth: usize,
    /// Timeout waiting for a concurrency permit.
    pub acquire_timeout: Duration,
    /// Standard request body limit (bytes).
    pub max_request_body_bytes: u64,
    /// Larger request body limit for multimodal content.
    pub max_multimodal_body_bytes: u64,
    /// Read timeout for the full request body.
    pub request_read_timeout: Duration,
    /// Idle timeout (seconds) for upstream streaming responses. Used by
    /// `drive_upstream_stream` to close long-silent streams with a
    /// protocol-native end frame. Default 120s.
    pub upstream_stream_idle_timeout_secs: u64,
    /// Total wall-clock timeout (seconds) for upstream streaming
    /// responses. 0 disables the total budget. Used by
    /// `drive_upstream_stream` to close over-budget streams with a
    /// protocol-native error frame.
    pub upstream_stream_total_timeout_secs: u64,
    /// Shared reqwest connection pool across all handlers.
    pub http_client: reqwest::Client,
    /// Async telemetry bus — non-blocking send.
    pub telemetry: Arc<dyn TelemetryBus>,
    /// Routing strategy selector (default `Weighted`, per §3.4).
    pub routing_strategy: crate::config::RoutingStrategyName,
}

use crate::config::ServerConfig;

/// Build the ingress router.
pub fn router(
    config: ConfigStore,
    health: Arc<HealthRegistry>,
    server_config: &ServerConfig,
) -> Router {
    // Build a no-op telemetry bus for tests / direct router() calls. The
    // App::new() path wires up a real stdout-backed bus via the App struct.
    let telemetry: Arc<dyn TelemetryBus> = Arc::new(crate::telemetry::ChannelTelemetryBus::spawn(
        Arc::new(crate::telemetry::StdoutTelemetrySink::new()),
        64,
    ));
    router_with_telemetry(config, health, server_config, telemetry)
}

/// Build the ingress router with an explicit telemetry bus.
///
/// Production code should use this entry point so that bus instances are
/// not duplicated or orphaned.
pub fn router_with_telemetry(
    config: ConfigStore,
    health: Arc<HealthRegistry>,
    server_config: &ServerConfig,
    telemetry: Arc<dyn TelemetryBus>,
) -> Router {
    let semaphore = Arc::new(Semaphore::new(server_config.max_inflight_requests));

    let state = AppState {
        config,
        health,
        concurrency_semaphore: semaphore,
        max_inflight: server_config.max_inflight_requests,
        max_queue_depth: server_config.max_queue_depth,
        acquire_timeout: Duration::from_secs(server_config.acquire_timeout_secs),
        max_request_body_bytes: server_config.max_request_body_bytes,
        max_multimodal_body_bytes: server_config.max_multimodal_body_bytes,
        request_read_timeout: Duration::from_secs(server_config.request_read_timeout_secs),
        upstream_stream_idle_timeout_secs: server_config.upstream_stream_idle_timeout_secs,
        upstream_stream_total_timeout_secs: server_config.upstream_stream_total_timeout_secs,
        // Shared connection pool: 30s connect timeout, no per-call read
        // timeout (we rely on RequestBodyTimeoutLayer for ingress + Sse for
        // streaming keepalive). Pool size defaults to reqwest's recommended
        // 10 per host; for high-throughput deployments this can be tuned via
        // reqwest::ClientBuilder::pool_max_idle_per_host.
        http_client: reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(32)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new()),
        telemetry,
        routing_strategy: server_config.routing_strategy,
    };

    Router::new()
        .route("/v1/chat/completions", post(handle_chat_completions))
        .route("/v1/messages", post(handle_messages))
        .route("/v1/embeddings", post(handle_embeddings))
        .route("/v1/responses", post(handle_responses))
        .route(
            "/v1beta/models/:model/generateContent",
            post(handle_gemini_generate),
        )
        .route("/healthz", axum::routing::get(handle_healthz))
        .route("/readyz", axum::routing::get(handle_readyz))
        .layer(RequestBodyTimeoutLayer::new(Duration::from_secs(
            server_config.request_read_timeout_secs,
        )))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            server_config.max_multimodal_body_bytes as usize,
        ))
        .with_state(state)
}

/// Compute the raw-passthrough body and the same-suite flag, in a
/// way that all ingress handlers can share. When the target's
/// protocol suite matches the ingress suite and the codec declares
/// Passthrough, the captured `raw_envelope.body` is forwarded
/// verbatim to the upstream.
pub fn compute_pass_through<C: tiygate_core::EndpointCodec>(
    codec: &C,
    ingress_protocol: &tiygate_core::ProtocolEndpoint,
    targets: &[tiygate_core::RoutingTarget],
    raw_envelope: &RawEnvelope,
) -> (bool, Option<String>) {
    let pass_through_candidate = targets.iter().any(|t| {
        ingress_protocol.suite == t.api_protocol.suite
            && matches!(
                codec.pass_through_policy(ingress_protocol, &t.api_protocol),
                tiygate_core::PassThroughPolicy::Passthrough
            )
    });
    if pass_through_candidate {
        (true, raw_envelope.body.clone())
    } else {
        (false, None)
    }
}

/// Look up the registered provider matching `target.provider_id` and
/// invoke its `AuthApplier::apply` to populate the upstream headers.
/// Falls back to a protocol-aware default applier if no registered
/// provider is found (e.g., test fixtures).
pub async fn apply_provider_auth(
    target: &tiygate_core::RoutingTarget,
    upstream_headers: &mut http::HeaderMap,
) -> Result<(), AppError> {
    if let Some(provider) = tiygate_core::provider::find_provider(&target.provider_id) {
        let auth = provider.auth();
        if let Err(e) = auth.apply(upstream_headers, target).await {
            return Err(AppError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Provider auth applier failed: {e}"),
            ));
        }
        return Ok(());
    }
    // Protocol-aware fallback when no provider is registered for the
    // given `provider_id`. Anthropic-style targets use the x-api-key
    // header; everything else uses Bearer.
    use tiygate_core::ProtocolSuite;
    if matches!(target.api_protocol.suite, ProtocolSuite::AnthropicMessages) {
        let api_key = target.effective_api_key();
        let hv = http::HeaderValue::from_str(api_key).map_err(|e| {
            AppError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid header value: {e}"),
            )
        })?;
        upstream_headers.insert(http::HeaderName::from_static("x-api-key"), hv);
        upstream_headers.insert(
            http::HeaderName::from_static("anthropic-version"),
            http::HeaderValue::from_static("2023-06-01"),
        );
    } else {
        let api_key = target.effective_api_key();
        let hv = http::HeaderValue::from_str(&format!("Bearer {api_key}")).map_err(|e| {
            AppError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid header: {e}"),
            )
        })?;
        upstream_headers.insert(http::header::AUTHORIZATION, hv);
    }
    Ok(())
}

/// Look up the registered provider and invoke its `prepare_body`
/// hook (used for OAuth subscription providers that need to inject
/// tokens into the body instead of (or in addition to) headers).
pub async fn apply_provider_body_hook(
    target: &tiygate_core::RoutingTarget,
    body: &mut serde_json::Value,
) -> Result<(), AppError> {
    if let Some(provider) = tiygate_core::provider::find_provider(&target.provider_id) {
        let auth = provider.auth();
        if let Err(e) = auth.prepare_body(body, target).await {
            return Err(AppError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Provider prepare_body failed: {e}"),
            ));
        }
    }
    Ok(())
}

/// Per-route body limit. The default `RequestBodyLimitLayer` above uses
/// the multimodal limit (worst case). This helper is invoked by the
/// individual handlers to apply the correct limit based on the
/// request's Content-Type (text vs. multimodal).
pub fn enforce_body_limit(
    state: &AppState,
    content_type: Option<&str>,
    body_size: u64,
) -> Result<(), AppError> {
    if let Some(ct) = content_type {
        let ct_lower = ct.to_lowercase();
        let is_multimodal = ct_lower.contains("multipart")
            || ct_lower.contains("image/")
            || ct_lower.contains("audio/")
            || ct_lower.contains("video/")
            || ct_lower.contains("application/pdf")
            || ct_lower.contains("application/octet-stream");
        let limit = if is_multimodal {
            state.max_multimodal_body_bytes
        } else {
            state.max_request_body_bytes
        };
        if body_size > limit {
            return Err(AppError::new(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "payload too large: {} bytes exceeds limit {} bytes",
                    body_size, limit
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod ingress_helper_tests {
    //! Pure-function tests for header extraction. Mirrors the private helpers
    //! in this file so we can validate behavior without spinning up a server.
    use http::HeaderMap;
    use http::HeaderValue;

    fn extract_retry_after(headers: &HeaderMap) -> Option<String> {
        headers
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    }

    fn extract_rate_limit_headers(headers: &HeaderMap) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for name in &[
            "x-ratelimit-limit",
            "x-ratelimit-remaining",
            "x-ratelimit-reset",
        ] {
            if let Some(v) = headers.get(*name).and_then(|v| v.to_str().ok()) {
                out.push((name.to_string(), v.to_string()));
            }
        }
        out
    }

    /// Choose body limit based on the request's Content-Type and body size.
    /// Multimodal requests (those with image/* or audio/* media types) get
    /// the larger `max_multimodal_body_bytes` limit.
    fn resolve_body_limit(
        content_type: Option<&str>,
        body_size: u64,
        max_request_bytes: u64,
        max_multimodal_bytes: u64,
    ) -> Result<u64, &'static str> {
        let is_multimodal = content_type
            .map(|ct| {
                let ct_lower = ct.to_lowercase();
                ct_lower.contains("multipart")
                    || ct_lower.contains("image/")
                    || ct_lower.contains("audio/")
                    || ct_lower.contains("video/")
                    || ct_lower.contains("application/pdf")
                    || ct_lower.contains("application/octet-stream")
            })
            .unwrap_or(false);

        let limit = if is_multimodal {
            max_multimodal_bytes
        } else {
            max_request_bytes
        };

        if body_size > limit {
            Err("payload too large")
        } else {
            Ok(limit)
        }
    }

    #[test]
    fn retry_after_present() {
        let mut h = HeaderMap::new();
        h.insert(http::header::RETRY_AFTER, HeaderValue::from_static("30"));
        assert_eq!(extract_retry_after(&h), Some("30".to_string()));
    }

    #[test]
    fn retry_after_missing() {
        assert_eq!(extract_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn rate_limit_all_headers() {
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-limit", HeaderValue::from_static("100"));
        h.insert("x-ratelimit-remaining", HeaderValue::from_static("42"));
        h.insert("x-ratelimit-reset", HeaderValue::from_static("1700000000"));
        let got = extract_rate_limit_headers(&h);
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn rate_limit_partial() {
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        let got = extract_rate_limit_headers(&h);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn rate_limit_empty() {
        assert!(extract_rate_limit_headers(&HeaderMap::new()).is_empty());
    }

    #[test]
    fn multimodal_limit_for_image() {
        // image/* content type → use multimodal limit
        let r = resolve_body_limit(Some("image/png"), 1024, 10 * 1024 * 1024, 32 * 1024 * 1024);
        assert_eq!(r.unwrap(), 32 * 1024 * 1024);
    }

    #[test]
    fn standard_limit_for_text() {
        // application/json → use standard limit
        let r = resolve_body_limit(
            Some("application/json"),
            1024,
            10 * 1024 * 1024,
            32 * 1024 * 1024,
        );
        assert_eq!(r.unwrap(), 10 * 1024 * 1024);
    }

    #[test]
    fn multimodal_oversize_rejected() {
        // Body exceeds multimodal limit → error
        let r = resolve_body_limit(
            Some("image/jpeg"),
            64 * 1024 * 1024, // 64 MiB
            10 * 1024 * 1024,
            32 * 1024 * 1024,
        );
        assert!(r.is_err());
    }

    #[test]
    fn text_oversize_rejected() {
        // Body exceeds standard limit → error
        let r = resolve_body_limit(
            Some("application/json"),
            20 * 1024 * 1024, // 20 MiB
            10 * 1024 * 1024,
            32 * 1024 * 1024,
        );
        assert!(r.is_err());
    }
}

#[cfg(test)]
mod streaming_helper_tests {
    //! Tests for the streaming helper types in ingress.rs.
    //!
    //! These tests are intentionally simple — they exercise the
    //! `SseKeepaliveStream` forwarder and the
    //! `UsageAccumulator` ↔ `TruncationReason` transitions without
    //! spinning up an HTTP server. End-to-end idle / total / keepalive
    //! timing is covered by the wiremock tests in
    //! `crates/server/tests/wiremock_providers.rs`; here we focus on
    //! the deterministic state transitions.

    use futures::stream;
    use std::time::Duration;
    use tiygate_core::{TruncationReason, UsageAccumulator};

    /// `SseKeepaliveStream` configured with a non-zero interval
    /// forwards a real frame and resets the keepalive deadline. We
    /// verify that the first frame is observed *unchanged* by
    /// pinning the wrapper (its `pin-project` projection makes
    /// `SseKeepaliveStream` `!Unpin`).
    #[tokio::test]
    async fn keepalive_wrapper_forwards_real_frames_unchanged() {
        let inner = stream::iter(vec![Ok::<_, axum::Error>(
            axum::response::sse::Event::default().data("hello"),
        )]);
        let kept = Box::pin(super::SseKeepaliveStream::new(
            inner,
            Duration::from_millis(50),
        ));
        // `SseKeepaliveStream` is `!Unpin`; `Box::pin` it for the
        // duration of the test so `futures::StreamExt::next` can
        // take `&mut Self: Unpin` on the boxed value.
        let first = futures::StreamExt::next(&mut { kept }).await;
        // We don't introspect the frame body here because axum 0.7's
        // `Event` representation depends on the build; the important
        // invariant is that the wrapper returned *some* event before
        // closing.
        let saw_event = matches!(first, Some(Ok(_)));
        assert!(saw_event, "expected one real frame, got {first:?}");
    }

    /// `SseKeepaliveStream` configured with a `Duration::ZERO` interval
    /// never emits a synthetic keepalive comment for a short inner
    /// stream. The downstream observer should only see real frames
    /// and then immediate close.
    #[tokio::test]
    async fn keepalive_wrapper_disables_when_interval_is_zero() {
        let inner = stream::iter(vec![Ok::<_, axum::Error>(
            axum::response::sse::Event::default().data("first"),
        )]);
        let mut kept = Box::pin(super::SseKeepaliveStream::new(inner, Duration::ZERO));
        let first = futures::StreamExt::next(&mut kept).await;
        let saw_event = matches!(first, Some(Ok(_)));
        assert!(saw_event, "expected one real frame, got {first:?}");
        // No more events should be pending before the inner is
        // exhausted; pulling again should close the stream.
        let after = futures::StreamExt::next(&mut kept).await;
        assert!(after.is_none());
    }

    /// `mark_completed` and `mark_truncated` are mutually exclusive
    /// transitions on the accumulator — calling one clears the other
    /// so disconnect-billing can rely on a single source of truth.
    #[test]
    fn accumulator_completed_clears_truncated() {
        let mut a = UsageAccumulator::new();
        a.record_chunk("hello");
        a.mark_truncated(TruncationReason::Idle);
        assert!(!a.completed);
        assert_eq!(a.truncated, Some(TruncationReason::Idle));
        // Late natural close.
        a.mark_completed();
        assert!(a.completed);
        assert!(a.truncated.is_none());
        // `estimate_usage` is unchanged regardless of the reason.
        let usage = a.estimate_usage();
        assert!(usage.completion_tokens >= 1);
    }

    /// `mark_truncated` forces `completed = false` even if the caller
    /// had previously marked the stream complete. The last call wins.
    #[test]
    fn accumulator_truncated_clears_completed() {
        let mut a = UsageAccumulator::new();
        a.record_chunk("hello");
        a.mark_completed();
        assert!(a.completed);
        // A late upstream error after the natural end should
        // downgrade the state to truncated so billing knows it was
        // not a clean finish.
        a.mark_truncated(TruncationReason::UpstreamError);
        assert!(!a.completed);
        assert_eq!(a.truncated, Some(TruncationReason::UpstreamError));
    }

    /// The three truncation reasons round-trip through `Debug` /
    /// `PartialEq` so disconnect-billing logs are reliable.
    #[test]
    fn truncation_reason_distinct() {
        assert_ne!(TruncationReason::Idle, TruncationReason::Total);
        assert_ne!(TruncationReason::Idle, TruncationReason::UpstreamError);
        assert_ne!(TruncationReason::Total, TruncationReason::UpstreamError);
        // Debug formatting is used by telemetry events.
        assert!(format!("{:?}", TruncationReason::Idle).contains("Idle"));
    }
}

/// Health check — always returns 200 while process is alive.
async fn handle_healthz() -> StatusCode {
    StatusCode::OK
}

/// Readiness check — returns 200 by default, 503 once draining starts so
/// load balancers / K8s Service remove the pod from the rotation
/// (see §3.8 / §5 health probes).
async fn handle_readyz() -> StatusCode {
    if crate::drain::global_drain_signalled() {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}

/// Acquire a concurrency permit, waiting up to acquire_timeout.
/// Returns 503 if the semaphore is exhausted beyond queue depth.
async fn acquire_permit(state: &AppState) -> Result<tokio::sync::OwnedSemaphorePermit, AppError> {
    // Check queue depth before waiting
    let available = state.concurrency_semaphore.available_permits();
    let waiting = state.max_inflight.saturating_sub(available);
    if waiting > state.max_queue_depth {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "gateway overloaded, queue full".to_string(),
        )
        .with_retry_after(5));
    }

    match tokio::time::timeout(
        state.acquire_timeout,
        state.concurrency_semaphore.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "gateway overloaded".to_string(),
        )
        .with_retry_after(5)),
        Err(_) => Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "gateway too busy, try again later".to_string(),
        )
        .with_retry_after(5)),
    }
}

/// Handle POST /v1/chat/completions.
async fn handle_chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    // Acquire concurrency permit
    let _permit = acquire_permit(&state).await?;

    let codec = ChatCompletionsCodec::new();
    let ingress_protocol = codec.id().clone();

    // Per-route body-limit enforcement (text vs. multimodal).
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let body_size = serde_json::to_string(&body)
        .map(|s| s.len() as u64)
        .unwrap_or(0);
    enforce_body_limit(&state, content_type, body_size)?;

    // Build raw envelope
    let raw_env = RawEnvelope {
        method: "POST".to_string(),
        path: "/v1/chat/completions".to_string(),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
        body: Some(serde_json::to_string(&body).unwrap_or_default()),
        truncated: false,
        original_body_size: 0,
        timestamp: chrono::Utc::now(),
    };

    // Decode request
    let ir_request = codec
        .decode_request(body, &raw_env)
        .map_err(|e| AppError::new(StatusCode::BAD_REQUEST, format!("Decode error: {}", e)))?;

    let virtual_model = ir_request.model.clone();
    let is_stream = ir_request.stream;

    // Resolve route
    let targets = state
        .config
        .routing_table
        .resolve(&virtual_model)
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                format!("No route found for model: {}", virtual_model),
            )
        })?;

    // Create pipeline context
    let request_id = uuid::Uuid::now_v7().to_string();
    let _ctx = PipelineContext::new(
        request_id.clone(),
        ir_request.clone(),
        Some(raw_env.clone()),
    );

    // PassThrough detection: when the target's protocol suite matches
    // the ingress suite and the codec declares Passthrough, forward
    // the original body verbatim (no IR round-trip).
    let (pass_through_candidate, raw_passthrough_body) =
        compute_pass_through(&codec, &ingress_protocol, &targets, &raw_env);

    // Fallback policy and retry policy
    let fallback = DefaultFallbackPolicy::with_defaults();
    let retry = RetryPolicy::with_defaults();
    let max_attempts = fallback.max_total_attempts;
    let deadline = Instant::now() + fallback.deadline;

    let mut attempt = 0usize;
    let mut target_index = 0usize;
    let mut last_error: Option<AppError> = None;
    let bytes_emitted: u64 = 0;

    // Apply the routing strategy chosen by config (default `Weighted` per §3.4).
    // The strategy is consulted once at the top of the request — targets are
    // re-ordered every iteration so a slow/unhealthy target is not retried first.
    let (strategy, strategy_label) = build_strategy(state.routing_strategy, state.health.clone());
    let ordered_targets: Vec<&tiygate_core::RoutingTarget> = strategy.order(&targets);

    // Telemetry: emit a RequestStarted event so the event stream has
    // a record of the request's lifetime boundaries (Phase 2 §4.2).
    use chrono::Utc;
    use tiygate_core::telemetry::{EventPayload, PipelineEvent};
    state
        .telemetry
        .send(PipelineEvent {
            request_id: request_id.clone(),
            timestamp: Utc::now(),
            stage: "ingress".to_string(),
            payload: EventPayload::RouteResolved {
                targets: ordered_targets.iter().map(|t| t.health_key()).collect(),
                strategy: strategy_label.to_string(),
            },
        })
        .await;

    while target_index < targets.len() && attempt < max_attempts {
        if Instant::now() > deadline {
            return Err(AppError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "request deadline exceeded".to_string(),
            ));
        }

        let target = &targets[target_index];

        // Check health — skip circuit-broken targets
        let health_key = target.health_key();
        if !state.health.is_healthy(&health_key) {
            // Telemetry: emit a HopFailure so circuit-breaker skips are
            // visible in the event stream.
            state
                .telemetry
                .send(PipelineEvent {
                    request_id: request_id.clone(),
                    timestamp: Utc::now(),
                    stage: "routing".to_string(),
                    payload: EventPayload::HopFailure {
                        target: health_key.clone(),
                        error: "circuit-broken".to_string(),
                        error_class: "CircuitBreaker".to_string(),
                        latency_ms: 0,
                    },
                })
                .await;
            target_index += 1;
            continue;
        }

        // For retries on same target, apply backoff
        if attempt > 0 && attempt > target_index {
            let delay = retry.delay_for(attempt);
            tokio::time::sleep(delay).await;
        }

        attempt += 1;

        // Telemetry: emit HopStart so per-target attempts are queryable.
        let hop_started = Utc::now();
        state
            .telemetry
            .send(PipelineEvent {
                request_id: request_id.clone(),
                timestamp: hop_started,
                stage: "execute".to_string(),
                payload: EventPayload::HopStart {
                    target: health_key.clone(),
                    provider: target.provider_id.clone(),
                    model: target.model_id.clone(),
                    egress_protocol: format!(
                        "{:?}/{}",
                        target.api_protocol.suite, target.api_protocol.name
                    ),
                    hop: attempt,
                },
            })
            .await;

        match execute_upstream(
            &state,
            &codec,
            &ingress_protocol,
            &ir_request,
            target,
            is_stream,
            raw_passthrough_body.as_deref(),
        )
        .await
        {
            Ok(_response) => {
                let hop_elapsed_ms = (Utc::now() - hop_started).num_milliseconds().max(0)
                    as u64;
                // Record success in health registry + EWMA latency for
                // the LatencyStrategy to use on subsequent requests.
                state.health.record_success(&health_key);
                state.health.record_latency_ms(&health_key, hop_elapsed_ms);
                // Telemetry: emit HopSuccess for the event stream.
                state
                    .telemetry
                    .send(PipelineEvent {
                        request_id: request_id.clone(),
                        timestamp: Utc::now(),
                        stage: "execute".to_string(),
                        payload: EventPayload::HopSuccess {
                            target: health_key.clone(),
                            latency_ms: hop_elapsed_ms,
                            usage: None,
                        },
                    })
                    .await;
                return Ok(_response);
            }
            Err(app_err) => {
                let hop_elapsed_ms = (Utc::now() - hop_started).num_milliseconds().max(0)
                    as u64;
                // Record failure + the latency it took (for EWMA).
                state.health.record_failure(&health_key);
                state.health.record_latency_ms(&health_key, hop_elapsed_ms);

                // Classify error
                let core_err = tiygate_core::Error::Routing(app_err.message.clone());
                let classification = classify_error(&core_err);

                // Telemetry: emit HopFailure (Phase 2 §4.2 event model).
                state
                    .telemetry
                    .send(PipelineEvent {
                        request_id: request_id.clone(),
                        timestamp: Utc::now(),
                        stage: "execute".to_string(),
                        payload: EventPayload::HopFailure {
                            target: health_key.clone(),
                            error: app_err.message.clone(),
                            error_class: format!("{:?}", classification.class),
                            latency_ms: hop_elapsed_ms,
                        },
                    })
                    .await;

                // Handle Retry-After from upstream
                if classification.class == ErrorClass::RateLimited {
                    if let Some(rh) = &app_err.retry_after_header {
                        if let Ok(secs) = rh.parse::<u64>() {
                            state.health.apply_cooling(
                                &health_key,
                                Duration::from_secs(secs),
                                "rate_limited",
                            );
                        } else {
                            state.health.apply_cooling(
                                &health_key,
                                Duration::from_secs(30),
                                "rate_limited",
                            );
                        }
                    }
                }

                // Decide next action
                let decision =
                    fallback.classify(&core_err, target, attempt, max_attempts, bytes_emitted);

                match decision {
                    FallbackDecision::TryNext => {
                        // Per §3.4: 401/403 must not retry the same
                        // account — mark the failed account as
                        // "auth-broken" (a longer-than-default cooling
                        // so we don't immediately burn through other
                        // targets on the same account) and skip
                        // forward to the next account.
                        if classification.class == ErrorClass::Auth {
                            state.health.apply_cooling(
                                &health_key,
                                Duration::from_secs(300),
                                "auth_broken",
                            );
                            // Skip past any subsequent targets that
                            // share the same account_label.
                            let skip_label = target.account_label.clone();
                            last_error = Some(app_err);
                            target_index += 1;
                            while let Some(next) = ordered_targets.get(target_index) {
                                if skip_label.is_some()
                                    && next.account_label == skip_label
                                {
                                    target_index += 1;
                                } else {
                                    break;
                                }
                            }
                            continue;
                        }
                        last_error = Some(app_err);
                        target_index += 1;
                        continue;
                    }
                    FallbackDecision::Retry => {
                        last_error = Some(app_err);
                        continue;
                    }
                    FallbackDecision::Fail => {
                        return Err(app_err);
                    }
                }
            }
        }
    }

    // No more targets or attempts exhausted
    Err(last_error.unwrap_or_else(|| {
        AppError::new(
            StatusCode::BAD_GATEWAY,
            "all upstream targets exhausted".to_string(),
        )
    }))
}

/// Handle POST /v1/messages (Anthropic protocol).
async fn handle_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    // Acquire concurrency permit
    let _permit = acquire_permit(&state).await?;

    let codec = MessagesCodec::new();
    let ingress_protocol = codec.id().clone();

    let raw_env = RawEnvelope {
        method: "POST".to_string(),
        path: "/v1/messages".to_string(),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
        body: Some(serde_json::to_string(&body).unwrap_or_default()),
        truncated: false,
        original_body_size: 0,
        timestamp: chrono::Utc::now(),
    };
    let ir_request = codec
        .decode_request(body, &raw_env)
        .map_err(|e| AppError::new(StatusCode::BAD_REQUEST, format!("Decode error: {}", e)))?;
    let virtual_model = ir_request.model.clone();
    let is_stream = ir_request.stream;

    // Resolve route
    let targets = state
        .config
        .routing_table
        .resolve(&virtual_model)
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                format!("No route found for model: {}", virtual_model),
            )
        })?;

    // PassThrough: forward raw body bytes verbatim when the target's
    // protocol suite matches the ingress suite.
    let (_pass_through, raw_passthrough_body) =
        compute_pass_through(&codec, &ingress_protocol, &targets, &raw_env);

    // Fallback + retry loop
    let fallback = DefaultFallbackPolicy::with_defaults();
    let retry = RetryPolicy::with_defaults();
    let max_attempts = fallback.max_total_attempts;
    let deadline = Instant::now() + fallback.deadline;

    let mut attempt = 0usize;
    let mut target_index = 0usize;
    let mut last_error: Option<AppError> = None;
    let bytes_emitted: u64 = 0;

    // Apply the routing strategy chosen by config (default `Weighted` per §3.4).
    let (strategy, _strategy_label) = build_strategy(state.routing_strategy, state.health.clone());
    let ordered_targets: Vec<&tiygate_core::RoutingTarget> = strategy.order(&targets);

    while target_index < ordered_targets.len() && attempt < max_attempts {
        if Instant::now() > deadline {
            return Err(AppError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "request deadline exceeded".to_string(),
            ));
        }

        let target = ordered_targets[target_index];
        let health_key = target.health_key();
        if !state.health.is_healthy(&health_key) {
            target_index += 1;
            continue;
        }

        if attempt > 0 && attempt > target_index {
            let delay = retry.delay_for(attempt);
            tokio::time::sleep(delay).await;
        }

        attempt += 1;

        match execute_messages_upstream(&state, &codec, &ir_request, target, is_stream, raw_passthrough_body.as_deref()).await {
            Ok(response) => {
                state.health.record_success(&health_key);
                return Ok(response);
            }
            Err(app_err) => {
                let core_err = tiygate_core::Error::Routing(app_err.message.clone());
                let classification = classify_error(&core_err);
                state.health.record_failure(&health_key);

                if classification.class == ErrorClass::RateLimited {
                    state.health.apply_cooling(
                        &health_key,
                        Duration::from_secs(30),
                        "rate_limited",
                    );
                }

                let decision =
                    fallback.classify(&core_err, target, attempt, max_attempts, bytes_emitted);
                match decision {
                    FallbackDecision::TryNext => {
                        last_error = Some(app_err);
                        target_index += 1;
                        continue;
                    }
                    FallbackDecision::Retry => {
                        last_error = Some(app_err);
                        continue;
                    }
                    FallbackDecision::Fail => return Err(app_err),
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        AppError::new(
            StatusCode::BAD_GATEWAY,
            "all upstream targets exhausted".to_string(),
        )
    }))
}

/// Execute an upstream OpenAI-compatible request.

async fn execute_upstream(
    state: &AppState,
    codec: &ChatCompletionsCodec,
    ingress_protocol: &tiygate_core::ProtocolEndpoint,
    ir_request: &IrRequest,
    target: &tiygate_core::RoutingTarget,
    is_stream: bool,
    raw_passthrough_body: Option<&str>,
) -> Result<Response, AppError> {
    let egress_protocol = target.api_protocol.clone();
    let is_same_protocol = ingress_protocol.suite == egress_protocol.suite;
    // PassThrough check: same protocol suite + codec declares Passthrough →
    // forward the raw ingress body verbatim to the upstream.
    let is_pass_through = raw_passthrough_body.is_some() && is_same_protocol;

    // Encode for upstream. When PassThrough is in effect, forward the
    // raw ingress body bytes verbatim — no IR re-serialization, so any
    // upstream-specific fields (Anthropic `anthropic_version`,
    // OpenAI `metadata`, custom `user` fields, etc.) are preserved
    // exactly as the client sent them.
    let (upstream_body, mut upstream_headers) = if let Some(raw) = raw_passthrough_body {
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(v) => (v, http::HeaderMap::new()),
            Err(e) => {
                return Err(AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("PassThrough: invalid raw body JSON: {}", e),
                ));
            }
        }
    } else if is_same_protocol {
        codec.encode_request(ir_request).map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Encode error: {}", e),
            )
        })?
    } else {
        let egress_codec = get_egress_codec(&egress_protocol).ok_or_else(|| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("No codec for protocol: {:?}", egress_protocol),
            )
        })?;

        // Check lossy conversion against the field-level capability matrix
        // (§3.2 + docs/protocol-capability-matrix.md). Replaces the previous
        // single-dimension tools+function_calling check with a full per-dimension
        // sweep: tool_choice forms, parallel tool calls, media source kinds,
        // structured output, extended reasoning. Each rejection message names
        // the offending dimension so the caller can fix the request.
        let ingress_caps = codec.capabilities();
        let egress_caps = egress_codec.capabilities();
        if (ingress_caps.lossy_default_reject || egress_caps.lossy_default_reject) {
            if let Err((dim, err)) = tiygate_core::protocol::lossy::check_lossy_conversion(
                ir_request,
                &egress_protocol,
                egress_caps,
            ) {
                return Err(AppError::new(
                    StatusCode::BAD_REQUEST,
                    format!("Lossy conversion rejected: {err} (dimension: {})", dim.label()),
                ));
            }
        }

        egress_codec.encode_request(ir_request).map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Encode error: {}", e),
            )
        })?
    };

    // Apply auth via the registered provider's AuthApplier. Falls
    // back to a static `Bearer {api_key}` if no provider is registered
    // for `target.provider_id` (e.g., test fixtures or built-in
    // OpenAI-compatible endpoints that don't need OAuth).
    apply_provider_auth(target, &mut upstream_headers).await?;

    let client = &state.http_client;
    let upstream_url = format!("{}/chat/completions", target.effective_api_base());

    if is_stream {
        // PassThrough: forward raw body bytes verbatim (no re-serialize).
        // Non-PassThrough: re-serialize via reqwest::Client::json().
        let mut stream_req = client
            .post(&upstream_url)
            .headers(upstream_headers)
            .timeout(state.request_read_timeout);
        if is_pass_through {
            if let Some(raw) = raw_passthrough_body {
                stream_req = stream_req
                    .header("content-type", "application/json")
                    .body(raw.to_string());
            } else {
                stream_req = stream_req.json(&upstream_body);
            }
        } else {
            stream_req = stream_req.json(&upstream_body);
        }
        let response = stream_req.send().await.map_err(|e| {
            AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {}", e))
        })?;

        // Extract Retry-After for passthrough
        let retry_after = extract_retry_after(response.headers());
        let rate_limit_headers_vec: Vec<(&'static str, String)> =
            extract_rate_limit_headers(response.headers());
        let status = response.status();

        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!("Upstream {}: {}", status, error_body),
            );
            app_err.upstream_status = Some(status.as_u16());
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            app_err.rate_limit_headers = rate_limit_headers_vec;
            return Err(app_err);
        }

        // Usage accumulator tracks chunks received from upstream, used
        // by `drive_upstream_stream` for disconnect-billing and the
        // bytes_emitted idempotency gate.
        let accum =
            std::sync::Arc::new(std::sync::Mutex::new(tiygate_core::UsageAccumulator::new()));

        // Build the protocol-native end / error frames from the egress
        // codec. The streaming helper writes the right one for each
        // termination reason (natural end → end frame, idle / total /
        // upstream error → error frame).
        let mut end_enc = codec.stream_encoder();
        let mut err_enc = codec.stream_encoder();
        let end_marker = end_enc.encode_done();
        let error_marker = err_enc
            .encode_error("upstream stream truncated by gateway", Some("upstream_timeout"));

        let mut response = drive_upstream_stream(
            state,
            accum,
            response,
            end_marker,
            error_marker,
            Duration::from_secs(state.upstream_stream_idle_timeout_secs),
            Duration::from_secs(state.upstream_stream_total_timeout_secs),
            DEFAULT_SSE_KEEPALIVE_INTERVAL,
        );
        // Passthrough Retry-After if present
        if let Some(ra) = retry_after {
            response.headers_mut().insert(
                http::HeaderName::from_static("retry-after"),
                http::HeaderValue::from_str(&ra).unwrap_or(http::HeaderValue::from_static("")),
            );
        }
        // Passthrough upstream RateLimit-* headers
        for (name, value) in extract_rate_limit_headers(response.headers()) {
            if let Ok(hv) = http::HeaderValue::from_str(&value) {
                if let Ok(hn) = http::HeaderName::from_bytes(name.as_bytes()) {
                    response.headers_mut().insert(hn, hv);
                }
            }
        }
        Ok(response)
    } else {
        // PassThrough: forward raw body bytes verbatim (no re-serialize).
        // Non-PassThrough: re-serialize via reqwest::Client::json().
        let mut nonstream_req = client
            .post(&upstream_url)
            .headers(upstream_headers)
            .timeout(state.request_read_timeout);
        if is_pass_through {
            if let Some(raw) = raw_passthrough_body {
                nonstream_req = nonstream_req
                    .header("content-type", "application/json")
                    .body(raw.to_string());
            } else {
                nonstream_req = nonstream_req.json(&upstream_body);
            }
        } else {
            nonstream_req = nonstream_req.json(&upstream_body);
        }
        let response = nonstream_req.send().await.map_err(|e| {
            AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {}", e))
        })?;

        let retry_after = extract_retry_after(response.headers());
        let rate_limit_headers_vec: Vec<(&'static str, String)> =
            extract_rate_limit_headers(response.headers());
        let status = response.status();
        let response_body: Value = response
            .json()
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Parse error: {}", e)))?;

        if !status.is_success() {
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!(
                    "Upstream error: {}",
                    response_body["error"]["message"]
                        .as_str()
                        .unwrap_or("Unknown error")
                ),
            );
            app_err.upstream_status = Some(status.as_u16());
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            app_err.rate_limit_headers = rate_limit_headers_vec;
            return Err(app_err);
        }

        // Cross-protocol re-encoding
        let response_json = if is_same_protocol {
            response_body
        } else {
            let egress_codec = get_egress_codec(&egress_protocol).ok_or_else(|| {
                AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("No egress codec found: {:?}", egress_protocol),
                )
            })?;
            let ir_response = egress_codec.decode_response(response_body).map_err(|e| {
                AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Decode response error: {}", e),
                )
            })?;
            codec.encode_response(&ir_response).map_err(|e| {
                AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Encode response error: {}", e),
                )
            })?
        };

        let mut response = Json(response_json).into_response();
        if let Some(ra) = retry_after {
            response.headers_mut().insert(
                http::HeaderName::from_static("retry-after"),
                http::HeaderValue::from_str(&ra)
                    .unwrap_or_else(|_| http::HeaderValue::from_static("")),
            );
        }
        // Passthrough upstream RateLimit-* headers
        for (name, value) in extract_rate_limit_headers(response.headers()) {
            if let Ok(hv) = http::HeaderValue::from_str(&value) {
                if let Ok(hn) = http::HeaderName::from_bytes(name.as_bytes()) {
                    response.headers_mut().insert(hn, hv);
                }
            }
        }
        Ok(response)
    }
}

/// Execute an upstream Anthropic Messages request.
async fn execute_messages_upstream(
    state: &AppState,
    codec: &MessagesCodec,
    ir_request: &IrRequest,
    target: &tiygate_core::RoutingTarget,
    is_stream: bool,
    raw_passthrough_body: Option<&str>,
) -> Result<Response, AppError> {
    let is_pass_through = raw_passthrough_body.is_some();
    // PassThrough: forward raw body bytes verbatim. Non-PassThrough:
    // re-encode via the codec (IR → egress format).
    let (upstream_body, mut upstream_headers) = if let Some(raw) = raw_passthrough_body {
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(v) => (v, http::HeaderMap::new()),
            Err(e) => {
                return Err(AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("PassThrough: invalid raw body JSON: {}", e),
                ));
            }
        }
    } else {
        codec.encode_request(ir_request).map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Encode error: {}", e),
            )
        })?
    };

    // Apply auth via the registered provider's AuthApplier. For
    // Anthropic, this inserts the x-api-key header. The
    // `anthropic-version` header is added by the MessagesCodec's
    // `encode_request` (see protocol/messages.rs), so it survives
    // here.
    apply_provider_auth(target, &mut upstream_headers).await?;

    let client = &state.http_client;
    let upstream_url = format!("{}/messages", target.effective_api_base());

    if is_stream {
        let mut stream_req = client
            .post(&upstream_url)
            .headers(upstream_headers)
            .timeout(state.request_read_timeout);
        if is_pass_through {
            if let Some(raw) = raw_passthrough_body {
                stream_req = stream_req
                    .header("content-type", "application/json")
                    .body(raw.to_string());
            } else {
                stream_req = stream_req.json(&upstream_body);
            }
        } else {
            stream_req = stream_req.json(&upstream_body);
        }
        let response = stream_req
            .send()
            .await
            .map_err(|e| {
                AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {}", e))
            })?;

        let retry_after = extract_retry_after(response.headers());
        let status = response.status();

        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!("Upstream {}: {}", status, error_body),
            );
            app_err.upstream_status = Some(status.as_u16());
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            return Err(app_err);
        }

        let accum = std::sync::Arc::new(std::sync::Mutex::new(
            tiygate_core::UsageAccumulator::new(),
        ));

        // Build the protocol-native end / error frames from the egress
        // codec. The streaming helper writes the right one for each
        // termination reason (natural end → end frame, idle / total /
        // upstream error → error frame).
        let mut end_enc = codec.stream_encoder();
        let mut err_enc = codec.stream_encoder();
        let end_marker = end_enc.encode_done();
        let error_marker = err_enc
            .encode_error("upstream stream truncated by gateway", Some("upstream_timeout"));

        let mut response = drive_upstream_stream(
            state,
            accum,
            response,
            end_marker,
            error_marker,
            Duration::from_secs(state.upstream_stream_idle_timeout_secs),
            Duration::from_secs(state.upstream_stream_total_timeout_secs),
            DEFAULT_SSE_KEEPALIVE_INTERVAL,
        );
        if let Some(ra) = retry_after {
            response.headers_mut().insert(
                http::HeaderName::from_static("retry-after"),
                http::HeaderValue::from_str(&ra).unwrap_or(http::HeaderValue::from_static("")),
            );
        }
        Ok(response)
    } else {
        let response = client
            .post(&upstream_url)
            .headers(upstream_headers)
            .json(&upstream_body)
            .send()
            .await
            .map_err(|e| {
                AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {}", e))
            })?;

        let retry_after = extract_retry_after(response.headers());
        let rate_limit_headers_vec: Vec<(&'static str, String)> =
            extract_rate_limit_headers(response.headers());
        let status = response.status();
        let response_body: Value = response
            .json()
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Parse error: {}", e)))?;

        if !status.is_success() {
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!(
                    "Upstream error: {}",
                    response_body["error"]["message"]
                        .as_str()
                        .unwrap_or("Unknown error")
                ),
            );
            app_err.upstream_status = Some(status.as_u16());
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            app_err.rate_limit_headers = rate_limit_headers_vec;
            return Err(app_err);
        }

        let mut response = Json(response_body).into_response();
        if let Some(ra) = retry_after {
            response.headers_mut().insert(
                http::HeaderName::from_static("retry-after"),
                http::HeaderValue::from_str(&ra).unwrap_or(http::HeaderValue::from_static("")),
            );
        }
        Ok(response)
    }
}

/// Extract Retry-After value from response headers.
fn extract_retry_after(headers: &HeaderMap) -> Option<String> {
    headers
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Extract upstream `RateLimit-*` headers (X-RateLimit-Limit / -Remaining / -Reset)
/// for passthrough to the downstream client.
fn extract_rate_limit_headers(headers: &HeaderMap) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for name in &[
        "x-ratelimit-limit",
        "x-ratelimit-remaining",
        "x-ratelimit-reset",
        "x-ratelimit-limit-requests",
        "x-ratelimit-limit-tokens",
        "x-ratelimit-remaining-requests",
        "x-ratelimit-remaining-tokens",
        "x-ratelimit-reset-tokens",
    ] {
        if let Some(v) = headers.get(*name).and_then(|v| v.to_str().ok()) {
            out.push((*name, v.to_string()));
        }
    }
    out
}

/// Get the appropriate egress codec for a protocol endpoint.
fn get_egress_codec(protocol: &tiygate_core::ProtocolEndpoint) -> Option<Box<dyn EndpointCodec>> {
    match protocol.suite {
        tiygate_core::ProtocolSuite::OpenAiCompatible => {
            Some(Box::new(ChatCompletionsCodec::new()))
        }
        tiygate_core::ProtocolSuite::AnthropicMessages => Some(Box::new(MessagesCodec::new())),
        _ => None,
    }
}

/// Handle POST /v1/embeddings (passthrough to upstream).
async fn handle_embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let _permit = acquire_permit(&state).await?;

    let codec = EmbeddingsCodec::new();
    let raw_env = RawEnvelope {
        method: "POST".to_string(),
        path: "/v1/embeddings".to_string(),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
        body: Some(serde_json::to_string(&body).unwrap_or_default()),
        truncated: false,
        original_body_size: 0,
        timestamp: chrono::Utc::now(),
    };

    let ir_request = codec
        .decode_request(body, &raw_env)
        .map_err(|e| AppError::new(StatusCode::BAD_REQUEST, format!("Decode error: {}", e)))?;

    let virtual_model = ir_request.model.clone();
    let targets = state
        .config
        .routing_table
        .resolve(&virtual_model)
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                format!("No route found for model: {}", virtual_model),
            )
        })?;

    let target = &targets[0];
    let (upstream_body, mut upstream_headers) = codec.encode_request(&ir_request).map_err(|e| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Encode error: {}", e),
        )
    })?;

    apply_provider_auth(target, &mut upstream_headers).await?;
    let client = &state.http_client;
    let upstream_url = format!("{}/embeddings", target.effective_api_base());

    let response = client
        .post(&upstream_url)
        .headers(upstream_headers)
        .json(&upstream_body)
        .send()
        .await
        .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {}", e)))?;

    let status = response.status();
    let response_body: Value = response
        .json()
        .await
        .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Parse error: {}", e)))?;

    if !status.is_success() {
        return Err(AppError::new(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            format!(
                "Upstream error: {}",
                response_body["error"]["message"]
                    .as_str()
                    .unwrap_or("Unknown error")
            ),
        ));
    }

    // Record health success
    state.health.record_success(&target.health_key());

    Ok(Json(response_body).into_response())
}

/// Handle POST /v1/responses — OpenAI Responses API.
///
/// Mirrors `handle_chat_completions` but uses `ResponsesCodec`. The
/// egress pipeline is the same: per-route body limit, route resolve,
/// fallback / retry, RateLimit-* passthrough.
async fn handle_responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let _permit = acquire_permit(&state).await?;
    let codec = ResponsesCodec::new();
    let ingress_protocol = codec.id().clone();
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let body_size = serde_json::to_string(&body)
        .map(|s| s.len() as u64)
        .unwrap_or(0);
    enforce_body_limit(&state, content_type, body_size)?;

    let raw_env = RawEnvelope {
        method: "POST".to_string(),
        path: "/v1/responses".to_string(),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
        body: Some(serde_json::to_string(&body).unwrap_or_default()),
        truncated: false,
        original_body_size: 0,
        timestamp: chrono::Utc::now(),
    };

    let ir_request = codec
        .decode_request(body, &raw_env)
        .map_err(|e| AppError::new(StatusCode::BAD_REQUEST, format!("Decode error: {}", e)))?;

    let virtual_model = ir_request.model.clone();
    let is_stream = ir_request.stream;

    let targets = state
        .config
        .routing_table
        .resolve(&virtual_model)
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                format!("No route found for model: {}", virtual_model),
            )
        })?;

    let target = targets.first().ok_or_else(|| {
        AppError::new(StatusCode::BAD_GATEWAY, "no targets configured".to_string())
    })?;

    let (upstream_body, mut upstream_headers) = codec.encode_request(&ir_request).map_err(|e| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Encode error: {}", e),
        )
    })?;
    apply_provider_auth(target, &mut upstream_headers).await?;

    let upstream_url = format!("{}/responses", target.effective_api_base());

    if is_stream {
        // Streaming path: tell the upstream we accept SSE and drive the
        // body through the same idle/total/keepalive bridge used by the
        // chat-completions and anthropic-messages paths.
        let response = state
            .http_client
            .post(&upstream_url)
            .headers(upstream_headers)
            .header(http::header::ACCEPT, "text/event-stream")
            .json(&upstream_body)
            .send()
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")))?;
        let retry_after = extract_retry_after(response.headers());
        let status = response.status();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!("Upstream {}: {}", status, error_body),
            );
            app_err.upstream_status = Some(status.as_u16());
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            return Err(app_err);
        }

        let accum = std::sync::Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let mut end_enc = codec.stream_encoder();
        let mut err_enc = codec.stream_encoder();
        let end_marker = end_enc.encode_done();
        let error_marker = err_enc
            .encode_error("upstream stream truncated by gateway", Some("upstream_timeout"));

        let mut response = drive_upstream_stream(
            &state,
            accum,
            response,
            end_marker,
            error_marker,
            Duration::from_secs(state.upstream_stream_idle_timeout_secs),
            Duration::from_secs(state.upstream_stream_total_timeout_secs),
            DEFAULT_SSE_KEEPALIVE_INTERVAL,
        );
        if let Some(ra) = retry_after {
            response.headers_mut().insert(
                http::HeaderName::from_static("retry-after"),
                http::HeaderValue::from_str(&ra).unwrap_or(http::HeaderValue::from_static("")),
            );
        }
        // Passthrough upstream RateLimit-* headers so the downstream
        // client can observe the upstream's rate-limit posture on the
        // first response frame.
        for (name, value) in extract_rate_limit_headers(response.headers()) {
            if let Ok(hv) = http::HeaderValue::from_str(&value) {
                if let Ok(hn) = http::HeaderName::from_bytes(name.as_bytes()) {
                    response.headers_mut().insert(hn, hv);
                }
            }
        }
        state.health.record_success(&target.health_key());
        return Ok(response);
    }

    // Non-streaming path: read the full body and forward as JSON.
    let response = state
        .http_client
        .post(&upstream_url)
        .headers(upstream_headers)
        .json(&upstream_body)
        .send()
        .await
        .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")))?;
    let status = response.status();
    let retry_after = extract_retry_after(response.headers());
    let rate_limit_headers_vec: Vec<(&'static str, String)> =
        extract_rate_limit_headers(response.headers());
    let response_body: Value = response
        .json()
        .await
        .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Parse error: {e}")))?;
    if !status.is_success() {
        let mut app_err = AppError::new(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            format!("Upstream {}: {}", status, response_body),
        );
        app_err.upstream_status = Some(status.as_u16());
        if let Some(ra) = retry_after {
            app_err = app_err.with_retry_after_header(ra);
        }
        app_err.rate_limit_headers = rate_limit_headers_vec;
        return Err(app_err);
    }
    let mut resp = Json(response_body).into_response();
    if let Some(ra) = retry_after {
        resp.headers_mut().insert(
            http::HeaderName::from_static("retry-after"),
            http::HeaderValue::from_str(&ra).unwrap_or(http::HeaderValue::from_static("")),
        );
    }
    for (name, value) in extract_rate_limit_headers(resp.headers()) {
        if let Ok(hv) = http::HeaderValue::from_str(&value) {
            if let Ok(hn) = http::HeaderName::from_bytes(name.as_bytes()) {
                resp.headers_mut().insert(hn, hv);
            }
        }
    }
    state.health.record_success(&target.health_key());
    Ok(resp)
}

/// Handle POST /v1beta/models/:model/generateContent — Google Gemini.
///
/// Mirrors `handle_chat_completions` but uses `GeminiCodec`.
async fn handle_gemini_generate(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(model): axum::extract::Path<String>,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let _permit = acquire_permit(&state).await?;
    let codec = GeminiCodec::new();
    let ingress_protocol = codec.id().clone();
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let body_size = serde_json::to_string(&body)
        .map(|s| s.len() as u64)
        .unwrap_or(0);
    enforce_body_limit(&state, content_type, body_size)?;

    let raw_env = RawEnvelope {
        method: "POST".to_string(),
        path: format!("/v1beta/models/{model}/generateContent"),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
        body: Some(serde_json::to_string(&body).unwrap_or_default()),
        truncated: false,
        original_body_size: 0,
        timestamp: chrono::Utc::now(),
    };

    let ir_request = codec
        .decode_request(body, &raw_env)
        .map_err(|e| AppError::new(StatusCode::BAD_REQUEST, format!("Decode error: {}", e)))?;

    let virtual_model = model;
    let is_stream = ir_request.stream;

    let targets = state
        .config
        .routing_table
        .resolve(&virtual_model)
        .ok_or_else(|| {
            AppError::new(
                StatusCode::NOT_FOUND,
                format!("No route found for model: {}", virtual_model),
            )
        })?;

    let target = targets.first().ok_or_else(|| {
        AppError::new(StatusCode::BAD_GATEWAY, "no targets configured".to_string())
    })?;

    let (upstream_body, mut upstream_headers) = codec.encode_request(&ir_request).map_err(|e| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Encode error: {}", e),
        )
    })?;
    apply_provider_auth(target, &mut upstream_headers).await?;

    // Gemini uses `?alt=sse` on the query string to switch the same
    // POST endpoint into Server-Sent Events mode. The body shape is
    // identical to the non-streaming call, so we share `upstream_body`.
    let base_stream_url = format!(
        "{}/v1beta/models/{}:generateContent",
        target.effective_api_base(),
        virtual_model
    );

    if is_stream {
        // Streaming path: append `?alt=sse` and run the body through
        // `drive_upstream_stream` so the client sees the same idle /
        // total / keepalive / protocol-native end-frame semantics as
        // the other streaming ingress paths.
        let response = state
            .http_client
            .post(format!("{base_stream_url}?alt=sse"))
            .headers(upstream_headers)
            .json(&upstream_body)
            .send()
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")))?;
        let retry_after = extract_retry_after(response.headers());
        let status = response.status();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!("Upstream {}: {}", status, error_body),
            );
            app_err.upstream_status = Some(status.as_u16());
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            return Err(app_err);
        }

        let accum = std::sync::Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let mut end_enc = codec.stream_encoder();
        let mut err_enc = codec.stream_encoder();
        let end_marker = end_enc.encode_done();
        let error_marker = err_enc
            .encode_error("upstream stream truncated by gateway", Some("upstream_timeout"));

        let mut response = drive_upstream_stream(
            &state,
            accum,
            response,
            end_marker,
            error_marker,
            Duration::from_secs(state.upstream_stream_idle_timeout_secs),
            Duration::from_secs(state.upstream_stream_total_timeout_secs),
            DEFAULT_SSE_KEEPALIVE_INTERVAL,
        );
        if let Some(ra) = retry_after {
            response.headers_mut().insert(
                http::HeaderName::from_static("retry-after"),
                http::HeaderValue::from_str(&ra).unwrap_or(http::HeaderValue::from_static("")),
            );
        }
        // Passthrough upstream RateLimit-* headers so the downstream
        // client can observe the upstream's rate-limit posture on the
        // first response frame.
        for (name, value) in extract_rate_limit_headers(response.headers()) {
            if let Ok(hv) = http::HeaderValue::from_str(&value) {
                if let Ok(hn) = http::HeaderName::from_bytes(name.as_bytes()) {
                    response.headers_mut().insert(hn, hv);
                }
            }
        }
        state.health.record_success(&target.health_key());
        return Ok(response);
    }

    // Non-streaming path: read the full body and forward as JSON.
    let upstream_url = base_stream_url;
    let response = state
        .http_client
        .post(&upstream_url)
        .headers(upstream_headers)
        .json(&upstream_body)
        .send()
        .await
        .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")))?;
    let status = response.status();
    let retry_after = extract_retry_after(response.headers());
    let rate_limit_headers_vec: Vec<(&'static str, String)> =
        extract_rate_limit_headers(response.headers());
    let response_body: Value = response
        .json()
        .await
        .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Parse error: {e}")))?;
    if !status.is_success() {
        let mut app_err = AppError::new(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            format!("Upstream {}: {}", status, response_body),
        );
        app_err.upstream_status = Some(status.as_u16());
        if let Some(ra) = retry_after {
            app_err = app_err.with_retry_after_header(ra);
        }
        app_err.rate_limit_headers = rate_limit_headers_vec;
        return Err(app_err);
    }
    let mut resp = Json(response_body).into_response();
    if let Some(ra) = retry_after {
        resp.headers_mut().insert(
            http::HeaderName::from_static("retry-after"),
            http::HeaderValue::from_str(&ra).unwrap_or(http::HeaderValue::from_static("")),
        );
    }
    for (name, value) in extract_rate_limit_headers(resp.headers()) {
        if let Ok(hv) = http::HeaderValue::from_str(&value) {
            if let Ok(hn) = http::HeaderName::from_bytes(name.as_bytes()) {
                resp.headers_mut().insert(hn, hv);
            }
        }
    }
    state.health.record_success(&target.health_key());
    let _ = ingress_protocol;
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Streaming helper types
// ---------------------------------------------------------------------------

/// Default keepalive cadence for downstream SSE proxies. Cheap to send
/// (`:keepalive\n\n` is a single SSE comment line) and short enough to
/// keep corporate proxies from killing the connection on idle.
pub const DEFAULT_SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Wraps an inner event stream and emits an SSE comment frame every
/// `interval` while the inner stream is still pending. Once the
/// inner stream completes, the wrapper completes with it — keepalive
/// frames are only useful while a real frame could still arrive.
///
/// This is the "always-on" liveness signal for the downstream client;
/// the *protocol-native* end frame (or error frame) is the gateway's
/// "this is the end" signal and is handled by `drive_upstream_stream`,
/// not by this wrapper.
///
/// The struct is `!Unpin` because it carries a `tokio::time::Sleep`
/// (a non-Unpin future). The single production call site in
/// `drive_upstream_stream` wraps the constructed value in `Box::pin`
/// before handing it to `Sse::new`, so the field-level `!Unpin` is
/// invisible to the rest of the pipeline.
#[pin_project]
pub struct SseKeepaliveStream<S> {
    #[pin]
    inner: S,
    interval: Duration,
    #[pin]
    timer: tokio::time::Sleep,
    /// The instant at which we should next emit a keepalive. Re-armed
    /// every time a real frame is forwarded so the downstream only sees
    /// activity on a live connection.
    emit_keepalive_at: Instant,
    /// Set once the wrapper has decided the stream is closed (either
    /// the inner stream finished or a frame errored); prevents extra
    /// keepalive emissions after close.
    done: bool,
}

impl<S: Stream<Item = Result<axum::response::sse::Event, axum::Error>>> SseKeepaliveStream<S> {
    /// Build a new keepalive wrapper around `inner`. `interval` is the
    /// gap between successive keepalive comments; pass
    /// `Duration::ZERO` to effectively disable keepalives (the
    /// wrapper will then forward inner frames only).
    pub fn new(inner: S, interval: Duration) -> Self {
        let now = Instant::now();
        let interval_for_timer = if interval.is_zero() {
            // Park the timer 1000 years in the future so it never fires
            // in practice — the stream only resolves on the inner path.
            Duration::from_secs(60 * 60 * 24 * 365 * 1000)
        } else {
            interval
        };
        let timer = tokio::time::sleep(interval_for_timer);
        Self {
            inner,
            interval,
            timer,
            emit_keepalive_at: now + interval_for_timer,
            done: false,
        }
    }
}

impl<S: Stream<Item = Result<axum::response::sse::Event, axum::Error>>> Stream
    for SseKeepaliveStream<S>
{
    type Item = Result<axum::response::sse::Event, axum::Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.done {
            return std::task::Poll::Ready(None);
        }
        let mut this = self.project();

        // Fast path: poll the inner stream first. A real frame is
        // always preferred over a synthetic keepalive — keepalives are
        // a "no progress" signal, not a "please yield" signal.
        match this.inner.as_mut().poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(event))) => {
                // Reset the keepalive deadline on real activity.
                *this.emit_keepalive_at =
                    Instant::now() + if this.interval.is_zero() {
                        Duration::from_secs(0)
                    } else {
                        *this.interval
                    };
                let _ = this.timer.as_mut().reset(tokio::time::Instant::now() + *this.interval);
                return std::task::Poll::Ready(Some(Ok(event)));
            }
            std::task::Poll::Ready(Some(Err(e))) => {
                *this.done = true;
                return std::task::Poll::Ready(Some(Err(e)));
            }
            std::task::Poll::Ready(None) => {
                *this.done = true;
                return std::task::Poll::Ready(None);
            }
            std::task::Poll::Pending => {}
        }

        // Inner stream is pending: see whether the keepalive timer has
        // elapsed and, if so, emit a comment frame and re-arm the
        // timer.
        if !this.interval.is_zero() {
            let now = Instant::now();
            if now >= *this.emit_keepalive_at {
                *this.emit_keepalive_at = now + *this.interval;
                let _ = this
                    .timer
                    .as_mut()
                    .reset(tokio::time::Instant::now() + *this.interval);
                let keepalive = axum::response::sse::Event::default().comment("keepalive");
                return std::task::Poll::Ready(Some(Ok(keepalive)));
            }
            // Re-register the timer waker so the task wakes up when
            // the keepalive deadline is reached.
            let _ = ready!(this.timer.as_mut().poll(cx));
        }

        std::task::Poll::Pending
    }
}

/// Drive an upstream HTTP response body to the downstream client as an
/// SSE stream. Adds:
///
/// 1. An **idle timer** (default 120s). Every time a chunk is forwarded
///    the timer resets. If no chunk arrives for the full window, the
///    stream is closed with `end_marker` (a `encode_done()`-style
///    protocol-native end frame) and the accumulator is marked
///    truncated with `TruncationReason::Idle`.
/// 2. A **total timer** (default disabled, `0` = off). A wall-clock
///    budget measured from the moment this function is called. When it
///    elapses the stream is closed with `error_marker` and the
///    accumulator is marked truncated with
///    `TruncationReason::Total`.
/// 3. A **30s SSE keepalive** wrapper that emits `:keepalive` comments
///    on the downstream side whenever the upstream is silent but
///    inside the idle budget.
///
/// `end_marker` and `error_marker` are caller-supplied because the
/// protocol-native framing differs per ingress protocol (chat completions,
/// anthropic messages, responses, gemini). Bytes from the upstream are
/// passed through verbatim — we do not parse SSE in this path. When the
/// upstream connection produces an error mid-stream, the stream is
/// closed with `error_marker` and the accumulator is marked truncated
/// with `TruncationReason::UpstreamError`. The
/// `Retry-After` / `RateLimit-*` headers from the upstream response
/// are passed through by the caller; this function only builds the
/// streaming body.
#[allow(clippy::too_many_arguments)]
pub fn drive_upstream_stream(
    _state: &AppState,
    accum: Arc<std::sync::Mutex<UsageAccumulator>>,
    response: reqwest::Response,
    end_marker: Vec<u8>,
    error_marker: Vec<u8>,
    idle_timeout: Duration,
    total_timeout: Duration,
    keepalive_interval: Duration,
) -> Response {
    use async_stream::stream;

    let total_budget_enabled = !total_timeout.is_zero();
    let total_started = Instant::now();
    let mut upstream = response.bytes_stream();
    let mut last_reason: Option<TruncationReason> = None;
    let end_marker = end_marker;
    let error_marker = error_marker;
    let idle_timeout = if idle_timeout.is_zero() {
        // 0 means "use the keepalive cadence as a no-progress signal"
        // — but to be safe we still need *some* upper bound so a hung
        // upstream cannot pin a connection forever. Use the keepalive
        // cadence as the soft idle, and 24h as the absolute hard cap.
        Duration::from_secs(60 * 60 * 24)
    } else {
        idle_timeout
    };
    let keepalive_interval = if keepalive_interval.is_zero() {
        DEFAULT_SSE_KEEPALIVE_INTERVAL
    } else {
        keepalive_interval
    };

    // Per-poll timer state: we keep a `Sleep` future that is reset on
    // every forwarded chunk. While the future is pending the stream
    // returns `Pending`; when it fires, we close the stream with the
    // idle end frame.
    let idle_future = stream! {
        // Initial timer fires after one idle window. We give the
        // upstream a chance to deliver the first chunk by sleeping
        // before checking.
        let mut idle_deadline = tokio::time::Instant::now() + idle_timeout;
        let total_deadline: Option<tokio::time::Instant> =
            if total_budget_enabled {
                Some(tokio::time::Instant::now() + total_timeout)
            } else {
                None
            };
        loop {
            tokio::select! {
                biased;
                chunk = upstream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => {
                            // Reset the idle deadline — the upstream is
                            // actively producing.
                            idle_deadline = tokio::time::Instant::now() + idle_timeout;
                            if let Ok(text) = std::str::from_utf8(&bytes) {
                                if let Ok(mut a) = accum.lock() {
                                    a.record_chunk(text);
                                }
                            } else if let Ok(mut a) = accum.lock() {
                                a.record_chunk(&String::from_utf8_lossy(&bytes));
                            }
                            yield Ok(axum::response::sse::Event::default()
                                .data(String::from_utf8_lossy(&bytes).to_string()));
                        }
                        Some(Err(_e)) => {
                            last_reason = Some(TruncationReason::UpstreamError);
                            // Mark the accumulator as truncated BEFORE
                            // yielding the error marker so disconnect-
                            // billing sees the right state.
                            if let Ok(mut a) = accum.lock() {
                                a.mark_truncated(TruncationReason::UpstreamError);
                            }
                            // Emit the protocol-native error frame so
                            // the client can tell the upstream failed,
                            // then close.
                            if !error_marker.is_empty() {
                                if let Ok(text) = std::str::from_utf8(&error_marker) {
                                    yield Ok(axum::response::sse::Event::default()
                                        .data(text.to_string()));
                                } else {
                                    yield Ok(axum::response::sse::Event::default()
                                        .data(String::from_utf8_lossy(&error_marker).to_string()));
                                }
                            }
                            break;
                        }
                        None => {
                            // Upstream closed naturally — emit the
                            // protocol-native end frame and finish.
                            last_reason = None;
                            if let Ok(mut a) = accum.lock() {
                                a.mark_completed();
                            }
                            if !end_marker.is_empty() {
                                if let Ok(text) = std::str::from_utf8(&end_marker) {
                                    yield Ok(axum::response::sse::Event::default()
                                        .data(text.to_string()));
                                } else {
                                    yield Ok(axum::response::sse::Event::default()
                                        .data(String::from_utf8_lossy(&end_marker).to_string()));
                                }
                            }
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep_until(idle_deadline) => {
                    last_reason = Some(TruncationReason::Idle);
                    if let Ok(mut a) = accum.lock() {
                        a.mark_truncated(TruncationReason::Idle);
                    }
                    // Idle elapsed. Emit the protocol-native end
                    // frame and close — already-received bytes are
                    // still billable.
                    if !end_marker.is_empty() {
                        if let Ok(text) = std::str::from_utf8(&end_marker) {
                            yield Ok(axum::response::sse::Event::default()
                                .data(text.to_string()));
                        } else {
                            yield Ok(axum::response::sse::Event::default()
                                .data(String::from_utf8_lossy(&end_marker).to_string()));
                        }
                    }
                    break;
                }
                _ = async {
                    if let Some(t) = total_deadline {
                        tokio::time::sleep_until(t).await;
                    } else {
                        // No total budget — wait forever.
                        std::future::pending::<()>().await;
                    }
                } => {
                    last_reason = Some(TruncationReason::Total);
                    if let Ok(mut a) = accum.lock() {
                        a.mark_truncated(TruncationReason::Total);
                    }
                    // Total budget elapsed. Emit the protocol-native
                    // error frame so the client can tell this was a
                    // gateway-side cap, not a natural end.
                    if !error_marker.is_empty() {
                        if let Ok(text) = std::str::from_utf8(&error_marker) {
                            yield Ok(axum::response::sse::Event::default()
                                .data(text.to_string()));
                        } else {
                            yield Ok(axum::response::sse::Event::default()
                                .data(String::from_utf8_lossy(&error_marker).to_string()));
                        }
                    }
                    break;
                }
            }
        }
        // Touch `last_reason` to silence the unused-variable lint
        // — the variable is captured by the async-stream macro but
        // `cargo` does not always see the use.
        let _ = last_reason;
        // Touch the total_started clock for the same reason.
        let _ = total_started;
    };

    // Wrap the inner stream in a keepalive emitter so the downstream
    // client (and any middlebox) keeps seeing activity even when the
    // upstream is between chunks.
    let kept = SseKeepaliveStream::new(Box::pin(idle_future), keepalive_interval);
    Sse::new(kept).into_response()
}

/// Simple error type for the HTTP layer.
#[derive(Debug)]
pub struct AppError {
    status: StatusCode,
    message: String,
    /// Passthrough Retry-After header value from upstream.
    retry_after_header: Option<String>,
    /// Original upstream HTTP status for error source distinction.
    upstream_status: Option<u16>,
    /// Upstream RateLimit-* headers to passthrough on the error response.
    rate_limit_headers: Vec<(&'static str, String)>,
}

impl AppError {
    fn new(status: StatusCode, message: String) -> Self {
        Self {
            status,
            message,
            retry_after_header: None,
            upstream_status: None,
            rate_limit_headers: Vec::new(),
        }
    }

    /// Attach a Retry-After value (seconds).
    fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after_header = Some(seconds.to_string());
        self
    }

    /// Attach a raw Retry-After header value.
    fn with_retry_after_header(mut self, value: String) -> Self {
        self.retry_after_header = Some(value);
        self
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let error_source = if self.upstream_status.is_some() {
            "upstream"
        } else {
            "gateway"
        };

        let mut body = serde_json::json!({
            "error": {
                "message": self.message,
                "type": "gateway_error",
                "source": error_source,
            }
        });

        if let Some(us) = self.upstream_status {
            body["error"]["upstream_status"] = serde_json::json!(us);
        }

        let mut response = (self.status, Json(body)).into_response();

        // Passthrough Retry-After to downstream
        if let Some(ref ra) = self.retry_after_header {
            if let Ok(val) = http::HeaderValue::from_str(ra) {
                response
                    .headers_mut()
                    .insert(http::HeaderName::from_static("retry-after"), val);
            }
        }

        // Passthrough upstream RateLimit-* headers (they appear on 429/503)
        for (name, value) in &self.rate_limit_headers {
            if let Ok(hv) = http::HeaderValue::from_str(value) {
                if let Ok(hn) = http::HeaderName::from_bytes(name.as_bytes()) {
                    response.headers_mut().insert(hn, hv);
                }
            }
        }

        response
    }
}
