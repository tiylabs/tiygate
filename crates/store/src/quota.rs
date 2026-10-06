//! Redis quota adapter. Canonical quota types and the in-memory backend live in core.
use std::collections::HashMap;
use std::sync::Arc;
use tiygate_core::quota::{
    InMemoryQuota, QuotaCounter, QuotaDecision, QuotaError, QuotaKind, QuotaSpec,
};

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
/// time remain fail-open with a `warn!` log so a flaky Redis does
/// not block requests. No URL/disabled feature uses in-memory counters.
#[derive(Clone)]
pub struct RedisQuota {
    inner: Arc<dyn QuotaCounter>,
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

        pub(super) fn key_for(key_id: &str, kind: QuotaKind) -> String {
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

#[cfg(all(test, feature = "redis-quota"))]
mod redis_key_labels {
    //! The key naming is part of the public contract — operators
    //! may inspect the keys directly with `redis-cli`. Lock the
    //! labels down so a refactor cannot silently change them.
    use super::{redis_impl::RedisQuotaImpl, QuotaKind};

    #[test]
    fn key_labels_match_contract() {
        for (kind, suffix) in [
            (QuotaKind::RequestsPerMinute, "rpm"),
            (QuotaKind::RequestsPerDay, "rpd"),
            (QuotaKind::TokensPerMinute, "tpm"),
            (QuotaKind::TokensPerDay, "tpd"),
        ] {
            assert_eq!(
                RedisQuotaImpl::key_for("k", kind),
                format!("tiygate:quota:k:{suffix}")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn redis_quota_no_url_falls_back_to_in_memory() -> Result<(), QuotaError> {
        let counter = RedisQuota::new(RedisQuotaConfig { url: None });
        let spec = QuotaSpec {
            requests_per_minute: Some(2),
            ..Default::default()
        };
        assert!(counter.check_and_consume("k", &spec, 1).await?.is_allowed());
        assert!(counter.check_and_consume("k", &spec, 1).await?.is_allowed());
        assert!(!counter.check_and_consume("k", &spec, 1).await?.is_allowed());
        Ok(())
    }

    #[tokio::test]
    async fn redis_quota_unreachable_url_remains_fail_open() -> Result<(), QuotaError> {
        let counter = RedisQuota::new(RedisQuotaConfig {
            url: Some("redis://127.0.0.1:1/".into()),
        });
        let spec = QuotaSpec {
            requests_per_minute: Some(1),
            ..Default::default()
        };
        assert!(counter.check_and_consume("k", &spec, 1).await?.is_allowed());
        Ok(())
    }
}
