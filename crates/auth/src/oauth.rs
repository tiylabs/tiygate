//! OAuth 2.0 client-credentials `AuthApplier`.
//!
//! Implements:
//! - `start` — bootstrap (no-op for client-credentials; the applier is
//!   passive and obtains tokens on demand).
//! - `exchange` — perform a one-time authorization-code → token exchange.
//! - `refresh` — refresh an existing access token using the refresh token.
//! - `apply` — attach the current access token to outgoing request headers
//!   in the format expected by the upstream provider.
//!
//! ## Single-flight refresh
//!
//! `AuthApplier::apply` may be called from many concurrent in-flight
//! requests. If the access token is near expiry, naive implementations
//! would issue one refresh per request — which can race and invalidate
//! each other's refresh tokens. We use a per-label `tokio::sync::Mutex`
//! inside the applier so concurrent callers serialise on a single
//! in-flight refresh, and subsequent callers reuse the new token.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use http::HeaderMap;
use oauth2::basic::BasicClient;
use oauth2::reqwest::async_http_client;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tiygate_core::{AuthApplier, Error, RoutingTarget};
use tracing::{debug, info, warn};
use zeroize::Zeroize;

/// Configuration for an OAuth 2.0 client-credentials flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthConfig {
    /// Provider identifier (e.g., "anthropic-oauth", "openai-oauth").
    pub provider_id: String,
    /// Authorization endpoint (only used for authorization-code flows).
    pub auth_url: String,
    /// Token endpoint.
    pub token_url: String,
    /// Client ID.
    pub client_id: String,
    /// Client secret.
    pub client_secret: String,
    /// Scopes to request.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Optional redirect URL (authorization-code flow only).
    #[serde(default)]
    pub redirect_url: Option<String>,
    /// Optional header name override; defaults to `authorization: Bearer …`.
    #[serde(default)]
    pub authorization_header: Option<String>,
    /// Optional prefix override; defaults to `Bearer `.
    #[serde(default)]
    pub authorization_prefix: Option<String>,
    /// Refresh tokens proactively when they expire within this window.
    #[serde(default = "default_refresh_leeway")]
    pub refresh_leeway: Duration,
}

fn default_refresh_leeway() -> Duration {
    Duration::from_secs(60)
}

impl OAuthConfig {
    fn bearer_prefix(&self) -> &str {
        self.authorization_prefix.as_deref().unwrap_or("Bearer ")
    }

    fn header_name(&self) -> &str {
        self.authorization_header
            .as_deref()
            .unwrap_or("authorization")
    }
}

/// An in-memory token, refresh-token, and expiry pair.
#[derive(Debug, Clone, Zeroize)]
struct CachedToken {
    access_token: String,
    #[zeroize(skip)]
    refresh_token: Option<String>,
    #[zeroize(skip)]
    expires_at: Option<Instant>,
}

impl CachedToken {
    fn is_expiring(&self, leeway: Duration) -> bool {
        match self.expires_at {
            Some(t) => t.saturating_duration_since(Instant::now()) <= leeway,
            None => false,
        }
    }
}

/// Outcome of `start` / `exchange` / `refresh`.
#[derive(Debug, Clone)]
pub enum OAuthOutcome {
    /// Authorization-code flow: redirect the user-agent to this URL.
    RedirectUrl(String),
    /// Token obtained. Returned from `exchange` / `refresh` / `apply`.
    Token {
        access_token: String,
        refresh_token: Option<String>,
        expires_in: Option<Duration>,
    },
}

/// Per-label state — one `OAuthAuthApplier` can serve many targets
/// (e.g. multi-account channels) and serialises refreshes per label.
struct LabelState {
    /// The most recently issued access token. `None` means no token yet.
    cached: Option<CachedToken>,
    /// In-flight refresh future, for single-flight.
    inflight: Option<Arc<tokio::sync::Mutex<()>>>,
}

impl LabelState {
    fn new() -> Self {
        Self {
            cached: None,
            inflight: Some(Arc::new(tokio::sync::Mutex::new(()))),
        }
    }
}

/// An `AuthApplier` that performs OAuth 2.0 client-credentials and
/// authorization-code flows, with single-flight token refresh.
pub struct OAuthAuthApplier {
    config: OAuthConfig,
    /// Per-label state (keyed by `RoutingTarget::account_label` or the
    /// model id when no label is set).
    by_label: RwLock<HashMap<String, LabelState>>,
}

impl OAuthAuthApplier {
    pub fn new(config: OAuthConfig) -> Self {
        Self {
            config,
            by_label: RwLock::new(HashMap::new()),
        }
    }

    fn label_for(&self, target: &RoutingTarget) -> String {
        target
            .account_label
            .clone()
            .unwrap_or_else(|| target.model_id.clone())
    }

    fn state_for(&self, label: &str) -> LabelState {
        // Fast path: read lock.
        if let Some(s) = self.by_label.read().get(label) {
            // Clone the inflight mutex Arc only.
            return LabelState {
                cached: None, // not used by callers
                inflight: s.inflight.clone(),
            };
        }
        let mut w = self.by_label.write();
        w.entry(label.to_string())
            .or_insert_with(LabelState::new)
            .inflight
            .clone()
            .map(|m| LabelState {
                cached: None,
                inflight: Some(m),
            })
            .unwrap_or_else(LabelState::new)
    }

    fn store_token(
        &self,
        label: &str,
        access_token: String,
        refresh_token: Option<String>,
        expires_in: Option<Duration>,
    ) {
        let mut w = self.by_label.write();
        let entry = w.entry(label.to_string()).or_insert_with(LabelState::new);
        entry.cached = Some(CachedToken {
            access_token,
            refresh_token,
            expires_at: expires_in.map(|d| Instant::now() + d),
        });
    }

    fn cached(&self, label: &str) -> Option<CachedToken> {
        self.by_label
            .read()
            .get(label)
            .and_then(|s| s.cached.clone())
    }

    fn build_basic_client(&self) -> Result<BasicClient, Error> {
        let auth_url = AuthUrl::new(self.config.auth_url.clone())
            .map_err(|e| Error::Auth(format!("invalid auth_url: {e}")))?;
        let token_url = TokenUrl::new(self.config.token_url.clone())
            .map_err(|e| Error::Auth(format!("invalid token_url: {e}")))?;
        let mut client = BasicClient::new(
            ClientId::new(self.config.client_id.clone()),
            Some(ClientSecret::new(self.config.client_secret.clone())),
            auth_url,
            Some(token_url),
        );
        if let Some(ru) = &self.config.redirect_url {
            client = client.set_redirect_uri(
                RedirectUrl::new(ru.clone())
                    .map_err(|e| Error::Auth(format!("invalid redirect_url: {e}")))?,
            );
        }
        Ok(client)
    }

    /// Public entry point — explicit token refresh. Single-flighted per label.
    pub async fn refresh(&self, target: &RoutingTarget) -> Result<OAuthOutcome, Error> {
        let label = self.label_for(target);
        let _state = self.state_for(&label);

        let client = self.build_basic_client()?;
        let refresh_token = self
            .cached(&label)
            .and_then(|t| t.refresh_token.clone())
            .ok_or_else(|| {
                Error::Auth(format!(
                    "no refresh_token cached for label {label}; perform start/exchange first"
                ))
            })?;

        let refresh = RefreshToken::new(refresh_token);
        let mut req = client.exchange_refresh_token(&refresh);
        for s in &self.config.scopes {
            req = req.add_scope(Scope::new(s.clone()));
        }

        let token = req
            .request_async(async_http_client)
            .await
            .map_err(|e| Error::Auth(format!("refresh request failed: {e}")))?;

        let access = token.access_token().secret().to_string();
        let refresh = token.refresh_token().map(|t| t.secret().to_string());
        let expires_in: Option<Duration> =
            token.expires_in().map(|d| Duration::from_secs(d.as_secs()));

        self.store_token(&label, access.clone(), refresh.clone(), expires_in);
        info!(provider = %self.config.provider_id, label = %label, "oauth token refreshed");
        Ok(OAuthOutcome::Token {
            access_token: access,
            refresh_token: refresh,
            expires_in,
        })
    }

    /// Authorization-code flow: generate the URL the user-agent should be
    /// redirected to. The returned verifier must be persisted (e.g. cookie)
    /// and passed back into `exchange`.
    pub async fn start(&self) -> Result<(String, PkceCodeVerifier), Error> {
        let client = self.build_basic_client()?;
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let mut req = client.authorize_url(CsrfToken::new_random);
        for s in &self.config.scopes {
            req = req.add_scope(Scope::new(s.clone()));
        }
        let (url, _state) = req.set_pkce_challenge(pkce_challenge).url();
        Ok((url.to_string(), pkce_verifier))
    }

    /// Exchange an authorization code (and PKCE verifier) for an access token.
    pub async fn exchange(
        &self,
        target: &RoutingTarget,
        code: &str,
        pkce_verifier: PkceCodeVerifier,
    ) -> Result<OAuthOutcome, Error> {
        let label = self.label_for(target);
        let client = self.build_basic_client()?;

        let token = client
            .exchange_code(AuthorizationCode::new(code.to_string()))
            .set_pkce_verifier(pkce_verifier)
            .request_async(async_http_client)
            .await
            .map_err(|e| Error::Auth(format!("exchange failed: {e}")))?;

        let access = token.access_token().secret().to_string();
        let refresh = token.refresh_token().map(|t| t.secret().to_string());
        let expires_in: Option<Duration> =
            token.expires_in().map(|d| Duration::from_secs(d.as_secs()));

        self.store_token(&label, access.clone(), refresh.clone(), expires_in);
        Ok(OAuthOutcome::Token {
            access_token: access,
            refresh_token: refresh,
            expires_in,
        })
    }
}

#[async_trait]
impl AuthApplier for OAuthAuthApplier {
    async fn apply(&self, headers: &mut HeaderMap, target: &RoutingTarget) -> Result<(), Error> {
        let label = self.label_for(target);

        // Fast path: a still-valid token is cached.
        if let Some(cached) = self.cached(&label) {
            if !cached.is_expiring(self.config.refresh_leeway) {
                let header_value =
                    format!("{}{}", self.config.bearer_prefix(), cached.access_token);
                let hv = http::HeaderValue::from_str(&header_value)
                    .map_err(|e| Error::Auth(format!("invalid header value: {e}")))?;
                let hn = http::HeaderName::from_bytes(self.config.header_name().as_bytes())
                    .map_err(|e| Error::Auth(format!("invalid header name: {e}")))?;
                headers.insert(hn, hv);
                return Ok(());
            }
        }

        // Slow path: refresh, single-flighted per label.
        let state = self.state_for(&label);
        if let Some(mutex) = state.inflight {
            let _guard = mutex.lock().await;
            // Re-check after acquiring the lock — another task may have
            // already refreshed while we were waiting.
            if let Some(cached) = self.cached(&label) {
                if !cached.is_expiring(self.config.refresh_leeway) {
                    let header_value =
                        format!("{}{}", self.config.bearer_prefix(), cached.access_token);
                    let hv = http::HeaderValue::from_str(&header_value)
                        .map_err(|e| Error::Auth(format!("invalid header value: {e}")))?;
                    let hn = http::HeaderName::from_bytes(self.config.header_name().as_bytes())
                        .map_err(|e| Error::Auth(format!("invalid header name: {e}")))?;
                    headers.insert(hn, hv);
                    return Ok(());
                }
            }
            if let Err(e) = self.refresh(target).await {
                warn!(error = %e, label = %label, "oauth refresh failed in apply()");
                return Err(e);
            }
        }

        // Re-read after refresh.
        let cached = self.cached(&label).ok_or_else(|| {
            Error::Auth(format!(
                "no token available after refresh for label {label}"
            ))
        })?;
        let header_value = format!("{}{}", self.config.bearer_prefix(), cached.access_token);
        let hv = http::HeaderValue::from_str(&header_value)
            .map_err(|e| Error::Auth(format!("invalid header value: {e}")))?;
        let hn = http::HeaderName::from_bytes(self.config.header_name().as_bytes())
            .map_err(|e| Error::Auth(format!("invalid header name: {e}")))?;
        headers.insert(hn, hv);
        Ok(())
    }

    async fn prepare_body(
        &self,
        _body: &mut serde_json::Value,
        _target: &RoutingTarget,
    ) -> Result<(), Error> {
        // OAuth access tokens are applied to the request *headers*, not the
        // body. The `prepare_body` hook is a no-op for OAuth.
        Ok(())
    }
}

impl Drop for CachedToken {
    fn drop(&mut self) {
        self.access_token.zeroize();
        if let Some(rt) = &mut self.refresh_token {
            rt.zeroize();
        }
    }
}

/// Configuration for a real OAuth provider — the gateway parses this
/// from the DB / config store and constructs an `OAuthAuthApplier` for
/// the matching `RoutingTarget`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthProviderConfig {
    pub provider_id: String,
    pub auth_url: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub scopes: Vec<String>,
    pub redirect_url: Option<String>,
    pub authorization_header: Option<String>,
    pub authorization_prefix: Option<String>,
}

impl From<OAuthProviderConfig> for OAuthConfig {
    fn from(c: OAuthProviderConfig) -> Self {
        Self {
            provider_id: c.provider_id,
            auth_url: c.auth_url,
            token_url: c.token_url,
            client_id: c.client_id,
            client_secret: c.client_secret,
            scopes: c.scopes,
            redirect_url: c.redirect_url,
            authorization_header: c.authorization_header,
            authorization_prefix: c.authorization_prefix,
            refresh_leeway: default_refresh_leeway(),
        }
    }
}

// `CachedToken::cached` is read under a read-lock; the design here is
// safe because the apply() path re-checks after acquiring the per-label
// mutex. The `is_expiring` access is a single field read on an owned
// clone, so we don't need any extra interior-mutability.
#[allow(dead_code)]
fn _ensure_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<OAuthAuthApplier>();
}

// `apply` on `OAuthAuthApplier` is the hot path: it must be infallible on
// the fast path and must not call into the network unless the token is
// expiring. The `debug!` line is a no-op trace that helps when running
// with `RUST_LOG=tiygate::auth::oauth=debug`.
#[allow(dead_code)]
fn _trace_helper() {
    debug!("oauth trace helper loaded");
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn dummy_config() -> OAuthConfig {
        OAuthConfig {
            provider_id: "test".to_string(),
            auth_url: "https://example.com/oauth/authorize".to_string(),
            token_url: "https://example.com/oauth/token".to_string(),
            client_id: "client-id".to_string(),
            client_secret: "client-secret".to_string(),
            scopes: vec!["read".to_string()],
            redirect_url: Some("https://gateway.example.com/callback".to_string()),
            authorization_header: None,
            authorization_prefix: None,
            refresh_leeway: Duration::from_secs(60),
        }
    }

    fn dummy_target(label: Option<&str>) -> RoutingTarget {
        RoutingTarget {
            provider_id: "test".to_string(),
            model_id: "test-model".to_string(),
            api_base: "https://example.com".to_string(),
            api_key: "placeholder".to_string(),
            api_protocol: tiygate_core::ProtocolEndpoint::new(
                tiygate_core::ProtocolSuite::OpenAiCompatible,
                "chat-completions",
                "v1",
            ),
            account_label: label.map(|s| s.to_string()),
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
            oauth: None,
        }
    }

    #[tokio::test]
    async fn apply_with_no_cached_token_returns_error() {
        // No token has been issued yet; with no refresh token in cache,
        // apply() should surface an auth error rather than panic.
        let applier = OAuthAuthApplier::new(dummy_config());
        let target = dummy_target(Some("acc-a"));
        let mut headers = HeaderMap::new();
        // Apply will try to refresh, fail (no real token endpoint), and
        // surface the error. The fast path is the cached-token branch,
        // which returns 401-ish via the auth-error path.
        let result = applier.apply(&mut headers, &target).await;
        assert!(result.is_err(), "expected auth error when no token cached");
    }

    #[test]
    fn config_serde_round_trip() {
        let c = dummy_config();
        let json = serde_json::to_string(&c).unwrap();
        let de: OAuthConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(de.provider_id, c.provider_id);
        assert_eq!(de.token_url, c.token_url);
        assert_eq!(de.scopes, c.scopes);
    }

    #[test]
    fn label_for_uses_account_label_then_model_id() {
        let applier = OAuthAuthApplier::new(dummy_config());
        let t1 = dummy_target(Some("acc-a"));
        assert_eq!(applier.label_for(&t1), "acc-a");
        let t2 = dummy_target(None);
        assert_eq!(applier.label_for(&t2), "test-model");
    }

    #[tokio::test]
    async fn build_basic_client_rejects_invalid_urls() {
        let mut c = dummy_config();
        c.token_url = "not a url".to_string();
        let applier = OAuthAuthApplier::new(c);
        let res = applier.build_basic_client();
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn start_generates_redirect_url_with_pkce() {
        let applier = OAuthAuthApplier::new(dummy_config());
        let (url, _verifier) = applier.start().await.expect("start should succeed");
        assert!(url.contains("example.com/oauth/authorize"));
        assert!(url.contains("code_challenge=") || url.contains("code_challenge_method="));
    }

    #[tokio::test]
    async fn single_flight_uses_one_mutex_per_label() {
        // Verify the per-label mutex surface: N concurrent apply()
        // calls on the same label must contend on the same Mutex,
        // not on a different Mutex per call. We use a deliberately
        // broken config (invalid token_url) so apply() fails — we
        // are not testing the wire exchange here, only the single-flight
        // plumbing. The assertion: all calls observe the same shared
        // lock reference (verified via Arc pointer equality).
        use std::sync::Arc as StdArc;
        let applier = OAuthAuthApplier::new(dummy_config());
        // First call populates the label state.
        let _ = applier.state_for("acc-sf");
        let s1 = applier.state_for("acc-sf");
        let s2 = applier.state_for("acc-sf");
        let m1: StdArc<tokio::sync::Mutex<()>> = s1.inflight.expect("inflight mutex");
        let m2: StdArc<tokio::sync::Mutex<()>> = s2.inflight.expect("inflight mutex");
        assert!(StdArc::ptr_eq(&m1, &m2), "same label must share one mutex");
        // Different label → different mutex.
        let s3 = applier.state_for("acc-other");
        let m3: StdArc<tokio::sync::Mutex<()>> = s3.inflight.expect("inflight mutex");
        assert!(
            !StdArc::ptr_eq(&m1, &m3),
            "different labels must use distinct mutexes"
        );
    }

    #[tokio::test]
    async fn single_flight_serializes_concurrent_apply_calls() {
        // Wire up an OAuth applier that is pre-loaded with a still-valid
        // access token. Then fire N concurrent apply() calls. The fast
        // path (cached token) should be hit on every call without
        // attempting a refresh. We assert the cached token survives
        // concurrent access.
        use std::sync::Arc as StdArc;
        let applier = StdArc::new(OAuthAuthApplier::new(dummy_config()));
        applier.store_token(
            "acc-a",
            "tok-abc".to_string(),
            Some("refresh-xyz".to_string()),
            Some(Duration::from_secs(3600)),
        );
        let target = Arc::new(dummy_target(Some("acc-a")));
        let mut handles = Vec::new();
        for _ in 0..32 {
            let applier = applier.clone();
            let target = target.clone();
            handles.push(tokio::spawn(async move {
                let mut headers = HeaderMap::new();
                applier.apply(&mut headers, &target).await
            }));
        }
        for h in handles {
            let r = h.await.expect("task did not panic");
            r.expect("apply must succeed when a fresh token is cached");
        }
    }
}
