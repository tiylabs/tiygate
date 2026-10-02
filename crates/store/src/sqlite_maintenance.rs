//! SQLite-only local database maintenance.
//!
//! Deleting request logs and payloads releases pages into SQLite's
//! freelist, but it does not shrink the database file. This task keeps
//! local SQLite instances healthy by periodically checkpointing WAL,
//! running `PRAGMA optimize`, and optionally running `VACUUM` when the
//! freelist crosses configured thresholds.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config_store::DbConfigStore;
use crate::db::{DbKind, DbPool};
use crate::settings_keys;

const DEFAULT_INTERVAL_SECS: u64 = 24 * 60 * 60;
const DEFAULT_MIN_FREELIST_PAGES: u64 = 1024;
const DEFAULT_MIN_FREE_RATIO_PERCENT: u64 = 20;

#[derive(Debug, Clone)]
pub struct SqliteMaintenanceConfig {
    /// Whether the maintenance loop should execute maintenance passes.
    pub enabled: bool,
    /// How often the background loop wakes up.
    pub interval: Duration,
    /// Whether `VACUUM` is allowed after low-risk maintenance.
    pub vacuum_enabled: bool,
    /// Minimum freelist pages required before `VACUUM` is considered.
    pub min_freelist_pages: u64,
    /// Minimum freelist/page_count ratio, in percent, before `VACUUM` is considered.
    pub min_free_ratio_percent: u64,
}

impl Default for SqliteMaintenanceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECS),
            vacuum_enabled: false,
            min_freelist_pages: DEFAULT_MIN_FREELIST_PAGES,
            min_free_ratio_percent: DEFAULT_MIN_FREE_RATIO_PERCENT,
        }
    }
}

impl SqliteMaintenanceConfig {
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = std::env::var("TIYGATE_SQLITE_MAINTENANCE_ENABLED") {
            if let Ok(n) = v.parse() {
                c.enabled = n;
            }
        }
        if let Ok(v) = std::env::var("TIYGATE_SQLITE_MAINTENANCE_INTERVAL_SECS") {
            if let Ok(n) = v.parse() {
                c.interval = Duration::from_secs(n);
            }
        }
        if let Ok(v) = std::env::var("TIYGATE_SQLITE_MAINTENANCE_VACUUM_ENABLED") {
            if let Ok(n) = v.parse() {
                c.vacuum_enabled = n;
            }
        }
        if let Ok(v) = std::env::var("TIYGATE_SQLITE_MAINTENANCE_MIN_FREELIST_PAGES") {
            if let Ok(n) = v.parse() {
                c.min_freelist_pages = n;
            }
        }
        if let Ok(v) = std::env::var("TIYGATE_SQLITE_MAINTENANCE_MIN_FREE_RATIO_PERCENT") {
            if let Ok(n) = v.parse() {
                c.min_free_ratio_percent = n;
            }
        }
        c
    }

    async fn from_store(store: &DbConfigStore, fallback: &Self) -> Self {
        Self {
            enabled: settings_keys::get_bool(
                store,
                settings_keys::SQLITE_MAINTENANCE_ENABLED,
                fallback.enabled,
            )
            .await,
            interval: Duration::from_secs(
                settings_keys::get_u64(
                    store,
                    settings_keys::SQLITE_MAINTENANCE_INTERVAL_SECS,
                    fallback.interval.as_secs(),
                )
                .await,
            ),
            vacuum_enabled: settings_keys::get_bool(
                store,
                settings_keys::SQLITE_MAINTENANCE_VACUUM_ENABLED,
                fallback.vacuum_enabled,
            )
            .await,
            min_freelist_pages: settings_keys::get_u64(
                store,
                settings_keys::SQLITE_MAINTENANCE_MIN_FREELIST_PAGES,
                fallback.min_freelist_pages,
            )
            .await,
            min_free_ratio_percent: settings_keys::get_u64(
                store,
                settings_keys::SQLITE_MAINTENANCE_MIN_FREE_RATIO_PERCENT,
                fallback.min_free_ratio_percent,
            )
            .await,
        }
    }
}

/// Result details for one SQLite maintenance pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SqliteMaintenanceReport {
    pub skipped_disabled: bool,
    pub skipped_non_sqlite: bool,
    pub wal_checkpointed: bool,
    pub optimized: bool,
    pub vacuumed: bool,
    pub freelist_pages: u64,
    pub page_count: u64,
    pub page_size: u64,
    pub free_ratio_percent: u64,
    pub reclaimable_bytes: u64,
}

/// Handle for the spawned SQLite maintenance task.
pub struct SqliteMaintenanceHandle {
    handle: JoinHandle<()>,
}

impl SqliteMaintenanceHandle {
    /// Stop the background task. Idempotent.
    pub async fn stop(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

/// Spawn a settings-driven SQLite maintenance loop.
///
/// The loop exits immediately for non-SQLite pools. Runtime settings
/// are read on each iteration so operators can enable, disable, or
/// retune the task without restarting the gateway.
pub fn spawn(pool: Arc<DbPool>, store: Arc<DbConfigStore>) -> SqliteMaintenanceHandle {
    let fallback = SqliteMaintenanceConfig::from_env();
    let handle = tokio::spawn(async move {
        if pool.kind() != DbKind::Sqlite {
            debug!("sqlite maintenance task skipped for non-SQLite pool");
            return;
        }

        info!(
            interval_secs = fallback.interval.as_secs(),
            vacuum_enabled = fallback.vacuum_enabled,
            min_freelist_pages = fallback.min_freelist_pages,
            min_free_ratio_percent = fallback.min_free_ratio_percent,
            "sqlite maintenance task started (defaults; runtime values come from settings)"
        );

        loop {
            let cfg = SqliteMaintenanceConfig::from_store(store.as_ref(), &fallback).await;
            let interval_secs = cfg.interval.as_secs().max(1);
            if !cfg.enabled {
                debug!("sqlite maintenance disabled");
                tokio::time::sleep(Duration::from_secs(interval_secs)).await;
                continue;
            }

            match run_once(pool.as_ref(), &cfg).await {
                Ok(report) => {
                    if report.vacuumed {
                        info!(
                            freelist_pages = report.freelist_pages,
                            page_count = report.page_count,
                            page_size = report.page_size,
                            reclaimable_bytes = report.reclaimable_bytes,
                            "sqlite maintenance vacuum completed"
                        );
                    } else {
                        debug!(
                            freelist_pages = report.freelist_pages,
                            page_count = report.page_count,
                            page_size = report.page_size,
                            free_ratio_percent = report.free_ratio_percent,
                            reclaimable_bytes = report.reclaimable_bytes,
                            "sqlite maintenance pass completed"
                        );
                    }
                }
                Err(e) => warn!(error = %e, "sqlite maintenance pass failed"),
            }

            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
        }
    });
    SqliteMaintenanceHandle { handle }
}

/// Run a single maintenance pass. Public so tests can call it without
/// spawning a background task.
pub async fn run_once(
    pool: &DbPool,
    cfg: &SqliteMaintenanceConfig,
) -> Result<SqliteMaintenanceReport, sqlx::Error> {
    let mut report = SqliteMaintenanceReport::default();
    if !cfg.enabled {
        report.skipped_disabled = true;
        return Ok(report);
    }
    if pool.kind() != DbKind::Sqlite {
        report.skipped_non_sqlite = true;
        return Ok(report);
    }

    sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(pool.any())
        .await?;
    report.wal_checkpointed = true;

    sqlx::query("PRAGMA optimize").execute(pool.any()).await?;
    report.optimized = true;

    report.freelist_pages = read_pragma_u64(pool, "PRAGMA freelist_count").await?;
    report.page_count = read_pragma_u64(pool, "PRAGMA page_count").await?;
    report.page_size = read_pragma_u64(pool, "PRAGMA page_size").await?;
    report.free_ratio_percent = report
        .freelist_pages
        .saturating_mul(100)
        .checked_div(report.page_count)
        .unwrap_or(0);
    report.reclaimable_bytes = report.freelist_pages.saturating_mul(report.page_size);

    if should_vacuum(cfg, &report) {
        sqlx::query("VACUUM").execute(pool.any()).await?;
        report.vacuumed = true;
    }

    Ok(report)
}

fn should_vacuum(cfg: &SqliteMaintenanceConfig, report: &SqliteMaintenanceReport) -> bool {
    if !cfg.vacuum_enabled || report.freelist_pages == 0 {
        return false;
    }
    report.freelist_pages >= cfg.min_freelist_pages
        || report.free_ratio_percent >= cfg.min_free_ratio_percent
}

async fn read_pragma_u64(pool: &DbPool, sql: &str) -> Result<u64, sqlx::Error> {
    let value: i64 = sqlx::query_scalar(sql).fetch_one(pool.any()).await?;
    Ok(value.max(0) as u64)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::db;

    async fn in_mem_pool() -> Arc<DbPool> {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(&pool).await.expect("migrate");
        Arc::new(pool)
    }

    fn enabled_config() -> SqliteMaintenanceConfig {
        SqliteMaintenanceConfig {
            enabled: true,
            ..SqliteMaintenanceConfig::default()
        }
    }

    #[tokio::test]
    async fn run_once_skips_when_disabled() {
        let pool = in_mem_pool().await;
        let report = run_once(pool.as_ref(), &SqliteMaintenanceConfig::default())
            .await
            .expect("run maintenance");
        assert!(report.skipped_disabled);
        assert!(!report.wal_checkpointed);
        assert!(!report.optimized);
        assert!(!report.vacuumed);
    }

    #[tokio::test]
    async fn run_once_checkpoints_and_optimizes_sqlite() {
        let pool = in_mem_pool().await;
        let report = run_once(pool.as_ref(), &enabled_config())
            .await
            .expect("run maintenance");
        assert!(report.wal_checkpointed);
        assert!(report.optimized);
        assert!(report.page_count > 0);
        assert!(report.page_size > 0);
        assert!(!report.vacuumed);
    }

    #[tokio::test]
    async fn run_once_vacuums_when_threshold_is_met() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("maintenance.db");
        let url = format!("sqlite:{}", db_path.display());
        let pool = db::open_pool(&url).await.expect("pool");

        sqlx::query("CREATE TABLE blobs (id INTEGER PRIMARY KEY, body TEXT NOT NULL)")
            .execute(pool.any())
            .await
            .expect("create blobs");
        let body = "x".repeat(16 * 1024);
        for _ in 0..128 {
            sqlx::query("INSERT INTO blobs (body) VALUES ($1)")
                .bind(&body)
                .execute(pool.any())
                .await
                .expect("insert blob");
        }
        sqlx::query("DELETE FROM blobs")
            .execute(pool.any())
            .await
            .expect("delete blobs");

        let cfg = SqliteMaintenanceConfig {
            enabled: true,
            vacuum_enabled: true,
            min_freelist_pages: 1,
            min_free_ratio_percent: 1,
            ..SqliteMaintenanceConfig::default()
        };
        let report = run_once(&pool, &cfg).await.expect("run maintenance");
        assert!(
            report.freelist_pages > 0,
            "test setup should create free pages"
        );
        assert!(report.vacuumed);
    }

    #[tokio::test]
    async fn run_once_does_not_vacuum_below_threshold() {
        let pool = in_mem_pool().await;
        let cfg = SqliteMaintenanceConfig {
            enabled: true,
            vacuum_enabled: true,
            min_freelist_pages: u64::MAX,
            min_free_ratio_percent: u64::MAX,
            ..SqliteMaintenanceConfig::default()
        };
        let report = run_once(pool.as_ref(), &cfg)
            .await
            .expect("run maintenance");
        assert!(!report.vacuumed);
    }

    #[tokio::test]
    async fn spawn_handle_stops_cleanly() {
        let pool = in_mem_pool().await;
        let store = Arc::new(crate::config_store::DbConfigStore::new(
            (*pool).clone(),
            None,
        ));
        store
            .set_setting(settings_keys::SQLITE_MAINTENANCE_ENABLED, "false")
            .await
            .expect("set enabled");
        store
            .set_setting(settings_keys::SQLITE_MAINTENANCE_INTERVAL_SECS, "1")
            .await
            .expect("set interval");

        let handle = spawn(pool, store);
        tokio::time::sleep(Duration::from_millis(120)).await;
        handle.stop().await;
    }
}
