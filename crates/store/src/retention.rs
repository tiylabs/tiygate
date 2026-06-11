//! Background retention task — periodically deletes request log
//! rows older than `log_retention_days` (design doc §4.3).
//!
//! The cleanup interval is `log_retention_cleanup_interval` (default
//! 1 hour) and the threshold is `log_retention_days` (default 30
//! days). The task is self-contained: drop the [`RetentionHandle`]
//! and the task exits cleanly.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::db::DbPool;

#[derive(Clone)]
pub struct RetentionConfig {
    /// How often to scan and delete expired rows.
    pub interval: Duration,
    /// Threshold (in days). Rows with `ts < now - threshold` are
    /// deleted. `0` disables the cleanup entirely.
    pub retention_days: u32,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60 * 60),
            retention_days: 30,
        }
    }
}

impl RetentionConfig {
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = std::env::var("TIYGATE_LOG_RETENTION_DAYS") {
            if let Ok(n) = v.parse() {
                c.retention_days = n;
            }
        }
        if let Ok(v) = std::env::var("TIYGATE_LOG_RETENTION_INTERVAL_SECS") {
            if let Ok(n) = v.parse() {
                c.interval = Duration::from_secs(n);
            }
        }
        c
    }
}

/// Handle for the spawned retention task.
pub struct RetentionHandle {
    handle: JoinHandle<()>,
    config: RetentionConfig,
}

impl RetentionHandle {
    /// Stop the background task. Idempotent.
    pub async fn stop(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }

    pub fn config(&self) -> &RetentionConfig {
        &self.config
    }
}

/// Spawn a background task that periodically deletes log rows
/// older than `config.retention_days` from `request_logs`.
pub fn spawn(pool: Arc<DbPool>, config: RetentionConfig) -> RetentionHandle {
    let handle = tokio::spawn(async move {
        if config.retention_days == 0 {
            info!("log retention disabled (log_retention_days = 0)");
            return;
        }
        let mut tick = tokio::time::interval(config.interval);
        // The first tick fires immediately; that's a feature — we
        // want startup-time cleanup for catch-up after a long
        // downtime.
        loop {
            tick.tick().await;
            if let Err(e) = cleanup_once(pool.as_ref(), config.retention_days).await {
                warn!(error = %e, "log retention cleanup failed");
            }
        }
    });
    RetentionHandle { handle, config }
}

/// Run a single cleanup pass. Public so tests can call it without
/// spawning a task. Returns the number of rows deleted.
pub async fn cleanup_once(pool: &DbPool, retention_days: u32) -> Result<u64, sqlx::Error> {
    if retention_days == 0 {
        return Ok(0);
    }
    // Compute the cutoff as ISO-8601 / RFC-3339. `ts` is stored as
    // `chrono::Utc::now().to_rfc3339()` text, so lexicographic
    // comparison works as long as the timezone offset is uniform.
    let cutoff = chrono::Utc::now() - chrono::Duration::days(retention_days as i64);
    let cutoff_str = cutoff.to_rfc3339();
    let res = sqlx::query("DELETE FROM request_logs WHERE ts < ?1")
        .bind(&cutoff_str)
        .execute(pool.sqlite())
        .await?;
    let deleted = res.rows_affected();
    if deleted > 0 {
        info!(deleted, retention_days, "log retention cleanup pass");
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn in_mem_pool() -> Arc<DbPool> {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(pool.sqlite()).await.expect("migrate");
        Arc::new(pool)
    }

    async fn insert_log(pool: &DbPool, request_id: &str, ts: &str) {
        sqlx::query(
            "INSERT INTO request_logs (request_id, ts, virtual_model, ingress_protocol, status) \
             VALUES (?1, ?2, 'm', 'openai/chat-completions/v1', 'ok')",
        )
        .bind(request_id)
        .bind(ts)
        .execute(pool.sqlite())
        .await
        .expect("insert");
    }

    #[tokio::test]
    async fn cleanup_once_deletes_old_rows() {
        let pool = in_mem_pool().await;
        let old_ts = (chrono::Utc::now() - chrono::Duration::days(60)).to_rfc3339();
        let new_ts = chrono::Utc::now().to_rfc3339();
        insert_log(pool.as_ref(), "old", &old_ts).await;
        insert_log(pool.as_ref(), "new", &new_ts).await;

        let deleted = cleanup_once(pool.as_ref(), 30).await.expect("cleanup");
        assert_eq!(deleted, 1, "the old row must be deleted");

        // Verify the new row survives.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_logs WHERE request_id = 'new'")
                .fetch_one(pool.sqlite())
                .await
                .expect("count");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn cleanup_once_disabled_when_zero_days() {
        let pool = in_mem_pool().await;
        let old_ts = (chrono::Utc::now() - chrono::Duration::days(365)).to_rfc3339();
        insert_log(pool.as_ref(), "very-old", &old_ts).await;
        let deleted = cleanup_once(pool.as_ref(), 0).await.expect("cleanup");
        assert_eq!(deleted, 0);
    }

    #[tokio::test]
    async fn spawn_handle_stops_cleanly() {
        let pool = in_mem_pool().await;
        let cfg = RetentionConfig {
            interval: Duration::from_millis(50),
            retention_days: 30,
        };
        let handle = spawn(pool.clone(), cfg);
        tokio::time::sleep(Duration::from_millis(120)).await;
        handle.stop().await;
    }
}

// ---------------------------------------------------------------------
// Epoch polling (design doc §5): data-plane watches the
// `config_epoch` table and refreshes its routing table whenever the
// epoch advances. This is the mechanism by which admin CRUD writes
// propagate to live traffic within seconds.
// ---------------------------------------------------------------------

/// Configuration for the data-plane config-epoch polling loop.
#[derive(Debug, Clone)]
pub struct EpochPollConfig {
    pub interval: Duration,
}

impl Default for EpochPollConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(2),
        }
    }
}

impl EpochPollConfig {
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = std::env::var("TIYGATE_EPOCH_POLL_INTERVAL_SECS") {
            if let Ok(n) = v.parse() {
                c.interval = Duration::from_secs(n);
            }
        }
        c
    }
}

/// Handle for the spawned epoch-polling task.
pub struct EpochPollHandle {
    handle: tokio::task::JoinHandle<()>,
}

impl EpochPollHandle {
    pub async fn stop(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

/// Spawn a background task that polls `config_epoch` and refreshes
/// the in-memory snapshot whenever the epoch advances.
pub fn spawn_epoch_poll(
    store: Arc<crate::config_store::DbConfigStore>,
    config: EpochPollConfig,
) -> EpochPollHandle {
    let handle = tokio::spawn(async move {
        let mut tick = tokio::time::interval(config.interval);
        // Skip the immediate tick — `App::new()` already called
        // `refresh()` once at startup.
        tick.tick().await;
        let mut last_seen: Option<i64> = None;
        loop {
            tick.tick().await;
            let current = match store.current_epoch().await {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(error = %e, "epoch poll: current_epoch failed");
                    continue;
                }
            };
            if Some(current) == last_seen {
                continue;
            }
            if let Err(e) = store.refresh().await {
                tracing::warn!(error = %e, "epoch poll: refresh failed");
                continue;
            }
            tracing::debug!(epoch = current, "epoch poll: applied new config snapshot");
            last_seen = Some(current);
        }
    });
    EpochPollHandle { handle }
}

#[cfg(test)]
mod epoch_tests {
    use super::*;
    use crate::db;

    #[tokio::test]
    async fn epoch_starts_at_zero_after_initial_refresh() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(pool.sqlite()).await.expect("migrate");
        let store = Arc::new(crate::config_store::DbConfigStore::new(pool, None));
        store.refresh().await.expect("initial refresh");
        // The very first refresh bumps the epoch from 0 to 1.
        let epoch = store.current_epoch().await.expect("epoch");
        assert!(epoch >= 1);
    }
}
