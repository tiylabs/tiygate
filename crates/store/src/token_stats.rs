//! Background token-stats aggregation task.
//!
//! Periodically aggregates `request_logs` into the pre-computed
//! `token_daily_stats` and `token_summary` tables so the Dashboard
//! "Token Activity" panel can serve data in O(1) reads without
//! performing expensive GROUP BY queries on each API call.
//!
//! Design: space-for-time trade-off. The background task runs every
//! `interval` (default 5 min), re-aggregates today's row from
//! `request_logs`, and recomputes the single-row `token_summary`.
//! Historical days (before today) are aggregated only once — the
//! task upserts but skips days whose `request_count` hasn't changed.

use std::sync::Arc;
use std::time::Duration;

use chrono::{NaiveDate, Utc};
use sqlx::Row;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config_store::DbConfigStore;
use crate::db::DbPool;
use crate::models::ExportTokenDailyStat;
use crate::settings_keys;

/// Configuration for the token-stats aggregation background task.
#[derive(Debug, Clone)]
pub struct TokenStatsConfig {
    /// How often to run the aggregation pass.
    pub interval: Duration,
    /// How many past days to aggregate (lookback window). Days older
    /// than this are left untouched (they were already aggregated).
    /// Defaults to 400 (slightly over a year for heatmap coverage).
    pub lookback_days: u32,
}

impl Default for TokenStatsConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5 * 60),
            lookback_days: 400,
        }
    }
}

impl TokenStatsConfig {
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Ok(v) = std::env::var("TIYGATE_TOKEN_STATS_INTERVAL_SECS") {
            if let Ok(n) = v.parse() {
                c.interval = Duration::from_secs(n);
            }
        }
        if let Ok(v) = std::env::var("TIYGATE_TOKEN_STATS_LOOKBACK_DAYS") {
            if let Ok(n) = v.parse() {
                c.lookback_days = n;
            }
        }
        c
    }
}

/// Handle for the spawned token-stats aggregation task.
pub struct TokenStatsHandle {
    handle: JoinHandle<()>,
}

impl TokenStatsHandle {
    /// Stop the background task. Idempotent.
    pub async fn stop(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

/// Spawn the background aggregation task. Both the interval and the
/// lookback window are read from the `settings` table on every
/// iteration so they can be tuned at runtime. The env-derived
/// [`TokenStatsConfig`] is only a fallback default.
pub fn spawn(pool: Arc<DbPool>, store: Arc<DbConfigStore>) -> TokenStatsHandle {
    let fallback = TokenStatsConfig::from_env();
    let handle = tokio::spawn(async move {
        info!(
            interval_secs = fallback.interval.as_secs(),
            lookback_days = fallback.lookback_days,
            "token stats aggregation task started (defaults; runtime values come from settings)"
        );
        loop {
            let interval_secs = settings_keys::get_u64(
                store.as_ref(),
                settings_keys::TOKEN_STATS_INTERVAL_SECS,
                fallback.interval.as_secs(),
            )
            .await;
            let lookback_days = settings_keys::get_u64(
                store.as_ref(),
                settings_keys::TOKEN_STATS_LOOKBACK_DAYS,
                fallback.lookback_days as u64,
            )
            .await as u32;
            if let Err(e) = aggregate_once(pool.as_ref(), lookback_days).await {
                warn!(error = %e, "token stats aggregation failed");
            }
            tokio::time::sleep(Duration::from_secs(interval_secs.max(1))).await;
        }
    });
    TokenStatsHandle { handle }
}

/// Run a single aggregation pass. Public for testing.
pub async fn aggregate_once(pool: &DbPool, lookback_days: u32) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    let today = now.date_naive();
    let since = today - chrono::Duration::days(lookback_days as i64);
    let since_str = since.format("%Y-%m-%d").to_string();

    // Step 1: Aggregate per-day stats from request_logs.
    let rows = sqlx::query(
        "SELECT CAST(DATE(ts) AS TEXT) AS day, \
                COUNT(*) AS cnt, \
                COALESCE(SUM(total_tokens), 0) AS tt, \
                COALESCE(SUM(prompt_tokens), 0) AS pt, \
                COALESCE(SUM(completion_tokens), 0) AS ct, \
                COALESCE(SUM(reasoning_tokens), 0) AS rt, \
                COALESCE(MAX(total_tokens), 0) AS peak_req, \
                COALESCE(MAX(total_latency_ms), 0) AS longest_ms \
         FROM request_logs \
         WHERE ts >= $1 \
         GROUP BY DATE(ts) \
         ORDER BY day",
    )
    .bind(&since_str)
    .fetch_all(pool.any())
    .await?;

    let updated_at = now.to_rfc3339();

    // Step 2: Upsert each day into token_daily_stats.
    for r in &rows {
        let day: String = r.get("day");
        let request_count: i64 = r.get("cnt");
        let total_tokens: i64 = r.get("tt");
        let prompt_tokens: i64 = r.get("pt");
        let completion_tokens: i64 = r.get("ct");
        let reasoning_tokens: i64 = r.get("rt");
        let peak_single_request: i64 = r.get("peak_req");
        let longest_task_ms: i64 = r.get("longest_ms");

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
        .bind(&day)
        .bind(request_count)
        .bind(total_tokens)
        .bind(prompt_tokens)
        .bind(completion_tokens)
        .bind(reasoning_tokens)
        .bind(peak_single_request)
        .bind(longest_task_ms)
        .bind(&updated_at)
        .execute(pool.any())
        .await?;
    }

    // Step 3: Recompute summary from token_daily_stats.
    recompute_summary(pool).await?;

    debug!(
        days_aggregated = rows.len(),
        "token stats aggregation pass complete"
    );

    Ok(())
}

/// Recompute the single-row `token_summary` table from the current
/// contents of `token_daily_stats`. Computes `lifetime_tokens`
/// (SUM), `peak_day_tokens` (MAX), `longest_task_ms` (MAX), and
/// streaks (via `compute_streaks`). Public so the config import
/// path can call it after merging imported daily stats.
pub async fn recompute_summary(pool: &DbPool) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    let today = now.date_naive();
    let updated_at = now.to_rfc3339();

    let lifetime_tokens: i64 =
        sqlx::query_scalar("SELECT COALESCE(SUM(total_tokens), 0) FROM token_daily_stats")
            .fetch_one(pool.any())
            .await?;

    let peak_day_tokens: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(total_tokens), 0) FROM token_daily_stats")
            .fetch_one(pool.any())
            .await?;

    let longest_task_ms: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(longest_task_ms), 0) FROM token_daily_stats")
            .fetch_one(pool.any())
            .await?;

    let (current_streak, longest_streak) = compute_streaks(pool, today).await?;

    sqlx::query(
        "UPDATE token_summary SET \
            lifetime_tokens = $1, \
            peak_day_tokens = $2, \
            longest_task_ms = $3, \
            current_streak = $4, \
            longest_streak = $5, \
            updated_at = $6 \
         WHERE id = 1",
    )
    .bind(lifetime_tokens)
    .bind(peak_day_tokens)
    .bind(longest_task_ms)
    .bind(current_streak)
    .bind(longest_streak)
    .bind(&updated_at)
    .execute(pool.any())
    .await?;

    debug!(
        lifetime_tokens,
        current_streak, longest_streak, "token summary recomputed"
    );

    Ok(())
}

/// Compute the current streak (consecutive days ending at `today`) and
/// the longest streak ever from `token_daily_stats`.
async fn compute_streaks(pool: &DbPool, today: NaiveDate) -> Result<(i64, i64), sqlx::Error> {
    // Fetch all active days (those with at least 1 token) ordered descending.
    let days: Vec<String> = sqlx::query_scalar(
        "SELECT day FROM token_daily_stats WHERE total_tokens > 0 ORDER BY day DESC",
    )
    .fetch_all(pool.any())
    .await?;

    if days.is_empty() {
        return Ok((0, 0));
    }

    // Parse into NaiveDate for streak calculation.
    let mut parsed: Vec<NaiveDate> = Vec::with_capacity(days.len());
    for d in &days {
        if let Ok(nd) = NaiveDate::parse_from_str(d, "%Y-%m-%d") {
            parsed.push(nd);
        }
    }

    if parsed.is_empty() {
        return Ok((0, 0));
    }

    // Current streak: consecutive days ending at today (or yesterday
    // if today hasn't had activity yet).
    let mut current_streak: i64 = 0;
    let first = parsed[0];
    // The most recent active day must be today or yesterday to count
    // as "current".
    let diff_from_today = (today - first).num_days();
    if diff_from_today <= 1 {
        current_streak = 1;
        for i in 1..parsed.len() {
            if (parsed[i - 1] - parsed[i]).num_days() == 1 {
                current_streak += 1;
            } else {
                break;
            }
        }
    }

    // Longest streak: find the longest consecutive run in the sorted
    // (ascending) list.
    let mut sorted = parsed.clone();
    sorted.sort();
    let mut longest_streak: i64 = 1;
    let mut run: i64 = 1;
    for i in 1..sorted.len() {
        if (sorted[i] - sorted[i - 1]).num_days() == 1 {
            run += 1;
        } else {
            run = 1;
        }
        if run > longest_streak {
            longest_streak = run;
        }
    }

    Ok((current_streak, longest_streak))
}

// --- Public query helpers for the admin API ---

/// Single day of token activity data.
#[derive(Debug, serde::Serialize)]
pub struct TokenDayActivity {
    pub day: String,
    pub total_tokens: i64,
    pub request_count: i64,
}

/// Fetch the pre-aggregated daily token activity for the heatmap.
pub async fn get_token_activity(
    pool: &DbPool,
    days: u32,
) -> Result<Vec<TokenDayActivity>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT day, total_tokens, request_count \
         FROM token_daily_stats \
         ORDER BY day DESC \
         LIMIT $1",
    )
    .bind(days as i64)
    .fetch_all(pool.any())
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(TokenDayActivity {
            day: r.get("day"),
            total_tokens: r.get("total_tokens"),
            request_count: r.get("request_count"),
        });
    }
    // Return in chronological order (ascending).
    out.reverse();
    Ok(out)
}

/// The token summary metrics for the top card row.
#[derive(Debug, serde::Serialize)]
pub struct TokenSummaryData {
    pub lifetime_tokens: i64,
    pub peak_day_tokens: i64,
    pub longest_task_ms: i64,
    pub current_streak: i64,
    pub longest_streak: i64,
    pub updated_at: String,
}

/// Fetch the pre-computed summary from the single-row table.
pub async fn get_token_summary(pool: &DbPool) -> Result<TokenSummaryData, sqlx::Error> {
    let row = sqlx::query(
        "SELECT lifetime_tokens, peak_day_tokens, longest_task_ms, \
                current_streak, longest_streak, updated_at \
         FROM token_summary WHERE id = 1",
    )
    .fetch_one(pool.any())
    .await?;

    Ok(TokenSummaryData {
        lifetime_tokens: row.get("lifetime_tokens"),
        peak_day_tokens: row.get("peak_day_tokens"),
        longest_task_ms: row.get("longest_task_ms"),
        current_streak: row.get("current_streak"),
        longest_streak: row.get("longest_streak"),
        updated_at: row.get("updated_at"),
    })
}

/// Export all rows from `token_daily_stats` for inclusion in a config
/// backup bundle. Rows are ordered by `day` ascending. The
/// `updated_at` column is omitted — the importing instance refreshes
/// it during the additive merge.
pub async fn export_token_daily_stats(
    pool: &DbPool,
) -> Result<Vec<ExportTokenDailyStat>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT day, request_count, total_tokens, prompt_tokens, \
                completion_tokens, reasoning_tokens, peak_single_request, \
                longest_task_ms \
         FROM token_daily_stats ORDER BY day",
    )
    .fetch_all(pool.any())
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(ExportTokenDailyStat {
            day: r.get("day"),
            request_count: r.get("request_count"),
            total_tokens: r.get("total_tokens"),
            prompt_tokens: r.get("prompt_tokens"),
            completion_tokens: r.get("completion_tokens"),
            reasoning_tokens: r.get("reasoning_tokens"),
            peak_single_request: r.get("peak_single_request"),
            longest_task_ms: r.get("longest_task_ms"),
        });
    }
    Ok(out)
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

    async fn insert_log(pool: &DbPool, request_id: &str, ts: &str, tokens: i64, latency_ms: i64) {
        sqlx::query(
            "INSERT INTO request_logs \
                (request_id, ts, virtual_model, ingress_protocol, status, \
                 total_latency_ms, upstream_latency_ms, queue_latency_ms, lossy, \
                 total_tokens, prompt_tokens, completion_tokens) \
             VALUES ($1, $2, 'gpt-4o', 'openai/chat-completions/v1', 'ok', $3, 0, 0, 0, $4, $5, $6)",
        )
        .bind(request_id)
        .bind(ts)
        .bind(latency_ms)
        .bind(tokens)
        .bind(tokens / 2)
        .bind(tokens / 2)
        .execute(pool.any())
        .await
        .expect("insert");
    }

    #[tokio::test]
    async fn aggregate_once_produces_daily_stats_and_summary() {
        let pool = in_mem_pool().await;
        let today = Utc::now().date_naive();
        let yesterday = today - chrono::Duration::days(1);

        // Insert some logs for today and yesterday.
        let today_ts = format!("{}T10:00:00Z", today);
        let yesterday_ts = format!("{}T10:00:00Z", yesterday);

        insert_log(pool.as_ref(), "req-1", &today_ts, 100, 5000).await;
        insert_log(pool.as_ref(), "req-2", &today_ts, 200, 3000).await;
        insert_log(pool.as_ref(), "req-3", &yesterday_ts, 150, 8000).await;

        aggregate_once(pool.as_ref(), 30).await.expect("aggregate");

        // Check daily stats.
        let activity = get_token_activity(pool.as_ref(), 365)
            .await
            .expect("activity");
        assert_eq!(activity.len(), 2);
        // First day is yesterday (chronological order).
        assert_eq!(activity[0].day, yesterday.format("%Y-%m-%d").to_string());
        assert_eq!(activity[0].total_tokens, 150);
        // Second day is today.
        assert_eq!(activity[1].day, today.format("%Y-%m-%d").to_string());
        assert_eq!(activity[1].total_tokens, 300);

        // Check summary.
        let summary = get_token_summary(pool.as_ref()).await.expect("summary");
        assert_eq!(summary.lifetime_tokens, 450);
        assert_eq!(summary.peak_day_tokens, 300);
        assert_eq!(summary.longest_task_ms, 8000);
        assert_eq!(summary.current_streak, 2);
        assert_eq!(summary.longest_streak, 2);
    }

    #[tokio::test]
    async fn streak_not_current_if_gap() {
        let pool = in_mem_pool().await;
        let today = Utc::now().date_naive();
        // Insert activity 3 days ago only (gap of 2 days).
        let three_ago = today - chrono::Duration::days(3);
        let ts = format!("{}T10:00:00Z", three_ago);
        insert_log(pool.as_ref(), "req-old", &ts, 50, 1000).await;

        aggregate_once(pool.as_ref(), 30).await.expect("aggregate");

        let summary = get_token_summary(pool.as_ref()).await.expect("summary");
        assert_eq!(summary.current_streak, 0);
        assert_eq!(summary.longest_streak, 1);
    }
}
