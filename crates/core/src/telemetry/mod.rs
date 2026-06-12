//! Telemetry layer — event types and the asynchronous telemetry bus.
//!
//! Events are produced at pipeline stage boundaries and dispatched to
//! a bounded mpsc channel. The channel decouples hot-path request processing
//! from slow I/O (database writes, OTel export). When the channel is full,
//! low-value events are dropped rather than blocking the request.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ir::Usage;

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
        latency_ms: u64,
        usage: Option<Usage>,
    },
    /// Execution failed.
    HopFailure {
        target: String,
        error: String,
        error_class: String,
        latency_ms: u64,
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
    pub status: String,
    pub error_class: Option<String>,
    pub http_status: Option<u16>,
    pub error_source: Option<String>,
    pub latency_ms: LatencyBreakdown,
    pub ttfb_ms: Option<u64>,
    pub tokens: Option<Usage>,
    pub cost: Option<u64>,
    pub api_key_id: Option<String>,
    pub client_ip: Option<String>,
    pub user_agent: Option<String>,
    /// The redacted, truncated `RawEnvelope` captured at the
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
/// detail view. Carries the raw (un-redacted, un-truncated) headers
/// and bodies as captured on the request hot path. The telemetry
/// background task redacts + truncates + (for SSE) parses these
/// before persistence, so the hot path stays cheap (clone/move only).
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
