//! Brute-force protection for the Admin API.
//!
//! Each client (identified by IP) is tracked with a simple state
//! machine:
//!
//! * consecutive failures are counted; on reaching `max_failures`
//!   the client is locked out for `lockout_secs` and the lockout
//!   counter increments.
//! * on reaching `max_lockouts` the lockout escalates to
//!   `escalated_lockout_secs` (24h) and the lockout counter resets.
//! * a successful authentication resets all counters.
//!
//! Two backing implementations are provided:
//!
//! * [`InMemoryBruteForceLimiter`] — per-instance state behind a
//!   `tokio::sync::Mutex<HashMap>`. Suitable for single-replica
//!   deployments and tests.
//! * [`RedisBruteForceLimiter`] (feature `redis`) — Redis-backed
//!   state using a Lua script for atomic record-failure. Mirrors
//!   the `RedisQuota` fail-open pattern: a flaky Redis never blocks
//!   a legitimate request.
//!
//! The [`build_limiter`] factory inspects `TIYGATE_REDIS_URL` and
//! the `redis` feature to pick the implementation, defaulting to
//! the in-memory limiter.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

/// Threshold configuration for brute-force protection.
///
/// Defaults match the plan: 3 failures → 5 min lockout, 3 lockouts
/// → 24h escalated lockout. Operators override via env vars
/// `TIYGATE_ADMIN_BF_MAX_FAILURES`, `TIYGATE_ADMIN_BF_LOCKOUT_SECS`,
/// `TIYGATE_ADMIN_BF_MAX_LOCKOUTS`, and
/// `TIYGATE_ADMIN_BF_ESCALATED_LOCKOUT_SECS`.
#[derive(Debug, Clone)]
pub struct BruteForceConfig {
    /// Consecutive failures before a lockout is triggered.
    pub max_failures: u32,
    /// Base lockout duration in seconds.
    pub lockout_secs: u64,
    /// Consecutive lockouts before escalation.
    pub max_lockouts: u32,
    /// Escalated lockout duration in seconds.
    pub escalated_lockout_secs: u64,
}

impl Default for BruteForceConfig {
    fn default() -> Self {
        Self {
            max_failures: 3,
            lockout_secs: 300,
            max_lockouts: 3,
            escalated_lockout_secs: 86_400,
        }
    }
}

impl BruteForceConfig {
    /// Build the config from environment variables, falling back
    /// to the documented defaults when unset or malformed.
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            max_failures: env_u32("TIYGATE_ADMIN_BF_MAX_FAILURES", defaults.max_failures),
            lockout_secs: env_u64("TIYGATE_ADMIN_BF_LOCKOUT_SECS", defaults.lockout_secs),
            max_lockouts: env_u32("TIYGATE_ADMIN_BF_MAX_LOCKOUTS", defaults.max_lockouts),
            escalated_lockout_secs: env_u64(
                "TIYGATE_ADMIN_BF_ESCALATED_LOCKOUT_SECS",
                defaults.escalated_lockout_secs,
            ),
        }
    }
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Brute-force limiter trait. All methods must be safe to call
/// concurrently from the request hot path.
#[async_trait]
pub trait BruteForceLimiter: Send + Sync {
    /// Returns `true` when `client_id` is currently locked out.
    async fn is_locked(&self, client_id: &str) -> bool;

    /// Record a failed authentication attempt. Returns `true` when
    /// this failure triggered a *new* lockout (so the caller can
    /// log or surface it).
    async fn record_failure(&self, client_id: &str) -> bool;

    /// Record a successful authentication; resets all counters for
    /// `client_id`.
    async fn record_success(&self, client_id: &str);
}

/// Build the limiter selected by the environment and feature flags.
///
/// * `TIYGATE_REDIS_URL` set + `redis` feature on →
///   [`RedisBruteForceLimiter`].
/// * otherwise → [`InMemoryBruteForceLimiter`].
pub fn build_limiter(config: &BruteForceConfig) -> Arc<dyn BruteForceLimiter> {
    let redis_url = std::env::var("TIYGATE_REDIS_URL")
        .ok()
        .filter(|s| !s.is_empty());
    #[cfg(feature = "redis")]
    if let Some(url) = redis_url {
        match RedisBruteForceLimiter::try_new(url, config.clone()) {
            Some(limiter) => {
                tracing::info!("brute-force: using RedisBruteForceLimiter");
                return Arc::new(limiter);
            }
            None => {
                tracing::warn!("brute-force: TIYGATE_REDIS_URL set but client init failed; falling back to in-memory");
            }
        }
    }
    #[cfg(not(feature = "redis"))]
    {
        if redis_url.is_some() {
            tracing::debug!(
                "brute-force: TIYGATE_REDIS_URL set but redis feature disabled; using in-memory"
            );
        }
    }
    tracing::info!("brute-force: using InMemoryBruteForceLimiter");
    Arc::new(InMemoryBruteForceLimiter::new(config.clone()))
}

// ---------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------

/// Per-client attempt state held by the in-memory limiter.
#[derive(Debug, Default)]
struct AttemptState {
    consecutive_failures: u32,
    lockout_count: u32,
    /// `Some` when the client is locked out; the instant is when
    /// the lockout expires.
    locked_until: Option<tokio::time::Instant>,
}

/// In-memory brute-force limiter.
pub struct InMemoryBruteForceLimiter {
    config: BruteForceConfig,
    state: Mutex<HashMap<String, AttemptState>>,
}

impl InMemoryBruteForceLimiter {
    pub fn new(config: BruteForceConfig) -> Self {
        Self {
            config,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Remove expired entries to bound memory growth. Called
    /// opportunistically inside the locked section. Entries with a
    /// non-zero `lockout_count` are retained even after their
    /// lockout expires so the escalation counter survives across
    /// lockout cycles.
    fn purge_expired(map: &mut HashMap<String, AttemptState>) {
        let now = tokio::time::Instant::now();
        map.retain(|_, st| {
            // Still actively locked.
            st.locked_until.is_some_and(|deadline| deadline > now)
                // Pending failures not yet cleared by a lockout.
                || st.consecutive_failures > 0
                // Repeat-offender history for escalation tracking.
                || st.lockout_count > 0
        });
    }
}

#[async_trait]
impl BruteForceLimiter for InMemoryBruteForceLimiter {
    async fn is_locked(&self, client_id: &str) -> bool {
        let mut map = self.state.lock().await;
        Self::purge_expired(&mut map);
        match map.get(client_id) {
            Some(st) => st
                .locked_until
                .is_some_and(|deadline| deadline > tokio::time::Instant::now()),
            None => false,
        }
    }

    async fn record_failure(&self, client_id: &str) -> bool {
        let mut map = self.state.lock().await;
        Self::purge_expired(&mut map);
        let st = map.entry(client_id.to_string()).or_default();

        // If currently locked, this is an attempt during lockout —
        // do not extend or double-count.
        if st
            .locked_until
            .is_some_and(|deadline| deadline > tokio::time::Instant::now())
        {
            return false;
        }

        // A lockout may have just expired; clear it before counting.
        if st
            .locked_until
            .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
        {
            st.locked_until = None;
        }

        st.consecutive_failures += 1;
        if st.consecutive_failures >= self.config.max_failures {
            st.lockout_count += 1;
            st.consecutive_failures = 0;
            let now = tokio::time::Instant::now();
            if st.lockout_count >= self.config.max_lockouts {
                // Escalate to the long lockout and reset the
                // lockout counter so the next escalation cycle
                // starts fresh.
                st.locked_until = Some(
                    now + tokio::time::Duration::from_secs(self.config.escalated_lockout_secs),
                );
                st.lockout_count = 0;
            } else {
                st.locked_until =
                    Some(now + tokio::time::Duration::from_secs(self.config.lockout_secs));
            }
            return true;
        }
        false
    }

    async fn record_success(&self, client_id: &str) {
        let mut map = self.state.lock().await;
        if let Some(st) = map.get_mut(client_id) {
            st.consecutive_failures = 0;
            st.lockout_count = 0;
            // Do not clear an active lockout — a successful auth
            // during lockout should not bypass the lockout. The
            // middleware checks `is_locked` before verifying the
            // token, so a locked client never reaches here, but we
            // keep the invariant defensively.
        }
    }
}

// ---------------------------------------------------------------------
// Redis implementation (feature `redis`)
// ---------------------------------------------------------------------

#[cfg(feature = "redis")]
mod redis_impl {
    use super::{BruteForceConfig, BruteForceLimiter};
    use async_trait::async_trait;
    use redis::Script;
    use std::sync::Arc;
    use tracing::warn;

    /// Atomic record-failure. Stores the per-client state in a Redis
    /// hash and returns `1` when this failure triggered a new
    /// lockout, `0` otherwise.
    ///
    /// ARGV: now_ms, max_failures, lockout_secs_ms, max_lockouts,
    /// escalated_lockout_secs_ms
    const FAIL_SCRIPT: &str = r#"
        local now = tonumber(ARGV[1])
        local max_failures = tonumber(ARGV[2])
        local lockout_ms = tonumber(ARGV[3])
        local max_lockouts = tonumber(ARGV[4])
        local escalated_ms = tonumber(ARGV[5])

        local locked_until = tonumber(redis.call('HGET', KEYS[1], 'locked_until') or '0')
        if locked_until > now then
            return 0
        end

        local failures = tonumber(redis.call('HGET', KEYS[1], 'failures') or '0')
        local lockouts = tonumber(redis.call('HGET', KEYS[1], 'lockouts') or '0')

        failures = failures + 1
        if failures < max_failures then
            redis.call('HSET', KEYS[1], 'failures', failures)
            redis.call('PEXPIRE', KEYS[1], lockout_ms + escalated_ms)
            return 0
        end

        -- Lockout triggered.
        lockouts = lockouts + 1
        local lockout_duration
        if lockouts >= max_lockouts then
            lockout_duration = escalated_ms
            lockouts = 0
        else
            lockout_duration = lockout_ms
        end
        local locked_until_val = now + lockout_duration
        redis.call('HSET', KEYS[1], 'failures', 0, 'lockouts', lockouts, 'locked_until', locked_until_val)
        redis.call('PEXPIRE', KEYS[1], lockout_duration)
        return 1
    "#;

    /// Redis-backed brute-force limiter. Connection errors at
    /// request time fail open: `is_locked` returns `false`,
    /// `record_failure` returns `false`, `record_success` is a
    /// no-op. This matches the `RedisQuota` philosophy so a flaky
    /// Redis never blocks a legitimate operator.
    pub struct RedisBruteForceLimiter {
        client: redis::Client,
        config: BruteForceConfig,
        script: Arc<Script>,
    }

    impl RedisBruteForceLimiter {
        /// Attempt to construct the limiter. Returns `None` when
        /// the URL cannot be parsed into a client (so the caller
        /// can fall back to in-memory).
        pub fn try_new(url: String, config: BruteForceConfig) -> Option<Self> {
            let client = redis::Client::open(url).ok()?;
            Some(Self {
                client,
                config,
                script: Arc::new(Script::new(FAIL_SCRIPT)),
            })
        }

        fn key_for(client_id: &str) -> String {
            format!("tiygate:bf:{client_id}")
        }

        fn now_ms() -> u64 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        }
    }

    #[async_trait]
    impl BruteForceLimiter for RedisBruteForceLimiter {
        async fn is_locked(&self, client_id: &str) -> bool {
            let mut conn = match self.client.get_multiplexed_async_connection().await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "brute-force redis: connection failed; fail-open (not locked)");
                    return false;
                }
            };
            let key = Self::key_for(client_id);
            let now = Self::now_ms();
            // HGET returns the stored epoch-ms deadline (or nil).
            let result: redis::RedisResult<Option<i64>> = redis::cmd("HGET")
                .arg(&key)
                .arg("locked_until")
                .query_async(&mut conn)
                .await;
            match result {
                Ok(Some(deadline)) if deadline > now as i64 => true,
                Ok(_) => false,
                Err(e) => {
                    warn!(error = %e, "brute-force redis: HGET failed; fail-open (not locked)");
                    false
                }
            }
        }

        async fn record_failure(&self, client_id: &str) -> bool {
            let mut conn = match self.client.get_multiplexed_async_connection().await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "brute-force redis: connection failed; fail-open (no lockout)");
                    return false;
                }
            };
            let key = Self::key_for(client_id);
            let now = Self::now_ms() as i64;
            // ARGV order: now, max_failures, lockout_ms, max_lockouts, escalated_ms
            let result: redis::RedisResult<i64> = self
                .script
                .key(&key)
                .arg(now)
                .arg(self.config.max_failures as i64)
                .arg((self.config.lockout_secs * 1000) as i64)
                .arg(self.config.max_lockouts as i64)
                .arg((self.config.escalated_lockout_secs * 1000) as i64)
                .invoke_async(&mut conn)
                .await;
            match result {
                Ok(1) => true,
                Ok(_) => false,
                Err(e) => {
                    warn!(error = %e, "brute-force redis: script failed; fail-open (no lockout)");
                    false
                }
            }
        }

        async fn record_success(&self, client_id: &str) {
            let mut conn = match self.client.get_multiplexed_async_connection().await {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "brute-force redis: connection failed; record_success no-op");
                    return;
                }
            };
            let key = Self::key_for(client_id);
            let result: redis::RedisResult<()> =
                redis::cmd("DEL").arg(&key).query_async(&mut conn).await;
            if let Err(e) = result {
                warn!(error = %e, "brute-force redis: DEL failed; record_success no-op");
            }
        }
    }
}

#[cfg(feature = "redis")]
pub use redis_impl::RedisBruteForceLimiter;

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn fast_config() -> BruteForceConfig {
        BruteForceConfig {
            max_failures: 3,
            lockout_secs: 1,
            max_lockouts: 3,
            escalated_lockout_secs: 2,
        }
    }

    #[tokio::test]
    async fn in_memory_locks_after_max_failures() {
        let limiter = InMemoryBruteForceLimiter::new(fast_config());
        let id = "1.2.3.4";
        assert!(!limiter.is_locked(id).await);
        assert!(!limiter.record_failure(id).await);
        assert!(!limiter.record_failure(id).await);
        // Third failure triggers lockout.
        assert!(limiter.record_failure(id).await);
        assert!(limiter.is_locked(id).await);
    }

    #[tokio::test]
    async fn in_memory_unlocks_after_lockout_expires() {
        let limiter = InMemoryBruteForceLimiter::new(fast_config());
        let id = "5.6.7.8";
        for _ in 0..3 {
            limiter.record_failure(id).await;
        }
        assert!(limiter.is_locked(id).await);
        tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;
        assert!(!limiter.is_locked(id).await);
    }

    #[tokio::test]
    async fn in_memory_escalates_after_max_lockouts() {
        let limiter = InMemoryBruteForceLimiter::new(fast_config());
        let id = "9.10.11.12";

        // Two base-lockout cycles (1s each).
        for _ in 0..2 {
            for _ in 0..3 {
                limiter.record_failure(id).await;
            }
            assert!(limiter.is_locked(id).await);
            // Wait for the 1s base lockout to expire.
            tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;
            assert!(!limiter.is_locked(id).await);
        }

        // Third lockout cycle — this should escalate to 2s.
        for _ in 0..3 {
            limiter.record_failure(id).await;
        }
        assert!(limiter.is_locked(id).await);
        // After 1.1s the base lockout would have expired, but the
        // escalated lockout (2s) is still active.
        tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;
        assert!(
            limiter.is_locked(id).await,
            "escalated lockout should still be active after 1.1s"
        );
        // After another 1.1s (total 2.2s) the escalated lockout expires.
        tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;
        assert!(
            !limiter.is_locked(id).await,
            "escalated lockout should expire after ~2.2s"
        );
    }

    #[tokio::test]
    async fn in_memory_success_resets_counters() {
        let limiter = InMemoryBruteForceLimiter::new(fast_config());
        let id = "13.14.15.16";
        limiter.record_failure(id).await;
        limiter.record_failure(id).await;
        limiter.record_success(id).await;
        // After reset, it takes 3 failures again.
        assert!(!limiter.record_failure(id).await);
        assert!(!limiter.record_failure(id).await);
        assert!(limiter.record_failure(id).await);
    }

    #[tokio::test]
    async fn in_memory_failure_during_lockout_does_not_extend() {
        let limiter = InMemoryBruteForceLimiter::new(fast_config());
        let id = "17.18.19.20";
        for _ in 0..3 {
            limiter.record_failure(id).await;
        }
        assert!(limiter.is_locked(id).await);
        // Extra failures during lockout return false and do not
        // change the lockout expiry.
        assert!(!limiter.record_failure(id).await);
        tokio::time::sleep(tokio::time::Duration::from_millis(1100)).await;
        assert!(!limiter.is_locked(id).await);
    }

    #[tokio::test]
    async fn in_memory_per_client_isolation() {
        let limiter = InMemoryBruteForceLimiter::new(fast_config());
        for _ in 0..3 {
            limiter.record_failure("alice").await;
        }
        assert!(limiter.is_locked("alice").await);
        assert!(!limiter.is_locked("bob").await);
    }
}
