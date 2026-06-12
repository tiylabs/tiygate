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
    /// Handle for the data-plane config-epoch polling task. None
    /// when the control plane is disabled.
    #[allow(dead_code)]
    pub epoch_poll: Option<tiygate_store::retention::EpochPollHandle>,
    /// Handle for the log retention task. None when the control
    /// plane is disabled.
    #[allow(dead_code)]
    pub retention: Option<tiygate_store::retention::RetentionHandle>,
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
            Some(cp) => Arc::new(
                OltpSink::new(cp.pool.clone())
                    .with_payload_max_bytes(server_config.raw_envelope_max_bytes as usize),
            ),
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

        // Spawn the epoch polling task and the log retention
        // task when the control plane is active. The epoch poll
        // rewrites the DbConfigStore's inner ConfigStore on every
        // admin write, so the data plane sees changes within
        // `EpochPollConfig.interval` seconds.
        let (epoch_poll, retention) = match &control_plane {
            Some(cp) => {
                let pool = cp.pool.clone();
                let store = cp.store.clone();
                let retention_cfg = tiygate_store::retention::RetentionConfig::from_env();
                let retention_handle = tiygate_store::retention::spawn(pool, retention_cfg);
                let epoch_handle = tiygate_store::retention::spawn_epoch_poll(
                    store,
                    tiygate_store::retention::EpochPollConfig::from_env(),
                );
                (Some(epoch_handle), Some(retention_handle))
            }
            None => (None, None),
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
                        let admin_state = tiygate_admin::AdminState::new(
                            cp.store.clone(),
                            cp.pool.clone(),
                            Some(self.health.clone()),
                        )
                        .with_quota(self.quota.clone());
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
        router
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
    tiygate_store::db::run_migrations(pool.sqlite()).await?;

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
