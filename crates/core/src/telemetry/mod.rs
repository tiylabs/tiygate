//! Telemetry layer — event types and the asynchronous telemetry bus.
//!
//! Events are produced at pipeline stage boundaries and dispatched to
//! a bounded mpsc channel. The channel decouples hot-path request processing
//! from slow I/O (database writes, OTel export). When the channel is full,
//! low-value events are dropped rather than blocking the request.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ir::Usage;

// ---------------------------------------------------------------------------
// Request status / error class enums
//
// These enums normalise the previously free-form `status` (`"ok"` / `"error"`)
// and `error_class` (various PascalCase + snake_case literals) strings into
// a closed, type-safe set. The DB layer still stores TEXT, so `from_str`
// accepts both the new snake_case canonical form and the legacy PascalCase /
// `"ok"` / `"error"` values for backward compatibility with pre-migration
// rows.
// ---------------------------------------------------------------------------

/// Coarse-grained outcome of a request: success, a business-level failure
/// (upstream rejected the request), or an abnormal termination (gateway /
/// transport error that is not the upstream's fault).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    /// The request completed successfully (possibly with a stream truncation).
    #[default]
    Success,
    /// A business-level failure: the upstream returned an error that is
    /// attributable to the request itself or the upstream's policy
    /// (rate limit, auth, bad request, all targets exhausted, …).
    Failed,
    /// An abnormal termination not attributable to the upstream's business
    /// logic — e.g. an internal gateway error or a client disconnect.
    Abnormal,
}

impl RequestStatus {
    /// Canonical lowercase string stored in the DB / emitted in JSON.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Abnormal => "abnormal",
        }
    }

    /// Parse a stored status string. Accepts the new canonical values
    /// (`"success"` / `"failed"` / `"abnormal"`) and the legacy values
    /// (`"ok"` → `Success`, `"error"` → `Failed` as a default; the caller
    /// is expected to refine `"error"` into `Failed` vs `Abnormal` using
    /// the `error_class` tier when available).
    pub fn parse_str(s: &str) -> Option<Self> {
        match s {
            "success" | "ok" => Some(Self::Success),
            "failed" => Some(Self::Failed),
            "abnormal" => Some(Self::Abnormal),
            // Legacy "error" — map to Failed as the safe default; the
            // OLTP read path refines this using `error_class.tier()`.
            "error" => Some(Self::Failed),
            _ => None,
        }
    }

    /// Human-readable label for display / logging.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Abnormal => "abnormal",
        }
    }
}

/// Distinguishes a business-level failure from an abnormal termination.
/// Used by `RequestErrorClass::tier()` to derive `RequestStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorTier {
    /// A failure attributable to the upstream / request.
    Failed,
    /// An abnormal termination not attributable to the upstream's
    /// business logic (internal error, client disconnect).
    Abnormal,
}

impl From<ErrorTier> for RequestStatus {
    fn from(tier: ErrorTier) -> Self {
        match tier {
            ErrorTier::Failed => RequestStatus::Failed,
            ErrorTier::Abnormal => RequestStatus::Abnormal,
        }
    }
}

/// The closed set of error classes for request logs.
///
/// The canonical DB / JSON representation is `snake_case` (see `as_str`).
/// `from_str` also accepts the legacy PascalCase literals (`"Transient"`,
/// `"RateLimited"`, `"Auth"`, `"BadRequest"`, `"LossyOrCapability"`,
/// `"CircuitBreaker"`, `"DeadlineExceeded"`) emitted by older code paths
/// so historical rows continue to render correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestErrorClass {
    /// Transient upstream error (5xx, timeout, transport) — retryable.
    Transient,
    /// Upstream rate-limited the request (429).
    RateLimited,
    /// Upstream authentication / authorisation error (401/403).
    UpstreamAuth,
    /// Malformed request rejected by the upstream (400/422).
    BadRequest,
    /// Capability mismatch or lossy protocol conversion.
    LossyOrCapability,
    /// Target skipped due to an open circuit breaker.
    CircuitBreaker,
    /// Request exceeded the fallback deadline.
    DeadlineExceeded,
    /// All upstream targets were exhausted without a success.
    UpstreamExhausted,
    /// Inbound API key missing when `require_api_key` is on.
    AuthMissing,
    /// Inbound API key did not match any active key.
    AuthInvalid,
    /// Inbound API key matched a disabled / revoked key.
    AuthDisabled,
    /// Gateway internal error (unexpected drop, handler panic, …).
    InternalError,
    /// Client disconnected mid-request.
    ClientDisconnect,
    /// Gateway quota exceeded (inbound rate limit / token budget).
    QuotaExceeded,
    /// Request body could not be decoded (malformed JSON / protocol).
    DecodeError,
    /// No route found for the requested virtual model.
    RouteNotFound,
}

impl RequestErrorClass {
    /// Canonical `snake_case` string stored in the DB / emitted in JSON.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::RateLimited => "rate_limited",
            Self::UpstreamAuth => "upstream_auth",
            Self::BadRequest => "bad_request",
            Self::LossyOrCapability => "lossy_or_capability",
            Self::CircuitBreaker => "circuit_breaker",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::UpstreamExhausted => "upstream_exhausted",
            Self::AuthMissing => "auth_missing",
            Self::AuthInvalid => "auth_invalid",
            Self::AuthDisabled => "auth_disabled",
            Self::InternalError => "internal_error",
            Self::ClientDisconnect => "client_disconnect",
            Self::QuotaExceeded => "quota_exceeded",
            Self::DecodeError => "decode_error",
            Self::RouteNotFound => "route_not_found",
        }
    }

    /// Parse a stored error-class string. Accepts the new canonical
    /// `snake_case` values and the legacy PascalCase literals.
    pub fn parse_str(s: &str) -> Option<Self> {
        match s {
            "transient" | "Transient" => Some(Self::Transient),
            "rate_limited" | "RateLimited" => Some(Self::RateLimited),
            "upstream_auth" | "Auth" => Some(Self::UpstreamAuth),
            "bad_request" | "BadRequest" => Some(Self::BadRequest),
            "lossy_or_capability" | "LossyOrCapability" => Some(Self::LossyOrCapability),
            "circuit_breaker" | "CircuitBreaker" => Some(Self::CircuitBreaker),
            "deadline_exceeded" | "DeadlineExceeded" => Some(Self::DeadlineExceeded),
            "upstream_exhausted" => Some(Self::UpstreamExhausted),
            "auth_missing" => Some(Self::AuthMissing),
            "auth_invalid" => Some(Self::AuthInvalid),
            "auth_disabled" => Some(Self::AuthDisabled),
            "internal_error" => Some(Self::InternalError),
            "client_disconnect" => Some(Self::ClientDisconnect),
            "quota_exceeded" => Some(Self::QuotaExceeded),
            "decode_error" => Some(Self::DecodeError),
            "route_not_found" => Some(Self::RouteNotFound),
            _ => None,
        }
    }

    /// Classify the error into a tier, which maps to a `RequestStatus`.
    /// `InternalError` and `ClientDisconnect` are abnormal terminations;
    /// all others are business-level failures.
    pub fn tier(&self) -> ErrorTier {
        match self {
            Self::InternalError | Self::ClientDisconnect => ErrorTier::Abnormal,
            _ => ErrorTier::Failed,
        }
    }
}

/// A pipeline event emitted at stage boundaries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineEvent {
    /// Unique request identifier.
    pub request_id: String,
    /// Timestamp of the event.
    pub timestamp: DateTime<Utc>,
    /// The stage that produced this event.
    pub stage: String,
    /// Event payload.
    pub payload: EventPayload,
}

/// The specific event type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventPayload {
    /// Request started.
    RequestStarted {
        virtual_model: String,
        ingress_protocol: String,
        stream: bool,
    },
    /// Routing decision made.
    RouteResolved {
        targets: Vec<String>,
        strategy: String,
    },
    /// Execution attempt against a target.
    HopStart {
        target: String,
        provider: String,
        model: String,
        egress_protocol: String,
        hop: usize,
    },
    /// Execution succeeded.
    HopSuccess {
        target: String,
        hop: usize,
        latency_ms: u64,
        usage: Option<Usage>,
    },
    /// Execution failed.
    HopFailure {
        target: String,
        hop: usize,
        error: String,
        error_class: String,
        latency_ms: u64,
    },
    /// Fallback policy decision after an execution attempt.
    HopDecision {
        target: String,
        hop: usize,
        decision: String,
    },
    /// Request completed (success or failure).
    RequestCompleted {
        status: String,
        error_class: Option<String>,
        total_latency_ms: u64,
        upstream_latency_ms: u64,
        ttfb_ms: Option<u64>,
        tokens: Option<Usage>,
        cost: Option<u64>,
        api_key_id: Option<String>,
        client_ip: Option<String>,
        user_agent: Option<String>,
        trace_id: Option<String>,
        span_id: Option<String>,
    },
}

/// The complete request log event (aggregated after request completion).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestEvent {
    pub request_id: String,
    pub timestamp: DateTime<Utc>,
    pub virtual_model: String,
    pub resolved_provider: Option<String>,
    pub resolved_model: Option<String>,
    pub account_label: Option<String>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub traceparent: Option<String>,
    pub ingress_protocol: String,
    pub egress_protocol: Option<String>,
    pub lossy: bool,
    pub cache_hit: Option<String>,
    pub status: RequestStatus,
    pub error_class: Option<RequestErrorClass>,
    pub http_status: Option<u16>,
    pub error_source: Option<String>,
    pub latency_ms: LatencyBreakdown,
    pub ttfb_ms: Option<u64>,
    pub tokens: Option<Usage>,
    pub cost: Option<u64>,
    pub api_key_id: Option<String>,
    pub client_ip: Option<String>,
    pub user_agent: Option<String>,
    /// The redacted `RawEnvelope` captured at the
    /// ingress. Persisted to the OLTP log table so an operator can
    /// replay a failed request via the envelope and inspect the
    /// exact headers / body the caller sent. Per §8 acceptance
    /// #3 / #8 ("RawEnvelope 默认脱敏存储"), the `Redactor` is
    /// already applied at build time so the value here is safe to
    /// store as-is.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub raw_envelope: Option<crate::RawEnvelope>,
}

/// Latency breakdown for a request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LatencyBreakdown {
    pub total_ms: u64,
    pub upstream_ms: u64,
    pub queue_ms: u64,
}

/// A full request/response exchange capture for the request-log
/// detail view. Carries the raw (un-redacted) headers and bodies
/// as captured on the request hot path. The telemetry background
/// task redacts (and for SSE parses) these before persistence, so
/// the hot path stays cheap (clone/move only).
///
/// Headers are `Vec<(name, value)>` to preserve order and duplicates.
/// Bodies are raw `String`s (JSON for non-stream, concatenated SSE
/// bytes for stream). All fields are best-effort; missing data is
/// represented as empty/`None`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExchangeCapture {
    /// Gateway-side request id; matches `RequestEvent::request_id`.
    pub request_id: String,
    /// HTTP method used for the gateway → provider request (e.g.
    /// "POST"). Mirrors the value of `reqwest::Request::method()` at
    /// capture time.
    pub egress_method: String,
    /// URL path used for the gateway → provider request (e.g.
    /// "/v1/chat/completions"). Mirrors `req.url().path()` at
    /// capture time. The full URL is intentionally not stored to
    /// avoid leaking the provider's `api_base` plus path in the log.
    pub egress_path: String,
    /// Gateway → Provider request headers (the headers actually sent
    /// upstream, including injected auth + traceparent).
    pub egress_headers: Vec<(String, String)>,
    /// Gateway → Provider request body (JSON serialized).
    pub egress_body: Option<String>,
    /// Provider → Gateway HTTP status code.
    pub upstream_status: Option<u16>,
    /// Provider → Gateway response headers.
    pub upstream_resp_headers: Vec<(String, String)>,
    /// Provider → Gateway response body (full JSON for non-stream,
    /// concatenated raw SSE bytes for stream).
    pub upstream_resp_body: Option<String>,
    /// Gateway → Client response headers.
    pub client_resp_headers: Vec<(String, String)>,
    /// Gateway → Client response body.
    pub client_resp_body: Option<String>,
    /// Whether the exchange used a streaming (SSE) response.
    pub is_stream: bool,
    /// If the streaming response was terminated before a clean natural
    /// end, the reason: "idle" (idle timer), "total" (total wall-clock
    /// budget), "upstream_error" (upstream connection error mid-stream),
    /// or "client_disconnect" (downstream client cancelled). `None` for
    /// a clean end-of-stream and for non-stream exchanges.
    pub truncation_reason: Option<String>,
    /// Duration of the streaming body transfer in milliseconds,
    /// measured from upstream response-header arrival to stream
    /// EOF / error / timeout. Only set for SSE stream responses;
    /// `None` for non-stream exchanges. Used to compute output
    /// token rate: `completion_tokens / (stream_duration_ms / 1000)`.
    pub stream_duration_ms: Option<u64>,
    /// When the upstream returned HTTP 200 but the SSE stream
    /// contained an embedded error frame (e.g.
    /// `service_unavailable_error`, `overloaded_error`), this
    /// carries the error message so the OLTP sink can mark the
    /// request as failed despite the 200 status. `None` for clean
    /// streams and non-stream exchanges.
    pub upstream_error: Option<String>,
}

/// The telemetry bus — decouples event production from consumption.
#[async_trait::async_trait]
pub trait TelemetryBus: Send + Sync {
    /// Send an event to the bus (non-blocking).
    async fn send(&self, event: PipelineEvent);
    /// Send a completed request event.
    async fn send_request_event(&self, event: RequestEvent);
    /// Send a full request/response exchange capture (non-blocking).
    ///
    /// Default no-op so existing bus implementations remain valid;
    /// the production `ChannelTelemetryBus` overrides this to enqueue
    /// the capture for background persistence.
    async fn send_capture(&self, _capture: ExchangeCapture) {}
}

/// A log/event sink that persists events.
#[async_trait::async_trait]
pub trait EventSink: Send + Sync {
    /// Write a pipeline event to the sink.
    async fn write_event(&self, event: &PipelineEvent) -> Result<(), crate::Error>;
    /// Write a completed request event.
    async fn write_request_event(&self, event: &RequestEvent) -> Result<(), crate::Error>;
    /// Persist a full request/response exchange capture.
    ///
    /// Default no-op so existing sinks remain valid; the `OltpSink`
    /// overrides this to redact, truncate, parse SSE, and write the
    /// `request_payloads` row.
    async fn write_capture(&self, _capture: &ExchangeCapture) -> Result<(), crate::Error> {
        Ok(())
    }
    /// Flush any buffered events.
    async fn flush(&self) -> Result<(), crate::Error>;
}

/// Token kind for pricing queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    /// Prompt / input tokens.
    Input,
    /// Completion / output tokens.
    Output,
    /// Cached input tokens (prompt caching, Anthropic-style).
    CacheRead,
    /// Cache write tokens.
    CacheWrite,
}

/// Cost in micro-USD (1/1_000_000 of a cent).
pub type MicroUsd = u64;

/// Pluggable pricing data source for translating token usage into cost.
///
/// This trait is **reserved** for a future reliable pricing source. No
/// implementation is wired in Phase 4 because no trustworthy, complete
/// pricing API exists today (see §3.3 of the architecture). All `cost`
/// fields on events remain `None` until a `PriceProvider` is configured.
pub trait PriceProvider: Send + Sync {
    /// Return the unit price in micro-USD for a given model and token kind,
    /// or `None` when the price is unknown.
    fn unit_price(&self, model: &str, kind: TokenKind) -> Option<MicroUsd>;
}
