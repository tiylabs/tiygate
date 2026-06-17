//! SSE streaming helpers — keepalive wrapper, upstream stream driver,
//! and cross-protocol transcode support.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::{Future, Stream, StreamExt};
use pin_project::pin_project;

use tiygate_core::{TruncationReason, UsageAccumulator};

use super::AppState;
// ---------------------------------------------------------------------------
// Streaming helper types
// ---------------------------------------------------------------------------

/// Default keepalive cadence for downstream SSE proxies. Cheap to send
/// (`:keepalive\n\n` is a single SSE comment line) and short enough to
/// keep corporate proxies from killing the connection on idle.
pub(super) const DEFAULT_SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

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
pub(super) struct StreamTranscode {
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
pub(super) fn drive_upstream_stream(
    _state: &AppState,
    accum: Arc<std::sync::Mutex<UsageAccumulator>>,
    response: reqwest::Response,
    // Retained for API/call-site compatibility, but the gateway no longer
    // synthesizes a success terminator: a clean upstream EOF ends the
    // client stream at EOF (no fabricated `[DONE]`), and gateway-observed
    // failures emit `error_marker` instead. See the `None`/idle/error
    // branches below for the rationale.
    _end_marker: Vec<u8>,
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
    // view. `capture_buf` always records the raw upstream SSE bytes, so
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
                            capture_buf.extend_from_slice(&bytes);
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
                                    client_capture_buf.extend_from_slice(&out);
                                    yield Ok(Bytes::from(out));
                                }
                            } else {
                                // Forward upstream bytes VERBATIM. The upstream
                                // chunk is already a complete SSE frame
                                // (`data: ...\n\n`); wrapping it in an axum
                                // `Event` would double-prefix `data:` and
                                // corrupt the stream. Pass the raw bytes.
                                client_capture_buf.extend_from_slice(&bytes);
                                yield Ok(bytes);
                            }
                        }
                        Some(Err(_e)) => {
                            last_reason = Some(TruncationReason::UpstreamError);
                            // Log the underlying reqwest/hyper error with
                            // its full source chain. This is the single
                            // point where the real reason for a mid-stream
                            // truncation (connection reset, incomplete
                            // message body, h2 GOAWAY, decode error, …) is
                            // observable — without it the gateway only
                            // knows "the stream ended early" but not why,
                            // which makes "works direct, fails via gateway"
                            // bugs impossible to diagnose.
                            {
                                let mut detail = format!("{_e}");
                                let mut src = std::error::Error::source(&_e);
                                while let Some(s) = src {
                                    detail.push_str(" -> ");
                                    detail.push_str(&s.to_string());
                                    src = s.source();
                                }
                                let rid = capture
                                    .as_ref()
                                    .map(|c| c.request_id.as_str())
                                    .unwrap_or("");
                                tracing::warn!(
                                    request_id = %rid,
                                    error = %detail,
                                    is_timeout = _e.is_timeout(),
                                    is_body = _e.is_body(),
                                    is_decode = _e.is_decode(),
                                    bytes_received = capture_buf.len(),
                                    "upstream SSE stream errored mid-stream"
                                );
                            }
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
                                    client_capture_buf.extend_from_slice(&ef);
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
                                client_capture_buf
                                    .extend_from_slice(&error_marker);
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
                                if !out.is_empty() {
                                    client_capture_buf.extend_from_slice(&out);
                                    yield Ok(Bytes::from(out));
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
                    // Idle elapsed — this is a gateway-observed failure,
                    // not a natural end. Emit a protocol-native ERROR
                    // frame (never a success `[DONE]`) so the client can
                    // tell the stream did not complete and retry.
                    // Already-received bytes are still billable. In
                    // verbatim mode, if the upstream was cut mid-frame the
                    // tail does not end on a frame boundary; prepend a
                    // blank line so the error frame is not glued onto a
                    // half-written `data:` line.
                    if let Some(tc) = transcode.as_mut() {
                        let ef = tc.encoder.encode_error(
                            "upstream stream idle timeout",
                            Some("upstream_timeout"),
                        );
                        if !ef.is_empty() {
                            client_capture_buf.extend_from_slice(&ef);
                            yield Ok(Bytes::from(ef));
                        }
                    } else if !error_marker.is_empty() {
                        if !tail_ends_on_frame_boundary(&tail_buf) {
                            yield Ok(Bytes::from_static(b"\n\n"));
                        }
                        client_capture_buf.extend_from_slice(&error_marker);
                        yield Ok(Bytes::from(error_marker.clone()));
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
                            client_capture_buf.extend_from_slice(&ef);
                            yield Ok(Bytes::from(ef));
                        }
                    } else if !error_marker.is_empty() {
                        if !tail_ends_on_frame_boundary(&tail_buf) {
                            yield Ok(Bytes::from_static(b"\n\n"));
                        }
                        client_capture_buf.extend_from_slice(&error_marker);
                        yield Ok(Bytes::from(error_marker.clone()));
                    }
                    break;
                }
            }
        }
        // Report any gateway-side truncation. `last_reason` is captured
        // by the async-stream macro; previously it was discarded, which
        // hid mid-stream truncations (status stays 200) from logs.
        if let Some(reason) = last_reason {
            let rid = capture.as_ref().map(|c| c.request_id.as_str()).unwrap_or("");
            tracing::warn!(
                request_id = %rid,
                reason = reason.as_str(),
                is_transcode = transcode.is_some(),
                "upstream SSE stream truncated by gateway"
            );
        }
        // `total_started` was captured at the entry of
        // `drive_upstream_stream`, i.e. the moment the upstream
        // response object was handed to us (right after `client.execute()`
        // resolved — the response headers have arrived). This is the
        // correct start point for `stream_duration_ms`: it covers the
        // full wall-clock time from upstream response-header arrival to
        // stream EOF / error / timeout, including the TTFB gap where the
        // upstream is "thinking" before emitting the first SSE chunk.
        // Using a per-poll `Instant` would miss that gap and understate
        // the duration for tool_calls / short responses where TTFB
        // dominates.
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
            let stream_duration_ms =
                Some(total_started.elapsed().as_millis() as u64);
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
                    truncation_reason: last_reason.map(|r| r.as_str().to_string()),
                    stream_duration_ms,
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
