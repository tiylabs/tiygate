//! OAuth token lifecycle manager.
//!
//! Valid access tokens stay on the zero-I/O in-memory fast path. On a cache
//! miss, DB-backed deployments reuse shared encrypted access-token state and
//! coordinate the actual refresh grant per provider. PostgreSQL uses a
//! transaction advisory lock across instances; SQLite uses a process-local
//! mutex because it is a single-instance deployment backend.

use std::cell::RefCell;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use dashmap::DashMap;
use http::HeaderMap;
use serde_json::json;
use tiygate_auth::provider_oauth::{
    classify_refresh_failure, do_refresh_token, OAuthCredentialRefreshSummary,
    OAuthCredentialService, OAuthProviderMutation, OAuthRefreshFailureKind, OAuthTokenCache,
    OAuthTokenIdentity, TokenResult,
};
use tiygate_core::provider::oauth::OAuthTargetConfig;
use tiygate_core::RoutingTarget;
use tiygate_store::config_store::{build_oauth_target_config, DbConfigStore};
use tiygate_store::db::DbKind;
use tiygate_store::models::OAuthCredentialStatus;
use tiygate_store::oauth_token::{OAuthAccessTokenState, OAuthTokenCommit, OAuthTokenStore};
use tracing::{info, warn};

const ACCESS_TOKEN_LEEWAY: Duration = Duration::from_secs(60);
const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(5);
const REFRESH_LOCK_POLL: Duration = Duration::from_millis(25);
const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

tokio::task_local! {
    static APPLIED_OAUTH_ACCESS_TOKEN: RefCell<Option<String>>;
}

/// Run one upstream attempt while capturing the exact OAuth access token
/// injected by that attempt. The value stays task-local and is never logged.
pub(crate) async fn capture_applied_access_token<F, T>(future: F) -> (T, Option<String>)
where
    F: Future<Output = T>,
{
    APPLIED_OAUTH_ACCESS_TOKEN
        .scope(RefCell::new(None), async move {
            let output = future.await;
            let token = APPLIED_OAUTH_ACCESS_TOKEN.with(|slot| slot.borrow().clone());
            (output, token)
        })
        .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthRefreshMode {
    IfNeeded,
    Force,
    Keepalive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthRefreshOutcome {
    Refreshed,
    ReusedShared,
    Skipped,
}

/// Manages OAuth token lifecycle for the data plane.
///
/// Constructed once at startup and stored in `AppState` as
/// `Arc<OAuthTokenManager>`. Cloned cheaply (inner fields are
/// `Arc`-shared).
#[derive(Clone)]
pub struct OAuthTokenManager {
    cache: &'static OAuthTokenCache,
    store: Option<Arc<DbConfigStore>>,
    token_store: Option<OAuthTokenStore>,
    local_refresh_locks: Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    http_client: reqwest::Client,
    token_request_timeout: Duration,
}

impl OAuthTokenManager {
    /// Create a new manager.
    ///
    /// - `store`: the DB-backed config store, used to persist rotated
    ///   refresh tokens. `None` in legacy/test mode (no persistence).
    /// - `http_client`: shared reqwest client for token refresh calls.
    pub fn new(store: Option<Arc<DbConfigStore>>, http_client: reqwest::Client) -> Self {
        let token_store = store.as_ref().map(|store| store.oauth_token_store());
        Self {
            cache: OAuthTokenCache::global(),
            store,
            token_store,
            local_refresh_locks: Arc::new(DashMap::new()),
            http_client,
            token_request_timeout: TOKEN_REQUEST_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn new_with_cache(
        store: Option<Arc<DbConfigStore>>,
        http_client: reqwest::Client,
        cache: &'static OAuthTokenCache,
    ) -> Self {
        let token_store = store.as_ref().map(|store| store.oauth_token_store());
        Self {
            cache,
            store,
            token_store,
            local_refresh_locks: Arc::new(DashMap::new()),
            http_client,
            token_request_timeout: TOKEN_REQUEST_TIMEOUT,
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

        let label = oauth.cache_label();
        if self
            .cache
            .apply_cached(headers, &target.provider_id, label, oauth)?
        {
            remember_applied_access_token(headers, oauth);
            return Ok(true);
        }

        // Legacy/no-DB deployments retain the original process-local path.
        if self.token_store.is_none() {
            self.cache
                .seed(&target.provider_id, label, &oauth.refresh_token);
            self.cache
                .apply(
                    headers,
                    &target.provider_id,
                    label,
                    oauth,
                    &self.http_client,
                )
                .await?;
            remember_applied_access_token(headers, oauth);
            return Ok(true);
        }

        let config = self
            .ensure_provider_token(&target.provider_id, OAuthRefreshMode::IfNeeded, None)
            .await?;
        let applied =
            self.cache
                .apply_cached(headers, &target.provider_id, config.cache_label(), &config)?;
        if !applied {
            return Err("OAuth refresh completed without a usable access token".to_string());
        }
        remember_applied_access_token(headers, &config);
        Ok(true)
    }

    /// Return the access token currently cached for this OAuth target.
    #[cfg(test)]
    pub(crate) fn cached_access_token(&self, target: &RoutingTarget) -> Option<String> {
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
        if self
            .cache
            .get_access_token(&target.provider_id, oauth.cache_label())
            .is_some_and(|current| current != rejected_access_token)
        {
            return Ok(true);
        }
        self.cache.invalidate_access_token_if_matches(
            &target.provider_id,
            oauth.cache_label(),
            rejected_access_token,
        );
        if self.token_store.is_none() {
            let mut headers = HeaderMap::new();
            return self.apply(target, &mut headers).await;
        }
        self.ensure_provider_token(
            &target.provider_id,
            OAuthRefreshMode::IfNeeded,
            Some(rejected_access_token),
        )
        .await?;
        Ok(true)
    }

    /// Explicitly refresh one provider, used by the Admin API.
    pub async fn force_refresh(
        &self,
        provider_id: &str,
    ) -> Result<OAuthCredentialRefreshSummary, String> {
        self.ensure_provider_token(provider_id, OAuthRefreshMode::Force, None)
            .await?;
        self.refresh_summary(provider_id).await
    }

    async fn mutate_credentials_locked(
        &self,
        provider_id: &str,
        mutation: OAuthProviderMutation,
    ) -> Result<(), String> {
        let token_store = self
            .token_store
            .as_ref()
            .ok_or_else(|| "OAuth token store is unavailable".to_string())?;
        let local_lock = self
            .local_refresh_locks
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = local_lock.lock().await;

        let mutation_result = match token_store.kind() {
            DbKind::Sqlite => {
                let mutation_result = mutation().await;
                token_store
                    .reset(provider_id, None)
                    .await
                    .map_err(|error| error.to_string())?;
                mutation_result
            }
            DbKind::Postgres => {
                let deadline = Instant::now() + REFRESH_LOCK_WAIT;
                let mut mutation = Some(mutation);
                loop {
                    match token_store
                        .try_begin_postgres_refresh(provider_id)
                        .await
                        .map_err(|error| error.to_string())?
                    {
                        Some(mut tx) => {
                            // Stage token-state deletion before the external
                            // mutation. The advisory lock prevents refreshers
                            // from observing the intermediate state.
                            token_store
                                .reset_in_transaction(&mut tx, provider_id, None)
                                .await
                                .map_err(|error| error.to_string())?;
                            let mutation = mutation.take().ok_or_else(|| {
                                "OAuth provider mutation was already consumed".to_string()
                            })?;
                            let mutation_result = mutation().await;
                            tx.commit().await.map_err(|error| error.to_string())?;
                            break mutation_result;
                        }
                        None if Instant::now() >= deadline => {
                            return Err(format!(
                                "timed out coordinating OAuth credential update for provider {provider_id}"
                            ));
                        }
                        None => tokio::time::sleep(REFRESH_LOCK_POLL).await,
                    }
                }
            }
        };

        self.cache.invalidate_provider(provider_id);
        if let Some(store) = self.store.as_ref() {
            store.refresh().await.map_err(|error| error.to_string())?;
        }
        mutation_result
    }

    /// Non-blocking background keepalive. A busy PostgreSQL credential is
    /// skipped so another instance can finish its refresh.
    pub async fn try_keepalive_provider(
        &self,
        provider_id: &str,
    ) -> Result<OAuthRefreshOutcome, String> {
        if self.token_store.is_none() {
            return Ok(OAuthRefreshOutcome::Skipped);
        }
        let result = self
            .refresh_coordinated(provider_id, OAuthRefreshMode::Keepalive, None, false)
            .await
            .map(|(_, outcome)| outcome);
        if result.is_err() {
            if let Err(error) = self.ensure_keepalive_preflight_backoff(provider_id).await {
                warn!(provider = %provider_id, error = %error, "failed to persist OAuth keepalive preflight backoff");
            }
        }
        result
    }

    pub async fn due_keepalive_provider_ids(&self, limit: i64) -> Result<Vec<String>, String> {
        let Some(token_store) = self.token_store.as_ref() else {
            return Ok(Vec::new());
        };
        token_store
            .list_due_provider_ids(Utc::now(), limit)
            .await
            .map_err(|error| error.to_string())
    }

    async fn ensure_keepalive_preflight_backoff(&self, provider_id: &str) -> Result<(), String> {
        let token_store = self
            .token_store
            .as_ref()
            .ok_or_else(|| "OAuth token store is unavailable".to_string())?;
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| "OAuth config store is unavailable".to_string())?;
        let local_lock = self
            .local_refresh_locks
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = local_lock.lock().await;

        match token_store.kind() {
            DbKind::Sqlite => {
                let Some(provider) = store
                    .get_provider(provider_id)
                    .await
                    .map_err(|error| error.to_string())?
                else {
                    return Ok(());
                };
                if provider_has_usable_oauth_credentials(&provider) {
                    return Ok(());
                }
                let state = token_store
                    .load(provider_id)
                    .await
                    .map_err(|error| error.to_string())?;
                if state.as_ref().is_some_and(refresh_backoff_is_active) {
                    return Ok(());
                }
                let failure_count = state.map_or(0, |state| state.failure_count);
                token_store
                    .record_failure(
                        provider_id,
                        next_retry_at(OAuthRefreshFailureKind::Transient, failure_count),
                    )
                    .await
                    .map_err(|error| error.to_string())
            }
            DbKind::Postgres => {
                let Some(mut tx) = token_store
                    .try_begin_postgres_refresh(provider_id)
                    .await
                    .map_err(|error| error.to_string())?
                else {
                    return Ok(());
                };
                let Some(provider) = store
                    .get_provider_in_transaction(&mut tx, provider_id)
                    .await
                    .map_err(|error| error.to_string())?
                else {
                    tx.commit().await.map_err(|error| error.to_string())?;
                    return Ok(());
                };
                if provider_has_usable_oauth_credentials(&provider) {
                    tx.commit().await.map_err(|error| error.to_string())?;
                    return Ok(());
                }
                let state = token_store
                    .load_in_transaction(&mut tx, provider_id)
                    .await
                    .map_err(|error| error.to_string())?;
                if state.as_ref().is_some_and(refresh_backoff_is_active) {
                    tx.commit().await.map_err(|error| error.to_string())?;
                    return Ok(());
                }
                let failure_count = state.map_or(0, |state| state.failure_count);
                token_store
                    .record_failure_in_transaction(
                        &mut tx,
                        provider_id,
                        next_retry_at(OAuthRefreshFailureKind::Transient, failure_count),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                tx.commit().await.map_err(|error| error.to_string())
            }
        }
    }

    async fn ensure_provider_token(
        &self,
        provider_id: &str,
        mode: OAuthRefreshMode,
        rejected_access_token: Option<&str>,
    ) -> Result<OAuthTargetConfig, String> {
        let config = self.load_oauth_config(provider_id).await?;
        if mode == OAuthRefreshMode::IfNeeded
            && self
                .reuse_shared_token(provider_id, &config, rejected_access_token)
                .await?
        {
            return Ok(config);
        }
        self.refresh_coordinated(provider_id, mode, rejected_access_token, true)
            .await
            .map(|(config, _)| config)
    }

    async fn refresh_coordinated(
        &self,
        provider_id: &str,
        mode: OAuthRefreshMode,
        rejected_access_token: Option<&str>,
        wait_for_lock: bool,
    ) -> Result<(OAuthTargetConfig, OAuthRefreshOutcome), String> {
        let token_store = self
            .token_store
            .as_ref()
            .ok_or_else(|| "OAuth shared token store is unavailable".to_string())?;
        let local_lock = self
            .local_refresh_locks
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _local_guard = local_lock.lock().await;

        let (provider, config) = self.load_oauth_provider(provider_id).await?;
        if mode == OAuthRefreshMode::IfNeeded
            && self
                .reuse_shared_token(provider_id, &config, rejected_access_token)
                .await?
        {
            return Ok((config, OAuthRefreshOutcome::ReusedShared));
        }
        let keepalive_interval = self.keepalive_interval().await;

        match token_store.kind() {
            DbKind::Sqlite => {
                let state = token_store
                    .load(provider_id)
                    .await
                    .map_err(|error| error.to_string())?;
                if mode != OAuthRefreshMode::Force
                    && state.as_ref().is_some_and(refresh_backoff_is_active)
                {
                    if mode == OAuthRefreshMode::Keepalive {
                        return Ok((config, OAuthRefreshOutcome::Skipped));
                    }
                    return Err(refresh_backoff_error());
                }
                if mode == OAuthRefreshMode::Keepalive
                    && state
                        .as_ref()
                        .and_then(|state| state.next_keepalive_at)
                        .is_some_and(|next| next > Utc::now())
                {
                    return Ok((config, OAuthRefreshOutcome::Skipped));
                }
                self.perform_refresh(provider_id, provider, config, keepalive_interval, None)
                    .await
            }
            DbKind::Postgres => {
                let deadline = Instant::now() + REFRESH_LOCK_WAIT;
                loop {
                    match token_store
                        .try_begin_postgres_refresh(provider_id)
                        .await
                        .map_err(|error| error.to_string())?
                    {
                        Some(mut tx) => {
                            let store = self
                                .store
                                .as_ref()
                                .ok_or_else(|| "OAuth config store is unavailable".to_string())?;
                            let current_provider = store
                                .get_provider_in_transaction(&mut tx, provider_id)
                                .await
                                .map_err(|error| error.to_string())?
                                .ok_or_else(|| format!("provider {provider_id} not found"))?;
                            let current_config = build_oauth_target_config(&current_provider)
                                .ok_or_else(|| {
                                    format!(
                                        "provider {provider_id} has no usable OAuth configuration"
                                    )
                                })?;
                            let state = token_store
                                .load_in_transaction(&mut tx, provider_id)
                                .await
                                .map_err(|error| error.to_string())?;
                            if mode == OAuthRefreshMode::IfNeeded {
                                if let Some(state) = state.as_ref() {
                                    if shared_token_is_usable(state, rejected_access_token) {
                                        tx.commit().await.map_err(|error| error.to_string())?;
                                        self.seed_shared(provider_id, &current_config, state);
                                        return Ok((
                                            current_config,
                                            OAuthRefreshOutcome::ReusedShared,
                                        ));
                                    }
                                }
                            }
                            if mode != OAuthRefreshMode::Force
                                && state.as_ref().is_some_and(refresh_backoff_is_active)
                            {
                                tx.commit().await.map_err(|error| error.to_string())?;
                                if mode == OAuthRefreshMode::Keepalive {
                                    return Ok((current_config, OAuthRefreshOutcome::Skipped));
                                }
                                return Err(refresh_backoff_error());
                            }
                            if mode == OAuthRefreshMode::Keepalive
                                && state
                                    .as_ref()
                                    .and_then(|state| state.next_keepalive_at)
                                    .is_some_and(|next| next > Utc::now())
                            {
                                tx.commit().await.map_err(|error| error.to_string())?;
                                return Ok((current_config, OAuthRefreshOutcome::Skipped));
                            }
                            return self
                                .perform_refresh(
                                    provider_id,
                                    current_provider,
                                    current_config,
                                    keepalive_interval,
                                    Some(tx),
                                )
                                .await;
                        }
                        None if !wait_for_lock => {
                            return Ok((config, OAuthRefreshOutcome::Skipped));
                        }
                        None => {
                            if self
                                .reuse_shared_token(provider_id, &config, rejected_access_token)
                                .await?
                            {
                                return Ok((config, OAuthRefreshOutcome::ReusedShared));
                            }
                            if Instant::now() >= deadline {
                                return Err(format!(
                                    "timed out waiting for OAuth refresh coordination for provider {provider_id}"
                                ));
                            }
                            tokio::time::sleep(REFRESH_LOCK_POLL).await;
                        }
                    }
                }
            }
        }
    }

    async fn perform_refresh(
        &self,
        provider_id: &str,
        provider: tiygate_store::models::Provider,
        config: OAuthTargetConfig,
        keepalive_interval: Duration,
        tx: Option<sqlx::Transaction<'static, sqlx::Any>>,
    ) -> Result<(OAuthTargetConfig, OAuthRefreshOutcome), String> {
        if config.refresh_token.is_empty() {
            return Err(format!("provider {provider_id} has no refresh token"));
        }

        let result = tokio::time::timeout(
            self.token_request_timeout,
            do_refresh_token(
                &config.token_url,
                &config.client_id,
                &config.refresh_token,
                &config.scopes,
                &config.token_request_style,
                &self.http_client,
            ),
        )
        .await
        .map_err(|_| {
            format!(
                "OAuth token refresh timed out after {} seconds",
                self.token_request_timeout.as_secs()
            )
        })
        .and_then(|result| result);
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                let kind = classify_refresh_failure(&error);
                self.record_refresh_failure(&provider, kind, tx).await;
                return Err(format!(
                    "OAuth credential refresh failed: {}",
                    kind.status_reason()
                ));
            }
        };

        self.persist_token_result(
            provider_id,
            &provider,
            &config,
            result,
            keepalive_interval,
            tx,
        )
        .await
    }

    async fn persist_token_result(
        &self,
        provider_id: &str,
        provider: &tiygate_store::models::Provider,
        config: &OAuthTargetConfig,
        result: TokenResult,
        keepalive_interval: Duration,
        mut tx: Option<sqlx::Transaction<'static, sqlx::Any>>,
    ) -> Result<(OAuthTargetConfig, OAuthRefreshOutcome), String> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| "OAuth config store is unavailable".to_string())?;
        let token_store = self
            .token_store
            .as_ref()
            .ok_or_else(|| "OAuth token store is unavailable".to_string())?;
        let mut meta: serde_json::Value = provider
            .oauth_meta_cleartext
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or_else(|| json!({}));
        let object = meta
            .as_object_mut()
            .ok_or_else(|| "stored OAuth metadata is not an object".to_string())?;
        let effective_refresh_token = result
            .refresh_token
            .as_deref()
            .unwrap_or(&config.refresh_token);
        object.insert("refresh_token".to_string(), json!(effective_refresh_token));
        let account_id = result
            .account_id
            .clone()
            .or_else(|| config.account_id.clone());
        if let Some(account_id) = account_id.as_deref() {
            object.insert("account_id".to_string(), json!(account_id));
        }
        if let Some(account_email) = result.account_email.as_deref() {
            object.insert("account_email".to_string(), json!(account_email));
        }
        object.insert(
            "expires_in_s".to_string(),
            result
                .expires_in
                .map(|duration| json!(duration.as_secs()))
                .unwrap_or(serde_json::Value::Null),
        );
        object.insert(
            "status".to_string(),
            json!(OAuthCredentialStatus::Healthy.as_str()),
        );
        object.insert(
            "status_checked_at".to_string(),
            json!(Utc::now().to_rfc3339()),
        );
        object.remove("status_reason");
        let meta_plain = serde_json::to_string(&meta).map_err(|error| error.to_string())?;
        let expires_at = result
            .expires_in
            .and_then(|duration| chrono::Duration::from_std(duration).ok())
            .map(|duration| Utc::now() + duration);
        let next_keepalive_at = Utc::now()
            + chrono::Duration::from_std(keepalive_interval)
                .unwrap_or_else(|_| chrono::Duration::days(7));
        let commit = OAuthTokenCommit {
            provider_id,
            access_token: &result.access_token,
            access_expires_at: expires_at,
            oauth_meta_plain: &meta_plain,
            next_keepalive_at,
        };
        let version = match tx.as_mut() {
            Some(transaction) => token_store
                .commit_success_in_transaction(transaction, commit)
                .await
                .map_err(|error| error.to_string())?,
            None => token_store
                .commit_success(commit)
                .await
                .map_err(|error| error.to_string())?,
        };
        if let Some(transaction) = tx {
            transaction
                .commit()
                .await
                .map_err(|error| error.to_string())?;
        }
        if let Err(error) = store.refresh().await {
            warn!(provider = %provider_id, error = %error, "refreshing OAuth provider snapshot failed");
        }

        let refreshed_config = self.load_oauth_config(provider_id).await?;
        self.cache.seed_tokens_with_identity(
            provider_id,
            refreshed_config.cache_label(),
            &result.access_token,
            effective_refresh_token,
            result.expires_in,
            OAuthTokenIdentity {
                account_id,
                account_email: result.account_email,
            },
        );
        info!(provider = %provider_id, credential_version = version, "OAuth credential refreshed and persisted");
        Ok((refreshed_config, OAuthRefreshOutcome::Refreshed))
    }

    async fn install_tokens(
        &self,
        provider_id: &str,
        result: TokenResult,
    ) -> Result<OAuthCredentialRefreshSummary, String> {
        let token_store = self
            .token_store
            .as_ref()
            .ok_or_else(|| "OAuth token store is unavailable".to_string())?;
        let local_lock = self
            .local_refresh_locks
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = local_lock.lock().await;
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| "OAuth config store is unavailable".to_string())?;
        let provider = store
            .get_provider(provider_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("provider {provider_id} not found"))?;
        let config = build_oauth_target_config(&provider)
            .ok_or_else(|| format!("provider {provider_id} has no usable OAuth configuration"))?;
        let expires_in = result.expires_in;
        let keepalive_interval = self.keepalive_interval().await;
        let tx = if token_store.kind() == DbKind::Postgres {
            let deadline = Instant::now() + REFRESH_LOCK_WAIT;
            loop {
                if let Some(tx) = token_store
                    .try_begin_postgres_refresh(provider_id)
                    .await
                    .map_err(|error| error.to_string())?
                {
                    break Some(tx);
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "timed out coordinating OAuth token installation for provider {provider_id}"
                    ));
                }
                tokio::time::sleep(REFRESH_LOCK_POLL).await;
            }
        } else {
            None
        };
        self.persist_token_result(
            provider_id,
            &provider,
            &config,
            result,
            keepalive_interval,
            tx,
        )
        .await?;
        Ok(OAuthCredentialRefreshSummary { expires_in })
    }

    async fn apply_provider_by_id(
        &self,
        provider_id: &str,
        headers: &mut HeaderMap,
    ) -> Result<(), String> {
        let config = self
            .ensure_provider_token(provider_id, OAuthRefreshMode::IfNeeded, None)
            .await?;
        if !self
            .cache
            .apply_cached(headers, provider_id, config.cache_label(), &config)?
        {
            return Err("OAuth access token is unavailable after refresh".to_string());
        }
        remember_applied_access_token(headers, &config);
        Ok(())
    }

    async fn refresh_summary(
        &self,
        provider_id: &str,
    ) -> Result<OAuthCredentialRefreshSummary, String> {
        let state = self
            .token_store
            .as_ref()
            .ok_or_else(|| "OAuth token store is unavailable".to_string())?
            .load(provider_id)
            .await
            .map_err(|error| error.to_string())?;
        let expires_in = state
            .and_then(|state| state.access_expires_at)
            .and_then(|expires_at| (expires_at - Utc::now()).to_std().ok());
        Ok(OAuthCredentialRefreshSummary { expires_in })
    }

    async fn reuse_shared_token(
        &self,
        provider_id: &str,
        config: &OAuthTargetConfig,
        rejected_access_token: Option<&str>,
    ) -> Result<bool, String> {
        let Some(token_store) = self.token_store.as_ref() else {
            return Ok(false);
        };
        let Some(state) = token_store
            .load(provider_id)
            .await
            .map_err(|error| error.to_string())?
        else {
            return Ok(false);
        };
        if !shared_token_is_usable(&state, rejected_access_token) {
            return Ok(false);
        }
        self.seed_shared(provider_id, config, &state);
        Ok(true)
    }

    fn seed_shared(
        &self,
        provider_id: &str,
        config: &OAuthTargetConfig,
        state: &OAuthAccessTokenState,
    ) {
        let expires_in = state
            .access_expires_at
            .and_then(|expires_at| (expires_at - Utc::now()).to_std().ok());
        self.cache.seed_tokens_with_identity(
            provider_id,
            config.cache_label(),
            &state.access_token,
            &config.refresh_token,
            expires_in,
            OAuthTokenIdentity {
                account_id: config.account_id.clone(),
                account_email: None,
            },
        );
    }

    async fn load_oauth_config(&self, provider_id: &str) -> Result<OAuthTargetConfig, String> {
        self.load_oauth_provider(provider_id)
            .await
            .map(|(_, config)| config)
    }

    async fn load_oauth_provider(
        &self,
        provider_id: &str,
    ) -> Result<(tiygate_store::models::Provider, OAuthTargetConfig), String> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| "OAuth config store is unavailable".to_string())?;
        let provider = store
            .get_provider(provider_id)
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("provider {provider_id} not found"))?;
        let config = build_oauth_target_config(&provider)
            .ok_or_else(|| format!("provider {provider_id} has no usable OAuth configuration"))?;
        Ok((provider, config))
    }

    async fn keepalive_interval(&self) -> Duration {
        let Some(store) = self.store.as_ref() else {
            return DEFAULT_KEEPALIVE_INTERVAL;
        };
        let seconds = tiygate_store::settings_keys::get_u64(
            store.as_ref(),
            tiygate_store::settings_keys::OAUTH_KEEPALIVE_INTERVAL_SECS,
            DEFAULT_KEEPALIVE_INTERVAL.as_secs(),
        )
        .await;
        Duration::from_secs(seconds.max(60))
    }

    async fn record_refresh_failure(
        &self,
        provider: &tiygate_store::models::Provider,
        kind: OAuthRefreshFailureKind,
        tx: Option<sqlx::Transaction<'static, sqlx::Any>>,
    ) {
        let provider_id = provider.id.as_str();
        let status = match kind {
            OAuthRefreshFailureKind::CredentialInvalid => OAuthCredentialStatus::Invalid,
            OAuthRefreshFailureKind::Transient => OAuthCredentialStatus::Error,
        };
        let Some(token_store) = self.token_store.as_ref() else {
            return;
        };
        let backoff_result = match tx {
            Some(mut transaction) => {
                let failure_count = token_store
                    .load_in_transaction(&mut transaction, provider_id)
                    .await
                    .map(|state| state.map_or(0, |state| state.failure_count));
                match failure_count {
                    Ok(failure_count) => {
                        let result = token_store
                            .record_failure_in_transaction(
                                &mut transaction,
                                provider_id,
                                next_retry_at(kind, failure_count),
                            )
                            .await;
                        match result {
                            Ok(()) => transaction.commit().await.map_err(Into::into),
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            None => {
                let failure_count = token_store
                    .load(provider_id)
                    .await
                    .map(|state| state.map_or(0, |state| state.failure_count));
                match failure_count {
                    Ok(failure_count) => {
                        token_store
                            .record_failure(provider_id, next_retry_at(kind, failure_count))
                            .await
                    }
                    Err(error) => Err(error),
                }
            }
        };
        if let Err(store_error) = backoff_result {
            warn!(provider = %provider_id, error = %store_error, "failed to persist OAuth refresh backoff");
        }

        if let Some(store) = self.store.as_ref() {
            if let Err(status_error) = store
                .set_provider_oauth_status_if_unchanged(
                    provider,
                    status,
                    Some(kind.status_reason()),
                )
                .await
            {
                warn!(provider = %provider_id, error = %status_error, "failed to persist OAuth refresh failure status");
            }
        }
    }
}

#[async_trait::async_trait]
impl OAuthCredentialService for OAuthTokenManager {
    async fn apply_provider_headers(
        &self,
        provider_id: &str,
        headers: &mut HeaderMap,
    ) -> Result<(), String> {
        self.apply_provider_by_id(provider_id, headers).await
    }

    async fn force_refresh_provider(
        &self,
        provider_id: &str,
    ) -> Result<OAuthCredentialRefreshSummary, String> {
        self.force_refresh(provider_id).await
    }

    async fn install_provider_tokens(
        &self,
        provider_id: &str,
        tokens: TokenResult,
    ) -> Result<OAuthCredentialRefreshSummary, String> {
        self.install_tokens(provider_id, tokens).await
    }

    async fn mutate_provider_credentials(
        &self,
        provider_id: &str,
        mutation: OAuthProviderMutation,
    ) -> Result<(), String> {
        self.mutate_credentials_locked(provider_id, mutation).await
    }
}

fn shared_token_is_usable(
    state: &OAuthAccessTokenState,
    rejected_access_token: Option<&str>,
) -> bool {
    if state.access_token.is_empty()
        || rejected_access_token.is_some_and(|rejected| rejected == state.access_token)
    {
        return false;
    }
    state.access_expires_at.is_none_or(|expires_at| {
        expires_at
            > Utc::now()
                + chrono::Duration::from_std(ACCESS_TOKEN_LEEWAY)
                    .unwrap_or_else(|_| chrono::Duration::seconds(60))
    })
}

fn refresh_backoff_is_active(state: &OAuthAccessTokenState) -> bool {
    state
        .next_retry_at
        .is_some_and(|next_retry| next_retry > Utc::now())
}

fn provider_has_usable_oauth_credentials(provider: &tiygate_store::models::Provider) -> bool {
    build_oauth_target_config(provider)
        .is_some_and(|config| !config.refresh_token.trim().is_empty())
}

fn refresh_backoff_error() -> String {
    "OAuth credential refresh is temporarily backed off; reconnect the credential or use the manual refresh action"
        .to_string()
}

fn next_retry_at(kind: OAuthRefreshFailureKind, failure_count: i32) -> chrono::DateTime<Utc> {
    if kind == OAuthRefreshFailureKind::CredentialInvalid {
        return Utc::now() + chrono::Duration::days(3650);
    }
    let backoff_minutes = 5_i64.saturating_mul(1_i64 << failure_count.min(6));
    Utc::now() + chrono::Duration::minutes(backoff_minutes.min(360))
}

fn remember_applied_access_token(headers: &HeaderMap, config: &OAuthTargetConfig) {
    let Some(token) = headers
        .get(config.header_name())
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix(config.bearer_prefix()))
    else {
        return;
    };
    let _ = APPLIED_OAUTH_ACCESS_TOKEN.try_with(|slot| {
        *slot.borrow_mut() = Some(token.to_string());
    });
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use tiygate_core::provider::oauth::{OAuthTargetConfig, TokenRequestStyle, UpstreamTransport};
    use tiygate_store::db;
    use tiygate_store::models::AuthMode;
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

    #[tokio::test]
    async fn sqlite_concurrent_requests_share_one_refresh_grant_and_rotated_rt() {
        let token_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "shared-access",
                "refresh_token": "rotated-refresh",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&token_server)
            .await;

        let pool = db::open_pool("sqlite::memory:").await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        let store = Arc::new(DbConfigStore::new(pool, None));
        store.refresh().await.unwrap();
        let provider_id = "sqlite-concurrent-oauth";
        store
            .upsert_provider(
                provider_id,
                "SQLite OAuth",
                "openai",
                "https://example.test",
                "",
                None,
                AuthMode::OAuth,
                Some(&serde_json::json!({"refresh_token": "initial-refresh"}).to_string()),
                serde_json::json!({
                    "oauth": {
                        "token_url": format!("{}/token", token_server.uri()),
                        "client_id": "test-client",
                        "scopes": ["openid"],
                        "token_request_style": "form"
                    }
                }),
                true,
            )
            .await
            .unwrap();
        let provider = store.get_provider(provider_id).await.unwrap().unwrap();
        let oauth = build_oauth_target_config(&provider).unwrap();
        let target = RoutingTarget {
            provider_id: provider_id.to_string(),
            model_id: "model".to_string(),
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
            oauth: Some(oauth),
        };
        let manager = Arc::new(OAuthTokenManager::new(
            Some(store.clone()),
            reqwest::Client::new(),
        ));

        let mut joins = Vec::new();
        for _ in 0..16 {
            let manager = manager.clone();
            let target = target.clone();
            joins.push(tokio::spawn(async move {
                let mut headers = HeaderMap::new();
                manager.apply(&target, &mut headers).await.unwrap();
                headers["authorization"].clone()
            }));
        }
        for joined in joins {
            assert_eq!(joined.await.unwrap(), "Bearer shared-access");
        }

        let provider = store.get_provider(provider_id).await.unwrap().unwrap();
        let meta: serde_json::Value =
            serde_json::from_str(provider.oauth_meta_cleartext.as_deref().unwrap()).unwrap();
        assert_eq!(meta["refresh_token"], "rotated-refresh");
        let shared = store
            .oauth_token_store()
            .load(provider_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(shared.credential_version, 1);
        assert_eq!(shared.access_token, "shared-access");
    }

    #[tokio::test]
    async fn refresh_without_rotated_rt_preserves_existing_rt() {
        let token_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-without-new-rt",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&token_server)
            .await;

        let pool = db::open_pool("sqlite::memory:").await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        let store = Arc::new(DbConfigStore::new(pool, None));
        store.refresh().await.unwrap();
        let provider_id = "sqlite-preserve-refresh-token";
        store
            .upsert_provider(
                provider_id,
                "SQLite OAuth",
                "openai",
                "https://example.test",
                "",
                None,
                AuthMode::OAuth,
                Some(&serde_json::json!({"refresh_token": "existing-refresh"}).to_string()),
                serde_json::json!({
                    "oauth": {
                        "token_url": format!("{}/token", token_server.uri()),
                        "client_id": "test-client",
                        "scopes": ["openid"],
                        "token_request_style": "form"
                    }
                }),
                true,
            )
            .await
            .unwrap();
        let manager = OAuthTokenManager::new(Some(store.clone()), reqwest::Client::new());

        manager.force_refresh(provider_id).await.unwrap();

        let provider = store.get_provider(provider_id).await.unwrap().unwrap();
        let meta: serde_json::Value =
            serde_json::from_str(provider.oauth_meta_cleartext.as_deref().unwrap()).unwrap();
        assert_eq!(meta["refresh_token"], "existing-refresh");
    }

    #[tokio::test]
    async fn refresh_timeout_records_backoff_and_hot_path_honors_it() {
        let token_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(250))
                    .set_body_json(serde_json::json!({
                        "access_token": "must-not-be-installed",
                        "refresh_token": "must-not-be-rotated",
                        "expires_in": 3600
                    })),
            )
            .expect(1)
            .mount(&token_server)
            .await;

        let pool = db::open_pool("sqlite::memory:").await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        let store = Arc::new(DbConfigStore::new(pool, None));
        store.refresh().await.unwrap();
        let provider_id = "sqlite-refresh-timeout-backoff";
        store
            .upsert_provider(
                provider_id,
                "SQLite OAuth timeout",
                "openai",
                "https://example.test",
                "",
                None,
                AuthMode::OAuth,
                Some(&serde_json::json!({"refresh_token": "existing-refresh"}).to_string()),
                serde_json::json!({
                    "oauth": {
                        "token_url": format!("{}/token", token_server.uri()),
                        "client_id": "test-client",
                        "scopes": ["openid"],
                        "token_request_style": "form"
                    }
                }),
                true,
            )
            .await
            .unwrap();
        let provider = store.get_provider(provider_id).await.unwrap().unwrap();
        let mut target = make_target(Some(build_oauth_target_config(&provider).unwrap()));
        target.provider_id = provider_id.to_string();
        let cache = Box::leak(Box::new(OAuthTokenCache::new()));
        let mut manager =
            OAuthTokenManager::new_with_cache(Some(store.clone()), reqwest::Client::new(), cache);
        manager.token_request_timeout = Duration::from_millis(20);

        assert!(manager.force_refresh(provider_id).await.is_err());
        let state = store
            .oauth_token_store()
            .load(provider_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.failure_count, 1);
        assert!(state.next_retry_at.is_some_and(|retry| retry > Utc::now()));

        let mut headers = HeaderMap::new();
        let error = manager.apply(&target, &mut headers).await.unwrap_err();
        assert!(error.contains("temporarily backed off"));
    }

    #[tokio::test]
    async fn keepalive_preflight_failures_do_not_starve_later_candidates() {
        let pool = db::open_pool("sqlite::memory:").await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        let store = Arc::new(DbConfigStore::new(pool, None));
        store.refresh().await.unwrap();
        let oauth_config = serde_json::json!({
            "oauth": {
                "token_url": "https://example.test/token",
                "client_id": "test-client",
                "scopes": ["openid"],
                "token_request_style": "form"
            }
        });
        let invalid_ids = [
            "keepalive-invalid-a",
            "keepalive-invalid-b",
            "keepalive-invalid-c",
        ];
        for provider_id in invalid_ids {
            store
                .upsert_provider(
                    provider_id,
                    provider_id,
                    "openai",
                    "https://example.test",
                    "",
                    None,
                    AuthMode::OAuth,
                    None,
                    oauth_config.clone(),
                    true,
                )
                .await
                .unwrap();
        }
        let valid_id = "keepalive-valid-later";
        store
            .upsert_provider(
                valid_id,
                valid_id,
                "openai",
                "https://example.test",
                "",
                None,
                AuthMode::OAuth,
                Some(&serde_json::json!({"refresh_token": "valid-refresh"}).to_string()),
                oauth_config,
                true,
            )
            .await
            .unwrap();
        let manager = OAuthTokenManager::new(Some(store.clone()), reqwest::Client::new());

        let mut selected_valid = false;
        for _ in 0..3 {
            let provider_ids = manager.due_keepalive_provider_ids(2).await.unwrap();
            selected_valid = provider_ids
                .iter()
                .any(|provider_id| provider_id == valid_id);
            for provider_id in provider_ids
                .into_iter()
                .filter(|provider_id| provider_id != valid_id)
            {
                assert!(manager.try_keepalive_provider(&provider_id).await.is_err());
            }
            if selected_valid {
                break;
            }
        }

        assert!(selected_valid, "invalid providers must leave the due batch");
        for provider_id in invalid_ids {
            let state = store
                .oauth_token_store()
                .load(provider_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(state.failure_count, 1);
            assert!(state.next_retry_at.is_some_and(|retry| retry > Utc::now()));
        }
    }

    #[tokio::test]
    async fn keepalive_preflight_backoff_expires_and_corrected_provider_recovers() {
        let token_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "recovered-access",
                "refresh_token": "recovered-refresh",
                "expires_in": 3600
            })))
            .expect(1)
            .mount(&token_server)
            .await;

        let pool = db::open_pool("sqlite::memory:").await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        let store = Arc::new(DbConfigStore::new(pool, None));
        store.refresh().await.unwrap();
        let provider_id = "keepalive-preflight-recovery";
        store
            .upsert_provider(
                provider_id,
                provider_id,
                "openai",
                "https://example.test",
                "",
                None,
                AuthMode::OAuth,
                None,
                serde_json::json!({
                    "oauth": {
                        "token_url": format!("{}/token", token_server.uri()),
                        "client_id": "test-client",
                        "scopes": ["openid"],
                        "token_request_style": "form"
                    }
                }),
                true,
            )
            .await
            .unwrap();
        let manager = OAuthTokenManager::new(Some(store.clone()), reqwest::Client::new());

        assert!(manager.try_keepalive_provider(provider_id).await.is_err());
        store
            .set_provider_oauth_meta(
                provider_id,
                &serde_json::json!({"refresh_token": "corrected-refresh"}).to_string(),
            )
            .await
            .unwrap();
        store
            .oauth_token_store()
            .record_failure(provider_id, Utc::now() - chrono::Duration::seconds(1))
            .await
            .unwrap();

        assert_eq!(
            manager.try_keepalive_provider(provider_id).await.unwrap(),
            OAuthRefreshOutcome::Refreshed
        );
        let state = store
            .oauth_token_store()
            .load(provider_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.access_token, "recovered-access");
        assert!(state.next_retry_at.is_none());
        assert_eq!(state.failure_count, 0);
    }

    #[tokio::test]
    async fn unauthorized_reuses_newer_shared_at_before_refreshing() {
        let pool = db::open_pool("sqlite::memory:").await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        let store = Arc::new(DbConfigStore::new(pool, None));
        store.refresh().await.unwrap();
        let provider_id = "sqlite-401-shared-access";
        let oauth_meta = serde_json::json!({"refresh_token": "existing-refresh"}).to_string();
        store
            .upsert_provider(
                provider_id,
                "SQLite OAuth",
                "openai",
                "https://example.test",
                "",
                None,
                AuthMode::OAuth,
                Some(&oauth_meta),
                serde_json::json!({
                    "oauth": {
                        "token_url": "http://127.0.0.1:1/must-not-be-called",
                        "client_id": "test-client",
                        "scopes": ["openid"],
                        "token_request_style": "form"
                    }
                }),
                true,
            )
            .await
            .unwrap();
        store
            .oauth_token_store()
            .commit_success(OAuthTokenCommit {
                provider_id,
                access_token: "shared-new-access",
                access_expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
                oauth_meta_plain: &oauth_meta,
                next_keepalive_at: Utc::now() + chrono::Duration::days(7),
            })
            .await
            .unwrap();
        store.refresh().await.unwrap();
        let provider = store.get_provider(provider_id).await.unwrap().unwrap();
        let oauth = build_oauth_target_config(&provider).unwrap();
        let mut target = make_target(Some(oauth.clone()));
        target.provider_id = provider_id.to_string();
        let cache = Box::leak(Box::new(OAuthTokenCache::new()));
        let manager =
            OAuthTokenManager::new_with_cache(Some(store.clone()), reqwest::Client::new(), cache);

        let (applied, used_access_token) = capture_applied_access_token(async {
            let mut headers = HeaderMap::new();
            manager.apply(&target, &mut headers).await
        })
        .await;
        assert!(applied.unwrap());
        assert_eq!(used_access_token.as_deref(), Some("shared-new-access"));

        cache.seed_tokens(
            provider_id,
            oauth.cache_label(),
            "rejected-local-access",
            "existing-refresh",
            Some(Duration::from_secs(3600)),
        );

        assert!(manager
            .refresh_after_unauthorized(&target, "rejected-local-access")
            .await
            .unwrap());
        assert_eq!(
            manager.cached_access_token(&target).as_deref(),
            Some("shared-new-access")
        );
        assert_eq!(
            store
                .oauth_token_store()
                .load(provider_id)
                .await
                .unwrap()
                .unwrap()
                .credential_version,
            1
        );
    }

    #[tokio::test]
    async fn credential_edit_resets_shared_and_local_access_token_state() {
        let pool = db::open_pool("sqlite::memory:").await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        let store = Arc::new(DbConfigStore::new(pool, None));
        store.refresh().await.unwrap();
        let provider_id = "sqlite-reset-oauth-state";
        let old_meta = serde_json::json!({"refresh_token": "old-refresh"}).to_string();
        store
            .upsert_provider(
                provider_id,
                "SQLite OAuth",
                "openai",
                "https://example.test",
                "",
                None,
                AuthMode::OAuth,
                Some(&old_meta),
                serde_json::json!({
                    "oauth": {
                        "token_url": "https://example.test/token",
                        "client_id": "test-client",
                        "scopes": ["openid"],
                        "token_request_style": "form"
                    }
                }),
                true,
            )
            .await
            .unwrap();
        store
            .oauth_token_store()
            .commit_success(OAuthTokenCommit {
                provider_id,
                access_token: "old-access",
                access_expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
                oauth_meta_plain: &old_meta,
                next_keepalive_at: Utc::now() + chrono::Duration::days(7),
            })
            .await
            .unwrap();
        store.refresh().await.unwrap();
        let provider = store.get_provider(provider_id).await.unwrap().unwrap();
        let oauth = build_oauth_target_config(&provider).unwrap();
        let mut target = make_target(Some(oauth.clone()));
        target.provider_id = provider_id.to_string();
        let cache = Box::leak(Box::new(OAuthTokenCache::new()));
        cache.seed_tokens(
            provider_id,
            oauth.cache_label(),
            "old-access",
            "old-refresh",
            Some(Duration::from_secs(3600)),
        );
        let manager =
            OAuthTokenManager::new_with_cache(Some(store.clone()), reqwest::Client::new(), cache);
        let new_meta = serde_json::json!({"refresh_token": "new-refresh"}).to_string();
        let mutation_store = store.clone();
        let mutation_provider_id = provider_id.to_string();
        let mutation_meta = new_meta.clone();

        manager
            .mutate_credentials_locked(
                provider_id,
                Box::new(move || {
                    Box::pin(async move {
                        mutation_store
                            .set_provider_oauth_meta(&mutation_provider_id, &mutation_meta)
                            .await
                            .map_err(|error| error.to_string())
                    })
                }),
            )
            .await
            .unwrap();

        assert!(manager.cached_access_token(&target).is_none());
        assert!(store
            .oauth_token_store()
            .load(provider_id)
            .await
            .unwrap()
            .is_none());
        let provider = store.get_provider(provider_id).await.unwrap().unwrap();
        let meta: serde_json::Value =
            serde_json::from_str(provider.oauth_meta_cleartext.as_deref().unwrap()).unwrap();
        assert_eq!(meta["refresh_token"], "new-refresh");
    }

    #[tokio::test]
    async fn sqlite_keepalive_and_hot_path_share_one_refresh_grant() {
        let token_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(50))
                    .set_body_json(serde_json::json!({
                        "access_token": "worker-hot-shared-access",
                        "refresh_token": "worker-hot-rotated-refresh",
                        "expires_in": 3600
                    })),
            )
            .expect(1)
            .mount(&token_server)
            .await;

        let pool = db::open_pool("sqlite::memory:").await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        let store = Arc::new(DbConfigStore::new(pool, None));
        store.refresh().await.unwrap();
        let provider_id = "sqlite-worker-hot-path";
        store
            .upsert_provider(
                provider_id,
                "SQLite OAuth",
                "openai",
                "https://example.test",
                "",
                None,
                AuthMode::OAuth,
                Some(&serde_json::json!({"refresh_token": "initial-refresh"}).to_string()),
                serde_json::json!({
                    "oauth": {
                        "token_url": format!("{}/token", token_server.uri()),
                        "client_id": "test-client",
                        "scopes": ["openid"],
                        "token_request_style": "form"
                    }
                }),
                true,
            )
            .await
            .unwrap();
        let provider = store.get_provider(provider_id).await.unwrap().unwrap();
        let mut target = make_target(Some(build_oauth_target_config(&provider).unwrap()));
        target.provider_id = provider_id.to_string();
        let manager = Arc::new(OAuthTokenManager::new(
            Some(store.clone()),
            reqwest::Client::new(),
        ));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let keepalive = {
            let manager = manager.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                manager.try_keepalive_provider(provider_id).await.unwrap()
            })
        };
        let hot_path = {
            let manager = manager.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                let mut headers = HeaderMap::new();
                manager.apply(&target, &mut headers).await.unwrap();
                headers["authorization"].clone()
            })
        };

        let keepalive_outcome = keepalive.await.unwrap();
        assert!(matches!(
            keepalive_outcome,
            OAuthRefreshOutcome::Refreshed | OAuthRefreshOutcome::Skipped
        ));
        assert_eq!(hot_path.await.unwrap(), "Bearer worker-hot-shared-access");
        assert_eq!(
            store
                .oauth_token_store()
                .load(provider_id)
                .await
                .unwrap()
                .unwrap()
                .credential_version,
            1
        );
    }

    #[tokio::test]
    async fn postgres_single_connection_handles_preflight_and_endpoint_failures() {
        let Ok(database_url) = std::env::var("TIYGATE_TEST_PG_URL") else {
            return;
        };
        if database_url.trim().is_empty() {
            return;
        }

        let token_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(500).set_body_string("temporary failure"))
            .expect(1)
            .mount(&token_server)
            .await;
        let pool = db::open_pool_with_max_connections(&database_url, 1)
            .await
            .unwrap();
        db::run_migrations(&pool).await.unwrap();
        let store = Arc::new(DbConfigStore::new(pool, None));
        store.refresh().await.unwrap();
        let provider_id = format!("pg-oauth-failure-{}", uuid::Uuid::now_v7());
        store
            .upsert_provider(
                &provider_id,
                "Postgres OAuth failure",
                "openai",
                "https://example.test",
                "",
                None,
                AuthMode::OAuth,
                None,
                serde_json::json!({
                    "oauth": {
                        "token_url": format!("{}/token", token_server.uri()),
                        "client_id": "test-client",
                        "scopes": ["openid"],
                        "token_request_style": "form"
                    }
                }),
                true,
            )
            .await
            .unwrap();
        let manager = OAuthTokenManager::new(Some(store.clone()), reqwest::Client::new());

        let preflight = tokio::time::timeout(
            Duration::from_secs(5),
            manager.try_keepalive_provider(&provider_id),
        )
        .await
        .expect("preflight backoff must not acquire a second pool slot");
        assert!(preflight.is_err());
        assert!(store
            .oauth_token_store()
            .load(&provider_id)
            .await
            .unwrap()
            .is_some_and(|state| refresh_backoff_is_active(&state)));

        let oauth_meta = serde_json::json!({"refresh_token": "postgres-refresh"}).to_string();
        store
            .set_provider_oauth_meta(&provider_id, &oauth_meta)
            .await
            .unwrap();
        store
            .oauth_token_store()
            .reset(&provider_id, None)
            .await
            .unwrap();
        let refresh =
            tokio::time::timeout(Duration::from_secs(5), manager.force_refresh(&provider_id))
                .await
                .expect("failure persistence must not acquire a second pool slot");
        assert!(refresh.is_err());
        assert!(store
            .oauth_token_store()
            .load(&provider_id)
            .await
            .unwrap()
            .is_some_and(|state| refresh_backoff_is_active(&state)));

        store.delete_provider(&provider_id).await.unwrap();
    }

    #[tokio::test]
    async fn postgres_independent_instances_share_one_refresh_grant() {
        let Ok(database_url) = std::env::var("TIYGATE_TEST_PG_URL") else {
            return;
        };
        if database_url.trim().is_empty() {
            return;
        }

        let token_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .set_body_json(serde_json::json!({
                        "access_token": "postgres-shared-access",
                        "refresh_token": "postgres-rotated-refresh",
                        "expires_in": 3600
                    })),
            )
            .expect(1)
            .mount(&token_server)
            .await;

        let provider_id = format!("pg-oauth-{}", uuid::Uuid::now_v7());
        let pool_a = db::open_pool_with_max_connections(&database_url, 1)
            .await
            .unwrap();
        db::run_migrations(&pool_a).await.unwrap();
        let pool_b = db::open_pool_with_max_connections(&database_url, 1)
            .await
            .unwrap();
        let store_a = Arc::new(DbConfigStore::new(pool_a, None));
        let store_b = Arc::new(DbConfigStore::new(pool_b, None));
        store_a.refresh().await.unwrap();
        store_b.refresh().await.unwrap();
        let oauth_meta =
            serde_json::json!({"refresh_token": "postgres-initial-refresh"}).to_string();
        store_a
            .upsert_provider(
                &provider_id,
                "Postgres OAuth",
                "openai",
                "https://example.test",
                "",
                None,
                AuthMode::OAuth,
                Some(&oauth_meta),
                serde_json::json!({
                    "oauth": {
                        "token_url": format!("{}/token", token_server.uri()),
                        "client_id": "test-client",
                        "scopes": ["openid"],
                        "token_request_style": "form"
                    }
                }),
                true,
            )
            .await
            .unwrap();
        store_b.refresh().await.unwrap();
        let provider = store_a.get_provider(&provider_id).await.unwrap().unwrap();
        let target = RoutingTarget {
            provider_id: provider_id.clone(),
            model_id: "model".to_string(),
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
            oauth: Some(build_oauth_target_config(&provider).unwrap()),
        };
        let cache_a = Box::leak(Box::new(OAuthTokenCache::new()));
        let cache_b = Box::leak(Box::new(OAuthTokenCache::new()));
        let manager_a = Arc::new(OAuthTokenManager::new_with_cache(
            Some(store_a.clone()),
            reqwest::Client::new(),
            cache_a,
        ));
        let manager_b = Arc::new(OAuthTokenManager::new_with_cache(
            Some(store_b.clone()),
            reqwest::Client::new(),
            cache_b,
        ));
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let first = {
            let manager = manager_a.clone();
            let target = target.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                let mut headers = HeaderMap::new();
                manager.apply(&target, &mut headers).await.unwrap();
                headers["authorization"].clone()
            })
        };
        let second = {
            let manager = manager_b.clone();
            let target = target.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                let mut headers = HeaderMap::new();
                manager.apply(&target, &mut headers).await.unwrap();
                headers["authorization"].clone()
            })
        };
        let (first_header, second_header) = tokio::time::timeout(Duration::from_secs(5), async {
            (first.await.unwrap(), second.await.unwrap())
        })
        .await
        .expect("single-connection refresh coordination must not acquire another pool slot");
        assert_eq!(first_header, "Bearer postgres-shared-access");
        assert_eq!(second_header, "Bearer postgres-shared-access");

        store_a.delete_provider(&provider_id).await.unwrap();
    }
}
