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

fn is_meaningful(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::String(value) => !value.is_empty(),
        serde_json::Value::Array(value) => !value.is_empty(),
        serde_json::Value::Object(value) => !value.is_empty(),
        serde_json::Value::Number(_) => true,
    }
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
    format!("DeepSeek Responses capability rejected: {}", detail.into())
}

fn validate_deepseek_message_content(item: &serde_json::Value) -> Result<(), String> {
    let Some(parts) = item.get("content").and_then(|value| value.as_array()) else {
        return Ok(());
    };
    for part in parts {
        match part.get("type").and_then(|value| value.as_str()) {
            Some("input_text") | Some("output_text") | Some("input_image") => {}
            Some("input_file") => {
                return Err(deepseek_capability_error(
                    "input_file content is not supported",
                ));
            }
            Some(other) => {
                return Err(deepseek_capability_error(format!(
                    "message content type {other} is not supported"
                )));
            }
            None => {}
        }
    }
    Ok(())
}

fn validate_deepseek_tool_output(item: &serde_json::Value) -> Result<(), String> {
    let Some(parts) = item.get("output").and_then(|value| value.as_array()) else {
        return Ok(());
    };
    for part in parts {
        match part.get("type").and_then(|value| value.as_str()) {
            Some("input_text") | Some("input_image") => {}
            Some(other) => {
                return Err(deepseek_capability_error(format!(
                    "tool output content type {other} is not supported"
                )));
            }
            None => {}
        }
    }
    Ok(())
}

/// How a DeepSeek reasoning input item should be handled after preparation.
enum PreparedReasoningItem {
    /// Keep the item; the bool reports whether its body was mutated.
    Keep { changed: bool },
    /// Drop the whole item — nothing DeepSeek can consume remains.
    Drop,
}

fn prepare_deepseek_reasoning_item(
    item: &mut serde_json::Value,
) -> Result<PreparedReasoningItem, String> {
    // DeepSeek cannot decrypt OpenAI-style encrypted reasoning. When a client
    // echoes a reasoning item back that carries an encrypted blob, DeepSeek can
    // never consume it. Treat it like the other "accepted-but-ignored" controls:
    // strip the blob and keep any plaintext instead of rejecting the whole
    // request. When the item is encrypted-only (no plaintext summary or
    // reasoning_text), there is nothing DeepSeek can use, so drop the item.
    if item.get("encrypted_content").is_some_and(is_meaningful) {
        let has_plaintext = item
            .get("content")
            .and_then(|value| value.as_array())
            .is_some_and(|parts| !parts.is_empty())
            || item
                .get("summary")
                .and_then(|value| value.as_array())
                .is_some_and(|parts| !parts.is_empty())
            || item
                .get("text")
                .and_then(|value| value.as_str())
                .is_some_and(|text| !text.is_empty());
        if !has_plaintext {
            return Ok(PreparedReasoningItem::Drop);
        }
    }

    if let Some(parts) = item.get("content").and_then(|value| value.as_array()) {
        for part in parts {
            if part.get("type").and_then(|value| value.as_str()) != Some("reasoning_text") {
                return Err(deepseek_capability_error(
                    "reasoning content only supports reasoning_text parts",
                ));
            }
        }
    }

    let summary_text = item
        .get("summary")
        .and_then(|value| value.as_array())
        .map(|summary| {
            summary
                .iter()
                .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .filter(|text| !text.is_empty())
        .or_else(|| {
            item.get("text")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        });
    let has_content = item
        .get("content")
        .and_then(|value| value.as_array())
        .is_some_and(|parts| !parts.is_empty());
    if !has_content {
        let text = summary_text.ok_or_else(|| {
            deepseek_capability_error("reasoning input must contain reasoning_text content")
        })?;
        item["content"] = serde_json::json!([{"type": "reasoning_text", "text": text}]);
    }

    let object = item
        .as_object_mut()
        .ok_or_else(|| deepseek_capability_error("reasoning input item must be a JSON object"))?;
    let removed_summary = object.remove("summary").is_some();
    let removed_text = object.remove("text").is_some();
    let removed_encrypted = object.remove("encrypted_content").is_some();
    Ok(PreparedReasoningItem::Keep {
        changed: !has_content || removed_summary || removed_text || removed_encrypted,
    })
}

/// Remove a DeepSeek-accepted-but-ignored control field from a nested object.
/// Returns true if `body` was mutated. When the container object becomes
/// empty after the removal, the container is dropped so we never forward an
/// empty object that DeepSeek may treat differently from an absent one.
fn strip_ignored_deepseek_control(
    body: &mut serde_json::Value,
    container: &str,
    field: &str,
) -> bool {
    let mut mutated = false;
    if let Some(object) = body
        .pointer_mut(container)
        .and_then(|value| value.as_object_mut())
    {
        mutated |= object.remove(field).is_some();
    }
    let emptied = body
        .pointer(container)
        .and_then(|value| value.as_object())
        .is_some_and(|object| object.is_empty());
    if emptied {
        let key = container.trim_start_matches('/');
        if let Some(root) = body.as_object_mut() {
            mutated |= root.remove(key).is_some();
        }
    }
    mutated
}

/// Apply the official DeepSeek Responses compatibility profile. The upstream
/// silently ignores unsupported OpenAI fields, so TiyGate rejects semantic
/// loss and converts replayable OpenAI reasoning summaries to DeepSeek's
/// `reasoning_text` input form before sending the request.
fn prepare_deepseek_responses_request(body: &mut serde_json::Value) -> Result<bool, String> {
    // DeepSeek manages context caching automatically and silently ignores
    // non-semantic OpenAI controls. Codex clients commonly send cache-affinity
    // hints (`prompt_cache_*`), client-side tags (`metadata`), and a response
    // item selector (`include`). DeepSeek never parses these, and it returns
    // reasoning/tool items automatically, so stripping them is lossless.
    // Strip them instead of rejecting an otherwise compatible request.
    let mut mutated = false;
    if let Some(object) = body.as_object_mut() {
        for field in [
            "prompt_cache_key",
            "prompt_cache_retention",
            "prompt_cache_options",
            "metadata",
            "include",
        ] {
            mutated |= object.remove(field).is_some();
        }
    }

    for field in [
        "previous_response_id",
        "conversation",
        "background",
        "max_tool_calls",
        "prompt",
        "service_tier",
        "safety_identifier",
        "context_management",
        "stream_options",
        "client_metadata",
        "multi_agent",
        "stop",
        "presence_penalty",
        "frequency_penalty",
        "seed",
        "n",
        "logprobs",
    ] {
        if body.get(field).is_some_and(is_meaningful) {
            return Err(deepseek_capability_error(format!(
                "{field} is not supported"
            )));
        }
    }
    if body.get("store").and_then(|value| value.as_bool()) == Some(true) {
        return Err(deepseek_capability_error("store=true is not supported"));
    }
    if body
        .get("parallel_tool_calls")
        .and_then(|value| value.as_bool())
        == Some(false)
    {
        return Err(deepseek_capability_error(
            "parallel_tool_calls=false is not supported; DeepSeek always enables it",
        ));
    }
    if body
        .get("truncation")
        .and_then(|value| value.as_str())
        .is_some_and(|value| value != "disabled")
    {
        return Err(deepseek_capability_error(
            "automatic truncation is not supported",
        ));
    }
    // DeepSeek accepts but ignores `text.verbosity` and `reasoning.summary`.
    // Codex clients commonly set them, so strip these no-op controls instead of
    // rejecting an otherwise compatible request. If the container becomes empty
    // after stripping, drop it so we never forward an empty object.
    mutated |= strip_ignored_deepseek_control(body, "/text", "verbosity");
    mutated |= strip_ignored_deepseek_control(body, "/reasoning", "summary");

    if let Some(tools) = body.get("tools").and_then(|value| value.as_array()) {
        for tool in tools {
            match tool.get("type").and_then(|value| value.as_str()) {
                Some("function") => {}
                Some("web_search") | Some("web_search_2025_08_26") => {
                    if tool.get("search_context_size").is_some_and(is_meaningful)
                        || tool.get("user_location").is_some_and(is_meaningful)
                    {
                        return Err(deepseek_capability_error(
                            "web_search search_context_size and user_location are ignored",
                        ));
                    }
                }
                Some("custom")
                    if tool.get("name").and_then(|value| value.as_str()) == Some("apply_patch") => {
                }
                Some("custom") => {
                    return Err(deepseek_capability_error(
                        "only the apply_patch custom tool is supported",
                    ));
                }
                Some(other) => {
                    return Err(deepseek_capability_error(format!(
                        "tool type {other} is not supported"
                    )));
                }
                None => {
                    return Err(deepseek_capability_error("tool type is required"));
                }
            }
        }
    }

    if let Some(items) = body.get_mut("input").and_then(|value| value.as_array_mut()) {
        let mut drop_indices: Vec<usize> = Vec::new();
        for (idx, item) in items.iter_mut().enumerate() {
            let item_type = item.get("type").and_then(|value| value.as_str());
            match item_type {
                Some("reasoning") => match prepare_deepseek_reasoning_item(item)? {
                    PreparedReasoningItem::Keep { changed } => mutated |= changed,
                    PreparedReasoningItem::Drop => drop_indices.push(idx),
                },
                Some("message") | None if item.get("role").is_some() => {
                    validate_deepseek_message_content(item)?;
                }
                Some("function_call") | Some("web_search_call") => {}
                Some("function_call_output") | Some("custom_tool_call_output") => {
                    validate_deepseek_tool_output(item)?;
                }
                Some("custom_tool_call") => {
                    if item.get("name").and_then(|value| value.as_str()) != Some("apply_patch") {
                        return Err(deepseek_capability_error(
                            "only apply_patch custom_tool_call items are supported",
                        ));
                    }
                }
                Some(other) => {
                    return Err(deepseek_capability_error(format!(
                        "input item type {other} is not supported"
                    )));
                }
                None => {
                    return Err(deepseek_capability_error(
                        "input item must have a supported type or role",
                    ));
                }
            }
        }
        // Remove reasoning items DeepSeek can no longer consume (encrypted-only
        // shells with no plaintext). Reverse order keeps indices valid.
        if !drop_indices.is_empty() {
            for idx in drop_indices.into_iter().rev() {
                items.remove(idx);
            }
            mutated = true;
        }
    }
    Ok(mutated)
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
        mutated |= prepare_deepseek_responses_request(body)?;
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
