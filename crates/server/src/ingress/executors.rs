//! Upstream executors and codec/URL builders for each protocol.

use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

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
    merge_client_headers, override_model_in_body, reqwest_headers_to_vec, spawn_capture,
};
use super::streaming::{
    drive_upstream_stream, StreamCapture, StreamTranscode, DEFAULT_SSE_KEEPALIVE_INTERVAL,
};
use super::{apply_provider_auth, AppError, AppState};

/// Non-streaming timeout for image generation/edit requests. Image
/// generation is significantly slower than text chat (typically 10–60s
/// upstream), so we use a dedicated budget that is independent of the
/// global `request_read_timeout` (which defaults to 30s for chat).
const IMAGES_NONSTREAM_TIMEOUT: Duration = Duration::from_secs(300);

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
    let mut app_err = AppError::new(
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
        format!("Upstream error: {}", message),
    );
    app_err.upstream_status = Some(status);
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
    maybe_inject_prompt_cache_key(&mut upstream_body, &egress_protocol.suite, api_key_id);

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
        let response = client.execute(egress_req).await.map_err(|e| {
            AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {}", e))
        })?;
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
            &state.tunables().header_policy,
            request_id,
        );
        let upstream_resp_headers_for_forward = upstream_resp_headers_capture.clone();
        let mut response = drive_upstream_stream(
            state,
            accum,
            response,
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
                },
            );
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
    let model_was_overridden = override_model_in_body(&mut upstream_body, &target.model_id);
    maybe_inject_prompt_cache_key(&mut upstream_body, &egress_protocol.suite, api_key_id);
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
        let response = client.execute(egress_req).await.map_err(|e| {
            AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {}", e))
        })?;
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
            &state.tunables().header_policy,
            request_id,
        );
        let upstream_resp_headers_for_forward = upstream_resp_headers_capture.clone();
        let mut response = drive_upstream_stream(
            state,
            accum,
            response,
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
/// the caller forwards bytes verbatim (zero-loss fast path). The egress codec
/// supplies the upstream decoder; the ingress codec supplies the client
/// encoder. Returns `None` (verbatim) if either codec is unavailable rather
/// than failing the request.
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
            },
        );
        return Err(app_err);
    }

    let body_str_capture = serde_json::to_string(&response_body).ok();
    let mut resp = Json(response_body.clone()).into_response();
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
            upstream_resp_body: body_str_capture.clone(),
            client_resp_headers: header_map_to_vec(resp.headers()),
            client_resp_body: body_str_capture,
            is_stream: false,
            truncation_reason: None,
            stream_duration_ms: None,
            upstream_error: None,
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

    let model_was_overridden = override_model_in_body(&mut upstream_body, &target.model_id);
    maybe_inject_prompt_cache_key(&mut upstream_body, &egress_protocol.suite, api_key_id);
    let pass_through_verbatim = is_pass_through && !model_was_overridden;
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
        let response = state
            .tunables()
            .http_client
            .execute(egress_req)
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")))?;
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

        let accum = std::sync::Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let mut end_enc = codec.stream_encoder();
        let mut err_enc = codec.stream_encoder();
        let end_marker = end_enc.encode_done();
        let error_marker = err_enc.encode_error(
            "upstream stream truncated by gateway",
            Some("upstream_timeout"),
        );

        let mut response = drive_upstream_stream(
            state,
            accum,
            response,
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
            },
        );
        return Err(app_err);
    }

    let upstream_resp_body_capture = serde_json::to_string(&response_body).ok();
    let response_body = if is_same_protocol {
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
        },
    );
    Ok((resp, ttfb_ms))
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

    let model_was_overridden = override_model_in_body(&mut upstream_body, &target.model_id);
    maybe_inject_prompt_cache_key(&mut upstream_body, &egress_protocol.suite, api_key_id);
    let pass_through_verbatim = is_pass_through && !model_was_overridden;
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
        let response = state
            .tunables()
            .http_client
            .execute(egress_req)
            .await
            .map_err(|e| AppError::new(StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")))?;
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

        let accum = std::sync::Arc::new(std::sync::Mutex::new(UsageAccumulator::new()));
        let mut end_enc = codec.stream_encoder();
        let mut err_enc = codec.stream_encoder();
        let end_marker = end_enc.encode_done();
        let error_marker = err_enc.encode_error(
            "upstream stream truncated by gateway",
            Some("upstream_timeout"),
        );

        let mut response = drive_upstream_stream(
            state,
            accum,
            response,
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
            },
        );
        return Err(app_err);
    }

    let upstream_resp_body_capture = serde_json::to_string(&response_body).ok();
    let response_body = if is_same_protocol {
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

    maybe_inject_prompt_cache_key(&mut upstream_body, &egress_protocol.suite, api_key_id);

    let model_was_overridden = override_model_in_body(&mut upstream_body, &target.model_id);
    let pass_through_verbatim = is_pass_through && !model_was_overridden;

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
                },
            );
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!("Upstream {status}: {error_body}"),
            );
            app_err.upstream_status = Some(status.as_u16());
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
            Some("upstream_timeout"),
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
            response,
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
                },
            );
            return Err(app_err);
        }

        // Cross-protocol re-encoding: when the egress suite differs
        // from the ingress suite, decode via the egress codec and
        // re-encode to the ingress protocol. Same-suite: forward
        // verbatim.
        let response_json = if is_same_protocol {
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
        let client_resp_body_capture = upstream_resp_body_capture.clone();
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
                },
            );
            let mut app_err = AppError::new(
                StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                format!("Upstream {status}: {error_body}"),
            );
            app_err.upstream_status = Some(status.as_u16());
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
            Some("upstream_timeout"),
        );

        let forwarded_resp_headers = forwarded_resp_headers_for_capture(
            &upstream_resp_headers_capture,
            &state.tunables().header_policy,
            request_id,
        );
        let upstream_resp_headers_for_forward = upstream_resp_headers_capture.clone();
        // No transcode — verbatim SSE passthrough.
        let mut response = drive_upstream_stream(
            state,
            accum,
            response,
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
                },
            );
            return Err(app_err);
        }

        let upstream_resp_body_capture = serde_json::to_string(&response_body).ok();
        let client_resp_body_capture = upstream_resp_body_capture.clone();
        let mut response = Json(response_body).into_response();
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
            },
        );
        Ok((response, ttfb_ms))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

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
}
