//! Google Gemini (Public) provider implementation.
//!
//! Auth follows the official Google AI for Developers spec:
//! - Primary: `x-goog-api-key: <KEY>` header
//! - Alternative: `?key=<KEY>` query string
//!
//! Base URL defaults to the public Gemini endpoint
//! `https://generativelanguage.googleapis.com/v1beta`.

use std::sync::Arc;

use tiygate_auth::api_key::HeaderApiKeyAuthApplier;
use tiygate_core::{
    AuthApplier, AuthMode, ProtocolEndpoint, ProtocolSuite, Provider, ProviderMetadata,
    RoutingTarget,
};

/// Vendor identifier used by the admin/config layer to bind a `Provider`
/// row to this implementation.
pub const GEMINI_VENDOR_ID: &str = "gemini";

/// Default upstream base URL for the Google AI for Developers Public Gemini
/// endpoint. The full request path is `…/v1beta/models/{model}:generateContent`
/// (or `:streamGenerateContent?alt=sse`) — the trailing `/v1beta` is
/// intentionally part of the base so that `target.effective_api_base()` is a
/// drop-in for the `gemini_aware_upstream_url` / `upstream_stream_url_for_suite`
/// helpers in `crates/server/src/ingress.rs`.
pub const GEMINI_DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Header name used by the official Public Gemini API for static API keys.
pub const GEMINI_API_KEY_HEADER: &str = "x-goog-api-key";

/// Query parameter name used by the official Public Gemini API for static
/// API keys (alternative to the `x-goog-api-key` header).
pub const GEMINI_API_KEY_QUERY_PARAM: &str = "key";

pub struct GeminiProvider {
    metadata: ProviderMetadata,
}

impl Default for GeminiProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl GeminiProvider {
    pub fn new() -> Self {
        Self {
            metadata: ProviderMetadata {
                display_name: "Google Gemini".to_string(),
                base_url: GEMINI_DEFAULT_BASE_URL.to_string(),
                auth_mode: AuthMode::ApiKey {
                    header_name: GEMINI_API_KEY_HEADER.to_string(),
                },
                channels: vec!["default".to_string()],
                protocols: vec![ProtocolEndpoint::new(
                    ProtocolSuite::GoogleGemini,
                    "generateContent",
                    "v1beta",
                )],
                defaults: serde_json::json!({}),
            },
        }
    }
}

impl Provider for GeminiProvider {
    fn id(&self) -> &str {
        GEMINI_VENDOR_ID
    }

    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    fn supported_protocols(&self) -> &[ProtocolEndpoint] {
        &self.metadata.protocols
    }

    fn auth(&self) -> Arc<dyn AuthApplier> {
        Arc::new(HeaderApiKeyAuthApplier {
            header_name: GEMINI_API_KEY_HEADER.to_string(),
        })
    }

    fn egress_protocol_for_model(&self, _model_id: &str) -> ProtocolEndpoint {
        ProtocolEndpoint::new(ProtocolSuite::GoogleGemini, "generateContent", "v1beta")
    }
}

/// Helper used by the protocol-aware fallback in
/// `crates/server/src/ingress.rs::apply_provider_auth`. Applies the
/// `x-goog-api-key` header directly so the call site does not need to know
/// the header name.
pub fn apply_gemini_default_auth(
    target: &RoutingTarget,
    upstream_headers: &mut http::HeaderMap,
) -> Result<(), http::header::InvalidHeaderValue> {
    let key = target.effective_api_key();
    let hv = http::HeaderValue::from_str(key)?;
    upstream_headers.insert(http::HeaderName::from_static(GEMINI_API_KEY_HEADER), hv);
    Ok(())
}

/// Append the `?key=<API_KEY>` query parameter to an upstream URL while
/// preserving any existing query string (e.g. `?alt=sse` for streaming).
///
/// Returns `None` if the resulting URL would be malformed; callers should
/// treat that as a hard error.
pub fn append_api_key_query(base_url: &str, api_key: &str) -> Option<String> {
    let key_percent_encoded = percent_encode_query_value(api_key);
    let separator = if base_url.contains('?') { '&' } else { '?' };
    Some(format!(
        "{base_url}{separator}{}={key_percent_encoded}",
        GEMINI_API_KEY_QUERY_PARAM
    ))
}

/// Conservative percent-encoding for the API key query value. API keys
/// from AI Studio are typically `AIza…` and contain no reserved
/// characters, but we still encode anything that is not URL-safe to avoid
/// breaking on operator-provided keys that may include `+`, `/`, `=`, etc.
fn percent_encode_query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            // unreserved (RFC 3986)
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

inventory::submit! {
    tiygate_core::provider::ProviderRegistration {
        make: || Box::new(GeminiProvider::new()),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use http::HeaderMap;

    fn dummy_target(key: &str) -> RoutingTarget {
        RoutingTarget {
            provider_id: GEMINI_VENDOR_ID.to_string(),
            model_id: "gemini-1.5-pro".to_string(),
            api_base: GEMINI_DEFAULT_BASE_URL.to_string(),
            api_key: key.to_string(),
            api_protocol: ProtocolEndpoint::new(
                ProtocolSuite::GoogleGemini,
                "generateContent",
                "v1beta",
            ),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
            oauth: None,
        }
    }

    #[test]
    fn test_gemini_provider_id_and_metadata() {
        let p = GeminiProvider::new();
        assert_eq!(p.id(), "gemini");
        assert_eq!(p.metadata().display_name, "Google Gemini");
        assert_eq!(p.metadata().base_url, GEMINI_DEFAULT_BASE_URL);
        match &p.metadata().auth_mode {
            AuthMode::ApiKey { header_name } => {
                assert_eq!(header_name, GEMINI_API_KEY_HEADER);
            }
            other => panic!("unexpected auth_mode: {other:?}"),
        }
    }

    #[test]
    fn test_gemini_supported_protocols() {
        let p = GeminiProvider::new();
        let protocols = p.supported_protocols();
        assert_eq!(protocols.len(), 1);
        assert_eq!(protocols[0].suite, ProtocolSuite::GoogleGemini);
    }

    #[tokio::test]
    async fn test_gemini_auth_applier_sets_x_goog_api_key_header() {
        let p = GeminiProvider::new();
        let auth = p.auth();
        let mut headers = HeaderMap::new();
        auth.apply(&mut headers, &dummy_target("AIza-test-key"))
            .await
            .expect("auth apply should succeed");
        let v = headers
            .get(GEMINI_API_KEY_HEADER)
            .expect("x-goog-api-key header must be present");
        assert_eq!(v.to_str().unwrap(), "AIza-test-key");
        // We must NOT also emit an Authorization: Bearer header for
        // Public Gemini — Google rejects that combination.
        assert!(
            headers.get(http::header::AUTHORIZATION).is_none(),
            "Public Gemini must not receive an Authorization: Bearer header"
        );
    }

    #[test]
    fn test_apply_gemini_default_auth_helper() {
        let mut headers = HeaderMap::new();
        apply_gemini_default_auth(&dummy_target("k1"), &mut headers).unwrap();
        assert_eq!(headers.get(GEMINI_API_KEY_HEADER).unwrap(), "k1");
    }

    #[test]
    fn test_append_api_key_query_no_existing_query() {
        let u = append_api_key_query(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-pro:generateContent",
            "AIza-abc",
        )
        .unwrap();
        assert!(u.ends_with("?key=AIza-abc"), "got {u}");
    }

    #[test]
    fn test_append_api_key_query_preserves_existing_query() {
        let u = append_api_key_query(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-pro:streamGenerateContent?alt=sse",
            "AIza-abc",
        )
        .unwrap();
        assert!(
            u.contains("alt=sse"),
            "alt=sse should be preserved, got {u}"
        );
        assert!(
            u.contains("&key=AIza-abc"),
            "key should be appended with &, got {u}"
        );
        assert!(
            !u.contains("?key"),
            "must not use ? when alt= present, got {u}"
        );
    }

    #[test]
    fn test_append_api_key_query_percent_encodes_reserved() {
        let u = append_api_key_query("https://x.test/path", "k+y/=z").unwrap();
        assert!(u.contains("k%2By%2F%3Dz"), "got {u}");
    }
}
