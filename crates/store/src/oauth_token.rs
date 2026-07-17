//! Shared OAuth access-token persistence and refresh coordination.
//!
//! PostgreSQL uses transaction-scoped advisory locks so independent gateway
//! instances cannot consume a rotating refresh token concurrently. SQLite is
//! a supported single-process backend and is coordinated by the server's
//! process-local mutex; this module never issues PostgreSQL SQL on SQLite.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::{Any, Row, Transaction};

use crate::config_store::{DbConfigStore, StoreError};
use crate::db::{DbKind, DbPool};
use crate::encryption::KeyEncryption;
use crate::keys;

/// Decrypted shared access-token state. Token values must never be logged.
#[derive(Clone)]
pub struct OAuthAccessTokenState {
    pub provider_id: String,
    pub access_token: String,
    pub access_expires_at: Option<DateTime<Utc>>,
    pub credential_version: i64,
    pub last_refresh_at: Option<DateTime<Utc>>,
    pub next_keepalive_at: Option<DateTime<Utc>>,
    pub next_retry_at: Option<DateTime<Utc>>,
    pub failure_count: i32,
}

impl std::fmt::Debug for OAuthAccessTokenState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthAccessTokenState")
            .field("provider_id", &self.provider_id)
            .field("access_token", &"<redacted>")
            .field("access_expires_at", &self.access_expires_at)
            .field("credential_version", &self.credential_version)
            .field("last_refresh_at", &self.last_refresh_at)
            .field("next_keepalive_at", &self.next_keepalive_at)
            .field("next_retry_at", &self.next_retry_at)
            .field("failure_count", &self.failure_count)
            .finish()
    }
}

/// Values committed atomically after a successful token exchange or refresh.
pub struct OAuthTokenCommit<'a> {
    pub provider_id: &'a str,
    pub access_token: &'a str,
    pub access_expires_at: Option<DateTime<Utc>>,
    pub oauth_meta_plain: &'a str,
    pub next_keepalive_at: DateTime<Utc>,
}

/// Database-backed OAuth token state repository.
#[derive(Clone)]
pub struct OAuthTokenStore {
    pool: DbPool,
    encryption: Option<Arc<KeyEncryption>>,
}

impl OAuthTokenStore {
    pub fn new(pool: DbPool, encryption: Option<Arc<KeyEncryption>>) -> Self {
        Self { pool, encryption }
    }

    pub fn kind(&self) -> DbKind {
        self.pool.kind()
    }

    /// Stable signed 64-bit lock key for one provider credential.
    pub fn advisory_lock_key(provider_id: &str) -> i64 {
        let mut hasher = Sha256::new();
        hasher.update(b"tiygate/oauth-refresh/");
        hasher.update(provider_id.as_bytes());
        let digest = hasher.finalize();
        i64::from_be_bytes([
            digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
        ])
    }

    pub async fn load(
        &self,
        provider_id: &str,
    ) -> Result<Option<OAuthAccessTokenState>, StoreError> {
        let row = sqlx::query(
            "SELECT provider_id, encrypted_access_token, access_expires_at, credential_version, \
                    last_refresh_at, next_keepalive_at, next_retry_at, failure_count \
             FROM oauth_access_tokens WHERE provider_id = $1",
        )
        .bind(provider_id)
        .fetch_optional(self.pool.any())
        .await?;
        row.map(|row| self.row_to_state(row)).transpose()
    }

    pub async fn load_in_transaction(
        &self,
        tx: &mut Transaction<'static, Any>,
        provider_id: &str,
    ) -> Result<Option<OAuthAccessTokenState>, StoreError> {
        let row = sqlx::query(
            "SELECT provider_id, encrypted_access_token, access_expires_at, credential_version, \
                    last_refresh_at, next_keepalive_at, next_retry_at, failure_count \
             FROM oauth_access_tokens WHERE provider_id = $1",
        )
        .bind(provider_id)
        .fetch_optional(&mut **tx)
        .await?;
        row.map(|row| self.row_to_state(row)).transpose()
    }

    /// Begin a PostgreSQL transaction and try to acquire the provider lock.
    /// Returns `None` when another instance currently owns it. SQLite callers
    /// use the process-local coordinator and must not call this method.
    pub async fn try_begin_postgres_refresh(
        &self,
        provider_id: &str,
    ) -> Result<Option<Transaction<'static, Any>>, StoreError> {
        if self.kind() != DbKind::Postgres {
            return Err(StoreError::Invalid(
                "PostgreSQL OAuth advisory lock requested for a non-PostgreSQL database"
                    .to_string(),
            ));
        }
        let mut tx = self.pool.any().begin().await?;
        let row = sqlx::query("SELECT pg_try_advisory_xact_lock($1)")
            .bind(Self::advisory_lock_key(provider_id))
            .fetch_one(&mut *tx)
            .await?;
        let acquired: bool = row.try_get(0)?;
        if acquired {
            Ok(Some(tx))
        } else {
            tx.rollback().await?;
            Ok(None)
        }
    }

    pub async fn commit_success(&self, commit: OAuthTokenCommit<'_>) -> Result<i64, StoreError> {
        let mut tx = self.pool.any().begin().await?;
        let version = self.commit_success_in_transaction(&mut tx, commit).await?;
        tx.commit().await?;
        Ok(version)
    }

    /// Delete shared access-token state, optionally restoring explicitly
    /// submitted OAuth metadata in the same transaction.
    pub async fn reset(
        &self,
        provider_id: &str,
        oauth_meta_plain: Option<&str>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.any().begin().await?;
        self.reset_in_transaction(&mut tx, provider_id, oauth_meta_plain)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn reset_in_transaction(
        &self,
        tx: &mut Transaction<'static, Any>,
        provider_id: &str,
        oauth_meta_plain: Option<&str>,
    ) -> Result<(), StoreError> {
        if let Some(meta) = oauth_meta_plain {
            let encrypted_meta = self.encrypt_oauth_meta(meta)?;
            let result = sqlx::query(
                "UPDATE providers SET encrypted_oauth_meta = $1, updated_at = $2 WHERE id = $3",
            )
            .bind(encrypted_meta)
            .bind(Utc::now().to_rfc3339())
            .bind(provider_id)
            .execute(&mut **tx)
            .await?;
            if result.rows_affected() == 0 {
                return Err(StoreError::NotFound(format!("provider {provider_id}")));
            }
        }
        sqlx::query("DELETE FROM oauth_access_tokens WHERE provider_id = $1")
            .bind(provider_id)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    pub async fn commit_success_in_transaction(
        &self,
        tx: &mut Transaction<'static, Any>,
        commit: OAuthTokenCommit<'_>,
    ) -> Result<i64, StoreError> {
        let encrypted_access = self.encrypt_access_token(commit.access_token)?;
        let encrypted_meta = self.encrypt_oauth_meta(commit.oauth_meta_plain)?;
        let now = Utc::now().to_rfc3339();
        let expires_at = commit.access_expires_at.map(|value| value.to_rfc3339());
        let next_keepalive_at = commit.next_keepalive_at.to_rfc3339();

        let provider_update = sqlx::query(
            "UPDATE providers SET encrypted_oauth_meta = $1, updated_at = $2 WHERE id = $3",
        )
        .bind(&encrypted_meta)
        .bind(&now)
        .bind(commit.provider_id)
        .execute(&mut **tx)
        .await?;
        if provider_update.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!(
                "provider {}",
                commit.provider_id
            )));
        }

        sqlx::query(
            "INSERT INTO oauth_access_tokens \
                (provider_id, encrypted_access_token, access_expires_at, credential_version, \
                 last_refresh_at, next_keepalive_at, next_retry_at, failure_count, updated_at) \
             VALUES ($1, $2, $3, 1, $4, $5, NULL, 0, $4) \
             ON CONFLICT(provider_id) DO UPDATE SET \
                encrypted_access_token = excluded.encrypted_access_token, \
                access_expires_at = excluded.access_expires_at, \
                credential_version = oauth_access_tokens.credential_version + 1, \
                last_refresh_at = excluded.last_refresh_at, \
                next_keepalive_at = excluded.next_keepalive_at, \
                next_retry_at = NULL, failure_count = 0, updated_at = excluded.updated_at",
        )
        .bind(commit.provider_id)
        .bind(&encrypted_access)
        .bind(expires_at)
        .bind(&now)
        .bind(&next_keepalive_at)
        .execute(&mut **tx)
        .await?;

        let row = sqlx::query(
            "SELECT credential_version FROM oauth_access_tokens WHERE provider_id = $1",
        )
        .bind(commit.provider_id)
        .fetch_one(&mut **tx)
        .await?;
        Ok(row.try_get(0)?)
    }

    /// Record a failed refresh and schedule a retry without changing tokens.
    pub async fn record_failure(
        &self,
        provider_id: &str,
        next_retry_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.any().begin().await?;
        self.record_failure_in_transaction(&mut tx, provider_id, next_retry_at)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Record retry state through the caller's advisory-lock transaction.
    /// This avoids acquiring a second pooled connection while the refresh
    /// coordinator already owns one.
    pub async fn record_failure_in_transaction(
        &self,
        tx: &mut Transaction<'static, Any>,
        provider_id: &str,
        next_retry_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO oauth_access_tokens \
                (provider_id, encrypted_access_token, credential_version, next_retry_at, \
                 failure_count, updated_at) \
             VALUES ($1, '', 0, $2, 1, $3) \
             ON CONFLICT(provider_id) DO UPDATE SET \
                next_retry_at = excluded.next_retry_at, \
                failure_count = oauth_access_tokens.failure_count + 1, \
                updated_at = excluded.updated_at",
        )
        .bind(provider_id)
        .bind(next_retry_at.to_rfc3339())
        .bind(now)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// OAuth providers whose keepalive deadline is due. Missing state rows are
    /// included so credentials created by older versions are adopted lazily.
    pub async fn list_due_provider_ids(
        &self,
        now: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query(
            "SELECT p.id FROM providers p \
             LEFT JOIN oauth_access_tokens s ON s.provider_id = p.id \
             WHERE p.enabled = 1 AND p.auth_mode = 'oauth' \
               AND LOWER(p.vendor) <> 'xai' \
               AND (s.provider_id IS NULL OR s.next_keepalive_at IS NULL OR s.next_keepalive_at <= $1) \
               AND (s.next_retry_at IS NULL OR s.next_retry_at <= $1) \
             ORDER BY COALESCE(s.next_keepalive_at, p.updated_at) ASC LIMIT $2",
        )
        .bind(now.to_rfc3339())
        .bind(limit.max(1))
        .fetch_all(self.pool.any())
        .await?;
        rows.into_iter()
            .map(|row| row.try_get::<String, _>(0).map_err(StoreError::from))
            .collect()
    }

    fn row_to_state(&self, row: sqlx::any::AnyRow) -> Result<OAuthAccessTokenState, StoreError> {
        let encrypted: String = row.try_get("encrypted_access_token")?;
        Ok(OAuthAccessTokenState {
            provider_id: row.try_get("provider_id")?,
            access_token: self.decrypt_access_token(&encrypted)?,
            access_expires_at: parse_optional_timestamp(row.try_get("access_expires_at")?)?,
            credential_version: row.try_get("credential_version")?,
            last_refresh_at: parse_optional_timestamp(row.try_get("last_refresh_at")?)?,
            next_keepalive_at: parse_optional_timestamp(row.try_get("next_keepalive_at")?)?,
            next_retry_at: parse_optional_timestamp(row.try_get("next_retry_at")?)?,
            failure_count: row.try_get("failure_count")?,
        })
    }

    fn encrypt_access_token(&self, value: &str) -> Result<String, StoreError> {
        match self.encryption.as_ref() {
            Some(enc) => keys::encrypt_oauth_access_token(enc, value)
                .map_err(|error| StoreError::Decrypt(error.to_string())),
            None => Ok(value.to_string()),
        }
    }

    fn decrypt_access_token(&self, value: &str) -> Result<String, StoreError> {
        if value.is_empty() {
            return Ok(String::new());
        }
        match self.encryption.as_ref() {
            Some(enc) => keys::decrypt_oauth_access_token(enc, value)
                .map_err(|error| StoreError::Decrypt(error.to_string())),
            None => Ok(value.to_string()),
        }
    }

    fn encrypt_oauth_meta(&self, value: &str) -> Result<String, StoreError> {
        match self.encryption.as_ref() {
            Some(enc) => keys::encrypt_oauth_meta(enc, value)
                .map_err(|error| StoreError::Decrypt(error.to_string())),
            None => Ok(value.to_string()),
        }
    }
}

impl DbConfigStore {
    /// Build a shared token-state repository using the same pool and master key.
    pub fn oauth_token_store(&self) -> OAuthTokenStore {
        OAuthTokenStore::new(self.pool.clone(), self.encryption.clone())
    }
}

fn parse_optional_timestamp(value: Option<String>) -> Result<Option<DateTime<Utc>>, StoreError> {
    value
        .map(|raw| {
            DateTime::parse_from_rfc3339(&raw)
                .map(|value| value.with_timezone(&Utc))
                .map_err(|error| StoreError::Invalid(format!("invalid OAuth timestamp: {error}")))
        })
        .transpose()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::db;
    use crate::encryption::KeyEncryption;

    #[test]
    fn advisory_key_is_stable_and_namespaced() {
        assert_eq!(
            OAuthTokenStore::advisory_lock_key("provider-a"),
            OAuthTokenStore::advisory_lock_key("provider-a")
        );
        assert_ne!(
            OAuthTokenStore::advisory_lock_key("provider-a"),
            OAuthTokenStore::advisory_lock_key("provider-b")
        );
    }

    #[tokio::test]
    async fn sqlite_state_round_trip_and_never_uses_pg_lock() {
        let pool = db::open_pool("sqlite::memory:").await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        let store = DbConfigStore::new(pool, None);
        store.refresh().await.unwrap();
        store
            .upsert_provider(
                "oauth-provider",
                "OAuth",
                "openai",
                "https://example.test",
                "",
                None,
                crate::models::AuthMode::OAuth,
                Some("{\"refresh_token\":\"rt\"}"),
                serde_json::json!({"oauth": {}}),
                true,
            )
            .await
            .unwrap();
        let tokens = store.oauth_token_store();
        assert_eq!(tokens.kind(), DbKind::Sqlite);
        let now = Utc::now();
        let version = tokens
            .commit_success(OAuthTokenCommit {
                provider_id: "oauth-provider",
                access_token: "at-secret",
                access_expires_at: Some(now + chrono::Duration::hours(1)),
                oauth_meta_plain: "{\"refresh_token\":\"rt\"}",
                next_keepalive_at: now + chrono::Duration::days(7),
            })
            .await
            .unwrap();
        assert_eq!(version, 1);
        let loaded = tokens.load("oauth-provider").await.unwrap().unwrap();
        assert_eq!(loaded.access_token, "at-secret");
        assert!(tokens
            .try_begin_postgres_refresh("oauth-provider")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn access_token_is_encrypted_with_its_own_purpose() {
        let pool = db::open_pool("sqlite::memory:").await.unwrap();
        db::run_migrations(&pool).await.unwrap();
        let encryption = Arc::new(KeyEncryption::from_bytes([7_u8; 32]));
        let store = DbConfigStore::new(pool, Some(encryption.clone()));
        store.refresh().await.unwrap();
        store
            .upsert_provider(
                "encrypted-oauth-provider",
                "OAuth",
                "openai",
                "https://example.test",
                "",
                None,
                crate::models::AuthMode::OAuth,
                Some("{\"refresh_token\":\"rt-secret\"}"),
                serde_json::json!({"oauth": {}}),
                true,
            )
            .await
            .unwrap();
        let tokens = store.oauth_token_store();
        tokens
            .commit_success(OAuthTokenCommit {
                provider_id: "encrypted-oauth-provider",
                access_token: "at-secret",
                access_expires_at: Some(Utc::now() + chrono::Duration::hours(1)),
                oauth_meta_plain: "{\"refresh_token\":\"rt-secret\"}",
                next_keepalive_at: Utc::now() + chrono::Duration::days(7),
            })
            .await
            .unwrap();

        let row = sqlx::query(
            "SELECT encrypted_access_token FROM oauth_access_tokens WHERE provider_id = $1",
        )
        .bind("encrypted-oauth-provider")
        .fetch_one(tokens.pool.any())
        .await
        .unwrap();
        let ciphertext: String = row.try_get(0).unwrap();
        assert_ne!(ciphertext, "at-secret");
        assert!(!ciphertext.contains("at-secret"));
        assert_eq!(
            keys::decrypt_oauth_access_token(&encryption, &ciphertext).unwrap(),
            "at-secret"
        );
        assert!(keys::decrypt_oauth_meta(&encryption, &ciphertext).is_err());
    }
}
