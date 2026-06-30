//! HTTP ingress — request handling, routing, and SSE response streaming.
//!
//! Stability features:
//! - Multi-target fallback via FallbackPolicy + HealthRegistry
//! - Retry with exponential backoff + jitter
//! - Global concurrency semaphore + bounded queue
//! - Retry-After passthrough and upstream-aware cooling
//! - Error source distinction (gateway vs upstream)
//! - UsageAccumulator for disconnected streaming billing

mod executors;
mod fallback;
mod handlers;
mod headers;
mod observability;
mod streaming;

use handlers::{
    handle_chat_completions, handle_embeddings, handle_gemini_generate, handle_healthz,
    handle_images_edits, handle_images_generations, handle_messages, handle_readyz,
    handle_responses,
};

use std::sync::Arc;
use std::time::Duration;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use tokio::sync::Semaphore;
use tower_http::timeout::RequestBodyTimeoutLayer;

use tiygate_core::{HealthRegistry, TelemetryBus};

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

/// Runtime-tunable parameters that can be hot-reloaded from the
/// `settings` table without restarting the gateway. Wrapped in an
/// `ArcSwap` so the epoch-poll task can publish a new snapshot
/// atomically while the data plane reads the current value
/// lock-free on every request.
///
/// Fields that are expensive or unsafe to swap at runtime —
/// `reqwest::Client` (connection pool churn) and the concurrency
/// `Semaphore` (in-flight permit accounting) — are intentionally
/// excluded and remain fixed for the lifetime of the process.
#[derive(Clone)]
pub struct RuntimeTunables {
    /// Routing strategy selector (default `Weighted`, per §3.4).
    pub routing_strategy: crate::config::RoutingStrategyName,
    /// Whether to capture inline base64 media in raw envelopes.
    pub raw_envelope_capture_media: bool,
    /// Whether to require a valid API key on every data-plane
    /// request. When `true`, requests without a credential, with an
    /// unknown credential, or with a disabled credential are rejected
    /// with 401/403 before reaching the upstream. Hot-reloadable via
    /// `gateway.ingress.require_api_key`.
    pub require_api_key: bool,
    /// Bidirectional header forwarding policy.
    pub header_policy: Arc<tiygate_core::HeaderForwardPolicy>,
    /// Standard request body limit (bytes).
    pub max_request_body_bytes: u64,
    /// Max inflight requests before queueing.
    pub max_inflight: usize,
    /// Max queue depth before 503.
    pub max_queue_depth: usize,
    /// Timeout waiting for a concurrency permit.
    pub acquire_timeout: Duration,
    /// Idle timeout (seconds) for upstream streaming responses.
    pub upstream_stream_idle_timeout_secs: u64,
    /// Total wall-clock timeout (seconds) for upstream streaming.
    pub upstream_stream_total_timeout_secs: u64,
    /// Shared reqwest connection pool. Rebuilt when TCP keepalive,
    /// pool idle timeout, or tcp_nodelay settings change. The
    /// rebuild is debounced by the tunables reloader so frequent
    /// settings writes don't churn the connection pool.
    pub http_client: reqwest::Client,
}

/// Shared application state.
#[derive(Clone)]
#[allow(dead_code)]
pub struct AppState {
    pub config: Arc<ConfigStore>,
    /// Optional handle to the DB-backed config store. When `Some`,
    /// the data plane can perform per-caller `api_keys` lookups
    /// (used by `resolve_api_key` in `observability`). When `None`
    /// (legacy in-memory path, no control plane) the api key
    /// resolution is a no-op and all requests are treated as
    /// anonymous. Production code wires this in via
    /// `router_with_telemetry` from `app.rs`.
    pub db_store: Option<Arc<tiygate_store::config_store::DbConfigStore>>,
    pub health: Arc<HealthRegistry>,
    pub concurrency_semaphore: Arc<Semaphore>,
    /// Larger request body limit for multimodal content. Fixed at
    /// startup because the `RequestBodyLimitLayer` is installed
    /// at router build time.
    pub max_multimodal_body_bytes: u64,
    /// Read timeout for the full request body. Fixed at startup
    /// because the `RequestBodyTimeoutLayer` is installed at
    /// router build time.
    pub request_read_timeout: Duration,
    /// Async telemetry bus — non-blocking send.
    pub telemetry: Arc<dyn TelemetryBus>,
    /// Quota counter; `None` in the legacy in-memory path. The
    /// ingress hot path consults this *before* forwarding upstream
    /// and returns `429 + Retry-After` on deny.
    pub quota: Option<Arc<dyn tiygate_core::quota::QuotaCounter>>,
    /// Embedding cache; `None` when the `cache` feature is off.
    /// Only `/v1/embeddings` consults this; chat handlers ignore
    /// it (per §4.7).
    pub embedding_cache: Option<Arc<tiygate_cache::embedding_cache::EmbeddingCache>>,
    /// Per-request `Redactor` instance. Configurable so future
    /// env-var-driven extensions remain test-friendly.
    pub redactor: Arc<tiygate_core::redaction::Redactor>,
    /// Hot-reloadable runtime tunables. The epoch-poll task
    /// publishes a new `Arc<RuntimeTunables>` here after
    /// `store.refresh()` when settings change; the data plane
    /// reads the current value via `state.tunables()`.
    tunables: Arc<arc_swap::ArcSwap<RuntimeTunables>>,
    /// OAuth token manager. Handles OAuth token refresh and
    /// injection for providers configured with `AuthMode::OAuth`.
    pub oauth_manager: crate::oauth_manager::OAuthTokenManager,
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

    /// Load the current runtime tunables snapshot. This is a
    /// lock-free atomic pointer load — safe to call on every
    /// request.
    pub fn tunables(&self) -> arc_swap::Guard<Arc<RuntimeTunables>> {
        self.tunables.load()
    }

    /// Publish a new set of runtime tunables. Called by the
    /// epoch-poll task after it detects a settings change. The
    /// swap is atomic: in-flight requests continue reading the old
    /// snapshot until they call `tunables()` again.
    pub fn reload_tunables(&self, new: RuntimeTunables) {
        self.tunables.store(Arc::new(new));
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

/// Build a `reqwest::Client` from the upstream tuning parameters in
/// `ServerConfig`. Used at startup and by the tunables reloader when
/// TCP keepalive, pool idle timeout, or tcp_nodelay settings change.
fn build_http_client(server_config: &ServerConfig) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(32)
        .tcp_nodelay(server_config.upstream_tcp_nodelay);
    if server_config.upstream_tcp_keepalive_secs > 0 {
        builder = builder.tcp_keepalive(Duration::from_secs(
            server_config.upstream_tcp_keepalive_secs,
        ));
    }
    if server_config.upstream_pool_idle_timeout_secs > 0 {
        builder = builder.pool_idle_timeout(Duration::from_secs(
            server_config.upstream_pool_idle_timeout_secs,
        ));
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

/// Build a `reqwest::Client` from raw upstream parameter values.
/// Used by the tunables reloader which reads these from the settings
/// table rather than from `ServerConfig`.
fn build_http_client_from_params(
    tcp_nodelay: bool,
    tcp_keepalive_secs: u64,
    pool_idle_timeout_secs: u64,
) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(32)
        .tcp_nodelay(tcp_nodelay);
    if tcp_keepalive_secs > 0 {
        builder = builder.tcp_keepalive(Duration::from_secs(tcp_keepalive_secs));
    }
    if pool_idle_timeout_secs > 0 {
        builder = builder.pool_idle_timeout(Duration::from_secs(pool_idle_timeout_secs));
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
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
    // Clone the db_store before it is moved into AppState so we can
    // spawn the tunables reloader after state construction.
    let db_store_for_reloader = db_store.clone();
    let tunables = RuntimeTunables {
        routing_strategy: server_config.routing_strategy,
        raw_envelope_capture_media: server_config.raw_envelope_capture_media,
        require_api_key: server_config.require_api_key,
        header_policy: Arc::new(
            tiygate_core::HeaderForwardPolicy::with_defaults()
                .with_request_deny_extra(server_config.forward_request_header_deny_extra.iter())
                .with_response_deny_extra(server_config.forward_response_header_deny_extra.iter()),
        ),
        max_request_body_bytes: server_config.max_request_body_bytes,
        max_inflight: server_config.max_inflight_requests,
        max_queue_depth: server_config.max_queue_depth,
        acquire_timeout: Duration::from_secs(server_config.acquire_timeout_secs),
        upstream_stream_idle_timeout_secs: server_config.upstream_stream_idle_timeout_secs,
        upstream_stream_total_timeout_secs: server_config.upstream_stream_total_timeout_secs,
        http_client: build_http_client(server_config),
    };
    let state = AppState {
        config: Arc::new(config),
        db_store: db_store.clone(),
        health,
        concurrency_semaphore: semaphore,
        max_multimodal_body_bytes: server_config.max_multimodal_body_bytes,
        request_read_timeout: Duration::from_secs(server_config.request_read_timeout_secs),
        telemetry,
        quota,
        embedding_cache,
        redactor: Arc::new(tiygate_core::redaction::Redactor::with_defaults()),
        tunables: Arc::new(arc_swap::ArcSwap::from_pointee(tunables)),
        oauth_manager: crate::oauth_manager::OAuthTokenManager::new(
            db_store,
            build_http_client(server_config),
        ),
    };

    Router::new()
        .route("/v1/chat/completions", post(handle_chat_completions))
        .route("/v1/messages", post(handle_messages))
        .route("/v1/embeddings", post(handle_embeddings))
        .route("/v1/responses", post(handle_responses))
        .route("/v1/images/generations", post(handle_images_generations))
        .route("/v1/images/edits", post(handle_images_edits))
        // OpenAI-compatible model discovery (see
        // docs/models-endpoint-protocol.md). Baseline: lists the
        // virtual models in the live routing table.
        .route(
            "/v1/models",
            axum::routing::get(crate::models::handle_list_models),
        )
        .route(
            "/v1/models/:model_id",
            axum::routing::get(crate::models::handle_get_model),
        )
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
        .route("/v1beta/models/:capture", post(handle_gemini_generate))
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
        .layer(axum::extract::DefaultBodyLimit::disable())
        .layer(RequestBodyTimeoutLayer::new(Duration::from_secs(
            server_config.request_read_timeout_secs,
        )))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            server_config.max_multimodal_body_bytes as usize,
        ))
        .with_state({
            // Spawn the tunables reloader when a DB store is
            // available so runtime settings changes propagate to
            // the data plane without a restart.
            if let Some(ref db) = db_store_for_reloader {
                spawn_tunables_reloader(db.clone(), state.clone());
            }
            state
        })
}

/// Spawn a background task that watches the config epoch and
/// reloads [`RuntimeTunables`] from the `settings` table into the
/// [`AppState`] whenever the epoch advances. This runs alongside
/// the store-level epoch poll task (which refreshes the routing
/// snapshot); this task focuses exclusively on the ingress
/// tunables that live in the server crate.
pub(crate) fn spawn_tunables_reloader(
    store: Arc<tiygate_store::config_store::DbConfigStore>,
    state: AppState,
) -> tokio::task::JoinHandle<()> {
    use tiygate_store::settings_keys as sk;
    tokio::spawn(async move {
        let mut last_seen: Option<i64> = None;
        loop {
            let interval_secs = sk::get_u64(store.as_ref(), sk::EPOCH_POLL_INTERVAL_SECS, 2).await;
            tokio::time::sleep(Duration::from_secs(interval_secs.max(1))).await;
            let current = match store.current_epoch().await {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "tunables reloader: current_epoch failed");
                    continue;
                }
            };
            if Some(current) == last_seen {
                continue;
            }
            // Reload tunables from settings.
            let routing_strategy =
                sk::get_string(store.as_ref(), sk::ROUTING_DEFAULT_STRATEGY, "weighted").await;
            let routing_strategy =
                crate::config::RoutingStrategyName::parse(&routing_strategy).unwrap_or_default();
            let raw_envelope_capture_media = sk::get_bool(
                store.as_ref(),
                sk::INGRESS_RAW_ENVELOPE_CAPTURE_MEDIA,
                false,
            )
            .await;
            let require_api_key =
                sk::get_bool(store.as_ref(), sk::INGRESS_REQUIRE_API_KEY, true).await;
            let current_t = state.tunables();
            let max_request_body_bytes = sk::get_u64(
                store.as_ref(),
                sk::INGRESS_MAX_BODY_BYTES,
                current_t.max_request_body_bytes,
            )
            .await;
            let max_inflight = sk::get_usize(
                store.as_ref(),
                sk::INGRESS_MAX_INFLIGHT,
                current_t.max_inflight,
            )
            .await;
            let max_queue_depth = sk::get_usize(
                store.as_ref(),
                sk::INGRESS_MAX_QUEUE_DEPTH,
                current_t.max_queue_depth,
            )
            .await;
            let acquire_timeout = Duration::from_secs(
                sk::get_u64(
                    store.as_ref(),
                    sk::INGRESS_ACQUIRE_TIMEOUT_SECS,
                    current_t.acquire_timeout.as_secs(),
                )
                .await,
            );
            let upstream_stream_idle_timeout_secs = sk::get_u64(
                store.as_ref(),
                sk::UPSTREAM_STREAM_IDLE_TIMEOUT_SECS,
                current_t.upstream_stream_idle_timeout_secs,
            )
            .await;
            let upstream_stream_total_timeout_secs = sk::get_u64(
                store.as_ref(),
                sk::UPSTREAM_STREAM_TOTAL_TIMEOUT_SECS,
                current_t.upstream_stream_total_timeout_secs,
            )
            .await;
            // Read upstream TCP/connection-pool settings. The
            // http_client is rebuilt only when one of these three
            // values changes, to avoid churning the connection pool
            // on every epoch tick.
            let tcp_nodelay = sk::get_bool(store.as_ref(), sk::UPSTREAM_TCP_NODELAY, true).await;
            let tcp_keepalive_secs =
                sk::get_u64(store.as_ref(), sk::UPSTREAM_TCP_KEEPALIVE_SECS, 0).await;
            let pool_idle_timeout_secs =
                sk::get_u64(store.as_ref(), sk::UPSTREAM_POOL_IDLE_TIMEOUT_SECS, 0).await;
            // Rebuild header policy from settings deny lists.
            let req_deny =
                sk::get_string_list(store.as_ref(), sk::FORWARD_REQUEST_HEADER_DENY, &[]).await;
            let resp_deny =
                sk::get_string_list(store.as_ref(), sk::FORWARD_RESPONSE_HEADER_DENY, &[]).await;
            let header_policy = Arc::new(
                tiygate_core::HeaderForwardPolicy::with_defaults()
                    .with_request_deny_extra(req_deny.iter())
                    .with_response_deny_extra(resp_deny.iter()),
            );
            // Rebuild the http_client from the current TCP settings.
            // This block only runs when the epoch advanced (i.e.
            // something changed), so rebuilding is safe. The old
            // client's in-flight requests complete naturally because
            // reqwest::Client is internally Arc'd.
            let http_client = build_http_client_from_params(
                tcp_nodelay,
                tcp_keepalive_secs,
                pool_idle_timeout_secs,
            );
            drop(current_t);
            state.reload_tunables(RuntimeTunables {
                routing_strategy,
                raw_envelope_capture_media,
                require_api_key,
                header_policy,
                max_request_body_bytes,
                max_inflight,
                max_queue_depth,
                acquire_timeout,
                upstream_stream_idle_timeout_secs,
                upstream_stream_total_timeout_secs,
                http_client,
            });
            tracing::debug!(epoch = current, "tunables reloaded from settings");
            last_seen = Some(current);
        }
    })
}

/// Compute the raw-passthrough body and the same-suite flag, in a
/// way that all ingress handlers can share. When the target's
/// protocol suite matches the ingress suite and the codec declares
/// Passthrough, the `original_body` is forwarded verbatim to the
/// upstream.
///
/// **Important**: `original_body` must be the *unmodified* client
/// request body, NOT the `RawEnvelope.body` (which may have been
/// media-stripped for audit storage). Forwarding a media-stripped
/// body corrupts upstream requests — see the fix for the
/// `_media_meta` passthrough bug.
pub fn compute_pass_through<C: tiygate_core::EndpointCodec>(
    codec: &C,
    ingress_protocol: &tiygate_core::ProtocolEndpoint,
    targets: &[tiygate_core::RoutingTarget],
    original_body: &str,
) -> (bool, Option<String>) {
    let pass_through_candidate = targets.iter().any(|t| {
        ingress_protocol.suite == t.api_protocol.suite
            && matches!(
                codec.pass_through_policy(ingress_protocol, &t.api_protocol),
                tiygate_core::PassThroughPolicy::Passthrough
            )
    });
    if pass_through_candidate {
        (true, Some(original_body.to_string()))
    } else {
        (false, None)
    }
}

/// Look up the registered provider matching `target.provider_id` and
/// invoke its `AuthApplier::apply` to populate the upstream headers.
/// Falls back to a protocol-aware default applier if no registered
/// provider is found (e.g., test fixtures).
///
/// The client-supplied credential (`Authorization: Bearer …` /
/// `x-api-key: …` / `x-goog-api-key: …`) is **only** used to
/// authenticate the caller against TiyGate's own `api_keys` table
/// (for quota enforcement, audit, and per-key rate limiting). It is
/// **not** forwarded to the upstream provider — the upstream
/// always authenticates with the key configured on the routing
/// target (`target.effective_api_key()`). Mixing the two would let
/// a caller substitute a different upstream key than the one
/// TiyGate routes traffic to, breaking per-account model routing
/// and the audit trail.
pub async fn apply_provider_auth(
    target: &tiygate_core::RoutingTarget,
    upstream_headers: &mut http::HeaderMap,
    oauth_manager: &crate::oauth_manager::OAuthTokenManager,
) -> Result<(), AppError> {
    // OAuth path: if the routing target carries an OAuth config,
    // use the OAuthTokenManager to refresh/inject the access token.
    // Return immediately on success; fall through to the static
    // key path only when the target is not OAuth-mode.
    match oauth_manager.apply(target, upstream_headers).await {
        Ok(true) => return Ok(()),
        Ok(false) => { /* not OAuth — fall through to static key */ }
        Err(e) => {
            return Err(AppError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("OAuth token error: {e}"),
            ));
        }
    }

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
    //
    // In all three cases the key written to the upstream is the
    // routing target's `effective_api_key()` — the static key
    // configured on the target row, never the caller's credential.
    use tiygate_auth::{api_key::HeaderApiKeyAuthApplier, bearer::BearerAuthApplier};
    use tiygate_core::{AuthApplier, ProtocolSuite};
    if matches!(target.api_protocol.suite, ProtocolSuite::AnthropicMessages) {
        let applier = HeaderApiKeyAuthApplier {
            header_name: "x-api-key".to_string(),
        };
        if let Err(e) = applier.apply(upstream_headers, target).await {
            return Err(AppError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Provider auth applier failed: {e}"),
            ));
        }
        upstream_headers.insert(
            http::HeaderName::from_static("anthropic-version"),
            http::HeaderValue::from_static("2023-06-01"),
        );
    } else if matches!(target.api_protocol.suite, ProtocolSuite::GoogleGemini) {
        // The registered-provider branch above runs first; this
        // fallback only fires when the routing target has no
        // provider registered. We re-use the upstream `effective_api_key()`
        // — the static key configured on the target row — and
        // write it as `x-goog-api-key`.
        let applier = HeaderApiKeyAuthApplier {
            header_name: "x-goog-api-key".to_string(),
        };
        if let Err(e) = applier.apply(upstream_headers, target).await {
            return Err(AppError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Provider auth applier failed: {e}"),
            ));
        }
    } else {
        let applier = BearerAuthApplier;
        if let Err(e) = applier.apply(upstream_headers, target).await {
            return Err(AppError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("Provider auth applier failed: {e}"),
            ));
        }
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
            state.tunables().max_request_body_bytes
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
        let kept = Box::pin(super::streaming::SseKeepaliveStream::new(
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
        let mut kept = Box::pin(super::streaming::SseKeepaliveStream::new(
            inner,
            Duration::ZERO,
        ));
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

    /// The truncation reasons round-trip through `Debug` /
    /// `PartialEq` so disconnect-billing logs are reliable.
    #[test]
    fn truncation_reason_distinct() {
        assert_ne!(TruncationReason::Idle, TruncationReason::Total);
        assert_ne!(TruncationReason::Idle, TruncationReason::UpstreamError);
        assert_ne!(TruncationReason::Total, TruncationReason::UpstreamError);
        assert_ne!(
            TruncationReason::ClientDisconnect,
            TruncationReason::UpstreamError
        );
        assert_eq!(
            TruncationReason::ClientDisconnect.as_str(),
            "client_disconnect"
        );
        // Debug formatting is used by telemetry events.
        assert!(format!("{:?}", TruncationReason::Idle).contains("Idle"));
    }
}
/// Simple error type for the HTTP layer.
#[derive(Debug)]
pub struct AppError {
    status: StatusCode,
    message: String,
    /// Normalized error class for protocol-native error type mapping
    /// and fallback classification. Defaults to `Transient` when not
    /// explicitly set via `with_class()`.
    error_class: tiygate_core::ErrorClass,
    /// The ingress protocol suite, used to generate protocol-native
    /// error response bodies in `into_response()`. When `None`, falls
    /// back to a generic OpenAI-style error body.
    protocol_suite: Option<tiygate_core::ProtocolSuite>,
    /// Passthrough Retry-After header value from upstream.
    retry_after_header: Option<String>,
    /// Original upstream HTTP status for error source distinction.
    upstream_status: Option<u16>,
    /// Original upstream error code (e.g. `insufficient_quota`,
    /// `overloaded_error`), preserved for debugging passthrough as
    /// `error.code` in protocols that support it.
    upstream_error_code: Option<String>,
    /// Upstream RateLimit-* headers to passthrough on the error response.
    rate_limit_headers: Vec<(&'static str, String)>,
}

impl AppError {
    pub(crate) fn new(status: StatusCode, message: String) -> Self {
        Self {
            status,
            message,
            error_class: tiygate_core::ErrorClass::Transient,
            protocol_suite: None,
            retry_after_header: None,
            upstream_status: None,
            upstream_error_code: None,
            rate_limit_headers: Vec::new(),
        }
    }

    /// Attach a normalized error class. This drives both the
    /// protocol-native `error.type` / `error.status` field in the
    /// response body and the fallback classifier's retry/stop decision.
    pub(crate) fn with_class(mut self, class: tiygate_core::ErrorClass) -> Self {
        self.error_class = class;
        self
    }

    /// Attach the ingress protocol suite, enabling protocol-native
    /// error body generation in `into_response()`.
    pub(crate) fn with_protocol_suite(mut self, suite: tiygate_core::ProtocolSuite) -> Self {
        self.protocol_suite = Some(suite);
        self
    }

    /// Attach an upstream-native error code (e.g. `insufficient_quota`)
    /// for debugging passthrough as `error.code` in protocols that
    /// support it (OpenAI). Also influences fallback classification
    /// when `error_class` has not been explicitly set via `with_class()`.
    pub(crate) fn with_upstream_code(mut self, code: impl Into<String>) -> Self {
        self.upstream_error_code = Some(code.into());
        self
    }

    /// Public accessor for the upstream error code, if any.
    /// Used by the fallback classifier to pick `classify_structured`
    /// over substring matching.
    pub(crate) fn upstream_error_code(&self) -> Option<&str> {
        self.upstream_error_code.as_deref()
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
        // Generate protocol-native error body when the protocol suite
        // is known; otherwise fall back to a generic OpenAI-style body.
        let body = if let Some(suite) = self.protocol_suite {
            tiygate_protocols::error_body::encode_error_body_for_suite(
                suite,
                &self.message,
                self.error_class,
                self.status.as_u16(),
                self.upstream_error_code.as_deref(),
            )
        } else {
            // Fallback: generic OpenAI-style error body.
            let mut err = serde_json::json!({
                "message": self.message,
                "type": "server_error",
                "param": null,
            });
            if let Some(ref code) = self.upstream_error_code {
                err["code"] = serde_json::json!(code);
            }
            serde_json::json!({"error": err})
        };

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
