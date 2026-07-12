//! SSE streaming helpers — keepalive wrapper, upstream stream driver,
//! and cross-protocol transcode support.

use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::{Future, Stream, StreamExt};
use pin_project::pin_project;

use tiygate_core::{TruncationReason, UsageAccumulator};

use super::response_model::ResponseModelOverride;
use super::AppState;
// ---------------------------------------------------------------------------
// Streaming helper types
// ---------------------------------------------------------------------------

/// Default keepalive cadence for downstream SSE proxies. Cheap to send
/// (`:keepalive\n\n` is a single SSE comment line) and short enough to
/// keep corporate proxies from killing the connection on idle.
pub(super) const DEFAULT_SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Unified byte stream used by the downstream SSE bridge. HTTP/SSE and
/// WebSocket transports both normalize into this form before they reach the
/// protocol codec layer.
pub(super) type UpstreamByteStream =
    Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send + 'static>>;

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
pub(super) struct SseKeepaliveStream<S> {
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

impl<S: Stream<Item = Result<Bytes, axum::Error>>> Stream for SseKeepaliveStream<S> {
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

/// Drive an upstream byte stream to the downstream client as an SSE stream.
/// HTTP SSE and WebSocket JSON event frames both reach this bridge after the
/// transport executor has normalized them into complete SSE frames. Adds:
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
pub(super) struct StreamCapture {
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
    /// Health registry for recording stream-level failures (error
    /// frames, truncation) that arrive after the HTTP 200 `record_success`
    /// in the fallback loop. When `Some`, the guard calls
    /// `record_failure` on stream termination if an upstream error or
    /// truncation was observed.
    pub health: Option<Arc<tiygate_core::HealthRegistry>>,
    /// The health-registry key for this routing target
    /// (`provider_id:model_id`). Paired with `health`.
    pub health_key: Option<String>,
}

/// Shared, single-shot finalizer for stream exchange capture.
///
/// The response body stream may be dropped by hyper when the downstream
/// client disconnects. Code placed at the tail of `async_stream::stream!`
/// is skipped in that case, so this guard owns the accumulated bytes and
/// only falls back to `client_disconnect` when no upstream terminal or
/// error signal was observed.
struct StreamCaptureGuard {
    inner: Arc<std::sync::Mutex<StreamCaptureState>>,
    accum: Arc<std::sync::Mutex<UsageAccumulator>>,
    health: Option<Arc<tiygate_core::HealthRegistry>>,
    health_key: Option<String>,
}

struct StreamCaptureState {
    capture: Option<StreamCapture>,
    upstream_body: Vec<u8>,
    client_body: Vec<u8>,
    started: Instant,
    last_reason: Option<TruncationReason>,
    finalized: bool,
    /// Set to true once the upstream has delivered a natural termination
    /// signal (the `None` branch in `drive_upstream_stream` was reached).
    /// When the downstream client closes immediately after receiving the
    /// last frame, the `async_stream` future is cancelled at the `yield`
    /// point *before* `finalize_spawn()` runs. `Drop` checks this flag to
    /// distinguish "stream completed naturally, client just closed early"
    /// from "client truly disconnected mid-stream".
    upstream_completed: bool,
}

impl StreamCaptureGuard {
    fn new(capture: Option<StreamCapture>, accum: Arc<std::sync::Mutex<UsageAccumulator>>) -> Self {
        let (health, health_key) = capture
            .as_ref()
            .map(|c| (c.health.clone(), c.health_key.clone()))
            .unwrap_or((None, None));
        Self {
            inner: Arc::new(std::sync::Mutex::new(StreamCaptureState {
                capture,
                upstream_body: Vec::new(),
                client_body: Vec::new(),
                started: Instant::now(),
                last_reason: None,
                finalized: false,
                upstream_completed: false,
            })),
            accum,
            health,
            health_key,
        }
    }

    fn append_upstream(&self, bytes: &[u8]) {
        if let Ok(mut state) = self.inner.lock() {
            state.upstream_body.extend_from_slice(bytes);
        }
    }

    fn append_client(&self, bytes: &[u8]) {
        if let Ok(mut state) = self.inner.lock() {
            state.client_body.extend_from_slice(bytes);
        }
    }

    fn set_reason(&self, reason: Option<TruncationReason>) {
        if let Ok(mut state) = self.inner.lock() {
            state.last_reason = reason;
        }
    }

    fn mark_upstream_completed(&self) {
        if let Ok(mut state) = self.inner.lock() {
            state.upstream_completed = true;
        }
    }

    fn upstream_len(&self) -> usize {
        self.inner
            .lock()
            .map(|state| state.upstream_body.len())
            .unwrap_or(0)
    }

    fn request_id(&self) -> String {
        self.inner
            .lock()
            .ok()
            .and_then(|state| state.capture.as_ref().map(|cap| cap.request_id.clone()))
            .unwrap_or_default()
    }

    /// Check whether the stream terminated with an upstream error or
    /// gateway-observed truncation, and if so, record a health-registry
    /// failure for this routing target. This corrects the optimistic
    /// `record_success` called in the fallback loop (which fires on the
    /// HTTP 200 before the stream body is consumed) so that consecutive
    /// stream-level failures still drive the circuit breaker.
    fn record_health_failure_if_needed(&self) {
        let should_fail = {
            let state = self.inner.lock().ok();
            let truncation_is_failure = state
                .as_ref()
                .and_then(|s| s.last_reason)
                .map(|r| {
                    matches!(
                        r,
                        TruncationReason::UpstreamError
                            | TruncationReason::Idle
                            | TruncationReason::Total
                    )
                })
                .unwrap_or(false);
            let has_upstream_error = self
                .accum
                .lock()
                .map(|a| a.upstream_error.is_some())
                .unwrap_or(false);
            truncation_is_failure || has_upstream_error
        };
        if should_fail {
            if let (Some(health), Some(key)) = (&self.health, &self.health_key) {
                health.record_failure(key);
            }
        }
    }

    /// Returns `Some(client_disconnect)` only when no upstream terminal,
    /// error, or gateway-side truncation signal has already been recorded.
    fn drop_fallback_reason(&self) -> Option<TruncationReason> {
        let state = self.inner.lock().ok();
        let completed = state
            .as_ref()
            .map(|s| s.upstream_completed)
            .unwrap_or(false);
        let last_reason = state.as_ref().and_then(|s| s.last_reason);
        let accum = self.accum.lock().ok();
        let saw_upstream_terminal = accum.as_ref().map(|a| a.upstream_terminal).unwrap_or(false);
        let saw_upstream_error = accum
            .as_ref()
            .map(|a| a.upstream_error.is_some())
            .unwrap_or(false);

        if completed || last_reason.is_some() || saw_upstream_terminal || saw_upstream_error {
            None
        } else {
            Some(TruncationReason::ClientDisconnect)
        }
    }

    /// Mark finalized and fire-and-forget the capture via `tokio::spawn`.
    /// Use this **before** the final `yield` / `break` in the stream loop
    /// so that a client disconnect immediately after the last frame does
    /// not cancel an in-flight `finalize().await` and trigger the `Drop`
    /// fallback (`client_disconnect`).
    fn finalize_spawn(&self) {
        self.record_health_failure_if_needed();
        if let Some((telemetry, capture)) = self.take_capture(None) {
            tokio::spawn(async move {
                telemetry.send_capture(capture).await;
            });
        }
    }

    fn take_capture(
        &self,
        fallback_reason: Option<TruncationReason>,
    ) -> Option<(
        Arc<dyn tiygate_core::TelemetryBus>,
        tiygate_core::ExchangeCapture,
    )> {
        let mut state = self.inner.lock().ok()?;
        if state.finalized {
            return None;
        }
        state.finalized = true;
        if state.last_reason.is_none() {
            state.last_reason = fallback_reason;
        }
        let cap = state.capture.take()?;
        let telemetry = cap.telemetry.clone();
        let stream_duration_ms = Some(state.started.elapsed().as_millis() as u64);
        let upstream_resp_body = if state.upstream_body.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&state.upstream_body).into_owned())
        };
        let client_resp_body = if state.client_body.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(&state.client_body).into_owned())
        };
        // Check if the accumulator detected an embedded upstream
        // error frame (HTTP 200 + SSE error frame like
        // service_unavailable_error). If so, propagate it to the
        // capture so the OLTP sink can mark the request as failed.
        let upstream_error = self.accum.lock().ok().and_then(|a| {
            a.upstream_error.as_ref().map(|e| {
                if let Some(code) = &e.code {
                    format!("{} ({})", e.message, code)
                } else {
                    e.message.clone()
                }
            })
        });
        // Map the upstream error code to a canonical
        // `RequestErrorClass` string so the OLTP sink can populate
        // `request_logs.error_class` accurately instead of
        // hardcoding "transient".
        let upstream_error_class = self.accum.lock().ok().and_then(|a| {
            a.upstream_error
                .as_ref()
                .map(|e| e.class.to_request_class().as_str().to_string())
        });
        Some((
            telemetry,
            tiygate_core::ExchangeCapture {
                request_id: cap.request_id,
                egress_method: cap.egress_method,
                egress_path: cap.egress_path,
                egress_headers: cap.egress_headers,
                egress_body: cap.egress_body,
                upstream_status: cap.upstream_status,
                upstream_resp_headers: cap.upstream_resp_headers,
                upstream_resp_body,
                client_resp_headers: cap.client_resp_headers,
                client_resp_body,
                is_stream: true,
                truncation_reason: state.last_reason.map(|r| r.as_str().to_string()),
                stream_duration_ms,
                upstream_error,
                upstream_error_class,
            },
        ))
    }
}

impl Drop for StreamCaptureGuard {
    fn drop(&mut self) {
        let disconnect_reason = self.drop_fallback_reason();
        let Some((telemetry, capture)) = self.take_capture(disconnect_reason) else {
            return;
        };
        // Only reached when finalize_spawn did NOT run (client
        // disconnect or cancelled future). Record a health failure if
        // the stream saw an upstream error or a gateway-observed
        // truncation before the disconnect.
        self.record_health_failure_if_needed();
        if let Some(reason) = disconnect_reason {
            if let Ok(mut a) = self.accum.lock() {
                a.mark_truncated(reason);
            }
            let request_id = capture.request_id.clone();
            let bytes_received = capture
                .upstream_resp_body
                .as_deref()
                .map(str::len)
                .unwrap_or(0);
            tracing::warn!(
                request_id = %request_id,
                bytes_received,
                "downstream SSE client disconnected before stream completed"
            );
        }
        tokio::spawn(async move {
            telemetry.send_capture(capture).await;
        });
    }
}

/// Cross-protocol streaming re-encode plan for [`drive_upstream_stream`].
///
/// When the ingress entrypoint protocol differs from the egress (upstream
/// provider) protocol, the upstream SSE bytes cannot be forwarded verbatim —
/// the client expects its own protocol's wire format. This carries the IR
/// hub-spoke pair: the egress protocol's [`StreamDecoder`] (parses the
/// upstream SSE into canonical [`tiygate_core::StreamPart`]s) and the ingress
/// protocol's [`StreamEncoder`] (re-encodes those parts into the client's
/// native SSE frames). When `None`, the stream follows the same-protocol fast
/// path; its JSON `data:` fields may still receive the client-model override.
///
/// [`StreamDecoder`]: tiygate_core::StreamDecoder
/// [`StreamEncoder`]: tiygate_core::StreamEncoder
pub(super) struct StreamTranscode {
    /// Egress (upstream) protocol decoder: upstream SSE → IR stream parts.
    pub decoder: Box<dyn tiygate_core::StreamDecoder>,
    /// Ingress (client) protocol encoder: IR stream parts → client SSE.
    pub encoder: Box<dyn tiygate_core::StreamEncoder>,
}

/// Lightweight scan of verbatim upstream SSE bytes for error frames and
/// terminal signals.
///
/// In the same-protocol streaming path the gateway avoids protocol conversion.
/// This function performs a cheap detection pass for two things:
///
/// 1. **Error frames** — `data:` lines whose JSON payload contains a
///    top-level `"error"` key — the shape used by OpenAI, Anthropic, and
///    Google when embedding an error frame inside an HTTP 200 SSE stream
///    (e.g. `service_unavailable_error`, `overloaded_error`).
///
/// 2. **Terminal signals** — `data: [DONE]` or `data:` lines whose JSON
///    payload has a `"type"` field indicating a terminal event
///    (`response.completed`, `response.failed`, `response.incomplete`)
///    or an `event:` line naming a terminal event (`message_stop`).
///    When detected, `upstream_terminal` is set on the accumulator so
///    the gateway knows the upstream already closed the stream and must
///    not synthesize an additional end frame.
///
/// The scan is deliberately conservative: it only parses `data:` lines
/// that contain the byte substring `"error"` for error detection, and
/// only checks for terminal markers on lines that contain `[DONE]`,
/// `"type"`, or `event:`. Normal response chunks that merely mention
/// "error" in content text are not affected because their JSON will not
/// have a top-level `"error"` key.
fn detect_verbatim_signals(bytes: &[u8], accum: &Arc<std::sync::Mutex<UsageAccumulator>>) {
    // Quick gate: skip the scan entirely if neither `"error"` nor
    // `[DONE]` nor `"type"` nor `event:` is present. This avoids the
    // per-line loop for the common case of pure content deltas.
    let has_error = bytes.windows(7).any(|w| w == b"\"error\"");
    let has_done = bytes.windows(6).any(|w| w == b"[DONE]");
    let has_type = bytes.windows(6).any(|w| w == b"\"type\"");
    let has_event = bytes.windows(6).any(|w| w == b"event:");
    if !has_error && !has_done && !has_type && !has_event {
        return;
    }
    // Best-effort UTF-8 conversion for scanning SSE line prefixes.
    let text = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => &String::from_utf8_lossy(bytes),
    };
    for line in text.lines() {
        let trimmed = line.trim();

        // --- Terminal signal detection on `event:` lines ---
        // Anthropic uses `event: message_stop` as its terminal event.
        if let Some(ev) = trimmed.strip_prefix("event:") {
            let ev = ev.trim();
            if ev == "message_stop" {
                if let Ok(mut a) = accum.lock() {
                    a.set_upstream_terminal();
                }
            }
            // `event: error` is handled by the `data:` line that follows.
            continue;
        }

        // SSE data lines look like `data: { ... }` or `data:{ ... }`.
        let json_str = match trimmed.strip_prefix("data:") {
            Some(rest) => rest.trim(),
            None => continue,
        };

        // --- Terminal signal: `[DONE]` ---
        if json_str == "[DONE]" {
            if let Ok(mut a) = accum.lock() {
                a.set_upstream_terminal();
            }
            continue;
        }
        if json_str.is_empty() {
            continue;
        }

        // --- Terminal signal: JSON `type` field ---
        // Responses protocol: `response.completed`, `response.failed`,
        // `response.incomplete` are terminal events. We check for
        // `"type"` presence before JSON parsing to stay cheap.
        if has_type && json_str.contains("\"type\"") {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(t) = value["type"].as_str() {
                    if matches!(
                        t,
                        "response.completed"
                            | "response.failed"
                            | "response.incomplete"
                            | "message_stop"
                    ) {
                        if let Ok(mut a) = accum.lock() {
                            a.set_upstream_terminal();
                        }
                    }
                }
                // --- Error frame detection ---
                // Only treat as error when the parsed JSON has a
                // top-level `"error"` object or a nested
                // `response.error` (Responses protocol's
                // `response.failed` event). Normal chunks that
                // mention "error" in content text are not affected.
                if has_error && json_str.contains("\"error\"") {
                    let error = value
                        .get("error")
                        .or_else(|| value.get("response").and_then(|r| r.get("error")))
                        .filter(|e| e.is_object());
                    if let Some(error) = error {
                        let message = error["message"].as_str().unwrap_or("upstream stream error");
                        let code = error["type"].as_str().or_else(|| error["code"].as_str());
                        if let Ok(mut a) = accum.lock() {
                            a.set_upstream_error(message, code);
                        }
                    }
                }
            }
        } else if has_error && json_str.contains("\"error\"") {
            // --- Error frame detection (no `"type"` field) ---
            // Some providers embed `{"error": {...}}` without a `"type"`
            // wrapper. Parse only when `"error"` is present.
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(error) = value.get("error").filter(|e| e.is_object()) {
                    let message = error["message"].as_str().unwrap_or("upstream stream error");
                    let code = error["type"].as_str().or_else(|| error["code"].as_str());
                    if let Ok(mut a) = accum.lock() {
                        a.set_upstream_error(message, code);
                    }
                }
            }
        }
    }
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

#[allow(clippy::too_many_arguments, clippy::let_underscore_must_use)]
pub(super) fn drive_upstream_stream(
    _state: &AppState,
    accum: Arc<std::sync::Mutex<UsageAccumulator>>,
    mut upstream: UpstreamByteStream,
    // Protocol-native end frame (e.g. `data: [DONE]\n\n`). Emitted on a
    // clean upstream EOF **only** when the upstream delivered an error
    // frame but no terminal signal — this lets the client SDK close the
    // stream cleanly instead of timing out. Gateway-observed failures
    // (idle / total / mid-stream error) emit `error_marker` instead.
    end_marker: Vec<u8>,
    error_marker: Vec<u8>,
    idle_timeout: Duration,
    total_timeout: Duration,
    keepalive_interval: Duration,
    capture: Option<StreamCapture>,
    transcode: Option<StreamTranscode>,
    response_model: Option<ResponseModelOverride>,
) -> Response {
    use async_stream::stream;

    let total_budget_enabled = !total_timeout.is_zero();
    let capture_guard = StreamCaptureGuard::new(capture, accum.clone());
    // Streaming response-body accumulators for the request-log detail
    // view live in `capture_guard` instead of plain locals. The guard's
    // Drop path persists a best-effort capture when the downstream
    // client cancels and the async-stream body is dropped before the
    // normal tail code can run.
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
    // Same-suite streams otherwise bypass protocol codecs entirely. Keep the
    // normalization here, after either passthrough or transcoding, so every
    // client sees its requested virtual model without leaking target ids.
    let mut model_rewriter = response_model.map(|override_| override_.sse_rewriter());
    let mut frame_buf = String::new();
    // Transcode-only: tracks whether the upstream actually delivered a
    // genuine terminal signal in-band (a `Finish` or `ResponseCompleted`
    // IR part decoded from the upstream SSE). Some decoders (notably
    // Gemini) map the upstream's real terminator (`finishReason: STOP`)
    // to a bare `Finish` and only synthesize the `ResponseCompleted` the
    // ingress encoder needs (to emit `[DONE]`/`message_stop`) from
    // `decoder.finish()`. So on a clean upstream EOF we call
    // `decoder.finish()` to bridge that gap — but ONLY when a real
    // terminal signal was seen. If the stream was truncated before any
    // terminator arrived, we must NOT call finish() (it would fabricate a
    // success terminator the upstream never sent) and instead end at EOF.
    let mut saw_terminal = false;
    let log_truncation =
        |reason: TruncationReason, is_transcode: bool, guard: &StreamCaptureGuard| {
            let rid = guard.request_id();
            tracing::warn!(
                request_id = %rid,
                reason = reason.as_str(),
                is_transcode,
                "upstream SSE stream truncated by gateway"
            );
        };
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
                            // view. This is a single memory copy and
                            // never blocks the forward path.
                            capture_guard.append_upstream(&bytes);
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
                                                if matches!(
                                                    part,
                                                    tiygate_core::ir::StreamPart::Finish { .. }
                                                        | tiygate_core::ir::StreamPart::ResponseCompleted { .. }
                                                ) {
                                                    saw_terminal = true;
                                                }
                                                // Detect upstream error frames
                                                // embedded in an HTTP 200 SSE
                                                // stream (e.g.
                                                // service_unavailable_error).
                                                // Record in the accumulator so
                                                // the capture guard /
                                                // telemetry can mark the
                                                // request as failed.
                                                // An Error part is a terminal
                                                // signal in some protocols
                                                // (e.g. `response.failed`),
                                                // but we do NOT set
                                                // `saw_terminal` here because
                                                // `decoder.finish()` must
                                                // NOT be called on error
                                                // streams (some decoders
                                                // fabricate a success
                                                // terminator from buffered
                                                // state). Instead, the EOF
                                                // path uses `!saw_terminal`
                                                // to decide whether to
                                                // emit `encode_done()`,
                                                // matching the verbatim
                                                // path's behavior.
                                                if let tiygate_core::ir::StreamPart::Error {
                                                    message,
                                                    class,
                                                    upstream_code,
                                                } = part
                                                {
                                                    if let Ok(mut a) = accum.lock() {
                                                        a.set_upstream_error(message, upstream_code.as_deref());
                                                    }
                                                    // Use the class from the decoded error frame
                                                    let _ = class; // class is used via encode_part
                                                }
                                                match tc.encoder.encode_part(part) {
                                                    Ok(b) => out.extend_from_slice(&b),
                                                    Err(e) => {
                                                        let ef = tc.encoder.encode_error(
                                                            &format!("transcode encode error: {e}"),
                                                            tiygate_core::ErrorClass::LossyOrCapability, None,
                                                        );
                                                        out.extend_from_slice(&ef);
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            let ef = tc.encoder.encode_error(
                                                &format!("transcode decode error: {e}"),
                                                tiygate_core::ErrorClass::LossyOrCapability, None,
                                            );
                                            out.extend_from_slice(&ef);
                                        }
                                    }
                                }
                                if !out.is_empty() {
                                    let out = if let Some(rewriter) = model_rewriter.as_mut() {
                                        rewriter.rewrite(&out)
                                    } else {
                                        out
                                    };
                                    // Mirror the re-encoded bytes into the
                                    // client capture so cross-protocol streams
                                    // persist the ingress-format body in
                                    // `client_resp_body`, not the raw
                                    // upstream SSE.
                                    if !out.is_empty() {
                                        capture_guard.append_client(&out);
                                        yield Ok(Bytes::from(out));
                                    }
                                }
                            } else {
                                // Forward upstream bytes VERBATIM. The upstream
                                // chunk is already a complete SSE frame
                                // (`data: ...\n\n`); wrapping it in an axum
                                // `Event` would double-prefix `data:` and
                                // corrupt the stream. Pass the raw bytes.
                                //
                                // Lightweight error-frame scan: when the
                                // chunk contains a `data:` SSE line whose
                                // JSON payload has a top-level `"error"`
                                // key (e.g.
                                // `data: {"error":{"type":"service_unavailable_error",...}}`),
                                // record it in the accumulator so the
                                // capture guard / telemetry can mark the
                                // request as failed despite the HTTP 200
                                // status. The scan is cheap: a byte
                                // substring search for `"error"` gates the
                                // more expensive JSON parse.
                                detect_verbatim_signals(&bytes, &accum);
                                let client_bytes = if let Some(rewriter) = model_rewriter.as_mut() {
                                    rewriter.rewrite(&bytes)
                                } else {
                                    bytes.to_vec()
                                };
                                if !client_bytes.is_empty() {
                                    capture_guard.append_client(&client_bytes);
                                    yield Ok(Bytes::from(client_bytes));
                                }
                            }
                        }
                        Some(Err(error)) => {
                            capture_guard.set_reason(Some(TruncationReason::UpstreamError));
                            log_truncation(
                                TruncationReason::UpstreamError,
                                transcode.is_some(),
                                &capture_guard,
                            );
                            // Transport executors preserve the original error
                            // text when normalizing their byte stream. Logging
                            // it here covers both reqwest body failures and
                            // WebSocket close/read errors without coupling the
                            // canonical bridge to a concrete I/O client.
                            let rid = capture_guard.request_id();
                            tracing::warn!(
                                request_id = %rid,
                                error = %error,
                                bytes_received = capture_guard.upstream_len(),
                                "upstream stream errored mid-stream"
                            );
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
                            if let Some(rewriter) = model_rewriter.as_mut() {
                                let trailing = rewriter.finish();
                                if !trailing.is_empty() {
                                    capture_guard.append_client(&trailing);
                                    yield Ok(Bytes::from(trailing));
                                }
                            }
                            if let Some(tc) = transcode.as_mut() {
                                let ef = tc.encoder.encode_error(
                                    "upstream stream truncated by gateway",
                                    tiygate_core::ErrorClass::Transient, None,
                                );
                                if !ef.is_empty() {
                                    capture_guard.append_client(&ef);
                                    yield Ok(Bytes::from(ef));
                                }
                            } else if !error_marker.is_empty() {
                                // Verbatim mode: if the upstream was cut
                                // mid-frame the tail does not end on a frame
                                // boundary; prepend a blank line so the error
                                // frame is not glued onto a half-written
                                // `data:` line and corrupt the client parse.
                                if !tail_ends_on_frame_boundary(&tail_buf) {
                                    yield Ok(Bytes::from_static(b"\n\n"));
                                }
                                capture_guard.append_client(&error_marker);
                                yield Ok(Bytes::from(error_marker.clone()));
                            }
                            capture_guard.finalize_spawn();
                            break;
                        }
                        None => {
                            // Upstream closed naturally — emit the
                            // protocol-native end frame and finish.
                            capture_guard.set_reason(None);
                            // Mark as completed BEFORE yielding the last frame:
                            // if the client (SDK) closes immediately after
                            // receiving that frame, the `async_stream` future is
                            // cancelled at the `yield` suspension point and
                            // Drop's `upstream_completed` check prevents a
                            // false `client_disconnect`.
                            capture_guard.mark_upstream_completed();
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
                                            if matches!(
                                                part,
                                                tiygate_core::ir::StreamPart::Finish { .. }
                                                    | tiygate_core::ir::StreamPart::ResponseCompleted { .. }
                                            ) {
                                                saw_terminal = true;
                                            }
                                            if let Ok(b) = tc.encoder.encode_part(part) {
                                                out.extend_from_slice(&b);
                                            }
                                        }
                                    }
                                }
                                frame_buf.clear();
                                // Bridge `decoder.finish()` ONLY when the
                                // upstream actually delivered a genuine terminal
                                // signal in-band (`saw_terminal`). Some decoders
                                // (notably Gemini) map the real upstream
                                // terminator (`finishReason: STOP`) to a bare
                                // `Finish` and only synthesize the
                                // `ResponseCompleted` the ingress encoder needs
                                // (to emit `[DONE]`/`message_stop`) from
                                // `finish()`. When a terminator was seen we must
                                // call finish() so the client still gets its
                                // protocol-native end frame.
                                //
                                // When NO terminator was seen the stream was
                                // truncated: we must NOT call finish(), because
                                // some decoders fabricate a `ResponseCompleted`
                                // from buffered state (e.g. Gemini emits one
                                // whenever a `response_id` exists), which would
                                // turn a recoverable gap into a corrupt
                                // "successful" response. End at EOF instead so
                                // the client can detect the truncation and retry.
                                if saw_terminal {
                                    if let Ok(parts) = tc.decoder.finish() {
                                        for part in &parts {
                                            if let Ok(b) = tc.encoder.encode_part(part) {
                                                out.extend_from_slice(&b);
                                            }
                                        }
                                    }
                                }
                                // When the upstream sent an error frame
                                // but no terminal signal, the client SDK
                                // would hang waiting for its protocol-
                                // native end frame. Emit `encode_done()`
                                // (a pure terminator with no success
                                // semantics, e.g. `data: [DONE]`) so the
                                // SDK closes the stream cleanly.
                                // This matches the verbatim path: an
                                // error without a terminal signal still
                                // gets an end marker.
                                if !saw_terminal {
                                    let has_upstream_error = accum
                                        .lock()
                                        .map(|a| a.upstream_error.is_some())
                                        .unwrap_or(false);
                                    if has_upstream_error {
                                        let done = tc.encoder.encode_done();
                                        if !done.is_empty() {
                                            out.extend_from_slice(&done);
                                        }
                                    }
                                }
                                if !out.is_empty() {
                                    let out = if let Some(rewriter) = model_rewriter.as_mut() {
                                        rewriter.rewrite(&out)
                                    } else {
                                        out
                                    };
                                    if !out.is_empty() {
                                        capture_guard.append_client(&out);
                                        yield Ok(Bytes::from(out));
                                    }
                                }
                            } else {
                                // Verbatim / same-protocol path: the upstream
                                // closed cleanly. Do NOT synthesize an end
                                // frame (`data: [DONE]`, etc.). If the upstream
                                // sent its own terminator in-band it was already
                                // forwarded verbatim; if it did not, the gateway
                                // must not fabricate a success marker the
                                // upstream never produced. A fabricated
                                // terminator turns a recoverable "incomplete
                                // stream" (which clients detect on EOF and
                                // retry) into a corrupt "successful" response —
                                // especially when the upstream was cut mid-frame
                                // and the appended terminator would glue onto a
                                // half-written `data:` line. End at EOF and let
                                // the client decide.
                                //
                                // Exception: when the upstream delivered an
                                // error frame (detected via
                                // `detect_verbatim_signals`) but no terminal
                                // signal, emit the protocol-native end
                                // marker so the client SDK does not time out
                                // waiting for it. When a terminal signal
                                // was already sent (e.g. `response.failed`,
                                // `[DONE]`, `message_stop`), the stream is
                                // already closed and no end marker is needed.
                                let (has_upstream_error, has_terminal) = accum
                                    .lock()
                                    .map(|a| (a.upstream_error.is_some(), a.upstream_terminal))
                                    .unwrap_or((false, false));
                                if has_upstream_error && !has_terminal && !end_marker.is_empty() {
                                    capture_guard.append_client(&end_marker);
                                    yield Ok(Bytes::from(end_marker.clone()));
                                }
                            }
                            // A final upstream chunk may end halfway through
                            // an SSE line. Flush it only at EOF so model
                            // normalization never drops those bytes.
                            if let Some(rewriter) = model_rewriter.as_mut() {
                                let trailing = rewriter.finish();
                                if !trailing.is_empty() {
                                    capture_guard.append_client(&trailing);
                                    yield Ok(Bytes::from(trailing));
                                }
                            }
                            // Finalize capture BEFORE break: the client may
                            // close immediately after receiving the last frame
                            // (SDK reads [DONE] → response.close()), which
                            // cancels this future before `finalize().await`
                            // at the tail can run.
                            capture_guard.finalize_spawn();
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep_until(idle_deadline) => {
                    capture_guard.set_reason(Some(TruncationReason::Idle));
                    log_truncation(TruncationReason::Idle, transcode.is_some(), &capture_guard);
                    if let Ok(mut a) = accum.lock() {
                        a.mark_truncated(TruncationReason::Idle);
                    }
                    // Idle elapsed — this is a gateway-observed failure,
                    // not a natural end. Emit a protocol-native ERROR
                    // frame (never a success `[DONE]`) so the client can
                    // tell the stream did not complete and retry.
                    // Already-received bytes are still billable. In
                    // verbatim mode, if the upstream was cut mid-frame the
                    // tail does not end on a frame boundary; prepend a
                    // blank line so the error frame is not glued onto a
                    // half-written `data:` line.
                    if let Some(rewriter) = model_rewriter.as_mut() {
                        let trailing = rewriter.finish();
                        if !trailing.is_empty() {
                            capture_guard.append_client(&trailing);
                            yield Ok(Bytes::from(trailing));
                        }
                    }
                    if let Some(tc) = transcode.as_mut() {
                        let ef = tc.encoder.encode_error(
                            "upstream stream idle timeout",
                            tiygate_core::ErrorClass::DeadlineExceeded, None,
                        );
                        if !ef.is_empty() {
                            capture_guard.append_client(&ef);
                            yield Ok(Bytes::from(ef));
                        }
                    } else if !error_marker.is_empty() {
                        if !tail_ends_on_frame_boundary(&tail_buf) {
                            yield Ok(Bytes::from_static(b"\n\n"));
                        }
                        capture_guard.append_client(&error_marker);
                        yield Ok(Bytes::from(error_marker.clone()));
                    }
                    capture_guard.finalize_spawn();
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
                    capture_guard.set_reason(Some(TruncationReason::Total));
                    log_truncation(TruncationReason::Total, transcode.is_some(), &capture_guard);
                    if let Ok(mut a) = accum.lock() {
                        a.mark_truncated(TruncationReason::Total);
                    }
                    // Total budget elapsed. Emit the protocol-native
                    // error frame so the client can tell this was a
                    // gateway-side cap, not a natural end. In transcode
                    // mode the frame is built by the ingress encoder.
                    if let Some(rewriter) = model_rewriter.as_mut() {
                        let trailing = rewriter.finish();
                        if !trailing.is_empty() {
                            capture_guard.append_client(&trailing);
                            yield Ok(Bytes::from(trailing));
                        }
                    }
                    if let Some(tc) = transcode.as_mut() {
                        let ef = tc.encoder.encode_error(
                            "upstream stream exceeded gateway total budget",
                            tiygate_core::ErrorClass::DeadlineExceeded, None,
                        );
                        if !ef.is_empty() {
                            capture_guard.append_client(&ef);
                            yield Ok(Bytes::from(ef));
                        }
                    } else if !error_marker.is_empty() {
                        if !tail_ends_on_frame_boundary(&tail_buf) {
                            yield Ok(Bytes::from_static(b"\n\n"));
                        }
                        capture_guard.append_client(&error_marker);
                        yield Ok(Bytes::from(error_marker.clone()));
                    }
                    capture_guard.finalize_spawn();
                    break;
                }
            }
        }
        // Stream finished (natural end, idle, total, or upstream
        // error). Send the accumulated exchange capture to the
        // telemetry bus. If the downstream client cancels before this
        // point, `StreamCaptureGuard::drop` sends a best-effort capture
        // with `client_disconnect` instead.
        // Safety net: if we reach here without having hit a break
        // (shouldn't happen, but keeps the contract), finalize now.
        capture_guard.finalize_spawn();
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
    // Tell downstream reverse proxies (Nginx, Caddy, etc.) to disable
    // response buffering so SSE chunks are forwarded immediately.
    // Non-Nginx intermediaries and browsers ignore this header harmlessly.
    headers.insert(
        http::HeaderName::from_static("x-accel-buffering"),
        http::HeaderValue::from_static("no"),
    );
    response
}

/// Returns true if `tail` (the rolling window of the most recently
/// forwarded upstream bytes) ends on an SSE frame boundary, i.e. the
/// last bytes are a blank line (`\n\n` or `\r\n\r\n`) or the tail is
/// empty (nothing forwarded yet). When this returns false the upstream
/// was cut in the middle of a `data:` frame, so the gateway must emit a
/// blank line before appending its own error frame — otherwise the error
/// frame would be glued onto the half-written line and corrupt the
/// client's SSE parse.
fn tail_ends_on_frame_boundary(tail: &[u8]) -> bool {
    if tail.is_empty() {
        return true;
    }
    tail.ends_with(b"\n\n") || tail.ends_with(b"\r\n\r\n")
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use tiygate_core::{ExchangeCapture, PipelineEvent, RequestEvent, TelemetryBus};

    #[derive(Default)]
    struct CapturingBus {
        captures: Mutex<Vec<ExchangeCapture>>,
    }

    #[async_trait]
    impl TelemetryBus for CapturingBus {
        async fn send(&self, _event: PipelineEvent) {}

        async fn send_request_event(&self, _event: RequestEvent) {}

        async fn send_capture(&self, capture: ExchangeCapture) {
            self.captures.lock().unwrap().push(capture);
        }
    }

    fn sample_stream_capture(bus: Arc<CapturingBus>) -> StreamCapture {
        StreamCapture {
            request_id: "req-1".to_string(),
            telemetry: bus,
            egress_method: "POST".to_string(),
            egress_path: "/v1/chat/completions".to_string(),
            egress_headers: Vec::new(),
            egress_body: Some("{}".to_string()),
            upstream_status: Some(200),
            upstream_resp_headers: Vec::new(),
            client_resp_headers: Vec::new(),
            health: None,
            health_key: None,
        }
    }

    #[tokio::test]
    async fn stream_capture_guard_drop_sends_client_disconnect_capture() {
        let bus = Arc::new(CapturingBus::default());
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        {
            let guard =
                StreamCaptureGuard::new(Some(sample_stream_capture(bus.clone())), accum.clone());
            guard.append_upstream(b"data: partial\n\n");
            guard.append_client(b"data: partial\n\n");
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if bus.captures.lock().unwrap().len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drop capture send should complete");

        let captures = bus.captures.lock().unwrap();
        assert_eq!(captures.len(), 1);
        let capture = &captures[0];
        assert_eq!(capture.request_id, "req-1");
        assert_eq!(
            capture.truncation_reason.as_deref(),
            Some("client_disconnect")
        );
        assert_eq!(
            capture.upstream_resp_body.as_deref(),
            Some("data: partial\n\n")
        );
        assert_eq!(
            capture.client_resp_body.as_deref(),
            Some("data: partial\n\n")
        );
        assert!(capture.stream_duration_ms.is_some());
        drop(captures);

        let acc = accum.lock().unwrap();
        assert_eq!(acc.truncated, Some(TruncationReason::ClientDisconnect));
        assert!(!acc.completed);
    }

    #[tokio::test]
    async fn stream_capture_guard_finalize_sends_once_and_drop_is_noop() {
        let bus = Arc::new(CapturingBus::default());
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        {
            let guard =
                StreamCaptureGuard::new(Some(sample_stream_capture(bus.clone())), accum.clone());
            guard.append_upstream(b"data: done\n\n");
            guard.append_client(b"data: done\n\n");
            guard.finalize_spawn();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if bus.captures.lock().unwrap().len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("finalize_spawn should complete");

        let captures = bus.captures.lock().unwrap();
        assert_eq!(captures.len(), 1);
        let capture = &captures[0];
        assert_eq!(capture.truncation_reason, None);
        assert_eq!(
            capture.upstream_resp_body.as_deref(),
            Some("data: done\n\n")
        );
    }

    #[tokio::test]
    async fn stream_capture_guard_upstream_terminal_drop_is_clean() {
        let bus = Arc::new(CapturingBus::default());
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        {
            let guard =
                StreamCaptureGuard::new(Some(sample_stream_capture(bus.clone())), accum.clone());
            guard.append_upstream(b"data: [DONE]\n\n");
            guard.append_client(b"data: [DONE]\n\n");
            {
                let mut a = accum.lock().unwrap();
                a.set_upstream_terminal();
            }
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if bus.captures.lock().unwrap().len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drop capture send should complete");

        let captures = bus.captures.lock().unwrap();
        assert_eq!(captures.len(), 1);
        let capture = &captures[0];
        assert_eq!(capture.truncation_reason, None);
        drop(captures);

        let acc = accum.lock().unwrap();
        assert!(acc.truncated.is_none());
        assert!(acc.upstream_terminal);
    }

    #[tokio::test]
    async fn stream_capture_guard_upstream_error_drop_is_clean() {
        let bus = Arc::new(CapturingBus::default());
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        {
            let guard =
                StreamCaptureGuard::new(Some(sample_stream_capture(bus.clone())), accum.clone());
            guard.append_upstream(b"data: {\"error\":{\"type\":\"service_unavailable_error\",\"message\":\"Service unavailable\"}}\n\n");
            guard.append_client(b"data: {\"error\":{\"type\":\"service_unavailable_error\",\"message\":\"Service unavailable\"}}\n\n");
            {
                let mut a = accum.lock().unwrap();
                a.set_upstream_error("Service unavailable", Some("service_unavailable_error"));
            }
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if bus.captures.lock().unwrap().len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drop capture send should complete");

        let captures = bus.captures.lock().unwrap();
        assert_eq!(captures.len(), 1);
        let capture = &captures[0];
        assert_eq!(capture.truncation_reason, None);
        assert!(capture.upstream_error.as_deref().is_some());
        drop(captures);

        let acc = accum.lock().unwrap();
        assert!(acc.truncated.is_none());
        assert!(acc.upstream_error.is_some());
    }

    #[tokio::test]
    async fn stream_capture_guard_upstream_completed_drop_is_clean() {
        let bus = Arc::new(CapturingBus::default());
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        {
            let guard =
                StreamCaptureGuard::new(Some(sample_stream_capture(bus.clone())), accum.clone());
            guard.append_upstream(b"data: last\n\n");
            guard.append_client(b"data: last\n\n");
            // Simulate natural EOF: upstream delivered termination signal,
            // but the yield point was cancelled before finalize_spawn ran.
            guard.mark_upstream_completed();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if bus.captures.lock().unwrap().len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drop capture send should complete");

        let captures = bus.captures.lock().unwrap();
        assert_eq!(captures.len(), 1);
        let capture = &captures[0];
        // No truncation_reason — upstream completed, Drop should NOT
        // mark client_disconnect.
        assert_eq!(capture.truncation_reason, None);
        assert_eq!(
            capture.upstream_resp_body.as_deref(),
            Some("data: last\n\n")
        );
        drop(captures);

        // Accumulator should NOT be marked truncated.
        let acc = accum.lock().unwrap();
        assert!(acc.truncated.is_none());
    }

    // --- detect_verbatim_signals tests ---

    #[test]
    fn detect_verbatim_signals_openai_style_error() {
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bytes = b"data: {\"error\":{\"type\":\"service_unavailable_error\",\"message\":\"Service unavailable\"}}\n\n";
        detect_verbatim_signals(bytes, &accum);
        let acc = accum.lock().unwrap();
        let err = acc.upstream_error.as_ref().expect("error should be set");
        assert_eq!(err.message, "Service unavailable");
        assert_eq!(err.code.as_deref(), Some("service_unavailable_error"));
        // Error frame without a terminal `type` → no terminal signal.
        assert!(!acc.upstream_terminal);
    }

    #[test]
    fn detect_verbatim_signals_anthropic_style_error() {
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        // Anthropic uses event: error + data: {...}
        let bytes = b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        detect_verbatim_signals(bytes, &accum);
        let acc = accum.lock().unwrap();
        let err = acc.upstream_error.as_ref().expect("error should be set");
        assert_eq!(err.message, "Overloaded");
        assert_eq!(err.code.as_deref(), Some("overloaded_error"));
    }

    #[test]
    fn detect_verbatim_signals_normal_chunk_not_flagged() {
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        // A normal chunk that contains "error" in content text but not
        // as a top-level key.
        let bytes = b"data: {\"choices\":[{\"delta\":{\"content\":\"An error occurred\"}}]}\n\n";
        detect_verbatim_signals(bytes, &accum);
        let acc = accum.lock().unwrap();
        assert!(acc.upstream_error.is_none(), "should not flag normal chunk");
        assert!(!acc.upstream_terminal);
    }

    #[test]
    fn detect_verbatim_signals_no_error_key_skipped() {
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bytes = b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n";
        detect_verbatim_signals(bytes, &accum);
        let acc = accum.lock().unwrap();
        assert!(acc.upstream_error.is_none());
    }

    #[test]
    fn detect_verbatim_signals_first_error_wins() {
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bytes1 = b"data: {\"error\":{\"type\":\"service_unavailable_error\",\"message\":\"First error\"}}\n\n";
        let bytes2 =
            b"data: {\"error\":{\"type\":\"overloaded_error\",\"message\":\"Second error\"}}\n\n";
        detect_verbatim_signals(bytes1, &accum);
        detect_verbatim_signals(bytes2, &accum);
        let acc = accum.lock().unwrap();
        let err = acc.upstream_error.as_ref().expect("error should be set");
        assert_eq!(err.message, "First error");
    }

    #[test]
    fn detect_verbatim_signals_error_null_not_flagged() {
        // OpenAI Responses protocol frames include `"error":null` in
        // `response.created`, `response.in_progress`, and
        // `response.completed` events. These must NOT be treated as
        // upstream errors — only a non-null error object should.
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bytes = b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"status\":\"in_progress\",\"error\":null}}\n\n";
        detect_verbatim_signals(bytes, &accum);
        let acc = accum.lock().unwrap();
        assert!(
            acc.upstream_error.is_none(),
            "\"error\":null must not be treated as an error"
        );
    }

    #[test]
    fn detect_verbatim_signals_response_completed_with_error_null_not_flagged() {
        // The `response.completed` event from OpenAI Responses includes
        // `"error":null` alongside usage data. This was the exact frame
        // shape that caused the false-positive failure.
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bytes = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\",\"error\":null,\"usage\":{\"input_tokens\":12,\"output_tokens\":12,\"total_tokens\":24}}}\n\n";
        detect_verbatim_signals(bytes, &accum);
        let acc = accum.lock().unwrap();
        assert!(
            acc.upstream_terminal,
            "response.completed must set terminal"
        );
        assert!(
            acc.upstream_error.is_none(),
            "\"error\":null in response.completed must not be treated as an error"
        );
    }

    #[test]
    fn detect_verbatim_signals_bare_error_null_not_flagged() {
        // A frame without a `\"type\"` field but with `"error":null`
        // (no-type error detection path) must also not be flagged.
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bytes = b"data: {\"error\":null}\n\n";
        detect_verbatim_signals(bytes, &accum);
        let acc = accum.lock().unwrap();
        assert!(
            acc.upstream_error.is_none(),
            "bare \"error\":null must not be treated as an error"
        );
    }

    // --- terminal signal detection tests ---

    #[test]
    fn detect_verbatim_signals_done_marker_sets_terminal() {
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bytes = b"data: [DONE]\n\n";
        detect_verbatim_signals(bytes, &accum);
        let acc = accum.lock().unwrap();
        assert!(acc.upstream_terminal, "[DONE] must set upstream_terminal");
        assert!(acc.upstream_error.is_none());
    }

    #[test]
    fn detect_verbatim_signals_response_failed_sets_terminal() {
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bytes = b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"boom\",\"type\":\"server_error\"}}}\n\n";
        detect_verbatim_signals(bytes, &accum);
        let acc = accum.lock().unwrap();
        assert!(
            acc.upstream_terminal,
            "response.failed must set upstream_terminal"
        );
        // Error should also be recorded since `\"error\"` is present.
        assert!(acc.upstream_error.is_some());
    }

    #[test]
    fn detect_verbatim_signals_response_completed_sets_terminal() {
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bytes = b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\"}}\n\n";
        detect_verbatim_signals(bytes, &accum);
        let acc = accum.lock().unwrap();
        assert!(
            acc.upstream_terminal,
            "response.completed must set upstream_terminal"
        );
        assert!(acc.upstream_error.is_none());
    }

    #[test]
    fn detect_verbatim_signals_response_incomplete_sets_terminal() {
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bytes = b"data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"r1\",\"status\":\"incomplete\"}}\n\n";
        detect_verbatim_signals(bytes, &accum);
        let acc = accum.lock().unwrap();
        assert!(
            acc.upstream_terminal,
            "response.incomplete must set upstream_terminal"
        );
    }

    #[test]
    fn detect_verbatim_signals_anthropic_message_stop_sets_terminal() {
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bytes = b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        detect_verbatim_signals(bytes, &accum);
        let acc = accum.lock().unwrap();
        assert!(
            acc.upstream_terminal,
            "message_stop event must set upstream_terminal"
        );
    }

    #[test]
    fn detect_verbatim_signals_non_terminal_type_not_flagged() {
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bytes = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n";
        detect_verbatim_signals(bytes, &accum);
        let acc = accum.lock().unwrap();
        assert!(
            !acc.upstream_terminal,
            "non-terminal type must not set upstream_terminal"
        );
    }

    #[test]
    fn detect_verbatim_signals_error_without_terminal_no_terminal_flag() {
        // An error frame WITHOUT a terminal `type` (e.g. bare `{"error":...}}`)
        // must set upstream_error but NOT upstream_terminal — the gateway
        // still needs to synthesize an end frame.
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bytes = b"data: {\"error\":{\"type\":\"server_error\",\"message\":\"boom\"}}\n\n";
        detect_verbatim_signals(bytes, &accum);
        let acc = accum.lock().unwrap();
        assert!(acc.upstream_error.is_some(), "error should be set");
        assert!(
            !acc.upstream_terminal,
            "bare error without terminal type must not set upstream_terminal"
        );
    }

    #[tokio::test]
    async fn stream_capture_guard_propagates_upstream_error_to_capture() {
        let bus = Arc::new(CapturingBus::default());
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        {
            // Simulate detecting an error frame during streaming
            {
                let mut a = accum.lock().unwrap();
                a.set_upstream_error("Service unavailable", Some("service_unavailable_error"));
            }
            let guard =
                StreamCaptureGuard::new(Some(sample_stream_capture(bus.clone())), accum.clone());
            guard.append_upstream(b"data: {\"error\":{...}}\n\n");
            guard.append_client(b"data: {\"error\":{...}}\n\n");
            guard.mark_upstream_completed();
            guard.finalize_spawn();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if bus.captures.lock().unwrap().len() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("finalize_spawn should complete");

        let captures = bus.captures.lock().unwrap();
        assert_eq!(captures.len(), 1);
        let capture = &captures[0];
        assert!(
            capture.upstream_error.is_some(),
            "upstream_error should be propagated"
        );
        assert!(capture
            .upstream_error
            .as_ref()
            .unwrap()
            .contains("Service unavailable"));
        assert!(capture
            .upstream_error
            .as_ref()
            .unwrap()
            .contains("service_unavailable_error"));
    }

    #[tokio::test]
    async fn record_health_failure_on_upstream_error() {
        let health = Arc::new(tiygate_core::HealthRegistry::new(
            1,
            vec![Duration::from_secs(60)],
        ));
        let key = "provider:model".to_string();
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        {
            let mut a = accum.lock().unwrap();
            a.set_upstream_error("Service unavailable", Some("service_unavailable_error"));
        }
        let bus = Arc::new(CapturingBus::default());
        let mut capture = sample_stream_capture(bus);
        capture.health = Some(health.clone());
        capture.health_key = Some(key.clone());
        let guard = StreamCaptureGuard::new(Some(capture), accum.clone());
        guard.finalize_spawn();
        // After an upstream error, the target should be unhealthy
        // (threshold = 1).
        assert!(
            !health.is_healthy(&key),
            "health should record failure after upstream error"
        );
    }

    #[tokio::test]
    async fn no_health_failure_on_clean_stream() {
        let health = Arc::new(tiygate_core::HealthRegistry::new(
            1,
            vec![Duration::from_secs(60)],
        ));
        let key = "provider:model".to_string();
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bus = Arc::new(CapturingBus::default());
        let mut capture = sample_stream_capture(bus);
        capture.health = Some(health.clone());
        capture.health_key = Some(key.clone());
        let guard = StreamCaptureGuard::new(Some(capture), accum.clone());
        guard.mark_upstream_completed();
        guard.finalize_spawn();
        assert!(
            health.is_healthy(&key),
            "health should remain healthy on clean stream"
        );
    }

    #[tokio::test]
    async fn record_health_failure_on_truncation() {
        let health = Arc::new(tiygate_core::HealthRegistry::new(
            1,
            vec![Duration::from_secs(60)],
        ));
        let key = "provider:model".to_string();
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bus = Arc::new(CapturingBus::default());
        let mut capture = sample_stream_capture(bus);
        capture.health = Some(health.clone());
        capture.health_key = Some(key.clone());
        let guard = StreamCaptureGuard::new(Some(capture), accum.clone());
        guard.set_reason(Some(TruncationReason::Idle));
        guard.finalize_spawn();
        assert!(
            !health.is_healthy(&key),
            "health should record failure after idle truncation"
        );
    }

    #[tokio::test]
    async fn no_health_failure_on_client_disconnect_only() {
        let health = Arc::new(tiygate_core::HealthRegistry::new(
            1,
            vec![Duration::from_secs(60)],
        ));
        let key = "provider:model".to_string();
        let accum = Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let bus = Arc::new(CapturingBus::default());
        let mut capture = sample_stream_capture(bus);
        capture.health = Some(health.clone());
        capture.health_key = Some(key.clone());
        // Simulate client disconnect without upstream error or gateway
        // truncation — Drop path sets ClientDisconnect, which is NOT
        // a failure reason for health.
        drop(StreamCaptureGuard::new(Some(capture), accum));
        assert!(
            health.is_healthy(&key),
            "client disconnect alone should not trigger health failure"
        );
    }
}
