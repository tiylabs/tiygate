//! OAuth 2.0 target configuration — pure data types shared between
//! the data plane (`RoutingTarget.oauth`) and the auth crate.
//!
//! These types carry no I/O and no provider-specific logic; they
//! describe *what* a routing target needs to perform an OAuth token
//! refresh (token endpoint, client credentials, scopes, request
//! style). The `auth` crate provides the `OAuthTokenCache` that
//! performs the actual refresh; `store` populates these fields from
//! the DB; `server` threads them into `apply_provider_auth`.

use serde::{Deserialize, Serialize};

/// Transport used for an OAuth provider's data-plane requests.
///
/// OAuth providers normally use HTTP. Codex's ChatGPT Responses surface is
/// an exception: requests are created through a WebSocket session while the
/// gateway continues to expose the normal HTTP/SSE API to its own clients.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamTransport {
    /// Conventional HTTP request, optionally followed by an SSE response.
    #[default]
    Http,
    /// OpenAI Codex Responses WebSocket (`response.create`) transport.
    #[serde(rename = "codex_responses_websocket")]
    CodexResponsesWebSocket,
}

/// How the token endpoint expects the refresh / exchange request body.
///
/// Most providers use the standard `application/x-www-form-urlencoded` body.
/// Anthropic (Claude)
/// requires a JSON body — a critical divergence that the `oauth2`
/// crate does not support, which is why the auth crate implements
/// the token exchange directly with `reqwest`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TokenRequestStyle {
    /// `application/x-www-form-urlencoded` body (RFC 6749 default).
    Form,
    /// `application/json` body (Anthropic-specific).
    Json,
}

/// Provider-owned egress behavior for OAuth credentials.
///
/// This remains a pure routing-data choice: the server owns the HTTP client,
/// TLS settings, and request mutation that implement the selected profile.
/// Existing persisted configurations use the standard profile.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OAuthEgressProfile {
    /// Use the generic upstream HTTP path.
    #[default]
    Standard,
    /// Apply the OpenAI Codex OAuth Responses egress contract.
    #[serde(rename = "openai_codex")]
    OpenAiCodex,
    /// Apply the isolated Anthropic OAuth Messages egress profile.
    #[serde(rename = "anthropic_oauth")]
    AnthropicOAuth,
}

/// OAuth configuration carried on a `RoutingTarget` when the
/// provider is configured with `AuthMode::OAuth`.
///
/// This struct is populated by `snapshot_to_routing_table` from
/// the provider's decrypted `encrypted_oauth_meta` column and
/// provider metadata. It gives the data-plane auth path everything
/// it needs to refresh and inject an access token without touching
/// the DB on the hot path.
///
/// The `refresh_token` field is `#[serde(skip)]` so it is never
/// serialised into logs, snapshots, or debug output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTargetConfig {
    /// Data-plane transport selected for this OAuth provider. Kept here so
    /// the routing core remains pure data while the server owns all socket
    /// I/O. Existing persisted configurations deserialize to HTTP.
    #[serde(default)]
    pub upstream_transport: UpstreamTransport,
    /// Provider-specific egress behavior selected from the provider identity.
    #[serde(default)]
    pub egress_profile: OAuthEgressProfile,
    /// Token endpoint URL for refresh / exchange.
    pub token_url: String,
    /// OAuth client identifier (public client — no secret needed
    /// for the built-in OAuth providers).
    pub client_id: String,
    /// Optional client secret. `None` for public clients (Codex,
    /// Claude use PKCE-only public clients).
    #[serde(default, skip_serializing)]
    pub client_secret: Option<String>,
    /// The current refresh token. Populated from the DB; updated
    /// in-memory by the `OAuthTokenCache` after each refresh (refresh
    /// token rotation). Never serialised.
    #[serde(skip)]
    pub refresh_token: String,
    /// Scopes to include in the refresh request.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Whether the token endpoint expects form-encoded or JSON body.
    pub token_request_style: TokenRequestStyle,
    /// Header name for the access token. Defaults to `"authorization"`.
    #[serde(default)]
    pub authorization_header: Option<String>,
    /// Prefix for the access token value (e.g. `"Bearer "`).
    /// Defaults to `"Bearer "`.
    #[serde(default)]
    pub authorization_prefix: Option<String>,
    /// Extra headers to inject alongside the access token
    /// (provider-specific, e.g. `anthropic-beta: oauth-2025-04-20`
    /// for Claude OAuth).
    #[serde(default)]
    pub extra_headers: Vec<(String, String)>,
    /// Stable upstream account/workspace identifier associated with
    /// this credential. OAuth access tokens are cached per provider
    /// credential (not per routed model), using this value when present.
    #[serde(default)]
    pub account_id: Option<String>,
}

impl OAuthTargetConfig {
    /// Returns the header name to use for the access token,
    /// defaulting to `"authorization"`.
    pub fn header_name(&self) -> &str {
        self.authorization_header
            .as_deref()
            .unwrap_or("authorization")
    }

    /// Returns the prefix for the access token value, defaulting
    /// to `"Bearer "`.
    pub fn bearer_prefix(&self) -> &str {
        self.authorization_prefix.as_deref().unwrap_or("Bearer ")
    }

    /// Cache label shared by every route using this provider credential.
    /// A provider row owns one OAuth credential, so model or route labels
    /// must not split refresh-token rotation into independent caches.
    pub fn cache_label(&self) -> &str {
        self.account_id.as_deref().unwrap_or("__provider__")
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn token_request_style_serde() {
        let form = TokenRequestStyle::Form;
        let json_str = serde_json::to_string(&form).unwrap();
        assert_eq!(json_str, "\"form\"");

        let json = TokenRequestStyle::Json;
        let json_str = serde_json::to_string(&json).unwrap();
        assert_eq!(json_str, "\"json\"");

        let round_trip: TokenRequestStyle = serde_json::from_str("\"json\"").unwrap();
        assert_eq!(round_trip, TokenRequestStyle::Json);
    }

    #[test]
    fn oauth_egress_profile_serde_names_are_stable() {
        assert_eq!(
            serde_json::to_string(&OAuthEgressProfile::OpenAiCodex).unwrap(),
            "\"openai_codex\""
        );
        assert_eq!(
            serde_json::to_string(&OAuthEgressProfile::AnthropicOAuth).unwrap(),
            "\"anthropic_oauth\""
        );
    }

    #[test]
    fn oauth_target_config_skip_serializing_refresh_token() {
        let cfg = OAuthTargetConfig {
            upstream_transport: UpstreamTransport::Http,
            egress_profile: OAuthEgressProfile::Standard,
            token_url: "https://example.com/token".to_string(),
            client_id: "test-client".to_string(),
            client_secret: None,
            refresh_token: "secret-refresh-token".to_string(),
            scopes: vec!["openid".to_string()],
            token_request_style: TokenRequestStyle::Form,
            authorization_header: None,
            authorization_prefix: None,
            extra_headers: vec![],
            account_id: None,
        };
        let json = serde_json::to_value(&cfg).unwrap();
        // refresh_token must not appear in serialised output.
        assert!(json.get("refresh_token").is_none());
        // client_secret is skip_serializing when None.
        assert!(json.get("client_secret").is_none());
        // token_url and client_id should be present.
        assert_eq!(json["token_url"], "https://example.com/token");
        assert_eq!(json["client_id"], "test-client");
    }

    #[test]
    fn oauth_target_config_defaults() {
        let cfg = OAuthTargetConfig {
            upstream_transport: UpstreamTransport::Http,
            egress_profile: OAuthEgressProfile::Standard,
            token_url: String::new(),
            client_id: String::new(),
            client_secret: None,
            refresh_token: String::new(),
            scopes: vec![],
            token_request_style: TokenRequestStyle::Form,
            authorization_header: None,
            authorization_prefix: None,
            extra_headers: vec![],
            account_id: None,
        };
        assert_eq!(cfg.header_name(), "authorization");
        assert_eq!(cfg.bearer_prefix(), "Bearer ");
        assert_eq!(cfg.cache_label(), "__provider__");
    }
}
