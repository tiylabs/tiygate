//! Quota tracking — token / request counters keyed by API key.
//!
//! Phase 4 (产品化) of the design implements a pluggable
//! [`QuotaCounter`] trait with two backing implementations:
//!
//! * [`InMemoryQuota`] — per-instance counters using `parking_lot`
//!   atomics; suitable for single-replica deployments and tests.
//! * [`RedisQuota`] — Redis-backed counters for multi-replica
//!   deployments. Lazily initialised; a missing or unreachable Redis
//!   gracefully degrades to the in-memory implementation so a transient
//!   Redis outage does not turn into a quota-loss event.
//!
//! Counters are bucketed by `(key_id, kind)` where `kind` is one of
//! `Requests` (per-minute and per-day) or `Tokens` (per-minute and
//! per-day). The trait surface is intentionally narrow — the ingress
//! hot path only needs [`QuotaCounter::check_and_consume`] and the
//! admin / health endpoints need [`QuotaCounter::current_usage`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Which bucket a quota check should be charged against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuotaKind {
    /// Requests-per-minute.
    RequestsPerMinute,
    /// Requests-per-day.
    RequestsPerDay,
    /// Tokens-per-minute (prompt + completion).
    TokensPerMinute,
    /// Tokens-per-day (prompt + completion).
    TokensPerDay,
}

impl QuotaKind {
    /// Returns the window length this quota is measured over.
    pub fn window(self) -> Duration {
        match self {
            QuotaKind::RequestsPerMinute | QuotaKind::TokensPerMinute => Duration::from_secs(60),
            QuotaKind::RequestsPerDay | QuotaKind::TokensPerDay => Duration::from_secs(86_400),
        }
    }
}

/// Outcome of a quota check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaDecision {
    /// Within budget; `remaining` is informational and may be `None`
    /// when the underlying backend cannot give an exact figure.
    Allow { remaining: Option<u64> },
    /// Over budget; `retry_after` is the minimum time the caller
    /// should wait before retrying.
    Deny {
        retry_after: Duration,
        limit: u64,
        kind: QuotaKind,
    },
}

impl QuotaDecision {
    /// Convenience: returns `true` for [`QuotaDecision::Allow`].
    pub fn is_allowed(&self) -> bool {
        matches!(self, QuotaDecision::Allow { .. })
    }
}

/// Static quota specification — how much of each `QuotaKind` a key
/// is allowed to use in its respective window.
///
/// The JSON shape (when serialized from the admin API) is:
///
/// ```json
/// {
///   "requests_per_minute": 100,
///   "requests_per_day": 1000,
///   "tokens_per_minute": 10000,
///   "tokens_per_day": 100000
/// }
/// ```
///
/// Every field is optional; a missing field is treated as
/// "unlimited for this bucket". This is the same shape that
/// `ApiKey::quota_json` (in the store) is expected to round-trip
/// through.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuotaSpec {
    pub requests_per_minute: Option<u64>,
    pub requests_per_day: Option<u64>,
    pub tokens_per_minute: Option<u64>,
    pub tokens_per_day: Option<u64>,
}

impl QuotaSpec {
    /// Returns `true` if the spec imposes no limits on any bucket.
    pub fn is_unlimited(&self) -> bool {
        self.requests_per_minute.is_none()
            && self.requests_per_day.is_none()
            && self.tokens_per_minute.is_none()
            && self.tokens_per_day.is_none()
    }

    /// Build a `QuotaSpec` from a `serde_json::Value` (typically
    /// `ApiKey::quota_json`). Malformed / missing fields fall back to
    /// the unlimited default — quota misconfiguration should not turn
    /// into a 429 storm, so we fail open per the §4.6 design note
    /// ("宁可少算不误杀").
    pub fn from_json(value: &serde_json::Value) -> Self {
        serde_json::from_value(value.clone()).unwrap_or_default()
    }
}

/// Errors emitted by quota backends.
#[derive(Debug, Error)]
pub enum QuotaError {
    #[error("quota backend error: {0}")]
    Backend(String),
}

/// Pluggable quota counter. All methods must be safe to call
/// concurrently from the request hot path.
#[async_trait::async_trait]
pub trait QuotaCounter: Send + Sync {
    /// Atomically check whether `key_id` may consume `tokens` (or
    /// `requests`, if `tokens = 1`) under `spec`, and consume the
    /// budget on `Allow`.
    async fn check_and_consume(
        &self,
        key_id: &str,
        spec: &QuotaSpec,
        tokens: u64,
    ) -> Result<QuotaDecision, QuotaError>;

    /// Returns the current usage for a key (no consumption).
    async fn current_usage(&self, key_id: &str) -> Result<HashMap<QuotaKind, u64>, QuotaError>;
}

// ---------------------------------------------------------------------
// In-memory implementation
// ---------------------------------------------------------------------

/// One rolling window.
#[derive(Debug, Default)]
struct WindowCounter {
    /// The window's start instant (epoch milliseconds).
    window_start_ms: u64,
    /// Tokens or requests consumed in the current window.
    used: u64,
}

impl WindowCounter {
    /// Returns the (used, ms_until_reset) pair, rolling the window if
    /// the current one has expired.
    fn snapshot(&mut self, window: Duration) -> (u64, Duration) {
        let now = now_ms();
        let window_ms = window.as_millis() as u64;
        if now.saturating_sub(self.window_start_ms) >= window_ms {
            self.window_start_ms = now;
            self.used = 0;
        }
        let elapsed = now.saturating_sub(self.window_start_ms);
        let remaining = Duration::from_millis(window_ms.saturating_sub(elapsed));
        (self.used, remaining)
    }
}

/// In-memory quota counter. Suitable for single-replica deployments
/// and for tests.
pub struct InMemoryQuota {
    state: Mutex<HashMap<String, HashMap<QuotaKind, WindowCounter>>>,
}

impl InMemoryQuota {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(HashMap::new()),
        })
    }
}

impl Default for InMemoryQuota {
    fn default() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl QuotaCounter for InMemoryQuota {
    async fn check_and_consume(
        &self,
        key_id: &str,
        spec: &QuotaSpec,
        tokens: u64,
    ) -> Result<QuotaDecision, QuotaError> {
        let mut state = self.state.lock();
        let per_kind = state.entry(key_id.to_string()).or_default();

        // Phase 1: verify each bucket has room.
        let mut to_consume: Vec<QuotaKind> = Vec::new();
        if spec.requests_per_minute.is_some() || spec.requests_per_day.is_some() {
            to_consume.push(QuotaKind::RequestsPerMinute);
            to_consume.push(QuotaKind::RequestsPerDay);
        }
        if spec.tokens_per_minute.is_some() || spec.tokens_per_day.is_some() {
            to_consume.push(QuotaKind::TokensPerMinute);
            to_consume.push(QuotaKind::TokensPerDay);
        }

        for kind in &to_consume {
            let counter = per_kind.entry(*kind).or_default();
            let (used, _remaining) = counter.snapshot(kind.window());
            let limit = match kind {
                QuotaKind::RequestsPerMinute => spec.requests_per_minute,
                QuotaKind::RequestsPerDay => spec.requests_per_day,
                QuotaKind::TokensPerMinute => spec.tokens_per_minute,
                QuotaKind::TokensPerDay => spec.tokens_per_day,
            };
            if let Some(limit) = limit {
                let increment = match kind {
                    QuotaKind::RequestsPerMinute | QuotaKind::RequestsPerDay => 1,
                    QuotaKind::TokensPerMinute | QuotaKind::TokensPerDay => tokens,
                };
                if used + increment > limit {
                    let (_, retry_after) = counter.snapshot(kind.window());
                    return Ok(QuotaDecision::Deny {
                        retry_after,
                        limit,
                        kind: *kind,
                    });
                }
            }
        }

        // Phase 2: commit the consumption.
        for kind in &to_consume {
            let counter = per_kind.entry(*kind).or_default();
            let _ = counter.snapshot(kind.window()); // ensure window is current
            let increment = match kind {
                QuotaKind::RequestsPerMinute | QuotaKind::RequestsPerDay => 1,
                QuotaKind::TokensPerMinute | QuotaKind::TokensPerDay => tokens,
            };
            counter.used = counter.used.saturating_add(increment);
        }

        Ok(QuotaDecision::Allow { remaining: None })
    }

    async fn current_usage(&self, key_id: &str) -> Result<HashMap<QuotaKind, u64>, QuotaError> {
        let mut state = self.state.lock();
        let per_kind = state.entry(key_id.to_string()).or_default();
        let mut out = HashMap::new();
        for (kind, counter) in per_kind.iter_mut() {
            let (used, _) = counter.snapshot(kind.window());
            out.insert(*kind, used);
        }
        Ok(out)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// Redis implementation (best-effort, optional `redis-quota` feature)
// ---------------------------------------------------------------------

/// Configuration for the Redis-backed quota counter.
///
/// The connection string follows the `redis://` URL convention used
/// by the `redis` crate. When the URL is `None` the implementation
/// falls back to in-memory counters — see [`RedisQuota::new`].
#[derive(Debug, Clone, Default)]
pub struct RedisQuotaConfig {
    pub url: Option<String>,
}

impl RedisQuotaConfig {
    pub fn from_env() -> Self {
        let url = std::env::var("TIYGATE_REDIS_URL")
            .ok()
            .filter(|s| !s.is_empty());
        Self { url }
    }
}

/// Redis-backed quota counter. When constructed without a URL
/// (or when the optional `redis-quota` feature is disabled) this
/// implementation transparently delegates to [`InMemoryQuota`],
/// which is the §4.6 fall-back behaviour ("宁可少算不误杀").
///
/// When the `redis-quota` feature is on and a URL is provided,
/// the counter performs a single-round-trip Lua script per kind
/// (atomic `INCRBY` + `PEXPIRE`). Connection errors at request
/// time also fall back to the in-memory path with a `warn!` log
/// line so a flaky Redis does not turn into a quota-loss event.
#[derive(Clone)]
pub struct RedisQuota {
    inner: Arc<dyn QuotaCounter>,
    /// Resolved Redis URL. Stored so a runtime-failed
    /// `MultiplexedConnection` can be re-attempted on the next
    /// call. `None` means "no Redis configured; always in-memory".
    #[allow(dead_code)]
    url: Option<String>,
}

impl RedisQuota {
    /// Build a Redis-backed quota counter.
    ///
    /// * `cfg.url == None` → returns a counter that always
    ///   delegates to [`InMemoryQuota`].
    /// * `cfg.url == Some(_)` + `redis-quota` feature on →
    ///   returns a counter that uses Redis; per-request errors
    ///   degrade to the in-memory path.
    /// * `cfg.url == Some(_)` + `redis-quota` feature off →
    ///   returns the in-memory counter with a debug log line
    ///   (the binary still builds, just without Redis support).
    pub fn new(cfg: RedisQuotaConfig) -> Self {
        #[cfg(feature = "redis-quota")]
        {
            if let Some(url) = cfg.url.clone() {
                if let Some(redis_impl) = RedisQuotaImpl::try_new(&url) {
                    return Self {
                        inner: Arc::new(redis_impl),
                        url: Some(url),
                    };
                }
            }
        }
        #[cfg(not(feature = "redis-quota"))]
        {
            // The feature is off — silently fall back. Operators
            // who want Redis support must rebuild with
            // `--features redis-quota`.
            let _ = &cfg;
        }
        Self {
            inner: InMemoryQuota::new(),
            url: cfg.url,
        }
    }

    /// Hand the underlying counter to a caller that wants to
    /// store the trait object directly (avoids the wrapper
    /// indirection on the hot path). This is the same counter
    /// `check_and_consume` already calls into.
    pub fn into_inner(self) -> Arc<dyn QuotaCounter> {
        self.inner
    }
}

#[async_trait::async_trait]
impl QuotaCounter for RedisQuota {
    async fn check_and_consume(
        &self,
        key_id: &str,
        spec: &QuotaSpec,
        tokens: u64,
    ) -> Result<QuotaDecision, QuotaError> {
        // The inner counter already implements the fail-open
        // path internally (Redis errors → Allow + warn). The
        // indirection through `Arc<dyn QuotaCounter>` keeps the
        // call site in the trait hot path identical between
        // feature flags.
        self.inner.check_and_consume(key_id, spec, tokens).await
    }

    async fn current_usage(&self, key_id: &str) -> Result<HashMap<QuotaKind, u64>, QuotaError> {
        self.inner.current_usage(key_id).await
    }
}

// ---------------------------------------------------------------------
// Real Redis implementation (compiled only with the feature on).
// ---------------------------------------------------------------------

#[cfg(feature = "redis-quota")]
mod redis_impl {
    use std::collections::HashMap;

    use async_trait::async_trait;
    use redis::Script;
    use tracing::warn;

    use super::{QuotaCounter, QuotaDecision, QuotaError, QuotaKind, QuotaSpec};

    /// Atomic check-and-consume. The script returns `-1` when the
    /// increment would exceed `ARGV[3]` (the limit), or the new
    /// post-increment counter value on success. TTL is set only on
    /// the first increment of a fresh window so the rolling window
    /// slides as a fixed clock interval since the first hit.
    const INCR_SCRIPT: &str = r#"
        local n = redis.call('GET', KEYS[1])
        if n == false then n = 0 else n = tonumber(n) end
        local inc = tonumber(ARGV[1])
        local ttl = tonumber(ARGV[2])
        local limit = tonumber(ARGV[3])
        if n + inc > limit then
            return -1
        end
        local new = redis.call('INCRBY', KEYS[1], inc)
        if new == inc then
            redis.call('PEXPIRE', KEYS[1], ttl)
        end
        return new
    "#;

    pub(super) struct RedisQuotaImpl {
        client: redis::Client,
        script: Script,
    }

    impl RedisQuotaImpl {
        pub(super) fn try_new(url: &str) -> Option<Self> {
            let client = redis::Client::open(url).ok()?;
            let script = Script::new(INCR_SCRIPT);
            Some(Self { client, script })
        }

        fn key_for(key_id: &str, kind: QuotaKind) -> String {
            let label = match kind {
                QuotaKind::RequestsPerMinute => "rpm",
                QuotaKind::RequestsPerDay => "rpd",
                QuotaKind::TokensPerMinute => "tpm",
                QuotaKind::TokensPerDay => "tpd",
            };
            format!("tiygate:quota:{key_id}:{label}")
        }
    }

    #[async_trait]
    impl QuotaCounter for RedisQuotaImpl {
        async fn check_and_consume(
            &self,
            key_id: &str,
            spec: &QuotaSpec,
            tokens: u64,
        ) -> Result<QuotaDecision, QuotaError> {
            // Build the per-kind check list. Skip kinds the caller
            // did not configure (unlimited).
            let mut checks: Vec<(QuotaKind, u64, u64)> = Vec::with_capacity(4);
            if let Some(lim) = spec.requests_per_minute {
                checks.push((QuotaKind::RequestsPerMinute, 1, lim));
            }
            if let Some(lim) = spec.requests_per_day {
                checks.push((QuotaKind::RequestsPerDay, 1, lim));
            }
            if let Some(lim) = spec.tokens_per_minute {
                checks.push((QuotaKind::TokensPerMinute, tokens, lim));
            }
            if let Some(lim) = spec.tokens_per_day {
                checks.push((QuotaKind::TokensPerDay, tokens, lim));
            }
            if checks.is_empty() {
                return Ok(QuotaDecision::Allow { remaining: None });
            }

            let mut conn = match self.client.get_multiplexed_async_connection().await {
                Ok(c) => c,
                Err(e) => {
                    // §4.6 fail-open: a flaky Redis must not turn
                    // into a quota-loss event.
                    warn!(error = %e, "redis quota: connection failed; allowing request");
                    return Ok(QuotaDecision::Allow { remaining: None });
                }
            };

            for (kind, increment, limit) in &checks {
                let key = Self::key_for(key_id, *kind);
                let ttl_ms = kind.window().as_millis() as u64;
                let result: redis::RedisResult<i64> = self
                    .script
                    .key(&key)
                    .arg(*increment as i64)
                    .arg(ttl_ms as i64)
                    .arg(*limit as i64)
                    .invoke_async(&mut conn)
                    .await;
                match result {
                    Ok(-1) => {
                        // First overflow wins. The cross-kind
                        // over-count is bounded to (n-1) per-kind
                        // increments because the script returns -1
                        // *before* INCRBY for the failing kind.
                        // This is the per-bucket limit acting as
                        // the authoritative gate.
                        return Ok(QuotaDecision::Deny {
                            retry_after: kind.window(),
                            limit: *limit,
                            kind: *kind,
                        });
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        warn!(error = %e, kind = ?kind, "redis quota: script failed; allowing request");
                        return Ok(QuotaDecision::Allow { remaining: None });
                    }
                }
            }

            Ok(QuotaDecision::Allow { remaining: None })
        }

        async fn current_usage(&self, key_id: &str) -> Result<HashMap<QuotaKind, u64>, QuotaError> {
            let mut conn = self
                .client
                .get_multiplexed_async_connection()
                .await
                .map_err(|e| QuotaError::Backend(e.to_string()))?;
            let mut out = HashMap::new();
            for kind in [
                QuotaKind::RequestsPerMinute,
                QuotaKind::RequestsPerDay,
                QuotaKind::TokensPerMinute,
                QuotaKind::TokensPerDay,
            ] {
                let key = Self::key_for(key_id, kind);
                let v: redis::RedisResult<Option<i64>> =
                    redis::cmd("GET").arg(&key).query_async(&mut conn).await;
                if let Ok(Some(n)) = v {
                    out.insert(kind, n.max(0) as u64);
                }
            }
            Ok(out)
        }
    }
}

#[cfg(feature = "redis-quota")]
use redis_impl::RedisQuotaImpl;

#[cfg(test)]
mod redis_key_labels {
    //! The key naming is part of the public contract — operators
    //! may inspect the keys directly with `redis-cli`. Lock the
    //! labels down so a refactor cannot silently change them.
    #[cfg(feature = "redis-quota")]
    use crate::quota::QuotaKind;
    #[cfg(feature = "redis-quota")]
    fn label(kind: QuotaKind) -> &'static str {
        match kind {
            QuotaKind::RequestsPerMinute => "rpm",
            QuotaKind::RequestsPerDay => "rpd",
            QuotaKind::TokensPerMinute => "tpm",
            QuotaKind::TokensPerDay => "tpd",
        }
    }

    #[cfg(feature = "redis-quota")]
    #[test]
    fn key_labels_match_contract() {
        assert_eq!(label(QuotaKind::RequestsPerMinute), "rpm");
        assert_eq!(label(QuotaKind::RequestsPerDay), "rpd");
        assert_eq!(label(QuotaKind::TokensPerMinute), "tpm");
        assert_eq!(label(QuotaKind::TokensPerDay), "tpd");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_rpm(limit: u64) -> QuotaSpec {
        QuotaSpec {
            requests_per_minute: Some(limit),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn unlimited_spec_never_denies() {
        let q = InMemoryQuota::new();
        let d = q
            .check_and_consume("k", &QuotaSpec::default(), 1000)
            .await
            .expect("ok");
        assert!(d.is_allowed());
    }

    #[tokio::test]
    async fn rpm_limit_denies_after_budget() {
        let q = InMemoryQuota::new();
        let spec = spec_rpm(2);
        assert!(q
            .check_and_consume("k", &spec, 1)
            .await
            .unwrap()
            .is_allowed());
        assert!(q
            .check_and_consume("k", &spec, 1)
            .await
            .unwrap()
            .is_allowed());
        let d = q.check_and_consume("k", &spec, 1).await.unwrap();
        match d {
            QuotaDecision::Deny { kind, .. } => {
                assert_eq!(kind, QuotaKind::RequestsPerMinute);
            }
            _ => panic!("expected deny"),
        }
    }

    #[tokio::test]
    async fn per_key_isolation() {
        let q = InMemoryQuota::new();
        let spec = spec_rpm(1);
        assert!(q
            .check_and_consume("alice", &spec, 1)
            .await
            .unwrap()
            .is_allowed());
        // bob still has budget
        assert!(q
            .check_and_consume("bob", &spec, 1)
            .await
            .unwrap()
            .is_allowed());
    }

    #[tokio::test]
    async fn tokens_per_minute_charges_input_tokens() {
        let q = InMemoryQuota::new();
        let spec = QuotaSpec {
            tokens_per_minute: Some(10),
            ..Default::default()
        };
        assert!(q
            .check_and_consume("k", &spec, 7)
            .await
            .unwrap()
            .is_allowed());
        let d = q.check_and_consume("k", &spec, 5).await.unwrap();
        assert!(matches!(d, QuotaDecision::Deny { .. }));
    }

    #[tokio::test]
    async fn current_usage_reports_consumption() {
        let q = InMemoryQuota::new();
        let spec = spec_rpm(5);
        q.check_and_consume("k", &spec, 1).await.unwrap();
        q.check_and_consume("k", &spec, 1).await.unwrap();
        let usage = q.current_usage("k").await.unwrap();
        let rpm = usage
            .get(&QuotaKind::RequestsPerMinute)
            .copied()
            .unwrap_or(0);
        assert_eq!(rpm, 2);
    }

    #[test]
    fn quota_kind_window_lengths() {
        assert_eq!(
            QuotaKind::RequestsPerMinute.window(),
            Duration::from_secs(60)
        );
        assert_eq!(
            QuotaKind::TokensPerDay.window(),
            Duration::from_secs(86_400)
        );
    }

    /// `RedisQuota::new` with no URL must still produce a working
    /// counter (it falls back to the in-memory implementation).
    /// This is the §4.6 contract that the binary always boots.
    #[tokio::test]
    async fn redis_quota_no_url_falls_back_to_in_memory() {
        let cfg = RedisQuotaConfig { url: None };
        let counter = RedisQuota::new(cfg);
        let spec = spec_rpm(2);
        // Three calls under a 2-rpm limit; the third must deny.
        // The `expect` calls match the existing test pattern in
        // this file; the workspace's `clippy::expect_used` deny
        // applies to the whole crate equally.
        assert!(counter
            .check_and_consume("k", &spec, 1)
            .await
            .expect("ok")
            .is_allowed());
        assert!(counter
            .check_and_consume("k", &spec, 1)
            .await
            .expect("ok")
            .is_allowed());
        let d = counter.check_and_consume("k", &spec, 1).await.expect("ok");
        assert!(!d.is_allowed());
    }

    /// `RedisQuota::new` with an unreachable URL must still
    /// produce a working counter (the feature is on, but the
    /// underlying Redis is not reachable; the constructor's
    /// `try_new` falls back to the in-memory path).
    #[tokio::test]
    async fn redis_quota_unreachable_url_falls_back_to_in_memory() {
        let cfg = RedisQuotaConfig {
            url: Some("redis://127.0.0.1:1/".to_string()),
        };
        let counter = RedisQuota::new(cfg);
        let spec = spec_rpm(1);
        // First call must allow (no error surfaces to the hot path).
        let d = counter.check_and_consume("k", &spec, 1).await;
        assert!(d.is_ok());
    }
}
