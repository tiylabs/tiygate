//! Fixed egress behavior for OpenAI Codex OAuth credentials.
//!
//! The profile is selected explicitly in routing data. It owns Codex-specific
//! Responses request normalization, headers, HTTP response parsing, and
//! WebSocket negotiation without creating a separate HTTP connection pool.

use std::collections::HashMap;

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use base64::{engine::general_purpose, Engine as _};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tiygate_core::provider::oauth::{OAuthEgressProfile, UpstreamTransport};
use tiygate_core::{Content, IrRequest, ProtocolEndpoint, ProtocolSuite, RoutingTarget};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::ingress::AppError;

/// Required by the Codex Responses WebSocket endpoint.
pub(crate) const RESPONSES_WEBSOCKET_BETA: &str = "responses_websockets=2026-02-06";

const CODEX_TOOL_NAME_MAX_BYTES: usize = 64;

/// True when the routed request uses the OpenAI Codex OAuth Responses profile.
pub(crate) fn is_enabled(target: &RoutingTarget, egress_suite: ProtocolSuite) -> bool {
    egress_suite == ProtocolSuite::OpenAiResponses
        && target
            .oauth
            .as_ref()
            .is_some_and(|oauth| oauth.egress_profile == OAuthEgressProfile::OpenAiCodex)
}

/// Whether this profile should attempt the Codex Responses WebSocket transport.
pub(crate) fn uses_websocket(target: &RoutingTarget) -> bool {
    matches!(
        target.oauth.as_ref().map(|oauth| oauth.upstream_transport),
        Some(UpstreamTransport::CodexResponsesWebSocket)
    )
}

/// Normalize an OpenAI Responses body for the Codex OAuth contract.
pub(crate) fn prepare_body(body: &mut Value, websocket: bool) -> bool {
    let mut changed = false;
    {
        let Some(object) = body.as_object_mut() else {
            return false;
        };
        if object.get("stream").and_then(Value::as_bool) != Some(true) {
            object.insert("stream".to_string(), json!(true));
            changed = true;
        }
        if object.get("instructions").is_none_or(Value::is_null) {
            object.insert("instructions".to_string(), json!(""));
            changed = true;
        }
        // The ChatGPT subscription backend requires an explicit `store: false`
        // request. This is specific to the Codex OAuth egress profile; the
        // public OpenAI Responses API has different storage semantics.
        if object.get("store").and_then(Value::as_bool) != Some(false) {
            object.insert("store".to_string(), json!(false));
            changed = true;
        }
        // The ChatGPT subscription backend (`/backend-api/codex/responses`,
        // FastAPI) rejects Responses fields it does not model. The native Codex
        // `ResponsesApiRequest` has no generic sampling/output fields such as
        // `temperature`, `top_p`, `stop`, or `max_output_tokens`; strip them here
        // so relayed clients cannot break the request.
        for field in [
            "metadata",
            "prompt_cache_retention",
            "safety_identifier",
            "max_output_tokens",
            "max_completion_tokens",
            "temperature",
            "top_p",
            "stop",
            "user",
            "truncation",
            "prompt_cache_options",
            "context_management",
            "generate",
        ] {
            changed |= object.remove(field).is_some();
        }
        let include = json!(["reasoning.encrypted_content"]);
        if object.get("include") != Some(&include) {
            object.insert("include".to_string(), include);
            changed = true;
        }
        if object.get("parallel_tool_calls").and_then(Value::as_bool) != Some(true) {
            object.insert("parallel_tool_calls".to_string(), json!(true));
            changed = true;
        }
        if !websocket {
            for field in ["previous_response_id", "stream_options"] {
                changed |= object.remove(field).is_some();
            }
        }
    }
    changed |= normalize_codex_input_shape(body);
    changed |= normalize_codex_system_roles(body);
    changed |= normalize_codex_service_tier(body);
    changed |= strip_prompt_cache_breakpoints(body);
    changed |= normalize_codex_input_ids(body);
    changed
}

/// Codex expects Responses input strings in the message-array form.
fn normalize_codex_input_shape(body: &mut Value) -> bool {
    let Some(text) = body
        .get("input")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return false;
    };
    body["input"] = json!([{
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}]
    }]);
    true
}

/// Codex does not accept `system` roles inside the Responses input array.
fn normalize_codex_system_roles(body: &mut Value) -> bool {
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for item in input {
        if item.get("role").and_then(Value::as_str) == Some("system") {
            item["role"] = json!("developer");
            changed = true;
        }
    }
    changed
}

/// Codex currently supports only the priority service tier.
fn normalize_codex_service_tier(body: &mut Value) -> bool {
    if body
        .get("service_tier")
        .is_some_and(|value| value.as_str() != Some("priority"))
    {
        return body
            .as_object_mut()
            .is_some_and(|object| object.remove("service_tier").is_some());
    }
    false
}

/// Reject tool names that Codex cannot accept rather than silently changing
/// them and losing the mapping needed to address the original client tool.
pub(crate) fn validate_codex_tool_names(body: &Value) -> Result<(), AppError> {
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for (index, tool) in tools.iter().enumerate() {
            validate_codex_tool_name(tool.get("name"), &format!("tools[{index}].name"))?;
            validate_codex_tool_name(
                tool.pointer("/function/name"),
                &format!("tools[{index}].function.name"),
            )?;
        }
    }
    if let Some(input) = body.get("input").and_then(Value::as_array) {
        for (index, item) in input.iter().enumerate() {
            validate_codex_tool_name(item.get("name"), &format!("input[{index}].name"))?;
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        validate_codex_tool_name(choice.get("name"), "tool_choice.name")?;
        validate_codex_tool_name(
            choice.pointer("/function/name"),
            "tool_choice.function.name",
        )?;
        if let Some(tools) = choice.get("tools").and_then(Value::as_array) {
            for (index, tool) in tools.iter().enumerate() {
                validate_codex_tool_name(
                    tool.get("name"),
                    &format!("tool_choice.tools[{index}].name"),
                )?;
            }
        }
    }
    Ok(())
}

fn validate_codex_tool_name(value: Option<&Value>, path: &str) -> Result<(), AppError> {
    let Some(name) = value.and_then(Value::as_str) else {
        return Ok(());
    };
    if name.len() <= CODEX_TOOL_NAME_MAX_BYTES {
        return Ok(());
    }
    Err(AppError::new(
        StatusCode::BAD_REQUEST,
        format!("Codex tool name at {path} exceeds {CODEX_TOOL_NAME_MAX_BYTES}-byte limit"),
    )
    .with_class(tiygate_core::ErrorClass::LossyOrCapability)
    .with_upstream_code("unsupported_tool_name"))
}

/// Apply the Codex-specific reasoning and identity rules after IR encoding.
///
/// Anthropic signatures are not interchangeable with Codex encrypted content.
/// Only a locally valid GPT-shaped encrypted signature is replayed; invalid or
/// foreign reasoning blocks are removed instead of causing an upstream 400.
pub(crate) fn normalize_reasoning_and_ids(body: &mut Value, request: &IrRequest) -> bool {
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return false;
    };
    let original_len = input.len();
    let anthropic_ingress = request.ingress_protocol.suite == ProtocolSuite::AnthropicMessages;
    let signatures: Vec<Option<String>> = request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            Content::Reasoning { signature, .. } if anthropic_ingress => Some(signature.clone()),
            _ => None,
        })
        .collect();
    let mut signature_index = 0usize;
    let mut id_map = HashMap::new();
    let mut changed = false;
    let mut normalized = Vec::with_capacity(input.len());

    for mut item in std::mem::take(input) {
        let mut keep = true;
        if item.get("type").and_then(Value::as_str) == Some("reasoning") {
            let mut valid_encrypted = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(is_valid_gpt_reasoning_signature);

            if anthropic_ingress {
                let signature = signatures.get(signature_index).cloned().flatten();
                signature_index += 1;
                match signature.filter(|value| is_valid_gpt_reasoning_signature(value)) {
                    Some(signature) => {
                        item["summary"] = json!([]);
                        item["encrypted_content"] = json!(signature);
                        valid_encrypted = true;
                        changed = true;
                    }
                    None => {
                        // Claude thinking signatures cannot be replayed to
                        // Codex. Match the native Codex translator and drop
                        // the provider-specific reasoning item.
                        keep = false;
                        changed = true;
                    }
                }
            } else if item.get("encrypted_content").is_some() && !valid_encrypted {
                if let Some(object) = item.as_object_mut() {
                    object.remove("encrypted_content");
                }
                changed = true;
            }

            if keep && !valid_encrypted {
                if let Some(object) = item.as_object_mut() {
                    // `store=false` means the upstream cannot resolve an
                    // old reasoning item by id. Keep only id-less summaries.
                    if object.remove("id").is_some() {
                        changed = true;
                    }
                    let has_summary = object
                        .get("summary")
                        .and_then(Value::as_array)
                        .is_some_and(|summary| !summary.is_empty());
                    if !has_summary && !anthropic_ingress {
                        keep = false;
                        changed = true;
                    }
                }
            }
        }

        if keep {
            for key in ["id", "call_id"] {
                if let Some(value) = item.get(key).and_then(Value::as_str).map(str::to_string) {
                    let shortened = shorten_codex_id(&value, &mut id_map);
                    if shortened != value {
                        item[key] = Value::String(shortened);
                        changed = true;
                    }
                }
            }
            normalized.push(item);
        }
    }

    if normalized.len() != original_len {
        changed = true;
    }
    *input = normalized;
    changed
}

/// Derive one stable Codex prompt-cache identity per Claude session and agent.
pub(crate) fn claude_prompt_cache_key(
    ingress_protocol: &ProtocolEndpoint,
    request: &IrRequest,
    client_headers: &HeaderMap,
    model: &str,
) -> Option<String> {
    if ingress_protocol.suite != ProtocolSuite::AnthropicMessages {
        return None;
    }
    let session = first_header(
        client_headers,
        [
            "x-claude-code-session-id",
            "x-session-id",
            "session-id",
            "session_id",
        ],
    )
    .or_else(|| {
        request
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("user_id"))
            .and_then(|value| claude_session_from_user_id(value))
    })?;
    let agent = first_header(client_headers, ["x-claude-code-agent-id"])
        .unwrap_or_else(|| "main".to_string());
    let identity = format!(
        "tiygate:codex:claude-code\0{}\0{}\0{}",
        model.trim(),
        session,
        agent
    );
    let digest = Sha256::digest(identity.as_bytes());
    let mut uuid_bytes = [0u8; 16];
    uuid_bytes.copy_from_slice(&digest[..16]);
    uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x50;
    uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;
    Some(uuid::Uuid::from_bytes(uuid_bytes).to_string())
}

pub(crate) fn set_prompt_cache_key(body: &mut Value, key: &str) -> bool {
    if body.get("prompt_cache_key").and_then(Value::as_str) == Some(key) {
        return false;
    }
    body["prompt_cache_key"] = Value::String(key.to_string());
    true
}

/// Match Codex Responses Lite's serial tool-call contract and avoid sending a
/// parallel flag when the request has no callable tools.
pub(crate) fn normalize_parallel_tool_calls(body: &mut Value, headers: &HeaderMap) -> bool {
    let lite_from_header = headers
        .get("x-openai-internal-codex-responses-lite")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"));
    let lite_from_body = body
        .pointer("/client_metadata/ws_request_header_x_openai_internal_codex_responses_lite")
        .is_some_and(|value| {
            value.as_bool() == Some(true)
                || value
                    .as_str()
                    .is_some_and(|text| text.trim().eq_ignore_ascii_case("true"))
        });
    if lite_from_header || lite_from_body {
        if body.get("parallel_tool_calls").and_then(Value::as_bool) == Some(false) {
            return false;
        }
        body["parallel_tool_calls"] = Value::Bool(false);
        return true;
    }

    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    if !has_tools {
        return body
            .as_object_mut()
            .is_some_and(|object| object.remove("parallel_tool_calls").is_some());
    }
    false
}

/// Apply Codex request headers after authentication has been injected.
pub(crate) fn apply_request_headers(
    headers: &mut HeaderMap,
    is_stream: bool,
    websocket: bool,
    request_id: &str,
    session_key: Option<&str>,
    account_id: Option<&str>,
) {
    if !websocket {
        let accept = if is_stream {
            "text/event-stream"
        } else {
            "application/json"
        };
        set_header(headers, HeaderName::from_static("accept"), accept);
    }
    if !request_id.is_empty() {
        insert_header_if_missing(
            headers,
            HeaderName::from_static("x-client-request-id"),
            request_id,
        );
    }
    let session_key = session_key
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let has_session = ["session_id", "session-id"]
                .iter()
                .any(|name| headers.contains_key(*name));
            let desktop_user_agent = headers
                .get(http::header::USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.contains("Mac OS"));
            (!has_session && desktop_user_agent).then(|| uuid::Uuid::now_v7().to_string())
        });
    if let Some(session_key) = session_key.as_deref() {
        let header = if websocket {
            HeaderName::from_static("conversation_id")
        } else {
            HeaderName::from_static("session-id")
        };
        if websocket {
            set_header(headers, header, session_key);
        } else {
            headers.remove("session_id");
            headers.remove("session-id");
            set_header(headers, header, session_key);
        }
    }
    if let Some(account_id) = account_id.filter(|value| !value.is_empty()) {
        insert_header_if_missing(
            headers,
            HeaderName::from_static("chatgpt-account-id"),
            account_id,
        );
    }
    insert_header_if_missing(
        headers,
        HeaderName::from_static("originator"),
        "Codex Desktop",
    );
    insert_header_if_missing(
        headers,
        HeaderName::from_static("user-agent"),
        "Codex Desktop",
    );
}

fn set_header(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn insert_header_if_missing(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if headers.contains_key(&name) {
        return;
    }
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn first_header<const N: usize>(headers: &HeaderMap, names: [&str; N]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

fn claude_session_from_user_id(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<Value>(value) {
            if let Some(session) = parsed.get("session_id").and_then(Value::as_str) {
                if !session.trim().is_empty() {
                    return Some(session.trim().to_string());
                }
            }
        }
    }
    value
        .rsplit_once("_session_")
        .map(|(_, session)| session.trim())
        .filter(|session| {
            !session.is_empty()
                && session
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        })
        .map(str::to_string)
}

fn strip_prompt_cache_breakpoints(body: &mut Value) -> bool {
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for item in input {
        if let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) {
            for part in content {
                if let Some(object) = part.as_object_mut() {
                    changed |= object.remove("prompt_cache_breakpoint").is_some();
                }
            }
        }
    }
    changed
}

fn normalize_codex_input_ids(body: &mut Value) -> bool {
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut id_map = HashMap::new();
    let mut changed = false;
    for item in input {
        for key in ["id", "call_id"] {
            if let Some(value) = item.get(key).and_then(Value::as_str).map(str::to_string) {
                let shortened = shorten_codex_id(&value, &mut id_map);
                if shortened != value {
                    item[key] = Value::String(shortened);
                    changed = true;
                }
            }
        }
    }
    changed
}

fn shorten_codex_id(id: &str, id_map: &mut HashMap<String, String>) -> String {
    if id.chars().count() <= 64 {
        return id.to_string();
    }
    if let Some(mapped) = id_map.get(id) {
        return mapped.clone();
    }
    let digest = Sha256::digest(id.as_bytes());
    let suffix = format!("_{}", hex::encode(&digest[..8]));
    let prefix_len = 64usize.saturating_sub(suffix.chars().count());
    let shortened = format!(
        "{}{}",
        id.chars().take(prefix_len).collect::<String>(),
        suffix
    );
    id_map.insert(id.to_string(), shortened.clone());
    shortened
}

fn is_valid_gpt_reasoning_signature(signature: &str) -> bool {
    let trimmed = signature.trim();
    if trimmed != signature {
        return false;
    }
    let signature = trimmed;
    if signature.is_empty() || signature.len() > 32 * 1024 * 1024 || !signature.starts_with("gAAAA")
    {
        return false;
    }
    if !signature
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'='))
    {
        return false;
    }
    let decoded = general_purpose::URL_SAFE_NO_PAD
        .decode(signature)
        .or_else(|_| general_purpose::URL_SAFE.decode(signature));
    let Ok(decoded) = decoded else {
        return false;
    };
    if decoded.len() < 73 || decoded[0] != 0x80 {
        return false;
    }
    let ciphertext_len = decoded.len().saturating_sub(1 + 8 + 16 + 32);
    ciphertext_len > 0 && ciphertext_len % 16 == 0
}

/// Parse either a JSON response or the terminal event from a Codex HTTP/SSE response.
pub(crate) fn parse_http_response(body: &str) -> Result<Value, AppError> {
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

/// Convert the configured HTTP endpoint into the corresponding WebSocket URL.
pub(crate) fn websocket_url(upstream_url: &str) -> Result<String, AppError> {
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

/// Wrap a Responses payload in the WebSocket `response.create` event shape.
pub(crate) fn websocket_request_body(mut body: Value) -> Result<Value, AppError> {
    let object = body.as_object_mut().ok_or_else(|| {
        AppError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Codex Responses request body must be a JSON object".to_string(),
        )
    })?;
    object.insert("type".to_string(), json!("response.create"));
    Ok(body)
}

/// Remove HTTP entity headers and add the WebSocket negotiation beta.
pub(crate) fn prepare_websocket_headers(headers: &mut http::HeaderMap) {
    headers.remove(http::header::CONTENT_TYPE);
    headers.remove(http::header::CONTENT_LENGTH);
    headers.remove(http::header::TRANSFER_ENCODING);
    headers.insert(
        http::HeaderName::from_static("openai-beta"),
        http::HeaderValue::from_static(RESPONSES_WEBSOCKET_BETA),
    );
}

/// Build a complete WebSocket handshake while preserving profile/auth headers.
pub(crate) fn websocket_handshake_request(
    websocket_url: &str,
    headers: &http::HeaderMap,
) -> Result<http::Request<()>, AppError> {
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

/// Normalize terminal event variants for the standard Responses SSE bridge.
pub(crate) fn normalize_websocket_event(text: String) -> (String, bool) {
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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tiygate_core::provider::oauth::{OAuthTargetConfig, TokenRequestStyle};
    use tiygate_core::{Content, IrRequest, Message, ProtocolEndpoint, Role};

    fn target(profile: OAuthEgressProfile, transport: UpstreamTransport) -> RoutingTarget {
        RoutingTarget {
            provider_id: "openai-oauth".to_string(),
            model_id: "gpt-test".to_string(),
            api_base: "https://chatgpt.com/backend-api/codex".to_string(),
            api_key: String::new(),
            api_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiResponses, "responses", "v1"),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            egress_dialect_id: None,
            weight: 1.0,
            oauth: Some(OAuthTargetConfig {
                upstream_transport: transport,
                egress_profile: profile,
                token_url: "https://example.test/token".to_string(),
                client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_string(),
                client_secret: None,
                refresh_token: "refresh".to_string(),
                scopes: vec![],
                token_request_style: TokenRequestStyle::Form,
                authorization_header: None,
                authorization_prefix: None,
                extra_headers: vec![],
                account_id: None,
            }),
        }
    }

    #[test]
    fn profile_selection_is_explicit_and_protocol_scoped() {
        let codex = target(OAuthEgressProfile::OpenAiCodex, UpstreamTransport::Http);
        assert!(is_enabled(&codex, ProtocolSuite::OpenAiResponses));
        assert!(!is_enabled(&codex, ProtocolSuite::OpenAiCompatible));

        let standard = target(OAuthEgressProfile::Standard, UpstreamTransport::Http);
        assert!(!is_enabled(&standard, ProtocolSuite::OpenAiResponses));
    }

    #[test]
    fn websocket_transport_remains_independent_from_profile_selection() {
        let websocket = target(
            OAuthEgressProfile::OpenAiCodex,
            UpstreamTransport::CodexResponsesWebSocket,
        );
        assert!(is_enabled(&websocket, ProtocolSuite::OpenAiResponses));
        assert!(uses_websocket(&websocket));

        let http = target(OAuthEgressProfile::OpenAiCodex, UpstreamTransport::Http);
        assert!(!uses_websocket(&http));
    }

    #[test]
    fn prepare_body_strips_codex_unsupported_fields_on_both_transports() {
        for websocket in [false, true] {
            let mut body = json!({
                "model": "gpt-5.6",
                "stream": false,
                "instructions": null,
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{
                        "type": "input_text",
                        "text": "hi",
                        "prompt_cache_breakpoint": {"mode": "explicit"}
                    }]
                }],
                "metadata": {"user_id": "user-123"},
                "max_output_tokens": 4096,
                "max_completion_tokens": 4096,
                "temperature": 0.2,
                "top_p": 0.8,
                "stop": ["DONE"],
                "user": "request-owner",
                "truncation": "auto",
                "prompt_cache_options": {"mode": "implicit"},
                "context_management": [{"type": "compaction"}],
                "generate": true,
                "service_tier": "standard",
                "prompt_cache_retention": "24h",
                "safety_identifier": "user",
                "previous_response_id": "resp-old",
                "stream_options": {"include_usage": true},
            });
            assert!(prepare_body(&mut body, websocket));
            assert_eq!(body["stream"], true);
            assert_eq!(body["instructions"], "");
            assert_eq!(body["store"], false);
            assert_eq!(body["model"], "gpt-5.6");
            assert_eq!(body["input"][0]["content"][0]["text"], "hi");
            assert!(body["input"][0]["content"][0]
                .get("prompt_cache_breakpoint")
                .is_none());
            assert!(body.get("metadata").is_none());
            assert!(
                body.get("max_output_tokens").is_none(),
                "ChatGPT Codex backend rejects max_output_tokens (websocket={websocket})"
            );
            assert!(body.get("temperature").is_none());
            assert!(body.get("top_p").is_none());
            assert!(body.get("stop").is_none());
            assert!(body.get("user").is_none());
            assert!(body.get("truncation").is_none());
            assert!(body.get("prompt_cache_options").is_none());
            assert!(body.get("context_management").is_none());
            assert!(body.get("generate").is_none());
            assert!(body.get("service_tier").is_none());
            assert_eq!(body["include"][0], "reasoning.encrypted_content");
            assert_eq!(body["parallel_tool_calls"], true);
            assert!(body.get("prompt_cache_retention").is_none());
            assert!(body.get("safety_identifier").is_none());
            // HTTP strips these; the WebSocket transport keeps them.
            if websocket {
                assert_eq!(body["previous_response_id"], "resp-old");
                assert_eq!(body["stream_options"]["include_usage"], true);
            } else {
                assert!(body.get("previous_response_id").is_none());
                assert!(body.get("stream_options").is_none());
            }
        }
    }

    #[test]
    fn prepare_body_forces_store_false_for_all_codex_transports() {
        for websocket in [false, true] {
            let mut missing = json!({"stream": true, "instructions": ""});
            assert!(prepare_body(&mut missing, websocket));
            assert_eq!(missing["store"], false);

            let mut enabled = json!({
                "stream": true,
                "instructions": "",
                "store": true,
            });
            assert!(prepare_body(&mut enabled, websocket));
            assert_eq!(enabled["store"], false);

            let mut disabled = json!({
                "stream": true,
                "instructions": "",
                "store": false,
            });
            assert!(prepare_body(&mut disabled, websocket));
            assert_eq!(disabled["store"], false);
            assert_eq!(disabled["include"][0], "reasoning.encrypted_content");
        }
    }

    #[test]
    fn prepare_body_normalizes_input_shape_and_system_role() {
        let mut string_input = json!({
            "input": "hello",
            "service_tier": "priority"
        });
        assert!(prepare_body(&mut string_input, false));
        assert_eq!(string_input["input"][0]["role"], "user");
        assert_eq!(string_input["input"][0]["content"][0]["text"], "hello");
        assert_eq!(string_input["service_tier"], "priority");

        let mut system_input = json!({
            "input": [{"type": "message", "role": "system", "content": "rule"}]
        });
        assert!(prepare_body(&mut system_input, false));
        assert_eq!(system_input["input"][0]["role"], "developer");
    }

    #[test]
    fn validate_codex_tool_names_rejects_names_over_64_bytes() {
        let body = json!({
            "tools": [{"type": "function", "name": "x".repeat(65)}]
        });
        let error = validate_codex_tool_names(&body).unwrap_err();
        assert_eq!(error.http_status(), StatusCode::BAD_REQUEST);
        assert_eq!(error.upstream_error_code(), Some("unsupported_tool_name"));
    }

    fn anthropic_request_with_reasoning(signature: Option<String>) -> IrRequest {
        IrRequest {
            model: "gpt-5.6".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![Content::Reasoning {
                    text: "summary".to_string(),
                    signature,
                    id: None,
                    encrypted_content: None,
                }],
            }],
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            stream: false,
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::AnthropicMessages,
                "messages",
                "2023-06-01",
            ),
            metadata: None,
            extensions: HashMap::new(),
        }
    }

    #[test]
    fn normalize_reasoning_drops_foreign_signature_and_shortens_ids() {
        let mut body = json!({
            "store": false,
            "input": [
                {"type": "reasoning", "id": "rs_foreign", "summary": [{"type": "summary_text", "text": "x"}]},
                {"type": "function_call", "id": "call_abcdefghijklmnopqrstuvwxyz_abcdefghijklmnopqrstuvwxyz", "call_id": "call_abcdefghijklmnopqrstuvwxyz_abcdefghijklmnopqrstuvwxyz", "name": "lookup", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_abcdefghijklmnopqrstuvwxyz_abcdefghijklmnopqrstuvwxyz", "output": "ok"}
            ]
        });
        let request =
            anthropic_request_with_reasoning(Some("claude-foreign-signature".to_string()));

        assert!(normalize_reasoning_and_ids(&mut body, &request));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert!(input.iter().all(|item| item["type"] != "reasoning"));
        assert!(input[0]["id"].as_str().unwrap().len() <= 64);
        assert!(input[0]["call_id"].as_str().unwrap().len() <= 64);
        assert_eq!(input[0]["call_id"], input[1]["call_id"]);
    }

    #[test]
    fn normalize_reasoning_preserves_valid_gpt_encrypted_content() {
        let mut decoded = vec![0x80u8];
        decoded.extend([0u8; 8 + 16 + 16 + 32]);
        let signature = general_purpose::URL_SAFE_NO_PAD.encode(decoded);
        let mut body = json!({
            "store": false,
            "input": [{"type": "reasoning", "id": "rs_valid", "summary": [{"type": "summary_text", "text": "x"}]}]
        });
        let request = anthropic_request_with_reasoning(Some(signature.clone()));

        assert!(normalize_reasoning_and_ids(&mut body, &request));
        assert_eq!(body["input"][0]["encrypted_content"], signature);
        assert_eq!(body["input"][0]["summary"], json!([]));
        assert_eq!(body["input"][0]["id"], "rs_valid");
    }

    #[test]
    fn normalize_reasoning_drops_whitespace_padded_gpt_signature() {
        let mut decoded = vec![0x80u8];
        decoded.extend([0u8; 8 + 16 + 16 + 32]);
        let signature = general_purpose::URL_SAFE_NO_PAD.encode(decoded);
        let mut body = json!({
            "store": false,
            "input": [{"type": "reasoning", "encrypted_content": format!(" {signature}")}]
        });
        let request = anthropic_request_with_reasoning(Some(format!(" {signature}")));

        assert!(normalize_reasoning_and_ids(&mut body, &request));
        assert!(body["input"].as_array().is_some_and(Vec::is_empty));
    }

    #[test]
    fn claude_session_cache_key_is_stable_and_agent_scoped() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "user_id".to_string(),
            r#"{"device_id":"d","session_id":"session-1"}"#.to_string(),
        );
        let mut request = anthropic_request_with_reasoning(None);
        request.metadata = Some(metadata);
        let protocol = request.ingress_protocol.clone();
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-claude-code-agent-id"),
            HeaderValue::from_static("agent-a"),
        );

        let first = claude_prompt_cache_key(&protocol, &request, &headers, "gpt-5.6").unwrap();
        let second = claude_prompt_cache_key(&protocol, &request, &headers, "gpt-5.6").unwrap();
        assert_eq!(first, second);
        headers.insert(
            HeaderName::from_static("x-claude-code-agent-id"),
            HeaderValue::from_static("agent-b"),
        );
        let other_agent =
            claude_prompt_cache_key(&protocol, &request, &headers, "gpt-5.6").unwrap();
        assert_ne!(first, other_agent);
    }

    #[test]
    fn claude_prompt_cache_key_rejects_bare_user_id() {
        let mut metadata = HashMap::new();
        metadata.insert("user_id".to_string(), "same-user-across-chats".to_string());
        let mut request = anthropic_request_with_reasoning(None);
        request.metadata = Some(metadata);

        assert!(claude_prompt_cache_key(
            &request.ingress_protocol.clone(),
            &request,
            &HeaderMap::new(),
            "gpt-5.6"
        )
        .is_none());
    }

    #[test]
    fn apply_request_headers_sets_codex_http_and_websocket_identity() {
        let mut http_headers = HeaderMap::new();
        apply_request_headers(
            &mut http_headers,
            true,
            false,
            "request-1",
            Some("cache-1"),
            Some("account-1"),
        );
        assert_eq!(http_headers["accept"], "text/event-stream");
        assert_eq!(http_headers["x-client-request-id"], "request-1");
        assert_eq!(http_headers["session-id"], "cache-1");
        assert_eq!(http_headers["chatgpt-account-id"], "account-1");
        assert_eq!(http_headers["originator"], "Codex Desktop");

        http_headers.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("application/json"),
        );
        apply_request_headers(&mut http_headers, true, false, "", None, None);
        assert_eq!(http_headers["accept"], "text/event-stream");

        let mut websocket_headers = HeaderMap::new();
        apply_request_headers(
            &mut websocket_headers,
            true,
            true,
            "request-1",
            Some("cache-1"),
            None,
        );
        assert!(websocket_headers.get("accept").is_none());
        assert_eq!(websocket_headers["conversation_id"], "cache-1");
    }
}
