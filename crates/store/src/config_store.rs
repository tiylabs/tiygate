//! Config store — both the legacy in-memory `ConfigStore` (kept as
//! a thin compatibility shim) and the DB-backed `DbConfigStore` that
//! powers Phase 4 (产品化).
//!
//! The DB-backed store is the production source of truth. The
//! in-memory store is used by the unit tests in the data plane and
//! as the default when no `TIYGATE_DATABASE_URL` is configured —
//! this preserves the Phase 1-3 ergonomics (`ConfigStore::from_env()`)
//! while letting operators opt into the full control plane by
//! setting the env var.

use std::sync::Arc;

use parking_lot::RwLock;
use sqlx::Row;
use thiserror::Error;
use tracing::{debug, warn};
use uuid::Uuid;

use tiygate_core::protocol::{ProtocolEndpoint, ProtocolSuite};
use tiygate_core::routing::{RouteEntry, RoutingTable, RoutingTarget};

use crate::db::DbPool;
use crate::encryption::KeyEncryption;
use crate::keys;
use crate::models::{
    ApiKey, ApiKeyStatus, AuthMode, ConfigEpoch, ConfigSnapshot, Provider, Route, RouteTarget,
};

/// Convenience error for store operations.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("store database error: {0}")]
    DbLayer(#[from] crate::db::DbError),
    #[error("decryption error: {0}")]
    Decrypt(String),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
}

// ---------------------------------------------------------------------
// Legacy in-memory store
// ---------------------------------------------------------------------

/// In-memory `ConfigStore` used by Phase 1-3 callers.
///
/// Phase 4 introduces a DB-backed store; this struct remains so the
/// data plane's existing call sites (`state.config.routing_table`)
/// keep working without churn.
#[derive(Clone)]
pub struct ConfigStore {
    pub routing_table: RoutingTable,
    /// Optional snapshot, populated when a `DbConfigStore` produces
    /// one. `None` for the legacy in-memory store.
    snapshot: Option<Arc<RwLock<ConfigSnapshot>>>,
}

impl Default for ConfigStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigStore {
    pub fn new() -> Self {
        Self {
            routing_table: RoutingTable::new(),
            snapshot: None,
        }
    }

    /// Build a default routing table from environment variables.
    /// Detects `OPENAI_API_KEY` and `ANTHROPIC_API_KEY` and inserts
    /// the corresponding routes — same behaviour as Phase 1-3.
    pub fn from_env() -> Self {
        let mut store = Self::new();
        let mut table = RoutingTable::new();

        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            let openai_targets = vec![RoutingTarget {
                provider_id: "openai".to_string(),
                model_id: "gpt-4o".to_string(),
                api_base: "https://api.openai.com/v1".to_string(),
                api_key: key.clone(),
                api_protocol: ProtocolEndpoint::new(
                    ProtocolSuite::OpenAiCompatible,
                    "chat-completions",
                    "v1",
                ),
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                weight: 1.0,
            }];

            table.insert("gpt-4o".to_string(), openai_targets.clone());
            table.insert("gpt-4o-mini".to_string(), {
                let mut t = openai_targets.clone();
                t[0].model_id = "gpt-4o-mini".to_string();
                t
            });
            table.insert("gpt-3.5-turbo".to_string(), {
                let mut t = openai_targets;
                t[0].model_id = "gpt-3.5-turbo".to_string();
                t
            });
        }

        if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
            let anthropic_targets = vec![RoutingTarget {
                provider_id: "anthropic".to_string(),
                model_id: "claude-sonnet-4-20250514".to_string(),
                api_base: "https://api.anthropic.com/v1".to_string(),
                api_key: key.clone(),
                api_protocol: ProtocolEndpoint::new(
                    ProtocolSuite::AnthropicMessages,
                    "messages",
                    "2023-06-01",
                ),
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                weight: 1.0,
            }];
            table.insert("claude-sonnet-4-20250514".to_string(), anthropic_targets);
        }

        store.routing_table = table;
        store
    }

    /// Build a `ConfigStore` with a custom routing table (test helper).
    pub fn with_routing_table(routing_table: RoutingTable) -> Self {
        Self {
            routing_table,
            snapshot: None,
        }
    }

    /// Build a `ConfigStore` from an explicit `ConfigSnapshot`.
    /// Used by the DB-backed store when applying a fresh snapshot.
    pub fn from_snapshot(snapshot: ConfigSnapshot) -> Self {
        let routing_table = snapshot_to_routing_table(&snapshot);
        Self {
            routing_table,
            snapshot: Some(Arc::new(RwLock::new(snapshot))),
        }
    }

    /// Returns the current config snapshot, if any.
    pub fn snapshot(&self) -> Option<ConfigSnapshot> {
        self.snapshot.as_ref().map(|s| s.read().clone())
    }

    /// Look up an API key by its cleartext secret. The legacy
    /// in-memory `ConfigStore` (no DB) returns `Ok(None)` so the
    /// caller treats the request as anonymous — this preserves the
    /// "fail open" principle from §4.6. The DB-backed
    /// `DbConfigStore` overrides this behaviour by holding its own
    /// pool; the data plane typically receives a snapshot-derived
    /// `ConfigStore` and so falls through to the no-op path.
    pub async fn find_api_key_by_secret(
        &self,
        _secret: &str,
    ) -> Result<Option<ApiKey>, StoreError> {
        Ok(None)
    }
}

/// Convert a `ConfigSnapshot` (DB representation) into the data
/// plane's `RoutingTable`. Decryption happens here.
pub fn snapshot_to_routing_table(snapshot: &ConfigSnapshot) -> RoutingTable {
    let mut table = RoutingTable::new();
    for (virtual_model, route) in &snapshot.routes {
        if !route.enabled {
            continue;
        }
        let mut targets = Vec::with_capacity(route.targets.len());
        for t in &route.targets {
            let provider = match snapshot.providers.get(&t.provider_id) {
                Some(p) if p.enabled => p,
                _ => {
                    debug!(
                        provider = %t.provider_id,
                        virtual_model = %virtual_model,
                        "route target skipped: provider disabled or missing"
                    );
                    continue;
                }
            };
            // Decrypt the API key just-in-time. The cleartext is
            // stored on the RoutingTarget for the duration of the
            // request; ingress hands the target off to the upstream
            // call without re-reading the snapshot.
            let api_key = match provider.auth_mode {
                AuthMode::ApiKey => {
                    if provider.encrypted_api_key.is_empty() {
                        String::new()
                    } else {
                        // Prefer the cleartext populated by
                        // `DbConfigStore::refresh()`. If absent
                        // (e.g. the snapshot was loaded through a
                        // path that did not run refresh, or the
                        // provider was hand-built in a test), fall
                        // back to the legacy `metadata_json`
                        // carrier so the snapshot remains
                        // self-contained.
                        provider
                            .api_key_cleartext
                            .clone()
                            .or_else(|| {
                                provider
                                    .metadata_json
                                    .get("__decrypted_api_key")
                                    .and_then(|v| v.as_str())
                                    .map(str::to_string)
                            })
                            .unwrap_or_default()
                    }
                }
                AuthMode::None => String::new(),
                // OAuth / IAM auth is enforced at the request layer,
                // not by reading an api_key column. Pass through.
                AuthMode::OAuth | AuthMode::Iam => String::new(),
            };
            let api_base = t
                .api_base_override
                .clone()
                .unwrap_or_else(|| provider.api_base.clone());
            targets.push(RoutingTarget {
                provider_id: provider.id.clone(),
                model_id: t.model_id.clone(),
                api_base,
                api_key,
                api_protocol: vendor_to_suite(&provider.vendor).default_endpoint(),
                account_label: t.account_label.clone(),
                api_key_override: t.api_key_override.clone(),
                api_base_override: t.api_base_override.clone(),
                weight: t.weight,
            });
        }
        if !targets.is_empty() {
            table.insert_entry(
                virtual_model.clone(),
                RouteEntry {
                    targets,
                    strategy: route.routing_strategy,
                },
            );
        }
    }
    table
}

fn vendor_to_suite(vendor: &str) -> ProtocolSuite {
    match vendor {
        "anthropic" => ProtocolSuite::AnthropicMessages,
        "google" | "gemini" => ProtocolSuite::GoogleGemini,
        "bedrock" => ProtocolSuite::OpenAiCompatible, // Bedrock uses Converse; mapped by executor
        _ => ProtocolSuite::OpenAiCompatible,
    }
}

// ---------------------------------------------------------------------
// DB-backed store
// ---------------------------------------------------------------------

/// DB-backed configuration store. Owns the DB pool and the master
/// encryption key; the data plane sees a `ConfigStore` rebuilt from
/// [`Self::snapshot`] on every epoch tick.
pub struct DbConfigStore {
    pool: DbPool,
    encryption: Option<Arc<KeyEncryption>>,
    /// In-memory copy of the latest snapshot, used by readers that
    /// want a `ConfigStore` view. Held in an `ArcSwap` so the data
    /// plane can read the latest snapshot lock-free (a single
    /// atomic pointer load) and the epoch-poll task can publish a
    /// new snapshot without blocking readers.
    inner: arc_swap::ArcSwap<ConfigStore>,
}

impl DbConfigStore {
    pub fn new(pool: DbPool, encryption: Option<Arc<KeyEncryption>>) -> Self {
        let inner = arc_swap::ArcSwap::from_pointee(ConfigStore::new());
        Self {
            pool,
            encryption,
            inner,
        }
    }

    /// Open from a `database_url` and load (or initialise) the
    /// current snapshot. Runs migrations.
    pub async fn open(
        database_url: &str,
        encryption: Option<Arc<KeyEncryption>>,
    ) -> Result<Arc<Self>, StoreError> {
        let pool = crate::db::open_pool(database_url).await?;
        crate::db::run_migrations(pool.sqlite()).await?;
        let store = Arc::new(Self::new(pool, encryption));
        store.refresh().await?;
        Ok(store)
    }

    /// Returns a clone of the current `ConfigStore` view. The data
    /// plane uses this in `App::new()`.
    pub fn config_store(&self) -> ConfigStore {
        (*self.inner.load_full()).clone()
    }

    /// Returns the latest config snapshot as a shared `Arc`. This is
    /// a lock-free atomic pointer load — the data plane calls this on
    /// every request to read the most recent routing table published
    /// by the epoch-poll task, without cloning the underlying
    /// `HashMap`.
    pub fn snapshot(&self) -> Arc<ConfigStore> {
        self.inner.load_full()
    }

    /// Returns the current config epoch (cheap DB read).
    pub async fn current_epoch(&self) -> Result<i64, StoreError> {
        let row = sqlx::query("SELECT epoch FROM config_epoch WHERE id = 1")
            .fetch_optional(self.pool.sqlite())
            .await?;
        Ok(row.map(|r| r.get::<i64, _>(0)).unwrap_or(0))
    }

    /// Re-read providers + routes from the DB and update the
    /// in-memory snapshot. Increments the epoch.
    pub async fn refresh(&self) -> Result<(), StoreError> {
        let mut providers = self.load_providers().await?;
        // Decrypt the cleartext API key for each provider so the
        // data plane can forward credentials without re-running
        // the crypto on every request. Providers with no master
        // key configured fall back to the column-as-cleartext
        // mode (encryption never happened in that path).
        if let Some(enc) = self.encryption.as_ref() {
            for provider in providers.iter_mut() {
                if provider.encrypted_api_key.is_empty() {
                    provider.api_key_cleartext = Some(String::new());
                    continue;
                }
                match keys::decrypt_api_key(enc, &provider.encrypted_api_key) {
                    Ok(plain) => provider.api_key_cleartext = Some(plain),
                    Err(e) => {
                        // Log and continue; the data plane will
                        // see a None cleartext and skip the auth
                        // header. Failing the entire refresh here
                        // would block every provider on a single
                        // bad row.
                        tracing::warn!(
                            provider = %provider.id,
                            error = %e,
                            "decrypting provider api key failed; data plane will skip the auth header"
                        );
                        provider.api_key_cleartext = None;
                    }
                }
            }
        } else {
            for provider in providers.iter_mut() {
                if provider.encrypted_api_key.is_empty() {
                    provider.api_key_cleartext = Some(String::new());
                } else {
                    // No master key: the column already holds the
                    // cleartext (see `upsert_provider`).
                    provider.api_key_cleartext = Some(provider.encrypted_api_key.clone());
                }
            }
        }
        let routes = self.load_routes().await?;
        let epoch = self.bump_epoch().await?;
        let snapshot = ConfigSnapshot {
            epoch,
            providers: providers.into_iter().map(|p| (p.id.clone(), p)).collect(),
            routes: routes
                .into_iter()
                .map(|r| (r.virtual_model.clone(), r))
                .collect(),
        };
        let store = ConfigStore::from_snapshot(snapshot);
        self.inner.store(Arc::new(store));
        debug!(epoch, "config snapshot refreshed");
        Ok(())
    }

    async fn bump_epoch(&self) -> Result<i64, StoreError> {
        // Atomic upsert: insert or update the single-row table.
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO config_epoch (id, epoch, updated_at) VALUES (1, 1, ?1) \
             ON CONFLICT(id) DO UPDATE SET epoch = epoch + 1, updated_at = excluded.updated_at",
        )
        .bind(now)
        .execute(self.pool.sqlite())
        .await?;
        let row = sqlx::query("SELECT epoch FROM config_epoch WHERE id = 1")
            .fetch_one(self.pool.sqlite())
            .await?;
        Ok(row.get::<i64, _>(0))
    }

    // --- Provider CRUD ---

    pub async fn list_providers(&self) -> Result<Vec<Provider>, StoreError> {
        self.load_providers().await
    }

    pub async fn get_provider(&self, id: &str) -> Result<Option<Provider>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, name, vendor, api_base, encrypted_api_key, auth_mode, \
                    encrypted_oauth_meta, metadata_json, enabled, \
                    created_at, updated_at FROM providers WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.pool.sqlite())
        .await?;
        rows.map(row_to_provider).transpose()
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_provider(
        &self,
        id: &str,
        name: &str,
        vendor: &str,
        api_base: &str,
        api_key_plain: Option<&str>,
        auth_mode: AuthMode,
        oauth_meta_plain: Option<&str>,
        metadata_json: serde_json::Value,
        enabled: bool,
    ) -> Result<Provider, StoreError> {
        let now = chrono::Utc::now().to_rfc3339();
        let existing = self.get_provider(id).await?;
        let encrypted_api_key = match (api_key_plain, self.encryption.as_ref()) {
            (Some(plain), Some(enc)) => {
                keys::encrypt_api_key(enc, plain).map_err(|e| StoreError::Decrypt(e.to_string()))?
            }
            (Some(plain), None) => {
                warn!(
                    "TIYGATE_MASTER_KEY not set; storing API key in cleartext (NOT FOR PRODUCTION)"
                );
                plain.to_string()
            }
            (None, _) => existing
                .as_ref()
                .map(|p| p.encrypted_api_key.clone())
                .unwrap_or_default(),
        };
        let encrypted_oauth_meta = match (oauth_meta_plain, self.encryption.as_ref()) {
            (Some(plain), Some(enc)) => keys::encrypt_oauth_meta(enc, plain)
                .map_err(|e| StoreError::Decrypt(e.to_string()))?,
            (Some(plain), None) => plain.to_string(),
            (None, _) => existing
                .as_ref()
                .map(|p| p.encrypted_oauth_meta.clone())
                .unwrap_or_default(),
        };
        let metadata_str = serde_json::to_string(&metadata_json)?;
        let enabled_int: i32 = if enabled { 1 } else { 0 };
        let created_at = existing
            .as_ref()
            .map(|p| p.created_at.to_rfc3339())
            .unwrap_or_else(|| now.clone());

        sqlx::query(
            "INSERT INTO providers (id, name, vendor, api_base, encrypted_api_key, auth_mode, \
             encrypted_oauth_meta, metadata_json, enabled, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
             ON CONFLICT(id) DO UPDATE SET \
                name=excluded.name, vendor=excluded.vendor, api_base=excluded.api_base, \
                encrypted_api_key=excluded.encrypted_api_key, auth_mode=excluded.auth_mode, \
                encrypted_oauth_meta=excluded.encrypted_oauth_meta, metadata_json=excluded.metadata_json, \
                enabled=excluded.enabled, updated_at=excluded.updated_at",
        )
        .bind(id)
        .bind(name)
        .bind(vendor)
        .bind(api_base)
        .bind(&encrypted_api_key)
        .bind(auth_mode.as_str())
        .bind(&encrypted_oauth_meta)
        .bind(&metadata_str)
        .bind(enabled_int)
        .bind(&created_at)
        .bind(&now)
        .execute(self.pool.sqlite())
        .await?;

        self.refresh().await?;
        self.get_provider(id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("provider {id} disappeared post-upsert")))
    }

    pub async fn delete_provider(&self, id: &str) -> Result<(), StoreError> {
        let res = sqlx::query("DELETE FROM providers WHERE id = ?1")
            .bind(id)
            .execute(self.pool.sqlite())
            .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("provider {id}")));
        }
        self.refresh().await?;
        Ok(())
    }

    /// Update only the `encrypted_oauth_meta` column for an
    /// existing provider. Used by the OAuth callback handler to
    /// persist the refresh-token metadata after a successful
    /// authorization-code → token exchange. The plain-text
    /// `meta` argument is encrypted at rest by the
    /// `KeyEncryption` configured on the store; the encrypted
    /// blob replaces the existing column value.
    ///
    /// Returns `Ok(())` when the update lands. Returns
    /// `Err(StoreError::NotFound)` when no row matched the
    /// `id`, so the admin handler can surface a 404 to the
    /// operator instead of silently no-oping.
    pub async fn set_provider_oauth_meta(
        &self,
        id: &str,
        meta_plain: &str,
    ) -> Result<(), StoreError> {
        // Encrypt the meta blob with the OAuth-purpose subkey
        // so the API-key subkey cannot decrypt it (defence in
        // depth). When encryption is not configured (legacy /
        // test path), fall back to storing the cleartext — the
        // production gate requires encryption.
        let encrypted = match self.encryption.as_ref() {
            Some(enc) => keys::encrypt_oauth_meta(enc, meta_plain)
                .map_err(|e| StoreError::Decrypt(e.to_string()))?,
            None => meta_plain.to_string(),
        };
        let now = chrono::Utc::now().to_rfc3339();
        let res = sqlx::query(
            "UPDATE providers SET encrypted_oauth_meta = ?1, updated_at = ?2 WHERE id = ?3",
        )
        .bind(&encrypted)
        .bind(&now)
        .bind(id)
        .execute(self.pool.sqlite())
        .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("provider {id}")));
        }
        // Refresh the in-memory snapshot so subsequent reads
        // from the data plane see the new metadata.
        self.refresh().await?;
        Ok(())
    }

    async fn load_providers(&self) -> Result<Vec<Provider>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, name, vendor, api_base, encrypted_api_key, auth_mode, \
                    encrypted_oauth_meta, metadata_json, enabled, \
                    created_at, updated_at FROM providers",
        )
        .fetch_all(self.pool.sqlite())
        .await?;
        rows.into_iter().map(row_to_provider).collect()
    }

    // --- Route CRUD ---

    pub async fn list_routes(&self) -> Result<Vec<Route>, StoreError> {
        self.load_routes().await
    }

    pub async fn get_route(&self, id: &str) -> Result<Option<Route>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, virtual_model, targets_json, routing_strategy, enabled, created_at, updated_at \
             FROM routes WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.pool.sqlite())
        .await?;
        rows.map(row_to_route).transpose()
    }

    pub async fn upsert_route(
        &self,
        id: &str,
        virtual_model: &str,
        targets: &[RouteTarget],
        routing_strategy: Option<tiygate_core::routing::RoutingStrategyName>,
        enabled: bool,
    ) -> Result<Route, StoreError> {
        if targets.is_empty() {
            return Err(StoreError::Invalid(
                "route must have at least one target".into(),
            ));
        }
        let now = chrono::Utc::now().to_rfc3339();
        let existing = self.get_route(id).await?;
        let created_at = existing
            .as_ref()
            .map(|r| r.created_at.to_rfc3339())
            .unwrap_or_else(|| now.clone());
        let targets_json = serde_json::to_string(targets)?;
        let strategy_str = routing_strategy.map(|s| s.as_str());
        let enabled_int: i32 = if enabled { 1 } else { 0 };

        sqlx::query(
            "INSERT INTO routes (id, virtual_model, targets_json, routing_strategy, enabled, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(id) DO UPDATE SET \
                virtual_model=excluded.virtual_model, targets_json=excluded.targets_json, \
                routing_strategy=excluded.routing_strategy, \
                enabled=excluded.enabled, updated_at=excluded.updated_at",
        )
        .bind(id)
        .bind(virtual_model)
        .bind(&targets_json)
        .bind(strategy_str)
        .bind(enabled_int)
        .bind(&created_at)
        .bind(&now)
        .execute(self.pool.sqlite())
        .await?;

        self.refresh().await?;
        self.get_route(id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("route {id} disappeared post-upsert")))
    }

    pub async fn delete_route(&self, id: &str) -> Result<(), StoreError> {
        let res = sqlx::query("DELETE FROM routes WHERE id = ?1")
            .bind(id)
            .execute(self.pool.sqlite())
            .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("route {id}")));
        }
        self.refresh().await?;
        Ok(())
    }

    async fn load_routes(&self) -> Result<Vec<Route>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, virtual_model, targets_json, routing_strategy, enabled, created_at, updated_at \
             FROM routes",
        )
        .fetch_all(self.pool.sqlite())
        .await?;
        rows.into_iter().map(row_to_route).collect()
    }

    // --- API key CRUD ---

    pub async fn list_api_keys(&self) -> Result<Vec<ApiKey>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, name, key_hash, quota_json, status, created_at, updated_at \
             FROM api_keys",
        )
        .fetch_all(self.pool.sqlite())
        .await?;
        rows.into_iter().map(row_to_api_key).collect()
    }

    pub async fn create_api_key(
        &self,
        name: &str,
        secret_plain: &str,
        quota: serde_json::Value,
    ) -> Result<(ApiKey, String), StoreError> {
        let id = Uuid::now_v7().to_string();
        let key_hash = hash_api_key(secret_plain);
        let now = chrono::Utc::now().to_rfc3339();
        let quota_str = serde_json::to_string(&quota)?;
        sqlx::query(
            "INSERT INTO api_keys (id, name, key_hash, quota_json, status, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6)",
        )
        .bind(&id)
        .bind(name)
        .bind(&key_hash)
        .bind(&quota_str)
        .bind(&now)
        .bind(&now)
        .execute(self.pool.sqlite())
        .await?;
        let key = self
            .get_api_key(&id)
            .await?
            .ok_or_else(|| StoreError::NotFound("just-created api key".into()))?;
        Ok((key, secret_plain.to_string()))
    }

    pub async fn get_api_key(&self, id: &str) -> Result<Option<ApiKey>, StoreError> {
        let row = sqlx::query(
            "SELECT id, name, key_hash, quota_json, status, created_at, updated_at \
             FROM api_keys WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.pool.sqlite())
        .await?;
        row.map(row_to_api_key).transpose()
    }

    pub async fn find_api_key_by_secret(&self, secret: &str) -> Result<Option<ApiKey>, StoreError> {
        let key_hash = hash_api_key(secret);
        let row = sqlx::query(
            "SELECT id, name, key_hash, quota_json, status, created_at, updated_at \
             FROM api_keys WHERE key_hash = ?1",
        )
        .bind(&key_hash)
        .fetch_optional(self.pool.sqlite())
        .await?;
        row.map(row_to_api_key).transpose()
    }

    /// Update the quota JSON of an API key in place. Unlike
    /// [`Self::disable_api_key`] this does not touch the `status`
    /// column, so the PATCH quota endpoint and the PUT disable
    /// endpoint stay semantically distinct.
    pub async fn update_api_key_quota(
        &self,
        id: &str,
        quota: serde_json::Value,
    ) -> Result<ApiKey, StoreError> {
        let now = chrono::Utc::now().to_rfc3339();
        let quota_str = serde_json::to_string(&quota)?;
        let res = sqlx::query("UPDATE api_keys SET quota_json = ?1, updated_at = ?2 WHERE id = ?3")
            .bind(&quota_str)
            .bind(&now)
            .bind(id)
            .execute(self.pool.sqlite())
            .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("api key {id}")));
        }
        self.get_api_key(id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("api key {id}")))
    }

    pub async fn disable_api_key(&self, id: &str) -> Result<(), StoreError> {
        let now = chrono::Utc::now().to_rfc3339();
        let res =
            sqlx::query("UPDATE api_keys SET status = 'disabled', updated_at = ?1 WHERE id = ?2")
                .bind(&now)
                .bind(id)
                .execute(self.pool.sqlite())
                .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("api key {id}")));
        }
        Ok(())
    }

    pub async fn delete_api_key(&self, id: &str) -> Result<(), StoreError> {
        let res = sqlx::query("DELETE FROM api_keys WHERE id = ?1")
            .bind(id)
            .execute(self.pool.sqlite())
            .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("api key {id}")));
        }
        Ok(())
    }

    // --- Settings ---

    pub async fn get_setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        let row = sqlx::query("SELECT value FROM settings WHERE key = ?1")
            .bind(key)
            .fetch_optional(self.pool.sqlite())
            .await?;
        Ok(row.map(|r| r.get::<String, _>(0)))
    }

    pub async fn set_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(&now)
        .execute(self.pool.sqlite())
        .await?;
        Ok(())
    }

    // --- ConfigEpoch ---

    pub async fn get_epoch(&self) -> Result<ConfigEpoch, StoreError> {
        let row = sqlx::query("SELECT epoch, updated_at FROM config_epoch WHERE id = 1")
            .fetch_optional(self.pool.sqlite())
            .await?;
        match row {
            Some(r) => Ok(ConfigEpoch {
                epoch: r.get::<i64, _>(0),
                updated_at: chrono::DateTime::parse_from_rfc3339(&r.get::<String, _>(1))
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now()),
            }),
            None => Ok(ConfigEpoch::default()),
        }
    }
}

// ---------- row → model helpers ----------

fn row_to_provider(row: sqlx::sqlite::SqliteRow) -> Result<Provider, StoreError> {
    let auth_mode_str: String = row.get("auth_mode");
    let auth_mode = AuthMode::parse(&auth_mode_str)
        .ok_or_else(|| StoreError::Invalid(format!("unknown auth_mode: {auth_mode_str}")))?;
    let metadata_str: String = row.get("metadata_json");
    let metadata_json: serde_json::Value = if metadata_str.is_empty() {
        serde_json::Value::Object(Default::default())
    } else {
        serde_json::from_str(&metadata_str)?
    };
    let enabled_int: i32 = row.get("enabled");
    Ok(Provider {
        id: row.get("id"),
        name: row.get("name"),
        vendor: row.get("vendor"),
        api_base: row.get("api_base"),
        encrypted_api_key: row.get("encrypted_api_key"),
        auth_mode,
        encrypted_oauth_meta: row.get("encrypted_oauth_meta"),
        metadata_json,
        enabled: enabled_int != 0,
        created_at: parse_dt(row.get("created_at"))?,
        updated_at: parse_dt(row.get("updated_at"))?,
        api_key_cleartext: None,
    })
}

fn row_to_route(row: sqlx::sqlite::SqliteRow) -> Result<Route, StoreError> {
    let targets_str: String = row.get("targets_json");
    let targets: Vec<RouteTarget> = serde_json::from_str(&targets_str)?;
    let enabled_int: i32 = row.get("enabled");
    // `routing_strategy` is a nullable TEXT column. Unknown / unparsable
    // tokens are treated as `None` (inherit the gateway default) rather
    // than failing the whole snapshot load.
    let routing_strategy: Option<String> = row.get("routing_strategy");
    let routing_strategy = routing_strategy
        .as_deref()
        .and_then(tiygate_core::routing::RoutingStrategyName::parse);
    Ok(Route {
        id: row.get("id"),
        virtual_model: row.get("virtual_model"),
        targets,
        routing_strategy,
        enabled: enabled_int != 0,
        created_at: parse_dt(row.get("created_at"))?,
        updated_at: parse_dt(row.get("updated_at"))?,
    })
}

fn row_to_api_key(row: sqlx::sqlite::SqliteRow) -> Result<ApiKey, StoreError> {
    let status_str: String = row.get("status");
    let status = ApiKeyStatus::parse(&status_str)
        .ok_or_else(|| StoreError::Invalid(format!("unknown api key status: {status_str}")))?;
    let quota_str: String = row.get("quota_json");
    let quota_json: serde_json::Value = if quota_str.is_empty() {
        serde_json::Value::Object(Default::default())
    } else {
        serde_json::from_str(&quota_str)?
    };
    Ok(ApiKey {
        id: row.get("id"),
        name: row.get("name"),
        key_hash: row.get("key_hash"),
        quota_json,
        status,
        created_at: parse_dt(row.get("created_at"))?,
        updated_at: parse_dt(row.get("updated_at"))?,
    })
}

fn parse_dt(s: String) -> Result<chrono::DateTime<chrono::Utc>, StoreError> {
    chrono::DateTime::parse_from_rfc3339(&s)
        .map(|d| d.with_timezone(&chrono::Utc))
        .map_err(|e| StoreError::Invalid(format!("bad datetime '{s}': {e}")))
}

/// SHA-256 hex digest of an API key. Used for storage — the cleartext
/// is returned to the caller exactly once.
pub fn hash_api_key(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let digest = hasher.finalize();
    hex::encode(digest)
}

// Keep some imports that the migration runner uses later.
#[allow(unused_imports)]
use std::collections::HashMap as _HashMapImport;
#[allow(unused_imports)]
use tracing::info as _info;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_api_key_is_deterministic() {
        assert_eq!(hash_api_key("a"), hash_api_key("a"));
        assert_ne!(hash_api_key("a"), hash_api_key("b"));
    }

    #[test]
    #[serial_test::serial]
    fn legacy_env_constructor_works() {
        // The `OPENAI_API_KEY` env var is process-wide; this test
        // is sensitive to concurrent mutation by other tests in
        // the same binary. `serial_test` forces it to run in
        // isolation within the binary.
        let prev = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("ANTHROPIC_API_KEY");
        let s = ConfigStore::from_env();
        assert!(s.routing_table.routes.is_empty());
        if let Some(v) = prev {
            std::env::set_var("OPENAI_API_KEY", v);
        }
    }

    #[test]
    #[serial_test::serial]
    fn legacy_env_constructor_picks_up_openai() {
        // Pair with the previous test: `serial_test::serial`
        // ensures these two do not interleave with anything that
        // mutates `OPENAI_API_KEY` concurrently.
        std::env::set_var("OPENAI_API_KEY", "sk-test");
        std::env::remove_var("ANTHROPIC_API_KEY");
        let s = ConfigStore::from_env();
        assert!(
            s.routing_table.routes.contains_key("gpt-4o"),
            "openai routes should be populated when OPENAI_API_KEY is set"
        );
        // Snapshot then clear so other test binaries in the same
        // `cargo test` invocation (e.g. the admin integration
        // tests) do not see a residual `OPENAI_API_KEY` that would
        // pollute their assertions about the `from_env()`
        // constructor.
        std::env::remove_var("OPENAI_API_KEY");
    }

    #[test]
    fn vendor_to_suite_mapping() {
        assert_eq!(
            vendor_to_suite("anthropic"),
            ProtocolSuite::AnthropicMessages
        );
        assert_eq!(vendor_to_suite("google"), ProtocolSuite::GoogleGemini);
        assert_eq!(vendor_to_suite("openai"), ProtocolSuite::OpenAiCompatible);
    }
}
