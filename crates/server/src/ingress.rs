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
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use bytes::Bytes;
use futures::{Future, Stream, StreamExt};
use pin_project::pin_project;
use serde_json::Value;
use tokio::sync::Semaphore;
use tower_http::timeout::RequestBodyTimeoutLayer;

use tiygate_core::tracing_ctx::TraceContext;
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
    pub config: Arc<ConfigStore>,
    /// Optional handle to the DB-backed config store. When `Some`,
    /// the data plane can perform per-caller `api_keys` lookups
    /// (used by `resolve_api_key` in `ingress_phase4`). When `None`
    /// (legacy in-memory path, no control plane) the api key
    /// resolution is a no-op and all requests are treated as
    /// anonymous. Production code wires this in via
    /// `router_with_telemetry` from `app.rs`.
    pub db_store: Option<Arc<tiygate_store::config_store::DbConfigStore>>,
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
    /// Quota counter; `None` in the legacy in-memory path. The
    /// ingress hot path consults this *before* forwarding upstream
    /// and returns `429 + Retry-After` on deny.
    pub quota: Option<Arc<dyn tiygate_core::quota::QuotaCounter>>,
    /// Embedding cache; `None` when the `cache` feature is off.
    /// Only `/v1/embeddings` consults this; chat handlers ignore
    /// it (per §4.7).
    pub embedding_cache: Option<Arc<tiygate_cache::embedding_cache::EmbeddingCache>>,
    /// Raw envelope body cap in bytes; bodies larger than this are
    /// truncated and `truncated=true` is set on the `RawEnvelope`.
    pub raw_envelope_max_bytes: u64,
    /// Whether to capture inline base64 media in raw envelopes
    /// (default false — store metadata only, per §4.1).
    pub raw_envelope_capture_media: bool,
    /// Per-request `Redactor` instance. Configurable so future
    /// env-var-driven extensions remain test-friendly.
    pub redactor: Arc<tiygate_core::redaction::Redactor>,
    /// Bidirectional header forwarding policy (denylist-based). Decides
    /// which client request headers reach the provider and which
    /// upstream response headers reach the client.
    pub header_policy: Arc<tiygate_core::HeaderForwardPolicy>,
}

impl AppState {
    /// Returns the config snapshot the data plane should read for
    /// this request. When a `DbConfigStore` is wired in (production
    /// control-plane path), this returns the latest snapshot the
    /// epoch-poll task has published — so admin CRUD writes become
    /// visible to live traffic within the poll interval, without
    /// restarting the process and without the request itself
    /// triggering any DB read. When no DB store is present (legacy /
    /// test path), it returns the static snapshot captured at router
    /// build time.
    pub fn current_config(&self) -> Arc<ConfigStore> {
        match &self.db_store {
            Some(db) => db.snapshot(),
            None => self.config.clone(),
        }
    }
}

use crate::config::ServerConfig;

/// Build the ingress router.
#[allow(dead_code)]
pub fn router(
    config: ConfigStore,
    health: Arc<HealthRegistry>,
    server_config: &ServerConfig,
) -> Router {
    // Build a no-op telemetry bus for tests / direct router() calls. The
    // App::new() path wires up a real stdout-backed bus via the App struct.
    let telemetry: Arc<dyn TelemetryBus> = Arc::new(crate::telemetry::ChannelTelemetryBus::spawn(
        Arc::new(tiygate_store::log_sink::stdout::StdoutSink::new()),
        64,
    ));
    router_with_telemetry(config, health, server_config, telemetry, None, None)
}

/// Build the ingress router with an explicit telemetry bus.
///
/// Production code should use this entry point so that bus instances are
/// not duplicated or orphaned.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub fn router_with_telemetry(
    config: ConfigStore,
    health: Arc<HealthRegistry>,
    server_config: &ServerConfig,
    telemetry: Arc<dyn TelemetryBus>,
    quota: Option<Arc<dyn tiygate_core::quota::QuotaCounter>>,
    embedding_cache: Option<Arc<tiygate_cache::embedding_cache::EmbeddingCache>>,
) -> Router {
    // The legacy call path (tests, `router()` shim) does not have
    // a DB store — the data plane can still serve traffic, but
    // `resolve_api_key` will treat every request as anonymous.
    router_with_telemetry_full(
        config,
        health,
        server_config,
        telemetry,
        quota,
        embedding_cache,
        None,
    )
}

/// Build the ingress router with the full set of production
/// dependencies — including the optional `DbConfigStore` used by
/// `resolve_api_key` to look up `api_keys` rows. This is the
/// entry point called from `app.rs`; the simpler
/// `router_with_telemetry` shim is kept for tests.
#[allow(clippy::too_many_arguments)]
pub fn router_with_telemetry_full(
    config: ConfigStore,
    health: Arc<HealthRegistry>,
    server_config: &ServerConfig,
    telemetry: Arc<dyn TelemetryBus>,
    quota: Option<Arc<dyn tiygate_core::quota::QuotaCounter>>,
    embedding_cache: Option<Arc<tiygate_cache::embedding_cache::EmbeddingCache>>,
    db_store: Option<Arc<tiygate_store::config_store::DbConfigStore>>,
) -> Router {
    build_data_plane_router(
        config,
        health,
        server_config,
        telemetry,
        quota,
        embedding_cache,
        db_store,
    )
}

/// Internal builder kept separate from the public `router_with_telemetry_full`
/// so we can also expose the bare `Router<AppState>` for tests and inspection
/// harnesses.
fn build_data_plane_router(
    config: ConfigStore,
    health: Arc<HealthRegistry>,
    server_config: &ServerConfig,
    telemetry: Arc<dyn TelemetryBus>,
    quota: Option<Arc<dyn tiygate_core::quota::QuotaCounter>>,
    embedding_cache: Option<Arc<tiygate_cache::embedding_cache::EmbeddingCache>>,
    db_store: Option<Arc<tiygate_store::config_store::DbConfigStore>>,
) -> Router {
    let semaphore = Arc::new(Semaphore::new(server_config.max_inflight_requests));
    let state = AppState {
        config: Arc::new(config),
        db_store,
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
        quota,
        embedding_cache,
        raw_envelope_max_bytes: server_config.raw_envelope_max_bytes,
        raw_envelope_capture_media: server_config.raw_envelope_capture_media,
        redactor: Arc::new(tiygate_core::redaction::Redactor::with_defaults()),
        header_policy: Arc::new(
            tiygate_core::HeaderForwardPolicy::with_defaults()
                .with_request_deny_extra(server_config.forward_request_header_deny_extra.iter())
                .with_response_deny_extra(server_config.forward_response_header_deny_extra.iter()),
        ),
    };

    Router::new()
        .route("/v1/chat/completions", post(handle_chat_completions))
        .route("/v1/messages", post(handle_messages))
        .route("/v1/embeddings", post(handle_embeddings))
        .route("/v1/responses", post(handle_responses))
        // Google Gemini — two path shapes are accepted to cover the
        // divergence between the public Gemini docs (which use
        // `models/{model}:generateContent` with a colon) and
        // OpenAI-style path conventions that use a slash before the
        // method verb. The colon shape is the official one per
        // https://ai.google.dev/api/generate-content; the slash
        // shape is a convenience for SDKs that prefer URL
        // hierarchies. Both routes are routed to the same handler.
        //
        // Implementation note: axum 0.7 does not allow two captures
        // in the same path segment (e.g. `:model:generateContent`
        // panics at router-construction time with "only one
        // parameter is allowed per path segment"). To capture the
        // colon form we use a single-segment capture that swallows
        // the colon: `/v1beta/models/:capture` — here the value
        // captured for `capture` is the entire `foo:generateContent`
        // token, which we then split on the last `:` in
        // `split_gemini_capture` (handler entrypoint). The slash
        // form uses a regular `:model` capture with the literal
        // `generateContent` / `streamGenerateContent` segments
        // consumed by the static route suffix.
        .route(
            "/v1beta/models/:capture",
            post(handle_gemini_generate),
        )
        .route(
            "/v1beta/models/:model/generateContent",
            post(handle_gemini_generate),
        )
        // Streaming variants (`:streamGenerateContent?alt=sse`).
        .route(
            "/v1beta/models/:model/streamGenerateContent",
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
    // given `provider_id`.
    //   - Anthropic: `x-api-key` header + `anthropic-version`.
    //   - Google Gemini (Public, `generativelanguage.googleapis.com`):
    //     `x-goog-api-key: <KEY>` header per the official Google AI for
    //     Developers spec. (`?key=<KEY>` query is also supported by the
    //     official endpoint; the URL builders do not append it by default
    //     because the header is the recommended form.)
    //   - Everything else: `Authorization: Bearer <KEY>`.
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
    } else if matches!(target.api_protocol.suite, ProtocolSuite::GoogleGemini) {
        tiygate_providers::gemini::apply_gemini_default_auth(target, upstream_headers).map_err(
            |e| {
                AppError::new(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Invalid x-goog-api-key header value: {e}"),
                )
            },
        )?;
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
#[allow(dead_code)]
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
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
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
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
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
        let inner = stream::iter(vec![Ok::<_, axum::Error>(bytes::Bytes::from_static(
            b"data: hello\n\n",
        ))]);
        let kept = Box::pin(super::SseKeepaliveStream::new(
            inner,
            Duration::from_millis(50),
        ));
        // `SseKeepaliveStream` is `!Unpin`; `Box::pin` it for the
        // duration of the test so `futures::StreamExt::next` can
        // take `&mut Self: Unpin` on the boxed value.
        let first = futures::StreamExt::next(&mut { kept }).await;
        // The wrapper must forward the upstream bytes VERBATIM — no
        // extra `data:` prefixing. This is the regression guard for
        // the double-`data:` bug.
        match first {
            Some(Ok(b)) => assert_eq!(
                b.as_ref(),
                b"data: hello\n\n",
                "frame must be forwarded verbatim"
            ),
            other => panic!("expected one real frame, got {other:?}"),
        }
    }

    /// `SseKeepaliveStream` configured with a `Duration::ZERO` interval
    /// never emits a synthetic keepalive comment for a short inner
    /// stream. The downstream observer should only see real frames
    /// and then immediate close.
    #[tokio::test]
    async fn keepalive_wrapper_disables_when_interval_is_zero() {
        let inner = stream::iter(vec![Ok::<_, axum::Error>(bytes::Bytes::from_static(
            b"data: first\n\n",
        ))]);
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

/// Split a Gemini path-capture into `(model_id, method)`.
///
/// The Google Gemini endpoint grammar allows two shapes:
///   * colon form  — `models/{model}:{method}`     (e.g. `foo:generateContent`)
///   * slash form  — `models/{model}/{method}`     (e.g. `foo/generateContent`)
///
/// Both shapes are normalised by the router into a single
/// `:capture` value. The slash form arrives here as just `foo`
/// (the verb is consumed by the static route suffix). The colon
/// form arrives as `foo:generateContent`.
///
/// Returns `None` for inputs that contain none of the recognised
/// methods or contain multiple `:` separators.
fn split_gemini_capture(capture: &str) -> Option<(&str, &str)> {
    const METHODS: &[&str] = &[
        "generateContent",
        "streamGenerateContent",
        "countTokens",
        "embedContent",
        "batchGenerateContent",
    ];
    if let Some((model, method)) = capture.rsplit_once(':') {
        // colon form: ensure the suffix is a known method, and the
        // model id does not contain another `:` (so `a:b:generate`
        // does not get matched as model=`a:b`, method=`generate`).
        if model.contains(':') {
            return None;
        }
        if METHODS.contains(&method) {
            return Some((model, method));
        }
        return None;
    }
    // No colon — the slash form. The trailing verb has already
    // been consumed by the static route suffix, so we can hand
    // back the bare capture as the model id with an empty method.
    Some((capture, ""))
}

/// Strip a known Gemini method-verb suffix from a bare model id
/// (legacy helper kept for unit-test coverage; the colon-form
/// path-capture is split via `split_gemini_capture` instead).
#[cfg(test)]
fn strip_gemini_method_suffix(captured: &str) -> &str {
    const SUFFIXES: &[&str] = &[
        ":generateContent",
        ":streamGenerateContent",
        ":countTokens",
        ":embedContent",
        ":batchGenerateContent",
    ];
    for s in SUFFIXES {
        if let Some(stripped) = captured.strip_suffix(s) {
            return stripped;
        }
    }
    captured
}

#[cfg(test)]
mod gemini_path_tests {
    use super::{split_gemini_capture, strip_gemini_method_suffix};

    #[test]
    fn splits_colon_generate_content() {
        let (m, v) = split_gemini_capture("foo:generateContent").unwrap();
        assert_eq!(m, "foo");
        assert_eq!(v, "generateContent");
    }

    #[test]
    fn splits_colon_stream_generate_content_with_slashes() {
        let (m, v) =
            split_gemini_capture("anthropic/claude-opus-4.8:streamGenerateContent").unwrap();
        assert_eq!(m, "anthropic/claude-opus-4.8");
        assert_eq!(v, "streamGenerateContent");
    }

    #[test]
    fn handles_slash_form_capture() {
        // Slash form arrives at the handler as just the model id;
        // the verb was consumed by the static route suffix.
        let (m, v) = split_gemini_capture("foo").unwrap();
        assert_eq!(m, "foo");
        assert_eq!(v, "");
    }

    #[test]
    fn rejects_unknown_colon_suffix() {
        assert!(split_gemini_capture("foo:unknown").is_none());
    }

    #[test]
    fn rejects_multiple_colons_in_model() {
        // `a:b:generate` should NOT match model=`a:b`, method=`generate`.
        assert!(split_gemini_capture("a:b:generateContent").is_none());
    }

    #[test]
    fn legacy_strip_helper_still_works() {
        assert_eq!(strip_gemini_method_suffix("foo:generateContent"), "foo");
        assert_eq!(strip_gemini_method_suffix("foo"), "foo");
        assert_eq!(strip_gemini_method_suffix("foo:unknown"), "foo:unknown");
    }
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

    // Wall-clock anchor for the Phase 4 `RequestEvent`. We measure
    // the *whole* request handler duration (including fallback
    // retries) so the latency column reflects what the client
    // actually experienced.
    let started = Instant::now();
    let request_id = uuid::Uuid::now_v7().to_string();

    let trace_ctx = crate::ingress_phase4::extract_trace(&headers);
    let raw_env = crate::ingress_phase4::build_redacted_envelope(
        &state,
        "POST",
        "/v1/chat/completions",
        &body,
        &headers,
    );

    // Build the RequestScope *after* the body-limit check passes so
    // that an oversized payload surfaces as the appropriate 413
    // (no terminal RequestEvent needed for the data-plane
    // pre-pipeline checks; the existing app-level logger captures
    // it). We do install the scope for the downstream pipeline
    // (decode → quota → route → execute) so every code path emits.
    let mut scope = crate::ingress_phase4::RequestScope::new(
        &state,
        request_id.clone(),
        "unknown",
        ingress_protocol.clone(),
        trace_ctx.clone(),
        started,
    );
    // Persist the redacted envelope on the terminal RequestEvent
    // for audit / replay (§8 #3 / #8). `Redactor` is already
    // applied at envelope build time, so the value is safe to
    // store as-is in the OLTP `request_logs.raw_envelope_json`
    // column.
    scope.set_envelope(raw_env.clone());

    // Phase 4 §4.6: api key resolution + quota enforcement. The
    // resolved `api_key` is bound to the scope so the terminal
    // RequestEvent attributes the row to the right caller.
    let api_key = crate::ingress_phase4::resolve_api_key(&state, &headers).await;
    scope.set_api_key_id(api_key.key_id.clone());
    match crate::ingress_phase4::check_quota(&state, &api_key.key_id, &api_key.spec, 1).await {
        crate::ingress_phase4::QuotaOutcome::Allow => {}
        crate::ingress_phase4::QuotaOutcome::Deny { retry_after, .. } => {
            let app_err =
                AppError::new(StatusCode::TOO_MANY_REQUESTS, "quota exceeded".to_string())
                    .with_retry_after(retry_after.as_secs().max(1));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("quota_exceeded", Some(http_status));
            return Err(app_err);
        }
    }

    // Decode request
    let ir_request = match codec.decode_request(body, &raw_env) {
        Ok(r) => r,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_REQUEST, format!("Decode error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("decode_error", Some(http_status));
            return Err(app_err);
        }
    };

    let virtual_model = ir_request.model.clone();
    let is_stream = ir_request.stream;
    // Re-key the scope now that we know the actual model.
    scope.set_virtual_model(virtual_model.clone());

    // Resolve route
    let targets = match state.current_config().routing_table.resolve(&virtual_model) {
        Some(t) => t,
        None => {
            let app_err = AppError::new(
                StatusCode::NOT_FOUND,
                format!("No route found for model: {virtual_model}"),
            );
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("route_not_found", Some(http_status));
            return Err(app_err);
        }
    };

    // Create pipeline context
    let _ctx = PipelineContext::new(
        request_id.clone(),
        ir_request.clone(),
        Some(raw_env.clone()),
    );

    // PassThrough detection: when the target's protocol suite matches
    // the ingress suite and the codec declares Passthrough, forward
    // the original body verbatim (no IR round-trip).
    let (_pass_through_candidate, raw_passthrough_body) =
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
            let app_err = AppError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "request deadline exceeded".to_string(),
            );
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("deadline_exceeded", Some(http_status));
            return Err(app_err);
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

        // Bind the resolved target on the scope so the terminal
        // RequestEvent attributes the row to the right upstream.
        scope.set_egress(target.api_protocol.clone());
        scope.set_resolved(target.provider_id.clone(), target.model_id.clone());

        match execute_upstream(
            &state,
            &codec,
            &ingress_protocol,
            &ir_request,
            target,
            is_stream,
            raw_passthrough_body.as_deref(),
            &trace_ctx,
            &request_id,
            &headers,
        )
        .await
        {
            Ok(_response) => {
                let hop_elapsed_ms = (Utc::now() - hop_started).num_milliseconds().max(0) as u64;
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
                // Phase 4 §4.2: terminal `RequestEvent` via the scope.
                scope.emit_ok(Some(_response.status().as_u16()));
                return Ok(_response);
            }
            Err(app_err) => {
                let hop_elapsed_ms = (Utc::now() - hop_started).num_milliseconds().max(0) as u64;
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
                                if skip_label.is_some() && next.account_label == skip_label {
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
                        // The fallback policy says this is terminal.
                        // Surface the error_class so the dashboard
                        // can distinguish auth / rate-limit / timeout
                        // / routing failures.
                        let http_status = app_err.http_status().as_u16();
                        let error_class = format!("{:?}", classification.class);
                        scope.emit_error(&error_class, Some(http_status));
                        return Err(app_err);
                    }
                }
            }
        }
    }

    // No more targets or attempts exhausted
    let final_err = last_error.unwrap_or_else(|| {
        AppError::new(
            StatusCode::BAD_GATEWAY,
            "all upstream targets exhausted".to_string(),
        )
    });
    // Phase 4 §4.2: surface a terminal `RequestEvent` so the OltpSink
    // gets a row even when the request never made it to an upstream
    // success. The classification of the failure (auth / rate-limit /
    // timeout / routing) was already emitted as `HopFailure` events
    // inside the loop; here we record the *terminal* state.
    let http_status = final_err.http_status().as_u16();
    scope.emit_error("upstream_exhausted", Some(http_status));
    Err(final_err)
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

    // Wall-clock anchor for the Phase 4 `RequestEvent`.
    let started = Instant::now();
    let request_id = uuid::Uuid::now_v7().to_string();

    let trace_ctx = crate::ingress_phase4::extract_trace(&headers);
    let raw_env = crate::ingress_phase4::build_redacted_envelope(
        &state,
        "POST",
        "/v1/messages",
        &body,
        &headers,
    );

    // Build the RequestScope so every early-return path emits a
    // terminal `RequestEvent`. See `handle_chat_completions` for
    // the full rationale.
    let mut scope = crate::ingress_phase4::RequestScope::new(
        &state,
        request_id.clone(),
        "unknown",
        ingress_protocol.clone(),
        trace_ctx.clone(),
        started,
    );
    // Persist the redacted envelope on the terminal RequestEvent
    // for audit / replay (§8 #3 / #8). `Redactor` is already
    // applied at envelope build time, so the value is safe to
    // store as-is in the OLTP `request_logs.raw_envelope_json`
    // column.
    scope.set_envelope(raw_env.clone());

    // Phase 4 §4.6: api key resolution + quota enforcement (parity
    // with the chat-completions path). The resolved `api_key` is
    // bound to the scope so the terminal `RequestEvent` attributes
    // the row to the right caller.
    let api_key = crate::ingress_phase4::resolve_api_key(&state, &headers).await;
    scope.set_api_key_id(api_key.key_id.clone());
    match crate::ingress_phase4::check_quota(&state, &api_key.key_id, &api_key.spec, 1).await {
        crate::ingress_phase4::QuotaOutcome::Allow => {}
        crate::ingress_phase4::QuotaOutcome::Deny { retry_after, .. } => {
            let app_err =
                AppError::new(StatusCode::TOO_MANY_REQUESTS, "quota exceeded".to_string())
                    .with_retry_after(retry_after.as_secs().max(1));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("quota_exceeded", Some(http_status));
            return Err(app_err);
        }
    }

    let ir_request = match codec.decode_request(body, &raw_env) {
        Ok(r) => r,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_REQUEST, format!("Decode error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("decode_error", Some(http_status));
            return Err(app_err);
        }
    };
    let virtual_model = ir_request.model.clone();
    let is_stream = ir_request.stream;
    scope.set_virtual_model(virtual_model.clone());

    // Resolve route
    let targets = match state.current_config().routing_table.resolve(&virtual_model) {
        Some(t) => t,
        None => {
            let app_err = AppError::new(
                StatusCode::NOT_FOUND,
                format!("No route found for model: {virtual_model}"),
            );
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("route_not_found", Some(http_status));
            return Err(app_err);
        }
    };

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
            let app_err = AppError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "request deadline exceeded".to_string(),
            );
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("deadline_exceeded", Some(http_status));
            return Err(app_err);
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

        // Bind the resolved target on the scope so the terminal
        // RequestEvent attributes the row to the right upstream.
        scope.set_egress(target.api_protocol.clone());
        scope.set_resolved(target.provider_id.clone(), target.model_id.clone());

        match execute_messages_upstream(
            &state,
            &codec,
            &ingress_protocol,
            &ir_request,
            target,
            is_stream,
            raw_passthrough_body.as_deref(),
            &trace_ctx,
            &request_id,
            &headers,
        )
        .await
        {
            Ok(response) => {
                state.health.record_success(&health_key);
                // Phase 4 §4.2: terminal `RequestEvent` via the scope.
                scope.emit_ok(Some(response.status().as_u16()));
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
                    FallbackDecision::Fail => {
                        let http_status = app_err.http_status().as_u16();
                        let error_class = format!("{:?}", classification.class);
                        scope.emit_error(&error_class, Some(http_status));
                        return Err(app_err);
                    }
                }
            }
        }
    }

    let final_err = last_error.unwrap_or_else(|| {
        AppError::new(
            StatusCode::BAD_GATEWAY,
            "all upstream targets exhausted".to_string(),
        )
    });
    let http_status = final_err.http_status().as_u16();
    scope.emit_error("upstream_exhausted", Some(http_status));
    Err(final_err)
}

/// Convert an `http::HeaderMap` into an ordered `Vec<(name, value)>`
/// for `ExchangeCapture`. Non-UTF8 header values are rendered lossily.
fn header_map_to_vec(headers: &http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

/// Convert a reqwest response `HeaderMap` into an ordered Vec.
fn reqwest_headers_to_vec(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

/// Merge client request headers into the upstream header map per the
/// denylist forwarding policy (C→G→P). Called *after* the codec / auth
/// have populated `upstream_headers` and *before* `apply_provider_auth`
/// runs, so a forwarded client header never overwrites a header the
/// gateway already set (codec content-type, etc.) and auth injection
/// always wins last. Headers blocked by the policy (credentials,
/// hop-by-hop, gateway-controlled, trace) are skipped.
fn merge_client_headers(
    client: &http::HeaderMap,
    upstream: &mut http::HeaderMap,
    policy: &tiygate_core::HeaderForwardPolicy,
) {
    for (name, value) in client.iter() {
        let name_str = name.as_str();
        if !policy.should_forward_request(name_str) {
            continue;
        }
        // Do not clobber a header the codec already set for the
        // upstream request (e.g. content-type).
        if upstream.contains_key(name) {
            continue;
        }
        upstream.insert(name.clone(), value.clone());
    }
}

/// Forward upstream response headers to the client response per the
/// denylist forwarding policy (P→G→C). The upstream headers are passed
/// as the already-snapshotted `Vec<(name, value)>` (captured before the
/// reqwest response body/object is consumed). Headers blocked by the
/// policy (hop-by-hop, length/encoding, framework-controlled) are
/// skipped; everything else is inserted onto the client response.
fn forward_upstream_resp_headers(
    resp: &mut Response,
    upstream_headers: &[(String, String)],
    policy: &tiygate_core::HeaderForwardPolicy,
) {
    for (name, value) in upstream_headers {
        if !policy.should_forward_response(name) {
            continue;
        }
        if let (Ok(hn), Ok(hv)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            resp.headers_mut().insert(hn, hv);
        }
    }
}

/// Filter a snapshotted upstream response header list down to the set
/// that is actually forwarded to the client, for the request-log
/// `client_resp_headers` capture on the streaming path.
fn forwarded_resp_headers_for_capture(
    upstream_headers: &[(String, String)],
    policy: &tiygate_core::HeaderForwardPolicy,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = upstream_headers
        .iter()
        .filter(|(name, _)| policy.should_forward_response(name))
        .cloned()
        .collect();
    // The Sse response sets content-type itself; reflect that in the
    // recorded client_resp_headers so the log matches the wire.
    out.push(("content-type".to_string(), "text/event-stream".to_string()));
    out
}

/// Overwrite the `model` field of an upstream request body with the
/// routing target's real upstream model id.
///
/// The client may send a *virtual* model name (used only for routing);
/// the upstream provider must receive `target.model_id`. We only replace
/// the value when the body is a JSON object that already carries a
/// `model` key — so Gemini egress (model lives in the URL, body has no
/// `model`) is left untouched and we never inject a spurious field.
///
/// Returns `true` when the body's `model` value was actually changed.
/// Callers use this to decide whether a PassThrough body can still be
/// forwarded byte-for-byte (no change) or must be re-serialized (changed).
fn override_model_in_body(body: &mut serde_json::Value, model_id: &str) -> bool {
    if let Some(obj) = body.as_object_mut() {
        if let Some(existing) = obj.get("model") {
            if existing.as_str() == Some(model_id) {
                return false;
            }
            obj.insert("model".to_string(), serde_json::json!(model_id));
            return true;
        }
    }
    false
}

/// Fire-and-forget: send an `ExchangeCapture` to the telemetry bus.
/// The bus uses a non-blocking `try_send`, so this never stalls the
/// request hot path; the background drain task redacts + persists.
fn spawn_capture(state: &AppState, capture: tiygate_core::ExchangeCapture) {
    let bus = state.telemetry.clone();
    tokio::spawn(async move {
        bus.send_capture(capture).await;
    });
}

/// Execute an upstream OpenAI-compatible request.
#[allow(clippy::too_many_arguments)]
async fn execute_upstream(
    state: &AppState,
    codec: &ChatCompletionsCodec,
    ingress_protocol: &tiygate_core::ProtocolEndpoint,
    ir_request: &IrRequest,
    target: &tiygate_core::RoutingTarget,
    is_stream: bool,
    raw_passthrough_body: Option<&str>,
    trace: &TraceContext,
    request_id: &str,
    client_headers: &http::HeaderMap,
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
    let (mut upstream_body, mut upstream_headers) = if let Some(raw) = raw_passthrough_body {
        if is_same_protocol {
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
            // The raw passthrough body was eligible because *some* target
            // shares the ingress suite, but this specific target is
            // cross-protocol — convert from IR instead of forwarding bytes.
            encode_cross_protocol(codec, &egress_protocol, ir_request)?
        }
    } else if is_same_protocol {
        codec.encode_request(ir_request).map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Encode error: {}", e),
            )
        })?
    } else {
        encode_cross_protocol(codec, &egress_protocol, ir_request)?
    };

    // Replace the (possibly virtual) model name with the routing
    // target's real upstream model id before sending and before we
    // snapshot the egress body for the request-log detail view.
    let model_was_overridden = override_model_in_body(&mut upstream_body, &target.model_id);
    // PassThrough can only forward the raw client bytes verbatim when the
    // model name did not change. If we rewrote `model`, the raw body is
    // stale and we must send the re-serialized `upstream_body` instead.
    let pass_through_verbatim = is_pass_through && !model_was_overridden;

    // Apply auth via the registered provider's AuthApplier. Falls
    // back to a static `Bearer {api_key}` if no provider is registered
    // for `target.provider_id` (e.g., test fixtures or built-in
    // OpenAI-compatible endpoints that don't need OAuth).
    //
    // First merge forwardable client request headers (denylist policy),
    // then apply auth so gateway-injected credentials always win.
    merge_client_headers(client_headers, &mut upstream_headers, &state.header_policy);
    apply_provider_auth(target, &mut upstream_headers).await?;

    // Capture the egress request (headers + body) for the request-log
    // detail view. We snapshot here, *after* auth injection and just
    // before the headers are moved into the reqwest builder, then add
    // the `traceparent` that `inject_trace` stamps on the builder so
    // the captured set matches what is actually sent. Redaction +
    // truncation happen later on the telemetry background task.
    // The egress *headers* are captured from the built `reqwest::Request`
    // (see `finalize_egress` below) so the snapshot includes every
    // header reqwest adds at finalize time (content-type, content-length,
    // traceparent, auth). The body snapshot is taken here.
    let egress_body_capture = if pass_through_verbatim {
        raw_passthrough_body.map(|s| s.to_string())
    } else {
        serde_json::to_string(&upstream_body).ok()
    };

    let client = &state.http_client;
    // Address the upstream by the *egress* protocol (the target provider's
    // protocol), not the ingress entrypoint. When a chat-completions request
    // is routed to an Anthropic provider, the body is converted above and
    // must be POSTed to `/messages`, not `/chat/completions`. Google Gemini
    // has no fixed suffix — its URL embeds the model and method, and the
    // streaming variant uses `:streamGenerateContent?alt=sse`.
    let upstream_url = if is_stream {
        upstream_stream_url_for_suite(target, egress_protocol.suite)
    } else {
        gemini_aware_upstream_url(target, egress_protocol.suite)
    }
    .ok_or_else(|| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "No upstream path for egress protocol suite: {:?}",
                egress_protocol.suite
            ),
        )
    })?;

    if is_stream {
        // PassThrough: forward raw body bytes verbatim (no re-serialize).
        // Non-PassThrough: re-serialize via reqwest::Client::json().
        // `inject_trace` stamps `traceparent` on the builder so the
        // upstream service sees the same trace id as the downstream.
        let mut stream_req = crate::ingress_phase4::inject_trace(
            client
                .post(&upstream_url)
                .headers(upstream_headers)
                .timeout(state.request_read_timeout),
            trace,
        );
        if pass_through_verbatim {
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
        // Freeze the request and snapshot the complete egress header set.
        let (egress_req, egress_headers_capture, egress_method, egress_path) =
            crate::ingress_phase4::finalize_egress(stream_req)?;
        let response = client.execute(egress_req).await.map_err(|e| {
            AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {}", e))
        })?;

        // Extract Retry-After for passthrough
        let retry_after = extract_retry_after(response.headers());
        let rate_limit_headers_vec: Vec<(&'static str, String)> =
            extract_rate_limit_headers(response.headers());
        let status = response.status();
        let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
        let upstream_status_capture = status.as_u16();

        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            // Capture the failed streaming exchange (the error body is
            // not an SSE stream, so store it verbatim).
            spawn_capture(
                state,
                tiygate_core::ExchangeCapture {
                    request_id: request_id.to_string(),
                    egress_method: egress_method.clone(),
                    egress_path: egress_path.clone(),
                    egress_headers: egress_headers_capture.clone(),
                    egress_body: egress_body_capture.clone(),
                    upstream_status: Some(upstream_status_capture),
                    upstream_resp_headers: upstream_resp_headers_capture.clone(),
                    upstream_resp_body: Some(error_body.clone()),
                    client_resp_headers: Vec::new(),
                    client_resp_body: None,
                    is_stream: true,
                },
            );
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
        let error_marker = err_enc.encode_error(
            "upstream stream truncated by gateway",
            Some("upstream_timeout"),
        );

        let forwarded_resp_headers = forwarded_resp_headers_for_capture(
            &upstream_resp_headers_capture,
            &state.header_policy,
        );
        let upstream_resp_headers_for_forward = upstream_resp_headers_capture.clone();
        let mut response = drive_upstream_stream(
            state,
            accum,
            response,
            end_marker,
            error_marker,
            Duration::from_secs(state.upstream_stream_idle_timeout_secs),
            Duration::from_secs(state.upstream_stream_total_timeout_secs),
            DEFAULT_SSE_KEEPALIVE_INTERVAL,
            Some(StreamCapture {
                request_id: request_id.to_string(),
                telemetry: state.telemetry.clone(),
                egress_method: egress_method.to_string(),
                egress_path: egress_path.to_string(),
                egress_headers: egress_headers_capture,
                egress_body: egress_body_capture,
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture,
                client_resp_headers: forwarded_resp_headers,
                max_bytes: state.raw_envelope_max_bytes as usize,
            }),
            build_stream_transcode(ingress_protocol, &egress_protocol),
        );
        // Forward upstream response headers to the client (denylist).
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_for_forward,
            &state.header_policy,
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
        // `inject_trace` stamps `traceparent` on the builder so the
        // upstream service sees the same trace id as the downstream.
        let mut nonstream_req = crate::ingress_phase4::inject_trace(
            client
                .post(&upstream_url)
                .headers(upstream_headers)
                .timeout(state.request_read_timeout),
            trace,
        );
        if pass_through_verbatim {
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
        // Freeze the request and snapshot the complete egress header set.
        let (egress_req, egress_headers_capture, egress_method, egress_path) =
            crate::ingress_phase4::finalize_egress(nonstream_req)?;
        let response = client.execute(egress_req).await.map_err(|e| {
            AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {}", e))
        })?;

        let retry_after = extract_retry_after(response.headers());
        let rate_limit_headers_vec: Vec<(&'static str, String)> =
            extract_rate_limit_headers(response.headers());
        let status = response.status();
        // Snapshot upstream response headers before `.json()` consumes
        // the body, for the request-log detail view.
        let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
        let upstream_status_capture = status.as_u16();
        let response_body: Value = response
            .json()
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Parse error: {}", e)))?;

        if !status.is_success() {
            // Capture the failed exchange (upstream error body) so the
            // detail view shows what the provider returned.
            spawn_capture(
                state,
                tiygate_core::ExchangeCapture {
                    request_id: request_id.to_string(),
                    egress_method: egress_method.clone(),
                    egress_path: egress_path.clone(),
                    egress_headers: egress_headers_capture.clone(),
                    egress_body: egress_body_capture.clone(),
                    upstream_status: Some(upstream_status_capture),
                    upstream_resp_headers: upstream_resp_headers_capture.clone(),
                    upstream_resp_body: serde_json::to_string(&response_body).ok(),
                    client_resp_headers: Vec::new(),
                    client_resp_body: None,
                    is_stream: false,
                },
            );
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

        // Keep a copy of the raw upstream body for the capture before
        // any cross-protocol re-encoding.
        let upstream_resp_body_capture = serde_json::to_string(&response_body).ok();

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

        let client_resp_body_capture = serde_json::to_string(&response_json).ok();
        let mut response = Json(response_json).into_response();
        // Forward upstream response headers to the client (denylist).
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_capture,
            &state.header_policy,
        );
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
        // Capture the full successful exchange for the detail view.
        spawn_capture(
            state,
            tiygate_core::ExchangeCapture {
                request_id: request_id.to_string(),
                egress_method: egress_method.clone(),
                egress_path: egress_path.clone(),
                egress_headers: egress_headers_capture,
                egress_body: egress_body_capture,
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture,
                upstream_resp_body: upstream_resp_body_capture,
                client_resp_headers: header_map_to_vec(response.headers()),
                client_resp_body: client_resp_body_capture,
                is_stream: false,
            },
        );
        Ok(response)
    }
}

/// Execute an upstream Anthropic Messages request.
#[allow(clippy::too_many_arguments)]
async fn execute_messages_upstream(
    state: &AppState,
    codec: &MessagesCodec,
    ingress_protocol: &tiygate_core::ProtocolEndpoint,
    ir_request: &IrRequest,
    target: &tiygate_core::RoutingTarget,
    is_stream: bool,
    raw_passthrough_body: Option<&str>,
    trace: &TraceContext,
    request_id: &str,
    client_headers: &http::HeaderMap,
) -> Result<Response, AppError> {
    let egress_protocol = target.api_protocol.clone();
    let is_same_protocol = ingress_protocol.suite == egress_protocol.suite;
    let is_pass_through = raw_passthrough_body.is_some() && is_same_protocol;
    // PassThrough: forward raw body bytes verbatim. Same-protocol: re-encode
    // via the ingress codec. Cross-protocol: convert IR → egress format via
    // the egress codec (e.g. Anthropic Messages → OpenAI chat-completions),
    // mirroring `execute_upstream`.
    let (mut upstream_body, mut upstream_headers) = if let Some(raw) = raw_passthrough_body {
        if is_same_protocol {
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
            encode_cross_protocol(codec, &egress_protocol, ir_request)?
        }
    } else if is_same_protocol {
        codec.encode_request(ir_request).map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Encode error: {}", e),
            )
        })?
    } else {
        encode_cross_protocol(codec, &egress_protocol, ir_request)?
    };

    // Replace the (possibly virtual) model name with the routing
    // target's real upstream model id.
    let model_was_overridden = override_model_in_body(&mut upstream_body, &target.model_id);
    // PassThrough forwards raw bytes verbatim only when `model` was
    // unchanged; otherwise we must send the re-serialized body.
    let pass_through_verbatim = is_pass_through && !model_was_overridden;

    // Apply auth via the registered provider's AuthApplier. For
    // Anthropic, this inserts the x-api-key header. The
    // `anthropic-version` header is added by the MessagesCodec's
    // `encode_request` (see protocol/messages.rs), so it survives
    // here.
    //
    // Merge forwardable client request headers first, then auth so
    // gateway-injected credentials always win.
    merge_client_headers(client_headers, &mut upstream_headers, &state.header_policy);
    apply_provider_auth(target, &mut upstream_headers).await?;

    // Capture egress request (headers + body) for the detail view.
    let egress_body_capture = if pass_through_verbatim {
        raw_passthrough_body.map(|s| s.to_string())
    } else {
        serde_json::to_string(&upstream_body).ok()
    };

    let client = &state.http_client;
    // Address the upstream by the *egress* protocol, not the ingress
    // entrypoint. A `/v1/messages` request routed to an OpenAI provider is
    // converted above and must be POSTed to `/chat/completions`. Gemini
    // egress embeds the model and method (stream vs non-stream) in the URL.
    let upstream_url = if is_stream {
        upstream_stream_url_for_suite(target, egress_protocol.suite)
    } else {
        gemini_aware_upstream_url(target, egress_protocol.suite)
    }
    .ok_or_else(|| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "No upstream path for egress protocol suite: {:?}",
                egress_protocol.suite
            ),
        )
    })?;

    if is_stream {
        let mut stream_req = crate::ingress_phase4::inject_trace(
            client
                .post(&upstream_url)
                .headers(upstream_headers)
                .timeout(state.request_read_timeout),
            trace,
        );
        if pass_through_verbatim {
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
        // Freeze the request and snapshot the complete egress header set.
        let (egress_req, egress_headers_capture, egress_method, egress_path) =
            crate::ingress_phase4::finalize_egress(stream_req)?;
        let response = client.execute(egress_req).await.map_err(|e| {
            AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {}", e))
        })?;

        let retry_after = extract_retry_after(response.headers());
        let status = response.status();
        let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
        let upstream_status_capture = status.as_u16();

        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            spawn_capture(
                state,
                tiygate_core::ExchangeCapture {
                    request_id: request_id.to_string(),
                    egress_method: egress_method.clone(),
                    egress_path: egress_path.clone(),
                    egress_headers: egress_headers_capture.clone(),
                    egress_body: egress_body_capture.clone(),
                    upstream_status: Some(upstream_status_capture),
                    upstream_resp_headers: upstream_resp_headers_capture.clone(),
                    upstream_resp_body: Some(error_body.clone()),
                    client_resp_headers: Vec::new(),
                    client_resp_body: None,
                    is_stream: true,
                },
            );
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

        let accum =
            std::sync::Arc::new(std::sync::Mutex::new(tiygate_core::UsageAccumulator::new()));

        // Build the protocol-native end / error frames from the egress
        // codec. The streaming helper writes the right one for each
        // termination reason (natural end → end frame, idle / total /
        // upstream error → error frame).
        let mut end_enc = codec.stream_encoder();
        let mut err_enc = codec.stream_encoder();
        let end_marker = end_enc.encode_done();
        let error_marker = err_enc.encode_error(
            "upstream stream truncated by gateway",
            Some("upstream_timeout"),
        );

        let forwarded_resp_headers = forwarded_resp_headers_for_capture(
            &upstream_resp_headers_capture,
            &state.header_policy,
        );
        let upstream_resp_headers_for_forward = upstream_resp_headers_capture.clone();
        let mut response = drive_upstream_stream(
            state,
            accum,
            response,
            end_marker,
            error_marker,
            Duration::from_secs(state.upstream_stream_idle_timeout_secs),
            Duration::from_secs(state.upstream_stream_total_timeout_secs),
            DEFAULT_SSE_KEEPALIVE_INTERVAL,
            Some(StreamCapture {
                request_id: request_id.to_string(),
                telemetry: state.telemetry.clone(),
                egress_method: egress_method.to_string(),
                egress_path: egress_path.to_string(),
                egress_headers: egress_headers_capture,
                egress_body: egress_body_capture,
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture,
                client_resp_headers: forwarded_resp_headers,
                max_bytes: state.raw_envelope_max_bytes as usize,
            }),
            build_stream_transcode(ingress_protocol, &egress_protocol),
        );
        // Forward upstream response headers to the client (denylist).
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_for_forward,
            &state.header_policy,
        );
        if let Some(ra) = retry_after {
            response.headers_mut().insert(
                http::HeaderName::from_static("retry-after"),
                http::HeaderValue::from_str(&ra).unwrap_or(http::HeaderValue::from_static("")),
            );
        }
        Ok(response)
    } else {
        let mut nonstream_req = crate::ingress_phase4::inject_trace(
            client.post(&upstream_url).headers(upstream_headers),
            trace,
        );
        if pass_through_verbatim {
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
        // Freeze the request and snapshot the complete egress header set.
        let (egress_req, egress_headers_capture, egress_method, egress_path) =
            crate::ingress_phase4::finalize_egress(nonstream_req)?;
        let response = client
            .execute(egress_req)
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {}", e)))?;

        let retry_after = extract_retry_after(response.headers());
        let rate_limit_headers_vec: Vec<(&'static str, String)> =
            extract_rate_limit_headers(response.headers());
        let status = response.status();
        let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
        let upstream_status_capture = status.as_u16();
        let response_body: Value = response
            .json()
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Parse error: {}", e)))?;

        if !status.is_success() {
            spawn_capture(
                state,
                tiygate_core::ExchangeCapture {
                    request_id: request_id.to_string(),
                    egress_method: egress_method.clone(),
                    egress_path: egress_path.clone(),
                    egress_headers: egress_headers_capture.clone(),
                    egress_body: egress_body_capture.clone(),
                    upstream_status: Some(upstream_status_capture),
                    upstream_resp_headers: upstream_resp_headers_capture.clone(),
                    upstream_resp_body: serde_json::to_string(&response_body).ok(),
                    client_resp_headers: Vec::new(),
                    client_resp_body: None,
                    is_stream: false,
                },
            );
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

        let upstream_resp_body_capture = serde_json::to_string(&response_body).ok();

        // Cross-protocol re-encoding: when the upstream spoke a different
        // protocol (e.g. OpenAI chat-completions) than the client's ingress
        // (Anthropic Messages), decode the upstream body via the egress codec
        // and re-encode it into the ingress protocol so the client sees the
        // format it expects. Mirrors `execute_upstream`.
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

        let client_resp_body_capture = serde_json::to_string(&response_json).ok();
        let mut response = Json(response_json).into_response();
        // Forward upstream response headers to the client (denylist).
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_capture,
            &state.header_policy,
        );
        if let Some(ra) = retry_after {
            response.headers_mut().insert(
                http::HeaderName::from_static("retry-after"),
                http::HeaderValue::from_str(&ra).unwrap_or(http::HeaderValue::from_static("")),
            );
        }
        spawn_capture(
            state,
            tiygate_core::ExchangeCapture {
                request_id: request_id.to_string(),
                egress_method: egress_method.clone(),
                egress_path: egress_path.clone(),
                egress_headers: egress_headers_capture,
                egress_body: egress_body_capture,
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture,
                upstream_resp_body: upstream_resp_body_capture,
                client_resp_headers: header_map_to_vec(response.headers()),
                client_resp_body: client_resp_body_capture,
                is_stream: false,
            },
        );
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
        tiygate_core::ProtocolSuite::GoogleGemini => Some(Box::new(GeminiCodec::new())),
        tiygate_core::ProtocolSuite::OpenAiResponses => Some(Box::new(ResponsesCodec::new())),
    }
}

/// Build the non-streaming upstream URL by egress suite, with Gemini support.
/// Google Gemini's non-streaming URL embeds the model and uses the
/// `:generateContent` method; the other suites have a fixed path suffix.
fn gemini_aware_upstream_url(
    target: &tiygate_core::RoutingTarget,
    suite: tiygate_core::ProtocolSuite,
) -> Option<String> {
    match suite {
        tiygate_core::ProtocolSuite::GoogleGemini => Some(format!(
            "{}/v1beta/models/{}:generateContent",
            target.effective_api_base().trim_end_matches('/'),
            target.model_id
        )),
        _ => upstream_url_for_suite(target, suite),
    }
}

/// Build a [`StreamTranscode`] for a streaming response when the ingress and
/// egress protocol suites differ. Returns `None` for same-protocol streams so
/// the caller forwards bytes verbatim (zero-loss fast path). The egress codec
/// supplies the upstream decoder; the ingress codec supplies the client
/// encoder. Returns `None` (verbatim) if either codec is unavailable rather
/// than failing the request.
fn build_stream_transcode(
    ingress_protocol: &tiygate_core::ProtocolEndpoint,
    egress_protocol: &tiygate_core::ProtocolEndpoint,
) -> Option<StreamTranscode> {
    if ingress_protocol.suite == egress_protocol.suite {
        return None;
    }
    let egress_codec = get_egress_codec(egress_protocol)?;
    let ingress_codec = get_egress_codec(ingress_protocol)?;
    Some(StreamTranscode {
        decoder: egress_codec.stream_decoder(),
        encoder: ingress_codec.stream_encoder(),
    })
}

/// Build the upstream URL for a *streaming* chat-style request, addressed by
/// the egress protocol suite. Identical to [`upstream_url_for_suite`] for the
/// fixed-suffix suites (chat-completions, responses, anthropic messages), but
/// Google Gemini has no fixed suffix — its URL embeds the model and uses the
/// `:streamGenerateContent` method plus the `?alt=sse` query string to switch
/// the endpoint into Server-Sent Events mode. Returns `None` only if the base
/// URL cannot be formed.
fn upstream_stream_url_for_suite(
    target: &tiygate_core::RoutingTarget,
    suite: tiygate_core::ProtocolSuite,
) -> Option<String> {
    match suite {
        tiygate_core::ProtocolSuite::GoogleGemini => Some(format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            target.effective_api_base().trim_end_matches('/'),
            target.model_id
        )),
        _ => upstream_url_for_suite(target, suite),
    }
}

/// Build the upstream URL for a chat-style request, addressed by the *egress*
/// protocol suite (the target provider's protocol) rather than the ingress
/// entrypoint. Returns `None` for suites that have no fixed path suffix
/// (e.g. Google Gemini, whose URL embeds the model and method).
fn upstream_url_for_suite(
    target: &tiygate_core::RoutingTarget,
    suite: tiygate_core::ProtocolSuite,
) -> Option<String> {
    suite.upstream_path_suffix().map(|suffix| {
        format!(
            "{}{}",
            target.effective_api_base().trim_end_matches('/'),
            suffix
        )
    })
}

/// Convert an IR request into the egress protocol's wire format, running the
/// field-level lossy-conversion check first. Shared by the chat-completions
/// and messages egress paths so cross-protocol routing behaves identically
/// regardless of the ingress entrypoint.
fn encode_cross_protocol<C: EndpointCodec + ?Sized>(
    ingress_codec: &C,
    egress_protocol: &tiygate_core::ProtocolEndpoint,
    ir_request: &IrRequest,
) -> Result<(serde_json::Value, http::HeaderMap), AppError> {
    let egress_codec = get_egress_codec(egress_protocol).ok_or_else(|| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("No codec for protocol: {:?}", egress_protocol),
        )
    })?;

    let ingress_caps = ingress_codec.capabilities();
    let egress_caps = egress_codec.capabilities();
    if ingress_caps.lossy_default_reject || egress_caps.lossy_default_reject {
        if let Err((dim, err)) = tiygate_core::protocol::lossy::check_lossy_conversion(
            ir_request,
            egress_protocol,
            egress_caps,
        ) {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                format!(
                    "Lossy conversion rejected: {err} (dimension: {})",
                    dim.label()
                ),
            ));
        }
    }

    egress_codec.encode_request(ir_request).map_err(|e| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Encode error: {}", e),
        )
    })
}

/// Handle POST /v1/embeddings.
///
/// Phase 4 wiring (§4.7 + §4.1 + §4.8):
/// 1. Build a *redacted, truncated* `RawEnvelope` for the audit log.
/// 2. Extract (or mint) the W3C trace context.
/// 3. Check the embedding cache; on hit, serve the cached value
///    and emit a `RequestEvent` with `cache_hit = hit`.
/// 4. On miss, build the upstream request, inject the
///    `traceparent` header, call the upstream, store the response,
///    and emit a `RequestEvent` with `cache_hit = miss` and
///    `latency_ms` populated.
async fn handle_embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let _permit = acquire_permit(&state).await?;

    let codec = EmbeddingsCodec::new();
    let ingress_protocol = codec.id().clone();
    let raw_env = crate::ingress_phase4::build_redacted_envelope(
        &state,
        "POST",
        "/v1/embeddings",
        &body,
        &headers,
    );
    let _raw_traceparent = headers
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let trace_ctx = crate::ingress_phase4::extract_trace(&headers);

    // Wall-clock anchor + scope so every return path emits a
    // terminal `RequestEvent` (parity with the other 4 handlers).
    // The `started` clock is also used for the `latency_ms` column
    // on the miss / hit events below.
    let started = Instant::now();
    let request_id = uuid::Uuid::now_v7().to_string();
    let mut scope = crate::ingress_phase4::RequestScope::new(
        &state,
        request_id,
        "unknown",
        ingress_protocol.clone(),
        trace_ctx.clone(),
        started,
    );
    // Persist the redacted envelope on the terminal RequestEvent
    // for audit / replay (§8 #3 / #8).
    scope.set_envelope(raw_env.clone());

    // Phase 4 §4.6: api key resolution + quota enforcement, parity
    // with the chat/messages/responses/gemini handlers. Embedding
    // requests count against the same `requests_per_minute` /
    // `requests_per_day` bucket as chat completions.
    let api_key = crate::ingress_phase4::resolve_api_key(&state, &headers).await;
    scope.set_api_key_id(api_key.key_id.clone());
    match crate::ingress_phase4::check_quota(&state, &api_key.key_id, &api_key.spec, 1).await {
        crate::ingress_phase4::QuotaOutcome::Allow => {}
        crate::ingress_phase4::QuotaOutcome::Deny { retry_after, .. } => {
            let app_err =
                AppError::new(StatusCode::TOO_MANY_REQUESTS, "quota exceeded".to_string())
                    .with_retry_after(retry_after.as_secs().max(1));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("quota_exceeded", Some(http_status));
            return Err(app_err);
        }
    }

    // Build the cache key from the body. We don't need to fully
    // decode the request to know the cache key — the model and
    // input are at the top level of the OpenAI embeddings schema.
    let model_for_cache = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();
    let input_for_cache = body.get("input").map(|v| v.to_string()).unwrap_or_default();
    scope.set_virtual_model(model_for_cache.clone());
    let cache_key = tiygate_cache::embedding_cache::EmbeddingCacheKey::new(
        model_for_cache.clone(),
        input_for_cache,
    );

    // Cache lookup.
    if let Some(cached) = crate::ingress_phase4::embedding_cache_lookup(&state, &cache_key).await {
        // Emit a hit event through the scope (which now also
        // knows the cache_hit column) so the OltpSink persists
        // a row with `cache_hit = hit`. We pass the hit status
        // to the scope via a custom helper because `emit_ok` only
        // takes an http_status; the cache_hit column is filled
        // in by the underlying `emit_request_event` call.
        let latency_ms = tiygate_core::telemetry::LatencyBreakdown {
            total_ms: started.elapsed().as_millis() as u64,
            upstream_ms: 0,
            queue_ms: 0,
        };
        crate::ingress_phase4::emit_request_event(
            &state,
            scope.request_id(),
            &model_for_cache,
            None,
            None,
            codec.id(),
            None,
            "ok",
            None,
            None,
            Some(200),
            false,
            Some("hit"),
            latency_ms,
            None,
            None,
            Some(&api_key.key_id),
            &trace_ctx,
            Some(&raw_env),
        );
        scope.disarm();
        return Ok(Json((*cached).clone()).into_response());
    }

    let ir_request = match codec.decode_request(body, &raw_env) {
        Ok(r) => r,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_REQUEST, format!("Decode error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("decode_error", Some(http_status));
            return Err(app_err);
        }
    };

    let virtual_model = ir_request.model.clone();
    scope.set_virtual_model(virtual_model.clone());
    let targets = match state.current_config().routing_table.resolve(&virtual_model) {
        Some(t) => t,
        None => {
            let app_err = AppError::new(
                StatusCode::NOT_FOUND,
                format!("No route found for model: {virtual_model}"),
            );
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("route_not_found", Some(http_status));
            return Err(app_err);
        }
    };

    let target = match targets.first() {
        Some(t) => t,
        None => {
            let app_err =
                AppError::new(StatusCode::BAD_GATEWAY, "no targets configured".to_string());
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("no_targets", Some(http_status));
            return Err(app_err);
        }
    };
    scope.set_egress(target.api_protocol.clone());
    scope.set_resolved(target.provider_id.clone(), target.model_id.clone());

    let (mut upstream_body, mut upstream_headers) = match codec.encode_request(&ir_request) {
        Ok(b) => b,
        Err(e) => {
            let app_err = AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Encode error: {e}"),
            );
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("encode_error", Some(http_status));
            return Err(app_err);
        }
    };

    // Replace the (possibly virtual) model name with the routing
    // target's real upstream model id.
    override_model_in_body(&mut upstream_body, &target.model_id);

    merge_client_headers(&headers, &mut upstream_headers, &state.header_policy);
    if let Err(e) = apply_provider_auth(target, &mut upstream_headers).await {
        let http_status = e.http_status().as_u16();
        scope.emit_error("auth_error", Some(http_status));
        return Err(e);
    }
    // Capture egress request (headers + body) for the detail view.
    let egress_body_capture = serde_json::to_string(&upstream_body).ok();
    let req_id_capture = scope.request_id().to_string();

    let client = &state.http_client;
    let upstream_url = format!("{}/embeddings", target.effective_api_base());

    // Build the upstream request manually so we can inject the
    // `traceparent` header before sending. `inject_trace` stamps the
    // header on the builder so it survives the JSON body merge below.
    // `finalize_egress` freezes the builder into the concrete request
    // and snapshots its complete header set (content-type,
    // content-length, traceparent, auth) for the request-log detail view.
    let builder = crate::ingress_phase4::inject_trace(client.post(&upstream_url), &trace_ctx)
        .headers(upstream_headers)
        .json(&upstream_body);
    let (req, egress_headers_capture, egress_method, egress_path) =
        match crate::ingress_phase4::finalize_egress(builder) {
        Ok(r) => r,
        Err(app_err) => {
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("upstream_send_error", Some(http_status));
            return Err(app_err);
        }
    };
    let response = match client.execute(req).await {
        Ok(r) => r,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("upstream_send_error", Some(http_status));
            return Err(app_err);
        }
    };

    let status = response.status();
    let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
    let upstream_status_capture = status.as_u16();
    let response_body: Value = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_GATEWAY, format!("Parse error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("upstream_parse_error", Some(http_status));
            return Err(app_err);
        }
    };

    if !status.is_success() {
        spawn_capture(
            &state,
            tiygate_core::ExchangeCapture {
                request_id: req_id_capture.clone(),
                egress_method: egress_method.clone(),
                egress_path: egress_path.clone(),
                    egress_headers: egress_headers_capture.clone(),
                egress_body: egress_body_capture.clone(),
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture.clone(),
                upstream_resp_body: serde_json::to_string(&response_body).ok(),
                client_resp_headers: Vec::new(),
                client_resp_body: None,
                is_stream: false,
            },
        );
        let app_err = AppError::new(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            format!(
                "Upstream error: {}",
                response_body["error"]["message"]
                    .as_str()
                    .unwrap_or("Unknown error")
            ),
        );
        let http_status = app_err.http_status().as_u16();
        scope.emit_error("upstream_error", Some(http_status));
        return Err(app_err);
    }

    state.health.record_success(&target.health_key());

    // Capture the full successful embeddings exchange for the detail
    // view (client body == upstream body, no re-encoding here).
    let body_str_capture = serde_json::to_string(&response_body).ok();
    // Build the client response and forward upstream response headers
    // (denylist) so the recorded client_resp_headers match the wire.
    let mut response = Json(response_body.clone()).into_response();
    forward_upstream_resp_headers(
        &mut response,
        &upstream_resp_headers_capture,
        &state.header_policy,
    );
    spawn_capture(
        &state,
        tiygate_core::ExchangeCapture {
            request_id: req_id_capture,
            egress_method: egress_method.to_string(),
            egress_path: egress_path.to_string(),
            egress_headers: egress_headers_capture,
            egress_body: egress_body_capture,
            upstream_status: Some(upstream_status_capture),
            upstream_resp_headers: upstream_resp_headers_capture,
            upstream_resp_body: body_str_capture.clone(),
            client_resp_headers: header_map_to_vec(response.headers()),
            client_resp_body: body_str_capture,
            is_stream: false,
        },
    );

    // Phase 4 §4.7: store the upstream response for the next call.
    crate::ingress_phase4::embedding_cache_store(&state, &cache_key, response_body).await;

    // Phase 4 §4.2: emit a `RequestEvent` with `cache_hit = miss`
    // so the OltpSink persists the row and the dashboard can
    // aggregate. We use the explicit `emit_request_event` form
    // here (instead of `scope.emit_ok`) because the cache-hit
    // column is a *miss* on this path; the scope is disarmed so
    // Drop is a no-op.
    let latency_ms = tiygate_core::telemetry::LatencyBreakdown {
        total_ms: started.elapsed().as_millis() as u64,
        upstream_ms: started.elapsed().as_millis() as u64,
        queue_ms: 0,
    };
    crate::ingress_phase4::emit_request_event(
        &state,
        scope.request_id(),
        &virtual_model,
        Some(target.provider_id.as_str()),
        Some(target.model_id.as_str()),
        codec.id(),
        Some(&target.api_protocol),
        "ok",
        None,
        None,
        Some(status.as_u16()),
        false,
        Some("miss"),
        latency_ms,
        None,
        None,
        Some(&api_key.key_id),
        &trace_ctx,
        Some(&raw_env),
    );
    scope.disarm();

    Ok(response)
}

/// Handle POST /v1/responses — OpenAI Responses API.
///
/// Mirrors `handle_chat_completions` but uses `ResponsesCodec`. The
/// egress pipeline is the same: per-route body limit, route resolve,
/// fallback / retry, RateLimit-* passthrough. A `RequestScope` drop
/// guard ensures the terminal `RequestEvent` is emitted on every
/// return path (success, upstream error, decode / route / encode
/// failure).
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

    let trace_ctx = crate::ingress_phase4::extract_trace(&headers);
    let raw_env = crate::ingress_phase4::build_redacted_envelope(
        &state,
        "POST",
        "/v1/responses",
        &body,
        &headers,
    );

    let started = Instant::now();
    let request_id = uuid::Uuid::now_v7().to_string();
    let virtual_model_hint = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let mut scope = crate::ingress_phase4::RequestScope::new(
        &state,
        request_id,
        virtual_model_hint,
        ingress_protocol.clone(),
        trace_ctx.clone(),
        started,
    );
    // Persist the redacted envelope on the terminal RequestEvent
    // for audit / replay (§8 #3 / #8).
    scope.set_envelope(raw_env.clone());
    // Bind the api key id so the terminal RequestEvent attributes the
    // row to the right caller (used by the per-key quota dashboard).
    let api_key = crate::ingress_phase4::resolve_api_key(&state, &headers).await;
    scope.set_api_key_id(api_key.key_id.clone());
    // Phase 4 §4.6: quota enforcement on the request hot path.
    // Parity with the chat-completions / anthropic-messages paths.
    match crate::ingress_phase4::check_quota(&state, &api_key.key_id, &api_key.spec, 1).await {
        crate::ingress_phase4::QuotaOutcome::Allow => {}
        crate::ingress_phase4::QuotaOutcome::Deny { retry_after, .. } => {
            let app_err =
                AppError::new(StatusCode::TOO_MANY_REQUESTS, "quota exceeded".to_string())
                    .with_retry_after(retry_after.as_secs().max(1));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("quota_exceeded", Some(http_status));
            return Err(app_err);
        }
    }

    let ir_request = match codec.decode_request(body, &raw_env) {
        Ok(r) => r,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_REQUEST, format!("Decode error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("decode_error", Some(http_status));
            return Err(app_err);
        }
    };

    let virtual_model = ir_request.model.clone();
    let is_stream = ir_request.stream;

    let targets = match state.current_config().routing_table.resolve(&virtual_model) {
        Some(t) => t,
        None => {
            let app_err = AppError::new(
                StatusCode::NOT_FOUND,
                format!("No route found for model: {virtual_model}"),
            );
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("route_not_found", Some(http_status));
            return Err(app_err);
        }
    };

    let target = match targets.first() {
        Some(t) => t,
        None => {
            let app_err =
                AppError::new(StatusCode::BAD_GATEWAY, "no targets configured".to_string());
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("no_targets", Some(http_status));
            return Err(app_err);
        }
    };

    scope.set_egress(target.api_protocol.clone());
    scope.set_resolved(target.provider_id.clone(), target.model_id.clone());

    let egress_protocol = target.api_protocol.clone();
    let is_same_protocol = ingress_protocol.suite == egress_protocol.suite;

    // Encode for the upstream. Same-protocol → use the ingress codec
    // directly; cross-protocol → convert IR into the egress protocol's
    // wire format (with the field-level lossy-conversion check), so a
    // /v1/responses request can be routed to an OpenAI / Anthropic /
    // Gemini upstream.
    let (mut upstream_body, mut upstream_headers) = if is_same_protocol {
        match codec.encode_request(&ir_request) {
            Ok(b) => b,
            Err(e) => {
                let app_err = AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Encode error: {e}"),
                );
                let http_status = app_err.http_status().as_u16();
                scope.emit_error("encode_error", Some(http_status));
                return Err(app_err);
            }
        }
    } else {
        match encode_cross_protocol(&codec, &egress_protocol, &ir_request) {
            Ok(b) => b,
            Err(app_err) => {
                let http_status = app_err.http_status().as_u16();
                scope.emit_error("encode_error", Some(http_status));
                return Err(app_err);
            }
        }
    };

    // Replace the (possibly virtual) model name with the routing
    // target's real upstream model id.
    override_model_in_body(&mut upstream_body, &target.model_id);
    merge_client_headers(&headers, &mut upstream_headers, &state.header_policy);
    if let Err(e) = apply_provider_auth(target, &mut upstream_headers).await {
        let http_status = e.http_status().as_u16();
        scope.emit_error("auth_error", Some(http_status));
        return Err(e);
    }

    // Capture egress request (headers + body) for the detail view.
    // The egress *headers* are snapshotted per-branch from the built
    // `reqwest::Request` via `finalize_egress` so they include every
    // header reqwest adds at finalize time.
    let egress_body_capture = serde_json::to_string(&upstream_body).ok();
    let req_id_capture = scope.request_id().to_string();

    // Address the upstream by the egress protocol suite. Same-protocol
    // Responses stays on `/responses`; cross-protocol routing targets the
    // egress protocol's endpoint (chat-completions, messages, or Gemini's
    // model-embedded URL).
    let upstream_url = if is_stream {
        upstream_stream_url_for_suite(target, egress_protocol.suite)
    } else {
        gemini_aware_upstream_url(target, egress_protocol.suite)
    };
    let upstream_url = match upstream_url {
        Some(u) => u,
        None => {
            let app_err = AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "No upstream path for egress protocol suite: {:?}",
                    egress_protocol.suite
                ),
            );
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("encode_error", Some(http_status));
            return Err(app_err);
        }
    };

    if is_stream {
        // Streaming path: tell the upstream we accept SSE and drive the
        // body through the same idle/total/keepalive bridge used by the
        // chat-completions and anthropic-messages paths.
        let stream_builder = crate::ingress_phase4::inject_trace(
            state
                .http_client
                .post(&upstream_url)
                .headers(upstream_headers)
                .header(http::header::ACCEPT, "text/event-stream"),
            &trace_ctx,
        )
        .json(&upstream_body);
        let (egress_req, egress_headers_capture, egress_method, egress_path) =
            match crate::ingress_phase4::finalize_egress(stream_builder) {
                Ok(r) => r,
                Err(app_err) => {
                    let http_status = app_err.http_status().as_u16();
                    scope.emit_error("upstream_send_error", Some(http_status));
                    return Err(app_err);
                }
            };
        let response = match state.http_client.execute(egress_req).await {
            Ok(r) => r,
            Err(e) => {
                let app_err =
                    AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}"));
                let http_status = app_err.http_status().as_u16();
                scope.emit_error("upstream_send_error", Some(http_status));
                return Err(app_err);
            }
        };
        let retry_after = extract_retry_after(response.headers());
        let status = response.status();
        let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
        let upstream_status_capture = status.as_u16();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            spawn_capture(
                &state,
                tiygate_core::ExchangeCapture {
                    request_id: req_id_capture.clone(),
                    egress_method: egress_method.clone(),
                    egress_path: egress_path.clone(),
                    egress_headers: egress_headers_capture.clone(),
                    egress_body: egress_body_capture.clone(),
                    upstream_status: Some(upstream_status_capture),
                    upstream_resp_headers: upstream_resp_headers_capture.clone(),
                    upstream_resp_body: Some(error_body.clone()),
                    client_resp_headers: Vec::new(),
                    client_resp_body: None,
                    is_stream: true,
                },
            );
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!("Upstream {}: {}", status, error_body),
            );
            app_err.upstream_status = Some(status.as_u16());
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("upstream_error", Some(http_status));
            return Err(app_err);
        }

        let accum = std::sync::Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let mut end_enc = codec.stream_encoder();
        let mut err_enc = codec.stream_encoder();
        let end_marker = end_enc.encode_done();
        let error_marker = err_enc.encode_error(
            "upstream stream truncated by gateway",
            Some("upstream_timeout"),
        );

        let mut response = drive_upstream_stream(
            &state,
            accum,
            response,
            end_marker,
            error_marker,
            Duration::from_secs(state.upstream_stream_idle_timeout_secs),
            Duration::from_secs(state.upstream_stream_total_timeout_secs),
            DEFAULT_SSE_KEEPALIVE_INTERVAL,
            Some(StreamCapture {
                request_id: req_id_capture.clone(),
                telemetry: state.telemetry.clone(),
                egress_method: egress_method.to_string(),
                egress_path: egress_path.to_string(),
                egress_headers: egress_headers_capture.clone(),
                egress_body: egress_body_capture.clone(),
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture.clone(),
                client_resp_headers: forwarded_resp_headers_for_capture(
                    &upstream_resp_headers_capture,
                    &state.header_policy,
                ),
                max_bytes: state.raw_envelope_max_bytes as usize,
            }),
            // Cross-protocol streaming: decode the egress SSE and re-encode
            // into the Responses client format. Same-protocol → None (verbatim).
            build_stream_transcode(&ingress_protocol, &egress_protocol),
        );
        // Forward upstream response headers to the client (denylist).
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_capture,
            &state.header_policy,
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
        scope.emit_ok(Some(response.status().as_u16()));
        return Ok(response);
    }

    // Non-streaming path: read the full body and forward as JSON.
    let nonstream_builder = crate::ingress_phase4::inject_trace(
        state
            .http_client
            .post(&upstream_url)
            .headers(upstream_headers),
        &trace_ctx,
    )
    .json(&upstream_body);
    let (egress_req, egress_headers_capture, egress_method, egress_path) =
        match crate::ingress_phase4::finalize_egress(nonstream_builder) {
            Ok(r) => r,
            Err(app_err) => {
                let http_status = app_err.http_status().as_u16();
                scope.emit_error("upstream_send_error", Some(http_status));
                return Err(app_err);
            }
        };
    let response = match state.http_client.execute(egress_req).await {
        Ok(r) => r,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("upstream_send_error", Some(http_status));
            return Err(app_err);
        }
    };
    let status = response.status();
    let retry_after = extract_retry_after(response.headers());
    let rate_limit_headers_vec: Vec<(&'static str, String)> =
        extract_rate_limit_headers(response.headers());
    let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
    let upstream_status_capture = status.as_u16();
    let response_body: Value = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_GATEWAY, format!("Parse error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("upstream_parse_error", Some(http_status));
            return Err(app_err);
        }
    };
    if !status.is_success() {
        spawn_capture(
            &state,
            tiygate_core::ExchangeCapture {
                request_id: req_id_capture.clone(),
                egress_method: egress_method.clone(),
                egress_path: egress_path.clone(),
                    egress_headers: egress_headers_capture.clone(),
                egress_body: egress_body_capture.clone(),
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture.clone(),
                upstream_resp_body: serde_json::to_string(&response_body).ok(),
                client_resp_headers: Vec::new(),
                client_resp_body: None,
                is_stream: false,
            },
        );
        let mut app_err = AppError::new(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            format!("Upstream {}: {}", status, response_body),
        );
        app_err.upstream_status = Some(status.as_u16());
        if let Some(ra) = retry_after {
            app_err = app_err.with_retry_after_header(ra);
        }
        app_err.rate_limit_headers = rate_limit_headers_vec;
        let http_status = app_err.http_status().as_u16();
        scope.emit_error("upstream_error", Some(http_status));
        return Err(app_err);
    }
    // Cross-protocol response re-encode: when the upstream spoke a
    // different protocol than the Responses client, decode the upstream
    // body via the egress codec and re-encode it into the Responses
    // format so the client sees what it expects. Same-protocol → verbatim.
    let response_body = if is_same_protocol {
        response_body
    } else {
        let egress_codec = match get_egress_codec(&egress_protocol) {
            Some(c) => c,
            None => {
                let app_err = AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("No egress codec found: {:?}", egress_protocol),
                );
                let http_status = app_err.http_status().as_u16();
                scope.emit_error("decode_error", Some(http_status));
                return Err(app_err);
            }
        };
        let ir_response = match egress_codec.decode_response(response_body) {
            Ok(ir) => ir,
            Err(e) => {
                let app_err = AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Decode response error: {e}"),
                );
                let http_status = app_err.http_status().as_u16();
                scope.emit_error("decode_error", Some(http_status));
                return Err(app_err);
            }
        };
        match codec.encode_response(&ir_response) {
            Ok(v) => v,
            Err(e) => {
                let app_err = AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Encode response error: {e}"),
                );
                let http_status = app_err.http_status().as_u16();
                scope.emit_error("encode_error", Some(http_status));
                return Err(app_err);
            }
        }
    };
    let body_str_capture = serde_json::to_string(&response_body).ok();
    let mut resp = Json(response_body).into_response();
    // Forward upstream response headers to the client (denylist).
    forward_upstream_resp_headers(&mut resp, &upstream_resp_headers_capture, &state.header_policy);
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
    spawn_capture(
        &state,
        tiygate_core::ExchangeCapture {
            request_id: req_id_capture,
            egress_method: egress_method.to_string(),
            egress_path: egress_path.to_string(),
            egress_headers: egress_headers_capture,
            egress_body: egress_body_capture,
            upstream_status: Some(upstream_status_capture),
            upstream_resp_headers: upstream_resp_headers_capture,
            upstream_resp_body: body_str_capture.clone(),
            client_resp_headers: header_map_to_vec(resp.headers()),
            client_resp_body: body_str_capture,
            is_stream: false,
        },
    );
    let http_status = resp.status().as_u16();
    scope.emit_ok(Some(http_status));
    Ok(resp)
}

/// Handle POST /v1beta/models/:model/generateContent — Google Gemini.
///
/// Mirrors `handle_chat_completions` but uses `GeminiCodec`. A
/// `RequestScope` drop guard ensures the terminal `RequestEvent` is
/// emitted on every return path (success, upstream error, decode /
/// route / encode failure).
async fn handle_gemini_generate(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(capture): axum::extract::Path<String>,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    // The router registers two path shapes for Gemini ingress:
    //   * colon shape  — `/v1beta/models/:capture`  (the `:capture`
    //     value is e.g. `foo:generateContent` per the Google
    //     official URL grammar)
    //   * slash shape  — `/v1beta/models/:model/generateContent`
    //     (the `:model` value is the bare id; the verb is consumed
    //     by the static suffix)
    //
    // `split_gemini_capture` normalises both shapes into
    // `(model_id, method)` and rejects malformed inputs.
    let (model, method) = match split_gemini_capture(&capture) {
        Some(pair) => pair,
        None => {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                format!("Invalid Gemini path capture: {capture:?}"),
            ));
        }
    };
    let _ = method; // We currently route all methods to one handler.
    let model = model.to_string();
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

    let trace_ctx = crate::ingress_phase4::extract_trace(&headers);
    let raw_env = crate::ingress_phase4::build_redacted_envelope(
        &state,
        "POST",
        &format!("/v1beta/models/{model}/generateContent"),
        &body,
        &headers,
    );

    let started = Instant::now();
    let request_id = uuid::Uuid::now_v7().to_string();
    let mut scope = crate::ingress_phase4::RequestScope::new(
        &state,
        request_id,
        model.clone(),
        ingress_protocol.clone(),
        trace_ctx.clone(),
        started,
    );
    // Persist the redacted envelope on the terminal RequestEvent
    // for audit / replay (§8 #3 / #8).
    scope.set_envelope(raw_env.clone());
    // Bind the api key id so the terminal RequestEvent attributes the
    // row to the right caller (used by the per-key quota dashboard).
    let api_key = crate::ingress_phase4::resolve_api_key(&state, &headers).await;
    scope.set_api_key_id(api_key.key_id.clone());
    // Phase 4 §4.6: quota enforcement on the request hot path.
    // Parity with the chat-completions / anthropic-messages paths.
    match crate::ingress_phase4::check_quota(&state, &api_key.key_id, &api_key.spec, 1).await {
        crate::ingress_phase4::QuotaOutcome::Allow => {}
        crate::ingress_phase4::QuotaOutcome::Deny { retry_after, .. } => {
            let app_err =
                AppError::new(StatusCode::TOO_MANY_REQUESTS, "quota exceeded".to_string())
                    .with_retry_after(retry_after.as_secs().max(1));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("quota_exceeded", Some(http_status));
            return Err(app_err);
        }
    }

    let ir_request = match codec.decode_request(body, &raw_env) {
        Ok(r) => r,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_REQUEST, format!("Decode error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("decode_error", Some(http_status));
            return Err(app_err);
        }
    };

    let virtual_model = model;
    let is_stream = ir_request.stream;

    let targets = match state.current_config().routing_table.resolve(&virtual_model) {
        Some(t) => t,
        None => {
            let app_err = AppError::new(
                StatusCode::NOT_FOUND,
                format!("No route found for model: {virtual_model}"),
            );
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("route_not_found", Some(http_status));
            return Err(app_err);
        }
    };

    let target = match targets.first() {
        Some(t) => t,
        None => {
            let app_err =
                AppError::new(StatusCode::BAD_GATEWAY, "no targets configured".to_string());
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("no_targets", Some(http_status));
            return Err(app_err);
        }
    };

    scope.set_egress(target.api_protocol.clone());
    scope.set_resolved(target.provider_id.clone(), target.model_id.clone());

    let egress_protocol = target.api_protocol.clone();
    let is_same_protocol = ingress_protocol.suite == egress_protocol.suite;

    // Encode for the upstream. Same-protocol Gemini → use the Gemini
    // codec directly; cross-protocol → convert IR into the egress
    // protocol's wire format (with the lossy-conversion check) so a
    // Gemini generateContent request can be routed to an OpenAI /
    // Anthropic / Responses upstream.
    let (mut upstream_body, mut upstream_headers) = if is_same_protocol {
        match codec.encode_request(&ir_request) {
            Ok(b) => b,
            Err(e) => {
                let app_err = AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Encode error: {e}"),
                );
                let http_status = app_err.http_status().as_u16();
                scope.emit_error("encode_error", Some(http_status));
                return Err(app_err);
            }
        }
    } else {
        match encode_cross_protocol(&codec, &egress_protocol, &ir_request) {
            Ok(b) => b,
            Err(app_err) => {
                let http_status = app_err.http_status().as_u16();
                scope.emit_error("encode_error", Some(http_status));
                return Err(app_err);
            }
        }
    };

    // Replace the (possibly virtual) model name with the routing
    // target's real upstream model id.
    override_model_in_body(&mut upstream_body, &target.model_id);
    merge_client_headers(&headers, &mut upstream_headers, &state.header_policy);
    if let Err(e) = apply_provider_auth(target, &mut upstream_headers).await {
        let http_status = e.http_status().as_u16();
        scope.emit_error("auth_error", Some(http_status));
        return Err(e);
    }

    // Capture egress request (headers + body) for the detail view.
    // The egress *headers* are snapshotted per-branch from the built
    // `reqwest::Request` via `finalize_egress` so they include every
    // header reqwest adds at finalize time.
    let egress_body_capture = serde_json::to_string(&upstream_body).ok();
    let req_id_capture = scope.request_id().to_string();

    // Resolve the streaming and non-streaming upstream URLs by the egress
    // protocol suite. Same-protocol Gemini uses `:streamGenerateContent`
    // / `:generateContent`; cross-protocol routing targets the egress
    // protocol's endpoint (chat-completions, messages, responses).
    let stream_upstream_url = match upstream_stream_url_for_suite(target, egress_protocol.suite) {
        Some(u) => u,
        None => {
            let app_err = AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "No upstream path for egress protocol suite: {:?}",
                    egress_protocol.suite
                ),
            );
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("encode_error", Some(http_status));
            return Err(app_err);
        }
    };
    let nonstream_upstream_url = match gemini_aware_upstream_url(target, egress_protocol.suite) {
        Some(u) => u,
        None => {
            let app_err = AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "No upstream path for egress protocol suite: {:?}",
                    egress_protocol.suite
                ),
            );
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("encode_error", Some(http_status));
            return Err(app_err);
        }
    };

    if is_stream {
        // Streaming path: the egress URL already carries `?alt=sse` for
        // Gemini; for cross-protocol egress it is the target protocol's
        // streaming endpoint. Run the body through `drive_upstream_stream`
        // so the client sees the same idle / total / keepalive /
        // protocol-native end-frame semantics as the other ingress paths.
        let stream_builder = crate::ingress_phase4::inject_trace(
            state
                .http_client
                .post(&stream_upstream_url)
                .headers(upstream_headers),
            &trace_ctx,
        )
        .json(&upstream_body);
        let (egress_req, egress_headers_capture, egress_method, egress_path) =
            match crate::ingress_phase4::finalize_egress(stream_builder) {
                Ok(r) => r,
                Err(app_err) => {
                    let http_status = app_err.http_status().as_u16();
                    scope.emit_error("upstream_send_error", Some(http_status));
                    return Err(app_err);
                }
            };
        let response = match state.http_client.execute(egress_req).await {
            Ok(r) => r,
            Err(e) => {
                let app_err =
                    AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}"));
                let http_status = app_err.http_status().as_u16();
                scope.emit_error("upstream_send_error", Some(http_status));
                return Err(app_err);
            }
        };
        let retry_after = extract_retry_after(response.headers());
        let status = response.status();
        let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
        let upstream_status_capture = status.as_u16();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            spawn_capture(
                &state,
                tiygate_core::ExchangeCapture {
                    request_id: req_id_capture.clone(),
                    egress_method: egress_method.clone(),
                    egress_path: egress_path.clone(),
                    egress_headers: egress_headers_capture.clone(),
                    egress_body: egress_body_capture.clone(),
                    upstream_status: Some(upstream_status_capture),
                    upstream_resp_headers: upstream_resp_headers_capture.clone(),
                    upstream_resp_body: Some(error_body.clone()),
                    client_resp_headers: Vec::new(),
                    client_resp_body: None,
                    is_stream: true,
                },
            );
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!("Upstream {}: {}", status, error_body),
            );
            app_err.upstream_status = Some(status.as_u16());
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("upstream_error", Some(http_status));
            return Err(app_err);
        }

        let accum = std::sync::Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let mut end_enc = codec.stream_encoder();
        let mut err_enc = codec.stream_encoder();
        let end_marker = end_enc.encode_done();
        let error_marker = err_enc.encode_error(
            "upstream stream truncated by gateway",
            Some("upstream_timeout"),
        );

        let mut response = drive_upstream_stream(
            &state,
            accum,
            response,
            end_marker,
            error_marker,
            Duration::from_secs(state.upstream_stream_idle_timeout_secs),
            Duration::from_secs(state.upstream_stream_total_timeout_secs),
            DEFAULT_SSE_KEEPALIVE_INTERVAL,
            Some(StreamCapture {
                request_id: req_id_capture.clone(),
                telemetry: state.telemetry.clone(),
                egress_method: egress_method.to_string(),
                egress_path: egress_path.to_string(),
                egress_headers: egress_headers_capture.clone(),
                egress_body: egress_body_capture.clone(),
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture.clone(),
                client_resp_headers: forwarded_resp_headers_for_capture(
                    &upstream_resp_headers_capture,
                    &state.header_policy,
                ),
                max_bytes: state.raw_envelope_max_bytes as usize,
            }),
            // Cross-protocol streaming: decode the egress SSE and re-encode
            // into the Gemini client format. Same-protocol → None (verbatim).
            build_stream_transcode(&ingress_protocol, &egress_protocol),
        );
        // Forward upstream response headers to the client (denylist).
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_capture,
            &state.header_policy,
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
        scope.emit_ok(Some(response.status().as_u16()));
        return Ok(response);
    }

    // Non-streaming path: read the full body and forward as JSON.
    let upstream_url = nonstream_upstream_url;
    let nonstream_builder = crate::ingress_phase4::inject_trace(
        state
            .http_client
            .post(&upstream_url)
            .headers(upstream_headers),
        &trace_ctx,
    )
    .json(&upstream_body);
    let (egress_req, egress_headers_capture, egress_method, egress_path) =
        match crate::ingress_phase4::finalize_egress(nonstream_builder) {
            Ok(r) => r,
            Err(app_err) => {
                let http_status = app_err.http_status().as_u16();
                scope.emit_error("upstream_send_error", Some(http_status));
                return Err(app_err);
            }
        };
    let response = match state.http_client.execute(egress_req).await {
        Ok(r) => r,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("upstream_send_error", Some(http_status));
            return Err(app_err);
        }
    };
    let status = response.status();
    let retry_after = extract_retry_after(response.headers());
    let rate_limit_headers_vec: Vec<(&'static str, String)> =
        extract_rate_limit_headers(response.headers());
    let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
    let upstream_status_capture = status.as_u16();
    let response_body: Value = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_GATEWAY, format!("Parse error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error("upstream_parse_error", Some(http_status));
            return Err(app_err);
        }
    };
    if !status.is_success() {
        spawn_capture(
            &state,
            tiygate_core::ExchangeCapture {
                request_id: req_id_capture.clone(),
                egress_method: egress_method.clone(),
                egress_path: egress_path.clone(),
                    egress_headers: egress_headers_capture.clone(),
                egress_body: egress_body_capture.clone(),
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture.clone(),
                upstream_resp_body: serde_json::to_string(&response_body).ok(),
                client_resp_headers: Vec::new(),
                client_resp_body: None,
                is_stream: false,
            },
        );
        let mut app_err = AppError::new(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            format!("Upstream {}: {}", status, response_body),
        );
        app_err.upstream_status = Some(status.as_u16());
        if let Some(ra) = retry_after {
            app_err = app_err.with_retry_after_header(ra);
        }
        app_err.rate_limit_headers = rate_limit_headers_vec;
        let http_status = app_err.http_status().as_u16();
        scope.emit_error("upstream_error", Some(http_status));
        return Err(app_err);
    }
    // Cross-protocol response re-encode: when the upstream spoke a
    // different protocol than the Gemini client, decode the upstream body
    // via the egress codec and re-encode it into the Gemini format so the
    // client sees what it expects. Same-protocol → verbatim.
    let response_body = if is_same_protocol {
        response_body
    } else {
        let egress_codec = match get_egress_codec(&egress_protocol) {
            Some(c) => c,
            None => {
                let app_err = AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("No egress codec found: {:?}", egress_protocol),
                );
                let http_status = app_err.http_status().as_u16();
                scope.emit_error("decode_error", Some(http_status));
                return Err(app_err);
            }
        };
        let ir_response = match egress_codec.decode_response(response_body) {
            Ok(ir) => ir,
            Err(e) => {
                let app_err = AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Decode response error: {e}"),
                );
                let http_status = app_err.http_status().as_u16();
                scope.emit_error("decode_error", Some(http_status));
                return Err(app_err);
            }
        };
        match codec.encode_response(&ir_response) {
            Ok(v) => v,
            Err(e) => {
                let app_err = AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Encode response error: {e}"),
                );
                let http_status = app_err.http_status().as_u16();
                scope.emit_error("encode_error", Some(http_status));
                return Err(app_err);
            }
        }
    };
    let body_str_capture = serde_json::to_string(&response_body).ok();
    let mut resp = Json(response_body).into_response();
    // Forward upstream response headers to the client (denylist).
    forward_upstream_resp_headers(&mut resp, &upstream_resp_headers_capture, &state.header_policy);
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
    spawn_capture(
        &state,
        tiygate_core::ExchangeCapture {
            request_id: req_id_capture,
            egress_method: egress_method.to_string(),
            egress_path: egress_path.to_string(),
            egress_headers: egress_headers_capture,
            egress_body: egress_body_capture,
            upstream_status: Some(upstream_status_capture),
            upstream_resp_headers: upstream_resp_headers_capture,
            upstream_resp_body: body_str_capture.clone(),
            client_resp_headers: header_map_to_vec(resp.headers()),
            client_resp_body: body_str_capture,
            is_stream: false,
        },
    );
    let http_status = resp.status().as_u16();
    scope.emit_ok(Some(http_status));
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

impl<S: Stream<Item = Result<Bytes, axum::Error>>> SseKeepaliveStream<S> {
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

impl<S: Stream<Item = Result<Bytes, axum::Error>>> Stream
    for SseKeepaliveStream<S>
{
    type Item = Result<Bytes, axum::Error>;

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
                *this.emit_keepalive_at = Instant::now()
                    + if this.interval.is_zero() {
                        Duration::from_secs(0)
                    } else {
                        *this.interval
                    };
                this.timer
                    .as_mut()
                    .reset(tokio::time::Instant::now() + *this.interval);
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
                this.timer
                    .as_mut()
                    .reset(tokio::time::Instant::now() + *this.interval);
                let keepalive = Bytes::from_static(b":keepalive\n\n");
                return std::task::Poll::Ready(Some(Ok(keepalive)));
            }
            // Re-register the timer waker so the task wakes up when
            // the keepalive deadline is reached.
            let _ = this.timer.as_mut().poll(cx);
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
/// Context for capturing a streaming (SSE) exchange into the
/// request-log detail view. The egress request (headers + body) and
/// the upstream response headers/status are already known when the
/// stream starts; the response body is accumulated chunk-by-chunk as
/// the stream is forwarded and the `ExchangeCapture` is sent to the
/// telemetry bus once the stream terminates.
pub struct StreamCapture {
    pub request_id: String,
    pub telemetry: Arc<dyn tiygate_core::TelemetryBus>,
    /// HTTP method used for the gateway → provider request. Captured
    /// from `finalize_egress` so the request-log detail view can
    /// render the "POST /v1/chat/..." status line.
    pub egress_method: String,
    /// URL path used for the gateway → provider request.
    pub egress_path: String,
    pub egress_headers: Vec<(String, String)>,
    pub egress_body: Option<String>,
    pub upstream_status: Option<u16>,
    pub upstream_resp_headers: Vec<(String, String)>,
    /// Headers actually forwarded to the client on the SSE response
    /// (denylist-filtered upstream headers + content-type), recorded as
    /// the `client_resp_headers` in the detail view.
    pub client_resp_headers: Vec<(String, String)>,
    /// Byte cap for the accumulated response body; once exceeded the
    /// buffer stops growing (best-effort; truncation is flagged by the
    /// sink when the body hits the persistence cap).
    pub max_bytes: usize,
}

/// Cross-protocol streaming re-encode plan for [`drive_upstream_stream`].
///
/// When the ingress entrypoint protocol differs from the egress (upstream
/// provider) protocol, the upstream SSE bytes cannot be forwarded verbatim —
/// the client expects its own protocol's wire format. This carries the IR
/// hub-spoke pair: the egress protocol's [`StreamDecoder`] (parses the
/// upstream SSE into canonical [`tiygate_core::StreamPart`]s) and the ingress
/// protocol's [`StreamEncoder`] (re-encodes those parts into the client's
/// native SSE frames). When `None`, the stream is forwarded verbatim (the
/// same-protocol fast path with zero information loss).
///
/// [`StreamDecoder`]: tiygate_core::StreamDecoder
/// [`StreamEncoder`]: tiygate_core::StreamEncoder
pub struct StreamTranscode {
    /// Egress (upstream) protocol decoder: upstream SSE → IR stream parts.
    pub decoder: Box<dyn tiygate_core::StreamDecoder>,
    /// Ingress (client) protocol encoder: IR stream parts → client SSE.
    pub encoder: Box<dyn tiygate_core::StreamEncoder>,
}

/// Split a UTF-8 SSE buffer into complete lines, returning the parsed lines
/// and any trailing partial line (no terminating `\n` yet) that must be
/// carried over to the next chunk. SSE events are delimited by blank lines
/// and each protocol decoder parses a single `data:` line at a time while
/// ignoring `event:` / blank lines, so line-granular feeding is sufficient
/// and robust to TCP packet boundaries that split a frame mid-line.
fn split_sse_lines(buf: &str) -> (Vec<String>, String) {
    let mut lines: Vec<String> = Vec::new();
    let mut remainder = String::new();
    let mut last_end = 0usize;
    for (idx, ch) in buf.char_indices() {
        if ch == '\n' {
            lines.push(buf[last_end..idx].to_string());
            last_end = idx + 1;
        }
    }
    if last_end < buf.len() {
        remainder.push_str(&buf[last_end..]);
    }
    (lines, remainder)
}

#[allow(
    clippy::too_many_arguments,
    clippy::let_underscore_must_use,
    // `last_reason` is captured by the async-stream macro but
    // the captured value is only used via the trailing
    // `let _ = last_reason;` touch at the end of the stream
    // block; rustc's NLL does not see through the macro
    // expansion. The variable is documented intent — the
    // truncation reason is held in scope for future
    // `TelemetryBus` reports.
    unused_assignments
)]
pub fn drive_upstream_stream(
    _state: &AppState,
    accum: Arc<std::sync::Mutex<UsageAccumulator>>,
    response: reqwest::Response,
    end_marker: Vec<u8>,
    error_marker: Vec<u8>,
    idle_timeout: Duration,
    total_timeout: Duration,
    keepalive_interval: Duration,
    capture: Option<StreamCapture>,
    transcode: Option<StreamTranscode>,
) -> Response {
    use async_stream::stream;

    let total_budget_enabled = !total_timeout.is_zero();
    let total_started = Instant::now();
    let mut upstream = response.bytes_stream();
    let mut last_reason: Option<TruncationReason> = None;
    // Streaming response-body accumulators for the request-log detail
    // view. Bounded by `capture.max_bytes`; once the cap is hit we
    // stop appending (the sink flags truncation on read-back).
    //
    // `capture_buf` always records the raw upstream SSE bytes, so
    // `upstream_resp_body` in the persisted row reflects what came
    // from the provider. `client_capture_buf` records the bytes that
    // were actually yielded to the downstream client:
    //  * In verbatim / same-protocol mode it is appended with the same
    //    upstream bytes, so `client_resp_body == upstream_resp_body`.
    //  * In transcode / cross-protocol mode it is appended with the
    //    ingress encoder's output (Anthropic SSE → OpenAI chunks, etc.),
    //    so the request-log detail view shows what the client really
    //    received, not a byte-identical copy of the upstream stream.
    let mut capture_buf: Vec<u8> = Vec::new();
    let mut client_capture_buf: Vec<u8> = Vec::new();
    let capture_max_bytes = capture.as_ref().map(|c| c.max_bytes).unwrap_or(0);
    // Rolling tail of the most recently forwarded upstream bytes. Used
    // on natural close to detect whether the upstream already emitted
    // its own protocol-native terminal frame (e.g. `data: [DONE]` or
    // `event: message_stop`), so the gateway does not append a
    // *duplicate* end frame. Capped to a small window large enough to
    // hold the biggest terminal frame.
    let mut tail_buf: Vec<u8> = Vec::new();
    const TAIL_CAP: usize = 512;
    // Cross-protocol transcode state. When `Some`, upstream SSE bytes are
    // decoded into IR stream parts (egress decoder) and re-encoded into the
    // client's protocol (ingress encoder) instead of being forwarded
    // verbatim. `frame_buf` carries any partial trailing line across chunk
    // boundaries so a frame split by a TCP packet boundary is parsed once
    // complete.
    let mut transcode = transcode;
    let mut frame_buf = String::new();
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
    #[allow(clippy::let_underscore_must_use)]
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
                            // Accumulate the raw SSE bytes for the detail
                            // view, bounded by the byte cap. This is a
                            // single memory copy and never blocks the
                            // forward path.
                            if capture_max_bytes > 0 && capture_buf.len() < capture_max_bytes {
                                let remaining = capture_max_bytes - capture_buf.len();
                                let take = remaining.min(bytes.len());
                                capture_buf.extend_from_slice(&bytes[..take]);
                            }
                            // Maintain a small rolling tail for terminal-
                            // frame dedup on natural close. Only needed for the
                            // verbatim path; transcode dedups on its own
                            // re-encoded output instead.
                            if transcode.is_none() {
                                tail_buf.extend_from_slice(&bytes);
                                if tail_buf.len() > TAIL_CAP {
                                    let cut = tail_buf.len() - TAIL_CAP;
                                    tail_buf.drain(..cut);
                                }
                            }
                            if let Some(tc) = transcode.as_mut() {
                                // Cross-protocol mode: decode upstream SSE
                                // into IR stream parts and re-encode into the
                                // client's protocol. Append to the line buffer
                                // and feed each *complete* line to the egress
                                // decoder; the trailing partial line (if any)
                                // is held over to the next chunk.
                                frame_buf.push_str(&String::from_utf8_lossy(&bytes));
                                let (lines, remainder) = split_sse_lines(&frame_buf);
                                frame_buf = remainder;
                                let mut out: Vec<u8> = Vec::new();
                                for line in lines {
                                    match tc.decoder.feed(&line) {
                                        Ok(parts) => {
                                            for part in &parts {
                                                match tc.encoder.encode_part(part) {
                                                    Ok(b) => out.extend_from_slice(&b),
                                                    Err(e) => {
                                                        let ef = tc.encoder.encode_error(
                                                            &format!("transcode encode error: {e}"),
                                                            Some("transcode_error"),
                                                        );
                                                        out.extend_from_slice(&ef);
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            let ef = tc.encoder.encode_error(
                                                &format!("transcode decode error: {e}"),
                                                Some("transcode_error"),
                                            );
                                            out.extend_from_slice(&ef);
                                        }
                                    }
                                }
                                if !out.is_empty() {
                                    // Mirror the re-encoded bytes into the
                                    // client capture so cross-protocol streams
                                    // persist the ingress-format body in
                                    // `client_resp_body`, not the raw
                                    // upstream SSE.
                                    if capture_max_bytes > 0
                                        && client_capture_buf.len() < capture_max_bytes
                                    {
                                        let remaining = capture_max_bytes - client_capture_buf.len();
                                        let take = remaining.min(out.len());
                                        client_capture_buf.extend_from_slice(&out[..take]);
                                    }
                                    yield Ok(Bytes::from(out));
                                }
                            } else {
                                // Forward upstream bytes VERBATIM. The upstream
                                // chunk is already a complete SSE frame
                                // (`data: ...\n\n`); wrapping it in an axum
                                // `Event` would double-prefix `data:` and
                                // corrupt the stream. Pass the raw bytes.
                                if capture_max_bytes > 0
                                    && client_capture_buf.len() < capture_max_bytes
                                {
                                    let remaining = capture_max_bytes - client_capture_buf.len();
                                    let take = remaining.min(bytes.len());
                                    client_capture_buf.extend_from_slice(&bytes[..take]);
                                }
                                yield Ok(bytes);
                            }
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
                            // then close. In transcode mode the frame is
                            // generated by the *ingress* encoder so the
                            // client sees its own protocol's error shape.
                            if let Some(tc) = transcode.as_mut() {
                                let ef = tc.encoder.encode_error(
                                    "upstream stream truncated by gateway",
                                    Some("upstream_error"),
                                );
                                if !ef.is_empty() {
                                    if capture_max_bytes > 0
                                        && client_capture_buf.len() < capture_max_bytes
                                    {
                                        let remaining =
                                            capture_max_bytes - client_capture_buf.len();
                                        let take = remaining.min(ef.len());
                                        client_capture_buf.extend_from_slice(&ef[..take]);
                                    }
                                    yield Ok(Bytes::from(ef));
                                }
                            } else if !error_marker.is_empty() {
                                if capture_max_bytes > 0
                                    && client_capture_buf.len() < capture_max_bytes
                                {
                                    let remaining =
                                        capture_max_bytes - client_capture_buf.len();
                                    let take = remaining.min(error_marker.len());
                                    client_capture_buf
                                        .extend_from_slice(&error_marker[..take]);
                                }
                                yield Ok(Bytes::from(error_marker.clone()));
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
                            if let Some(tc) = transcode.as_mut() {
                                // Transcode mode: flush any buffered partial
                                // line and drain decoder.finish(). The
                                // ingress encoder emits its protocol-native
                                // done frame (e.g. `data: [DONE]\n\n` for
                                // ChatCompletions, `event: message_stop` for
                                // Anthropic) from the feed path when the
                                // upstream sends its terminal event, so we
                                // must NOT call `tc.encoder.encode_done()`
                                // here — that would append a *second*
                                // terminator (the dedup check uses a
                                // fresh local `out` that has no view of
                                // what was already yielded in previous
                                // chunks, so it would never fire).
                                let mut out: Vec<u8> = Vec::new();
                                if !frame_buf.trim().is_empty() {
                                    if let Ok(parts) = tc.decoder.feed(&frame_buf) {
                                        for part in &parts {
                                            if let Ok(b) = tc.encoder.encode_part(part) {
                                                out.extend_from_slice(&b);
                                            }
                                        }
                                    }
                                }
                                frame_buf.clear();
                                if let Ok(parts) = tc.decoder.finish() {
                                    for part in &parts {
                                        if let Ok(b) = tc.encoder.encode_part(part) {
                                            out.extend_from_slice(&b);
                                        }
                                    }
                                }
                                if !out.is_empty() {
                                    if capture_max_bytes > 0
                                        && client_capture_buf.len() < capture_max_bytes
                                    {
                                        let remaining =
                                            capture_max_bytes - client_capture_buf.len();
                                        let take = remaining.min(out.len());
                                        client_capture_buf.extend_from_slice(&out[..take]);
                                    }
                                    yield Ok(Bytes::from(out));
                                }
                            } else if !end_marker.is_empty()
                                && !tail_ends_with_marker(&tail_buf, &end_marker)
                            {
                                // Only append our own end frame if the upstream
                                // did NOT already send an identical terminal
                                // frame (avoids the duplicate `[DONE]` the old
                                // code produced). Bytes forwarded verbatim.
                                if capture_max_bytes > 0
                                    && client_capture_buf.len() < capture_max_bytes
                                {
                                    let remaining =
                                        capture_max_bytes - client_capture_buf.len();
                                    let take = remaining.min(end_marker.len());
                                    client_capture_buf.extend_from_slice(&end_marker[..take]);
                                }
                                yield Ok(Bytes::from(end_marker.clone()));
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
                    // still billable. Dedup against an upstream
                    // terminal frame just like the natural-close path.
                    if let Some(tc) = transcode.as_mut() {
                        let done = tc.encoder.encode_done();
                        if !done.is_empty() {
                            if capture_max_bytes > 0
                                && client_capture_buf.len() < capture_max_bytes
                            {
                                let remaining = capture_max_bytes - client_capture_buf.len();
                                let take = remaining.min(done.len());
                                client_capture_buf.extend_from_slice(&done[..take]);
                            }
                            yield Ok(Bytes::from(done));
                        }
                    } else if !end_marker.is_empty()
                        && !tail_ends_with_marker(&tail_buf, &end_marker)
                    {
                        if capture_max_bytes > 0
                            && client_capture_buf.len() < capture_max_bytes
                        {
                            let remaining = capture_max_bytes - client_capture_buf.len();
                            let take = remaining.min(end_marker.len());
                            client_capture_buf.extend_from_slice(&end_marker[..take]);
                        }
                        yield Ok(Bytes::from(end_marker.clone()));
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
                    // gateway-side cap, not a natural end. In transcode
                    // mode the frame is built by the ingress encoder.
                    if let Some(tc) = transcode.as_mut() {
                        let ef = tc.encoder.encode_error(
                            "upstream stream exceeded gateway total budget",
                            Some("upstream_timeout"),
                        );
                        if !ef.is_empty() {
                            if capture_max_bytes > 0
                                && client_capture_buf.len() < capture_max_bytes
                            {
                                let remaining = capture_max_bytes - client_capture_buf.len();
                                let take = remaining.min(ef.len());
                                client_capture_buf.extend_from_slice(&ef[..take]);
                            }
                            yield Ok(Bytes::from(ef));
                        }
                    } else if !error_marker.is_empty() {
                        if capture_max_bytes > 0
                            && client_capture_buf.len() < capture_max_bytes
                        {
                            let remaining = capture_max_bytes - client_capture_buf.len();
                            let take = remaining.min(error_marker.len());
                            client_capture_buf.extend_from_slice(&error_marker[..take]);
                        }
                        yield Ok(Bytes::from(error_marker.clone()));
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

        // Stream finished (natural end, idle, total, or upstream
        // error). Send the accumulated exchange capture to the
        // telemetry bus for the request-log detail view.
        //  * `upstream_resp_body` always records the raw upstream
        //    SSE bytes captured at chunk arrival time.
        //  * `client_resp_body` records the bytes that were actually
        //    yielded to the downstream client. In verbatim /
        //    same-protocol mode this is byte-identical to the
        //    upstream body; in cross-protocol / transcode mode it is
        //    the ingress-format SSE produced by the encoder (e.g.
        //    OpenAI `chat.completion.chunk` data lines decoded from
        //    Anthropic `content_block_delta` events).
        if let Some(cap) = capture {
            let upstream_body = if capture_buf.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&capture_buf).into_owned())
            };
            let client_body = if client_capture_buf.is_empty() {
                None
            } else {
                Some(String::from_utf8_lossy(&client_capture_buf).into_owned())
            };
            cap.telemetry
                .send_capture(tiygate_core::ExchangeCapture {
                    request_id: cap.request_id,
                    egress_method: cap.egress_method,
                    egress_path: cap.egress_path,
                    egress_headers: cap.egress_headers,
                    egress_body: cap.egress_body,
                    upstream_status: cap.upstream_status,
                    upstream_resp_headers: cap.upstream_resp_headers,
                    upstream_resp_body: upstream_body,
                    client_resp_headers: cap.client_resp_headers,
                    client_resp_body: client_body,
                    is_stream: true,
                })
                .await;
        }
    };

    // Wrap the inner stream in a keepalive emitter so the downstream
    // client (and any middlebox) keeps seeing activity even when the
    // upstream is between chunks.
    let kept = SseKeepaliveStream::new(Box::pin(idle_future), keepalive_interval);
    // Build a raw byte-stream body. We deliberately do NOT use axum's
    // `Sse` responder here: the upstream already delivers fully-formed
    // SSE frames, and `Sse`/`Event` would re-encode (double `data:`
    // prefix) the bytes. We forward the bytes verbatim and set the SSE
    // headers ourselves.
    let mut response = Body::from_stream(kept).into_response();
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-cache"),
    );
    response
}

/// Returns true if `tail` (the rolling window of the most recently
/// forwarded upstream bytes) already ends with the gateway's terminal
/// `marker`, ignoring trailing ASCII whitespace on both sides. Used to
/// suppress a duplicate end frame when the upstream already sent its
/// own protocol-native terminator (`data: [DONE]`, `message_stop`).
fn tail_ends_with_marker(tail: &[u8], marker: &[u8]) -> bool {
    let trim_end = |b: &[u8]| -> usize {
        let mut n = b.len();
        while n > 0 && b[n - 1].is_ascii_whitespace() {
            n -= 1;
        }
        n
    };
    let t = &tail[..trim_end(tail)];
    let m = &marker[..trim_end(marker)];
    if m.is_empty() {
        return false;
    }
    t.ends_with(m)
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
    pub(crate) fn new(status: StatusCode, message: String) -> Self {
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

    /// Public accessor for the HTTP status code. Used by the Phase
    /// 4 telemetry helpers to record the terminal `RequestEvent`'s
    /// `http_status` column on the failure path.
    pub fn http_status(&self) -> StatusCode {
        self.status
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
