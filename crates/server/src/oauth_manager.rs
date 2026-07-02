//! OAuth token manager — bridges the global `OAuthTokenCache` with
//! the DB-backed `DbConfigStore` for refresh-token persistence.
//!
//! The `OAuthTokenCache` (in `tiygate-auth`) is a process-global,
//! in-memory cache that handles token refresh with single-flight
//! semantics. The `OAuthTokenManager` wraps it and adds:
//!
//! 1. **Seeding** — when a `RoutingTarget` arrives with an OAuth
//!    config containing a refresh token, seed the cache so the first
//!    request can use it.
//! 2. **Persistence** — after a successful refresh (which may rotate
//!    the refresh token), asynchronously persist the new refresh
//!    token back to the DB via `set_provider_oauth_meta`.
//!
//! This struct lives in the `server` crate (not `auth`) because it
//! needs `DbConfigStore` — a `store` dependency that the `auth`
//! crate must not have (layering constraint).

use std::sync::Arc;

use http::HeaderMap;
use serde_json::json;
use tiygate_auth::provider_oauth::OAuthTokenCache;
use tiygate_core::RoutingTarget;
use tiygate_store::config_store::DbConfigStore;
use tracing::warn;

/// Manages OAuth token lifecycle for the data plane.
///
/// Constructed once at startup and stored in `AppState` as
/// `Arc<OAuthTokenManager>`. Cloned cheaply (inner fields are
/// `Arc`-shared).
#[derive(Clone)]
pub struct OAuthTokenManager {
    cache: &'static OAuthTokenCache,
    store: Option<Arc<DbConfigStore>>,
    http_client: reqwest::Client,
}

impl OAuthTokenManager {
    /// Create a new manager.
    ///
    /// - `store`: the DB-backed config store, used to persist rotated
    ///   refresh tokens. `None` in legacy/test mode (no persistence).
    /// - `http_client`: shared reqwest client for token refresh calls.
    pub fn new(store: Option<Arc<DbConfigStore>>, http_client: reqwest::Client) -> Self {
        Self {
            cache: OAuthTokenCache::global(),
            store,
            http_client,
        }
    }

    /// Apply OAuth authentication to the upstream headers.
    ///
    /// Returns `Ok(true)` if OAuth auth was applied (target has an
    /// OAuth config), or `Ok(false)` if the target is not OAuth-mode
    /// and the caller should fall back to the static key path.
    pub async fn apply(
        &self,
        target: &RoutingTarget,
        headers: &mut HeaderMap,
    ) -> Result<bool, String> {
        let oauth = match &target.oauth {
            Some(o) => o,
            None => return Ok(false),
        };

        let label = target.account_label.as_deref().unwrap_or(&target.model_id);

        // Seed the cache with the refresh token from the routing
        // target. The `seed` method is idempotent — it only writes
        // when the cache entry is empty or has an empty refresh
        // token, so a newer cached token (from a rotation) is never
        // overwritten.
        self.cache
            .seed(&target.provider_id, label, &oauth.refresh_token);

        // Apply the token (refresh if needed, inject header).
        self.cache
            .apply(
                headers,
                &target.provider_id,
                label,
                oauth,
                &self.http_client,
            )
            .await?;

        // Best-effort persistence of the (possibly rotated) refresh
        // token. The cache may have updated its refresh_token during
        // the `apply` call; if it differs from what the routing
        // target carried, persist the new one to the DB.
        if let Some(new_rt) = self.cache.get_refresh_token(&target.provider_id, label) {
            if new_rt != oauth.refresh_token {
                self.persist_refresh_token(&target.provider_id, &new_rt);
            }
        }

        Ok(true)
    }

    /// Asynchronously persist a new refresh token to the DB.
    ///
    /// This is fire-and-forget: if the DB write fails, the token
    /// remains in the in-memory cache and the request succeeds, but
    /// a process restart will lose the rotated token (the operator
    /// must re-run the OAuth flow). The warning log surfaces the
    /// failure for visibility.
    fn persist_refresh_token(&self, provider_id: &str, refresh_token: &str) {
        let store = match &self.store {
            Some(s) => s.clone(),
            None => return,
        };
        let pid = provider_id.to_string();
        let rt = refresh_token.to_string();
        tokio::spawn(async move {
            let meta = json!({ "refresh_token": rt });
            let meta_str = match serde_json::to_string(&meta) {
                Ok(s) => s,
                Err(e) => {
                    warn!(
                        provider = %pid,
                        error = %e,
                        "failed to serialize OAuth meta for persistence"
                    );
                    return;
                }
            };
            if let Err(e) = store.set_provider_oauth_meta(&pid, &meta_str).await {
                warn!(
                    provider = %pid,
                    error = %e,
                    "failed to persist rotated OAuth refresh token; \
                     a restart may require re-authorization"
                );
            }
        });
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use tiygate_core::provider::oauth::{OAuthTargetConfig, TokenRequestStyle};

    fn make_oauth_config(refresh_token: &str) -> OAuthTargetConfig {
        OAuthTargetConfig {
            token_url: "https://example.com/token".to_string(),
            client_id: "test-client".to_string(),
            client_secret: None,
            refresh_token: refresh_token.to_string(),
            scopes: vec!["openid".to_string()],
            token_request_style: TokenRequestStyle::Form,
            authorization_header: None,
            authorization_prefix: None,
            extra_headers: vec![],
        }
    }

    fn make_target(oauth: Option<OAuthTargetConfig>) -> RoutingTarget {
        RoutingTarget {
            provider_id: "test-prov".to_string(),
            model_id: "test-model".to_string(),
            api_base: String::new(),
            api_key: String::new(),
            api_protocol: tiygate_core::ProtocolEndpoint::new(
                tiygate_core::ProtocolSuite::OpenAiCompatible,
                "chat-completions",
                "v1",
            ),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
            oauth,
        }
    }

    #[tokio::test]
    async fn apply_returns_false_for_non_oauth_target() {
        let manager = OAuthTokenManager::new(None, reqwest::Client::new());
        let target = make_target(None);
        let mut headers = HeaderMap::new();
        let applied = manager.apply(&target, &mut headers).await.unwrap();
        assert!(!applied);
    }

    #[tokio::test]
    async fn apply_returns_error_for_empty_refresh_token() {
        let manager = OAuthTokenManager::new(None, reqwest::Client::new());
        let target = make_target(Some(make_oauth_config("")));
        let mut headers = HeaderMap::new();
        let result = manager.apply(&target, &mut headers).await;
        assert!(result.is_err());
    }
}
