//! Gateway application assembly and lifecycle.
//!
//! Phase 4 (产品化) introduces the *control plane* layer:
//! * a sqlx-backed `DbConfigStore` driving the data-plane routing
//!   table on every epoch tick (here: on every startup; §5 epoch
//!   polling is wired via the per-request snapshot handle);
//! * the admin REST router, mounted on the same axum server;
//! * a retention background task for the OLTP log table;
//! * a pluggable telemetry bus — by default an `OltpSink` writing
//!   to `request_logs`; a `StdoutSink` is kept for dev / debugging.

use std::sync::Arc;

use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;
use tracing::warn;

use tiygate_core::EventSink;
use tiygate_store::config_store::DbConfigStore;
use tiygate_store::encryption::KeyEncryption;
use tiygate_store::log_sink::oltp::OltpSink;
use tiygate_store::log_sink::stdout::StdoutSink;

use crate::config::ServerConfig;
use crate::telemetry::ChannelTelemetryBus;

/// The TiyGate application — holds all components.
pub struct App {
    pub config: tiygate_store::config::ConfigStore,
    pub health: Arc<tiygate_core::HealthRegistry>,
    pub server_config: ServerConfig,
    /// Async telemetry bus for pipeline / request events.
    pub telemetry: ChannelTelemetryBus,
    /// Optional control-plane state (DB pool + admin router).
    pub control_plane: Option<ControlPlane>,
    /// Embedding cache (only when the `cache` feature is on and the
    /// cache was constructed).
    #[cfg(feature = "cache")]
    pub embedding_cache: Option<Arc<tiygate_cache::embedding_cache::EmbeddingCache>>,
    /// Quota counter (only when the control plane is active).
    pub quota: Option<Arc<dyn tiygate_core::quota::QuotaCounter>>,
    /// Model catalog snapshot store used to enrich `/v1/models`.
    #[allow(dead_code)]
    pub model_catalog: Option<Arc<tiygate_store::model_catalog::ModelCatalogStore>>,
    /// Handle for the model-catalog background refresh task. None
    /// when the control plane is disabled.
    #[allow(dead_code)]
    pub model_catalog_refresh: Option<tiygate_store::model_catalog::ModelCatalogRefreshHandle>,
    /// Handle for the data-plane config-epoch polling task. None
    /// when the control plane is disabled.
    #[allow(dead_code)]
    pub epoch_poll: Option<tiygate_store::retention::EpochPollHandle>,
    /// Handle for the log retention task. None when the control
    /// plane is disabled.
    #[allow(dead_code)]
    pub retention: Option<tiygate_store::retention::RetentionHandle>,
    /// Handle for the token-stats aggregation task. None when the
    /// control plane is disabled.
    #[allow(dead_code)]
    pub token_stats: Option<tiygate_store::token_stats::TokenStatsHandle>,
    /// Handle for SQLite local maintenance. None when the control
    /// plane is disabled or the pool is not SQLite.
    #[allow(dead_code)]
    pub sqlite_maintenance: Option<tiygate_store::sqlite_maintenance::SqliteMaintenanceHandle>,
    /// Handle for the payload archive background task. None when
    /// archiving is disabled or the control plane is absent.
    #[allow(dead_code)]
    pub payload_archive: Option<tiygate_store::archive::PayloadArchiveHandle>,
    /// Shared payload archive client for worker uploads and admin
    /// replay reads.
    pub payload_archive_client: Option<Arc<dyn tiygate_store::archive::PayloadArchiveClient>>,
}

/// State attached to the control plane — held so the binary can
/// spawn the retention task alongside the server.
pub struct ControlPlane {
    pub pool: Arc<tiygate_store::db::DbPool>,
    pub store: Arc<DbConfigStore>,
    #[allow(dead_code)]
    pub encryption: Option<Arc<KeyEncryption>>,
}

impl App {
    pub async fn new() -> anyhow::Result<Self> {
        let server_config = ServerConfig::from_env();
        let health = Arc::new(tiygate_core::HealthRegistry::with_defaults());

        // -- Control plane (DB + admin) --
        let control_plane = boot_control_plane(&server_config).await?;
        let config = match &control_plane {
            Some(cp) => cp.store.config_store(),
            None => {
                // Legacy path: env-derived routes (Phase 1-3
                // behaviour). The data plane reads
                // `state.config.routing_table` directly, so this
                // shape stays compatible.
                tiygate_store::config::ConfigStore::from_env()
            }
        };

        // -- Telemetry bus --
        // The OltpSink requires the control-plane DB. When the
        // control plane is absent we fall back to the stdout sink
        // so the data plane still emits useful structured logs.
        let sink: Arc<dyn EventSink> = match &control_plane {
            Some(cp) => Arc::new(OltpSink::new(cp.pool.clone())),
            None => Arc::new(StdoutSink::new()),
        };
        let telemetry =
            ChannelTelemetryBus::spawn(sink, crate::telemetry::DEFAULT_TELEMETRY_CHANNEL_CAPACITY);

        // -- Quota counter --
        // Prefer the Redis-backed counter when the operator has set
        // `TIYGATE_REDIS_URL` (and the server was built with the
        // `redis-quota` feature). `RedisQuota::new` already falls
        // back to `InMemoryQuota` on connection failure, so the
        // data plane never sees a backend error from the quota
        // check. We hand the inner counter (not the wrapper) to
        // `AppState` to keep the hot path a single trait dispatch.
        let quota: Option<Arc<dyn tiygate_core::quota::QuotaCounter>> =
            control_plane.as_ref().map(|_| {
                let cfg = tiygate_core::quota::RedisQuotaConfig::from_env();
                if cfg.url.is_some() {
                    tracing::info!("quota: using RedisQuota (url configured)");
                    tiygate_core::quota::RedisQuota::new(cfg).into_inner()
                } else {
                    tracing::info!("quota: using InMemoryQuota (no TIYGATE_REDIS_URL)");
                    tiygate_core::quota::InMemoryQuota::new()
                        as Arc<dyn tiygate_core::quota::QuotaCounter>
                }
            });

        // -- Embedding cache --
        #[cfg(feature = "cache")]
        let embedding_cache = Some(tiygate_cache::embedding_cache::EmbeddingCache::new());

        tracing::info!(
            routes = config.routing_table.routes.len(),
            control_plane = control_plane.is_some(),
            "TiyGate initialized",
        );

        let (payload_archive_client, payload_archive_handle) = match &control_plane {
            Some(cp) => {
                // Bootstrap settings from env on first start, then
                // spawn the settings-driven archive worker.
                bootstrap_settings(&cp.store, &server_config).await;
                let handle =
                    tiygate_store::archive::spawn_from_store(cp.pool.clone(), cp.store.clone());
                // Build a one-time S3 client for the admin replay
                // path. The worker owns its own internally-rebuilt
                // client; this client is used only by the admin
                // replay handler to fetch archived payloads. If the
                // archive config is incomplete or disabled the
                // client is `None` and replay falls back to the DB.
                let replay_client = build_admin_archive_client(&cp.store).await;
                (replay_client, Some(handle))
            }
            None => (None, None),
        };

        let model_catalog = Some(tiygate_store::model_catalog::ModelCatalogStore::load_embedded()?);
        let model_catalog_refresh = match (&control_plane, &model_catalog) {
            (Some(_), Some(store)) => Some(store.spawn_refresh()),
            _ => None,
        };
        // task when the control plane is active. The epoch poll
        // rewrites the DbConfigStore's inner ConfigStore on every
        // admin write, so the data plane sees changes within
        // the configured poll interval seconds.
        let (epoch_poll, retention, token_stats, sqlite_maintenance) = match &control_plane {
            Some(cp) => {
                let pool = cp.pool.clone();
                let store = cp.store.clone();
                let retention_handle = tiygate_store::retention::spawn(pool.clone(), store.clone());
                let epoch_handle = tiygate_store::retention::spawn_epoch_poll(store.clone());
                let sqlite_maintenance_handle = if pool.kind() == tiygate_store::db::DbKind::Sqlite
                {
                    Some(tiygate_store::sqlite_maintenance::spawn(
                        pool.clone(),
                        store.clone(),
                    ))
                } else {
                    None
                };
                let token_stats_handle = tiygate_store::token_stats::spawn(pool, store);
                (
                    Some(epoch_handle),
                    Some(retention_handle),
                    Some(token_stats_handle),
                    sqlite_maintenance_handle,
                )
            }
            None => (None, None, None, None),
        };

        Ok(Self {
            config,
            health,
            server_config,
            telemetry,
            control_plane,
            #[cfg(feature = "cache")]
            embedding_cache,
            quota,
            epoch_poll,
            retention,
            token_stats,
            sqlite_maintenance,
            payload_archive: payload_archive_handle,
            payload_archive_client,
            model_catalog,
            model_catalog_refresh,
        })
    }

    pub fn control_plane(&self) -> Option<&ControlPlane> {
        self.control_plane.as_ref()
    }

    pub fn router(&self) -> Router {
        let db_store = self.control_plane.as_ref().map(|cp| cp.store.clone());
        // `router` is reassigned when the admin and/or webui routers
        // are merged in. In the rare build combo where neither the
        // `admin` nor `webui` feature is enabled, those reassignments
        // are compiled out, so silence the spurious unused_mut.
        #[allow(unused_mut)]
        let mut router = crate::ingress::router_with_telemetry_full(
            self.config.clone(),
            self.health.clone(),
            &self.server_config,
            Arc::new(self.telemetry.clone()),
            self.quota.clone(),
            #[cfg(feature = "cache")]
            self.embedding_cache.clone(),
            #[cfg(not(feature = "cache"))]
            None,
            db_store,
            self.model_catalog.clone(),
        );
        if let Some(cp) = &self.control_plane {
            // Mount the admin router under `/admin`. Phase 4 splits
            // the surface: in `proxy` mode we omit the admin routes
            // (data plane only); in `admin` mode we mount only the
            // admin routes; in `all` we mount both.
            match self.server_config.mode {
                crate::config::DeployMode::All | crate::config::DeployMode::Admin => {
                    #[cfg(feature = "admin")]
                    {
                        let bf_config = tiygate_admin::BruteForceConfig::from_env();
                        let bf_limiter = tiygate_admin::build_limiter(&bf_config);
                        let admin_state = tiygate_admin::AdminState::new(
                            cp.store.clone(),
                            cp.pool.clone(),
                            Some(self.health.clone()),
                        )
                        .with_quota(self.quota.clone())
                        .with_payload_archive(self.payload_archive_client.clone())
                        .with_model_catalog(self.model_catalog.clone())
                        .with_bf_limiter(bf_limiter);
                        let admin = tiygate_admin::build_router(admin_state);
                        router = router.merge(admin);
                    }
                    #[cfg(not(feature = "admin"))]
                    {
                        let _ = cp;
                        info!("admin feature disabled: admin router not mounted");
                    }
                }
                crate::config::DeployMode::Proxy => {
                    info!("proxy mode: admin router not mounted");
                }
            }

            // Mount the embedded WebUI SPA under `/admin/ui` in the
            // same modes that expose the admin API. The routes own
            // their own SPA fallback scoped to `/admin/ui`, so they
            // never intercept data-plane (`/v1/*`) or admin API
            // (`/admin/v1/*`) routes.
            #[cfg(feature = "webui")]
            if matches!(
                self.server_config.mode,
                crate::config::DeployMode::All | crate::config::DeployMode::Admin
            ) {
                router = crate::webui::mount(router);
            }
        }
        // The embedding cache is reachable from ingress via the
        // `AppState` in `router_with_telemetry`; nothing to wire
        // here at the router level.

        // Apply permissive CORS so the Tauri desktop client (whose
        // webview origin is `tauri://localhost`) can make cross-origin
        // fetch calls to the sidecar's admin API. In standard server
        // deployments behind a reverse proxy this is harmless because
        // the proxy typically handles CORS, and the admin API is
        // already token-gated.
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
            .expose_headers(Any);

        router.layer(cors)
    }
}

/// Seed the `settings` table from the env-derived `ServerConfig`
/// on first start. For each migratable key, if the setting is
/// absent in the DB the env value is written so the gateway
/// starts with the operator's existing configuration. Subsequent
/// runtime changes go through the admin API and persist in the
/// `settings` table; env changes are only re-read if the table is
/// cleared.
///
/// Sensitive S3 credentials are written through
/// [`DbConfigStore::set_setting_encrypted`] so they are encrypted
/// at rest with the master key.
async fn bootstrap_settings(store: &Arc<DbConfigStore>, cfg: &ServerConfig) {
    use tiygate_store::settings_keys as sk;

    // Retention
    let _ = ensure_setting(
        store,
        sk::RETENTION_INTERVAL_SECS,
        &cfg_retention_interval_secs().to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::RETENTION_LOG_RETENTION_DAYS,
        &env_or("TIYGATE_LOG_RETENTION_DAYS", "30"),
    )
    .await;

    // SQLite maintenance
    let _ = ensure_setting(
        store,
        sk::SQLITE_MAINTENANCE_ENABLED,
        &env_or("TIYGATE_SQLITE_MAINTENANCE_ENABLED", "false"),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::SQLITE_MAINTENANCE_INTERVAL_SECS,
        &env_or("TIYGATE_SQLITE_MAINTENANCE_INTERVAL_SECS", "86400"),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::SQLITE_MAINTENANCE_VACUUM_ENABLED,
        &env_or("TIYGATE_SQLITE_MAINTENANCE_VACUUM_ENABLED", "false"),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::SQLITE_MAINTENANCE_MIN_FREELIST_PAGES,
        &env_or("TIYGATE_SQLITE_MAINTENANCE_MIN_FREELIST_PAGES", "1024"),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::SQLITE_MAINTENANCE_MIN_FREE_RATIO_PERCENT,
        &env_or("TIYGATE_SQLITE_MAINTENANCE_MIN_FREE_RATIO_PERCENT", "20"),
    )
    .await;

    // Epoch poll
    let _ = ensure_setting(
        store,
        sk::EPOCH_POLL_INTERVAL_SECS,
        &env_or("TIYGATE_EPOCH_POLL_INTERVAL_SECS", "2"),
    )
    .await;

    // Token stats
    let _ = ensure_setting(
        store,
        sk::TOKEN_STATS_INTERVAL_SECS,
        &env_or("TIYGATE_TOKEN_STATS_INTERVAL_SECS", "300"),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::TOKEN_STATS_LOOKBACK_DAYS,
        &env_or("TIYGATE_TOKEN_STATS_LOOKBACK_DAYS", "400"),
    )
    .await;

    // Payload archive
    let archive = &cfg.payload_archive;
    let _ = ensure_setting(store, sk::ARCHIVE_ENABLED, &archive.enabled.to_string()).await;
    let _ = ensure_setting_opt(store, sk::ARCHIVE_S3_ENDPOINT, &archive.s3_endpoint).await;
    let _ = ensure_setting(store, sk::ARCHIVE_S3_REGION, &archive.s3_region).await;
    let _ = ensure_setting_opt(store, sk::ARCHIVE_S3_BUCKET, &archive.s3_bucket).await;
    let _ = ensure_setting(store, sk::ARCHIVE_S3_PREFIX, &archive.s3_prefix).await;
    let _ = ensure_setting(
        store,
        sk::ARCHIVE_S3_FORCE_PATH_STYLE,
        &archive.s3_force_path_style.to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::ARCHIVE_SCAN_INTERVAL_SECS,
        &archive.scan_interval_secs.to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::ARCHIVE_BATCH_SIZE,
        &archive.batch_size.to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::ARCHIVE_CONCURRENCY,
        &archive.concurrency.to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::ARCHIVE_TIMEOUT_SECS,
        &archive.timeout_secs.to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::ARCHIVE_MAX_RETRIES,
        &archive.max_retries.to_string(),
    )
    .await;
    // Encrypted S3 credentials — only seed when present in env.
    if let Some(ak) = &archive.s3_access_key_id {
        if store
            .get_setting(sk::ARCHIVE_S3_ACCESS_KEY_ID)
            .await
            .ok()
            .flatten()
            .is_none()
        {
            let _ = store
                .set_setting_encrypted(sk::ARCHIVE_S3_ACCESS_KEY_ID, ak)
                .await;
        }
    }
    if let Some(sk_key) = &archive.s3_secret_access_key {
        if store
            .get_setting(sk::ARCHIVE_S3_SECRET_ACCESS_KEY)
            .await
            .ok()
            .flatten()
            .is_none()
        {
            let _ = store
                .set_setting_encrypted(sk::ARCHIVE_S3_SECRET_ACCESS_KEY, sk_key)
                .await;
        }
    }

    // Routing
    let _ = ensure_setting(
        store,
        sk::ROUTING_DEFAULT_STRATEGY,
        cfg.routing_strategy.as_str(),
    )
    .await;

    // Ingress
    let _ = ensure_setting(
        store,
        sk::INGRESS_MAX_BODY_BYTES,
        &cfg.max_request_body_bytes.to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::INGRESS_MAX_INFLIGHT,
        &cfg.max_inflight_requests.to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::INGRESS_MAX_QUEUE_DEPTH,
        &cfg.max_queue_depth.to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::INGRESS_ACQUIRE_TIMEOUT_SECS,
        &cfg.acquire_timeout_secs.to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::INGRESS_RAW_ENVELOPE_CAPTURE_MEDIA,
        &cfg.raw_envelope_capture_media.to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::INGRESS_REQUIRE_API_KEY,
        &cfg.require_api_key.to_string(),
    )
    .await;

    // Upstream
    let _ = ensure_setting(
        store,
        sk::UPSTREAM_STREAM_IDLE_TIMEOUT_SECS,
        &cfg.upstream_stream_idle_timeout_secs.to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::UPSTREAM_STREAM_TOTAL_TIMEOUT_SECS,
        &cfg.upstream_stream_total_timeout_secs.to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::UPSTREAM_TTFB_TIMEOUT_SECS,
        &cfg.upstream_ttfb_timeout_secs.to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::UPSTREAM_TCP_KEEPALIVE_SECS,
        &cfg.upstream_tcp_keepalive_secs.to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::UPSTREAM_POOL_IDLE_TIMEOUT_SECS,
        &cfg.upstream_pool_idle_timeout_secs.to_string(),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::UPSTREAM_TCP_NODELAY,
        &cfg.upstream_tcp_nodelay.to_string(),
    )
    .await;

    // Forward header deny lists
    let _ = ensure_setting(
        store,
        sk::FORWARD_REQUEST_HEADER_DENY,
        &cfg.forward_request_header_deny_extra.join(","),
    )
    .await;
    let _ = ensure_setting(
        store,
        sk::FORWARD_RESPONSE_HEADER_DENY,
        &cfg.forward_response_header_deny_extra.join(","),
    )
    .await;

    tracing::info!("settings table bootstrapped from env defaults");
}

/// Write `value` to `key` only when the setting is currently
/// absent. Existing settings are never overwritten by bootstrap.
async fn ensure_setting(store: &DbConfigStore, key: &str, value: &str) {
    if store.get_setting(key).await.ok().flatten().is_none() {
        let _ = store.set_setting(key, value).await;
    }
}

/// Like [`ensure_setting`] but for optional values — an empty
/// `None` writes an empty string so the key exists.
async fn ensure_setting_opt(store: &DbConfigStore, key: &str, value: &Option<String>) {
    if store.get_setting(key).await.ok().flatten().is_none() {
        let _ = store.set_setting(key, value.as_deref().unwrap_or("")).await;
    }
}

/// Read an env var or return the default.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Compute the retention interval from env, mirroring the legacy
/// `RetentionConfig::from_env` default logic.
fn cfg_retention_interval_secs() -> u64 {
    env_or("TIYGATE_LOG_RETENTION_INTERVAL_SECS", "3600")
        .parse()
        .unwrap_or(3600)
}

/// Build a one-time `S3ArchiveClient` from the current settings
/// table for the admin replay path. Returns `None` when archiving is
/// disabled or the S3 config is incomplete. The worker owns its own
/// internally-rebuilt client for uploads; this client is only used
/// by `hydrate_archived_replay` to fetch already-archived payloads.
async fn build_admin_archive_client(
    store: &Arc<DbConfigStore>,
) -> Option<Arc<dyn tiygate_store::archive::PayloadArchiveClient>> {
    use tiygate_store::settings_keys as sk;
    let enabled = sk::get_bool(store.as_ref(), sk::ARCHIVE_ENABLED, false).await;
    if !enabled {
        return None;
    }
    let endpoint = sk::get_opt_string(store.as_ref(), sk::ARCHIVE_S3_ENDPOINT).await?;
    let bucket = sk::get_opt_string(store.as_ref(), sk::ARCHIVE_S3_BUCKET).await?;
    let access_key_id = store
        .get_setting_encrypted(sk::ARCHIVE_S3_ACCESS_KEY_ID)
        .await
        .ok()
        .flatten()?;
    let secret_access_key = store
        .get_setting_encrypted(sk::ARCHIVE_S3_SECRET_ACCESS_KEY)
        .await
        .ok()
        .flatten()?;
    let region = sk::get_string(store.as_ref(), sk::ARCHIVE_S3_REGION, "us-east-1").await;
    let prefix = sk::get_string(store.as_ref(), sk::ARCHIVE_S3_PREFIX, "").await;
    let force_path_style =
        sk::get_bool(store.as_ref(), sk::ARCHIVE_S3_FORCE_PATH_STYLE, true).await;
    let timeout_secs = sk::get_u64(store.as_ref(), sk::ARCHIVE_TIMEOUT_SECS, 30).await;
    match tiygate_store::archive::S3ArchiveClient::new(
        endpoint,
        region,
        bucket,
        tiygate_store::archive::normalize_prefix(&prefix),
        force_path_style,
        access_key_id,
        secret_access_key,
        timeout_secs,
    ) {
        Ok(c) => Some(Arc::new(c) as Arc<dyn tiygate_store::archive::PayloadArchiveClient>),
        Err(e) => {
            warn!(error = %e, "failed to build admin archive replay client");
            None
        }
    }
}

/// Open the DB pool (if `TIYGATE_DATABASE_URL` is set), run
/// migrations, build the `DbConfigStore`, and spawn the retention
/// background task. Returns `None` in the legacy in-memory path.
async fn boot_control_plane(cfg: &ServerConfig) -> anyhow::Result<Option<ControlPlane>> {
    let Some(database_url) = cfg.database_url.as_ref() else {
        return Ok(None);
    };
    let pool = Arc::new(tiygate_store::db::open_pool(database_url).await?);
    tiygate_store::db::run_migrations(&pool).await?;

    // Build (or read) the master key. We log a warning when the
    // key is missing so operators see a clear signal in their
    // start-up logs.
    let encryption = match tiygate_store::encryption::KeyEncryption::from_env() {
        Some(Ok(k)) => Some(Arc::new(k)),
        Some(Err(e)) => {
            warn!(error = %e, "TIYGATE_MASTER_KEY present but invalid; provider secrets will be stored in cleartext");
            None
        }
        None => {
            warn!("TIYGATE_MASTER_KEY not set; provider secrets will be stored in cleartext (NOT FOR PRODUCTION)");
            None
        }
    };
    let store = Arc::new(DbConfigStore::new((*pool).clone(), encryption.clone()));
    store.refresh().await?;

    info!(
        database_url = %database_url,
        "control plane initialised",
    );

    // The retention task and the epoch polling task are spawned
    // by `App::new()` (see below) so that they are owned by the
    // `App` and can be aborted on graceful shutdown. Do not spawn
    // them here.
    Ok(Some(ControlPlane {
        pool,
        store,
        encryption,
    }))
}
