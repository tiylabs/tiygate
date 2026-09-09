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

fn normalized_model_id(model_id: &str) -> String {
    let without_provider = model_id.split(':').next().unwrap_or(model_id);
    without_provider
        .rsplit('/')
        .next()
        .unwrap_or(without_provider)
        .to_ascii_lowercase()
}

/// Return whether the target accepts the `max` reasoning effort verbatim.
fn supports_reasoning_max(model_id: &str) -> bool {
    let model = normalized_model_id(model_id);
    model == "gpt-5.6"
        || model.starts_with("gpt-5.6-")
        || matches!(
            model.as_str(),
            "deepseek-v4-flash" | "deepseek-v4-pro" | "deepseek-v4-flash-vision-exp"
        )
}

/// Downgrade `max` reasoning for OpenAI-family targets that do not declare it.
/// The codec sees a virtual model; this helper runs after routing and therefore
/// uses the real `target.model_id`.
pub(super) fn normalize_openai_reasoning_for_target(
    body: &mut serde_json::Value,
    egress_suite: &tiygate_core::ProtocolSuite,
    target_model_id: &str,
) -> bool {
    if supports_reasoning_max(target_model_id) {
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

fn is_official_deepseek_responses_target(
    target: &tiygate_core::RoutingTarget,
    egress_suite: &tiygate_core::ProtocolSuite,
) -> bool {
    if *egress_suite != tiygate_core::ProtocolSuite::OpenAiResponses {
        return false;
    }
    if target.provider_id == "deepseek" || target.api_protocol.name == "deepseek-responses" {
        return true;
    }
    url::Url::parse(target.effective_api_base())
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| host.eq_ignore_ascii_case("api.deepseek.com"))
}

fn deepseek_capability_error(detail: impl Into<String>) -> String {
    let mut message = String::from("DeepSeek Responses capability rejected: ");
    message.push_str(&detail.into());
    message
}

pub(super) fn prepare_provider_request_body(
    body: &mut serde_json::Value,
    target: &tiygate_core::RoutingTarget,
    egress_suite: &tiygate_core::ProtocolSuite,
    api_key_id: &str,
) -> Result<bool, String> {
    let deepseek_responses = is_official_deepseek_responses_target(target, egress_suite);
    let mut mutated = false;
    if !deepseek_responses {
        mutated |= maybe_inject_prompt_cache_key(body, egress_suite, api_key_id);
    }
    mutated |= normalize_openai_reasoning_for_target(body, egress_suite, &target.model_id);
    if deepseek_responses {
        let outcome = tiygate_core::sanitize_with_profile(
            body,
            super::capability::deepseek_responses_profile(),
        )
        .map_err(|reject| deepseek_capability_error(reject.detail))?;
        mutated |= outcome.mutated;
    }
    Ok(mutated)
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

        let mut deepseek = serde_json::json!({"reasoning": {"effort": "max"}});
        assert!(!normalize_openai_reasoning_for_target(
            &mut deepseek,
            &tiygate_core::ProtocolSuite::OpenAiResponses,
            "deepseek/deepseek-v4-pro:official"
        ));
        assert_eq!(deepseek["reasoning"]["effort"], "max");
    }

    fn deepseek_target() -> tiygate_core::RoutingTarget {
        tiygate_core::RoutingTarget {
            provider_id: "deepseek".to_string(),
            model_id: "deepseek-v4-pro".to_string(),
            api_base: "https://api.deepseek.com".to_string(),
            api_key: "sk-test".to_string(),
            api_protocol: tiygate_core::ProtocolSuite::OpenAiResponses.default_endpoint(),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
            oauth: None,
        }
    }

    #[test]
    fn deepseek_responses_converts_reasoning_summary_and_preserves_max() {
        let target = deepseek_target();
        let mut body = serde_json::json!({
            "model": "deepseek-v4-pro",
            "reasoning": {"effort": "max", "summary": "auto"},
            "text": {"verbosity": "high"},
            "prompt_cache_key": "codex-session",
            "prompt_cache_retention": "24h",
            "prompt_cache_options": {"mode": "explicit"},
            "metadata": {"user_id": "codex-session-1"},
            "include": ["reasoning.encrypted_content"],
            "input": [
                {"role": "user", "content": "weather?"},
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{"type": "summary_text", "text": "call weather"}]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "weather",
                    "arguments": "{}"
                }
            ],
            "tools": [{
                "type": "function",
                "name": "weather",
                "parameters": {"type": "object"}
            }]
        });

        let prepared = prepare_provider_request_body(
            &mut body,
            &target,
            &tiygate_core::ProtocolSuite::OpenAiResponses,
            "key-1",
        );
        assert!(
            matches!(prepared, Ok(true)),
            "unexpected result: {prepared:?}"
        );
        assert_eq!(body["reasoning"]["effort"], "max");
        assert_eq!(body["input"][1]["content"][0]["type"], "reasoning_text");
        assert_eq!(body["input"][1]["content"][0]["text"], "call weather");
        assert!(body["input"][1].get("summary").is_none());
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("prompt_cache_retention").is_none());
        assert!(body.get("prompt_cache_options").is_none());
        assert!(body.get("metadata").is_none());
        assert!(body.get("include").is_none());
        assert!(body["reasoning"].get("summary").is_none());
        assert!(body.get("text").is_none());
    }

    #[test]
    fn deepseek_profile_survives_database_id_and_proxy_override() {
        let mut target = deepseek_target();
        target.provider_id = "provider-row-id".to_string();
        target.api_base = "https://proxy.example/deepseek".to_string();
        target.api_protocol = tiygate_core::ProtocolEndpoint::new(
            tiygate_core::ProtocolSuite::OpenAiResponses,
            "deepseek-responses",
            "v1",
        );

        assert!(is_official_deepseek_responses_target(
            &target,
            &tiygate_core::ProtocolSuite::OpenAiResponses
        ));
        let mut body = serde_json::json!({
            "input": "hi",
            "tools": [{"type": "mcp", "server_label": "internal"}]
        });
        let result = prepare_provider_request_body(
            &mut body,
            &target,
            &tiygate_core::ProtocolSuite::OpenAiResponses,
            "anonymous",
        );
        assert!(
            matches!(&result, Err(error) if error.contains("mcp")),
            "unexpected result: {result:?}"
        );
    }

    #[test]
    fn deepseek_responses_rejects_silently_ignored_capabilities() {
        let target = deepseek_target();
        for (field, mut body) in [
            (
                "previous_response_id",
                serde_json::json!({"input": "hi", "previous_response_id": "resp_1"}),
            ),
            (
                "file_search",
                serde_json::json!({"input": "hi", "tools": [{"type": "file_search"}]}),
            ),
        ] {
            let result = prepare_provider_request_body(
                &mut body,
                &target,
                &tiygate_core::ProtocolSuite::OpenAiResponses,
                "anonymous",
            );
            assert!(
                matches!(&result, Err(error) if error.contains(field)),
                "unexpected result: {result:?}"
            );
        }
    }

    #[test]
    fn deepseek_responses_strips_or_drops_encrypted_reasoning() {
        let target = deepseek_target();

        // 1. Encrypted-only shell (no plaintext summary / content / text):
        //    DeepSeek can consume nothing, so the whole reasoning item is
        //    dropped rather than rejecting the entire request.
        let mut body = serde_json::json!({
            "input": [{
                "type": "reasoning",
                "encrypted_content": "opaque",
                "summary": []
            }]
        });
        let prepared = prepare_provider_request_body(
            &mut body,
            &target,
            &tiygate_core::ProtocolSuite::OpenAiResponses,
            "anonymous",
        );
        assert!(
            matches!(prepared, Ok(true)),
            "encrypted-only reasoning should be stripped, not rejected: {prepared:?}"
        );
        assert_eq!(body["input"].as_array().map(|arr| arr.len()), Some(0));

        // 2. Encrypted blob alongside a plaintext summary: strip the blob and
        //    keep the plaintext as DeepSeek `reasoning_text`.
        let mut body = serde_json::json!({
            "input": [
                {"role": "user", "content": "weather?"},
                {
                    "type": "reasoning",
                    "encrypted_content": "opaque",
                    "summary": [{"type": "summary_text", "text": "call weather"}]
                }
            ]
        });
        let prepared = prepare_provider_request_body(
            &mut body,
            &target,
            &tiygate_core::ProtocolSuite::OpenAiResponses,
            "anonymous",
        );
        assert!(
            matches!(prepared, Ok(true)),
            "encrypted reasoning with plaintext should be converted: {prepared:?}"
        );
        assert_eq!(body["input"][1]["content"][0]["type"], "reasoning_text");
        assert_eq!(body["input"][1]["content"][0]["text"], "call weather");
        assert!(body["input"][1].get("encrypted_content").is_none());
        assert!(body["input"][1].get("summary").is_none());
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
