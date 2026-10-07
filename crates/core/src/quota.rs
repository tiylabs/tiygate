//! Quota tracking — token / request counters keyed by API key.
//!
//! Phase 4 (产品化) of the design implements a pluggable
//! [`QuotaCounter`] trait and a zero-I/O in-memory implementation:
//!
//! * [`InMemoryQuota`] — per-instance counters using `parking_lot`;
//!   suitable for single-replica deployments and tests.
//! * Concrete Redis quota I/O is implemented by the store crate behind
//!   this trait, keeping database dependencies out of core.
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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
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
}
