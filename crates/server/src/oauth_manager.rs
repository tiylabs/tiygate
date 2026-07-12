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
use tiygate_auth::provider_oauth::{
    classify_refresh_failure, OAuthRefreshFailureKind, OAuthTokenCache,
};
use tiygate_core::RoutingTarget;
use tiygate_store::config_store::DbConfigStore;
use tiygate_store::models::OAuthCredentialStatus;
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

        // A provider row owns one OAuth credential. Cache it by the upstream
        // account/workspace rather than by route/model so refresh-token
        // rotation is single-flight across every model using the credential.
        let label = oauth.cache_label();

        // Seed the cache with the refresh token from the routing
        // target. The `seed` method is idempotent — it only writes
        // when the cache entry is empty or has an empty refresh
        // token, so a newer cached token (from a rotation) is never
        // overwritten.
        self.cache
            .seed(&target.provider_id, label, &oauth.refresh_token);

        // Apply the token (refresh if needed, inject header).
        if let Err(error) = self
            .cache
            .apply(
                headers,
                &target.provider_id,
                label,
                oauth,
                &self.http_client,
            )
            .await
        {
            self.persist_refresh_failure(&target.provider_id, &error);
            return Err(error);
        }

        // Persist rotated tokens and refreshed workspace identity together.
        if let Some(new_rt) = self.cache.get_refresh_token(&target.provider_id, label) {
            let account_id = self
                .cache
                .get_account_id(&target.provider_id, label)
                .or_else(|| oauth.account_id.clone());
            if new_rt != oauth.refresh_token || account_id != oauth.account_id {
                self.persist_credentials(&target.provider_id, &new_rt, account_id.as_deref());
            }
        }

        Ok(true)
    }

    /// Return the access token currently cached for this OAuth target.
    #[cfg(test)]
    pub fn cached_access_token(&self, target: &RoutingTarget) -> Option<String> {
        let oauth = target.oauth.as_ref()?;
        self.cache
            .get_access_token(&target.provider_id, oauth.cache_label())
    }

    /// Recover once from an upstream 401. The conditional invalidation avoids
    /// consuming a rotating refresh token twice when concurrent requests are
    /// rejected with the same stale access token.
    pub async fn refresh_after_unauthorized(
        &self,
        target: &RoutingTarget,
        rejected_access_token: &str,
    ) -> Result<bool, String> {
        let oauth = match &target.oauth {
            Some(oauth) => oauth,
            None => return Ok(false),
        };
        self.cache.invalidate_access_token_if_matches(
            &target.provider_id,
            oauth.cache_label(),
            rejected_access_token,
        );
        let mut headers = HeaderMap::new();
        self.apply(target, &mut headers).await
    }

    /// Asynchronously persist a new refresh token to the DB.
    ///
    /// This is fire-and-forget: if the DB write fails, the token
    /// remains in the in-memory cache and the request succeeds, but
    /// a process restart will lose the rotated token (the operator
    /// must re-run the OAuth flow). The warning log surfaces the
    /// failure for visibility.
    fn persist_credentials(
        &self,
        provider_id: &str,
        refresh_token: &str,
        account_id: Option<&str>,
    ) {
        let store = match &self.store {
            Some(s) => s.clone(),
            None => return,
        };
        let pid = provider_id.to_string();
        let rt = refresh_token.to_string();
        let account_id = account_id.map(str::to_string);
        tokio::spawn(async move {
            let provider = match store.get_provider(&pid).await {
                Ok(Some(provider)) => provider,
                Ok(None) => return,
                Err(e) => {
                    warn!(provider = %pid, error = %e, "failed to load OAuth metadata");
                    return;
                }
            };
            let mut meta = provider
                .oauth_meta_cleartext
                .as_deref()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                .unwrap_or_else(|| json!({}));
            let Some(object) = meta.as_object_mut() else {
                warn!(provider = %pid, "stored OAuth metadata is not an object");
                return;
            };
            object.insert("refresh_token".to_string(), json!(rt));
            object.insert("account_id".to_string(), json!(account_id));
            object.insert(
                "status".to_string(),
                json!(OAuthCredentialStatus::Healthy.as_str()),
            );
            object.insert(
                "status_checked_at".to_string(),
                json!(chrono::Utc::now().to_rfc3339()),
            );
            object.remove("status_reason");
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

    /// Persist a sanitized failure classification off the request hot path so
    /// the Admin Console can distinguish invalid credentials from transient
    /// refresh errors.
    fn persist_refresh_failure(&self, provider_id: &str, error: &str) {
        let store = match &self.store {
            Some(store) => store.clone(),
            None => return,
        };
        let provider_id = provider_id.to_string();
        let kind = classify_refresh_failure(error);
        let status = match kind {
            OAuthRefreshFailureKind::CredentialInvalid => OAuthCredentialStatus::Invalid,
            OAuthRefreshFailureKind::Transient => OAuthCredentialStatus::Error,
        };
        tokio::spawn(async move {
            if let Err(status_error) = store
                .set_provider_oauth_status(&provider_id, status, Some(kind.status_reason()))
                .await
            {
                warn!(
                    provider = %provider_id,
                    error = %status_error,
                    "failed to persist OAuth refresh failure status"
                );
            }
        });
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use tiygate_core::provider::oauth::{OAuthTargetConfig, TokenRequestStyle, UpstreamTransport};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_oauth_config(refresh_token: &str) -> OAuthTargetConfig {
        OAuthTargetConfig {
            upstream_transport: UpstreamTransport::Http,
            token_url: "https://example.com/token".to_string(),
            client_id: "test-client".to_string(),
            client_secret: None,
            refresh_token: refresh_token.to_string(),
            scopes: vec!["openid".to_string()],
            token_request_style: TokenRequestStyle::Form,
            authorization_header: None,
            authorization_prefix: None,
            extra_headers: vec![],
            account_id: None,
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

    #[tokio::test]
    async fn different_models_share_one_account_token_cache() {
        let manager = OAuthTokenManager::new(None, reqwest::Client::new());
        let mut oauth = make_oauth_config("refresh");
        oauth.account_id = Some("workspace-shared".to_string());
        let mut first = make_target(Some(oauth.clone()));
        first.provider_id = "provider-shared-model-test".to_string();
        first.model_id = "model-a".to_string();
        let mut second = first.clone();
        second.model_id = "model-b".to_string();

        OAuthTokenCache::global().seed_tokens(
            &first.provider_id,
            oauth.cache_label(),
            "access",
            "refresh",
            Some(std::time::Duration::from_secs(3600)),
        );

        let mut first_headers = HeaderMap::new();
        let mut second_headers = HeaderMap::new();
        assert!(manager.apply(&first, &mut first_headers).await.unwrap());
        assert!(manager.apply(&second, &mut second_headers).await.unwrap());
        assert_eq!(
            first_headers.get("authorization"),
            second_headers.get("authorization")
        );
    }

    #[tokio::test]
    async fn unauthorized_refresh_replaces_only_rejected_token_and_workspace() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "fresh-access",
                "refresh_token": "fresh-refresh",
                "expires_in": 3600,
                "account_id": "workspace-new"
            })))
            .mount(&server)
            .await;

        let manager = OAuthTokenManager::new(None, reqwest::Client::new());
        let mut oauth = make_oauth_config("old-refresh");
        oauth.token_url = format!("{}/token", server.uri());
        oauth.account_id = Some("workspace-old".to_string());
        oauth.extra_headers.push((
            "chatgpt-account-id".to_string(),
            "workspace-old".to_string(),
        ));
        let mut target = make_target(Some(oauth.clone()));
        target.provider_id = "provider-401-refresh-test".to_string();
        OAuthTokenCache::global().seed_tokens_with_identity(
            &target.provider_id,
            oauth.cache_label(),
            "stale-access",
            "old-refresh",
            Some(std::time::Duration::from_secs(3600)),
            tiygate_auth::provider_oauth::OAuthTokenIdentity {
                account_id: Some("workspace-old".to_string()),
                account_email: None,
            },
        );

        assert!(manager
            .refresh_after_unauthorized(&target, "stale-access")
            .await
            .unwrap());
        assert_eq!(
            manager.cached_access_token(&target).as_deref(),
            Some("fresh-access")
        );

        let mut headers = HeaderMap::new();
        assert!(manager.apply(&target, &mut headers).await.unwrap());
        assert_eq!(headers["authorization"], "Bearer fresh-access");
        assert_eq!(headers["chatgpt-account-id"], "workspace-new");

        // A late 401 for the old token must not invalidate the fresh token.
        assert!(manager
            .refresh_after_unauthorized(&target, "stale-access")
            .await
            .unwrap());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}
