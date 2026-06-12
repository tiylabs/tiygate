//! Phase-4 ingress helpers — redacted envelopes, trace context,
//! quota enforcement, embedding cache, and `RequestEvent` emission.
//!
//! These helpers are wired into the existing ingress handlers via
//! incremental refactors; the helpers are pure functions or take
//! only `&AppState`, so handlers can call them without restructuring
//! their control flow.
//!
//! The two design pillars (per docs/ai-gateway-architecture-design.md
//! §3.4-§4.8):
//!
//! * **W3C trace propagation** — every request's `traceparent`
//!   header is extracted and the gateway's own span id is appended
//!   for telemetry correlation.
//! * **Redact-and-truncate** — the `RawEnvelope` body is truncated
//!   at `state.raw_envelope_max_bytes` and the header set is passed
//!   through the `Redactor` before any persistence path.

use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use serde_json::Value;

use tiygate_core::protocol::ProtocolEndpoint;
use tiygate_core::quota::{QuotaDecision, QuotaSpec};
use tiygate_core::redaction::Redactor;
use tiygate_core::telemetry::{EventPayload, LatencyBreakdown, PipelineEvent, RequestEvent};
// Re-exported under a stable path so external callers (admin /
// tests / future gateway extensions) can import the trace context
// type without reaching into `tiygate-core` directly. Reserved
// for the public API surface; not used in the current build.
#[allow(unused_imports)]
pub use tiygate_core::tracing_ctx::TraceContext as PublicTraceContext;
use tiygate_core::tracing_ctx::{
    extract_from_headers, new_span_id, new_trace_id, TraceContext, TraceContextExtraction,
};
use tiygate_core::RawEnvelope;
use tiygate_core::Usage;

use crate::ingress::{AppError, AppState};

// ---------------------------------------------------------------------------
// ResolvedApiKey — the result of looking up the inbound credential
// against the `api_keys` table.
//
// The credential can arrive in either an `Authorization: Bearer
// <secret>` header (OpenAI / Responses / Gemini) or an `x-api-key:
// <secret>` header (Anthropic). Once we have the cleartext, we
// look it up in the store by SHA-256 hash. A non-empty `key_id` means
// the request was authenticated; a `key_id == "anonymous"` means we
// could not identify the caller and the spec is unlimited.
//
// All four ingress handlers call `resolve_api_key` at the top so the
// quota check is real (per the Phase 4 §4.6 design) — the previous
// `QuotaSpec::default() + "anonymous"` was a placeholder that did
// not exercise the api-key → spec wiring.
// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ResolvedApiKey {
    /// The key id (`api_keys.id`). The literal string `"anonymous"`
    /// when no credential was supplied or the lookup did not match.
    pub key_id: String,
    /// The deserialized `QuotaSpec` for this key. Default (unlimited)
    /// when the lookup did not match.
    pub spec: tiygate_core::quota::QuotaSpec,
    /// The cleartext secret (only retained for the duration of the
    /// request). Useful for upstream auth fallback paths and for the
    /// trace / audit log. Never persisted.
    #[allow(dead_code)]
    pub secret: Option<String>,
}

/// Extract the credential from the inbound headers, hash it, and
/// resolve the matching `api_keys` row. Returns an `anonymous`
/// `ResolvedApiKey` when no credential is present, the credential is
/// malformed, or the lookup fails. The store call is async and short
/// (single-row index lookup), so the cost on the hot path is
/// negligible; the lookup result is memoized in a future revision.
pub async fn resolve_api_key(state: &AppState, headers: &axum::http::HeaderMap) -> ResolvedApiKey {
    // Anthropic-style keys come in `x-api-key`; everything else
    // (OpenAI, Responses, Gemini) uses `Authorization: Bearer`. We
    // try both for resilience — clients occasionally send both.
    let secret = extract_credential(headers);
    let Some(secret) = secret else {
        return ResolvedApiKey {
            key_id: "anonymous".to_string(),
            spec: tiygate_core::quota::QuotaSpec::default(),
            secret: None,
        };
    };
    // Best-effort lookup. A `None` result (key not present, or
    // disabled) is still treated as anonymous + unlimited so a
    // missing key is not a 5xx. The store can fail (db error); we
    // fail open per the §4.6 design note.
    //
    // We prefer the live `DbConfigStore` when the AppState was
    // built from the production control-plane path. The legacy
    // in-memory `ConfigStore::find_api_key_by_secret` returns
    // `Ok(None)`, so the request falls through to the unlimited
    // default — same behaviour as before for tests / single-process
    // deployments.
    let lookup_result = match state.db_store.as_ref() {
        Some(db) => db.find_api_key_by_secret(&secret).await,
        None => state.current_config().find_api_key_by_secret(&secret).await,
    };
    match lookup_result {
        Ok(Some(api_key)) => {
            use tiygate_store::models::ApiKeyStatus;
            if !matches!(api_key.status, ApiKeyStatus::Active) {
                // Disabled / revoked → fall through to anonymous +
                // unlimited. The handler can layer its own 401 later
                // when admin auth is required; for the data-plane
                // anonymous path we silently ignore disabled keys.
                return ResolvedApiKey {
                    key_id: "anonymous".to_string(),
                    spec: tiygate_core::quota::QuotaSpec::default(),
                    secret: Some(secret),
                };
            }
            let spec = tiygate_core::quota::QuotaSpec::from_json(&api_key.quota_json);
            ResolvedApiKey {
                key_id: api_key.id,
                spec,
                secret: Some(secret),
            }
        }
        Ok(None) => ResolvedApiKey {
            key_id: "anonymous".to_string(),
            spec: tiygate_core::quota::QuotaSpec::default(),
            secret: Some(secret),
        },
        Err(_) => ResolvedApiKey {
            key_id: "anonymous".to_string(),
            spec: tiygate_core::quota::QuotaSpec::default(),
            secret: Some(secret),
        },
    }
}

/// Pull the cleartext credential out of the inbound headers.
/// Recognises `Authorization: Bearer …` and `x-api-key: …`.
///
/// Returns `Some(secret)` when the header is well-formed and
/// non-empty. Returns `None` when no credential is present (the
/// caller is then treated as anonymous).
fn extract_credential(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(v) = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(rest) = v.strip_prefix("Bearer ") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        } else if let Some(rest) = v.strip_prefix("bearer ") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    if let Some(v) = headers
        .get(http::header::HeaderName::from_static("x-api-key"))
        .and_then(|v| v.to_str().ok())
    {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// RequestScope — drop-guard that guarantees a terminal RequestEvent
//
// The four non-embeddings ingress handlers each have several early-return
// points (auth/limit/decode/route/encode failures, plus the upstream call
// itself, plus the success path). To make sure we always persist a
// `RequestEvent` row even when the handler returns `Err(...)` from
// somewhere we forgot to instrument, we install a `RequestScope` at the
// top of the handler. The guard holds the data needed for emission and
// `Drop`s with a fire-and-forget event when the caller has not yet
// `complete()`-ed it. This keeps the hot path free of bookkeeping while
// making the OltpSink contract "every request → one row" hold for
// every code path.
// ---------------------------------------------------------------------------
#[allow(dead_code)]
pub struct RequestScope<'a> {
    state: &'a AppState,
    request_id: String,
    virtual_model: String,
    ingress: ProtocolEndpoint,
    egress: Option<ProtocolEndpoint>,
    resolved_provider: Option<String>,
    resolved_model: Option<String>,
    trace: TraceContext,
    started: Instant,
    /// Resolved api key id from `resolve_api_key` (`"anonymous"` when
    /// no credential was supplied). Surface this on the terminal
    /// `RequestEvent` so the OltpSink can attribute the row to a
    /// specific key — required for the per-key quota dashboard.
    api_key_id: String,
    /// Optional `RawEnvelope` captured at the ingress. Set via
    /// `set_envelope` and persisted on the terminal `RequestEvent`
    /// so the OLTP log row carries the redacted envelope for
    /// audit + replay.
    envelope: Option<RawEnvelope>,
    /// When `true`, the scope has already emitted its terminal event;
    /// Drop must not emit a second one.
    armed: bool,
}

#[allow(clippy::too_many_arguments)]
impl<'a> RequestScope<'a> {
    pub fn new(
        state: &'a AppState,
        request_id: String,
        virtual_model: impl Into<String>,
        ingress: ProtocolEndpoint,
        trace: TraceContext,
        started: Instant,
    ) -> Self {
        Self {
            state,
            request_id,
            virtual_model: virtual_model.into(),
            ingress,
            egress: None,
            resolved_provider: None,
            resolved_model: None,
            trace,
            started,
            api_key_id: "anonymous".to_string(),
            envelope: None,
            armed: true,
        }
    }

    /// Bind the egress endpoint resolved by the route lookup. Call after
    /// the first target is known.
    pub fn set_egress(&mut self, egress: ProtocolEndpoint) {
        self.egress = Some(egress);
    }

    /// Bind the resolved provider/model (the upstream target that ended
    /// up serving the request, when known).
    pub fn set_resolved(&mut self, provider: String, model: String) {
        self.resolved_provider = Some(provider);
        self.resolved_model = Some(model);
    }

    /// Bind the resolved api key id. Call after `resolve_api_key` at
    /// the top of the handler.
    pub fn set_api_key_id(&mut self, key_id: String) {
        self.api_key_id = key_id;
    }

    /// Bind the `RawEnvelope` so the terminal `RequestEvent`
    /// persists it. Call after `build_redacted_envelope`.
    pub fn set_envelope(&mut self, envelope: RawEnvelope) {
        self.envelope = Some(envelope);
    }

    /// Re-key the virtual model on the scope. Useful when the
    /// model name is not known until *after* the request body has
    /// been decoded (e.g. the chat-completions handler where the
    /// scope is created before the IR is available). The model
    /// string is part of the terminal `RequestEvent`, so a wrong
    /// value would corrupt the per-model dashboard aggregations.
    pub fn set_virtual_model(&mut self, model: String) {
        self.virtual_model = model;
    }

    /// Read-only view of the request id. Callers that need to
    /// emit their own `RequestEvent` (e.g. the embeddings handler,
    /// which must stamp `cache_hit` on the event) can use this
    /// to keep the same id across the cache-hit and cache-miss
    /// events.
    #[allow(dead_code)]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// Mark the scope as having emitted its terminal event. Future
    /// `Drop`s are no-ops. Call after `emit_ok` / `emit_error`.
    ///
    /// Currently the four production handlers all go through
    /// `emit_ok` / `emit_error` and never need to disarm, but the
    /// method is kept as a safety hatch for future handlers that
    /// may want to transfer ownership of emission to another
    /// component (e.g. a streaming-response accumulator that emits
    /// only after the SSE stream closes).
    #[allow(dead_code)]
    pub fn disarm(mut self) {
        self.armed = false;
    }

    /// Emit a terminal `RequestEvent` for the success path.
    pub fn emit_ok(self, http_status: Option<u16>) {
        self.emit_internal("ok", None, http_status);
    }

    /// Emit a terminal `RequestEvent` for an upstream / gateway error.
    pub fn emit_error(self, error_class: &str, http_status: Option<u16>) {
        self.emit_internal("error", Some(error_class), http_status);
    }

    fn emit_internal(mut self, status: &str, error_class: Option<&str>, http_status: Option<u16>) {
        self.armed = false;
        let resolved_provider = self.resolved_provider.as_deref();
        let resolved_model = self.resolved_model.as_deref();
        let egress = self.egress.as_ref();
        let envelope = self.envelope.as_ref();
        emit_request_event(
            self.state,
            &self.request_id,
            &self.virtual_model,
            resolved_provider,
            resolved_model,
            &self.ingress,
            egress,
            status,
            error_class,
            None,
            http_status,
            false,
            None,
            LatencyBreakdown {
                total_ms: self.started.elapsed().as_millis() as u64,
                upstream_ms: self.started.elapsed().as_millis() as u64,
                queue_ms: 0,
            },
            None,
            None,
            Some(&self.api_key_id),
            &self.trace,
            envelope,
        );
    }
}

impl<'a> Drop for RequestScope<'a> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // The handler is returning from a code path we did not
        // explicitly instrument. Emit a default "internal_error"
        // terminal event so the OltpSink still gets a row. We use the
        // pre-built `emit_request_event` helper so the column shape
        // matches the rest of the pipeline. `emit_request_event`
        // itself dispatches to the bus via `tokio::spawn`, so this
        // Drop is non-blocking.
        let latency_ms = LatencyBreakdown {
            total_ms: self.started.elapsed().as_millis() as u64,
            upstream_ms: self.started.elapsed().as_millis() as u64,
            queue_ms: 0,
        };
        emit_request_event(
            self.state,
            &self.request_id,
            &self.virtual_model,
            self.resolved_provider.as_deref(),
            self.resolved_model.as_deref(),
            &self.ingress,
            self.egress.as_ref(),
            "error",
            Some("internal_error"),
            None,
            Some(500u16),
            false,
            None,
            latency_ms,
            None,
            None,
            Some(&self.api_key_id),
            &self.trace,
            self.envelope.as_ref(),
        );
    }
}

/// Build a `RawEnvelope` with the body redacted, truncated, and
/// inline media stripped to metadata only (when `capture_media` is
/// `false`, which is the default per §4.1).
pub fn build_redacted_envelope(
    state: &AppState,
    method: &str,
    path: &str,
    body: &Value,
    raw_headers: &axum::http::HeaderMap,
) -> RawEnvelope {
    let mut body_str = serde_json::to_string(body).unwrap_or_default();
    let original_body_size = body_str.len() as u64;

    // If inline media capture is disabled (§4.1), strip base64
    // payloads to metadata only before applying the byte-cap
    // truncation. This reduces storage pressure for multimodal
    // requests (images, audio, etc.).
    if !state.raw_envelope_capture_media {
        body_str = strip_inline_media(&body_str);
    }

    let max = state.raw_envelope_max_bytes;
    let (stored_body, truncated) = if (body_str.len() as u64) > max {
        let cut = max as usize;
        let mut truncated = body_str;
        truncated.truncate(cut);
        (Some(truncated), true)
    } else {
        (Some(body_str), false)
    };
    let raw_iter = raw_headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()));
    let redacted = state.redactor.redact_headers(raw_iter);
    RawEnvelope {
        method: method.to_string(),
        path: path.to_string(),
        headers: redacted.into_iter().collect(),
        body: stored_body,
        truncated,
        original_body_size,
        timestamp: Utc::now(),
    }
}

/// Strip inline base64 media payloads from a JSON body string,
/// replacing them with metadata objects `{"_media_meta": {"mime":
/// "...", "size_bytes": N, "sha256_hex": "..."}}`. This is a
/// best-effort scan: it parses the body JSON, walks all string
/// values, and replaces any base64-encoded data blocks (>= 512
/// chars) with a compact metadata stub.
///
/// The approach avoids the `regex` dependency by using `serde_json`
/// to walk the JSON structure and detect large base64-like strings
/// in-place.
fn strip_inline_media(body: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(body) else {
        // If the body isn't valid JSON, return it as-is.
        return body.to_string();
    };
    strip_media_from_value(&mut value);
    serde_json::to_string(&value).unwrap_or_else(|_| body.to_string())
}

/// Recursively walk a `serde_json::Value` and replace large base64
/// strings with metadata objects.
fn strip_media_from_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (_, v) in map.iter_mut() {
                strip_media_from_value(v);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                strip_media_from_value(v);
            }
        }
        Value::String(s) => {
            if is_large_base64(s) {
                let meta = build_media_meta(s);
                *value = meta;
            }
        }
        _ => {}
    }
}

/// Heuristic: a string is considered a large base64 payload if:
/// - It's at least 512 characters long
/// - It matches the base64 character set ([A-Za-z0-9+/=])
/// - Less than 1% of non-base64 characters (allows data: URLs)
fn is_large_base64(s: &str) -> bool {
    if s.len() < 512 {
        return false;
    }

    // Count non-base64 characters. Also accept the `data:` URL
    // prefix which is common in OpenAI-style requests.
    let working = if let Some(stripped) = s.strip_prefix("data:") {
        // Skip past the MIME type and base64 marker
        if let Some(idx) = stripped.find(";base64,") {
            &stripped[idx + 8..]
        } else {
            s
        }
    } else {
        s
    };

    if working.len() < 512 {
        return false;
    }

    let total = working.len();
    let non_b64 = working
        .chars()
        .filter(|c| !c.is_ascii_alphanumeric() && *c != '+' && *c != '/' && *c != '=')
        .count();

    // Allow up to 2% non-base64 (e.g., whitespace, newlines)
    non_b64 * 100 <= total * 2
}

/// Build a `{"_media_meta": {...}}` JSON value from a base64 string,
/// capturing MIME type, approximate binary size, and SHA-256 hash.
fn build_media_meta(s: &str) -> Value {
    use sha2::{Digest, Sha256};

    // Try to extract MIME type from data: URL prefix.
    let mime = if let Some(stripped) = s.strip_prefix("data:") {
        let mime_part = stripped
            .split(';')
            .next()
            .unwrap_or("application/octet-stream");
        mime_part.to_string()
    } else {
        "application/octet-stream".to_string()
    };

    let working = if let Some(stripped) = s.strip_prefix("data:") {
        if let Some(idx) = stripped.find(";base64,") {
            &stripped[idx + 8..]
        } else {
            s
        }
    } else {
        s
    };

    let encoded_len = working.len();
    let binary_size = (encoded_len * 3) / 4;
    let hash = {
        let mut hasher = Sha256::new();
        hasher.update(working.as_bytes());
        hex::encode(hasher.finalize())
    };

    serde_json::json!({
        "_media_meta": {
            "mime": mime,
            "size_bytes": binary_size,
            "sha256_hex": hash
        }
    })
}

/// Extract the W3C trace context from the inbound headers. When
/// the inbound request is *not* traced, the gateway mints a fresh
/// root trace.
pub fn extract_trace(headers: &axum::http::HeaderMap) -> TraceContext {
    let raw_tp = headers.get("traceparent").and_then(|v| v.to_str().ok());
    let raw_ts = headers.get("tracestate").and_then(|v| v.to_str().ok());
    match extract_from_headers(raw_tp, raw_ts) {
        TraceContextExtraction::Present(mut ctx) => {
            // Stamp this gateway's span as the child of the caller's.
            // (The actual OTel SDK would do this via the span
            // context; the RequestEvent column is the practical
            // place to record the relationship for the Phase 4 log
            // surface.)
            ctx.parent_span_id = new_span_id();
            ctx
        }
        TraceContextExtraction::Absent => {
            // Mint a fresh root trace.
            TraceContext {
                trace_id: new_trace_id(),
                parent_span_id: new_span_id(),
                flags: 0x01, // sampled
                tracestate: raw_ts.map(str::to_string),
            }
        }
    }
}

/// Inject the W3C `traceparent` header into an outbound request
/// builder. Used by ingress handlers when building the upstream
/// call so the upstream service receives the same trace id.
pub fn inject_trace(
    builder: reqwest::RequestBuilder,
    ctx: &TraceContext,
) -> reqwest::RequestBuilder {
    builder.header("traceparent", ctx.to_traceparent())
}

/// Freeze an outbound request builder into the concrete
/// [`reqwest::Request`] that will actually be sent, and snapshot its
/// complete header set for the request-log detail view.
///
/// This is the single source of truth for the gateway → provider
/// (egress) headers we record: by capturing from the *built* request
/// rather than the hand-assembled `HeaderMap`, the snapshot includes
/// every header reqwest adds at finalize time (`content-type`,
/// `content-length`, the injected `traceparent`, auth, etc.) so the
/// recorded set matches the bytes on the wire. Redaction + truncation
/// happen later on the telemetry background task; the returned headers
/// are still cleartext here.
///
/// Callers send the returned request via
/// `state.http_client.execute(req)`.
pub fn finalize_egress(
    builder: reqwest::RequestBuilder,
) -> Result<(reqwest::Request, Vec<(String, String)>), AppError> {
    let req = builder.build().map_err(|e| {
        AppError::new(
            axum::http::StatusCode::BAD_GATEWAY,
            format!("build upstream request: {e}"),
        )
    })?;
    let headers = req
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    Ok((req, headers))
}

/// Charge a single request against the configured quota, returning
/// `Ok(())` for allow and a [`QuotaOutcome::Deny`] for deny. The
/// `tokens` argument is the prompt + completion token estimate; pass
/// `1` for request-level counters.
pub async fn check_quota(
    state: &AppState,
    api_key_id: &str,
    spec: &QuotaSpec,
    tokens: u64,
) -> QuotaOutcome {
    let Some(q) = state.quota.as_ref() else {
        return QuotaOutcome::Allow;
    };
    match q.check_and_consume(api_key_id, spec, tokens).await {
        Ok(QuotaDecision::Allow { .. }) => QuotaOutcome::Allow,
        Ok(QuotaDecision::Deny {
            retry_after,
            limit,
            kind,
        }) => QuotaOutcome::Deny {
            retry_after,
            limit,
            kind,
        },
        Err(_) => QuotaOutcome::Allow, // fail-open on quota backend errors
    }
}

/// Outcome of a quota check, designed for the HTTP layer to map to
/// either `200 Continue` or `429 + Retry-After`.
///
/// The `Deny` variant carries `limit` + `kind` so future revisions
/// can surface a structured 429 body (`{"error": "quota_exceeded",
/// "limit": 100, "kind": "RequestsPerMinute", "retry_after_s": 30}`)
/// without a trait change. The four production handlers today only
/// consume `retry_after` for the `Retry-After` header, so the other
/// two fields are currently dead on the hot path.
#[derive(Debug)]
#[allow(dead_code)] // `limit` + `kind` are reserved for the structured 429 body (Phase 5+).
pub enum QuotaOutcome {
    Allow,
    Deny {
        retry_after: std::time::Duration,
        limit: u64,
        kind: tiygate_core::quota::QuotaKind,
    },
}

impl QuotaOutcome {
    #[allow(dead_code)]
    pub fn is_allowed(&self) -> bool {
        matches!(self, QuotaOutcome::Allow)
    }
    #[allow(dead_code)]
    pub fn retry_after_seconds(&self) -> Option<u64> {
        match self {
            QuotaOutcome::Deny { retry_after, .. } => Some(retry_after.as_secs().max(1)),
            _ => None,
        }
    }
}

/// Embedding cache lookup.
pub async fn embedding_cache_lookup(
    state: &AppState,
    key: &tiygate_cache::embedding_cache::EmbeddingCacheKey,
) -> Option<Arc<serde_json::Value>> {
    let cache = state.embedding_cache.as_ref()?;
    cache
        .get(key)
        .await
        .map(|entry| Arc::new(entry.response.clone()))
}

/// Embedding cache write.
pub async fn embedding_cache_store(
    state: &AppState,
    key: &tiygate_cache::embedding_cache::EmbeddingCacheKey,
    response: serde_json::Value,
) {
    if let Some(cache) = state.embedding_cache.as_ref() {
        cache.put(key, response).await;
    }
}

/// Build a `RequestEvent` from the request hot-path data and push
/// it to the telemetry bus. Phase-4 OltpSink picks it up; stdout
/// sinks surface it as JSON.
#[allow(clippy::too_many_arguments)]
pub fn emit_request_event(
    state: &AppState,
    request_id: &str,
    virtual_model: &str,
    resolved_provider: Option<&str>,
    resolved_model: Option<&str>,
    ingress: &ProtocolEndpoint,
    egress: Option<&ProtocolEndpoint>,
    status: &str,
    error_class: Option<&str>,
    error_source: Option<&str>,
    http_status: Option<u16>,
    lossy: bool,
    cache_hit: Option<&str>,
    latency: LatencyBreakdown,
    ttfb_ms: Option<u64>,
    tokens: Option<Usage>,
    api_key_id: Option<&str>,
    trace: &TraceContext,
    envelope: Option<&RawEnvelope>,
) {
    let event = RequestEvent {
        request_id: request_id.to_string(),
        timestamp: Utc::now(),
        virtual_model: virtual_model.to_string(),
        resolved_provider: resolved_provider.map(str::to_string),
        resolved_model: resolved_model.map(str::to_string),
        account_label: None,
        trace_id: Some(trace.trace_id.clone()),
        span_id: Some(trace.parent_span_id.clone()),
        traceparent: Some(trace.to_traceparent()),
        ingress_protocol: format!(
            "{}/{}/{}",
            ingress.suite.label(),
            ingress.name,
            ingress.version
        ),
        egress_protocol: egress.map(|e| format!("{}/{}/{}", e.suite.label(), e.name, e.version)),
        lossy,
        cache_hit: cache_hit.map(str::to_string),
        status: status.to_string(),
        error_class: error_class.map(str::to_string),
        http_status,
        error_source: error_source.map(str::to_string),
        latency_ms: latency.clone(),
        ttfb_ms,
        tokens: tokens.clone(),
        cost: None,
        api_key_id: api_key_id.map(str::to_string),
        client_ip: None,
        user_agent: None,
        // Persist the redacted, truncated envelope alongside the
        // event row so an operator can replay a failed request
        // via the envelope. The `Redactor` is already applied at
        // envelope build time, so the value is safe to store.
        raw_envelope: envelope.cloned(),
    };
    // Send the RequestEvent to the telemetry bus. The bus is async;
    // spawn the send so the request hot path never blocks.
    let bus = state.telemetry.clone();
    let bus2 = state.telemetry.clone();
    let event_for_bus = event;
    let pe = PipelineEvent {
        request_id: request_id.to_string(),
        timestamp: Utc::now(),
        stage: "request_completed".to_string(),
        payload: EventPayload::RequestCompleted {
            status: status.to_string(),
            error_class: error_class.map(str::to_string),
            total_latency_ms: latency.total_ms,
            upstream_latency_ms: latency.upstream_ms,
            ttfb_ms,
            tokens,
            cost: None,
            api_key_id: api_key_id.map(str::to_string),
            client_ip: None,
            user_agent: None,
            trace_id: Some(trace.trace_id.clone()),
            span_id: Some(trace.parent_span_id.clone()),
        },
    };
    tokio::spawn(async move {
        bus.send_request_event(event_for_bus).await;
    });
    tokio::spawn(async move {
        bus2.send(pe).await;
    });
}

/// Compute a single upstream URL + method + body triple from the
/// request context. (Placeholder for future protocol-aware
/// addressing; the current ingress builds the URL inline in each
/// handler.)
#[allow(dead_code)]
pub fn upstream_url_for(target: &tiygate_core::RoutingTarget, suffix: &str) -> String {
    format!(
        "{}/{}",
        target.effective_api_base().trim_end_matches('/'),
        suffix.trim_start_matches('/')
    )
}

/// Re-export `Redactor` for convenience — admin tests use the
/// same construction path as ingress helpers.
#[allow(dead_code)]
pub fn redactor() -> Redactor {
    Redactor::with_defaults()
}

/// Compact `emit_request_event` wrapper used by the four non-
/// embeddings handlers. Centralises the field set so the per-
/// handler diff stays small and the call sites read like
/// `emit_completion(&state, &trace, ..., "ok", Some(200), None, started)`.
#[allow(clippy::too_many_arguments, dead_code)]
pub fn emit_completion(
    state: &AppState,
    request_id: &str,
    virtual_model: &str,
    resolved_provider: Option<&str>,
    resolved_model: Option<&str>,
    ingress: &ProtocolEndpoint,
    egress: Option<&ProtocolEndpoint>,
    status: &str,
    error_class: Option<&str>,
    http_status: Option<u16>,
    trace: &TraceContext,
    api_key_id: Option<&str>,
    started: Instant,
    envelope: Option<&RawEnvelope>,
) {
    let latency_ms = LatencyBreakdown {
        total_ms: started.elapsed().as_millis() as u64,
        upstream_ms: started.elapsed().as_millis() as u64,
        queue_ms: 0,
    };
    emit_request_event(
        state,
        request_id,
        virtual_model,
        resolved_provider,
        resolved_model,
        ingress,
        egress,
        status,
        error_class,
        None,
        http_status,
        false,
        None,
        latency_ms,
        None,
        None,
        api_key_id,
        trace,
        envelope,
    );
}

#[cfg(test)]
mod api_key_resolution_tests {
    //! Unit tests for the `extract_credential` helper. The
    //! `resolve_api_key` async path needs a real (or in-memory)
    //! `ConfigStore`; we cover that via the integration tests in
    //! `crates/server/tests/`. Here we only exercise the pure
    //! header-parsing logic.
    use super::extract_credential;
    use axum::http::{HeaderMap, HeaderName, HeaderValue};

    fn headers_from(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            // The test inputs are static `&'static str` values, so
            // the `from_bytes` / `from_str` calls cannot fail at
            // runtime. `expect` documents the invariant; the
            // workspace lints do not allow `unwrap`, so we use
            // `expect` with a precise message. The linter
            // (clippy::expect_used) is denied at the workspace
            // level; this test helper is the *only* place where a
            // panic is acceptable (the input is a static literal,
            // not user data), so we suppress the lint for this
            // one function.
            #[allow(clippy::expect_used, clippy::unwrap_used)]
            let (name, value) = (
                HeaderName::from_bytes(k.as_bytes()).expect("static test header name must parse"),
                HeaderValue::from_str(v).expect("static test header value must parse"),
            );
            h.insert(name, value);
        }
        h
    }

    #[test]
    fn extract_bearer_token() {
        let h = headers_from(&[("authorization", "Bearer sk-abc123")]);
        assert_eq!(extract_credential(&h).as_deref(), Some("sk-abc123"));
    }

    #[test]
    fn extract_lowercase_bearer() {
        let h = headers_from(&[("authorization", "bearer sk-abc123")]);
        assert_eq!(extract_credential(&h).as_deref(), Some("sk-abc123"));
    }

    #[test]
    fn extract_x_api_key_fallback() {
        // No Authorization header — fall back to x-api-key.
        let h = headers_from(&[("x-api-key", "sk-ant-xyz")]);
        assert_eq!(extract_credential(&h).as_deref(), Some("sk-ant-xyz"));
    }

    #[test]
    fn extract_prefers_authorization_over_x_api_key() {
        let h = headers_from(&[
            ("authorization", "Bearer sk-abc"),
            ("x-api-key", "sk-ant-xyz"),
        ]);
        assert_eq!(extract_credential(&h).as_deref(), Some("sk-abc"));
    }

    #[test]
    fn extract_empty_bearer_returns_none() {
        let h = headers_from(&[("authorization", "Bearer ")]);
        assert_eq!(extract_credential(&h), None);
    }

    #[test]
    fn extract_non_bearer_auth_is_ignored() {
        // `Basic`, `Digest`, etc. — we only support Bearer / x-api-key.
        let h = headers_from(&[("authorization", "Basic dXNlcjpwYXNz")]);
        assert_eq!(extract_credential(&h), None);
    }

    #[test]
    fn extract_missing_credential_returns_none() {
        let h = HeaderMap::new();
        assert_eq!(extract_credential(&h), None);
    }
}
