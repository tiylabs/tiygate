//! Provider OAuth 2.0 — PKCE, token exchange, refresh, and global
//! token cache for the three supported OAuth providers:
//! Codex (OpenAI), Claude (Anthropic), and xAI (Grok).
//!
//! This module replaces the `oauth2` crate's `BasicClient` with
//! direct `reqwest` calls so we can support both form-encoded
//! (Codex/xAI) and JSON body (Claude) token exchange formats.
//!
//! ## Architecture
//!
//! - `OAuthProviderPreset` — static configuration for each provider
//!   (auth URL, token URL, client ID, scopes, redirect URL, etc.).
//! - `generate_pkce()` — PKCE code verifier/challenge pair (S256).
//! - `build_authorize_url()` — construct the authorization URL with
//!   PKCE + extra provider-specific params.
//! - `exchange_code()` / `refresh_token()` — token endpoint calls
//!   via `reqwest`, supporting both form and JSON body styles.
//! - `OAuthTokenCache` — process-global cache keyed by
//!   `provider_id:label`, with per-key single-flight refresh.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine;
use dashmap::DashMap;
use http::HeaderMap;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tiygate_core::provider::oauth::{OAuthTargetConfig, TokenRequestStyle};
use tracing::info;

/// Leeway before token expiry to trigger a proactive refresh.
const REFRESH_LEEWAY: Duration = Duration::from_secs(60);

/// Codex protocol version advertised to the ChatGPT backend.
///
/// This is deliberately independent from TiyGate's package version. The
/// Codex models endpoint uses `client_version` as a compatibility gate and
/// returns an empty model list for obsolete client versions.
pub const CODEX_CLIENT_VERSION: &str = "0.144.0-alpha.4";

/// Stable, non-secret classification of a failed OAuth refresh. Callers may
/// persist these values for operator-facing health without exposing the raw
/// authorization-server response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthRefreshFailureKind {
    CredentialInvalid,
    Transient,
}

impl OAuthRefreshFailureKind {
    pub fn status_reason(self) -> &'static str {
        match self {
            Self::CredentialInvalid => "credential_rejected",
            Self::Transient => "refresh_failed",
        }
    }
}

/// Classify a refresh failure while deliberately discarding the raw response,
/// which may contain provider-specific details unsuitable for the Admin API.
pub fn classify_refresh_failure(error: &str) -> OAuthRefreshFailureKind {
    let error = error.to_ascii_lowercase();
    let credential_rejected = [
        "401 unauthorized",
        "403 forbidden",
        "invalid_grant",
        "invalid refresh token",
        "refresh_token_reused",
        "refresh token has expired",
        "refresh token is expired",
        "token revoked",
    ]
    .iter()
    .any(|marker| error.contains(marker));
    if credential_rejected {
        OAuthRefreshFailureKind::CredentialInvalid
    } else {
        OAuthRefreshFailureKind::Transient
    }
}

// -----------------------------------------------------------------------
// PKCE
// -----------------------------------------------------------------------

/// Generate a PKCE verifier/challenge pair using S256.
///
/// The verifier is 96 random bytes encoded as base64url (no padding),
/// yielding a 128-character string — matching the CLIProxyAPI
/// reference implementation. The challenge is
/// `base64url(SHA256(verifier))`.
pub fn generate_pkce() -> (String, String) {
    let mut bytes = [0u8; 96];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let challenge = pkce_challenge(&verifier);
    (verifier, challenge)
}

/// Compute the S256 PKCE challenge from a verifier string.
fn pkce_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

// -----------------------------------------------------------------------
// Provider presets
// -----------------------------------------------------------------------

/// Static OAuth configuration for a supported provider.
///
/// Contains everything the admin flow (authorize URL) and the data
/// plane (token refresh) need. The `vendor` field selects which
/// preset to use; it maps to the `providers.vendor` DB column.
#[derive(Debug, Clone)]
pub struct OAuthProviderPreset {
    /// Provider vendor identifier (e.g. `"openai"`, `"anthropic"`, `"xai"`).
    pub vendor: String,
    /// Authorization endpoint URL.
    pub auth_url: String,
    /// Token endpoint URL.
    pub token_url: String,
    /// OAuth client identifier (public client, no secret).
    pub client_id: String,
    /// Redirect URI registered with the provider.
    pub redirect_url: String,
    /// Scopes to request.
    pub scopes: Vec<String>,
    /// Authorization-code exchange request body style.
    pub exchange_request_style: TokenRequestStyle,
    /// Refresh-token request body style. Some providers use different
    /// encodings for exchange and refresh (Codex is form + JSON).
    pub refresh_request_style: TokenRequestStyle,
    /// Whether the authorization-code exchange includes scopes.
    pub send_scopes_in_exchange_request: bool,
    /// Scopes sent specifically during refresh. This is separate from the
    /// authorize scopes because Codex uses a smaller refresh scope set.
    pub refresh_scopes: Vec<String>,
    /// Extra query parameters to append to the authorize URL
    /// (provider-specific, e.g. `prompt=login` for Codex).
    pub extra_authorize_params: Vec<(String, String)>,
}

/// Codex (OpenAI) OAuth preset.
pub fn codex_preset() -> OAuthProviderPreset {
    OAuthProviderPreset {
        vendor: "openai".to_string(),
        auth_url: "https://auth.openai.com/oauth/authorize".to_string(),
        token_url: "https://auth.openai.com/oauth/token".to_string(),
        client_id: "app_EMoamEEZ73f0CkXaXp7hrann".to_string(),
        redirect_url: "http://localhost:1455/auth/callback".to_string(),
        scopes: vec![
            "openid".to_string(),
            "email".to_string(),
            "profile".to_string(),
            "offline_access".to_string(),
        ],
        exchange_request_style: TokenRequestStyle::Form,
        refresh_request_style: TokenRequestStyle::Form,
        send_scopes_in_exchange_request: false,
        refresh_scopes: vec![
            "openid".to_string(),
            "profile".to_string(),
            "email".to_string(),
        ],
        extra_authorize_params: vec![
            ("prompt".to_string(), "login".to_string()),
            ("id_token_add_organizations".to_string(), "true".to_string()),
            ("codex_cli_simplified_flow".to_string(), "true".to_string()),
        ],
    }
}

/// Claude (Anthropic) OAuth preset.
pub fn claude_preset() -> OAuthProviderPreset {
    OAuthProviderPreset {
        vendor: "anthropic".to_string(),
        auth_url: "https://claude.ai/oauth/authorize".to_string(),
        token_url: "https://api.anthropic.com/v1/oauth/token".to_string(),
        client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_string(),
        redirect_url: "http://localhost:54545/callback".to_string(),
        scopes: vec![
            "user:profile".to_string(),
            "user:inference".to_string(),
            "user:sessions:claude_code".to_string(),
            "user:mcp_servers".to_string(),
            "user:file_upload".to_string(),
        ],
        exchange_request_style: TokenRequestStyle::Json,
        refresh_request_style: TokenRequestStyle::Json,
        send_scopes_in_exchange_request: true,
        refresh_scopes: vec![
            "user:profile".to_string(),
            "user:inference".to_string(),
            "user:sessions:claude_code".to_string(),
            "user:mcp_servers".to_string(),
            "user:file_upload".to_string(),
        ],
        extra_authorize_params: vec![("code".to_string(), "true".to_string())],
    }
}

/// xAI (Grok) OAuth preset.
pub fn xai_preset() -> OAuthProviderPreset {
    OAuthProviderPreset {
        vendor: "xai".to_string(),
        auth_url: "https://auth.x.ai/oauth2/authorize".to_string(),
        token_url: "https://auth.x.ai/oauth2/token".to_string(),
        client_id: "b1a00492-073a-47ea-816f-4c329264a828".to_string(),
        redirect_url: "http://127.0.0.1:56121/callback".to_string(),
        scopes: vec![
            "openid".to_string(),
            "profile".to_string(),
            "email".to_string(),
            "offline_access".to_string(),
            "grok-cli:access".to_string(),
            "api:access".to_string(),
        ],
        exchange_request_style: TokenRequestStyle::Form,
        refresh_request_style: TokenRequestStyle::Form,
        send_scopes_in_exchange_request: true,
        refresh_scopes: vec![
            "openid".to_string(),
            "profile".to_string(),
            "email".to_string(),
            "offline_access".to_string(),
            "grok-cli:access".to_string(),
            "api:access".to_string(),
        ],
        extra_authorize_params: vec![
            ("plan".to_string(), "generic".to_string()),
            ("referrer".to_string(), "tiygate".to_string()),
        ],
    }
}

/// Look up the OAuth preset for a given vendor string.
///
/// Returns `None` for vendors without a built-in OAuth preset;
/// the caller should fall back to a custom config in that case.
pub fn preset_for_vendor(vendor: &str) -> Option<OAuthProviderPreset> {
    match vendor {
        "openai" => Some(codex_preset()),
        "anthropic" => Some(claude_preset()),
        "xai" => Some(xai_preset()),
        _ => None,
    }
}

// -----------------------------------------------------------------------
// Authorize URL
// -----------------------------------------------------------------------

/// Build the authorization URL with PKCE challenge, state, scopes,
/// and provider-specific extra parameters.
pub fn build_authorize_url(
    preset: &OAuthProviderPreset,
    state: &str,
    pkce_challenge: &str,
) -> String {
    use url::form_urlencoded;
    let mut encoder = form_urlencoded::Serializer::new(String::new());
    encoder
        .append_pair("response_type", "code")
        .append_pair("client_id", &preset.client_id)
        .append_pair("redirect_uri", &preset.redirect_url)
        .append_pair("scope", &preset.scopes.join(" "))
        .append_pair("state", state)
        .append_pair("code_challenge", pkce_challenge)
        .append_pair("code_challenge_method", "S256");
    for (k, v) in &preset.extra_authorize_params {
        encoder.append_pair(k, v);
    }
    let query = encoder.finish();
    format!("{}?{}", preset.auth_url, query)
}

// -----------------------------------------------------------------------
// Token response
// -----------------------------------------------------------------------

/// Parsed token endpoint response.
#[derive(Debug, Clone, Deserialize)]
struct TokenResponseRaw {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    id_token: Option<String>,
    account_id: Option<String>,
    email: Option<String>,
    account_email: Option<String>,
}

/// Normalized token response used internally.
#[derive(Debug, Clone)]
pub struct TokenResult {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<Duration>,
    pub account_id: Option<String>,
    pub account_email: Option<String>,
}

/// Sanitized result returned to control-plane callers. Access and refresh
/// token values deliberately never cross this interface.
#[derive(Debug, Clone, Copy)]
pub struct OAuthCredentialRefreshSummary {
    pub expires_in: Option<Duration>,
}

/// Control-plane mutation executed while the server owns the provider's OAuth
/// refresh lock. The boxed shape keeps [`OAuthCredentialService`] object-safe.
pub type OAuthProviderMutation = Box<
    dyn FnOnce() -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>
        + Send
        + 'static,
>;

/// Shared control-plane surface implemented by the server's cluster-aware
/// token manager. Keeping the trait in `auth` avoids an `admin -> server`
/// dependency while ensuring Admin handlers cannot bypass refresh coordination.
#[async_trait::async_trait]
pub trait OAuthCredentialService: Send + Sync {
    async fn apply_provider_headers(
        &self,
        provider_id: &str,
        headers: &mut HeaderMap,
    ) -> Result<(), String>;

    async fn force_refresh_provider(
        &self,
        provider_id: &str,
    ) -> Result<OAuthCredentialRefreshSummary, String>;

    async fn install_provider_tokens(
        &self,
        provider_id: &str,
        tokens: TokenResult,
    ) -> Result<OAuthCredentialRefreshSummary, String>;

    /// Execute a provider credential edit while holding the same lock used by
    /// token refresh, then clear the old shared and local access-token state.
    async fn mutate_provider_credentials(
        &self,
        provider_id: &str,
        mutation: OAuthProviderMutation,
    ) -> Result<(), String>;
}

// -----------------------------------------------------------------------
// Token exchange / refresh
// -----------------------------------------------------------------------

/// Exchange an authorization code for tokens.
///
/// Uses form-encoded body for Codex/xAI and JSON body for Claude,
/// per each provider's token endpoint requirements.
pub async fn exchange_code(
    preset: &OAuthProviderPreset,
    code: &str,
    pkce_verifier: &str,
    http_client: &reqwest::Client,
) -> Result<TokenResult, String> {
    let scopes = preset.scopes.join(" ");
    match preset.exchange_request_style {
        TokenRequestStyle::Form => {
            let mut params = vec![
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", preset.client_id.as_str()),
                ("redirect_uri", preset.redirect_url.as_str()),
                ("code_verifier", pkce_verifier),
            ];
            if preset.send_scopes_in_exchange_request {
                params.push(("scope", scopes.as_str()));
            }
            let resp = http_client
                .post(&preset.token_url)
                .form(&params)
                .send()
                .await
                .map_err(|e| format!("token exchange request failed: {e}"))?;
            parse_token_response(resp).await
        }
        TokenRequestStyle::Json => {
            let mut body = serde_json::json!({
                "grant_type": "authorization_code",
                "code": code,
                "client_id": preset.client_id,
                "redirect_uri": preset.redirect_url,
                "code_verifier": pkce_verifier,
            });
            if preset.send_scopes_in_exchange_request {
                body["scope"] = serde_json::json!(scopes);
            }
            let resp = http_client
                .post(&preset.token_url)
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("token exchange request failed: {e}"))?;
            parse_token_response(resp).await
        }
    }
}

/// Refresh an access token using a refresh token.
///
/// Uses form-encoded body for Codex/xAI and JSON body for Claude.
/// The returned `TokenResult` may contain a new `refresh_token`
/// (token rotation) — the caller must persist it.
pub async fn do_refresh_token(
    token_url: &str,
    client_id: &str,
    refresh_token: &str,
    scopes: &[String],
    style: &TokenRequestStyle,
    http_client: &reqwest::Client,
) -> Result<TokenResult, String> {
    let scopes_str = scopes.join(" ");
    match style {
        TokenRequestStyle::Form => {
            let mut params = vec![
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", client_id),
            ];
            if !scopes_str.is_empty() {
                params.push(("scope", scopes_str.as_str()));
            }
            let resp = http_client
                .post(token_url)
                .form(&params)
                .send()
                .await
                .map_err(|e| format!("token refresh request failed: {e}"))?;
            parse_token_response(resp).await
        }
        TokenRequestStyle::Json => {
            let mut body = serde_json::json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
                "client_id": client_id,
            });
            if !scopes_str.is_empty() {
                body["scope"] = serde_json::json!(scopes_str);
            }
            let resp = http_client
                .post(token_url)
                .header("Content-Type", "application/json")
                .json(&body)
                .send()
                .await
                .map_err(|e| format!("token refresh request failed: {e}"))?;
            parse_token_response(resp).await
        }
    }
}

/// Parse a token endpoint HTTP response, extracting access_token,
/// refresh_token, and expires_in.
async fn parse_token_response(resp: reqwest::Response) -> Result<TokenResult, String> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("token endpoint returned {status}: {body}"));
    }
    let raw: TokenResponseRaw = resp
        .json()
        .await
        .map_err(|e| format!("failed to parse token response: {e}"))?;
    let account_id = raw
        .account_id
        .filter(|value| !value.is_empty())
        .or_else(|| raw.id_token.as_deref().and_then(chatgpt_account_id));
    let account_email = raw
        .account_email
        .or(raw.email)
        .filter(|value| !value.is_empty())
        .or_else(|| raw.id_token.as_deref().and_then(chatgpt_account_email));
    Ok(TokenResult {
        access_token: raw.access_token,
        refresh_token: raw.refresh_token,
        expires_in: raw.expires_in.map(Duration::from_secs),
        account_id,
        account_email,
    })
}

/// Extract the ChatGPT workspace/account identifier from an ID-token JWT.
/// Signature verification is performed by the OAuth issuer; this local parse
/// only reads the identity claim needed to scope subsequent requests.
fn chatgpt_account_id(id_token: &str) -> Option<String> {
    let claims = jwt_claims(id_token)?;
    claims
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .or_else(|| claims.get("chatgpt_account_id"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Extract the email address from an OpenAI ID-token JWT. OpenAI currently
/// emits the standard OIDC `email` claim, while accepting the namespaced
/// variants keeps this compatible with older token shapes.
fn chatgpt_account_email(id_token: &str) -> Option<String> {
    let claims = jwt_claims(id_token)?;
    claims
        .get("email")
        .or_else(|| {
            claims
                .get("https://api.openai.com/profile")
                .and_then(|profile| profile.get("email"))
        })
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(|auth| auth.get("email"))
        })
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn jwt_claims(id_token: &str) -> Option<serde_json::Value> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

// -----------------------------------------------------------------------
// Global token cache
// -----------------------------------------------------------------------

/// Non-secret identity claims learned from an OAuth token response.
#[derive(Debug, Clone, Default)]
pub struct OAuthTokenIdentity {
    pub account_id: Option<String>,
    pub account_email: Option<String>,
}

/// Cached token entry. The `refresh_token` is updated in-place when
/// the provider rotates it during a refresh.
#[derive(Debug, Clone)]
struct CachedToken {
    access_token: String,
    refresh_token: String,
    expires_at: Option<Instant>,
    account_id: Option<String>,
    account_email: Option<String>,
}

impl CachedToken {
    fn is_expiring(&self) -> bool {
        match self.expires_at {
            Some(t) => t.saturating_duration_since(Instant::now()) <= REFRESH_LEEWAY,
            None => false,
        }
    }
}

/// Process-global OAuth token cache.
///
/// Uses `OnceLock<DashMap>` so the cache is shared across all
/// `OAuthTokenManager` instances and all requests. Per-key
/// single-flight refresh is enforced via per-key `tokio::sync::Mutex`
/// stored in a separate `DashMap`.
///
/// The cache is keyed by `"{provider_id}:{label}"` where `label` is
/// the `account_label` (or `model_id` if no label is set).
pub struct OAuthTokenCache {
    tokens: OnceLock<DashMap<String, CachedToken>>,
    inflight: OnceLock<DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl OAuthTokenCache {
    /// Create a new empty cache. The underlying `DashMap`s are
    /// lazily initialised on first access via `OnceLock`.
    pub fn new() -> Self {
        Self {
            tokens: OnceLock::new(),
            inflight: OnceLock::new(),
        }
    }

    /// Returns a process-global shared `OAuthTokenCache`. All callers
    /// — data plane, admin plane, background tasks — share the same
    /// in-memory token cache so an access token refreshed by one
    /// subsystem is immediately available to all others.
    pub fn global() -> &'static OAuthTokenCache {
        static GLOBAL: std::sync::OnceLock<OAuthTokenCache> = std::sync::OnceLock::new();
        GLOBAL.get_or_init(OAuthTokenCache::new)
    }

    fn tokens(&self) -> &DashMap<String, CachedToken> {
        self.tokens.get_or_init(DashMap::new)
    }

    fn inflight(&self) -> &DashMap<String, Arc<tokio::sync::Mutex<()>>> {
        self.inflight.get_or_init(DashMap::new)
    }

    fn key(provider_id: &str, label: &str) -> String {
        format!("{provider_id}:{label}")
    }

    /// Remove every cached token and refresh mutex for one provider.
    pub fn invalidate_provider(&self, provider_id: &str) {
        let prefix = format!("{provider_id}:");
        self.tokens().retain(|key, _| !key.starts_with(&prefix));
        self.inflight().retain(|key, _| !key.starts_with(&prefix));
    }

    /// Get the mutex for a given key, creating one if it doesn't
    /// exist. All concurrent refreshes for the same key will queue
    /// on this mutex (single-flight).
    fn mutex_for(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.inflight()
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Seed the cache with a refresh token from the DB (cold start).
    /// Does nothing if a cache entry already exists (the cache
    /// may have a newer refresh token from a rotation).
    pub fn seed(&self, provider_id: &str, label: &str, refresh_token: &str) {
        let key = Self::key(provider_id, label);
        let mut entry = self.tokens().entry(key.clone()).or_insert(CachedToken {
            access_token: String::new(),
            refresh_token: refresh_token.to_string(),
            expires_at: None,
            account_id: None,
            account_email: None,
        });
        // Only update refresh_token if the entry is empty (cold start).
        // If we already have a token, the cache's refresh_token may
        // be newer (from a rotation) — don't overwrite it.
        if entry.refresh_token.is_empty() {
            entry.refresh_token = refresh_token.to_string();
        }
    }

    /// Replace the cached credential immediately after an interactive login
    /// or explicit refresh. This preserves the access token returned by the
    /// token endpoint and prevents the first data-plane request from consuming
    /// the refresh token again.
    pub fn seed_tokens(
        &self,
        provider_id: &str,
        label: &str,
        access_token: &str,
        refresh_token: &str,
        expires_in: Option<Duration>,
    ) {
        self.seed_tokens_with_identity(
            provider_id,
            label,
            access_token,
            refresh_token,
            expires_in,
            OAuthTokenIdentity::default(),
        );
    }

    /// Replace the cached credential and retain the non-secret account
    /// identity returned by an OAuth token exchange or refresh.
    pub fn seed_tokens_with_identity(
        &self,
        provider_id: &str,
        label: &str,
        access_token: &str,
        refresh_token: &str,
        expires_in: Option<Duration>,
        identity: OAuthTokenIdentity,
    ) {
        let key = Self::key(provider_id, label);
        self.tokens().insert(
            key,
            CachedToken {
                access_token: access_token.to_string(),
                refresh_token: refresh_token.to_string(),
                expires_at: expires_in.map(|duration| Instant::now() + duration),
                account_id: identity.account_id,
                account_email: identity.account_email,
            },
        );
    }

    /// Apply OAuth authentication to the upstream headers.
    ///
    /// 1. Check the cache for a valid (non-expiring) access token.
    /// 2. If missing/expiring, acquire the per-key mutex (single-flight),
    ///    re-check, then refresh using the cached refresh_token.
    /// 3. Inject `Authorization: Bearer <access_token>` (or the
    ///    configured header/prefix) into `headers`.
    pub async fn apply(
        &self,
        headers: &mut HeaderMap,
        provider_id: &str,
        label: &str,
        oauth: &OAuthTargetConfig,
        http_client: &reqwest::Client,
    ) -> Result<(), String> {
        let key = Self::key(provider_id, label);

        // Fast path: valid cached token.
        if let Some(cached) = self.tokens().get(&key) {
            if !cached.is_expiring() && !cached.access_token.is_empty() {
                inject_token(
                    headers,
                    &cached.access_token,
                    cached.account_id.as_deref().or(oauth.account_id.as_deref()),
                    oauth,
                )?;
                return Ok(());
            }
        }

        // Slow path: single-flight refresh.
        let mutex = self.mutex_for(&key);
        let _guard = mutex.lock().await;

        // Re-check after acquiring the lock (double-checked locking).
        if let Some(cached) = self.tokens().get(&key) {
            if !cached.is_expiring() && !cached.access_token.is_empty() {
                inject_token(
                    headers,
                    &cached.access_token,
                    cached.account_id.as_deref().or(oauth.account_id.as_deref()),
                    oauth,
                )?;
                return Ok(());
            }
        }

        // Get the refresh token from the cache (or fall back to the
        // config's refresh token if the cache was never seeded).
        let refresh_token = self
            .tokens()
            .get(&key)
            .map(|c| c.refresh_token.clone())
            .unwrap_or_else(|| oauth.refresh_token.clone());

        if refresh_token.is_empty() {
            return Err(format!(
                "no refresh_token available for OAuth key {key}; \
                 run the admin OAuth flow first"
            ));
        }

        // Perform the refresh.
        let result = do_refresh_token(
            &oauth.token_url,
            &oauth.client_id,
            &refresh_token,
            &oauth.scopes,
            &oauth.token_request_style,
            http_client,
        )
        .await?;

        // Update the cache with the new tokens.
        let new_refresh = result
            .refresh_token
            .clone()
            .unwrap_or_else(|| refresh_token.clone());
        let expires_at = result.expires_in.map(|d| Instant::now() + d);
        let account_email = result
            .account_email
            .clone()
            .or_else(|| self.get_account_email(provider_id, label));
        let account_id = result
            .account_id
            .clone()
            .or_else(|| self.get_account_id(provider_id, label))
            .or_else(|| oauth.account_id.clone());
        self.tokens().insert(
            key.clone(),
            CachedToken {
                access_token: result.access_token.clone(),
                refresh_token: new_refresh,
                expires_at,
                account_id: account_id.clone(),
                account_email,
            },
        );

        info!(provider = %provider_id, label = %label, "OAuth token refreshed");

        inject_token(headers, &result.access_token, account_id.as_deref(), oauth)?;
        Ok(())
    }

    /// Inject a cached access token only when it is present and not within the
    /// refresh leeway. This never calls the token endpoint. Cluster-aware
    /// managers use it as their zero-I/O fast path before consulting shared
    /// database state and refresh coordination.
    pub fn apply_cached(
        &self,
        headers: &mut HeaderMap,
        provider_id: &str,
        label: &str,
        oauth: &OAuthTargetConfig,
    ) -> Result<bool, String> {
        let key = Self::key(provider_id, label);
        let Some(cached) = self.tokens().get(&key) else {
            return Ok(false);
        };
        if cached.is_expiring() || cached.access_token.is_empty() {
            return Ok(false);
        }
        inject_token(
            headers,
            &cached.access_token,
            cached.account_id.as_deref().or(oauth.account_id.as_deref()),
            oauth,
        )?;
        Ok(true)
    }

    /// Return the current refresh token from the cache for a given
    /// key, if any. Used by the `OAuthTokenManager` to persist
    /// rotated refresh tokens back to the DB.
    pub fn get_refresh_token(&self, provider_id: &str, label: &str) -> Option<String> {
        let key = Self::key(provider_id, label);
        self.tokens().get(&key).map(|c| c.refresh_token.clone())
    }

    /// Return the non-secret account email learned from the current token.
    pub fn get_account_email(&self, provider_id: &str, label: &str) -> Option<String> {
        let key = Self::key(provider_id, label);
        self.tokens()
            .get(&key)
            .and_then(|cached| cached.account_email.clone())
    }

    /// Return the latest account/workspace identity learned from a refresh.
    pub fn get_account_id(&self, provider_id: &str, label: &str) -> Option<String> {
        let key = Self::key(provider_id, label);
        self.tokens()
            .get(&key)
            .and_then(|cached| cached.account_id.clone())
    }

    /// Return the access token currently used for this credential.
    pub fn get_access_token(&self, provider_id: &str, label: &str) -> Option<String> {
        let key = Self::key(provider_id, label);
        self.tokens()
            .get(&key)
            .map(|cached| cached.access_token.clone())
            .filter(|token| !token.is_empty())
    }

    /// Clear a rejected access token only if it has not already been replaced
    /// by another concurrent request's refresh.
    pub fn invalidate_access_token_if_matches(
        &self,
        provider_id: &str,
        label: &str,
        rejected_access_token: &str,
    ) {
        let key = Self::key(provider_id, label);
        if let Some(mut cached) = self.tokens().get_mut(&key) {
            if cached.access_token == rejected_access_token {
                cached.access_token.clear();
                cached.expires_at = None;
            }
        }
    }
}

impl Default for OAuthTokenCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Inject the access token into the upstream headers using the
/// OAuthTargetConfig's header name and prefix. Also injects any
/// extra provider-specific headers (e.g. `anthropic-beta`).
fn inject_token(
    headers: &mut HeaderMap,
    access_token: &str,
    account_id: Option<&str>,
    oauth: &OAuthTargetConfig,
) -> Result<(), String> {
    let hv = format!("{}{}", oauth.bearer_prefix(), access_token);
    let hv = http::HeaderValue::from_str(&hv)
        .map_err(|e| format!("invalid header value for OAuth token: {e}"))?;
    let hn = http::HeaderName::from_bytes(oauth.header_name().as_bytes())
        .map_err(|e| format!("invalid header name for OAuth token: {e}"))?;
    headers.insert(hn, hv);

    // Inject extra provider-specific headers. The presence of the ChatGPT
    // account header is also the explicit signal that this target uses the
    // Codex workspace-scoping contract; a generic OAuth `account_id` must not
    // leak an OpenAI-specific header to Anthropic, xAI, or custom providers.
    let uses_chatgpt_account_header = oauth
        .extra_headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("chatgpt-account-id"));
    for (name, value) in &oauth.extra_headers {
        let hv = http::HeaderValue::from_str(value)
            .map_err(|e| format!("invalid header value for '{name}': {e}"))?;
        let hn = http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| format!("invalid header name '{name}': {e}"))?;
        headers.insert(hn, hv);
    }

    if uses_chatgpt_account_header {
        let Some(account_id) = account_id.filter(|account_id| !account_id.is_empty()) else {
            return Ok(());
        };
        let value = http::HeaderValue::from_str(account_id)
            .map_err(|e| format!("invalid ChatGPT account header value: {e}"))?;
        headers.insert(http::HeaderName::from_static("chatgpt-account-id"), value);
    }

    Ok(())
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn pkce_verifier_length() {
        let (verifier, challenge) = generate_pkce();
        // 96 bytes → 128 base64url characters (no padding).
        assert_eq!(verifier.len(), 128, "verifier must be 128 chars");
        // Challenge is base64url(SHA256(verifier)) = 43 chars.
        assert_eq!(challenge.len(), 43, "challenge must be 43 chars");
    }

    #[test]
    fn pkce_challenge_correctness() {
        let verifier = "test-verifier-string";
        let challenge = pkce_challenge(verifier);
        // Manually compute expected.
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let digest = hasher.finalize();
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
        assert_eq!(challenge, expected);
    }

    #[test]
    fn pkce_verifier_uniqueness() {
        let (v1, _) = generate_pkce();
        let (v2, _) = generate_pkce();
        assert_ne!(v1, v2, "PKCE verifiers must be unique");
    }

    #[test]
    fn codex_preset_values() {
        let p = codex_preset();
        assert_eq!(p.vendor, "openai");
        assert_eq!(p.auth_url, "https://auth.openai.com/oauth/authorize");
        assert_eq!(p.token_url, "https://auth.openai.com/oauth/token");
        assert_eq!(p.client_id, "app_EMoamEEZ73f0CkXaXp7hrann");
        assert_eq!(p.redirect_url, "http://localhost:1455/auth/callback");
        assert_eq!(
            p.scopes,
            vec!["openid", "email", "profile", "offline_access"]
        );
        assert_eq!(p.exchange_request_style, TokenRequestStyle::Form);
        assert_eq!(p.refresh_request_style, TokenRequestStyle::Form);
        assert!(!p.send_scopes_in_exchange_request);
        assert_eq!(p.refresh_scopes, vec!["openid", "profile", "email"]);
        assert!(p
            .extra_authorize_params
            .contains(&("prompt".into(), "login".into())));
    }

    #[test]
    fn claude_preset_values() {
        let p = claude_preset();
        assert_eq!(p.vendor, "anthropic");
        assert_eq!(p.auth_url, "https://claude.ai/oauth/authorize");
        assert_eq!(p.token_url, "https://api.anthropic.com/v1/oauth/token");
        assert_eq!(p.client_id, "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        assert_eq!(p.redirect_url, "http://localhost:54545/callback");
        assert_eq!(
            p.scopes,
            vec![
                "user:profile",
                "user:inference",
                "user:sessions:claude_code",
                "user:mcp_servers",
                "user:file_upload"
            ]
        );
        assert_eq!(p.exchange_request_style, TokenRequestStyle::Json);
        assert_eq!(p.refresh_request_style, TokenRequestStyle::Json);
        assert!(p
            .extra_authorize_params
            .contains(&("code".into(), "true".into())));
    }

    #[test]
    fn xai_preset_values() {
        let p = xai_preset();
        assert_eq!(p.vendor, "xai");
        assert_eq!(p.auth_url, "https://auth.x.ai/oauth2/authorize");
        assert_eq!(p.token_url, "https://auth.x.ai/oauth2/token");
        assert_eq!(p.client_id, "b1a00492-073a-47ea-816f-4c329264a828");
        assert_eq!(p.redirect_url, "http://127.0.0.1:56121/callback");
        assert_eq!(
            p.scopes,
            vec![
                "openid",
                "profile",
                "email",
                "offline_access",
                "grok-cli:access",
                "api:access"
            ]
        );
        assert_eq!(p.exchange_request_style, TokenRequestStyle::Form);
        assert_eq!(p.refresh_request_style, TokenRequestStyle::Form);
        assert!(p
            .extra_authorize_params
            .contains(&("plan".into(), "generic".into())));
    }

    #[test]
    fn preset_for_vendor_lookup() {
        assert!(preset_for_vendor("openai").is_some());
        assert!(preset_for_vendor("anthropic").is_some());
        assert!(preset_for_vendor("xai").is_some());
        assert!(preset_for_vendor("unknown").is_none());
    }

    #[test]
    fn build_authorize_url_includes_params() {
        let preset = codex_preset();
        let url = build_authorize_url(&preset, "mystate", "mychallenge");
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("state=mystate"));
        assert!(url.contains("code_challenge=mychallenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
        assert!(url.contains("prompt=login"));
        assert!(!url.contains("originator="));
        assert!(!url.contains("api.connectors"));
    }

    #[test]
    fn cache_seed_and_get_refresh_token() {
        let cache = OAuthTokenCache::new();
        cache.seed("prov1", "label1", "rt-initial");
        assert_eq!(
            cache.get_refresh_token("prov1", "label1"),
            Some("rt-initial".to_string())
        );
        // Seeding again with a different token should NOT overwrite
        // (the cache already has a non-empty refresh token).
        cache.seed("prov1", "label1", "rt-different");
        assert_eq!(
            cache.get_refresh_token("prov1", "label1"),
            Some("rt-initial".to_string())
        );
    }

    #[test]
    fn cache_seed_empty_then_update() {
        let cache = OAuthTokenCache::new();
        // Seed with empty refresh token, then seed with real one.
        cache.seed("prov2", "label2", "");
        cache.seed("prov2", "label2", "rt-real");
        assert_eq!(
            cache.get_refresh_token("prov2", "label2"),
            Some("rt-real".to_string())
        );
    }

    #[test]
    fn cache_apply_no_refresh_token_returns_error() {
        let cache = OAuthTokenCache::new();
        let oauth = OAuthTargetConfig {
            upstream_transport: tiygate_core::provider::oauth::UpstreamTransport::Http,
            token_url: "https://example.com/token".to_string(),
            client_id: "test".to_string(),
            client_secret: None,
            refresh_token: String::new(),
            scopes: vec![],
            token_request_style: TokenRequestStyle::Form,
            authorization_header: None,
            authorization_prefix: None,
            extra_headers: vec![],
            account_id: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = reqwest::Client::new();
        let mut headers = HeaderMap::new();
        let result = rt.block_on(cache.apply(&mut headers, "prov3", "label3", &oauth, &client));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no refresh_token"));
    }

    #[test]
    fn account_identity_only_sets_header_for_explicit_chatgpt_targets() {
        let mut oauth = OAuthTargetConfig {
            upstream_transport: tiygate_core::provider::oauth::UpstreamTransport::Http,
            token_url: "https://example.com/token".to_string(),
            client_id: "test".to_string(),
            client_secret: None,
            refresh_token: "refresh".to_string(),
            scopes: vec![],
            token_request_style: TokenRequestStyle::Form,
            authorization_header: None,
            authorization_prefix: None,
            extra_headers: vec![],
            account_id: Some("workspace-new".to_string()),
        };

        let mut generic_headers = HeaderMap::new();
        inject_token(
            &mut generic_headers,
            "access",
            Some("workspace-new"),
            &oauth,
        )
        .unwrap();
        assert!(!generic_headers.contains_key("chatgpt-account-id"));

        oauth.extra_headers.push((
            "chatgpt-account-id".to_string(),
            "workspace-old".to_string(),
        ));
        let mut codex_headers = HeaderMap::new();
        inject_token(&mut codex_headers, "access", Some("workspace-new"), &oauth).unwrap();
        assert_eq!(
            codex_headers["chatgpt-account-id"],
            http::HeaderValue::from_static("workspace-new")
        );
    }

    #[tokio::test]
    async fn cache_concurrent_apply_with_valid_token_all_succeed() {
        // Test that concurrent apply calls on the same key all
        // succeed when a valid (non-expiring) token is cached.
        let cache = Arc::new(OAuthTokenCache::new());

        // Manually insert a valid token into the cache.
        cache.tokens().insert(
            "prov4:label4".to_string(),
            CachedToken {
                access_token: "test-access-token".to_string(),
                refresh_token: "test-refresh-token".to_string(),
                expires_at: Some(Instant::now() + Duration::from_secs(3600)),
                account_id: None,
                account_email: None,
            },
        );

        let oauth = OAuthTargetConfig {
            upstream_transport: tiygate_core::provider::oauth::UpstreamTransport::Http,
            token_url: "https://example.com/token".to_string(),
            client_id: "test".to_string(),
            client_secret: None,
            refresh_token: "test-refresh-token".to_string(),
            scopes: vec![],
            token_request_style: TokenRequestStyle::Form,
            authorization_header: None,
            authorization_prefix: None,
            extra_headers: vec![],
            account_id: None,
        };
        let client = reqwest::Client::new();

        // Spawn 16 concurrent apply calls — all should hit the
        // cache fast path and succeed.
        let mut handles = Vec::new();
        for _ in 0..16 {
            let cache = cache.clone();
            let oauth = oauth.clone();
            let client = client.clone();
            handles.push(tokio::spawn(async move {
                let mut headers = HeaderMap::new();
                cache
                    .apply(&mut headers, "prov4", "label4", &oauth, &client)
                    .await
            }));
        }

        for handle in handles {
            let result = handle.await.unwrap();
            assert!(result.is_ok(), "concurrent apply should succeed");
        }
    }

    #[test]
    fn parses_chatgpt_account_id_from_namespaced_claim() {
        let claims = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "workspace-123"
            },
            "email": "codex@example.com"
        });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        let jwt = format!("header.{payload}.signature");
        assert_eq!(chatgpt_account_id(&jwt).as_deref(), Some("workspace-123"));
        assert_eq!(
            chatgpt_account_email(&jwt).as_deref(),
            Some("codex@example.com")
        );
    }

    #[test]
    fn seed_tokens_makes_access_token_immediately_available() {
        let cache = OAuthTokenCache::new();
        cache.seed_tokens(
            "provider",
            "workspace-123",
            "access",
            "refresh",
            Some(Duration::from_secs(3600)),
        );
        let cached = cache.tokens().get("provider:workspace-123").unwrap();
        assert_eq!(cached.access_token, "access");
        assert_eq!(cached.refresh_token, "refresh");
        assert!(!cached.is_expiring());
    }

    #[test]
    fn classifies_rejected_refresh_credentials_without_exposing_details() {
        assert_eq!(
            classify_refresh_failure(
                "token endpoint returned 401 Unauthorized: refresh_token_reused"
            ),
            OAuthRefreshFailureKind::CredentialInvalid
        );
        assert_eq!(
            classify_refresh_failure("token refresh request failed: connection reset"),
            OAuthRefreshFailureKind::Transient
        );
        assert_eq!(
            OAuthRefreshFailureKind::CredentialInvalid.status_reason(),
            "credential_rejected"
        );
    }

    #[tokio::test]
    async fn codex_refresh_uses_form_with_scopes_and_parses_account() {
        let server = MockServer::start().await;
        let claims = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "workspace-123"
            }
        });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        let id_token = format!("header.{payload}.signature");
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-new",
                "refresh_token": "refresh-new",
                "expires_in": 3600,
                "id_token": id_token,
            })))
            .mount(&server)
            .await;

        let result = do_refresh_token(
            &format!("{}/oauth/token", server.uri()),
            "client",
            "refresh-old",
            &["openid".into(), "profile".into(), "email".into()],
            &TokenRequestStyle::Form,
            &reqwest::Client::new(),
        )
        .await
        .unwrap();

        assert_eq!(result.access_token, "access-new");
        assert_eq!(result.refresh_token.as_deref(), Some("refresh-new"));
        assert_eq!(result.account_id.as_deref(), Some("workspace-123"));
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests[0]
                .headers
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            "application/x-www-form-urlencoded"
        );
        let body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(body.contains("grant_type=refresh_token"));
        assert!(body.contains("refresh_token=refresh-old"));
        assert!(body.contains("client_id=client"));
        assert!(body.contains("scope=openid+profile+email"));
    }

    #[tokio::test]
    async fn codex_exchange_uses_form_without_scope() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access",
                "refresh_token": "refresh",
                "expires_in": 3600,
                "account_id": "workspace-123",
            })))
            .mount(&server)
            .await;

        let mut preset = codex_preset();
        preset.token_url = format!("{}/oauth/token", server.uri());
        let result = exchange_code(&preset, "code", "verifier", &reqwest::Client::new())
            .await
            .unwrap();
        assert_eq!(result.account_id.as_deref(), Some("workspace-123"));

        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(body.contains("grant_type=authorization_code"));
        assert!(body.contains("code_verifier=verifier"));
        assert!(!body.contains("scope="));
        assert_eq!(
            requests[0]
                .headers
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("application/x-www-form-urlencoded")
        );
    }
}
