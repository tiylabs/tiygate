//! Fixed egress behavior for OpenAI Codex OAuth credentials.
//!
//! The profile is selected explicitly in routing data. It owns Codex-specific
//! Responses request normalization, headers, HTTP response parsing, and
//! WebSocket negotiation without creating a separate HTTP connection pool.

use axum::http::StatusCode;
use serde_json::{json, Value};
use tiygate_core::provider::oauth::{OAuthEgressProfile, UpstreamTransport};
use tiygate_core::{ProtocolSuite, RoutingTarget};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::ingress::AppError;

/// Required by the Codex Responses WebSocket endpoint.
pub(crate) const RESPONSES_WEBSOCKET_BETA: &str = "responses_websockets=2026-02-06";

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

/// Apply Codex-owned request headers after credential injection.
pub(crate) fn apply_headers(headers: &mut http::HeaderMap) {
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
    use tiygate_core::provider::oauth::{OAuthTargetConfig, TokenRequestStyle};
    use tiygate_core::ProtocolEndpoint;

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
}
