//! Header forwarding, extraction, and capture helpers for ingress.

use axum::http::HeaderMap;
use axum::response::Response;

use super::AppState;

/// Convert an `http::HeaderMap` into an ordered `Vec<(name, value)>`
/// for `ExchangeCapture`. Non-UTF8 header values are rendered lossily.
pub(super) fn header_map_to_vec(headers: &http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect()
}

/// Convert a reqwest response `HeaderMap` into an ordered Vec.
pub(super) fn reqwest_headers_to_vec(
    headers: &reqwest::header::HeaderMap,
) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect()
}

/// Merge client request headers into the upstream header map per the
/// denylist forwarding policy (C→G→P). Called *after* the codec / auth
/// have populated `upstream_headers` and *before* `apply_provider_auth`
/// runs, so a forwarded client header never overwrites a header the
/// gateway already set (codec content-type, etc.) and auth injection
/// always wins last. Headers blocked by the policy (credentials,
/// hop-by-hop, gateway-controlled, trace) are skipped.
pub(super) fn merge_client_headers(
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

pub(super) const GATEWAY_REQUEST_ID_HEADER: &str = "x-request-id";

pub(super) fn set_gateway_request_id_header(resp: &mut Response, request_id: &str) {
    if let Ok(hv) = http::HeaderValue::from_str(request_id) {
        resp.headers_mut()
            .insert(http::HeaderName::from_static(GATEWAY_REQUEST_ID_HEADER), hv);
    }
}

/// Forward upstream response headers to the client response per the
/// denylist forwarding policy (P→G→C). The upstream headers are passed
/// as the already-snapshotted `Vec<(name, value)>` (captured before the
/// reqwest response body/object is consumed). Headers blocked by the
/// policy (hop-by-hop, length/encoding, framework-controlled) are
/// skipped; everything else is inserted onto the client response. The
/// gateway request id always wins over any provider-supplied
/// `x-request-id`.
pub(super) fn forward_upstream_resp_headers(
    resp: &mut Response,
    upstream_headers: &[(String, String)],
    policy: &tiygate_core::HeaderForwardPolicy,
    request_id: &str,
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
    set_gateway_request_id_header(resp, request_id);
}

/// Filter a snapshotted upstream response header list down to the set
/// that is actually forwarded to the client, for the request-log
/// `client_resp_headers` capture on the streaming path.
pub(super) fn forwarded_resp_headers_for_capture(
    upstream_headers: &[(String, String)],
    policy: &tiygate_core::HeaderForwardPolicy,
    request_id: &str,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = upstream_headers
        .iter()
        .filter(|(name, _)| policy.should_forward_response(name))
        .filter(|(name, _)| !name.eq_ignore_ascii_case(GATEWAY_REQUEST_ID_HEADER))
        .cloned()
        .collect();
    // The Sse response sets content-type itself; reflect that in the
    // recorded client_resp_headers so the log matches the wire.
    out.push(("content-type".to_string(), "text/event-stream".to_string()));
    // `drive_upstream_stream` injects these headers on the actual
    // response; mirror them here so the logged `client_resp_headers`
    // match what the client really receives on the wire.
    out.push(("cache-control".to_string(), "no-cache".to_string()));
    out.push(("x-accel-buffering".to_string(), "no".to_string()));
    out.push((
        GATEWAY_REQUEST_ID_HEADER.to_string(),
        request_id.to_string(),
    ));
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
pub(super) fn override_model_in_body(body: &mut serde_json::Value, model_id: &str) -> bool {
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
pub(super) fn spawn_capture(state: &AppState, capture: tiygate_core::ExchangeCapture) {
    let bus = state.telemetry.clone();
    tokio::spawn(async move {
        bus.send_capture(capture).await;
    });
}

/// Inject `prompt_cache_key` into the upstream request body when the egress
/// target is an OpenAI-family protocol (Chat Completions or Responses).
///
/// The value is set to the caller's API-key identifier so that requests from
/// the same user are routed to the same inference machine, maximising prompt
/// prefix cache hits.  If the client already supplied a `prompt_cache_key` it
/// is left untouched.
pub(super) fn maybe_inject_prompt_cache_key(
    body: &mut serde_json::Value,
    egress_suite: &tiygate_core::ProtocolSuite,
    api_key_id: &str,
) -> bool {
    let dominated_by_openai = matches!(
        egress_suite,
        tiygate_core::ProtocolSuite::OpenAiCompatible
            | tiygate_core::ProtocolSuite::OpenAiResponses
    );
    if !dominated_by_openai {
        return false;
    }
    // Never overwrite a value the client explicitly set.
    if body.get("prompt_cache_key").is_some() {
        return false;
    }
    // "anonymous" callers have no stable identity → skip injection.
    if api_key_id == "anonymous" {
        return false;
    }
    body["prompt_cache_key"] = serde_json::Value::String(api_key_id.to_string());
    true
}

/// Return whether the normalized upstream model id belongs to GPT-5.6.
fn supports_openai_reasoning_max(model_id: &str) -> bool {
    let without_provider = model_id.split(':').next().unwrap_or(model_id);
    let model = without_provider
        .rsplit('/')
        .next()
        .unwrap_or(without_provider)
        .to_ascii_lowercase();
    model == "gpt-5.6" || model.starts_with("gpt-5.6-")
}

/// Downgrade GPT-5.6-only `max` reasoning for older OpenAI-family targets.
/// The codec sees a virtual model; this helper runs after routing and therefore
/// uses the real `target.model_id`.
pub(super) fn normalize_openai_reasoning_for_target(
    body: &mut serde_json::Value,
    egress_suite: &tiygate_core::ProtocolSuite,
    target_model_id: &str,
) -> bool {
    if supports_openai_reasoning_max(target_model_id) {
        return false;
    }
    let effort = match egress_suite {
        tiygate_core::ProtocolSuite::OpenAiCompatible => body.get_mut("reasoning_effort"),
        tiygate_core::ProtocolSuite::OpenAiResponses => body
            .get_mut("reasoning")
            .and_then(|value| value.get_mut("effort")),
        _ => None,
    };
    if effort.as_deref().and_then(serde_json::Value::as_str) == Some("max") {
        if let Some(effort) = effort {
            *effort = serde_json::json!("xhigh");
            return true;
        }
    }
    false
}

/// Extract Retry-After value from response headers.
pub(super) fn extract_retry_after(headers: &HeaderMap) -> Option<String> {
    headers
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Extract upstream `RateLimit-*` headers (X-RateLimit-Limit / -Remaining / -Reset)
/// for passthrough to the downstream client.
pub(super) fn extract_rate_limit_headers(headers: &HeaderMap) -> Vec<(&'static str, String)> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_max_is_target_model_aware() {
        let mut responses = serde_json::json!({"reasoning": {"effort": "max"}});
        assert!(!normalize_openai_reasoning_for_target(
            &mut responses,
            &tiygate_core::ProtocolSuite::OpenAiResponses,
            "openai/gpt-5.6-sol:provider"
        ));
        assert_eq!(responses["reasoning"]["effort"], "max");

        assert!(normalize_openai_reasoning_for_target(
            &mut responses,
            &tiygate_core::ProtocolSuite::OpenAiResponses,
            "gpt-5.5"
        ));
        assert_eq!(responses["reasoning"]["effort"], "xhigh");

        let mut chat = serde_json::json!({"reasoning_effort": "max"});
        assert!(normalize_openai_reasoning_for_target(
            &mut chat,
            &tiygate_core::ProtocolSuite::OpenAiCompatible,
            "gpt-5.4"
        ));
        assert_eq!(chat["reasoning_effort"], "xhigh");
    }

    #[test]
    fn prompt_cache_key_reports_body_mutation() {
        let mut body = serde_json::json!({});
        assert!(maybe_inject_prompt_cache_key(
            &mut body,
            &tiygate_core::ProtocolSuite::OpenAiResponses,
            "key-id"
        ));
        assert_eq!(body["prompt_cache_key"], "key-id");
        assert!(!maybe_inject_prompt_cache_key(
            &mut body,
            &tiygate_core::ProtocolSuite::OpenAiResponses,
            "other"
        ));
    }
}
