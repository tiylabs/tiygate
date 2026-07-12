//! Upstream executors and codec/URL builders for each protocol.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures::{SinkExt, Stream, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use tiygate_core::provider::oauth::UpstreamTransport;
use tiygate_core::tracing_ctx::TraceContext;
use tiygate_core::{EndpointCodec, IrRequest, UsageAccumulator};
use tiygate_protocols::chat_completions::ChatCompletionsCodec;
use tiygate_protocols::embeddings::EmbeddingsCodec;
use tiygate_protocols::gemini::GeminiCodec;
use tiygate_protocols::images::ImagesGenerationsCodec;
use tiygate_protocols::messages::MessagesCodec;
use tiygate_protocols::responses::ResponsesCodec;

use super::headers::{
    extract_rate_limit_headers, extract_retry_after, forward_upstream_resp_headers,
    forwarded_resp_headers_for_capture, header_map_to_vec, maybe_inject_prompt_cache_key,
    merge_client_headers, normalize_openai_reasoning_for_target, override_model_in_body,
    reqwest_headers_to_vec, spawn_capture,
};
use super::response_model::ResponseModelOverride;
use super::streaming::{
    drive_upstream_stream, StreamCapture, StreamTranscode, UpstreamByteStream,
    DEFAULT_SSE_KEEPALIVE_INTERVAL,
};
use super::{apply_provider_auth, AppError, AppState};

/// Non-streaming timeout for image generation/edit requests. Image
/// generation is significantly slower than text chat (typically 10–60s
/// upstream), so we use a dedicated budget that is independent of the
/// global `request_read_timeout` (which defaults to 30s for chat).
const IMAGES_NONSTREAM_TIMEOUT: Duration = Duration::from_secs(300);

/// Required by the Codex Responses WebSocket endpoint. This is a transport
/// negotiation header, not a client-supplied preference, so it is injected
/// after header forwarding and OAuth auth have run.
const CODEX_RESPONSES_WEBSOCKET_BETA: &str = "responses_websockets=2026-02-06";
const CODEX_WEBSOCKET_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const CODEX_WEBSOCKET_EVENT_BUFFER: usize = 16;

type CodexWebSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Receiver half of a Codex WebSocket bridge. Dropping the downstream HTTP
/// body drops this stream too, which signals the worker that owns the socket
/// to perform a graceful WebSocket close instead of leaving upstream work to
/// run until a TCP reset or idle timeout.
struct CancelableCodexWebSocketStream {
    receiver: mpsc::Receiver<Result<Bytes, String>>,
    cancel: Option<oneshot::Sender<()>>,
}

impl Stream for CancelableCodexWebSocketStream {
    type Item = Result<Bytes, String>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}

impl Drop for CancelableCodexWebSocketStream {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

fn uses_codex_responses_websocket(target: &tiygate_core::RoutingTarget) -> bool {
    matches!(
        target.oauth.as_ref().map(|oauth| oauth.upstream_transport),
        Some(UpstreamTransport::CodexResponsesWebSocket)
    )
}

fn is_codex_oauth_target(target: &tiygate_core::RoutingTarget) -> bool {
    target.oauth.as_ref().is_some_and(|oauth| {
        oauth.client_id == "app_EMoamEEZ73f0CkXaXp7hrann"
            || target
                .effective_api_base()
                .contains("chatgpt.com/backend-api/codex")
    })
}

fn normalize_codex_request(body: &mut Value, websocket: bool) -> bool {
    let Some(object) = body.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    if object.get("stream").and_then(Value::as_bool) != Some(true) {
        object.insert("stream".to_string(), json!(true));
        changed = true;
    }
    if object.get("instructions").is_none_or(Value::is_null) {
        object.insert("instructions".to_string(), json!(""));
        changed = true;
    }
    for field in ["prompt_cache_retention", "safety_identifier"] {
        changed |= object.remove(field).is_some();
    }
    if !websocket {
        for field in ["previous_response_id", "stream_options"] {
            changed |= object.remove(field).is_some();
        }
    }
    changed
}

fn ensure_codex_request_headers(headers: &mut http::HeaderMap) {
    let has_session = ["session_id", "session-id"]
        .iter()
        .any(|name| headers.contains_key(*name));
    let desktop_user_agent = headers
        .get(http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("Mac OS"));
    if desktop_user_agent && !has_session {
        if let Ok(value) = http::HeaderValue::from_str(&uuid::Uuid::now_v7().to_string()) {
            headers.insert(http::HeaderName::from_static("session_id"), value);
        }
    }
}

fn parse_codex_http_response(body: &str) -> Result<Value, AppError> {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        return Ok(value);
    }
    let mut terminal_error = None;
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("response.completed" | "response.done") => {
                if let Some(response) = event.get("response") {
                    return Ok(response.clone());
                }
            }
            Some("response.failed" | "response.incomplete" | "error") => {
                terminal_error = Some(event);
            }
            _ => {}
        }
    }
    if let Some(error) = terminal_error {
        return Err(AppError::new(
            StatusCode::BAD_GATEWAY,
            format!("Codex terminal error: {error}"),
        ));
    }
    Err(AppError::new(
        StatusCode::BAD_GATEWAY,
        "Codex response did not contain response.completed".to_string(),
    ))
}

fn codex_websocket_url(upstream_url: &str) -> Result<String, AppError> {
    let mut url = url::Url::parse(upstream_url).map_err(|error| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("invalid Codex upstream URL: {error}"),
        )
    })?;
    match url.scheme() {
        "https" => url.set_scheme("wss").map_err(|_| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to convert Codex HTTPS URL to WSS".to_string(),
            )
        })?,
        "http" => url.set_scheme("ws").map_err(|_| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to convert Codex HTTP URL to WS".to_string(),
            )
        })?,
        "ws" | "wss" => {}
        scheme => {
            return Err(AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("unsupported Codex WebSocket URL scheme: {scheme}"),
            ));
        }
    }
    Ok(url.into())
}

fn codex_response_create(mut body: Value) -> Result<Value, AppError> {
    let object = body.as_object_mut().ok_or_else(|| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Codex Responses request body must be a JSON object".to_string(),
        )
    })?;
    // The WebSocket endpoint uses an event envelope instead of an HTTP body.
    // Always overwrite a forwarded value: clients may not select arbitrary
    // command types through the gateway.
    object.insert("type".to_string(), json!("response.create"));
    Ok(body)
}

fn codex_websocket_handshake_request(
    websocket_url: &str,
    headers: &http::HeaderMap,
) -> Result<http::Request<()>, AppError> {
    // Start from tungstenite's client request builder so the mandatory
    // Upgrade / Connection / Sec-WebSocket-* headers are present. A plain
    // `http::Request::builder()` looks valid but fails the handshake because
    // it omits the generated Sec-WebSocket-Key.
    let mut request = websocket_url.into_client_request().map_err(|error| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("build Codex WebSocket handshake request: {error}"),
        )
    })?;
    for (name, value) in headers {
        request.headers_mut().insert(name.clone(), value.clone());
    }
    Ok(request)
}

/// Normalize terminal variants emitted by Codex's WebSocket surface.
///
/// `response.done` is wire-compatible with `response.completed`, but the
/// standard Responses SSE decoder only understands the latter. Codex also
/// uses a top-level `error` event as a terminal event. A WebSocket session can
/// remain open after any of these, while a gateway HTTP request cannot.
fn normalize_codex_websocket_event(text: String) -> (String, bool) {
    let Ok(mut event) = serde_json::from_str::<Value>(&text) else {
        return (text, false);
    };
    let Some(event_type) = event.get("type").and_then(Value::as_str) else {
        return (text, false);
    };
    if event_type == "response.done" {
        event["type"] = json!("response.completed");
        let normalized = serde_json::to_string(&event).unwrap_or(text);
        return (normalized, true);
    }
    let terminal = matches!(
        event_type,
        "response.completed" | "response.failed" | "response.incomplete" | "error"
    );
    (text, terminal)
}

async fn close_codex_websocket(socket: &mut CodexWebSocket, reason: &'static str) {
    match tokio::time::timeout(CODEX_WEBSOCKET_CLOSE_TIMEOUT, socket.close(None)).await {
        Ok(Ok(())) => {
            tracing::debug!(reason, "Codex WebSocket closed");
        }
        Ok(Err(error)) => {
            tracing::debug!(error = %error, reason, "Codex WebSocket close failed");
        }
        Err(_) => {
            tracing::debug!(reason, "Codex WebSocket close timed out");
        }
    }
}

async fn send_codex_websocket_event(
    sender: &mpsc::Sender<Result<Bytes, String>>,
    cancel: &mut oneshot::Receiver<()>,
    event: Result<Bytes, String>,
) -> bool {
    tokio::select! {
        result = sender.send(event) => result.is_ok(),
        _ = cancel => false,
    }
}

fn websocket_event_stream(socket: CodexWebSocket) -> UpstreamByteStream {
    let (sender, receiver) = mpsc::channel(CODEX_WEBSOCKET_EVENT_BUFFER);
    let (cancel_sender, mut cancel_receiver) = oneshot::channel();

    tokio::spawn(async move {
        let mut socket = socket;
        loop {
            let frame = tokio::select! {
                _ = &mut cancel_receiver => {
                    close_codex_websocket(&mut socket, "downstream_cancelled").await;
                    return;
                }
                frame = socket.next() => frame,
            };
            match frame {
                Some(Ok(Message::Text(text))) => {
                    // Codex sends one Responses event per text message. The
                    // existing stream bridge consumes SSE, so retain the
                    // event JSON verbatim and add only the SSE envelope.
                    let (text, terminal) = normalize_codex_websocket_event(text.to_string());
                    if !send_codex_websocket_event(
                        &sender,
                        &mut cancel_receiver,
                        Ok(Bytes::from(format!("data: {text}\n\n"))),
                    )
                    .await
                    {
                        close_codex_websocket(&mut socket, "downstream_cancelled").await;
                        return;
                    }
                    if terminal {
                        close_codex_websocket(&mut socket, "terminal_event").await;
                        return;
                    }
                }
                Some(Ok(Message::Binary(bytes))) => match String::from_utf8(bytes.to_vec()) {
                    Ok(text) => {
                        let (text, terminal) = normalize_codex_websocket_event(text);
                        if !send_codex_websocket_event(
                            &sender,
                            &mut cancel_receiver,
                            Ok(Bytes::from(format!("data: {text}\n\n"))),
                        )
                        .await
                        {
                            close_codex_websocket(&mut socket, "downstream_cancelled").await;
                            return;
                        }
                        if terminal {
                            close_codex_websocket(&mut socket, "terminal_event").await;
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = send_codex_websocket_event(
                            &sender,
                            &mut cancel_receiver,
                            Err(format!("Codex WebSocket sent non-UTF-8 event: {error}")),
                        )
                        .await;
                        return;
                    }
                },
                Some(Ok(Message::Ping(payload))) => {
                    if let Err(error) = socket.send(Message::Pong(payload)).await {
                        let _ = send_codex_websocket_event(
                            &sender,
                            &mut cancel_receiver,
                            Err(format!("failed to send Codex WebSocket pong: {error}")),
                        )
                        .await;
                        return;
                    }
                }
                Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                Some(Ok(Message::Close(_))) | None => return,
                Some(Err(error)) => {
                    let _ = send_codex_websocket_event(
                        &sender,
                        &mut cancel_receiver,
                        Err(format!("Codex WebSocket read error: {error}")),
                    )
                    .await;
                    return;
                }
            }
        }
    });

    Box::pin(CancelableCodexWebSocketStream {
        receiver,
        cancel: Some(cancel_sender),
    })
}

async fn collect_codex_websocket_response(socket: &mut CodexWebSocket) -> Result<Value, String> {
    while let Some(frame) = socket.next().await {
        let text = match frame {
            Ok(Message::Text(text)) => text.to_string(),
            Ok(Message::Binary(bytes)) => String::from_utf8(bytes.to_vec())
                .map_err(|error| format!("Codex WebSocket sent non-UTF-8 event: {error}"))?,
            Ok(Message::Ping(payload)) => {
                socket
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|error| format!("failed to send Codex WebSocket pong: {error}"))?;
                continue;
            }
            Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => continue,
            Ok(Message::Close(_)) => {
                return Err("Codex WebSocket closed before response.completed".to_string());
            }
            Err(error) => return Err(format!("Codex WebSocket read error: {error}")),
        };
        let event: Value = serde_json::from_str(&text)
            .map_err(|error| format!("invalid Codex WebSocket event JSON: {error}"))?;
        match event.get("type").and_then(Value::as_str) {
            Some("response.completed") => {
                return event
                    .get("response")
                    .cloned()
                    .ok_or_else(|| "Codex response.completed lacks response payload".to_string());
            }
            Some("response.failed") | Some("response.incomplete") => {
                let message = event
                    .pointer("/response/error/message")
                    .or_else(|| event.pointer("/response/incomplete_details/reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("Codex response did not complete");
                return Err(message.to_string());
            }
            Some(_) | None => {}
        }
    }
    Err("Codex WebSocket reached EOF before response.completed".to_string())
}

/// Execute a reqwest request with an optional TTFB (time-to-first-byte)
/// timeout. When `ttfb_timeout_secs` is non-zero, the `client.execute()`
/// call is wrapped in `tokio::time::timeout` so a non-responsive upstream
/// (no response headers within the window) is bounded independently of
/// the streaming idle timer. When zero, the timeout is disabled and the
/// call behaves as a plain `client.execute().await`.
///
/// This is used **only** in streaming branches — non-streaming branches
/// already set `.timeout()` on the `RequestBuilder` which covers the
/// entire request lifecycle.
async fn execute_with_ttfb_timeout(
    client: &reqwest::Client,
    request: reqwest::Request,
    ttfb_timeout_secs: u64,
) -> Result<reqwest::Response, AppError> {
    if ttfb_timeout_secs > 0 {
        match tokio::time::timeout(
            Duration::from_secs(ttfb_timeout_secs),
            client.execute(request),
        )
        .await
        {
            Ok(result) => result.map_err(|e| {
                AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}"))
            }),
            Err(_) => {
                let mut err = AppError::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "upstream TTFB timeout".to_string(),
                )
                .with_class(tiygate_core::ErrorClass::DeadlineExceeded);
                // Set upstream_status=504 so the fallback classifier's
                // `classify_structured` path maps 504 → DeadlineExceeded
                // (rather than falling through to `classify_error` which
                // does not recognize timeout messages and would default
                // to Transient).
                err.upstream_status = Some(504);
                Err(err)
            }
        }
    } else {
        client
            .execute(request)
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")))
    }
}

/// Check whether an HTTP 200 non-streaming response body is actually
/// an error response (top-level `"error"` key). Some providers return
/// HTTP 200 with `{"error": {...}}` instead of a proper non-2xx status
/// code — e.g. `service_unavailable_error`, `overloaded_error`. When
/// detected, this returns an `AppError` so the fallback loop can
/// retry / try the next target, instead of silently passing the error
/// body to the client as a success.
///
/// Only triggers when the top-level JSON object has an `"error"` key
/// and does NOT simultaneously contain normal response fields
/// (`choices`, `candidates`, `output`, `data`, etc.) that would
/// indicate a mixed/success response. This avoids false positives on
/// responses that merely mention "error" in metadata.
fn check_nonstream_error_body(
    response_body: &Value,
    status: u16,
    retry_after: Option<String>,
    rate_limit_headers: Vec<(&'static str, String)>,
) -> Option<AppError> {
    let error = response_body.get("error")?;
    // Guard against false positives: if the body also contains
    // normal response fields, it's not a pure error response.
    let has_normal_field = ["choices", "candidates", "output", "data", "messages"]
        .iter()
        .any(|k| response_body.get(k).is_some());
    if has_normal_field {
        return None;
    }
    let message = error["message"]
        .as_str()
        .unwrap_or("upstream returned error in 200 response body");
    let code = error["code"].as_str().or_else(|| error["type"].as_str());
    let mut app_err = AppError::new(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        format!("Upstream error: {}", message),
    );
    app_err.upstream_status = Some(status);
    let class = tiygate_core::classify_upstream_error(Some(status), code);
    app_err = app_err.with_class(class);
    if let Some(c) = code {
        app_err = app_err.with_upstream_code(c);
    }
    if let Some(ra) = retry_after {
        app_err = app_err.with_retry_after_header(ra);
    }
    app_err.rate_limit_headers = rate_limit_headers;
    Some(app_err)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_upstream(
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
    api_key_id: &str,
) -> Result<(Response, Option<u64>), AppError> {
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
                Ok(v) => {
                    // Obtain protocol-specific headers from the codec
                    // (e.g. anthropic-version). We reuse encode_request's
                    // header map but keep the raw body unchanged.
                    let headers = codec
                        .encode_request(ir_request)
                        .map(|(_, h)| h)
                        .unwrap_or_default();
                    (v, headers)
                }
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

    // Inject `prompt_cache_key` for OpenAI-family egress targets so that
    // requests from the same caller are routed to the same inference
    // machine, improving prompt-prefix cache hit rates.
    let mut body_mutated =
        maybe_inject_prompt_cache_key(&mut upstream_body, &egress_protocol.suite, api_key_id);

    // Replace the (possibly virtual) model name with the routing
    // target's real upstream model id before sending and before we
    // snapshot the egress body for the request-log detail view.
    body_mutated |= override_model_in_body(&mut upstream_body, &target.model_id);
    body_mutated |= normalize_openai_reasoning_for_target(
        &mut upstream_body,
        &egress_protocol.suite,
        &target.model_id,
    );
    // Any body mutation invalidates the byte-for-byte raw body.
    let pass_through_verbatim = is_pass_through && !body_mutated;

    // Apply auth via the registered provider's AuthApplier. Falls
    // back to a static `Bearer {api_key}` if no provider is registered
    // for `target.provider_id` (e.g., test fixtures or built-in
    // OpenAI-compatible endpoints that don't need OAuth).
    //
    // First merge forwardable client request headers (denylist policy),
    // then apply auth so gateway-injected credentials always win.
    merge_client_headers(
        client_headers,
        &mut upstream_headers,
        &state.tunables().header_policy,
    );
    apply_provider_auth(target, &mut upstream_headers, &state.oauth_manager).await?;

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

    let client = &state.tunables().http_client;
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
        //
        // NOTE: we deliberately do NOT set `.timeout()` here. reqwest's
        // request timeout covers the *entire* request lifecycle including
        // reading the whole response body, so on a streaming (SSE)
        // response it caps the total generation time — a long
        // legitimately-streaming response (e.g. a large tool_use / plan
        // payload that takes > request_read_timeout to generate) would be
        // killed mid-stream with `operation timed out`. Streaming liveness
        // is instead bounded by `drive_upstream_stream`'s idle timer
        // (no-data window) + optional total budget. `request_read_timeout`
        // only applies to the non-streaming branch below.
        let mut stream_req = crate::ingress::observability::inject_trace(
            client.post(&upstream_url).headers(upstream_headers),
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
            crate::ingress::observability::finalize_egress(stream_req)?;
        let exec_started = std::time::Instant::now();
        let response = execute_with_ttfb_timeout(
            client,
            egress_req,
            state.tunables().upstream_ttfb_timeout_secs,
        )
        .await?;
        let ttfb_ms = Some(exec_started.elapsed().as_millis() as u64);

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
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
                },
            );
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!("Upstream {}: {}", status, error_body),
            );
            app_err.upstream_status = Some(status.as_u16());
            app_err = app_err.with_class(tiygate_core::classify_upstream_error(
                Some(status.as_u16()),
                None,
            ));
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
            tiygate_core::ErrorClass::DeadlineExceeded,
            None,
        );

        let forwarded_resp_headers = forwarded_resp_headers_for_capture(
            &upstream_resp_headers_capture,
            &state.tunables().header_policy,
            request_id,
        );
        let upstream_resp_headers_for_forward = upstream_resp_headers_capture.clone();
        let mut response = drive_upstream_stream(
            state,
            accum,
            Box::pin(
                response
                    .bytes_stream()
                    .map(|result| result.map_err(|error| error.to_string())),
            ),
            end_marker,
            error_marker,
            Duration::from_secs(state.tunables().upstream_stream_idle_timeout_secs),
            Duration::from_secs(state.tunables().upstream_stream_total_timeout_secs),
            DEFAULT_SSE_KEEPALIVE_INTERVAL,
            Some(StreamCapture {
                request_id: request_id.to_string(),
                telemetry: state.telemetry.clone(),
                health: Some(state.health.clone()),
                health_key: Some(target.health_key()),
                egress_method: egress_method.to_string(),
                egress_path: egress_path.to_string(),
                egress_headers: egress_headers_capture,
                egress_body: egress_body_capture,
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture,
                client_resp_headers: forwarded_resp_headers,
            }),
            build_stream_transcode(ingress_protocol, &egress_protocol),
            Some(ResponseModelOverride::new(
                ingress_protocol.suite,
                &ir_request.model,
            )),
        );
        // Forward upstream response headers to the client (denylist).
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_for_forward,
            &state.tunables().header_policy,
            request_id,
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
        Ok((response, ttfb_ms))
    } else {
        // PassThrough: forward raw body bytes verbatim (no re-serialize).
        // Non-PassThrough: re-serialize via reqwest::Client::json().
        // `inject_trace` stamps `traceparent` on the builder so the
        // upstream service sees the same trace id as the downstream.
        let mut nonstream_req = crate::ingress::observability::inject_trace(
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
            crate::ingress::observability::finalize_egress(nonstream_req)?;
        let exec_started = std::time::Instant::now();
        let response = client.execute(egress_req).await.map_err(|e| {
            AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {}", e))
        })?;
        let ttfb_ms = Some(exec_started.elapsed().as_millis() as u64);

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
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
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
            app_err = app_err.with_class(tiygate_core::classify_upstream_error(
                Some(status.as_u16()),
                None,
            ));
            if let Some(c) = response_body["error"]["code"]
                .as_str()
                .or_else(|| response_body["error"]["type"].as_str())
            {
                app_err = app_err.with_upstream_code(c);
            }
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            app_err.rate_limit_headers = rate_limit_headers_vec;
            return Err(app_err);
        }

        // Detect HTTP 200 responses that are actually error responses
        // (top-level `"error"` key, e.g. service_unavailable_error).
        // These must be treated as failures so fallback can retry.
        if let Some(app_err) = check_nonstream_error_body(
            &response_body,
            status.as_u16(),
            retry_after.clone(),
            rate_limit_headers_vec.clone(),
        ) {
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
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
                },
            );
            return Err(app_err);
        }

        // Keep a copy of the raw upstream body for the capture before
        // any cross-protocol re-encoding.
        let upstream_resp_body_capture = serde_json::to_string(&response_body).ok();

        // Cross-protocol re-encoding
        let mut response_json = if is_same_protocol {
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

        ResponseModelOverride::new(ingress_protocol.suite, &ir_request.model)
            .apply_json(&mut response_json);
        let client_resp_body_capture = serde_json::to_string(&response_json).ok();
        let mut response = Json(response_json).into_response();
        // Forward upstream response headers to the client (denylist).
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_capture,
            &state.tunables().header_policy,
            request_id,
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
                truncation_reason: None,
                stream_duration_ms: None,
                upstream_error: None,
                upstream_error_class: None,
            },
        );
        Ok((response, ttfb_ms))
    }
}

/// Execute an upstream Anthropic Messages request.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_messages_upstream(
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
    api_key_id: &str,
) -> Result<(Response, Option<u64>), AppError> {
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
                Ok(v) => {
                    // Obtain protocol-specific headers from the codec
                    // (e.g. anthropic-version). We reuse encode_request's
                    // header map but keep the raw body unchanged.
                    let headers = codec
                        .encode_request(ir_request)
                        .map(|(_, h)| h)
                        .unwrap_or_default();
                    (v, headers)
                }
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
    let mut body_mutated = override_model_in_body(&mut upstream_body, &target.model_id);
    body_mutated |=
        maybe_inject_prompt_cache_key(&mut upstream_body, &egress_protocol.suite, api_key_id);
    body_mutated |= normalize_openai_reasoning_for_target(
        &mut upstream_body,
        &egress_protocol.suite,
        &target.model_id,
    );
    let pass_through_verbatim = is_pass_through && !body_mutated;

    // Apply auth via the registered provider's AuthApplier. For
    // Anthropic, this inserts the x-api-key header. The
    // `anthropic-version` header is added by the MessagesCodec's
    // `encode_request` (see protocol/messages.rs), so it survives
    // here.
    //
    // Merge forwardable client request headers first, then auth so
    // gateway-injected credentials always win.
    merge_client_headers(
        client_headers,
        &mut upstream_headers,
        &state.tunables().header_policy,
    );
    apply_provider_auth(target, &mut upstream_headers, &state.oauth_manager).await?;

    // Capture egress request (headers + body) for the detail view.
    let egress_body_capture = if pass_through_verbatim {
        raw_passthrough_body.map(|s| s.to_string())
    } else {
        serde_json::to_string(&upstream_body).ok()
    };

    let client = &state.tunables().http_client;
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
        // No `.timeout()` on the streaming branch: reqwest's request
        // timeout caps the entire response-body read, which on an SSE
        // stream would kill a long-but-healthy generation mid-stream
        // (`operation timed out`). Streaming liveness is bounded by
        // `drive_upstream_stream`'s idle timer + optional total budget.
        let mut stream_req = crate::ingress::observability::inject_trace(
            client.post(&upstream_url).headers(upstream_headers),
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
            crate::ingress::observability::finalize_egress(stream_req)?;
        let exec_started = std::time::Instant::now();
        let response = execute_with_ttfb_timeout(
            client,
            egress_req,
            state.tunables().upstream_ttfb_timeout_secs,
        )
        .await?;
        let ttfb_ms = Some(exec_started.elapsed().as_millis() as u64);

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
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
                },
            );
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!("Upstream {}: {}", status, error_body),
            );
            app_err.upstream_status = Some(status.as_u16());
            app_err = app_err.with_class(tiygate_core::classify_upstream_error(
                Some(status.as_u16()),
                None,
            ));
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
            tiygate_core::ErrorClass::DeadlineExceeded,
            None,
        );

        let forwarded_resp_headers = forwarded_resp_headers_for_capture(
            &upstream_resp_headers_capture,
            &state.tunables().header_policy,
            request_id,
        );
        let upstream_resp_headers_for_forward = upstream_resp_headers_capture.clone();
        let mut response = drive_upstream_stream(
            state,
            accum,
            Box::pin(
                response
                    .bytes_stream()
                    .map(|result| result.map_err(|error| error.to_string())),
            ),
            end_marker,
            error_marker,
            Duration::from_secs(state.tunables().upstream_stream_idle_timeout_secs),
            Duration::from_secs(state.tunables().upstream_stream_total_timeout_secs),
            DEFAULT_SSE_KEEPALIVE_INTERVAL,
            Some(StreamCapture {
                request_id: request_id.to_string(),
                telemetry: state.telemetry.clone(),
                health: Some(state.health.clone()),
                health_key: Some(target.health_key()),
                egress_method: egress_method.to_string(),
                egress_path: egress_path.to_string(),
                egress_headers: egress_headers_capture,
                egress_body: egress_body_capture,
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture,
                client_resp_headers: forwarded_resp_headers,
            }),
            build_stream_transcode(ingress_protocol, &egress_protocol),
            Some(ResponseModelOverride::new(
                ingress_protocol.suite,
                &ir_request.model,
            )),
        );
        // Forward upstream response headers to the client (denylist).
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_for_forward,
            &state.tunables().header_policy,
            request_id,
        );
        if let Some(ra) = retry_after {
            response.headers_mut().insert(
                http::HeaderName::from_static("retry-after"),
                http::HeaderValue::from_str(&ra).unwrap_or(http::HeaderValue::from_static("")),
            );
        }
        Ok((response, ttfb_ms))
    } else {
        let mut nonstream_req = crate::ingress::observability::inject_trace(
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
            crate::ingress::observability::finalize_egress(nonstream_req)?;
        let exec_started = std::time::Instant::now();
        let response = client.execute(egress_req).await.map_err(|e| {
            AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {}", e))
        })?;
        let ttfb_ms = Some(exec_started.elapsed().as_millis() as u64);

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
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
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
            app_err = app_err.with_class(tiygate_core::classify_upstream_error(
                Some(status.as_u16()),
                None,
            ));
            if let Some(c) = response_body["error"]["code"]
                .as_str()
                .or_else(|| response_body["error"]["type"].as_str())
            {
                app_err = app_err.with_upstream_code(c);
            }
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            app_err.rate_limit_headers = rate_limit_headers_vec;
            return Err(app_err);
        }

        // Detect HTTP 200 responses that are actually error responses
        // (top-level `"error"` key, e.g. service_unavailable_error).
        if let Some(app_err) = check_nonstream_error_body(
            &response_body,
            status.as_u16(),
            retry_after.clone(),
            rate_limit_headers_vec.clone(),
        ) {
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
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
                },
            );
            return Err(app_err);
        }

        let upstream_resp_body_capture = serde_json::to_string(&response_body).ok();

        // Cross-protocol re-encoding: when the upstream spoke a different
        // protocol (e.g. OpenAI chat-completions) than the client's ingress
        // (Anthropic Messages), decode the upstream body via the egress codec
        // and re-encode it into the ingress protocol so the client sees the
        // format it expects. Mirrors `execute_upstream`.
        let mut response_json = if is_same_protocol {
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

        ResponseModelOverride::new(ingress_protocol.suite, &ir_request.model)
            .apply_json(&mut response_json);
        let client_resp_body_capture = serde_json::to_string(&response_json).ok();
        let mut response = Json(response_json).into_response();
        // Forward upstream response headers to the client (denylist).
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_capture,
            &state.tunables().header_policy,
            request_id,
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
                truncation_reason: None,
                stream_duration_ms: None,
                upstream_error: None,
                upstream_error_class: None,
            },
        );
        Ok((response, ttfb_ms))
    }
}

/// Get the appropriate egress codec for a protocol endpoint.
pub(super) fn get_egress_codec(
    protocol: &tiygate_core::ProtocolEndpoint,
) -> Option<Box<dyn EndpointCodec>> {
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
pub(super) fn gemini_aware_upstream_url(
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
/// the caller avoids protocol conversion (apart from client-model
/// normalization). The egress codec supplies the upstream decoder; the ingress
/// codec supplies the client encoder. Returns `None` if either codec is
/// unavailable rather than failing the request.
pub(super) fn build_stream_transcode(
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
pub(super) fn upstream_stream_url_for_suite(
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
pub(super) fn upstream_url_for_suite(
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
pub(super) fn encode_cross_protocol<C: EndpointCodec + ?Sized>(
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

/// Execute a single upstream call for the Embeddings protocol.
///
/// On success, also stores the result in the embedding cache (Phase 4 §4.7).
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_embeddings_upstream(
    state: &AppState,
    codec: &EmbeddingsCodec,
    ir_request: &IrRequest,
    target: &tiygate_core::RoutingTarget,
    trace: &TraceContext,
    request_id: &str,
    client_headers: &http::HeaderMap,
    cache_key: tiygate_cache::embedding_cache::EmbeddingCacheKey,
) -> Result<(Response, Option<u64>), AppError> {
    let (mut upstream_body, mut upstream_headers) =
        codec.encode_request(ir_request).map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Encode error: {e}"),
            )
        })?;

    override_model_in_body(&mut upstream_body, &target.model_id);
    merge_client_headers(
        client_headers,
        &mut upstream_headers,
        &state.tunables().header_policy,
    );
    apply_provider_auth(target, &mut upstream_headers, &state.oauth_manager).await?;

    let egress_body_capture = serde_json::to_string(&upstream_body).ok();
    let req_id_capture = request_id.to_string();

    let upstream_url = format!("{}/embeddings", target.effective_api_base());
    let builder = crate::ingress::observability::inject_trace(
        state.tunables().http_client.post(&upstream_url),
        trace,
    )
    .headers(upstream_headers)
    .json(&upstream_body);
    let (req, egress_headers_capture, egress_method, egress_path) =
        crate::ingress::observability::finalize_egress(builder)?;
    let exec_started = std::time::Instant::now();
    let response = state
        .tunables()
        .http_client
        .execute(req)
        .await
        .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")))?;
    let ttfb_ms = Some(exec_started.elapsed().as_millis() as u64);

    let status = response.status();
    let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
    let upstream_status_capture = status.as_u16();
    let response_body: Value = response
        .json()
        .await
        .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Parse error: {e}")))?;

    if !status.is_success() {
        spawn_capture(
            state,
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
                truncation_reason: None,
                stream_duration_ms: None,
                upstream_error: None,
                upstream_error_class: None,
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
        app_err = app_err.with_class(tiygate_core::classify_upstream_error(
            Some(status.as_u16()),
            None,
        ));
        if let Some(c) = response_body["error"]["code"]
            .as_str()
            .or_else(|| response_body["error"]["type"].as_str())
        {
            app_err = app_err.with_upstream_code(c);
        }
        return Err(app_err);
    }

    // Detect HTTP 200 responses that are actually error responses
    // (top-level `"error"` key, e.g. service_unavailable_error).
    if let Some(app_err) =
        check_nonstream_error_body(&response_body, status.as_u16(), None, Vec::new())
    {
        spawn_capture(
            state,
            tiygate_core::ExchangeCapture {
                request_id: req_id_capture.to_string(),
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
                truncation_reason: None,
                stream_duration_ms: None,
                upstream_error: None,
                upstream_error_class: None,
            },
        );
        return Err(app_err);
    }

    let upstream_resp_body_capture = serde_json::to_string(&response_body).ok();
    let mut client_response_body = response_body.clone();
    ResponseModelOverride::new(codec.id().suite, &ir_request.model)
        .apply_json(&mut client_response_body);
    let client_resp_body_capture = serde_json::to_string(&client_response_body).ok();
    let mut resp = Json(client_response_body).into_response();
    forward_upstream_resp_headers(
        &mut resp,
        &upstream_resp_headers_capture,
        &state.tunables().header_policy,
        &req_id_capture,
    );
    spawn_capture(
        state,
        tiygate_core::ExchangeCapture {
            request_id: req_id_capture,
            egress_method: egress_method.to_string(),
            egress_path: egress_path.to_string(),
            egress_headers: egress_headers_capture,
            egress_body: egress_body_capture,
            upstream_status: Some(upstream_status_capture),
            upstream_resp_headers: upstream_resp_headers_capture,
            upstream_resp_body: upstream_resp_body_capture,
            client_resp_headers: header_map_to_vec(resp.headers()),
            client_resp_body: client_resp_body_capture,
            is_stream: false,
            truncation_reason: None,
            stream_duration_ms: None,
            upstream_error: None,
            upstream_error_class: None,
        },
    );

    // Phase 4 §4.7: store the upstream response for the next call.
    crate::ingress::observability::embedding_cache_store(state, &cache_key, response_body).await;

    Ok((resp, ttfb_ms))
}

/// Execute a single upstream call for the Responses protocol.
///
/// Mirrors `execute_upstream` / `execute_messages_upstream` but handles
/// cross-protocol encoding/decoding (Responses → Chat / Messages / Gemini
/// and back) and both streaming and non-streaming paths.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_responses_upstream(
    state: &AppState,
    codec: &ResponsesCodec,
    ingress_protocol: &tiygate_core::ProtocolEndpoint,
    ir_request: &IrRequest,
    target: &tiygate_core::RoutingTarget,
    is_stream: bool,
    raw_passthrough_body: Option<&str>,
    trace: &TraceContext,
    request_id: &str,
    client_headers: &http::HeaderMap,
    api_key_id: &str,
) -> Result<(Response, Option<u64>), AppError> {
    let mut rejected_access_token = None;
    let first = execute_responses_upstream_once(
        state,
        codec,
        ingress_protocol,
        ir_request,
        target,
        is_stream,
        raw_passthrough_body,
        trace,
        request_id,
        client_headers,
        api_key_id,
        &mut rejected_access_token,
    )
    .await;
    let Err(first_error) = first else {
        return first;
    };
    if first_error.http_status() != StatusCode::UNAUTHORIZED {
        return Err(first_error);
    }
    let Some(rejected_access_token) = rejected_access_token else {
        return Err(first_error);
    };
    match state
        .oauth_manager
        .refresh_after_unauthorized(target, &rejected_access_token)
        .await
    {
        Ok(true) => {
            let mut retry_token = None;
            execute_responses_upstream_once(
                state,
                codec,
                ingress_protocol,
                ir_request,
                target,
                is_stream,
                raw_passthrough_body,
                trace,
                request_id,
                client_headers,
                api_key_id,
                &mut retry_token,
            )
            .await
        }
        Ok(false) | Err(_) => Err(first_error),
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_responses_upstream_once(
    state: &AppState,
    codec: &ResponsesCodec,
    ingress_protocol: &tiygate_core::ProtocolEndpoint,
    ir_request: &IrRequest,
    target: &tiygate_core::RoutingTarget,
    is_stream: bool,
    raw_passthrough_body: Option<&str>,
    trace: &TraceContext,
    request_id: &str,
    client_headers: &http::HeaderMap,
    api_key_id: &str,
    used_oauth_access_token: &mut Option<String>,
) -> Result<(Response, Option<u64>), AppError> {
    let egress_protocol = target.api_protocol.clone();
    let is_same_protocol = ingress_protocol.suite == egress_protocol.suite;
    let is_pass_through = raw_passthrough_body.is_some() && is_same_protocol;

    let (mut upstream_body, mut upstream_headers) = if let Some(raw) = raw_passthrough_body {
        if is_same_protocol {
            match serde_json::from_str::<serde_json::Value>(raw) {
                Ok(v) => {
                    // Obtain protocol-specific headers from the codec
                    // (e.g. anthropic-version). We reuse encode_request's
                    // header map but keep the raw body unchanged.
                    let headers = codec
                        .encode_request(ir_request)
                        .map(|(_, h)| h)
                        .unwrap_or_default();
                    (v, headers)
                }
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
                format!("Encode error: {e}"),
            )
        })?
    } else {
        encode_cross_protocol(codec, &egress_protocol, ir_request)?
    };

    let mut body_mutated = override_model_in_body(&mut upstream_body, &target.model_id);
    body_mutated |=
        maybe_inject_prompt_cache_key(&mut upstream_body, &egress_protocol.suite, api_key_id);
    body_mutated |= normalize_openai_reasoning_for_target(
        &mut upstream_body,
        &egress_protocol.suite,
        &target.model_id,
    );
    let codex_oauth = is_codex_oauth_target(target);
    let codex_websocket = codex_oauth && uses_codex_responses_websocket(target);
    if codex_oauth {
        body_mutated |= normalize_codex_request(&mut upstream_body, codex_websocket);
    }
    let pass_through_verbatim = is_pass_through && !body_mutated;
    merge_client_headers(
        client_headers,
        &mut upstream_headers,
        &state.tunables().header_policy,
    );
    apply_provider_auth(target, &mut upstream_headers, &state.oauth_manager).await?;
    if codex_oauth {
        ensure_codex_request_headers(&mut upstream_headers);
    }
    *used_oauth_access_token = upstream_headers
        .get(http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string);

    let mut egress_body_capture = if pass_through_verbatim {
        raw_passthrough_body.map(|s| s.to_string())
    } else {
        serde_json::to_string(&upstream_body).ok()
    };
    let req_id_capture = request_id.to_string();

    let upstream_url = if is_stream {
        upstream_stream_url_for_suite(target, egress_protocol.suite)
    } else {
        gemini_aware_upstream_url(target, egress_protocol.suite)
    };
    let upstream_url = upstream_url.ok_or_else(|| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "No upstream path for egress protocol suite: {:?}",
                egress_protocol.suite
            ),
        )
    })?;

    // ChatGPT/Codex OAuth Responses uses a WebSocket control plane rather
    // than HTTP POST + SSE. Keep this provider-specific transport outside of
    // the generic HTTP executor, while sharing the same canonical stream
    // bridge and response codecs below it.
    if codex_websocket && egress_protocol.suite == tiygate_core::ProtocolSuite::OpenAiResponses {
        let websocket_result = execute_codex_responses_websocket(
            state,
            codec,
            ingress_protocol,
            &egress_protocol,
            ir_request,
            target,
            is_stream,
            upstream_url.clone(),
            upstream_body.clone(),
            upstream_headers.clone(),
            trace,
            request_id,
            is_same_protocol,
        )
        .await;
        match websocket_result {
            Ok(response) => return Ok(response),
            Err(error) if error.http_status() == StatusCode::UPGRADE_REQUIRED => {
                normalize_codex_request(&mut upstream_body, false);
                egress_body_capture = serde_json::to_string(&upstream_body).ok();
            }
            Err(error) => return Err(error),
        }
    }

    if is_stream {
        let mut stream_req = crate::ingress::observability::inject_trace(
            state
                .tunables()
                .http_client
                .post(&upstream_url)
                .headers(upstream_headers)
                .header(http::header::ACCEPT, "text/event-stream"),
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
        let (egress_req, egress_headers_capture, egress_method, egress_path) =
            crate::ingress::observability::finalize_egress(stream_req)?;
        let exec_started = std::time::Instant::now();
        let tunables = state.tunables();
        let response = execute_with_ttfb_timeout(
            &tunables.http_client,
            egress_req,
            tunables.upstream_ttfb_timeout_secs,
        )
        .await?;
        let ttfb_ms = Some(exec_started.elapsed().as_millis() as u64);
        let retry_after = extract_retry_after(response.headers());
        let status = response.status();
        let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
        let upstream_status_capture = status.as_u16();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            spawn_capture(
                state,
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
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
                },
            );
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!("Upstream {}: {}", status, error_body),
            );
            app_err.upstream_status = Some(status.as_u16());
            app_err = app_err.with_class(tiygate_core::classify_upstream_error(
                Some(status.as_u16()),
                None,
            ));
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            return Err(app_err);
        }

        let accum = std::sync::Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let mut end_enc = codec.stream_encoder();
        let mut err_enc = codec.stream_encoder();
        let end_marker = end_enc.encode_done();
        let error_marker = err_enc.encode_error(
            "upstream stream truncated by gateway",
            tiygate_core::ErrorClass::DeadlineExceeded,
            None,
        );

        let mut response = drive_upstream_stream(
            state,
            accum,
            Box::pin(
                response
                    .bytes_stream()
                    .map(|result| result.map_err(|error| error.to_string())),
            ),
            end_marker,
            error_marker,
            Duration::from_secs(state.tunables().upstream_stream_idle_timeout_secs),
            Duration::from_secs(state.tunables().upstream_stream_total_timeout_secs),
            DEFAULT_SSE_KEEPALIVE_INTERVAL,
            Some(StreamCapture {
                request_id: req_id_capture.clone(),
                telemetry: state.telemetry.clone(),
                health: Some(state.health.clone()),
                health_key: Some(target.health_key()),
                egress_method: egress_method.to_string(),
                egress_path: egress_path.to_string(),
                egress_headers: egress_headers_capture.clone(),
                egress_body: egress_body_capture.clone(),
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture.clone(),
                client_resp_headers: forwarded_resp_headers_for_capture(
                    &upstream_resp_headers_capture,
                    &state.tunables().header_policy,
                    &req_id_capture,
                ),
            }),
            build_stream_transcode(ingress_protocol, &egress_protocol),
            Some(ResponseModelOverride::new(
                ingress_protocol.suite,
                &ir_request.model,
            )),
        );
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_capture,
            &state.tunables().header_policy,
            &req_id_capture,
        );
        if let Some(ra) = retry_after {
            response.headers_mut().insert(
                http::HeaderName::from_static("retry-after"),
                http::HeaderValue::from_str(&ra).unwrap_or(http::HeaderValue::from_static("")),
            );
        }
        for (name, value) in extract_rate_limit_headers(response.headers()) {
            if let Ok(hv) = http::HeaderValue::from_str(&value) {
                if let Ok(hn) = http::HeaderName::from_bytes(name.as_bytes()) {
                    response.headers_mut().insert(hn, hv);
                }
            }
        }
        return Ok((response, ttfb_ms));
    }

    // Non-streaming path
    let mut nonstream_req = crate::ingress::observability::inject_trace(
        state
            .tunables()
            .http_client
            .post(&upstream_url)
            .headers(upstream_headers),
        trace,
    );
    if codex_oauth {
        nonstream_req = nonstream_req.header(http::header::ACCEPT, "text/event-stream");
    }
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
    let (egress_req, egress_headers_capture, egress_method, egress_path) =
        crate::ingress::observability::finalize_egress(nonstream_req)?;
    let exec_started = std::time::Instant::now();
    let response = state
        .tunables()
        .http_client
        .execute(egress_req)
        .await
        .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")))?;
    let ttfb_ms = Some(exec_started.elapsed().as_millis() as u64);
    let status = response.status();
    let retry_after = extract_retry_after(response.headers());
    let rate_limit_headers_vec: Vec<(&'static str, String)> =
        extract_rate_limit_headers(response.headers());
    let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
    let upstream_status_capture = status.as_u16();
    let response_text = response
        .text()
        .await
        .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Read error: {e}")))?;
    let parsed_json = serde_json::from_str::<Value>(&response_text).ok();
    if !status.is_success() {
        let response_body = parsed_json.unwrap_or_else(
            || json!({"error": {"message": response_text, "type": "upstream_error"}}),
        );
        spawn_capture(
            state,
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
                truncation_reason: None,
                stream_duration_ms: None,
                upstream_error: None,
                upstream_error_class: None,
            },
        );
        let mut app_err = AppError::new(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            format!("Upstream {}: {}", status, response_body),
        );
        app_err.upstream_status = Some(status.as_u16());
        app_err = app_err.with_class(tiygate_core::classify_upstream_error(
            Some(status.as_u16()),
            None,
        ));
        if let Some(c) = response_body["error"]["code"]
            .as_str()
            .or_else(|| response_body["error"]["type"].as_str())
        {
            app_err = app_err.with_upstream_code(c);
        }
        if let Some(ra) = retry_after {
            app_err = app_err.with_retry_after_header(ra);
        }
        app_err.rate_limit_headers = rate_limit_headers_vec;
        return Err(app_err);
    }

    let response_body = if codex_oauth {
        parse_codex_http_response(&response_text)?
    } else {
        parsed_json.ok_or_else(|| {
            AppError::new(
                StatusCode::BAD_GATEWAY,
                "Parse error: upstream response is not JSON".to_string(),
            )
        })?
    };

    // Detect HTTP 200 responses that are actually error responses
    // (top-level "error" key, e.g. service_unavailable_error).
    if let Some(app_err) = check_nonstream_error_body(
        &response_body,
        status.as_u16(),
        retry_after.clone(),
        rate_limit_headers_vec.clone(),
    ) {
        spawn_capture(
            state,
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
                truncation_reason: None,
                stream_duration_ms: None,
                upstream_error: None,
                upstream_error_class: None,
            },
        );
        return Err(app_err);
    }

    let upstream_resp_body_capture = if codex_oauth {
        Some(response_text)
    } else {
        serde_json::to_string(&response_body).ok()
    };
    let mut response_body = if is_same_protocol {
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
                format!("Decode response error: {e}"),
            )
        })?;
        codec.encode_response(&ir_response).map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Encode response error: {e}"),
            )
        })?
    };
    ResponseModelOverride::new(ingress_protocol.suite, &ir_request.model)
        .apply_json(&mut response_body);
    let body_str_capture = serde_json::to_string(&response_body).ok();
    let mut resp = Json(response_body).into_response();
    forward_upstream_resp_headers(
        &mut resp,
        &upstream_resp_headers_capture,
        &state.tunables().header_policy,
        &req_id_capture,
    );
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
    spawn_capture(
        state,
        tiygate_core::ExchangeCapture {
            request_id: req_id_capture,
            egress_method: egress_method.to_string(),
            egress_path: egress_path.to_string(),
            egress_headers: egress_headers_capture,
            egress_body: egress_body_capture,
            upstream_status: Some(upstream_status_capture),
            upstream_resp_headers: upstream_resp_headers_capture,
            upstream_resp_body: upstream_resp_body_capture,
            client_resp_headers: header_map_to_vec(resp.headers()),
            client_resp_body: body_str_capture,
            is_stream: false,
            truncation_reason: None,
            stream_duration_ms: None,
            upstream_error: None,
            upstream_error_class: None,
        },
    );
    Ok((resp, ttfb_ms))
}

/// Execute OpenAI Codex Responses over its WebSocket transport.
///
/// The public gateway contract remains HTTP JSON/SSE. This adapter performs
/// the Codex handshake and `response.create` command, then normalizes each
/// JSON WebSocket frame into the Responses SSE shape consumed by the shared
/// stream bridge.
#[allow(clippy::too_many_arguments)]
async fn execute_codex_responses_websocket(
    state: &AppState,
    codec: &ResponsesCodec,
    ingress_protocol: &tiygate_core::ProtocolEndpoint,
    egress_protocol: &tiygate_core::ProtocolEndpoint,
    ir_request: &IrRequest,
    target: &tiygate_core::RoutingTarget,
    is_stream: bool,
    upstream_url: String,
    mut upstream_body: Value,
    mut upstream_headers: http::HeaderMap,
    trace: &TraceContext,
    request_id: &str,
    is_same_protocol: bool,
) -> Result<(Response, Option<u64>), AppError> {
    upstream_body = codex_response_create(upstream_body)?;

    // The payload lives exclusively in the first WebSocket message. Keeping
    // HTTP entity headers on the upgrade request can make strict proxies
    // treat it as an unsupported HTTP POST-style content type.
    upstream_headers.remove(http::header::CONTENT_TYPE);
    upstream_headers.remove(http::header::CONTENT_LENGTH);
    upstream_headers.remove(http::header::TRANSFER_ENCODING);
    upstream_headers.insert(
        http::HeaderName::from_static("openai-beta"),
        http::HeaderValue::from_static(CODEX_RESPONSES_WEBSOCKET_BETA),
    );
    let trace_value = http::HeaderValue::from_str(&trace.to_traceparent()).map_err(|error| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("invalid traceparent header: {error}"),
        )
    })?;
    upstream_headers.insert(http::HeaderName::from_static("traceparent"), trace_value);

    let websocket_url = codex_websocket_url(&upstream_url)?;
    let request = codex_websocket_handshake_request(&websocket_url, &upstream_headers)?;
    let egress_path = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let egress_headers_capture = header_map_to_vec(&upstream_headers);
    let egress_body_capture = serde_json::to_string(&upstream_body).ok();
    let request_id_capture = request_id.to_string();

    let connect_started = std::time::Instant::now();
    let connect = tokio_tungstenite::connect_async(request);
    let connection = if state.tunables().upstream_ttfb_timeout_secs > 0 {
        match tokio::time::timeout(
            Duration::from_secs(state.tunables().upstream_ttfb_timeout_secs),
            connect,
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                let mut error = AppError::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "Codex WebSocket handshake timeout".to_string(),
                )
                .with_class(tiygate_core::ErrorClass::DeadlineExceeded);
                error.upstream_status = Some(504);
                spawn_capture(
                    state,
                    tiygate_core::ExchangeCapture {
                        request_id: request_id_capture,
                        egress_method: "GET".to_string(),
                        egress_path,
                        egress_headers: egress_headers_capture,
                        egress_body: egress_body_capture,
                        upstream_status: Some(504),
                        upstream_resp_headers: Vec::new(),
                        upstream_resp_body: Some("Codex WebSocket handshake timeout".to_string()),
                        client_resp_headers: Vec::new(),
                        client_resp_body: None,
                        is_stream,
                        truncation_reason: None,
                        stream_duration_ms: None,
                        upstream_error: None,
                        upstream_error_class: None,
                    },
                );
                return Err(error);
            }
        }
    } else {
        connect.await
    };
    let (mut socket, handshake_response) = match connection {
        Ok(connection) => connection,
        Err(error) => {
            let status = match &error {
                tokio_tungstenite::tungstenite::Error::Http(response) => response.status(),
                _ => StatusCode::BAD_GATEWAY,
            };
            let message = format!("Codex WebSocket handshake error: {error}");
            spawn_capture(
                state,
                tiygate_core::ExchangeCapture {
                    request_id: request_id_capture,
                    egress_method: "GET".to_string(),
                    egress_path,
                    egress_headers: egress_headers_capture,
                    egress_body: egress_body_capture,
                    upstream_status: Some(status.as_u16()),
                    upstream_resp_headers: Vec::new(),
                    upstream_resp_body: Some(message.clone()),
                    client_resp_headers: Vec::new(),
                    client_resp_body: None,
                    is_stream,
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
                },
            );
            let mut app_error = AppError::new(status, message);
            app_error.upstream_status = Some(status.as_u16());
            return Err(app_error);
        }
    };
    let ttfb_ms = Some(connect_started.elapsed().as_millis() as u64);
    let handshake_status = handshake_response.status().as_u16();
    let handshake_headers_capture = header_map_to_vec(handshake_response.headers());
    let create_message = serde_json::to_string(&upstream_body).map_err(|error| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("serialize Codex response.create: {error}"),
        )
    })?;
    if let Err(error) = socket.send(Message::Text(create_message.into())).await {
        let message = format!("send Codex response.create: {error}");
        spawn_capture(
            state,
            tiygate_core::ExchangeCapture {
                request_id: request_id_capture,
                egress_method: "GET".to_string(),
                egress_path,
                egress_headers: egress_headers_capture,
                egress_body: egress_body_capture,
                upstream_status: Some(handshake_status),
                upstream_resp_headers: handshake_headers_capture,
                upstream_resp_body: Some(message.clone()),
                client_resp_headers: Vec::new(),
                client_resp_body: None,
                is_stream,
                truncation_reason: None,
                stream_duration_ms: None,
                upstream_error: None,
                upstream_error_class: None,
            },
        );
        return Err(AppError::new(StatusCode::BAD_GATEWAY, message));
    }

    if is_stream {
        let accum = std::sync::Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let mut end_enc = codec.stream_encoder();
        let mut err_enc = codec.stream_encoder();
        let end_marker = end_enc.encode_done();
        let error_marker = err_enc.encode_error(
            "upstream stream truncated by gateway",
            tiygate_core::ErrorClass::DeadlineExceeded,
            None,
        );
        let mut response = drive_upstream_stream(
            state,
            accum,
            websocket_event_stream(socket),
            end_marker,
            error_marker,
            Duration::from_secs(state.tunables().upstream_stream_idle_timeout_secs),
            Duration::from_secs(state.tunables().upstream_stream_total_timeout_secs),
            DEFAULT_SSE_KEEPALIVE_INTERVAL,
            Some(StreamCapture {
                request_id: request_id_capture,
                telemetry: state.telemetry.clone(),
                health: Some(state.health.clone()),
                health_key: Some(target.health_key()),
                egress_method: "GET".to_string(),
                egress_path,
                egress_headers: egress_headers_capture,
                egress_body: egress_body_capture,
                upstream_status: Some(handshake_status),
                upstream_resp_headers: handshake_headers_capture,
                // A WebSocket upgrade response is not a downstream HTTP
                // response; never forward its upgrade/connection headers.
                client_resp_headers: super::headers::forwarded_resp_headers_for_capture(
                    &Vec::new(),
                    &state.tunables().header_policy,
                    request_id,
                ),
            }),
            build_stream_transcode(ingress_protocol, egress_protocol),
            Some(ResponseModelOverride::new(
                ingress_protocol.suite,
                &ir_request.model,
            )),
        );
        super::headers::set_gateway_request_id_header(&mut response, request_id);
        return Ok((response, ttfb_ms));
    }

    let upstream_response = match collect_codex_websocket_response(&mut socket).await {
        Ok(response) => response,
        Err(message) => {
            spawn_capture(
                state,
                tiygate_core::ExchangeCapture {
                    request_id: request_id_capture,
                    egress_method: "GET".to_string(),
                    egress_path,
                    egress_headers: egress_headers_capture,
                    egress_body: egress_body_capture,
                    upstream_status: Some(handshake_status),
                    upstream_resp_headers: handshake_headers_capture,
                    upstream_resp_body: Some(message.clone()),
                    client_resp_headers: Vec::new(),
                    client_resp_body: None,
                    is_stream: false,
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
                },
            );
            return Err(AppError::new(StatusCode::BAD_GATEWAY, message));
        }
    };

    let upstream_resp_body_capture = serde_json::to_string(&upstream_response).ok();
    let mut response_body = if is_same_protocol {
        upstream_response
    } else {
        let egress_codec = get_egress_codec(egress_protocol).ok_or_else(|| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("No egress codec found: {egress_protocol:?}"),
            )
        })?;
        let ir_response = egress_codec
            .decode_response(upstream_response)
            .map_err(|error| {
                AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Decode response error: {error}"),
                )
            })?;
        codec.encode_response(&ir_response).map_err(|error| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Encode response error: {error}"),
            )
        })?
    };
    ResponseModelOverride::new(ingress_protocol.suite, &ir_request.model)
        .apply_json(&mut response_body);
    let client_body_capture = serde_json::to_string(&response_body).ok();
    let mut response = Json(response_body).into_response();
    super::headers::set_gateway_request_id_header(&mut response, request_id);
    spawn_capture(
        state,
        tiygate_core::ExchangeCapture {
            request_id: request_id_capture,
            egress_method: "GET".to_string(),
            egress_path,
            egress_headers: egress_headers_capture,
            egress_body: egress_body_capture,
            upstream_status: Some(handshake_status),
            upstream_resp_headers: handshake_headers_capture,
            upstream_resp_body: upstream_resp_body_capture,
            client_resp_headers: header_map_to_vec(response.headers()),
            client_resp_body: client_body_capture,
            is_stream: false,
            truncation_reason: None,
            stream_duration_ms: None,
            upstream_error: None,
            upstream_error_class: None,
        },
    );
    Ok((response, ttfb_ms))
}

/// Handle POST /v1/embeddings.
///
/// Wiring (§4.7 + §4.1 + §4.8):
/// 1. Build a *redacted* `RawEnvelope` for the audit log.
/// 2. Extract (or mint) the W3C trace context.
/// 3. Check the embedding cache; on hit, serve the cached value
///    and emit a `RequestEvent` with `cache_hit = hit`.
/// 4. On miss, build the upstream request, inject the
///    `traceparent` header, call the upstream, store the response,
///    and emit a `RequestEvent` with `cache_hit = miss`.
///
/// Execute a single upstream call for the Gemini protocol.
///
/// Structurally identical to `execute_responses_upstream` but uses the
/// `GeminiCodec` and resolves both streaming and non-streaming URLs
/// up-front because Gemini has model-embedded URL grammar
/// (`:streamGenerateContent` / `:generateContent`).
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_gemini_upstream(
    state: &AppState,
    codec: &GeminiCodec,
    ingress_protocol: &tiygate_core::ProtocolEndpoint,
    ir_request: &IrRequest,
    target: &tiygate_core::RoutingTarget,
    is_stream: bool,
    raw_passthrough_body: Option<&str>,
    trace: &TraceContext,
    request_id: &str,
    client_headers: &http::HeaderMap,
    api_key_id: &str,
) -> Result<(Response, Option<u64>), AppError> {
    // Delegate to the shared Responses executor — the only difference is
    // the codec type, and `execute_responses_upstream` already handles
    // cross-protocol encoding via `encode_cross_protocol` which is
    // codec-generic through the `EndpointCodec` trait. But since the
    // signatures are typed to concrete codec types, we duplicate the body
    // via a copy of `execute_responses_upstream` parameterised on
    // `GeminiCodec`. This preserves the Gemini-specific URL grammar
    // (model-embedded `:generateContent`/`:streamGenerateContent`
    // suffixes) via `gemini_aware_upstream_url` /
    // `upstream_stream_url_for_suite`.

    let egress_protocol = target.api_protocol.clone();
    let is_same_protocol = ingress_protocol.suite == egress_protocol.suite;
    let is_pass_through = raw_passthrough_body.is_some() && is_same_protocol;

    let (mut upstream_body, mut upstream_headers) = if let Some(raw) = raw_passthrough_body {
        if is_same_protocol {
            match serde_json::from_str::<serde_json::Value>(raw) {
                Ok(v) => {
                    // Obtain protocol-specific headers from the codec
                    // (e.g. anthropic-version). We reuse encode_request's
                    // header map but keep the raw body unchanged.
                    let headers = codec
                        .encode_request(ir_request)
                        .map(|(_, h)| h)
                        .unwrap_or_default();
                    (v, headers)
                }
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
                format!("Encode error: {e}"),
            )
        })?
    } else {
        encode_cross_protocol(codec, &egress_protocol, ir_request)?
    };

    let mut body_mutated = override_model_in_body(&mut upstream_body, &target.model_id);
    body_mutated |=
        maybe_inject_prompt_cache_key(&mut upstream_body, &egress_protocol.suite, api_key_id);
    body_mutated |= normalize_openai_reasoning_for_target(
        &mut upstream_body,
        &egress_protocol.suite,
        &target.model_id,
    );
    let pass_through_verbatim = is_pass_through && !body_mutated;
    merge_client_headers(
        client_headers,
        &mut upstream_headers,
        &state.tunables().header_policy,
    );
    apply_provider_auth(target, &mut upstream_headers, &state.oauth_manager).await?;

    let egress_body_capture = if pass_through_verbatim {
        raw_passthrough_body.map(|s| s.to_string())
    } else {
        serde_json::to_string(&upstream_body).ok()
    };
    let req_id_capture = request_id.to_string();

    let upstream_url = if is_stream {
        upstream_stream_url_for_suite(target, egress_protocol.suite)
    } else {
        gemini_aware_upstream_url(target, egress_protocol.suite)
    };
    let upstream_url = upstream_url.ok_or_else(|| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "No upstream path for egress protocol suite: {:?}",
                egress_protocol.suite
            ),
        )
    })?;

    if is_stream {
        let mut stream_req = crate::ingress::observability::inject_trace(
            state
                .tunables()
                .http_client
                .post(&upstream_url)
                .headers(upstream_headers),
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
        let (egress_req, egress_headers_capture, egress_method, egress_path) =
            crate::ingress::observability::finalize_egress(stream_req)?;
        let exec_started = std::time::Instant::now();
        let tunables = state.tunables();
        let response = execute_with_ttfb_timeout(
            &tunables.http_client,
            egress_req,
            tunables.upstream_ttfb_timeout_secs,
        )
        .await?;
        let ttfb_ms = Some(exec_started.elapsed().as_millis() as u64);
        let retry_after = extract_retry_after(response.headers());
        let status = response.status();
        let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
        let upstream_status_capture = status.as_u16();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            spawn_capture(
                state,
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
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
                },
            );
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!("Upstream {}: {}", status, error_body),
            );
            app_err.upstream_status = Some(status.as_u16());
            app_err = app_err.with_class(tiygate_core::classify_upstream_error(
                Some(status.as_u16()),
                None,
            ));
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            return Err(app_err);
        }

        let accum = std::sync::Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let mut end_enc = codec.stream_encoder();
        let mut err_enc = codec.stream_encoder();
        let end_marker = end_enc.encode_done();
        let error_marker = err_enc.encode_error(
            "upstream stream truncated by gateway",
            tiygate_core::ErrorClass::DeadlineExceeded,
            None,
        );

        let mut response = drive_upstream_stream(
            state,
            accum,
            Box::pin(
                response
                    .bytes_stream()
                    .map(|result| result.map_err(|error| error.to_string())),
            ),
            end_marker,
            error_marker,
            Duration::from_secs(state.tunables().upstream_stream_idle_timeout_secs),
            Duration::from_secs(state.tunables().upstream_stream_total_timeout_secs),
            DEFAULT_SSE_KEEPALIVE_INTERVAL,
            Some(StreamCapture {
                request_id: req_id_capture.clone(),
                telemetry: state.telemetry.clone(),
                health: Some(state.health.clone()),
                health_key: Some(target.health_key()),
                egress_method: egress_method.to_string(),
                egress_path: egress_path.to_string(),
                egress_headers: egress_headers_capture.clone(),
                egress_body: egress_body_capture.clone(),
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture.clone(),
                client_resp_headers: forwarded_resp_headers_for_capture(
                    &upstream_resp_headers_capture,
                    &state.tunables().header_policy,
                    &req_id_capture,
                ),
            }),
            build_stream_transcode(ingress_protocol, &egress_protocol),
            Some(ResponseModelOverride::new(
                ingress_protocol.suite,
                &ir_request.model,
            )),
        );
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_capture,
            &state.tunables().header_policy,
            &req_id_capture,
        );
        if let Some(ra) = retry_after {
            response.headers_mut().insert(
                http::HeaderName::from_static("retry-after"),
                http::HeaderValue::from_str(&ra).unwrap_or(http::HeaderValue::from_static("")),
            );
        }
        for (name, value) in extract_rate_limit_headers(response.headers()) {
            if let Ok(hv) = http::HeaderValue::from_str(&value) {
                if let Ok(hn) = http::HeaderName::from_bytes(name.as_bytes()) {
                    response.headers_mut().insert(hn, hv);
                }
            }
        }
        return Ok((response, ttfb_ms));
    }

    // Non-streaming path
    let mut nonstream_req = crate::ingress::observability::inject_trace(
        state
            .tunables()
            .http_client
            .post(&upstream_url)
            .headers(upstream_headers),
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
    let (egress_req, egress_headers_capture, egress_method, egress_path) =
        crate::ingress::observability::finalize_egress(nonstream_req)?;
    let exec_started = std::time::Instant::now();
    let response = state
        .tunables()
        .http_client
        .execute(egress_req)
        .await
        .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")))?;
    let ttfb_ms = Some(exec_started.elapsed().as_millis() as u64);
    let status = response.status();
    let retry_after = extract_retry_after(response.headers());
    let rate_limit_headers_vec: Vec<(&'static str, String)> =
        extract_rate_limit_headers(response.headers());
    let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
    let upstream_status_capture = status.as_u16();
    let response_body: Value = response
        .json()
        .await
        .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Parse error: {e}")))?;
    if !status.is_success() {
        spawn_capture(
            state,
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
                truncation_reason: None,
                stream_duration_ms: None,
                upstream_error: None,
                upstream_error_class: None,
            },
        );
        let mut app_err = AppError::new(
            StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
            format!("Upstream {}: {}", status, response_body),
        );
        app_err.upstream_status = Some(status.as_u16());
        app_err = app_err.with_class(tiygate_core::classify_upstream_error(
            Some(status.as_u16()),
            None,
        ));
        if let Some(c) = response_body["error"]["code"]
            .as_str()
            .or_else(|| response_body["error"]["type"].as_str())
        {
            app_err = app_err.with_upstream_code(c);
        }
        if let Some(ra) = retry_after {
            app_err = app_err.with_retry_after_header(ra);
        }
        app_err.rate_limit_headers = rate_limit_headers_vec;
        return Err(app_err);
    }

    // Detect HTTP 200 responses that are actually error responses
    // (top-level "error" key, e.g. service_unavailable_error).
    if let Some(app_err) = check_nonstream_error_body(
        &response_body,
        status.as_u16(),
        retry_after.clone(),
        rate_limit_headers_vec.clone(),
    ) {
        spawn_capture(
            state,
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
                truncation_reason: None,
                stream_duration_ms: None,
                upstream_error: None,
                upstream_error_class: None,
            },
        );
        return Err(app_err);
    }

    let upstream_resp_body_capture = serde_json::to_string(&response_body).ok();
    let mut response_body = if is_same_protocol {
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
                format!("Decode response error: {e}"),
            )
        })?;
        codec.encode_response(&ir_response).map_err(|e| {
            AppError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Encode response error: {e}"),
            )
        })?
    };
    ResponseModelOverride::new(ingress_protocol.suite, &ir_request.model)
        .apply_json(&mut response_body);
    let body_str_capture = serde_json::to_string(&response_body).ok();
    let mut resp = Json(response_body).into_response();
    forward_upstream_resp_headers(
        &mut resp,
        &upstream_resp_headers_capture,
        &state.tunables().header_policy,
        &req_id_capture,
    );
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
    spawn_capture(
        state,
        tiygate_core::ExchangeCapture {
            request_id: req_id_capture,
            egress_method: egress_method.to_string(),
            egress_path: egress_path.to_string(),
            egress_headers: egress_headers_capture,
            egress_body: egress_body_capture,
            upstream_status: Some(upstream_status_capture),
            upstream_resp_headers: upstream_resp_headers_capture,
            upstream_resp_body: upstream_resp_body_capture,
            client_resp_headers: header_map_to_vec(resp.headers()),
            client_resp_body: body_str_capture,
            is_stream: false,
            truncation_reason: None,
            stream_duration_ms: None,
            upstream_error: None,
            upstream_error_class: None,
        },
    );
    Ok((resp, ttfb_ms))
}

/// Execute a single upstream call for the Images Generations protocol.
///
/// Forwards the raw JSON body verbatim (passthrough) to the upstream
/// `/images/generations` endpoint, applying model override when the
/// virtual model differs from the target model. Supports both
/// non-streaming (JSON response) and streaming (SSE) paths.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_images_generations_upstream(
    state: &AppState,
    codec: &ImagesGenerationsCodec,
    ingress_protocol: &tiygate_core::ProtocolEndpoint,
    ir_request: &IrRequest,
    target: &tiygate_core::RoutingTarget,
    is_stream: bool,
    raw_passthrough_body: Option<&str>,
    trace: &TraceContext,
    request_id: &str,
    client_headers: &http::HeaderMap,
    api_key_id: &str,
) -> Result<(Response, Option<u64>), AppError> {
    let egress_protocol = target.api_protocol.clone();
    let is_same_protocol = ingress_protocol.suite == egress_protocol.suite;
    let is_pass_through = raw_passthrough_body.is_some() && is_same_protocol;

    let (mut upstream_body, mut upstream_headers) = if let Some(raw) = raw_passthrough_body {
        if is_same_protocol {
            match serde_json::from_str::<Value>(raw) {
                Ok(v) => {
                    let headers = codec
                        .encode_request(ir_request)
                        .map(|(_, h)| h)
                        .unwrap_or_default();
                    (v, headers)
                }
                Err(e) => {
                    return Err(AppError::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("PassThrough: invalid raw body JSON: {e}"),
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
                format!("Encode error: {e}"),
            )
        })?
    } else {
        encode_cross_protocol(codec, &egress_protocol, ir_request)?
    };

    let cache_key_injected =
        maybe_inject_prompt_cache_key(&mut upstream_body, &egress_protocol.suite, api_key_id);

    let model_was_overridden = override_model_in_body(&mut upstream_body, &target.model_id);
    let pass_through_verbatim = is_pass_through && !model_was_overridden && !cache_key_injected;

    merge_client_headers(
        client_headers,
        &mut upstream_headers,
        &state.tunables().header_policy,
    );
    apply_provider_auth(target, &mut upstream_headers, &state.oauth_manager).await?;

    let egress_body_capture = if pass_through_verbatim {
        raw_passthrough_body.map(|s| s.to_string())
    } else {
        serde_json::to_string(&upstream_body).ok()
    };

    let client = &state.tunables().http_client;
    let upstream_url = format!("{}/images/generations", target.effective_api_base());

    if is_stream {
        let mut stream_req = crate::ingress::observability::inject_trace(
            client.post(&upstream_url).headers(upstream_headers),
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
        let (egress_req, egress_headers_capture, egress_method, egress_path) =
            crate::ingress::observability::finalize_egress(stream_req)?;
        let exec_started = std::time::Instant::now();
        let response = execute_with_ttfb_timeout(
            client,
            egress_req,
            state.tunables().upstream_ttfb_timeout_secs,
        )
        .await?;
        let ttfb_ms = Some(exec_started.elapsed().as_millis() as u64);

        let retry_after = extract_retry_after(response.headers());
        let rate_limit_headers_vec: Vec<(&'static str, String)> =
            extract_rate_limit_headers(response.headers());
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
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
                },
            );
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!("Upstream {status}: {error_body}"),
            );
            app_err.upstream_status = Some(status.as_u16());
            app_err = app_err.with_class(tiygate_core::classify_upstream_error(
                Some(status.as_u16()),
                None,
            ));
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            app_err.rate_limit_headers = rate_limit_headers_vec;
            return Err(app_err);
        }

        let accum =
            std::sync::Arc::new(std::sync::Mutex::new(tiygate_core::UsageAccumulator::new()));

        let mut end_enc = codec.stream_encoder();
        let mut err_enc = codec.stream_encoder();
        let end_marker = end_enc.encode_done();
        let error_marker = err_enc.encode_error(
            "upstream stream truncated by gateway",
            tiygate_core::ErrorClass::DeadlineExceeded,
            None,
        );

        let forwarded_resp_headers = forwarded_resp_headers_for_capture(
            &upstream_resp_headers_capture,
            &state.tunables().header_policy,
            request_id,
        );
        let upstream_resp_headers_for_forward = upstream_resp_headers_capture.clone();
        let mut response = drive_upstream_stream(
            state,
            accum,
            Box::pin(
                response
                    .bytes_stream()
                    .map(|result| result.map_err(|error| error.to_string())),
            ),
            end_marker,
            error_marker,
            Duration::from_secs(state.tunables().upstream_stream_idle_timeout_secs),
            Duration::from_secs(state.tunables().upstream_stream_total_timeout_secs),
            DEFAULT_SSE_KEEPALIVE_INTERVAL,
            Some(StreamCapture {
                request_id: request_id.to_string(),
                telemetry: state.telemetry.clone(),
                health: Some(state.health.clone()),
                health_key: Some(target.health_key()),
                egress_method: egress_method.to_string(),
                egress_path: egress_path.to_string(),
                egress_headers: egress_headers_capture,
                egress_body: egress_body_capture,
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture,
                client_resp_headers: forwarded_resp_headers,
            }),
            build_stream_transcode(ingress_protocol, &egress_protocol),
            Some(ResponseModelOverride::new(
                ingress_protocol.suite,
                &ir_request.model,
            )),
        );
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_for_forward,
            &state.tunables().header_policy,
            request_id,
        );
        if let Some(ra) = retry_after {
            response.headers_mut().insert(
                http::HeaderName::from_static("retry-after"),
                http::HeaderValue::from_str(&ra).unwrap_or(http::HeaderValue::from_static("")),
            );
        }
        for (name, value) in extract_rate_limit_headers(response.headers()) {
            if let Ok(hv) = http::HeaderValue::from_str(&value) {
                if let Ok(hn) = http::HeaderName::from_bytes(name.as_bytes()) {
                    response.headers_mut().insert(hn, hv);
                }
            }
        }
        Ok((response, ttfb_ms))
    } else {
        let mut nonstream_req = crate::ingress::observability::inject_trace(
            client
                .post(&upstream_url)
                .headers(upstream_headers)
                .timeout(IMAGES_NONSTREAM_TIMEOUT),
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
        let (egress_req, egress_headers_capture, egress_method, egress_path) =
            crate::ingress::observability::finalize_egress(nonstream_req)?;
        let exec_started = std::time::Instant::now();
        let response = client
            .execute(egress_req)
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")))?;
        let ttfb_ms = Some(exec_started.elapsed().as_millis() as u64);

        let retry_after = extract_retry_after(response.headers());
        let rate_limit_headers_vec: Vec<(&'static str, String)> =
            extract_rate_limit_headers(response.headers());
        let status = response.status();
        let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
        let upstream_status_capture = status.as_u16();
        let response_text = response
            .text()
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Read error: {e}")))?;
        let response_body: Value = serde_json::from_str(&response_text)
            .unwrap_or_else(|_| json!({"error": {"message": response_text}}));

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
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
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
            app_err = app_err.with_class(tiygate_core::classify_upstream_error(
                Some(status.as_u16()),
                None,
            ));
            if let Some(c) = response_body["error"]["code"]
                .as_str()
                .or_else(|| response_body["error"]["type"].as_str())
            {
                app_err = app_err.with_upstream_code(c);
            }
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            app_err.rate_limit_headers = rate_limit_headers_vec;
            return Err(app_err);
        }

        // Detect HTTP 200 responses that are actually error responses
        // (top-level `"error"` key, e.g. service_unavailable_error).
        if let Some(app_err) = check_nonstream_error_body(
            &response_body,
            status.as_u16(),
            retry_after.clone(),
            rate_limit_headers_vec.clone(),
        ) {
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
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
                },
            );
            return Err(app_err);
        }

        // Cross-protocol re-encoding: when the egress suite differs
        // from the ingress suite, decode via the egress codec and
        // re-encode to the ingress protocol. Same-suite: forward
        // verbatim.
        let mut response_json = if is_same_protocol {
            response_body
        } else {
            let egress_codec = get_egress_codec(&egress_protocol).ok_or_else(|| {
                AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("No egress codec found: {egress_protocol:?}"),
                )
            })?;
            let ir_response = egress_codec.decode_response(response_body).map_err(|e| {
                AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Decode response error: {e}"),
                )
            })?;
            codec.encode_response(&ir_response).map_err(|e| {
                AppError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Encode response error: {e}"),
                )
            })?
        };

        let upstream_resp_body_capture = serde_json::to_string(&response_json).ok();
        ResponseModelOverride::new(ingress_protocol.suite, &ir_request.model)
            .apply_json(&mut response_json);
        let client_resp_body_capture = serde_json::to_string(&response_json).ok();
        let mut response = Json(response_json).into_response();
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_capture,
            &state.tunables().header_policy,
            request_id,
        );
        if let Some(ra) = retry_after {
            response.headers_mut().insert(
                http::HeaderName::from_static("retry-after"),
                http::HeaderValue::from_str(&ra)
                    .unwrap_or_else(|_| http::HeaderValue::from_static("")),
            );
        }
        for (name, value) in extract_rate_limit_headers(response.headers()) {
            if let Ok(hv) = http::HeaderValue::from_str(&value) {
                if let Ok(hn) = http::HeaderName::from_bytes(name.as_bytes()) {
                    response.headers_mut().insert(hn, hv);
                }
            }
        }
        spawn_capture(
            state,
            tiygate_core::ExchangeCapture {
                request_id: request_id.to_string(),
                egress_method: egress_method.to_string(),
                egress_path: egress_path.to_string(),
                egress_headers: egress_headers_capture,
                egress_body: egress_body_capture,
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture,
                upstream_resp_body: upstream_resp_body_capture,
                client_resp_headers: header_map_to_vec(response.headers()),
                client_resp_body: client_resp_body_capture,
                is_stream: false,
                truncation_reason: None,
                stream_duration_ms: None,
                upstream_error: None,
                upstream_error_class: None,
            },
        );
        Ok((response, ttfb_ms))
    }
}

/// Execute a single upstream call for the Images Edits protocol.
///
/// Forwards the raw multipart/form-data bytes verbatim to the upstream
/// `/images/edits` endpoint. The original Content-Type header (including
/// the multipart boundary) is preserved. No model override is applied
/// (multipart re-encoding is not supported in this version).
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_images_edits_upstream(
    state: &AppState,
    target: &tiygate_core::RoutingTarget,
    virtual_model: &str,
    is_stream: bool,
    raw_body: bytes::Bytes,
    content_type: String,
    trace: &TraceContext,
    request_id: &str,
    client_headers: &http::HeaderMap,
    api_key_id: &str,
) -> Result<(Response, Option<u64>), AppError> {
    let mut upstream_headers = http::HeaderMap::new();
    merge_client_headers(
        client_headers,
        &mut upstream_headers,
        &state.tunables().header_policy,
    );
    apply_provider_auth(target, &mut upstream_headers, &state.oauth_manager).await?;

    // TODO(prompt-cache): multipart re-encoding is not implemented in
    // v1, so prompt_cache_key cannot be injected for edits requests.
    // The virtual→upstream model mapping is also effectively ignored
    // for /v1/images/edits (model override requires multipart parsing).
    let _ = api_key_id;

    let upstream_url = format!("{}/images/edits", target.effective_api_base());
    let client = &state.tunables().http_client;

    if is_stream {
        let mut stream_req = crate::ingress::observability::inject_trace(
            client.post(&upstream_url).headers(upstream_headers),
            trace,
        );
        stream_req = stream_req
            .header("content-type", &content_type)
            .body(raw_body);
        let (egress_req, egress_headers_capture, egress_method, egress_path) =
            crate::ingress::observability::finalize_egress(stream_req)?;
        let exec_started = std::time::Instant::now();
        let response = execute_with_ttfb_timeout(
            client,
            egress_req,
            state.tunables().upstream_ttfb_timeout_secs,
        )
        .await?;
        let ttfb_ms = Some(exec_started.elapsed().as_millis() as u64);

        let retry_after = extract_retry_after(response.headers());
        let rate_limit_headers_vec: Vec<(&'static str, String)> =
            extract_rate_limit_headers(response.headers());
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
                    egress_body: None,
                    upstream_status: Some(upstream_status_capture),
                    upstream_resp_headers: upstream_resp_headers_capture.clone(),
                    upstream_resp_body: Some(error_body.clone()),
                    client_resp_headers: Vec::new(),
                    client_resp_body: None,
                    is_stream: true,
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
                },
            );
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!("Upstream {status}: {error_body}"),
            );
            app_err.upstream_status = Some(status.as_u16());
            app_err = app_err.with_class(tiygate_core::classify_upstream_error(
                Some(status.as_u16()),
                None,
            ));
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            app_err.rate_limit_headers = rate_limit_headers_vec;
            return Err(app_err);
        }

        let accum =
            std::sync::Arc::new(std::sync::Mutex::new(tiygate_core::UsageAccumulator::new()));

        // Use the images stream encoder for error/done markers.
        let images_codec = tiygate_protocols::images::ImagesEditsCodec::new();
        let mut end_enc = images_codec.stream_encoder();
        let mut err_enc = images_codec.stream_encoder();
        let end_marker = end_enc.encode_done();
        let error_marker = err_enc.encode_error(
            "upstream stream truncated by gateway",
            tiygate_core::ErrorClass::DeadlineExceeded,
            None,
        );

        let forwarded_resp_headers = forwarded_resp_headers_for_capture(
            &upstream_resp_headers_capture,
            &state.tunables().header_policy,
            request_id,
        );
        let upstream_resp_headers_for_forward = upstream_resp_headers_capture.clone();
        // No protocol transcode; the stream driver still normalizes the
        // client-facing model field.
        let mut response = drive_upstream_stream(
            state,
            accum,
            Box::pin(
                response
                    .bytes_stream()
                    .map(|result| result.map_err(|error| error.to_string())),
            ),
            end_marker,
            error_marker,
            Duration::from_secs(state.tunables().upstream_stream_idle_timeout_secs),
            Duration::from_secs(state.tunables().upstream_stream_total_timeout_secs),
            DEFAULT_SSE_KEEPALIVE_INTERVAL,
            Some(StreamCapture {
                request_id: request_id.to_string(),
                telemetry: state.telemetry.clone(),
                health: Some(state.health.clone()),
                health_key: Some(target.health_key()),
                egress_method: egress_method.to_string(),
                egress_path: egress_path.to_string(),
                egress_headers: egress_headers_capture,
                egress_body: None,
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture,
                client_resp_headers: forwarded_resp_headers,
            }),
            None,
            Some(ResponseModelOverride::new(
                tiygate_core::ProtocolSuite::OpenAiCompatible,
                virtual_model,
            )),
        );
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_for_forward,
            &state.tunables().header_policy,
            request_id,
        );
        if let Some(ra) = retry_after {
            response.headers_mut().insert(
                http::HeaderName::from_static("retry-after"),
                http::HeaderValue::from_str(&ra).unwrap_or(http::HeaderValue::from_static("")),
            );
        }
        for (name, value) in extract_rate_limit_headers(response.headers()) {
            if let Ok(hv) = http::HeaderValue::from_str(&value) {
                if let Ok(hn) = http::HeaderName::from_bytes(name.as_bytes()) {
                    response.headers_mut().insert(hn, hv);
                }
            }
        }
        Ok((response, ttfb_ms))
    } else {
        let mut nonstream_req = crate::ingress::observability::inject_trace(
            client
                .post(&upstream_url)
                .headers(upstream_headers)
                .timeout(IMAGES_NONSTREAM_TIMEOUT),
            trace,
        );
        nonstream_req = nonstream_req
            .header("content-type", &content_type)
            .body(raw_body);
        let (egress_req, egress_headers_capture, egress_method, egress_path) =
            crate::ingress::observability::finalize_egress(nonstream_req)?;
        let exec_started = std::time::Instant::now();
        let response = client
            .execute(egress_req)
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")))?;
        let ttfb_ms = Some(exec_started.elapsed().as_millis() as u64);

        let retry_after = extract_retry_after(response.headers());
        let rate_limit_headers_vec: Vec<(&'static str, String)> =
            extract_rate_limit_headers(response.headers());
        let status = response.status();
        let upstream_resp_headers_capture = reqwest_headers_to_vec(response.headers());
        let upstream_status_capture = status.as_u16();
        let response_text = response
            .text()
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Read error: {e}")))?;
        let response_body: Value = serde_json::from_str(&response_text)
            .unwrap_or_else(|_| json!({"error": {"message": response_text}}));

        if !status.is_success() {
            spawn_capture(
                state,
                tiygate_core::ExchangeCapture {
                    request_id: request_id.to_string(),
                    egress_method: egress_method.clone(),
                    egress_path: egress_path.clone(),
                    egress_headers: egress_headers_capture.clone(),
                    egress_body: None,
                    upstream_status: Some(upstream_status_capture),
                    upstream_resp_headers: upstream_resp_headers_capture.clone(),
                    upstream_resp_body: serde_json::to_string(&response_body).ok(),
                    client_resp_headers: Vec::new(),
                    client_resp_body: None,
                    is_stream: false,
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
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
            app_err = app_err.with_class(tiygate_core::classify_upstream_error(
                Some(status.as_u16()),
                None,
            ));
            if let Some(c) = response_body["error"]["code"]
                .as_str()
                .or_else(|| response_body["error"]["type"].as_str())
            {
                app_err = app_err.with_upstream_code(c);
            }
            if let Some(ra) = retry_after {
                app_err = app_err.with_retry_after_header(ra);
            }
            app_err.rate_limit_headers = rate_limit_headers_vec;
            return Err(app_err);
        }

        // Detect HTTP 200 responses that are actually error responses
        // (top-level `"error"` key, e.g. service_unavailable_error).
        if let Some(app_err) = check_nonstream_error_body(
            &response_body,
            status.as_u16(),
            retry_after.clone(),
            rate_limit_headers_vec.clone(),
        ) {
            spawn_capture(
                state,
                tiygate_core::ExchangeCapture {
                    request_id: request_id.to_string(),
                    egress_method: egress_method.clone(),
                    egress_path: egress_path.clone(),
                    egress_headers: egress_headers_capture.clone(),
                    egress_body: None,
                    upstream_status: Some(upstream_status_capture),
                    upstream_resp_headers: upstream_resp_headers_capture.clone(),
                    upstream_resp_body: serde_json::to_string(&response_body).ok(),
                    client_resp_headers: Vec::new(),
                    client_resp_body: None,
                    is_stream: false,
                    truncation_reason: None,
                    stream_duration_ms: None,
                    upstream_error: None,
                    upstream_error_class: None,
                },
            );
            return Err(app_err);
        }

        let upstream_resp_body_capture = serde_json::to_string(&response_body).ok();
        let mut client_response_body = response_body;
        ResponseModelOverride::new(tiygate_core::ProtocolSuite::OpenAiCompatible, virtual_model)
            .apply_json(&mut client_response_body);
        let client_resp_body_capture = serde_json::to_string(&client_response_body).ok();
        let mut response = Json(client_response_body).into_response();
        forward_upstream_resp_headers(
            &mut response,
            &upstream_resp_headers_capture,
            &state.tunables().header_policy,
            request_id,
        );
        if let Some(ra) = retry_after {
            response.headers_mut().insert(
                http::HeaderName::from_static("retry-after"),
                http::HeaderValue::from_str(&ra)
                    .unwrap_or_else(|_| http::HeaderValue::from_static("")),
            );
        }
        for (name, value) in extract_rate_limit_headers(response.headers()) {
            if let Ok(hv) = http::HeaderValue::from_str(&value) {
                if let Ok(hn) = http::HeaderName::from_bytes(name.as_bytes()) {
                    response.headers_mut().insert(hn, hv);
                }
            }
        }
        spawn_capture(
            state,
            tiygate_core::ExchangeCapture {
                request_id: request_id.to_string(),
                egress_method: egress_method.to_string(),
                egress_path: egress_path.to_string(),
                egress_headers: egress_headers_capture,
                egress_body: None,
                upstream_status: Some(upstream_status_capture),
                upstream_resp_headers: upstream_resp_headers_capture,
                upstream_resp_body: upstream_resp_body_capture,
                client_resp_headers: header_map_to_vec(response.headers()),
                client_resp_body: client_resp_body_capture,
                is_stream: false,
                truncation_reason: None,
                stream_duration_ms: None,
                upstream_error: None,
                upstream_error_class: None,
            },
        );
        Ok((response, ttfb_ms))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};

    #[test]
    fn check_nonstream_error_body_detects_pure_error() {
        let body = json!({
            "error": {
                "type": "service_unavailable_error",
                "message": "Service unavailable"
            }
        });
        let result = check_nonstream_error_body(&body, 200, None, Vec::new());
        assert!(result.is_some(), "should detect error body");
        let err = result.unwrap();
        assert!(err.message.contains("Service unavailable"));
        assert_eq!(err.upstream_status, Some(200));
    }

    #[test]
    fn check_nonstream_error_body_not_flagged_with_choices() {
        let body = json!({
            "choices": [{"message": {"content": "ok"}}],
            "error": {"type": "minor_warning", "message": "rate limit warning"}
        });
        let result = check_nonstream_error_body(&body, 200, None, Vec::new());
        assert!(result.is_none(), "should not flag when choices present");
    }

    #[test]
    fn check_nonstream_error_body_not_flagged_without_error_key() {
        let body = json!({
            "choices": [{"message": {"content": "hello"}}]
        });
        let result = check_nonstream_error_body(&body, 200, None, Vec::new());
        assert!(result.is_none());
    }

    #[test]
    fn check_nonstream_error_body_preserves_retry_after() {
        let body = json!({
            "error": {"type": "rate_limit", "message": "Too many requests"}
        });
        let result = check_nonstream_error_body(&body, 429, Some("30".to_string()), Vec::new());
        assert!(result.is_some());
        let err = result.unwrap();
        assert_eq!(err.retry_after_header, Some("30".to_string()));
    }

    #[test]
    fn codex_websocket_url_preserves_responses_path() {
        let result =
            codex_websocket_url("https://chatgpt.com/backend-api/codex/responses?foo=bar").unwrap();
        assert_eq!(
            result,
            "wss://chatgpt.com/backend-api/codex/responses?foo=bar"
        );
        assert_eq!(
            codex_websocket_url("http://127.0.0.1:8080/responses").unwrap(),
            "ws://127.0.0.1:8080/responses"
        );
        assert!(codex_websocket_url("ftp://example.test/responses").is_err());
    }

    #[test]
    fn codex_response_create_preserves_responses_payload() {
        let event = codex_response_create(json!({
            "model": "gpt-5-codex",
            "input": [{"role": "user", "content": "hello"}],
            "additional_tools": [{"type": "computer"}],
            "type": "untrusted.client.command"
        }))
        .unwrap();

        assert_eq!(event["type"], "response.create");
        assert_eq!(event["model"], "gpt-5-codex");
        assert_eq!(event["additional_tools"][0]["type"], "computer");
    }

    #[test]
    fn codex_http_request_is_normalized_for_reference_contract() {
        let mut body = json!({
            "model": "gpt-5.6",
            "stream": false,
            "instructions": null,
            "previous_response_id": "resp-old",
            "stream_options": {"include_usage": true},
            "prompt_cache_retention": "24h",
            "safety_identifier": "user"
        });
        assert!(normalize_codex_request(&mut body, false));
        assert_eq!(body["stream"], true);
        assert_eq!(body["instructions"], "");
        assert!(body.get("previous_response_id").is_none());
        assert!(body.get("stream_options").is_none());
        assert!(body.get("prompt_cache_retention").is_none());
        assert!(body.get("safety_identifier").is_none());
    }

    #[test]
    fn codex_http_sse_returns_completed_response() {
        let body = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-1\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"status\":\"completed\"}}\n\n"
        );
        let response = parse_codex_http_response(body).unwrap();
        assert_eq!(response["id"], "resp-1");
        assert_eq!(response["status"], "completed");
    }

    #[test]
    fn codex_desktop_headers_receive_session_id() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("Codex Desktop/1 (Mac OS 26; arm64)"),
        );
        ensure_codex_request_headers(&mut headers);
        assert!(headers.contains_key("session_id"));
    }

    #[test]
    fn codex_websocket_normalizes_all_terminal_variants() {
        let (completed, is_terminal) = normalize_codex_websocket_event(
            json!({"type": "response.done", "response": {"id": "resp-test"}}).to_string(),
        );
        assert!(is_terminal);
        assert_eq!(
            serde_json::from_str::<Value>(&completed).unwrap()["type"],
            "response.completed"
        );
        let (_, is_terminal) =
            normalize_codex_websocket_event(json!({"type": "error"}).to_string());
        assert!(is_terminal);
    }

    #[test]
    fn codex_handshake_keeps_auth_and_client_request_id() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer test-token"),
        );
        headers.insert(
            http::HeaderName::from_static("x-client-request-id"),
            http::HeaderValue::from_static("client-request-id"),
        );
        headers.insert(
            http::HeaderName::from_static("openai-beta"),
            http::HeaderValue::from_static(CODEX_RESPONSES_WEBSOCKET_BETA),
        );

        let request = codex_websocket_handshake_request(
            "wss://chatgpt.com/backend-api/codex/responses",
            &headers,
        )
        .unwrap();
        assert_eq!(request.method(), http::Method::GET);
        assert_eq!(request.uri().path(), "/backend-api/codex/responses");
        assert_eq!(
            request.headers().get(http::header::AUTHORIZATION).unwrap(),
            "Bearer test-token"
        );
        assert_eq!(
            request.headers().get("x-client-request-id").unwrap(),
            "client-request-id"
        );
        assert_eq!(
            request.headers().get("openai-beta").unwrap(),
            CODEX_RESPONSES_WEBSOCKET_BETA
        );
    }

    #[tokio::test]
    async fn codex_websocket_events_are_normalized_to_sse() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (command_tx, command_rx) = tokio::sync::oneshot::channel();
        let (close_tx, close_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let command = socket.next().await.unwrap().unwrap().into_text().unwrap();
            let _ = command_tx.send(command.to_string());
            socket
                .send(Message::Text(
                    json!({"type": "response.created", "response": {"id": "resp-test"}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            socket
                .send(Message::Text(
                    json!({
                        "type": "response.done",
                        "response": {"id": "resp-test", "status": "completed"}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let saw_client_close = matches!(
                tokio::time::timeout(Duration::from_secs(1), socket.next()).await,
                Ok(Some(Ok(Message::Close(_))))
            );
            let _ = close_tx.send(saw_client_close);
        });

        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::HeaderName::from_static("openai-beta"),
            http::HeaderValue::from_static(CODEX_RESPONSES_WEBSOCKET_BETA),
        );
        headers.insert(
            http::HeaderName::from_static("x-client-request-id"),
            http::HeaderValue::from_static("client-request-id"),
        );
        let request = codex_websocket_handshake_request(
            &format!("ws://{address}/backend-api/codex/responses"),
            &headers,
        )
        .unwrap();
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let command = codex_response_create(json!({
            "model": "gpt-5-codex",
            "additional_tools": [{"type": "computer"}]
        }))
        .unwrap();
        socket
            .send(Message::Text(
                serde_json::to_string(&command).unwrap().into(),
            ))
            .await
            .unwrap();

        let stream_started = std::time::Instant::now();
        let frames = websocket_event_stream(socket)
            .collect::<Vec<Result<Bytes, String>>>()
            .await;
        let frames: Vec<String> = frames
            .into_iter()
            .map(|frame| String::from_utf8(frame.unwrap().to_vec()).unwrap())
            .collect();
        let sent_command: Value = serde_json::from_str(&command_rx.await.unwrap()).unwrap();
        let saw_client_close = close_rx.await.unwrap();
        server.await.unwrap();

        assert!(stream_started.elapsed() < Duration::from_millis(500));
        assert!(saw_client_close);
        assert_eq!(sent_command["type"], "response.create");
        assert_eq!(sent_command["additional_tools"][0]["type"], "computer");
        assert_eq!(
            frames,
            vec![
                "data: {\"response\":{\"id\":\"resp-test\"},\"type\":\"response.created\"}\n\n",
                "data: {\"response\":{\"id\":\"resp-test\",\"status\":\"completed\"},\"type\":\"response.completed\"}\n\n",
            ]
        );
    }

    #[tokio::test]
    async fn dropping_codex_websocket_stream_closes_upstream() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (close_tx, close_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let saw_client_close = matches!(
                tokio::time::timeout(Duration::from_secs(1), socket.next()).await,
                Ok(Some(Ok(Message::Close(_))))
            );
            let _ = close_tx.send(saw_client_close);
        });

        let request = codex_websocket_handshake_request(
            &format!("ws://{address}/backend-api/codex/responses"),
            &http::HeaderMap::new(),
        )
        .unwrap();
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        drop(websocket_event_stream(socket));

        assert!(close_rx.await.unwrap());
        server.await.unwrap();
    }
}
