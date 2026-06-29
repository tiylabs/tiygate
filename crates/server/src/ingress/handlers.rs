//! Route handlers for each ingress protocol.

use std::time::Instant;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use serde_json::Value;

use tiygate_core::{EndpointCodec, PipelineContext};
use tiygate_protocols::chat_completions::ChatCompletionsCodec;
use tiygate_protocols::embeddings::EmbeddingsCodec;
use tiygate_protocols::gemini::GeminiCodec;
use tiygate_protocols::images::{ImagesEditsCodec, ImagesGenerationsCodec};
use tiygate_protocols::messages::MessagesCodec;
use tiygate_protocols::responses::ResponsesCodec;

use super::executors::{
    execute_embeddings_upstream, execute_gemini_upstream, execute_images_edits_upstream,
    execute_images_generations_upstream, execute_messages_upstream, execute_responses_upstream,
    execute_upstream,
};
use super::fallback::{execute_with_fallback, FallbackOutcome};
use super::{compute_pass_through, enforce_body_limit, AppError, AppState};
use tiygate_core::telemetry::RequestErrorClass;

/// Health check — always returns 200 while process is alive.
pub(super) async fn handle_healthz() -> StatusCode {
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
pub(super) fn split_gemini_capture(capture: &str) -> Option<(&str, &str)> {
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
pub(super) fn strip_gemini_method_suffix(captured: &str) -> &str {
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
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::items_after_test_module
)]
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
pub(super) async fn handle_readyz() -> StatusCode {
    if crate::drain::global_drain_signalled() {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}

/// Acquire a concurrency permit, waiting up to acquire_timeout.
/// Returns 503 if the semaphore is exhausted beyond queue depth.
pub(super) async fn acquire_permit(
    state: &AppState,
) -> Result<tokio::sync::OwnedSemaphorePermit, AppError> {
    // Check queue depth before waiting
    let tunables = state.tunables();
    let available = state.concurrency_semaphore.available_permits();
    let waiting = tunables.max_inflight.saturating_sub(available);
    if waiting > tunables.max_queue_depth {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "gateway overloaded, queue full".to_string(),
        )
        .with_retry_after(5));
    }

    match tokio::time::timeout(
        tunables.acquire_timeout,
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

fn early_error_class(err: &AppError) -> RequestErrorClass {
    match err.http_status() {
        StatusCode::BAD_REQUEST
        | StatusCode::PAYLOAD_TOO_LARGE
        | StatusCode::UNSUPPORTED_MEDIA_TYPE => RequestErrorClass::BadRequest,
        StatusCode::SERVICE_UNAVAILABLE | StatusCode::TOO_MANY_REQUESTS => {
            RequestErrorClass::Transient
        }
        _ => RequestErrorClass::InternalError,
    }
}

fn emit_early_error(scope: crate::ingress::observability::RequestScope<'_>, err: &AppError) {
    scope.emit_error(
        early_error_class(err),
        Some(&err.message),
        Some(err.http_status().as_u16()),
    );
}

async fn acquire_or_log<'a>(
    state: &'a AppState,
    scope: crate::ingress::observability::RequestScope<'a>,
) -> Result<
    (
        tokio::sync::OwnedSemaphorePermit,
        crate::ingress::observability::RequestScope<'a>,
    ),
    AppError,
> {
    match acquire_permit(state).await {
        Ok(permit) => Ok((permit, scope)),
        Err(err) => {
            emit_early_error(scope, &err);
            Err(err)
        }
    }
}

fn enforce_body_limit_or_log<'a>(
    state: &AppState,
    scope: crate::ingress::observability::RequestScope<'a>,
    content_type: Option<&str>,
    body_size: u64,
) -> Result<crate::ingress::observability::RequestScope<'a>, AppError> {
    match enforce_body_limit(state, content_type, body_size) {
        Ok(()) => Ok(scope),
        Err(err) => {
            emit_early_error(scope, &err);
            Err(err)
        }
    }
}

/// Handle POST /v1/chat/completions.
pub(super) async fn handle_chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let codec = ChatCompletionsCodec::new();
    let ingress_protocol = codec.id().clone();

    // Per-route body-limit enforcement (text vs. multimodal).
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let body_size = serde_json::to_string(&body)
        .map(|s| s.len() as u64)
        .unwrap_or(0);

    // Wall-clock anchor for the Phase 4 `RequestEvent`. We measure
    // the *whole* request handler duration (including queueing and
    // fallback retries) so the latency column reflects what the
    // client actually experienced.
    let started = Instant::now();
    let request_id = uuid::Uuid::now_v7().to_string();

    let trace_ctx = crate::ingress::observability::extract_trace(&headers);
    let raw_env = crate::ingress::observability::build_redacted_envelope(
        &state,
        "POST",
        "/v1/chat/completions",
        &body,
        &headers,
    );

    // Build the RequestScope before permit acquisition and body-limit
    // checks so 503 overload and 413/415 pre-pipeline failures still
    // produce a terminal RequestEvent.
    let scope = crate::ingress::observability::RequestScope::new(
        &state,
        request_id.clone(),
        "unknown",
        ingress_protocol.clone(),
        trace_ctx.clone(),
        started,
    );
    let scope = {
        let mut scope = scope;
        scope.set_envelope(raw_env.clone());
        scope
    };
    let (_permit, scope) = acquire_or_log(&state, scope).await?;
    let mut scope = enforce_body_limit_or_log(&state, scope, content_type, body_size)?;

    // Phase 4 §4.6: api key resolution + quota enforcement. The
    // resolved `api_key` is bound to the scope so the terminal
    // RequestEvent attributes the row to the right caller.
    let api_key = crate::ingress::observability::resolve_api_key(&state, &headers).await;
    scope.set_api_key_id(api_key.key_id.clone());
    if let Err((err, class)) = crate::ingress::observability::enforce_auth(&state, &api_key) {
        scope.emit_error(class, Some(&err.message), Some(err.http_status().as_u16()));
        return Err(err);
    }
    match crate::ingress::observability::check_quota(&state, &api_key.key_id, &api_key.spec, 1)
        .await
    {
        crate::ingress::observability::QuotaOutcome::Allow => {}
        crate::ingress::observability::QuotaOutcome::Deny { retry_after, .. } => {
            let app_err =
                AppError::new(StatusCode::TOO_MANY_REQUESTS, "quota exceeded".to_string())
                    .with_retry_after(retry_after.as_secs().max(1));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::QuotaExceeded,
                Some(&app_err.message),
                Some(http_status),
            );
            return Err(app_err);
        }
    }

    // Capture the original body string for passthrough before
    // `decode_request` moves `body`.
    let original_body_str = serde_json::to_string(&body).unwrap_or_default();

    // Decode request
    let ir_request = match codec.decode_request(body, &raw_env) {
        Ok(r) => r,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_REQUEST, format!("Decode error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::DecodeError,
                Some(&app_err.message),
                Some(http_status),
            );
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
            scope.emit_error(
                RequestErrorClass::RouteNotFound,
                Some(&app_err.message),
                Some(http_status),
            );
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
        compute_pass_through(&codec, &ingress_protocol, &targets, &original_body_str);

    // Delegate to the unified fallback / circuit-breaker / retry loop.
    scope.mark_waiting_upstream();
    let outcome = execute_with_fallback(
        &state,
        &mut scope,
        &targets,
        &virtual_model,
        &request_id,
        |target| {
            Box::pin(execute_upstream(
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
                &api_key.key_id,
            ))
        },
    )
    .await;

    match outcome {
        FallbackOutcome::Success { response, .. } => {
            scope.emit_ok(Some(response.status().as_u16()));
            Ok(response)
        }
        FallbackOutcome::Failed { error, error_class } => {
            let http_status = error.http_status().as_u16();
            scope.emit_error(error_class, Some(&error.message), Some(http_status));
            Err(error)
        }
        FallbackOutcome::Exhausted { error } => {
            let http_status = error.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::UpstreamExhausted,
                Some(&error.message),
                Some(http_status),
            );
            Err(error)
        }
    }
}

/// Handle POST /v1/messages (Anthropic protocol).
pub(super) async fn handle_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let codec = MessagesCodec::new();
    let ingress_protocol = codec.id().clone();

    // Wall-clock anchor for the Phase 4 `RequestEvent`.
    let started = Instant::now();
    let request_id = uuid::Uuid::now_v7().to_string();

    let trace_ctx = crate::ingress::observability::extract_trace(&headers);
    let raw_env = crate::ingress::observability::build_redacted_envelope(
        &state,
        "POST",
        "/v1/messages",
        &body,
        &headers,
    );

    // Build the RequestScope before permit acquisition so 503
    // overload failures still produce a terminal RequestEvent.
    let scope = crate::ingress::observability::RequestScope::new(
        &state,
        request_id.clone(),
        "unknown",
        ingress_protocol.clone(),
        trace_ctx.clone(),
        started,
    );
    let scope = {
        let mut scope = scope;
        scope.set_envelope(raw_env.clone());
        scope
    };
    let (_permit, mut scope) = acquire_or_log(&state, scope).await?;

    // Phase 4 §4.6: api key resolution + quota enforcement (parity
    // with the chat-completions path). The resolved `api_key` is
    // bound to the scope so the terminal `RequestEvent` attributes
    // the row to the right caller.
    let api_key = crate::ingress::observability::resolve_api_key(&state, &headers).await;
    scope.set_api_key_id(api_key.key_id.clone());
    if let Err((err, class)) = crate::ingress::observability::enforce_auth(&state, &api_key) {
        scope.emit_error(class, Some(&err.message), Some(err.http_status().as_u16()));
        return Err(err);
    }
    match crate::ingress::observability::check_quota(&state, &api_key.key_id, &api_key.spec, 1)
        .await
    {
        crate::ingress::observability::QuotaOutcome::Allow => {}
        crate::ingress::observability::QuotaOutcome::Deny { retry_after, .. } => {
            let app_err =
                AppError::new(StatusCode::TOO_MANY_REQUESTS, "quota exceeded".to_string())
                    .with_retry_after(retry_after.as_secs().max(1));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::QuotaExceeded,
                Some(&app_err.message),
                Some(http_status),
            );
            return Err(app_err);
        }
    }

    let original_body_str = serde_json::to_string(&body).unwrap_or_default();

    let ir_request = match codec.decode_request(body, &raw_env) {
        Ok(r) => r,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_REQUEST, format!("Decode error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::DecodeError,
                Some(&app_err.message),
                Some(http_status),
            );
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
            scope.emit_error(
                RequestErrorClass::RouteNotFound,
                Some(&app_err.message),
                Some(http_status),
            );
            return Err(app_err);
        }
    };

    // PassThrough: forward raw body bytes verbatim when the target's
    // protocol suite matches the ingress suite.
    let (_pass_through, raw_passthrough_body) =
        compute_pass_through(&codec, &ingress_protocol, &targets, &original_body_str);

    // Delegate to the unified fallback / circuit-breaker / retry loop.
    scope.mark_waiting_upstream();
    let outcome = execute_with_fallback(
        &state,
        &mut scope,
        &targets,
        &virtual_model,
        &request_id,
        |target| {
            Box::pin(execute_messages_upstream(
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
                &api_key.key_id,
            ))
        },
    )
    .await;

    match outcome {
        FallbackOutcome::Success { response, .. } => {
            scope.emit_ok(Some(response.status().as_u16()));
            Ok(response)
        }
        FallbackOutcome::Failed { error, error_class } => {
            let http_status = error.http_status().as_u16();
            scope.emit_error(error_class, Some(&error.message), Some(http_status));
            Err(error)
        }
        FallbackOutcome::Exhausted { error } => {
            let http_status = error.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::UpstreamExhausted,
                Some(&error.message),
                Some(http_status),
            );
            Err(error)
        }
    }
}

/// Execute an upstream OpenAI-compatible request.
///    `latency_ms` populated.
pub(super) async fn handle_embeddings(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let codec = EmbeddingsCodec::new();
    let ingress_protocol = codec.id().clone();
    let raw_env = crate::ingress::observability::build_redacted_envelope(
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
    let trace_ctx = crate::ingress::observability::extract_trace(&headers);

    // Wall-clock anchor + scope so every return path emits a
    // terminal `RequestEvent` (parity with the other handlers).
    // The `started` clock is also used for the `latency_ms` column
    // on the miss / hit events below.
    let started = Instant::now();
    let request_id = uuid::Uuid::now_v7().to_string();
    let mut scope = crate::ingress::observability::RequestScope::new(
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
    let (_permit, mut scope) = acquire_or_log(&state, scope).await?;

    // Phase 4 §4.6: api key resolution + quota enforcement, parity
    // with the chat/messages/responses/gemini handlers. Embedding
    // requests count against the same `requests_per_minute` /
    // `requests_per_day` bucket as chat completions.
    let api_key = crate::ingress::observability::resolve_api_key(&state, &headers).await;
    scope.set_api_key_id(api_key.key_id.clone());
    if let Err((err, class)) = crate::ingress::observability::enforce_auth(&state, &api_key) {
        scope.emit_error(class, Some(&err.message), Some(err.http_status().as_u16()));
        return Err(err);
    }
    match crate::ingress::observability::check_quota(&state, &api_key.key_id, &api_key.spec, 1)
        .await
    {
        crate::ingress::observability::QuotaOutcome::Allow => {}
        crate::ingress::observability::QuotaOutcome::Deny { retry_after, .. } => {
            let app_err =
                AppError::new(StatusCode::TOO_MANY_REQUESTS, "quota exceeded".to_string())
                    .with_retry_after(retry_after.as_secs().max(1));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::QuotaExceeded,
                Some(&app_err.message),
                Some(http_status),
            );
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
    if let Some(cached) =
        crate::ingress::observability::embedding_cache_lookup(&state, &cache_key).await
    {
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
        crate::ingress::observability::emit_request_event(
            &state,
            scope.request_id(),
            &model_for_cache,
            None,
            None,
            codec.id(),
            None,
            tiygate_core::telemetry::RequestStatus::Success,
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
            scope.emit_error(
                RequestErrorClass::DecodeError,
                Some(&app_err.message),
                Some(http_status),
            );
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
            scope.emit_error(
                RequestErrorClass::RouteNotFound,
                Some(&app_err.message),
                Some(http_status),
            );
            return Err(app_err);
        }
    };

    // Delegate to the unified fallback / circuit-breaker / retry loop.
    let request_id = scope.request_id().to_string();
    scope.mark_waiting_upstream();
    let outcome = execute_with_fallback(
        &state,
        &mut scope,
        &targets,
        &virtual_model,
        &request_id,
        |target| {
            let cache_key = cache_key.clone();
            Box::pin(execute_embeddings_upstream(
                &state,
                &codec,
                &ir_request,
                target,
                &trace_ctx,
                &request_id,
                &headers,
                cache_key,
            ))
        },
    )
    .await;

    match outcome {
        FallbackOutcome::Success { response, .. } => {
            scope.emit_ok(Some(response.status().as_u16()));
            Ok(response)
        }
        FallbackOutcome::Failed { error, error_class } => {
            let http_status = error.http_status().as_u16();
            scope.emit_error(error_class, Some(&error.message), Some(http_status));
            Err(error)
        }
        FallbackOutcome::Exhausted { error } => {
            let http_status = error.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::UpstreamExhausted,
                Some(&error.message),
                Some(http_status),
            );
            Err(error)
        }
    }
}

/// Handle POST /v1/responses — OpenAI Responses API.
///
/// Mirrors `handle_chat_completions` but uses `ResponsesCodec`. The
/// egress pipeline is the same: per-route body limit, route resolve,
/// fallback / retry, RateLimit-* passthrough. A `RequestScope` drop
/// guard ensures the terminal `RequestEvent` is emitted on every
/// return path (success, upstream error, decode / route / encode
/// failure).
pub(super) async fn handle_responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let codec = ResponsesCodec::new();
    let ingress_protocol = codec.id().clone();
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let body_size = serde_json::to_string(&body)
        .map(|s| s.len() as u64)
        .unwrap_or(0);

    let trace_ctx = crate::ingress::observability::extract_trace(&headers);
    let raw_env = crate::ingress::observability::build_redacted_envelope(
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
    let mut scope = crate::ingress::observability::RequestScope::new(
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
    let (_permit, scope) = acquire_or_log(&state, scope).await?;
    let mut scope = enforce_body_limit_or_log(&state, scope, content_type, body_size)?;
    // Bind the api key id so the terminal RequestEvent attributes the
    // row to the right caller (used by the per-key quota dashboard).
    let api_key = crate::ingress::observability::resolve_api_key(&state, &headers).await;
    scope.set_api_key_id(api_key.key_id.clone());
    if let Err((err, class)) = crate::ingress::observability::enforce_auth(&state, &api_key) {
        scope.emit_error(class, Some(&err.message), Some(err.http_status().as_u16()));
        return Err(err);
    }
    // Phase 4 §4.6: quota enforcement on the request hot path.
    // Parity with the chat-completions / anthropic-messages paths.
    match crate::ingress::observability::check_quota(&state, &api_key.key_id, &api_key.spec, 1)
        .await
    {
        crate::ingress::observability::QuotaOutcome::Allow => {}
        crate::ingress::observability::QuotaOutcome::Deny { retry_after, .. } => {
            let app_err =
                AppError::new(StatusCode::TOO_MANY_REQUESTS, "quota exceeded".to_string())
                    .with_retry_after(retry_after.as_secs().max(1));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::QuotaExceeded,
                Some(&app_err.message),
                Some(http_status),
            );
            return Err(app_err);
        }
    }

    let original_body_str = serde_json::to_string(&body).unwrap_or_default();

    let ir_request = match codec.decode_request(body, &raw_env) {
        Ok(r) => r,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_REQUEST, format!("Decode error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::DecodeError,
                Some(&app_err.message),
                Some(http_status),
            );
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
            scope.emit_error(
                RequestErrorClass::RouteNotFound,
                Some(&app_err.message),
                Some(http_status),
            );
            return Err(app_err);
        }
    };

    let (_pass_through, raw_passthrough_body) =
        compute_pass_through(&codec, &ingress_protocol, &targets, &original_body_str);

    // Delegate to the unified fallback / circuit-breaker / retry loop.
    let request_id = scope.request_id().to_string();
    scope.mark_waiting_upstream();
    let outcome = execute_with_fallback(
        &state,
        &mut scope,
        &targets,
        &virtual_model,
        &request_id,
        |target| {
            Box::pin(execute_responses_upstream(
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
                &api_key.key_id,
            ))
        },
    )
    .await;

    match outcome {
        FallbackOutcome::Success { response, .. } => {
            scope.emit_ok(Some(response.status().as_u16()));
            Ok(response)
        }
        FallbackOutcome::Failed { error, error_class } => {
            let http_status = error.http_status().as_u16();
            scope.emit_error(error_class, Some(&error.message), Some(http_status));
            Err(error)
        }
        FallbackOutcome::Exhausted { error } => {
            let http_status = error.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::UpstreamExhausted,
                Some(&error.message),
                Some(http_status),
            );
            Err(error)
        }
    }
}

/// Handle POST /v1beta/models/:model/generateContent — Google Gemini.
///
/// Mirrors `handle_chat_completions` but uses `GeminiCodec`. A
/// `RequestScope` drop guard ensures the terminal `RequestEvent` is
/// emitted on every return path (success, upstream error, decode /
/// route / encode failure).
pub(super) async fn handle_gemini_generate(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(capture): axum::extract::Path<String>,
    Json(mut body): Json<Value>,
) -> Result<Response, AppError> {
    let codec = GeminiCodec::new();
    let ingress_protocol = codec.id().clone();
    let started = Instant::now();
    let request_id = uuid::Uuid::now_v7().to_string();
    let trace_ctx = crate::ingress::observability::extract_trace(&headers);
    let raw_path = format!("/v1beta/models/{capture}");
    let raw_env = crate::ingress::observability::build_redacted_envelope(
        &state, "POST", &raw_path, &body, &headers,
    );
    let mut scope = crate::ingress::observability::RequestScope::new(
        &state,
        request_id,
        "unknown",
        ingress_protocol.clone(),
        trace_ctx.clone(),
        started,
    );
    scope.set_envelope(raw_env.clone());

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
            let app_err = AppError::new(
                StatusCode::BAD_REQUEST,
                format!("Invalid Gemini path capture: {capture:?}"),
            );
            let http_status = app_err.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::BadRequest,
                Some(&app_err.message),
                Some(http_status),
            );
            return Err(app_err);
        }
    };
    let is_gemini_stream = method == "streamGenerateContent";
    let model = model.to_string();
    scope.set_virtual_model(model.clone());
    let (_permit, scope) = acquire_or_log(&state, scope).await?;
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let body_size = serde_json::to_string(&body)
        .map(|s| s.len() as u64)
        .unwrap_or(0);
    let mut scope = enforce_body_limit_or_log(&state, scope, content_type, body_size)?;
    // Bind the api key id so the terminal RequestEvent attributes the
    // row to the right caller (used by the per-key quota dashboard).
    let api_key = crate::ingress::observability::resolve_api_key(&state, &headers).await;
    scope.set_api_key_id(api_key.key_id.clone());
    if let Err((err, class)) = crate::ingress::observability::enforce_auth(&state, &api_key) {
        scope.emit_error(class, Some(&err.message), Some(err.http_status().as_u16()));
        return Err(err);
    }
    // Phase 4 §4.6: quota enforcement on the request hot path.
    // Parity with the chat-completions / anthropic-messages paths.
    match crate::ingress::observability::check_quota(&state, &api_key.key_id, &api_key.spec, 1)
        .await
    {
        crate::ingress::observability::QuotaOutcome::Allow => {}
        crate::ingress::observability::QuotaOutcome::Deny { retry_after, .. } => {
            let app_err =
                AppError::new(StatusCode::TOO_MANY_REQUESTS, "quota exceeded".to_string())
                    .with_retry_after(retry_after.as_secs().max(1));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::QuotaExceeded,
                Some(&app_err.message),
                Some(http_status),
            );
            return Err(app_err);
        }
    }

    let original_body_str = serde_json::to_string(&body).unwrap_or_default();

    // Inject the streaming flag from the URL method into the body so the
    // Gemini codec's `decode_request` (which reads `body["_stream"]`) can
    // pick it up. Standard Gemini clients do not send a `_stream` field —
    // streaming is encoded in the URL method (`:streamGenerateContent`).
    if is_gemini_stream {
        body["_stream"] = Value::Bool(true);
    }

    let ir_request = match codec.decode_request(body, &raw_env) {
        Ok(r) => r,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_REQUEST, format!("Decode error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::DecodeError,
                Some(&app_err.message),
                Some(http_status),
            );
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
            scope.emit_error(
                RequestErrorClass::RouteNotFound,
                Some(&app_err.message),
                Some(http_status),
            );
            return Err(app_err);
        }
    };

    let (_pass_through, raw_passthrough_body) =
        compute_pass_through(&codec, &ingress_protocol, &targets, &original_body_str);

    // Delegate to the unified fallback / circuit-breaker / retry loop.
    let request_id = scope.request_id().to_string();
    scope.mark_waiting_upstream();
    let outcome = execute_with_fallback(
        &state,
        &mut scope,
        &targets,
        &virtual_model,
        &request_id,
        |target| {
            Box::pin(execute_gemini_upstream(
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
                &api_key.key_id,
            ))
        },
    )
    .await;

    match outcome {
        FallbackOutcome::Success { response, .. } => {
            scope.emit_ok(Some(response.status().as_u16()));
            Ok(response)
        }
        FallbackOutcome::Failed { error, error_class } => {
            let http_status = error.http_status().as_u16();
            scope.emit_error(error_class, Some(&error.message), Some(http_status));
            Err(error)
        }
        FallbackOutcome::Exhausted { error } => {
            let http_status = error.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::UpstreamExhausted,
                Some(&error.message),
                Some(http_status),
            );
            Err(error)
        }
    }
}

/// Handle POST /v1/images/generations.
///
/// OpenAI Images generations endpoint. The request body is JSON and is
/// forwarded to the upstream provider in raw-body passthrough mode (no
/// IR round-trip) when the routing target shares the same protocol suite.
/// Model override is applied to the JSON body before forwarding.
pub(super) async fn handle_images_generations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, AppError> {
    let codec = ImagesGenerationsCodec::new();
    let ingress_protocol = codec.id().clone();

    // Per-route body-limit enforcement (text vs. multimodal).
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let body_size = serde_json::to_string(&body)
        .map(|s| s.len() as u64)
        .unwrap_or(0);

    let started = Instant::now();
    let request_id = uuid::Uuid::now_v7().to_string();

    let trace_ctx = crate::ingress::observability::extract_trace(&headers);
    let raw_env = crate::ingress::observability::build_redacted_envelope(
        &state,
        "POST",
        "/v1/images/generations",
        &body,
        &headers,
    );

    let mut scope = crate::ingress::observability::RequestScope::new(
        &state,
        request_id.clone(),
        "unknown",
        ingress_protocol.clone(),
        trace_ctx.clone(),
        started,
    );
    scope.set_envelope(raw_env.clone());
    let (_permit, scope) = acquire_or_log(&state, scope).await?;
    let mut scope = enforce_body_limit_or_log(&state, scope, content_type, body_size)?;

    let api_key = crate::ingress::observability::resolve_api_key(&state, &headers).await;
    scope.set_api_key_id(api_key.key_id.clone());
    if let Err((err, class)) = crate::ingress::observability::enforce_auth(&state, &api_key) {
        scope.emit_error(class, Some(&err.message), Some(err.http_status().as_u16()));
        return Err(err);
    }
    match crate::ingress::observability::check_quota(&state, &api_key.key_id, &api_key.spec, 1)
        .await
    {
        crate::ingress::observability::QuotaOutcome::Allow => {}
        crate::ingress::observability::QuotaOutcome::Deny { retry_after, .. } => {
            let app_err =
                AppError::new(StatusCode::TOO_MANY_REQUESTS, "quota exceeded".to_string())
                    .with_retry_after(retry_after.as_secs().max(1));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::QuotaExceeded,
                Some(&app_err.message),
                Some(http_status),
            );
            return Err(app_err);
        }
    }

    let original_body_str = serde_json::to_string(&body).unwrap_or_default();

    // Decode request — in passthrough mode this is only used to extract
    // the model name and stream flag for routing.
    let ir_request = match codec.decode_request(body, &raw_env) {
        Ok(r) => r,
        Err(e) => {
            let app_err = AppError::new(StatusCode::BAD_REQUEST, format!("Decode error: {e}"));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::DecodeError,
                Some(&app_err.message),
                Some(http_status),
            );
            return Err(app_err);
        }
    };

    let virtual_model = ir_request.model.clone();
    let is_stream = ir_request.stream;
    scope.set_virtual_model(virtual_model.clone());

    let targets = match state.current_config().routing_table.resolve(&virtual_model) {
        Some(t) => t,
        None => {
            let app_err = AppError::new(
                StatusCode::NOT_FOUND,
                format!("No route found for model: {virtual_model}"),
            );
            let http_status = app_err.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::RouteNotFound,
                Some(&app_err.message),
                Some(http_status),
            );
            return Err(app_err);
        }
    };

    let _ctx = PipelineContext::new(
        request_id.clone(),
        ir_request.clone(),
        Some(raw_env.clone()),
    );

    let (_pass_through_candidate, raw_passthrough_body) =
        compute_pass_through(&codec, &ingress_protocol, &targets, &original_body_str);

    scope.mark_waiting_upstream();
    let outcome = execute_with_fallback(
        &state,
        &mut scope,
        &targets,
        &virtual_model,
        &request_id,
        |target| {
            Box::pin(execute_images_generations_upstream(
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
                &api_key.key_id,
            ))
        },
    )
    .await;

    match outcome {
        FallbackOutcome::Success { response, .. } => {
            scope.emit_ok(Some(response.status().as_u16()));
            Ok(response)
        }
        FallbackOutcome::Failed { error, error_class } => {
            let http_status = error.http_status().as_u16();
            scope.emit_error(error_class, Some(&error.message), Some(http_status));
            Err(error)
        }
        FallbackOutcome::Exhausted { error } => {
            let http_status = error.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::UpstreamExhausted,
                Some(&error.message),
                Some(http_status),
            );
            Err(error)
        }
    }
}

/// Extract the `model` and `stream` form-field values from a multipart
/// body without a full multipart parser. Scans for `name="model"` and
/// `name="stream"` field headers, then reads the value that follows the
/// blank-line separator. Returns `(model, is_stream)`.
///
/// This is a best-effort lightweight extractor sufficient for routing
/// decisions; the raw body is forwarded verbatim regardless of these
/// values, so an extraction miss only degrades to `model=default`,
/// `stream=false`.
fn extract_multipart_fields(body: &[u8]) -> (String, bool) {
    let text = String::from_utf8_lossy(body);
    let mut model = String::from("default");
    let mut is_stream = false;

    for field_name in ["model", "stream"] {
        // Match `name="field_name"` (with or without quotes).
        let needle = format!("name=\"{field_name}\"");
        if let Some(idx) = text.find(&needle) {
            // Find the blank line (\r\n\r\n) that separates headers from value.
            let rest = &text[idx + needle.len()..];
            if let Some(header_end) = rest.find("\r\n\r\n") {
                let value_start = header_end + 4;
                let value_rest = &rest[value_start..];
                // Value ends at the next boundary (starts with --).
                let value = if let Some(boundary_idx) = value_rest.find("\r\n--") {
                    &value_rest[..boundary_idx]
                } else {
                    value_rest
                };
                if field_name == "model" {
                    model = value.to_string();
                } else if field_name == "stream" {
                    is_stream = value.eq_ignore_ascii_case("true");
                }
            }
        }
    }

    (model, is_stream)
}

/// Handle POST /v1/images/edits.
///
/// OpenAI Images edits endpoint. The request body is multipart/form-data
/// and is forwarded to the upstream provider as raw bytes (no model
/// override, no IR round-trip). The original Content-Type header
/// (including the multipart boundary) is preserved.
pub(super) async fn handle_images_edits(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let codec = ImagesEditsCodec::new();
    let ingress_protocol = codec.id().clone();

    // Per-route body-limit enforcement — multipart bodies use the
    // multimodal limit.
    let content_type = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());

    let started = Instant::now();
    let request_id = uuid::Uuid::now_v7().to_string();

    let trace_ctx = crate::ingress::observability::extract_trace(&headers);

    // Extract model and stream flag from the multipart body for routing.
    let (virtual_model, is_stream) = extract_multipart_fields(&body);

    // Build a redacted RawEnvelope — the multipart body is binary,
    // not JSON, so we do not store it in envelope.body (audit safety).
    // Headers are still redacted via the Redactor to scrub
    // Authorization and other sensitive headers.
    let raw_env = crate::ingress::observability::build_redacted_envelope_raw(
        &state,
        "POST",
        "/v1/images/edits",
        body.len() as u64,
        &headers,
    );

    let mut scope = crate::ingress::observability::RequestScope::new(
        &state,
        request_id.clone(),
        &virtual_model,
        ingress_protocol.clone(),
        trace_ctx.clone(),
        started,
    );
    scope.set_envelope(raw_env.clone());
    let (_permit, scope) = acquire_or_log(&state, scope).await?;
    let mut scope = enforce_body_limit_or_log(&state, scope, content_type, body.len() as u64)?;

    let api_key = crate::ingress::observability::resolve_api_key(&state, &headers).await;
    scope.set_api_key_id(api_key.key_id.clone());
    if let Err((err, class)) = crate::ingress::observability::enforce_auth(&state, &api_key) {
        scope.emit_error(class, Some(&err.message), Some(err.http_status().as_u16()));
        return Err(err);
    }
    match crate::ingress::observability::check_quota(&state, &api_key.key_id, &api_key.spec, 1)
        .await
    {
        crate::ingress::observability::QuotaOutcome::Allow => {}
        crate::ingress::observability::QuotaOutcome::Deny { retry_after, .. } => {
            let app_err =
                AppError::new(StatusCode::TOO_MANY_REQUESTS, "quota exceeded".to_string())
                    .with_retry_after(retry_after.as_secs().max(1));
            let http_status = app_err.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::QuotaExceeded,
                Some(&app_err.message),
                Some(http_status),
            );
            return Err(app_err);
        }
    }

    let targets = match state.current_config().routing_table.resolve(&virtual_model) {
        Some(t) => t,
        None => {
            let app_err = AppError::new(
                StatusCode::NOT_FOUND,
                format!("No route found for model: {virtual_model}"),
            );
            let http_status = app_err.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::RouteNotFound,
                Some(&app_err.message),
                Some(http_status),
            );
            return Err(app_err);
        }
    };

    // Images edits always uses raw-bytes passthrough (same suite). We do
    // not call compute_pass_through because the raw body lives in the
    // Bytes extractor, not in raw_envelope.body.
    let content_type_str = content_type.unwrap_or("multipart/form-data").to_string();

    scope.mark_waiting_upstream();
    let outcome = execute_with_fallback(
        &state,
        &mut scope,
        &targets,
        &virtual_model,
        &request_id,
        |target| {
            let body = body.clone();
            let content_type_str = content_type_str.clone();
            Box::pin(execute_images_edits_upstream(
                &state,
                target,
                is_stream,
                body,
                content_type_str,
                &trace_ctx,
                &request_id,
                &headers,
                &api_key.key_id,
            ))
        },
    )
    .await;

    match outcome {
        FallbackOutcome::Success { response, .. } => {
            scope.emit_ok(Some(response.status().as_u16()));
            Ok(response)
        }
        FallbackOutcome::Failed { error, error_class } => {
            let http_status = error.http_status().as_u16();
            scope.emit_error(error_class, Some(&error.message), Some(http_status));
            Err(error)
        }
        FallbackOutcome::Exhausted { error } => {
            let http_status = error.http_status().as_u16();
            scope.emit_error(
                RequestErrorClass::UpstreamExhausted,
                Some(&error.message),
                Some(http_status),
            );
            Err(error)
        }
    }
}
