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
use serde::Serialize;
use sqlx::Row;
use thiserror::Error;
use tracing::{debug, warn};
use uuid::Uuid;

use tiygate_core::protocol::{ProtocolEndpoint, ProtocolSuite};
use tiygate_core::provider::find_provider;
use tiygate_core::provider::oauth::{OAuthTargetConfig, TokenRequestStyle};
use tiygate_core::routing::{RouteEntry, RoutingTable, RoutingTarget};

use crate::db::DbPool;
use crate::encryption::KeyEncryption;
use crate::keys;
use crate::model_catalog::ModelMetadata;
use crate::models::{
    ApiKey, ApiKeyStatus, AuthMode, ConfigEpoch, ConfigExport, ConfigSnapshot, ExportSetting,
    ImportReport, ImportSelection, Provider, Route, RouteTarget,
};
use crate::settings_keys::is_encrypted_key;

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
                oauth: None,
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
                oauth: None,
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
            if !t.enabled {
                debug!(
                    provider = %t.provider_id,
                    model_id = %t.model_id,
                    virtual_model = %virtual_model,
                    "route target skipped: target disabled"
                );
                continue;
            }
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

            // For OAuth-mode providers, build the OAuthTargetConfig
            // from the decrypted oauth meta + provider preset.
            let oauth_config = if matches!(provider.auth_mode, AuthMode::OAuth) {
                build_oauth_target_config(provider)
            } else {
                None
            };

            let raw_base = t
                .api_base_override
                .clone()
                .unwrap_or_else(|| provider.api_base.clone());
            let (api_protocol, api_base) =
                provider_egress_for_target(provider, &t.model_id, &raw_base);
            targets.push(RoutingTarget {
                provider_id: provider.id.clone(),
                model_id: t.model_id.clone(),
                api_base,
                api_key,
                api_protocol,
                account_label: t.account_label.clone(),
                api_key_override: t.api_key_override.clone(),
                api_base_override: t.api_base_override.clone(),
                weight: t.weight,
                oauth: oauth_config,
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

/// Build an `OAuthTargetConfig` for a provider configured with
/// `AuthMode::OAuth`.
///
/// The OAuth configuration (token_url, client_id, scopes, etc.) is
/// read from `provider.metadata_json["oauth"]`. The refresh token is
/// extracted from `provider.oauth_meta_cleartext` (decrypted by
/// `DbConfigStore::refresh()`). The `token_request_style` is
/// determined by the provider's vendor: Anthropic uses JSON body,
/// all others use form-encoded.
///
/// Returns `None` when the provider is missing the `oauth` metadata
/// block or the refresh token is empty — in that case the data plane
/// will fall through to the static key path (which will also fail,
/// but with a clearer error).
pub fn build_oauth_target_config(provider: &Provider) -> Option<OAuthTargetConfig> {
    let oauth_meta = provider.metadata_json.get("oauth")?;

    let token_url = oauth_meta
        .get("token_url")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let client_id = oauth_meta
        .get("client_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let scopes: Vec<String> = oauth_meta
        .get("scopes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let authorization_header = oauth_meta
        .get("authorization_header")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let authorization_prefix = oauth_meta
        .get("authorization_prefix")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Determine token request style from vendor.
    let token_request_style = match provider.vendor.as_str() {
        "anthropic" => TokenRequestStyle::Json,
        _ => TokenRequestStyle::Form,
    };

    // Extract extra headers from metadata, if present.
    let mut extra_headers: Vec<(String, String)> = oauth_meta
        .get("extra_headers")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let name = entry.get("name")?.as_str()?.to_string();
                    let value = entry.get("value")?.as_str()?.to_string();
                    Some((name, value))
                })
                .collect()
        })
        .unwrap_or_default();

    // Anthropic OAuth requires the `anthropic-beta` header.
    if provider.vendor == "anthropic"
        && !extra_headers
            .iter()
            .any(|(name, _)| name == "anthropic-beta")
    {
        extra_headers.push(("anthropic-beta".to_string(), "oauth-2025-04-20".to_string()));
    }

    // Extract the refresh token from the decrypted oauth meta.
    let refresh_token = provider
        .oauth_meta_cleartext
        .as_deref()
        .and_then(|meta_str| serde_json::from_str::<serde_json::Value>(meta_str).ok())
        .and_then(|meta| {
            meta.get("refresh_token")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();

    if token_url.is_empty() || client_id.is_empty() {
        debug!(
            provider = %provider.id,
            "oauth target config not built: missing token_url or client_id in metadata"
        );
        return None;
    }

    Some(OAuthTargetConfig {
        token_url,
        client_id,
        client_secret: None,
        refresh_token,
        scopes,
        token_request_style,
        authorization_header,
        authorization_prefix,
        extra_headers,
    })
}

fn provider_egress_for_target(
    provider: &Provider,
    model_id: &str,
    raw_base: &str,
) -> (ProtocolEndpoint, String) {
    if let Some(upstream) = find_provider(&provider.vendor) {
        let endpoint = upstream.egress_protocol_for_model(model_id);
        let api_base = upstream.egress_api_base(raw_base, &endpoint);
        (endpoint, api_base)
    } else {
        (
            ProtocolSuite::OpenAiCompatible.default_endpoint(),
            raw_base.to_string(),
        )
    }
}

// ---------------------------------------------------------------------
// DB-backed store
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ProviderRouteImpactRoute {
    pub id: String,
    pub virtual_model: String,
    pub target_count: usize,
    pub remaining_target_count: usize,
    pub will_delete_route: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderRouteImpact {
    pub provider_id: String,
    pub route_count: usize,
    pub target_count: usize,
    pub delete_route_count: usize,
    pub routes: Vec<ProviderRouteImpactRoute>,
}

impl ProviderRouteImpact {
    fn for_routes(provider_id: &str, routes: &[Route]) -> Self {
        let mut impacted = Vec::new();
        let mut target_count = 0usize;
        let mut delete_route_count = 0usize;

        for route in routes {
            let matched = route
                .targets
                .iter()
                .filter(|target| target.provider_id == provider_id)
                .count();
            if matched == 0 {
                continue;
            }
            let remaining = route.targets.len().saturating_sub(matched);
            let will_delete_route = remaining == 0;
            if will_delete_route {
                delete_route_count += 1;
            }
            target_count += matched;
            impacted.push(ProviderRouteImpactRoute {
                id: route.id.clone(),
                virtual_model: route.virtual_model.clone(),
                target_count: matched,
                remaining_target_count: remaining,
                will_delete_route,
            });
        }

        Self {
            provider_id: provider_id.to_string(),
            route_count: impacted.len(),
            target_count,
            delete_route_count,
            routes: impacted,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderRouteCleanup {
    pub before: Route,
    pub after: Option<Route>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderDeleteOutcome {
    pub impact: ProviderRouteImpact,
    pub route_cleanups: Vec<ProviderRouteCleanup>,
}

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
        crate::db::run_migrations(&pool).await?;
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
            .fetch_optional(self.pool.any())
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

        for provider in providers.iter_mut() {
            populate_provider_oauth_cleartext(self.encryption.as_ref(), provider);
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
            "INSERT INTO config_epoch (id, epoch, updated_at) VALUES (1, 1, $1) \
             ON CONFLICT(id) DO UPDATE SET epoch = config_epoch.epoch + 1, updated_at = excluded.updated_at",
        )
        .bind(now)
        .execute(self.pool.any())
        .await?;
        let row = sqlx::query("SELECT epoch FROM config_epoch WHERE id = 1")
            .fetch_one(self.pool.any())
            .await?;
        Ok(row.get::<i64, _>(0))
    }

    // --- Provider CRUD ---

    pub async fn list_providers(&self) -> Result<Vec<Provider>, StoreError> {
        self.load_providers().await
    }

    pub async fn get_provider(&self, id: &str) -> Result<Option<Provider>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, name, vendor, api_base, models_endpoint, encrypted_api_key, auth_mode, \
                    encrypted_oauth_meta, metadata_json, enabled, \
                    created_at, updated_at FROM providers WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool.any())
        .await?;
        let mut provider = rows.map(row_to_provider).transpose()?;
        if let Some(provider) = provider.as_mut() {
            populate_provider_oauth_cleartext(self.encryption.as_ref(), provider);
        }
        Ok(provider)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_provider(
        &self,
        id: &str,
        name: &str,
        vendor: &str,
        api_base: &str,
        models_endpoint: &str,
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
            "INSERT INTO providers (id, name, vendor, api_base, models_endpoint, encrypted_api_key, auth_mode, \
             encrypted_oauth_meta, metadata_json, enabled, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
             ON CONFLICT(id) DO UPDATE SET \
                name=excluded.name, vendor=excluded.vendor, api_base=excluded.api_base, \
                models_endpoint=excluded.models_endpoint, \
                encrypted_api_key=excluded.encrypted_api_key, auth_mode=excluded.auth_mode, \
                encrypted_oauth_meta=excluded.encrypted_oauth_meta, metadata_json=excluded.metadata_json, \
                enabled=excluded.enabled, updated_at=excluded.updated_at",
        )
        .bind(id)
        .bind(name)
        .bind(vendor)
        .bind(api_base)
        .bind(models_endpoint)
        .bind(&encrypted_api_key)
        .bind(auth_mode.as_str())
        .bind(&encrypted_oauth_meta)
        .bind(&metadata_str)
        .bind(enabled_int)
        .bind(&created_at)
        .bind(&now)
        .execute(self.pool.any())
        .await?;

        self.refresh().await?;
        self.get_provider(id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("provider {id} disappeared post-upsert")))
    }

    pub async fn provider_route_impact(
        &self,
        provider_id: &str,
    ) -> Result<ProviderRouteImpact, StoreError> {
        if self.get_provider(provider_id).await?.is_none() {
            return Err(StoreError::NotFound(format!("provider {provider_id}")));
        }
        let routes = self.load_routes().await?;
        Ok(ProviderRouteImpact::for_routes(provider_id, &routes))
    }

    pub async fn delete_provider_cascade_route_targets(
        &self,
        id: &str,
    ) -> Result<ProviderDeleteOutcome, StoreError> {
        let mut tx = self.pool.any().begin().await?;
        let provider: Option<(String,)> = sqlx::query_as("SELECT id FROM providers WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
        if provider.is_none() {
            return Err(StoreError::NotFound(format!("provider {id}")));
        }

        let rows = sqlx::query(
            "SELECT id, virtual_model, targets_json, routing_strategy, model_metadata_json, enabled, created_at, updated_at \
             FROM routes",
        )
        .fetch_all(&mut *tx)
        .await?;
        let routes: Vec<Route> = rows
            .into_iter()
            .map(row_to_route)
            .collect::<Result<_, _>>()?;
        let impact = ProviderRouteImpact::for_routes(id, &routes);
        let mut route_cleanups = Vec::new();
        let now = chrono::Utc::now().to_rfc3339();

        for route in routes {
            if !route.targets.iter().any(|target| target.provider_id == id) {
                continue;
            }
            let before = route.clone();
            let remaining_targets: Vec<RouteTarget> = route
                .targets
                .into_iter()
                .filter(|target| target.provider_id != id)
                .collect();
            if remaining_targets.is_empty() {
                sqlx::query("DELETE FROM routes WHERE id = $1")
                    .bind(&route.id)
                    .execute(&mut *tx)
                    .await?;
                route_cleanups.push(ProviderRouteCleanup {
                    before,
                    after: None,
                });
            } else {
                let targets_json = serde_json::to_string(&remaining_targets)?;
                sqlx::query("UPDATE routes SET targets_json = $1, updated_at = $2 WHERE id = $3")
                    .bind(&targets_json)
                    .bind(&now)
                    .bind(&route.id)
                    .execute(&mut *tx)
                    .await?;
                let mut after = before.clone();
                after.targets = remaining_targets;
                after.updated_at = chrono::DateTime::parse_from_rfc3339(&now)
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now());
                route_cleanups.push(ProviderRouteCleanup {
                    before,
                    after: Some(after),
                });
            }
        }

        let res = sqlx::query("DELETE FROM providers WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("provider {id}")));
        }
        tx.commit().await?;
        self.refresh().await?;
        Ok(ProviderDeleteOutcome {
            impact,
            route_cleanups,
        })
    }

    /// Delete a provider and clean up every route target that references it.
    /// Routes left without any targets are deleted as part of the same
    /// operation; callers that need the cleanup summary should use
    /// [`Self::delete_provider_cascade_route_targets`] directly.
    pub async fn delete_provider(&self, id: &str) -> Result<(), StoreError> {
        self.delete_provider_cascade_route_targets(id).await?;
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
            "UPDATE providers SET encrypted_oauth_meta = $1, updated_at = $2 WHERE id = $3",
        )
        .bind(&encrypted)
        .bind(&now)
        .bind(id)
        .execute(self.pool.any())
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
            "SELECT id, name, vendor, api_base, models_endpoint, encrypted_api_key, auth_mode, \
                    encrypted_oauth_meta, metadata_json, enabled, \
                    created_at, updated_at FROM providers",
        )
        .fetch_all(self.pool.any())
        .await?;
        rows.into_iter().map(row_to_provider).collect()
    }

    // --- Route CRUD ---

    /// Paginated route listing ordered by `created_at DESC`.
    ///
    /// Returns `(page_rows, total_count)` so the admin handler can build the
    /// standard `{ total, limit, offset, entries }` envelope. `limit` and
    /// `offset` are clamped by the caller before reaching this method.
    pub async fn list_routes_paginated(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<Route>, u64), StoreError> {
        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM routes")
            .fetch_one(self.pool.any())
            .await?;
        let rows = sqlx::query(
            "SELECT id, virtual_model, targets_json, routing_strategy, model_metadata_json, enabled, created_at, updated_at \
             FROM routes ORDER BY created_at DESC LIMIT $1 OFFSET $2",
        )
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(self.pool.any())
        .await?;
        let routes: Vec<Route> = rows
            .into_iter()
            .map(row_to_route)
            .collect::<Result<_, _>>()?;
        Ok((routes, total as u64))
    }

    pub async fn get_route(&self, id: &str) -> Result<Option<Route>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, virtual_model, targets_json, routing_strategy, model_metadata_json, enabled, created_at, updated_at \
             FROM routes WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool.any())
        .await?;
        rows.map(row_to_route).transpose()
    }

    pub async fn upsert_route(
        &self,
        id: &str,
        virtual_model: &str,
        targets: &[RouteTarget],
        routing_strategy: Option<tiygate_core::routing::RoutingStrategyName>,
        model_metadata: Option<&ModelMetadata>,
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
        let model_metadata_json = serialize_model_metadata(model_metadata)?;
        let enabled_int: i32 = if enabled { 1 } else { 0 };

        sqlx::query(
            "INSERT INTO routes (id, virtual_model, targets_json, routing_strategy, model_metadata_json, enabled, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT(id) DO UPDATE SET \
                virtual_model=excluded.virtual_model, targets_json=excluded.targets_json, \
                routing_strategy=excluded.routing_strategy, \
                model_metadata_json=excluded.model_metadata_json, \
                enabled=excluded.enabled, updated_at=excluded.updated_at",
        )
        .bind(id)
        .bind(virtual_model)
        .bind(&targets_json)
        .bind(strategy_str)
        .bind(&model_metadata_json)
        .bind(enabled_int)
        .bind(&created_at)
        .bind(&now)
        .execute(self.pool.any())
        .await?;

        self.refresh().await?;
        self.get_route(id)
            .await?
            .ok_or_else(|| StoreError::NotFound(format!("route {id} disappeared post-upsert")))
    }

    pub async fn delete_route(&self, id: &str) -> Result<(), StoreError> {
        let res = sqlx::query("DELETE FROM routes WHERE id = $1")
            .bind(id)
            .execute(self.pool.any())
            .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("route {id}")));
        }
        self.refresh().await?;
        Ok(())
    }

    async fn load_routes(&self) -> Result<Vec<Route>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, virtual_model, targets_json, routing_strategy, model_metadata_json, enabled, created_at, updated_at \
             FROM routes",
        )
        .fetch_all(self.pool.any())
        .await?;
        rows.into_iter().map(row_to_route).collect()
    }

    // --- API key CRUD ---

    pub async fn list_api_keys(&self) -> Result<Vec<ApiKey>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, name, key_hash, quota_json, status, created_at, updated_at \
             FROM api_keys",
        )
        .fetch_all(self.pool.any())
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
             VALUES ($1, $2, $3, $4, 'active', $5, $6)",
        )
        .bind(&id)
        .bind(name)
        .bind(&key_hash)
        .bind(&quota_str)
        .bind(&now)
        .bind(&now)
        .execute(self.pool.any())
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
             FROM api_keys WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool.any())
        .await?;
        row.map(row_to_api_key).transpose()
    }

    pub async fn find_api_key_by_secret(&self, secret: &str) -> Result<Option<ApiKey>, StoreError> {
        let key_hash = hash_api_key(secret);
        let row = sqlx::query(
            "SELECT id, name, key_hash, quota_json, status, created_at, updated_at \
             FROM api_keys WHERE key_hash = $1",
        )
        .bind(&key_hash)
        .fetch_optional(self.pool.any())
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
        let res = sqlx::query("UPDATE api_keys SET quota_json = $1, updated_at = $2 WHERE id = $3")
            .bind(&quota_str)
            .bind(&now)
            .bind(id)
            .execute(self.pool.any())
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
            sqlx::query("UPDATE api_keys SET status = 'disabled', updated_at = $1 WHERE id = $2")
                .bind(&now)
                .bind(id)
                .execute(self.pool.any())
                .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("api key {id}")));
        }
        Ok(())
    }

    pub async fn delete_api_key(&self, id: &str) -> Result<(), StoreError> {
        let res = sqlx::query("DELETE FROM api_keys WHERE id = $1")
            .bind(id)
            .execute(self.pool.any())
            .await?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound(format!("api key {id}")));
        }
        Ok(())
    }

    // --- Config export / import ---

    /// Export all configurable entities (providers, routes, api keys)
    /// into a single serializable bundle. Provider secrets are
    /// carried as their on-disk encrypted blobs; the `encrypted`
    /// flag reflects whether this instance has a master key
    /// configured, which tells the importer whether decryption is
    /// needed. The runtime-only `api_key_cleartext` field is cleared
    /// so no decrypted secret ever leaves the store.
    pub async fn export_config(&self) -> Result<ConfigExport, StoreError> {
        let mut providers = self.load_providers().await?;
        // `load_providers` does not populate `api_key_cleartext`
        // (only `refresh()` does), but we clear it defensively so a
        // future caller that builds a `ConfigExport` from a snapshot
        // cannot leak the decrypted secret.
        for p in providers.iter_mut() {
            p.api_key_cleartext = None;
        }
        let routes = self.load_routes().await?;
        let api_keys = self.list_api_keys().await?;
        // Read the settings table. Each row is tagged with whether it
        // is an encrypted key so the importer knows whether the value
        // is a ciphertext blob that needs the source master key.
        let settings = self
            .list_settings()
            .await?
            .into_iter()
            .map(|(key, value)| ExportSetting {
                encrypted: is_encrypted_key(&key),
                key,
                value,
            })
            .collect();
        let token_daily_stats = crate::token_stats::export_token_daily_stats(&self.pool).await?;
        Ok(ConfigExport {
            schema_version: 1,
            exported_at: chrono::Utc::now().to_rfc3339(),
            encrypted: self.encryption.is_some(),
            providers,
            routes,
            api_keys,
            settings,
            token_daily_stats,
        })
    }

    /// Import a config bundle produced by [`Self::export_config`].
    /// Only items whose id/key appears in `selection` are touched;
    /// everything else is skipped. Selected items are upserted — an
    /// existing row with the same id is overwritten, a new id is
    /// inserted. Provider and OAuth secrets are decrypted with the
    /// supplied `master_key` (the source instance's key) and
    /// re-encrypted with this instance's key before insertion. When
    /// the export was produced without a master key (`encrypted ==
    /// false`), secret columns hold cleartext and are encrypted
    /// directly. Encrypted settings are handled the same way. The
    /// whole import runs in a single transaction that rolls back on
    /// any failure.
    pub async fn import_config(
        &self,
        data: &ConfigExport,
        master_key: &str,
        selection: &ImportSelection,
    ) -> Result<ImportReport, StoreError> {
        // Guard against a future incompatible export format. The
        // exporter currently emits `1`; bumping the version on the
        // write side without updating this read side is a caller bug.
        if data.schema_version != 1 {
            return Err(StoreError::Invalid(format!(
                "unsupported export schema_version: {} (expected 1)",
                data.schema_version
            )));
        }
        // Build the source-instance decryptor when the export was
        // produced with encryption. An empty master_key on an
        // encrypted export is a caller bug.
        let source_enc = if data.encrypted {
            if master_key.trim().is_empty() {
                return Err(StoreError::Invalid(
                    "export is encrypted but no master key was supplied".into(),
                ));
            }
            Some(Arc::new(
                KeyEncryption::from_secret(master_key)
                    .map_err(|e| StoreError::Decrypt(e.to_string()))?,
            ))
        } else {
            None
        };

        // Pre-compute selection membership sets for O(1) lookup.
        let sel_providers: std::collections::HashSet<&str> =
            selection.providers.iter().map(String::as_str).collect();
        let sel_routes: std::collections::HashSet<&str> =
            selection.routes.iter().map(String::as_str).collect();
        let sel_api_keys: std::collections::HashSet<&str> =
            selection.api_keys.iter().map(String::as_str).collect();
        let sel_settings: std::collections::HashSet<&str> =
            selection.settings.iter().map(String::as_str).collect();
        let sel_token_stats: std::collections::HashSet<&str> =
            selection.token_stats.iter().map(String::as_str).collect();

        let mut tx = self.pool.any().begin().await?;
        let mut report = ImportReport {
            providers_imported: 0,
            providers_skipped: 0,
            routes_imported: 0,
            routes_skipped: 0,
            api_keys_imported: 0,
            api_keys_skipped: 0,
            settings_imported: 0,
            settings_skipped: 0,
            token_stats_imported: 0,
            token_stats_skipped: 0,
        };

        for p in &data.providers {
            if !sel_providers.contains(p.id.as_str()) {
                report.providers_skipped += 1;
                continue;
            }
            // Re-encrypt provider secrets so they are readable by
            // this instance. When the source had encryption, decrypt
            // with the source key first; otherwise the column holds
            // cleartext.
            let enc_api_key = if p.encrypted_api_key.is_empty() {
                String::new()
            } else {
                let plain = match &source_enc {
                    Some(src) => keys::decrypt_api_key(src, &p.encrypted_api_key)
                        .map_err(|e| StoreError::Decrypt(e.to_string()))?,
                    None => p.encrypted_api_key.clone(),
                };
                match self.encryption.as_ref() {
                    Some(enc) => keys::encrypt_api_key(enc, &plain)
                        .map_err(|e| StoreError::Decrypt(e.to_string()))?,
                    None => plain,
                }
            };
            let enc_oauth_meta = if p.encrypted_oauth_meta.is_empty() {
                String::new()
            } else {
                let plain = match &source_enc {
                    Some(src) => keys::decrypt_oauth_meta(src, &p.encrypted_oauth_meta)
                        .map_err(|e| StoreError::Decrypt(e.to_string()))?,
                    None => p.encrypted_oauth_meta.clone(),
                };
                match self.encryption.as_ref() {
                    Some(enc) => keys::encrypt_oauth_meta(enc, &plain)
                        .map_err(|e| StoreError::Decrypt(e.to_string()))?,
                    None => plain,
                }
            };
            let metadata_str = serde_json::to_string(&p.metadata_json)?;
            let enabled_int: i32 = if p.enabled { 1 } else { 0 };
            let created_at = p.created_at.to_rfc3339();
            let updated_at = chrono::Utc::now().to_rfc3339();
            // Upsert: overwrite an existing row with the same id so
            // the operator's explicit "overwrite" selection takes
            // effect.
            sqlx::query(
                "INSERT INTO providers (id, name, vendor, api_base, models_endpoint, encrypted_api_key, auth_mode, \
                 encrypted_oauth_meta, metadata_json, enabled, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
                 ON CONFLICT(id) DO UPDATE SET \
                    name=excluded.name, vendor=excluded.vendor, api_base=excluded.api_base, \
                    models_endpoint=excluded.models_endpoint, \
                    encrypted_api_key=excluded.encrypted_api_key, auth_mode=excluded.auth_mode, \
                    encrypted_oauth_meta=excluded.encrypted_oauth_meta, \
                    metadata_json=excluded.metadata_json, \
                    enabled=excluded.enabled, updated_at=excluded.updated_at",
            )
            .bind(&p.id)
            .bind(&p.name)
            .bind(&p.vendor)
            .bind(&p.api_base)
            .bind(&p.models_endpoint)
            .bind(&enc_api_key)
            .bind(p.auth_mode.as_str())
            .bind(&enc_oauth_meta)
            .bind(&metadata_str)
            .bind(enabled_int)
            .bind(&created_at)
            .bind(&updated_at)
            .execute(&mut *tx)
            .await?;
            report.providers_imported += 1;
        }

        for r in &data.routes {
            if !sel_routes.contains(r.id.as_str()) {
                report.routes_skipped += 1;
                continue;
            }
            let targets_json = serde_json::to_string(&r.targets)?;
            let strategy_str = r.routing_strategy.map(|s| s.as_str());
            let model_metadata_json = serialize_model_metadata(r.model_metadata.as_ref())?;
            let enabled_int: i32 = if r.enabled { 1 } else { 0 };
            let created_at = r.created_at.to_rfc3339();
            let updated_at = chrono::Utc::now().to_rfc3339();
            sqlx::query(
                "INSERT INTO routes (id, virtual_model, targets_json, routing_strategy, model_metadata_json, enabled, \
                 created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT(id) DO UPDATE SET \
                    virtual_model=excluded.virtual_model, targets_json=excluded.targets_json, \
                    routing_strategy=excluded.routing_strategy, \
                    model_metadata_json=excluded.model_metadata_json, \
                    enabled=excluded.enabled, updated_at=excluded.updated_at",
            )
            .bind(&r.id)
            .bind(&r.virtual_model)
            .bind(&targets_json)
            .bind(strategy_str)
            .bind(&model_metadata_json)
            .bind(enabled_int)
            .bind(&created_at)
            .bind(&updated_at)
            .execute(&mut *tx)
            .await?;
            report.routes_imported += 1;
        }

        // The api_keys table has a UNIQUE constraint on key_hash in
        // addition to the PRIMARY KEY on id. An import that carries
        // a key_hash already present under a different id would
        // violate that constraint, so we pre-check and skip such
        // rows rather than letting the INSERT fail and roll back the
        // whole transaction.
        for k in &data.api_keys {
            if !sel_api_keys.contains(k.id.as_str()) {
                report.api_keys_skipped += 1;
                continue;
            }
            let existing_hash: Option<(String,)> =
                sqlx::query_as("SELECT key_hash FROM api_keys WHERE key_hash = $1")
                    .bind(&k.key_hash)
                    .fetch_optional(&mut *tx)
                    .await?;
            if existing_hash.is_some() {
                report.api_keys_skipped += 1;
                continue;
            }
            let quota_str = serde_json::to_string(&k.quota_json)?;
            let created_at = k.created_at.to_rfc3339();
            let updated_at = chrono::Utc::now().to_rfc3339();
            // Upsert by id: overwrite when the operator explicitly
            // selected an existing id.
            sqlx::query(
                "INSERT INTO api_keys (id, name, key_hash, quota_json, status, created_at, \
                 updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT(id) DO UPDATE SET \
                    name=excluded.name, key_hash=excluded.key_hash, quota_json=excluded.quota_json, \
                    status=excluded.status, updated_at=excluded.updated_at",
            )
            .bind(&k.id)
            .bind(&k.name)
            .bind(&k.key_hash)
            .bind(&quota_str)
            .bind(k.status.as_str())
            .bind(&created_at)
            .bind(&updated_at)
            .execute(&mut *tx)
            .await?;
            report.api_keys_imported += 1;
        }

        // Settings: re-encrypt encrypted rows with this instance's
        // key (decrypting with the source key first when the export
        // was encrypted), and store plain rows verbatim.
        for s in &data.settings {
            if !sel_settings.contains(s.key.as_str()) {
                report.settings_skipped += 1;
                continue;
            }
            if s.encrypted {
                // Decrypt the source ciphertext blob, then re-encrypt
                // with this instance's key. When the export was not
                // encrypted, the value is already cleartext.
                let plain = match &source_enc {
                    Some(src) => keys::decrypt_settings(src, &s.value)
                        .map_err(|e| StoreError::Decrypt(e.to_string()))?,
                    None => s.value.clone(),
                };
                let stored = match self.encryption.as_ref() {
                    Some(enc) => keys::encrypt_settings(enc, &plain)
                        .map_err(|e| StoreError::Decrypt(e.to_string()))?,
                    None => plain,
                };
                let now = chrono::Utc::now().to_rfc3339();
                sqlx::query(
                    "INSERT INTO settings (key, value, updated_at) VALUES ($1, $2, $3) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
                     updated_at = excluded.updated_at",
                )
                .bind(&s.key)
                .bind(&stored)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
            } else {
                let now = chrono::Utc::now().to_rfc3339();
                sqlx::query(
                    "INSERT INTO settings (key, value, updated_at) VALUES ($1, $2, $3) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
                     updated_at = excluded.updated_at",
                )
                .bind(&s.key)
                .bind(&s.value)
                .bind(&now)
                .execute(&mut *tx)
                .await?;
            }
            report.settings_imported += 1;
        }

        // Token daily stats: additive merge. For each selected day,
        // sum the count/token columns and take MAX of the peak/latency
        // columns. This is non-destructive — importing the same day
        // twice accumulates rather than overwrites.
        for s in &data.token_daily_stats {
            if !sel_token_stats.contains(s.day.as_str()) {
                report.token_stats_skipped += 1;
                continue;
            }
            let now = chrono::Utc::now().to_rfc3339();
            sqlx::query(
                "INSERT INTO token_daily_stats \
                    (day, request_count, total_tokens, prompt_tokens, completion_tokens, \
                     reasoning_tokens, peak_single_request, longest_task_ms, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
                 ON CONFLICT(day) DO UPDATE SET \
                    request_count = token_daily_stats.request_count + excluded.request_count, \
                    total_tokens = token_daily_stats.total_tokens + excluded.total_tokens, \
                    prompt_tokens = token_daily_stats.prompt_tokens + excluded.prompt_tokens, \
                    completion_tokens = token_daily_stats.completion_tokens + excluded.completion_tokens, \
                    reasoning_tokens = token_daily_stats.reasoning_tokens + excluded.reasoning_tokens, \
                    peak_single_request = MAX(token_daily_stats.peak_single_request, excluded.peak_single_request), \
                    longest_task_ms = MAX(token_daily_stats.longest_task_ms, excluded.longest_task_ms), \
                    updated_at = excluded.updated_at",
            )
            .bind(&s.day)
            .bind(s.request_count)
            .bind(s.total_tokens)
            .bind(s.prompt_tokens)
            .bind(s.completion_tokens)
            .bind(s.reasoning_tokens)
            .bind(s.peak_single_request)
            .bind(s.longest_task_ms)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
            report.token_stats_imported += 1;
        }

        tx.commit().await?;
        // Recompute the token_summary from the merged daily stats so
        // lifetime/peak/streaks reflect the combined data. This runs
        // after commit because recompute_summary uses the pool
        // directly (compute_streaks queries via pool.any()).
        if report.token_stats_imported > 0 {
            crate::token_stats::recompute_summary(&self.pool).await?;
        }
        // Refresh the in-memory snapshot so the data plane picks up
        // the newly imported rows immediately.
        self.refresh().await?;
        Ok(report)
    }

    // --- Settings ---

    pub async fn get_setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        let row = sqlx::query("SELECT value FROM settings WHERE key = $1")
            .bind(key)
            .fetch_optional(self.pool.any())
            .await?;
        Ok(row.map(|r| r.get::<String, _>(0)))
    }

    /// Returns all settings as `(key, value)` pairs. Used by the
    /// admin API for bulk reads and by the bootstrap path on first
    /// start to detect whether any settings have been written yet.
    pub async fn list_settings(&self) -> Result<Vec<(String, String)>, StoreError> {
        let rows = sqlx::query("SELECT key, value FROM settings ORDER BY key")
            .fetch_all(self.pool.any())
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.get::<String, _>(0), r.get::<String, _>(1)))
            .collect())
    }

    /// Insert or update a setting and bump the config epoch so the
    /// data plane and background tasks pick up the new value on
    /// their next poll. `bump_epoch` is best-effort: a failure to
    /// increment the epoch is logged but does not roll back the
    /// settings write, matching the existing `refresh()` behaviour
    /// where the snapshot is still published even if the epoch read
    /// races.
    pub async fn set_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO settings (key, value, updated_at) VALUES ($1, $2, $3) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(&now)
        .execute(self.pool.any())
        .await?;
        // Notify the data plane + background tasks that a setting
        // changed. We intentionally do NOT call `refresh()` here:
        // settings are not part of the routing snapshot, and a full
        // reload of providers/routes on every settings write would
        // be wasteful. The epoch bump is enough for the epoch-poll
        // task to wake and reload the runtime tunables.
        if let Err(e) = self.bump_epoch().await {
            tracing::warn!(error = %e, key, "failed to bump epoch after settings update");
        }
        Ok(())
    }

    /// Read an encrypted setting, decrypting it with the master key.
    /// Returns `Ok(None)` when the key is absent. When no master key
    /// is configured the stored value is returned as-is (cleartext
    /// mode, consistent with provider API key handling).
    pub async fn get_setting_encrypted(&self, key: &str) -> Result<Option<String>, StoreError> {
        let raw = self.get_setting(key).await?;
        match (raw, self.encryption.as_ref()) {
            (Some(blob), Some(enc)) => enc
                .decrypt(crate::keys::PURPOSE_SETTINGS, &blob)
                .map(Some)
                .map_err(|e| StoreError::Decrypt(e.to_string())),
            (Some(plain), None) => Ok(Some(plain)),
            (None, _) => Ok(None),
        }
    }

    /// Encrypt and store a sensitive setting (e.g. S3 credentials).
    /// Without a master key the value is stored in cleartext with a
    /// warning, mirroring the provider API key path.
    pub async fn set_setting_encrypted(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let stored = match self.encryption.as_ref() {
            Some(enc) => crate::keys::encrypt_settings(enc, value)
                .map_err(|e| StoreError::Decrypt(e.to_string()))?,
            None => {
                warn!(
                    key,
                    "TIYGATE_MASTER_KEY not set; storing setting in cleartext (NOT FOR PRODUCTION)"
                );
                value.to_string()
            }
        };
        self.set_setting(key, &stored).await
    }

    // --- ConfigEpoch ---

    pub async fn get_epoch(&self) -> Result<ConfigEpoch, StoreError> {
        let row = sqlx::query("SELECT epoch, updated_at FROM config_epoch WHERE id = 1")
            .fetch_optional(self.pool.any())
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

fn row_to_provider(row: sqlx::any::AnyRow) -> Result<Provider, StoreError> {
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
        models_endpoint: row.get("models_endpoint"),
        encrypted_api_key: row.get("encrypted_api_key"),
        auth_mode,
        encrypted_oauth_meta: row.get("encrypted_oauth_meta"),
        metadata_json,
        enabled: enabled_int != 0,
        created_at: parse_dt(row.get("created_at"))?,
        updated_at: parse_dt(row.get("updated_at"))?,
        api_key_cleartext: None,
        oauth_meta_cleartext: None,
    })
}

fn populate_provider_oauth_cleartext(
    encryption: Option<&Arc<KeyEncryption>>,
    provider: &mut Provider,
) {
    if !matches!(provider.auth_mode, AuthMode::OAuth) {
        return;
    }
    if provider.encrypted_oauth_meta.is_empty() {
        provider.oauth_meta_cleartext = Some(String::new());
        return;
    }
    if let Some(enc) = encryption {
        match keys::decrypt_oauth_meta(enc, &provider.encrypted_oauth_meta) {
            Ok(plain) => provider.oauth_meta_cleartext = Some(plain),
            Err(e) => {
                tracing::warn!(
                    provider = %provider.id,
                    error = %e,
                    "decrypting provider oauth meta failed; data plane will not be able to refresh OAuth tokens"
                );
                provider.oauth_meta_cleartext = None;
            }
        }
    } else {
        // No master key: column holds cleartext.
        provider.oauth_meta_cleartext = Some(provider.encrypted_oauth_meta.clone());
    }
}

fn serialize_model_metadata(metadata: Option<&ModelMetadata>) -> Result<String, StoreError> {
    match metadata {
        Some(metadata) => Ok(serde_json::to_string(metadata)?),
        None => Ok("{}".to_string()),
    }
}

fn deserialize_model_metadata(raw: &str) -> Result<Option<ModelMetadata>, StoreError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_str(trimmed)?;
    match value {
        serde_json::Value::Object(map) if map.is_empty() => Ok(None),
        other => Ok(Some(serde_json::from_value(other)?)),
    }
}

fn row_to_route(row: sqlx::any::AnyRow) -> Result<Route, StoreError> {
    let targets_str: String = row.get("targets_json");
    let targets: Vec<RouteTarget> = serde_json::from_str(&targets_str)?;
    let model_metadata_str: String = row.get("model_metadata_json");
    let model_metadata = deserialize_model_metadata(&model_metadata_str)?;
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
        model_metadata,
        enabled: enabled_int != 0,
        created_at: parse_dt(row.get("created_at"))?,
        updated_at: parse_dt(row.get("updated_at"))?,
    })
}

fn row_to_api_key(row: sqlx::any::AnyRow) -> Result<ApiKey, StoreError> {
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
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn hash_api_key_is_deterministic() {
        assert_eq!(hash_api_key("a"), hash_api_key("a"));
        assert_ne!(hash_api_key("a"), hash_api_key("b"));
    }

    #[test]
    fn snapshot_to_routing_table_uses_fallback_when_provider_is_not_registered() {
        use crate::models::{ConfigSnapshot, Provider, Route, RouteTarget};
        use std::collections::HashMap;

        let now = chrono::Utc::now();
        let provider = Provider {
            id: "prov-unregistered".to_string(),
            name: "Unregistered Provider".to_string(),
            vendor: "unregistered-openai-compatible".to_string(),
            api_base: "https://example.test/root".to_string(),
            models_endpoint: String::new(),
            encrypted_api_key: "sk-test".to_string(),
            auth_mode: AuthMode::ApiKey,
            encrypted_oauth_meta: String::new(),
            metadata_json: serde_json::json!({}),
            enabled: true,
            created_at: now,
            updated_at: now,
            api_key_cleartext: Some("sk-test".to_string()),
            oauth_meta_cleartext: None,
        };
        let route = Route {
            id: "route-fallback".to_string(),
            virtual_model: "vm".to_string(),
            targets: vec![RouteTarget {
                provider_id: provider.id.clone(),
                model_id: "unknown-model".to_string(),
                weight: 1.0,
                enabled: true,
                account_label: None,
                api_key_override: None,
                api_base_override: None,
            }],
            routing_strategy: None,
            model_metadata: None,
            enabled: true,
            created_at: now,
            updated_at: now,
        };
        let mut providers = HashMap::new();
        providers.insert(provider.id.clone(), provider);
        let mut routes = HashMap::new();
        routes.insert(route.virtual_model.clone(), route);
        let snapshot = ConfigSnapshot {
            epoch: 1,
            providers,
            routes,
        };

        let table = snapshot_to_routing_table(&snapshot);
        let target = &table.routes.get("vm").unwrap().targets[0];
        assert_eq!(target.api_protocol.suite, ProtocolSuite::OpenAiCompatible);
        assert_eq!(target.api_base, "https://example.test/root");
    }

    #[test]
    fn snapshot_to_routing_table_skips_disabled_targets() {
        use crate::models::{ConfigSnapshot, Provider, Route, RouteTarget};
        use std::collections::HashMap;

        let now = chrono::Utc::now();
        let provider = Provider {
            id: "prov-x".to_string(),
            name: "p".to_string(),
            vendor: "openai".to_string(),
            api_base: "https://api.openai.com/v1".to_string(),
            models_endpoint: String::new(),
            encrypted_api_key: "sk-test".to_string(),
            auth_mode: AuthMode::ApiKey,
            encrypted_oauth_meta: String::new(),
            metadata_json: serde_json::json!({}),
            enabled: true,
            created_at: now,
            updated_at: now,
            api_key_cleartext: Some("sk-test".to_string()),
            oauth_meta_cleartext: None,
        };
        // Two targets: one enabled, one disabled.
        let target_enabled = RouteTarget {
            provider_id: "prov-x".to_string(),
            model_id: "gpt-4o".to_string(),
            weight: 2.0,
            enabled: true,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
        };
        let target_disabled = RouteTarget {
            provider_id: "prov-x".to_string(),
            model_id: "gpt-4o-mini".to_string(),
            weight: 1.0,
            enabled: false,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
        };
        let route = Route {
            id: "route-1".to_string(),
            virtual_model: "vm".to_string(),
            targets: vec![target_disabled, target_enabled],
            routing_strategy: None,
            model_metadata: None,
            enabled: true,
            created_at: now,
            updated_at: now,
        };
        let mut providers = HashMap::new();
        providers.insert(provider.id.clone(), provider);
        let mut routes = HashMap::new();
        routes.insert("vm".to_string(), route);
        let snapshot = ConfigSnapshot {
            epoch: 1,
            providers,
            routes,
        };
        let table = snapshot_to_routing_table(&snapshot);
        let entry = table
            .routes
            .get("vm")
            .expect("route entry should still exist when at least one target is enabled");
        assert_eq!(entry.targets.len(), 1);
        assert_eq!(entry.targets[0].model_id, "gpt-4o");
        assert_eq!(entry.targets[0].weight, 2.0);
    }

    #[test]
    fn snapshot_to_routing_table_drops_route_when_all_targets_disabled() {
        use crate::models::{ConfigSnapshot, Provider, Route, RouteTarget};
        use std::collections::HashMap;

        let now = chrono::Utc::now();
        let provider = Provider {
            id: "prov-x".to_string(),
            name: "p".to_string(),
            vendor: "openai".to_string(),
            api_base: "https://api.openai.com/v1".to_string(),
            models_endpoint: String::new(),
            encrypted_api_key: "sk-test".to_string(),
            auth_mode: AuthMode::ApiKey,
            encrypted_oauth_meta: String::new(),
            metadata_json: serde_json::json!({}),
            enabled: true,
            created_at: now,
            updated_at: now,
            api_key_cleartext: Some("sk-test".to_string()),
            oauth_meta_cleartext: None,
        };
        let route = Route {
            id: "route-1".to_string(),
            virtual_model: "vm".to_string(),
            targets: vec![RouteTarget {
                provider_id: "prov-x".to_string(),
                model_id: "gpt-4o".to_string(),
                weight: 1.0,
                enabled: false,
                account_label: None,
                api_key_override: None,
                api_base_override: None,
            }],
            routing_strategy: None,
            model_metadata: None,
            enabled: true,
            created_at: now,
            updated_at: now,
        };
        let mut providers = HashMap::new();
        providers.insert(provider.id.clone(), provider);
        let mut routes = HashMap::new();
        routes.insert("vm".to_string(), route);
        let snapshot = ConfigSnapshot {
            epoch: 1,
            providers,
            routes,
        };
        let table = snapshot_to_routing_table(&snapshot);
        assert!(
            !table.routes.contains_key("vm"),
            "route with all disabled targets must be omitted from the runtime routing table"
        );
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

    // ---- config export / import tests ----

    fn master_key_hex() -> String {
        (0..32u8).map(|i| format!("{:02x}", i)).collect()
    }

    async fn boot_store(encryption: Option<Arc<KeyEncryption>>) -> DbConfigStore {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrate");
        let store = DbConfigStore::new(pool, encryption);
        store.refresh().await.expect("refresh");
        store
    }

    #[tokio::test]
    async fn get_provider_populates_oauth_meta_cleartext() {
        let key = KeyEncryption::from_secret(&master_key_hex()).expect("key");
        let store = boot_store(Some(Arc::new(key))).await;
        let meta = serde_json::json!({
            "refresh_token": "rt-test",
            "expires_in_s": 3600,
        });
        let meta_str = serde_json::to_string(&meta).expect("serialize meta");
        store
            .upsert_provider(
                "oauth-openai",
                "OAuth OpenAI",
                "openai",
                "https://api.openai.com/v1",
                "",
                None,
                AuthMode::OAuth,
                Some(&meta_str),
                serde_json::json!({}),
                true,
            )
            .await
            .expect("upsert oauth provider");

        let provider = store
            .get_provider("oauth-openai")
            .await
            .expect("get provider")
            .expect("provider exists");
        assert_eq!(
            provider.oauth_meta_cleartext.as_deref(),
            Some(meta_str.as_str())
        );
    }

    fn test_model_metadata(id: &str) -> ModelMetadata {
        ModelMetadata {
            id: id.to_string(),
            lab_id: "test-lab".to_string(),
            display_name: "Saved Test Model".to_string(),
            family: Some("test-family".to_string()),
            context_window: Some(12345),
            max_input_tokens: Some(12000),
            max_output_tokens: Some(345),
            capabilities: serde_json::Map::new(),
            modalities: None,
            pricing: None,
            metadata: serde_json::Map::new(),
        }
    }

    #[tokio::test]
    async fn route_model_metadata_round_trips_through_crud_and_export_import() {
        let store = boot_store(None).await;
        store
            .upsert_provider(
                "p-meta",
                "Provider Meta",
                "openai",
                "https://example.test/v1",
                "",
                None,
                AuthMode::None,
                None,
                serde_json::json!({}),
                true,
            )
            .await
            .expect("provider");
        let metadata = test_model_metadata("virtual/meta");
        let route = store
            .upsert_route(
                "r-meta",
                "virtual/meta",
                &[RouteTarget {
                    provider_id: "p-meta".to_string(),
                    model_id: "upstream-meta".to_string(),
                    weight: 1.0,
                    enabled: true,
                    account_label: None,
                    api_key_override: None,
                    api_base_override: None,
                }],
                None,
                Some(&metadata),
                true,
            )
            .await
            .expect("route");
        assert_eq!(
            route.model_metadata.as_ref().unwrap().display_name,
            "Saved Test Model"
        );

        let bundle = store.export_config().await.expect("export");
        assert_eq!(
            bundle.routes[0].model_metadata.as_ref().unwrap().lab_id,
            "test-lab"
        );

        let target = boot_store(None).await;
        let selection = ImportSelection {
            providers: vec!["p-meta".to_string()],
            routes: vec!["r-meta".to_string()],
            ..Default::default()
        };
        target
            .import_config(&bundle, "", &selection)
            .await
            .expect("import");
        let imported = target
            .get_route("r-meta")
            .await
            .expect("get")
            .expect("route");
        assert_eq!(
            imported.model_metadata.as_ref().unwrap().context_window,
            Some(12345)
        );
    }

    #[tokio::test]
    async fn export_config_returns_all_entities_and_clears_cleartext() {
        let store = boot_store(None).await;
        store
            .upsert_provider(
                "p1",
                "Provider 1",
                "openai",
                "https://api.openai.com/v1",
                "",
                Some("sk-test"),
                AuthMode::ApiKey,
                None,
                serde_json::json!({}),
                true,
            )
            .await
            .expect("upsert provider");
        store
            .upsert_route(
                "r1",
                "gpt-4o",
                &[RouteTarget {
                    provider_id: "p1".into(),
                    model_id: "gpt-4o".into(),
                    weight: 1.0,
                    enabled: true,
                    account_label: None,
                    api_key_override: None,
                    api_base_override: None,
                }],
                None,
                None,
                true,
            )
            .await
            .expect("upsert route");
        store
            .create_api_key("key-1", "secret-1", serde_json::json!({}))
            .await
            .expect("create api key");

        let bundle = store.export_config().await.expect("export");
        assert_eq!(bundle.schema_version, 1);
        assert!(!bundle.encrypted, "no encryption configured");
        assert_eq!(bundle.providers.len(), 1);
        assert_eq!(bundle.routes.len(), 1);
        assert_eq!(bundle.api_keys.len(), 1);
        // The runtime-only cleartext field must never appear in an
        // export, even when the store runs without a master key.
        assert!(
            bundle.providers[0].api_key_cleartext.is_none(),
            "export must not carry decrypted api_key_cleartext"
        );
    }

    #[tokio::test]
    async fn import_config_skips_existing_ids_and_inserts_new() {
        let store = boot_store(None).await;
        // Pre-populate one provider so the import should skip it.
        store
            .upsert_provider(
                "p-existing",
                "Existing",
                "openai",
                "https://api.openai.com/v1",
                "",
                Some("sk-old"),
                AuthMode::ApiKey,
                None,
                serde_json::json!({}),
                true,
            )
            .await
            .expect("upsert existing provider");

        let now = chrono::Utc::now();
        let bundle = ConfigExport {
            schema_version: 1,
            exported_at: now.to_rfc3339(),
            encrypted: false,
            providers: vec![
                Provider {
                    id: "p-existing".into(),
                    name: "Existing (from export)".into(),
                    vendor: "openai".into(),
                    api_base: "https://api.openai.com/v1".into(),
                    models_endpoint: String::new(),
                    encrypted_api_key: "sk-exported".into(),
                    auth_mode: AuthMode::ApiKey,
                    encrypted_oauth_meta: String::new(),
                    metadata_json: serde_json::json!({}),
                    enabled: true,
                    created_at: now,
                    updated_at: now,
                    api_key_cleartext: None,
                    oauth_meta_cleartext: None,
                },
                Provider {
                    id: "p-new".into(),
                    name: "New".into(),
                    vendor: "anthropic".into(),
                    api_base: "https://api.anthropic.com".into(),
                    models_endpoint: String::new(),
                    encrypted_api_key: "sk-new".into(),
                    auth_mode: AuthMode::ApiKey,
                    encrypted_oauth_meta: String::new(),
                    metadata_json: serde_json::json!({}),
                    enabled: true,
                    created_at: now,
                    updated_at: now,
                    api_key_cleartext: None,
                    oauth_meta_cleartext: None,
                },
            ],
            routes: vec![],
            api_keys: vec![],
            settings: vec![],
            token_daily_stats: vec![],
        };

        // Select only the new provider; the existing one stays
        // untouched (matching the old skip-on-conflict behaviour
        // for a default-unchecked existing id).
        let selection = ImportSelection {
            providers: vec!["p-new".to_string()],
            ..Default::default()
        };
        let report = store
            .import_config(&bundle, "", &selection)
            .await
            .expect("import");
        assert_eq!(report.providers_imported, 1);
        assert_eq!(report.providers_skipped, 1);

        // The existing provider must not have been overwritten.
        let existing = store
            .get_provider("p-existing")
            .await
            .expect("get")
            .expect("provider exists");
        assert_eq!(
            existing.name, "Existing",
            "existing row must not be overwritten"
        );

        // The new provider should be present.
        let new = store
            .get_provider("p-new")
            .await
            .expect("get")
            .expect("new provider exists");
        assert_eq!(new.name, "New");
    }

    #[tokio::test]
    async fn import_config_re_encrypts_provider_secrets_with_different_master_keys() {
        // Source instance with master key A.
        let key_a = KeyEncryption::from_secret(&master_key_hex()).expect("key A");
        let source_store = boot_store(Some(Arc::new(key_a))).await;
        source_store
            .upsert_provider(
                "p-enc",
                "Encrypted Provider",
                "openai",
                "https://api.openai.com/v1",
                "",
                Some("sk-secret-value"),
                AuthMode::ApiKey,
                None,
                serde_json::json!({}),
                true,
            )
            .await
            .expect("upsert provider with encryption");

        let bundle = source_store.export_config().await.expect("export");
        assert!(bundle.encrypted, "export must be flagged encrypted");
        assert_eq!(bundle.providers.len(), 1);
        // The exported blob must NOT be the cleartext.
        assert_ne!(
            bundle.providers[0].encrypted_api_key, "sk-secret-value",
            "encrypted export must not carry cleartext"
        );

        // Target instance with master key B.
        let key_b_hex: String = (0..32u8).map(|i| format!("{:02x}", i + 100)).collect();
        let key_b = KeyEncryption::from_secret(&key_b_hex).expect("key B");
        let target_store = boot_store(Some(Arc::new(key_b))).await;

        // Select the provider for import.
        let selection = ImportSelection {
            providers: vec!["p-enc".to_string()],
            ..Default::default()
        };
        let report = target_store
            .import_config(&bundle, &master_key_hex(), &selection)
            .await
            .expect("import");
        assert_eq!(report.providers_imported, 1);

        // After import, the target instance should be able to decrypt
        // the provider key with its own master key (B).
        let imported = target_store
            .get_provider("p-enc")
            .await
            .expect("get")
            .expect("provider exists");
        let plain = keys::decrypt_api_key(
            &KeyEncryption::from_secret(&key_b_hex).expect("key B"),
            &imported.encrypted_api_key,
        )
        .expect("decrypt with target key");
        assert_eq!(plain, "sk-secret-value");
    }

    #[tokio::test]
    async fn import_config_rejects_encrypted_export_without_master_key() {
        let store = boot_store(None).await;
        let bundle = ConfigExport {
            schema_version: 1,
            exported_at: chrono::Utc::now().to_rfc3339(),
            encrypted: true,
            providers: vec![],
            routes: vec![],
            api_keys: vec![],
            settings: vec![],
            token_daily_stats: vec![],
        };
        let err = store
            .import_config(&bundle, "", &ImportSelection::default())
            .await;
        assert!(
            matches!(err, Err(StoreError::Invalid(_))),
            "encrypted export without master key must be rejected"
        );
    }

    #[tokio::test]
    async fn import_config_skips_api_key_with_duplicate_hash() {
        let store = boot_store(None).await;
        // Pre-create an api key with the same secret.
        store
            .create_api_key("original", "shared-secret", serde_json::json!({}))
            .await
            .expect("create api key");
        let existing_hash = hash_api_key("shared-secret");

        let now = chrono::Utc::now();
        let bundle = ConfigExport {
            schema_version: 1,
            exported_at: now.to_rfc3339(),
            encrypted: false,
            providers: vec![],
            routes: vec![],
            api_keys: vec![ApiKey {
                id: "imported-key".into(),
                name: "Imported".into(),
                key_hash: existing_hash,
                quota_json: serde_json::json!({}),
                status: ApiKeyStatus::Active,
                created_at: now,
                updated_at: now,
            }],
            settings: vec![],
            token_daily_stats: vec![],
        };

        let selection = ImportSelection {
            api_keys: vec!["imported-key".to_string()],
            ..Default::default()
        };
        let report = store
            .import_config(&bundle, "", &selection)
            .await
            .expect("import");
        assert_eq!(report.api_keys_imported, 0);
        assert_eq!(report.api_keys_skipped, 1);
    }

    #[tokio::test]
    async fn import_config_overwrites_existing_provider_when_selected() {
        let store = boot_store(None).await;
        store
            .upsert_provider(
                "p-dup",
                "Original",
                "openai",
                "https://api.openai.com/v1",
                "",
                Some("sk-old"),
                AuthMode::ApiKey,
                None,
                serde_json::json!({}),
                true,
            )
            .await
            .expect("upsert existing provider");

        let now = chrono::Utc::now();
        let bundle = ConfigExport {
            schema_version: 1,
            exported_at: now.to_rfc3339(),
            encrypted: false,
            providers: vec![Provider {
                id: "p-dup".into(),
                name: "Overwritten".into(),
                vendor: "openai".into(),
                api_base: "https://api.openai.com/v1".into(),
                models_endpoint: String::new(),
                encrypted_api_key: "sk-new".into(),
                auth_mode: AuthMode::ApiKey,
                encrypted_oauth_meta: String::new(),
                metadata_json: serde_json::json!({}),
                enabled: true,
                created_at: now,
                updated_at: now,
                api_key_cleartext: None,
                oauth_meta_cleartext: None,
            }],
            routes: vec![],
            api_keys: vec![],
            settings: vec![],
            token_daily_stats: vec![],
        };

        // Explicitly select the existing id → upsert overwrites it.
        let selection = ImportSelection {
            providers: vec!["p-dup".to_string()],
            ..Default::default()
        };
        let report = store
            .import_config(&bundle, "", &selection)
            .await
            .expect("import");
        assert_eq!(report.providers_imported, 1);
        assert_eq!(report.providers_skipped, 0);

        let p = store
            .get_provider("p-dup")
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(p.name, "Overwritten");
    }

    #[tokio::test]
    async fn import_config_empty_selection_imports_nothing() {
        let store = boot_store(None).await;
        let now = chrono::Utc::now();
        let bundle = ConfigExport {
            schema_version: 1,
            exported_at: now.to_rfc3339(),
            encrypted: false,
            providers: vec![Provider {
                id: "p-x".into(),
                name: "X".into(),
                vendor: "openai".into(),
                api_base: "https://api.openai.com/v1".into(),
                models_endpoint: String::new(),
                encrypted_api_key: "sk-x".into(),
                auth_mode: AuthMode::ApiKey,
                encrypted_oauth_meta: String::new(),
                metadata_json: serde_json::json!({}),
                enabled: true,
                created_at: now,
                updated_at: now,
                api_key_cleartext: None,
                oauth_meta_cleartext: None,
            }],
            routes: vec![],
            api_keys: vec![],
            settings: vec![],
            token_daily_stats: vec![],
        };

        let report = store
            .import_config(&bundle, "", &ImportSelection::default())
            .await
            .expect("import");
        assert_eq!(report.providers_imported, 0);
        assert_eq!(report.providers_skipped, 1);
        assert!(store.get_provider("p-x").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn export_then_import_round_trips_plain_settings() {
        let store = boot_store(None).await;
        store
            .set_setting("gateway.test.plain", "hello")
            .await
            .expect("set");

        let bundle = store.export_config().await.expect("export");
        assert_eq!(bundle.settings.len(), 1);
        let s = &bundle.settings[0];
        assert_eq!(s.key, "gateway.test.plain");
        assert_eq!(s.value, "hello");
        assert!(!s.encrypted);

        // Wipe and re-import.
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrate");
        let target = DbConfigStore::new(pool, None);
        target.refresh().await.expect("refresh");
        let selection = ImportSelection {
            settings: vec!["gateway.test.plain".to_string()],
            ..Default::default()
        };
        let report = target
            .import_config(&bundle, "", &selection)
            .await
            .expect("import");
        assert_eq!(report.settings_imported, 1);
        assert_eq!(report.settings_skipped, 0);
        assert_eq!(
            target
                .get_setting("gateway.test.plain")
                .await
                .expect("get")
                .expect("present"),
            "hello"
        );
    }

    #[tokio::test]
    async fn export_then_import_round_trips_encrypted_settings() {
        let source = settings_store().await;
        let key = "gateway.archive.s3_secret_access_key";
        source
            .set_setting_encrypted(key, "super-secret")
            .await
            .expect("encrypt+set");

        let bundle = source.export_config().await.expect("export");
        let s = bundle
            .settings
            .iter()
            .find(|s| s.key == key)
            .expect("encrypted setting present");
        assert!(s.encrypted);
        assert_ne!(s.value, "super-secret");

        // Target with a different master key.
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrate");
        let mut b = [0u8; 32];
        for (i, slot) in b.iter_mut().enumerate() {
            *slot = (i as u8).wrapping_add(99);
        }
        let target_enc = Arc::new(crate::encryption::KeyEncryption::from_bytes(b));
        let target = DbConfigStore::new(pool, Some(target_enc));
        target.refresh().await.expect("refresh");

        // Decrypt with the source master key, re-encrypt with target key.
        let source_key_hex: String = (0..32u8)
            .map(|i| format!("{:02x}", i.wrapping_add(7)))
            .collect();
        let selection = ImportSelection {
            settings: vec![key.to_string()],
            ..Default::default()
        };
        let report = target
            .import_config(&bundle, &source_key_hex, &selection)
            .await
            .expect("import");
        assert_eq!(report.settings_imported, 1);

        let decrypted = target
            .get_setting_encrypted(key)
            .await
            .expect("decrypt")
            .expect("present");
        assert_eq!(decrypted, "super-secret");
    }

    // --- Settings CRUD + encryption tests ---

    async fn settings_store() -> Arc<DbConfigStore> {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrate");
        let mut b = [0u8; 32];
        for (i, slot) in b.iter_mut().enumerate() {
            *slot = (i as u8).wrapping_add(7);
        }
        let enc = Arc::new(crate::encryption::KeyEncryption::from_bytes(b));
        Arc::new(DbConfigStore::new(pool, Some(enc)))
    }

    #[tokio::test]
    async fn list_settings_returns_all_keys() {
        let store = settings_store().await;
        store.set_setting("gateway.test.a", "1").await.expect("set");
        store.set_setting("gateway.test.b", "2").await.expect("set");
        let rows = store.list_settings().await.expect("list");
        let map: std::collections::HashMap<String, String> = rows.into_iter().collect();
        assert_eq!(map.get("gateway.test.a"), Some(&"1".to_string()));
        assert_eq!(map.get("gateway.test.b"), Some(&"2".to_string()));
    }

    #[tokio::test]
    async fn set_setting_bumps_epoch() {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrate");
        let store = Arc::new(DbConfigStore::new(pool, None));
        let before = store.current_epoch().await.expect("epoch");
        store
            .set_setting("gateway.test.key", "value")
            .await
            .expect("set");
        let after = store.current_epoch().await.expect("epoch");
        assert!(after > before, "set_setting must bump the epoch");
    }

    #[tokio::test]
    async fn encrypted_setting_round_trip() {
        let store = settings_store().await;
        let key = "gateway.archive.s3_secret_access_key";
        store
            .set_setting_encrypted(key, "super-secret")
            .await
            .expect("encrypt+set");
        // The raw DB value must NOT be the plaintext.
        let raw = store.get_setting(key).await.expect("get").expect("present");
        assert_ne!(raw, "super-secret", "encrypted value must not be plaintext");
        // Decrypted read returns the original.
        let decrypted = store
            .get_setting_encrypted(key)
            .await
            .expect("decrypt")
            .expect("present");
        assert_eq!(decrypted, "super-secret");
    }

    #[tokio::test]
    async fn encrypted_setting_without_master_key_falls_back_to_cleartext() {
        let pool = crate::db::open_pool("sqlite::memory:").await.expect("pool");
        crate::db::run_migrations(&pool).await.expect("migrate");
        let store = Arc::new(DbConfigStore::new(pool, None));
        let key = "gateway.archive.s3_access_key_id";
        store
            .set_setting_encrypted(key, "AKIATEST")
            .await
            .expect("set");
        // Without a master key, the value is stored as-is.
        let raw = store.get_setting(key).await.expect("get").expect("present");
        assert_eq!(raw, "AKIATEST");
        // Read-back also returns cleartext.
        let val = store
            .get_setting_encrypted(key)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(val, "AKIATEST");
    }

    // --- token_stats export / import tests ---

    use crate::models::ExportTokenDailyStat;

    async fn insert_daily_stat(
        pool: &DbPool,
        day: &str,
        request_count: i64,
        total_tokens: i64,
        peak: i64,
        longest_ms: i64,
    ) {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO token_daily_stats \
                (day, request_count, total_tokens, prompt_tokens, completion_tokens, \
                 reasoning_tokens, peak_single_request, longest_task_ms, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT(day) DO UPDATE SET \
                request_count = excluded.request_count, \
                total_tokens = excluded.total_tokens, \
                prompt_tokens = excluded.prompt_tokens, \
                completion_tokens = excluded.completion_tokens, \
                reasoning_tokens = excluded.reasoning_tokens, \
                peak_single_request = excluded.peak_single_request, \
                longest_task_ms = excluded.longest_task_ms, \
                updated_at = excluded.updated_at",
        )
        .bind(day)
        .bind(request_count)
        .bind(total_tokens)
        .bind(total_tokens / 2)
        .bind(total_tokens / 2)
        .bind(0i64)
        .bind(peak)
        .bind(longest_ms)
        .bind(&now)
        .execute(pool.any())
        .await
        .expect("insert daily stat");
    }

    #[tokio::test]
    async fn export_config_includes_token_daily_stats() {
        let store = boot_store(None).await;
        let today = chrono::Utc::now().date_naive();
        let day_str = today.format("%Y-%m-%d").to_string();
        insert_daily_stat(&store.pool, &day_str, 5, 500, 200, 8000).await;

        let bundle = store.export_config().await.expect("export");
        assert!(
            !bundle.token_daily_stats.is_empty(),
            "export must include token_daily_stats"
        );
        let row = bundle
            .token_daily_stats
            .iter()
            .find(|s| s.day == day_str)
            .expect("today's stat present");
        assert_eq!(row.request_count, 5);
        assert_eq!(row.total_tokens, 500);
        assert_eq!(row.peak_single_request, 200);
        assert_eq!(row.longest_task_ms, 8000);
    }

    #[tokio::test]
    async fn import_token_stats_merges_additively() {
        let store = boot_store(None).await;
        let today = chrono::Utc::now().date_naive();
        let day_str = today.format("%Y-%m-%d").to_string();
        // Pre-populate with 100 tokens for today.
        insert_daily_stat(&store.pool, &day_str, 1, 100, 50, 3000).await;

        // Build a bundle that imports 200 more tokens for the same day.
        let bundle = ConfigExport {
            schema_version: 1,
            exported_at: chrono::Utc::now().to_rfc3339(),
            encrypted: false,
            providers: vec![],
            routes: vec![],
            api_keys: vec![],
            settings: vec![],
            token_daily_stats: vec![ExportTokenDailyStat {
                day: day_str.clone(),
                request_count: 2,
                total_tokens: 200,
                prompt_tokens: 100,
                completion_tokens: 100,
                reasoning_tokens: 0,
                peak_single_request: 150,
                longest_task_ms: 5000,
            }],
        };
        let selection = ImportSelection {
            token_stats: vec![day_str.clone()],
            ..Default::default()
        };
        let report = store
            .import_config(&bundle, "", &selection)
            .await
            .expect("import");
        assert_eq!(report.token_stats_imported, 1);
        assert_eq!(report.token_stats_skipped, 0);

        // Verify additive merge: 100 + 200 = 300 tokens, 1 + 2 = 3 requests.
        let activity = crate::token_stats::get_token_activity(&store.pool, 365)
            .await
            .expect("activity");
        let today_row = activity
            .iter()
            .find(|a| a.day == day_str)
            .expect("today row");
        assert_eq!(today_row.total_tokens, 300);
        assert_eq!(today_row.request_count, 3);

        // Verify peak and longest take MAX: max(50, 150) = 150, max(3000, 5000) = 5000.
        let exported = crate::token_stats::export_token_daily_stats(&store.pool)
            .await
            .expect("export");
        let row = exported
            .iter()
            .find(|s| s.day == day_str)
            .expect("today row");
        assert_eq!(row.peak_single_request, 150);
        assert_eq!(row.longest_task_ms, 5000);
    }

    #[tokio::test]
    async fn import_token_stats_recomputes_summary() {
        let store = boot_store(None).await;
        let today = chrono::Utc::now().date_naive();
        let yesterday = today - chrono::Duration::days(1);
        let today_str = today.format("%Y-%m-%d").to_string();
        let yesterday_str = yesterday.format("%Y-%m-%d").to_string();

        // Import two days of stats.
        let bundle = ConfigExport {
            schema_version: 1,
            exported_at: chrono::Utc::now().to_rfc3339(),
            encrypted: false,
            providers: vec![],
            routes: vec![],
            api_keys: vec![],
            settings: vec![],
            token_daily_stats: vec![
                ExportTokenDailyStat {
                    day: yesterday_str.clone(),
                    request_count: 3,
                    total_tokens: 300,
                    prompt_tokens: 150,
                    completion_tokens: 150,
                    reasoning_tokens: 0,
                    peak_single_request: 200,
                    longest_task_ms: 4000,
                },
                ExportTokenDailyStat {
                    day: today_str.clone(),
                    request_count: 2,
                    total_tokens: 500,
                    prompt_tokens: 250,
                    completion_tokens: 250,
                    reasoning_tokens: 0,
                    peak_single_request: 350,
                    longest_task_ms: 6000,
                },
            ],
        };
        let selection = ImportSelection {
            token_stats: vec![yesterday_str.clone(), today_str.clone()],
            ..Default::default()
        };
        store
            .import_config(&bundle, "", &selection)
            .await
            .expect("import");

        // Verify summary was recomputed.
        let summary = crate::token_stats::get_token_summary(&store.pool)
            .await
            .expect("summary");
        // lifetime = 300 + 500 = 800
        assert_eq!(summary.lifetime_tokens, 800);
        // peak day = max(300, 500) = 500
        assert_eq!(summary.peak_day_tokens, 500);
        // longest task = max(4000, 6000) = 6000
        assert_eq!(summary.longest_task_ms, 6000);
        // current streak = 2 (yesterday + today)
        assert_eq!(summary.current_streak, 2);
        assert_eq!(summary.longest_streak, 2);
    }

    #[tokio::test]
    async fn import_old_bundle_without_token_stats_still_works() {
        let store = boot_store(None).await;
        // A bundle with no token_daily_stats field (simulating an old export).
        // Since the field has #[serde(default)], deserializing a JSON
        // without it yields an empty vec. We test the struct directly.
        let bundle = ConfigExport {
            schema_version: 1,
            exported_at: chrono::Utc::now().to_rfc3339(),
            encrypted: false,
            providers: vec![],
            routes: vec![],
            api_keys: vec![],
            settings: vec![],
            token_daily_stats: vec![],
        };
        let report = store
            .import_config(&bundle, "", &ImportSelection::default())
            .await
            .expect("import");
        assert_eq!(report.token_stats_imported, 0);
        assert_eq!(report.token_stats_skipped, 0);

        // Also verify JSON deserialization of an old-format export.
        let old_json = r#"{
            "schema_version": 1,
            "exported_at": "2024-01-01T00:00:00Z",
            "encrypted": false,
            "providers": [],
            "routes": [],
            "api_keys": []
        }"#;
        let parsed: ConfigExport = serde_json::from_str(old_json).expect("deserialize old");
        assert!(parsed.token_daily_stats.is_empty());
        assert!(parsed.settings.is_empty());
    }
}
