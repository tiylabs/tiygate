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

use crate::config_store::DbConfigStore;
use crate::db::DbPool;
use crate::settings_keys;

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
}

impl RetentionHandle {
    /// Stop the background task. Idempotent.
    pub async fn stop(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

/// Spawn a background task that periodically deletes log rows
/// older than the configured retention threshold. Both the interval
/// and the retention-days threshold are read from the `settings`
/// table on every loop iteration, so an operator can change them
/// at runtime through the admin API without restarting the gateway.
///
/// The env-derived [`RetentionConfig`] is used only as the fallback
/// default when a setting is absent (e.g. before bootstrap has run).
pub fn spawn(pool: Arc<DbPool>, store: Arc<DbConfigStore>) -> RetentionHandle {
    let fallback = RetentionConfig::from_env();
    let handle = tokio::spawn(async move {
        loop {
            // Read the current retention threshold each iteration.
            let retention_days = settings_keys::get_u64(
                store.as_ref(),
                settings_keys::RETENTION_LOG_RETENTION_DAYS,
                fallback.retention_days as u64,
            )
            .await as u32;
            let interval_secs = settings_keys::get_u64(
                store.as_ref(),
                settings_keys::RETENTION_INTERVAL_SECS,
                fallback.interval.as_secs(),
            )
            .await;
            if retention_days == 0 {
                info!("log retention disabled (log_retention_days = 0)");
                // Keep polling so a future settings change can
                // re-enable cleanup without a restart.
                tokio::time::sleep(Duration::from_secs(interval_secs.max(1))).await;
                continue;
            }
            if let Err(e) = cleanup_once(pool.as_ref(), retention_days).await {
                warn!(error = %e, "log retention cleanup failed");
            }
            tokio::time::sleep(Duration::from_secs(interval_secs.max(1))).await;
        }
    });
    RetentionHandle { handle }
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
    let mut tx = pool.any().begin().await?;
    let attempt_res = sqlx::query(
        "DELETE FROM request_attempts \
         WHERE request_id IN (SELECT request_id FROM request_logs WHERE ts < $1) \
            OR ts < $1",
    )
    .bind(&cutoff_str)
    .execute(&mut *tx)
    .await?;
    let payload_res = sqlx::query(
        "DELETE FROM request_payloads \
         WHERE request_id IN (SELECT request_id FROM request_logs WHERE ts < $1) \
            OR captured_at < $1",
    )
    .bind(&cutoff_str)
    .execute(&mut *tx)
    .await?;
    let log_res = sqlx::query("DELETE FROM request_logs WHERE ts < $1")
        .bind(&cutoff_str)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let deleted_attempts = attempt_res.rows_affected();
    let deleted_payloads = payload_res.rows_affected();
    let deleted_logs = log_res.rows_affected();
    let deleted = deleted_attempts + deleted_payloads + deleted_logs;
    if deleted > 0 {
        info!(
            deleted,
            deleted_logs,
            deleted_payloads,
            deleted_attempts,
            retention_days,
            "log retention cleanup pass"
        );
    }
    Ok(deleted)
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

    async fn insert_log(pool: &DbPool, request_id: &str, ts: &str) {
        sqlx::query(
            "INSERT INTO request_logs (request_id, ts, virtual_model, ingress_protocol, status) \
             VALUES ($1, $2, 'm', 'openai/chat-completions/v1', 'ok')",
        )
        .bind(request_id)
        .bind(ts)
        .execute(pool.any())
        .await
        .expect("insert");
    }

    async fn insert_payload(pool: &DbPool, request_id: &str, captured_at: &str) {
        sqlx::query(
            "INSERT INTO request_payloads (request_id, captured_at, egress_body) \
             VALUES ($1, $2, $3)",
        )
        .bind(request_id)
        .bind(captured_at)
        .bind(format!("payload-{request_id}"))
        .execute(pool.any())
        .await
        .expect("insert payload");
    }

    async fn insert_attempt(pool: &DbPool, request_id: &str, ts: &str) {
        sqlx::query(
            "INSERT INTO request_attempts (request_id, hop, ts, stage, target, status) \
             VALUES ($1, 1, $2, 'execute', 'target', 'failed')",
        )
        .bind(request_id)
        .bind(ts)
        .execute(pool.any())
        .await
        .expect("insert attempt");
    }

    async fn log_count(pool: &DbPool, request_id: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM request_logs WHERE request_id = $1")
            .bind(request_id)
            .fetch_one(pool.any())
            .await
            .expect("log count")
    }

    async fn payload_count(pool: &DbPool, request_id: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM request_payloads WHERE request_id = $1")
            .bind(request_id)
            .fetch_one(pool.any())
            .await
            .expect("payload count")
    }

    async fn attempt_count(pool: &DbPool, request_id: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM request_attempts WHERE request_id = $1")
            .bind(request_id)
            .fetch_one(pool.any())
            .await
            .expect("attempt count")
    }

    #[tokio::test]
    async fn cleanup_once_deletes_old_logs_payloads_and_orphans() {
        let pool = in_mem_pool().await;
        let old_ts = (chrono::Utc::now() - chrono::Duration::days(60)).to_rfc3339();
        let new_ts = chrono::Utc::now().to_rfc3339();
        insert_log(pool.as_ref(), "old", &old_ts).await;
        insert_payload(pool.as_ref(), "old", &new_ts).await;
        insert_attempt(pool.as_ref(), "old", &new_ts).await;
        insert_log(pool.as_ref(), "new", &new_ts).await;
        insert_payload(pool.as_ref(), "new", &new_ts).await;
        insert_attempt(pool.as_ref(), "new", &new_ts).await;
        insert_payload(pool.as_ref(), "orphan-old", &old_ts).await;
        insert_payload(pool.as_ref(), "orphan-new", &new_ts).await;
        insert_attempt(pool.as_ref(), "orphan-old", &old_ts).await;
        insert_attempt(pool.as_ref(), "orphan-new", &new_ts).await;

        let deleted = cleanup_once(pool.as_ref(), 30).await.expect("cleanup");
        assert_eq!(
            deleted, 5,
            "old log, old payload, old attempt, and old orphans must be deleted"
        );

        assert_eq!(log_count(pool.as_ref(), "old").await, 0);
        assert_eq!(payload_count(pool.as_ref(), "old").await, 0);
        assert_eq!(attempt_count(pool.as_ref(), "old").await, 0);
        assert_eq!(log_count(pool.as_ref(), "new").await, 1);
        assert_eq!(payload_count(pool.as_ref(), "new").await, 1);
        assert_eq!(attempt_count(pool.as_ref(), "new").await, 1);
        assert_eq!(payload_count(pool.as_ref(), "orphan-old").await, 0);
        assert_eq!(payload_count(pool.as_ref(), "orphan-new").await, 1);
        assert_eq!(attempt_count(pool.as_ref(), "orphan-old").await, 0);
        assert_eq!(attempt_count(pool.as_ref(), "orphan-new").await, 1);
    }

    #[tokio::test]
    async fn cleanup_once_disabled_when_zero_days() {
        let pool = in_mem_pool().await;
        let old_ts = (chrono::Utc::now() - chrono::Duration::days(365)).to_rfc3339();
        insert_log(pool.as_ref(), "very-old", &old_ts).await;
        insert_payload(pool.as_ref(), "very-old", &old_ts).await;
        insert_attempt(pool.as_ref(), "very-old", &old_ts).await;
        insert_payload(pool.as_ref(), "very-old-orphan", &old_ts).await;
        insert_attempt(pool.as_ref(), "very-old-orphan", &old_ts).await;
        let deleted = cleanup_once(pool.as_ref(), 0).await.expect("cleanup");
        assert_eq!(deleted, 0);
        assert_eq!(log_count(pool.as_ref(), "very-old").await, 1);
        assert_eq!(payload_count(pool.as_ref(), "very-old").await, 1);
        assert_eq!(attempt_count(pool.as_ref(), "very-old").await, 1);
        assert_eq!(payload_count(pool.as_ref(), "very-old-orphan").await, 1);
        assert_eq!(attempt_count(pool.as_ref(), "very-old-orphan").await, 1);
    }

    #[tokio::test]
    async fn spawn_handle_stops_cleanly() {
        let pool = in_mem_pool().await;
        let store = Arc::new(crate::config_store::DbConfigStore::new(
            (*pool).clone(),
            None,
        ));
        // Set a short interval and disable cleanup so the task is
        // idle but alive.
        store
            .set_setting(settings_keys::RETENTION_INTERVAL_SECS, "1")
            .await
            .expect("set interval");
        store
            .set_setting(settings_keys::RETENTION_LOG_RETENTION_DAYS, "0")
            .await
            .expect("set days");
        let handle = spawn(pool.clone(), store);
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
/// the in-memory snapshot whenever the epoch advances. The poll
/// interval is read from the `settings` table each iteration so it
/// can be tuned at runtime; the env-derived [`EpochPollConfig`] is
/// only a fallback default.
pub fn spawn_epoch_poll(store: Arc<crate::config_store::DbConfigStore>) -> EpochPollHandle {
    let fallback = EpochPollConfig::from_env();
    let handle = tokio::spawn(async move {
        // First tick fires immediately — `App::new()` already called
        // `refresh()` once at startup, so we skip it.
        let mut last_seen: Option<i64> = None;
        loop {
            let interval_secs = settings_keys::get_u64(
                store.as_ref(),
                settings_keys::EPOCH_POLL_INTERVAL_SECS,
                fallback.interval.as_secs(),
            )
            .await;
            tokio::time::sleep(Duration::from_secs(interval_secs.max(1))).await;
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
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod epoch_tests {
    use super::*;
    use crate::db;

    #[tokio::test]
    async fn epoch_starts_at_zero_after_initial_refresh() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(&pool).await.expect("migrate");
        let store = Arc::new(crate::config_store::DbConfigStore::new(pool, None));
        store.refresh().await.expect("initial refresh");
        // The very first refresh bumps the epoch from 0 to 1.
        let epoch = store.current_epoch().await.expect("epoch");
        assert!(epoch >= 1);
    }
}
