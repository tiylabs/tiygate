//! Isolated egress behavior for Anthropic OAuth credentials.
//!
//! The profile deliberately applies only to OAuth credentials that opt into
//! [`OAuthEgressProfile::AnthropicOAuth`] and only when the routed egress
//! protocol is Anthropic Messages. API-key credentials and every other
//! protocol retain the gateway's generic egress behavior.

use std::collections::HashSet;

use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tiygate_core::provider::oauth::OAuthEgressProfile;
use tiygate_core::{ProtocolSuite, RoutingTarget};
use twox_hash::XxHash64;
use uuid::Uuid;

const ANTHROPIC_OAUTH_BETAS: [&str; 3] = [
    "claude-code-20250219",
    "oauth-2025-04-20",
    "interleaved-thinking-2025-05-14",
];
const CCH_SEED: u64 = 0x6E52_736A_C806_831E;

/// True when the routed request must use the Anthropic OAuth egress profile.
pub(crate) fn is_enabled(target: &RoutingTarget, egress_suite: ProtocolSuite) -> bool {
    egress_suite == ProtocolSuite::AnthropicMessages
        && target
            .oauth
            .as_ref()
            .is_some_and(|oauth| oauth.egress_profile == OAuthEgressProfile::AnthropicOAuth)
}

/// Apply request-body normalization that is specific to Anthropic OAuth.
///
/// No synthetic Claude Code prompt is inserted. The profile only adds a
/// summarized-thinking default and re-signs a pre-existing billing header,
/// preserving caller intent and avoiding hidden prompt changes.
pub(crate) fn prepare_body(body: &mut Value) -> bool {
    ensure_thinking_display(body) | sign_existing_billing_header(body)
}

/// Apply egress-owned headers after client header forwarding and credential
/// injection. This keeps the gateway's credentials authoritative while still
/// preserving caller-requested beta flags.
pub(crate) fn apply_headers(
    target: &RoutingTarget,
    headers: &mut HeaderMap,
    is_stream: bool,
    request_id: &str,
) -> Result<(), String> {
    merge_required_betas(headers)?;
    insert_header(
        headers,
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static("2023-06-01"),
    );
    insert_header(
        headers,
        HeaderName::from_static("x-app"),
        HeaderValue::from_static("cli"),
    );
    insert_header(
        headers,
        HeaderName::from_static("x-stainless-retry-count"),
        HeaderValue::from_static("0"),
    );
    insert_header(
        headers,
        HeaderName::from_static("x-stainless-runtime"),
        HeaderValue::from_static("node"),
    );
    insert_header(
        headers,
        HeaderName::from_static("x-stainless-lang"),
        HeaderValue::from_static("js"),
    );
    insert_header(
        headers,
        HeaderName::from_static("x-stainless-timeout"),
        HeaderValue::from_static("600"),
    );
    insert_header(
        headers,
        HeaderName::from_static("x-claude-code-session-id"),
        HeaderValue::from_str(&stable_session_id(&target.provider_id))
            .map_err(|error| format!("invalid Anthropic OAuth session header: {error}"))?,
    );
    insert_header(
        headers,
        HeaderName::from_static("x-client-request-id"),
        HeaderValue::from_str(request_id)
            .map_err(|error| format!("invalid Anthropic OAuth request id: {error}"))?,
    );
    insert_header(
        headers,
        http::header::ACCEPT,
        HeaderValue::from_static(if is_stream {
            "text/event-stream"
        } else {
            "application/json"
        }),
    );
    // The workspace does not enable reqwest's compression decoders. Asking
    // for identity preserves correct parsing for both JSON and SSE responses.
    insert_header(
        headers,
        http::header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
    Ok(())
}

fn insert_header(headers: &mut HeaderMap, name: HeaderName, value: HeaderValue) {
    headers.insert(name, value);
}

fn merge_required_betas(headers: &mut HeaderMap) -> Result<(), String> {
    let mut betas = Vec::new();
    let mut seen = HashSet::new();
    for value in headers.get_all("anthropic-beta").iter() {
        let value = value
            .to_str()
            .map_err(|error| format!("invalid Anthropic beta header: {error}"))?;
        for beta in value
            .split(',')
            .map(str::trim)
            .filter(|beta| !beta.is_empty())
        {
            if seen.insert(beta.to_string()) {
                betas.push(beta.to_string());
            }
        }
    }
    for beta in ANTHROPIC_OAUTH_BETAS {
        if seen.insert(beta.to_string()) {
            betas.push(beta.to_string());
        }
    }
    let value = HeaderValue::from_str(&betas.join(","))
        .map_err(|error| format!("invalid merged Anthropic beta header: {error}"))?;
    headers.insert(HeaderName::from_static("anthropic-beta"), value);
    Ok(())
}

fn ensure_thinking_display(body: &mut Value) -> bool {
    let Some(thinking) = body.get_mut("thinking").and_then(Value::as_object_mut) else {
        return false;
    };
    let Some(kind) = thinking.get("type").and_then(Value::as_str) else {
        return false;
    };
    if !matches!(kind, "enabled" | "adaptive" | "auto") || thinking.contains_key("display") {
        return false;
    }
    thinking.insert(
        "display".to_string(),
        Value::String("summarized".to_string()),
    );
    true
}

fn sign_existing_billing_header(body: &mut Value) -> bool {
    let Some(text) = body
        .pointer("/system/0/text")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return false;
    };
    if !text.starts_with("x-anthropic-billing-header:") {
        return false;
    }
    let Some(unsigned) = zero_cch(&text) else {
        return false;
    };
    if let Some(slot) = body.pointer_mut("/system/0/text") {
        *slot = Value::String(unsigned);
    } else {
        return false;
    }
    let Ok(unsigned_body) = serde_json::to_vec(body) else {
        return false;
    };
    let cch = format!(
        "{:05x}",
        XxHash64::oneshot(CCH_SEED, &unsigned_body) & 0xF_FFFF
    );
    let Some(unsigned_text) = body
        .pointer("/system/0/text")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return false;
    };
    if let Some(slot) = body.pointer_mut("/system/0/text") {
        *slot = Value::String(unsigned_text.replacen("cch=00000;", &format!("cch={cch};"), 1));
        return true;
    }
    false
}

fn zero_cch(text: &str) -> Option<String> {
    let start = text.find("cch=")? + "cch=".len();
    let end = start.checked_add(5)?;
    let value = text.get(start..end)?;
    if !value.bytes().all(|byte| byte.is_ascii_hexdigit()) || text.get(end..end + 1)? != ";" {
        return None;
    }
    let mut unsigned = text.to_string();
    unsigned.replace_range(start..end, "00000");
    Some(unsigned)
}

fn stable_session_id(provider_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"tiygate/anthropic-oauth-session/");
    hasher.update(provider_id.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0F) | 0x50;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    Uuid::from_bytes(bytes).to_string()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use tiygate_core::provider::oauth::{OAuthTargetConfig, TokenRequestStyle, UpstreamTransport};
    use tiygate_core::{ProtocolEndpoint, ProtocolSuite};

    fn target(profile: OAuthEgressProfile) -> RoutingTarget {
        RoutingTarget {
            provider_id: "anthropic-oauth".to_string(),
            model_id: "claude-test".to_string(),
            api_base: "https://api.anthropic.com/v1".to_string(),
            api_key: String::new(),
            api_protocol: ProtocolEndpoint::new(ProtocolSuite::AnthropicMessages, "messages", "v1"),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
            oauth: Some(OAuthTargetConfig {
                upstream_transport: UpstreamTransport::Http,
                egress_profile: profile,
                token_url: "https://example.test/token".to_string(),
                client_id: "client".to_string(),
                client_secret: None,
                refresh_token: "refresh".to_string(),
                scopes: vec![],
                token_request_style: TokenRequestStyle::Json,
                authorization_header: None,
                authorization_prefix: None,
                extra_headers: vec![],
                account_id: None,
            }),
        }
    }

    #[test]
    fn profile_only_matches_anthropic_oauth_messages_egress() {
        assert!(is_enabled(
            &target(OAuthEgressProfile::AnthropicOAuth),
            ProtocolSuite::AnthropicMessages
        ));
        assert!(!is_enabled(
            &target(OAuthEgressProfile::Standard),
            ProtocolSuite::AnthropicMessages
        ));
        assert!(!is_enabled(
            &target(OAuthEgressProfile::AnthropicOAuth),
            ProtocolSuite::OpenAiCompatible
        ));
    }

    #[test]
    fn headers_merge_betas_and_preserve_authentication() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer access-token"),
        );
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("custom-beta,oauth-2025-04-20"),
        );
        apply_headers(
            &target(OAuthEgressProfile::AnthropicOAuth),
            &mut headers,
            true,
            "018f0000-0000-7000-8000-000000000000",
        )
        .unwrap();

        assert_eq!(headers["authorization"], "Bearer access-token");
        assert_eq!(headers["accept"], "text/event-stream");
        assert_eq!(headers["accept-encoding"], "identity");
        assert!(!headers.contains_key(http::header::CONNECTION));
        assert!(headers["anthropic-beta"]
            .to_str()
            .unwrap()
            .contains("custom-beta"));
        assert!(headers["anthropic-beta"]
            .to_str()
            .unwrap()
            .contains("oauth-2025-04-20"));
        assert_eq!(headers["x-app"], "cli");
    }

    #[test]
    fn body_adds_thinking_display_and_signs_existing_billing_header() {
        let mut body = serde_json::json!({
            "thinking": {"type": "enabled"},
            "system": [{"type": "text", "text": "x-anthropic-billing-header: cch=00000;"}]
        });
        assert!(prepare_body(&mut body));
        assert_eq!(body["thinking"]["display"], "summarized");
        let billing = body["system"][0]["text"].as_str().unwrap();
        assert!(billing.starts_with("x-anthropic-billing-header:"));
        assert!(!billing.contains("cch=00000;"));
    }
}
