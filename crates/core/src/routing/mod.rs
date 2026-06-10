//! Routing layer — routing table, strategies, health registry, and fallback policies.
//!
//! The routing system maps virtual model names to ordered chains of `RoutingTarget`s.
//! Strategies determine which target to try first; health registry tracks per-instance
//! circuit breaker state; fallback policy controls error handling and retry behavior.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::protocol::ProtocolEndpoint;

/// A single routing target — one hop in the routing chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingTarget {
    /// Provider identifier (e.g., "openai", "anthropic").
    pub provider_id: String,
    /// The specific model name to send to the upstream.
    pub model_id: String,
    /// The API base URL.
    pub api_base: String,
    /// The API key (redacted in logs/debug).
    #[serde(skip_serializing)]
    pub api_key: String,
    /// The protocol to use for this target.
    pub api_protocol: ProtocolEndpoint,
    /// Account label for multi-account routing.
    pub account_label: Option<String>,
    /// Override API key (set by route hooks).
    #[serde(default, skip)]
    pub api_key_override: Option<String>,
    /// Override API base (set by route hooks).
    #[serde(default, skip)]
    pub api_base_override: Option<String>,
    /// Weight for weighted routing strategy.
    pub weight: f64,
}

impl RoutingTarget {
    /// The effective API key, considering overrides.
    pub fn effective_api_key(&self) -> &str {
        self.api_key_override.as_deref().unwrap_or(&self.api_key)
    }

    /// The effective API base URL, considering overrides.
    pub fn effective_api_base(&self) -> &str {
        self.api_base_override.as_deref().unwrap_or(&self.api_base)
    }

    /// The health registry key for this target.
    pub fn health_key(&self) -> String {
        format!("{}:{}", self.provider_id, self.model_id)
    }
}

/// A routing table mapping virtual model names to ordered target chains.
#[derive(Debug, Clone)]
pub struct RoutingTable {
    /// Virtual model name → ordered list of routing targets (fallback chain).
    pub routes: HashMap<String, Vec<RoutingTarget>>,
}

impl RoutingTable {
    /// Create an empty routing table.
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
        }
    }

    /// Look up the routing chain for a virtual model name.
    pub fn resolve(&self, virtual_model: &str) -> Option<Vec<RoutingTarget>> {
        self.routes.get(virtual_model).cloned()
    }

    /// Register a route.
    pub fn insert(&mut self, virtual_model: String, targets: Vec<RoutingTarget>) {
        self.routes.insert(virtual_model, targets);
    }
}

impl Default for RoutingTable {
    fn default() -> Self {
        Self::new()
    }
}

/// Health status of a routing target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingTargetHealth {
    /// Target is healthy and can receive traffic.
    Healthy,
    /// Target has been circuit-broken (consecutive failures).
    CircuitBroken { until: Instant },
    /// Target is in cooling period after a rate limit.
    Cooling { until: Instant },
}

/// Per-instance health registry with circuit breaker semantics.
///
/// Tracks consecutive failures per target. After `failure_threshold` consecutive
/// failures, the target is circuit-broken for `recovery_after` duration.
/// A successful request during the half-open window restores health.
///
/// State is per-instance only; not shared across replicas.
pub struct HealthRegistry {
    states: RwLock<HashMap<String, TargetHealth>>,
    latencies: RwLock<HashMap<String, LatencyEwma>>,
    failure_threshold: u32,
    recovery_after: Duration,
}

struct TargetHealth {
    consecutive_failures: u32,
    last_failure_at: Option<Instant>,
    cooling_until: Option<Instant>,
    cooling_reason: Option<String>,
}

#[derive(Debug, Clone)]
struct LatencyEwma {
    ewma: f64,
    samples: u64,
}

impl HealthRegistry {
    /// Create a new health registry with default settings.
    pub fn new(failure_threshold: u32, recovery_after: Duration) -> Self {
        Self {
            states: RwLock::new(HashMap::new()),
            latencies: RwLock::new(HashMap::new()),
            failure_threshold,
            recovery_after,
        }
    }

    /// Create with defaults: 3 consecutive failures, 30s recovery.
    pub fn with_defaults() -> Self {
        Self::new(3, Duration::from_secs(30))
    }

    /// Check if a target is currently healthy.
    pub fn is_healthy(&self, target_key: &str) -> bool {
        let states = self.states.read();
        let now = Instant::now();

        match states.get(target_key) {
            None => true,
            Some(state) => {
                // Check cooling first
                if let Some(until) = state.cooling_until {
                    if now < until {
                        return false;
                    }
                }
                // If under threshold or recovery period elapsed, healthy
                state.consecutive_failures < self.failure_threshold
                    || state
                        .last_failure_at
                        .is_none_or(|t| now.duration_since(t) > self.recovery_after)
            }
        }
    }

    /// Record a successful request.
    pub fn record_success(&self, target_key: &str) {
        let mut states = self.states.write();
        if let Some(state) = states.get_mut(target_key) {
            state.consecutive_failures = 0;
            state.cooling_until = None;
            state.cooling_reason = None;
        }
    }

    /// Record a failed request.
    pub fn record_failure(&self, target_key: &str) {
        let mut states = self.states.write();
        let state = states
            .entry(target_key.to_string())
            .or_insert(TargetHealth {
                consecutive_failures: 0,
                last_failure_at: None,
                cooling_until: None,
                cooling_reason: None,
            });
        state.consecutive_failures += 1;
        state.last_failure_at = Some(Instant::now());
    }

    /// Apply a cooling period (e.g., from RateLimited with Retry-After).
    pub fn apply_cooling(&self, target_key: &str, duration: Duration, reason: &str) {
        let mut states = self.states.write();
        let state = states
            .entry(target_key.to_string())
            .or_insert(TargetHealth {
                consecutive_failures: 0,
                last_failure_at: None,
                cooling_until: None,
                cooling_reason: None,
            });
        state.cooling_until = Some(Instant::now() + duration);
        state.cooling_reason = Some(reason.to_string());
    }

    /// Get detailed health status for a target.
    pub fn health_status(&self, target_key: &str) -> RoutingTargetHealth {
        let states = self.states.read();
        let now = Instant::now();

        match states.get(target_key) {
            None => RoutingTargetHealth::Healthy,
            Some(state) => {
                if let Some(until) = state.cooling_until {
                    if now < until {
                        return RoutingTargetHealth::Cooling { until };
                    }
                }
                if state.consecutive_failures >= self.failure_threshold {
                    if let Some(last_failure) = state.last_failure_at {
                        let recovery_until = last_failure + self.recovery_after;
                        if now < recovery_until {
                            return RoutingTargetHealth::CircuitBroken {
                                until: recovery_until,
                            };
                        }
                    }
                }
                RoutingTargetHealth::Healthy
            }
        }
    }

    /// Reset all health state (for testing).
    pub fn reset(&self) {
        self.states.write().clear();
        self.latencies.write().clear();
    }

    /// Record a successful request latency observation (in milliseconds).
    /// Uses EWMA with α=0.3 (recent observations weighted more heavily).
    pub fn record_latency_ms(&self, target_key: &str, latency_ms: u64) {
        let mut latencies = self.latencies.write();
        let entry = latencies.entry(target_key.to_string()).or_insert(LatencyEwma {
            ewma: 0.0,
            samples: 0,
        });
        if entry.samples == 0 {
            entry.ewma = latency_ms as f64;
        } else {
            entry.ewma = 0.3 * (latency_ms as f64) + 0.7 * entry.ewma;
        }
        entry.samples += 1;
    }

    /// Get the current EWMA latency in milliseconds for a target.
    /// Returns None if no samples have been recorded yet.
    pub fn ewma_latency_ms(&self, target_key: &str) -> Option<u64> {
        self.latencies
            .read()
            .get(target_key)
            .map(|l| l.ewma as u64)
    }

    /// Number of latency samples recorded for the target.
    pub fn latency_samples(&self, target_key: &str) -> u64 {
        self.latencies
            .read()
            .get(target_key)
            .map(|l| l.samples)
            .unwrap_or(0)
    }
}

/// Error classification for fallback decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Transient error (5xx, timeout, transport) — may retry/transfer.
    Transient,
    /// Rate limited (429) — transfer to different target, apply cooling.
    RateLimited,
    /// Authentication error (401/403) — don't retry same account, try different.
    Auth,
    /// Bad request (400/422) — fail immediately, don't transfer.
    BadRequest,
    /// Capability mismatch (lossy conversion or unsupported feature).
    LossyOrCapability,
}

/// Classification of an error for fallback purposes.
#[derive(Debug, Clone)]
pub struct ErrorClassification {
    pub class: ErrorClass,
    /// Optional Retry-After duration from upstream headers.
    pub retry_after: Option<Duration>,
    /// Original HTTP status code if applicable.
    pub http_status: Option<u16>,
}

/// Decision from the fallback policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackDecision {
    /// Try the next target in the chain.
    TryNext,
    /// Retry the same target.
    Retry,
    /// Fail the request — no more attempts.
    Fail,
}

/// Fallback policy — determines what to do when execution fails.
///
/// Implements refined error classification: Transient → transfer,
/// RateLimited → transfer + cooling, Auth → transfer different account,
/// BadRequest → fail, Lossy → skip or fail.
pub trait FallbackPolicy: Send + Sync {
    /// Classify an error and decide the fallback action.
    fn classify(
        &self,
        error: &crate::Error,
        target: &RoutingTarget,
        attempt: usize,
        max_attempts: usize,
        bytes_emitted: u64,
    ) -> FallbackDecision;
}

/// Retry policy for same-target retries.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum retry attempts on the same target.
    pub max_retries: usize,
    /// Base delay for exponential backoff.
    pub base_delay: Duration,
    /// Maximum delay between retries.
    pub max_delay: Duration,
}

impl RetryPolicy {
    /// Default retry policy: 2 retries, 1s base, 30s max.
    pub fn with_defaults() -> Self {
        Self {
            max_retries: 2,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }

    /// Compute delay for the Nth retry with exponential backoff + jitter.
    pub fn delay_for(&self, retry_num: usize) -> Duration {
        let exp = 2u64.pow(retry_num as u32);
        let delay = self.base_delay * exp as u32;
        let clamped = delay.min(self.max_delay);
        // Add jitter: ±25%
        let jitter = rand::random::<f64>() * 0.5 * clamped.as_secs_f64();
        let jittered = clamped.as_secs_f64() * 0.75 + jitter;
        Duration::from_secs_f64(jittered)
    }
}

/// Routing strategy trait — determines target selection order.
pub trait Strategy: Send + Sync {
    /// Sort/select targets from the routing chain.
    /// Returns targets in the order they should be tried.
    fn order<'a>(&self, targets: &'a [RoutingTarget]) -> Vec<&'a RoutingTarget>;
}

/// Weighted random selection strategy.
pub struct WeightedStrategy;

impl Strategy for WeightedStrategy {
    fn order<'a>(&self, targets: &'a [RoutingTarget]) -> Vec<&'a RoutingTarget> {
        if targets.is_empty() {
            return vec![];
        }
        let total_weight: f64 = targets.iter().map(|t| t.weight.max(0.0)).sum();
        if total_weight <= 0.0 {
            return targets.iter().collect();
        }
        // Weighted random shuffle: pick targets in weighted order
        let mut remaining: Vec<(usize, &RoutingTarget)> = targets.iter().enumerate().collect();
        let mut result = Vec::with_capacity(targets.len());
        let mut rng = rand::thread_rng();
        while !remaining.is_empty() {
            let total: f64 = remaining.iter().map(|(_, t)| t.weight.max(0.0)).sum();
            let mut pick = rand::Rng::gen_range(&mut rng, 0.0..total);
            let mut chosen_idx = 0;
            for (i, (_, t)) in remaining.iter().enumerate() {
                pick -= t.weight.max(0.0);
                if pick <= 0.0 {
                    chosen_idx = i;
                    break;
                }
            }
            let (_, target) = remaining.remove(chosen_idx);
            result.push(target);
        }
        result
    }
}

/// Priority-based strategy (targets ordered by explicit priority, then weight).
pub struct PriorityStrategy;

impl Strategy for PriorityStrategy {
    fn order<'a>(&self, targets: &'a [RoutingTarget]) -> Vec<&'a RoutingTarget> {
        // Priority is implicit in the order; just return as-is for priority grouping
        let mut sorted: Vec<&RoutingTarget> = targets.iter().collect();
        sorted.sort_by(|a, b| {
            b.weight
                .partial_cmp(&a.weight)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        sorted
    }
}

/// Cooldown-aware strategy: prefers targets not in cooldown/circuit-broken.
pub struct CooldownStrategy {
    health: Arc<HealthRegistry>,
}

impl CooldownStrategy {
    pub fn new(health: Arc<HealthRegistry>) -> Self {
        Self { health }
    }
}

impl Strategy for CooldownStrategy {
    fn order<'a>(&self, targets: &'a [RoutingTarget]) -> Vec<&'a RoutingTarget> {
        let mut sorted: Vec<&RoutingTarget> = targets.iter().collect();
        sorted.sort_by_key(|t| {
            if self.health.is_healthy(&t.health_key()) {
                0u8
            } else {
                1u8
            }
        });
        sorted
    }
}

/// Latency-aware strategy: prefers targets with the lowest historical latency.
/// Tracks an EWMA (exponentially weighted moving average) of latency per
/// health-key. Unobserved targets (no samples yet) are preferred to avoid
/// starving new alternatives.
pub struct LatencyStrategy {
    health: Arc<HealthRegistry>,
}

impl LatencyStrategy {
    pub fn new(health: Arc<HealthRegistry>) -> Self {
        Self { health }
    }
}

impl Strategy for LatencyStrategy {
    fn order<'a>(&self, targets: &'a [RoutingTarget]) -> Vec<&'a RoutingTarget> {
        // Healthy targets first (lower key = preferred),
        // then by EWMA latency (lowest first),
        // then unobserved (None latency) before high-latency.
        let mut sorted: Vec<&RoutingTarget> = targets.iter().collect();
        sorted.sort_by_key(|t| {
            let healthy = if self.health.is_healthy(&t.health_key()) {
                0u32
            } else {
                1u32
            };
            let latency = self.health.ewma_latency_ms(&t.health_key());
            // u128 prevents overflow when combining healthy + latency_key.
            let latency_key: u128 = match latency {
                Some(ms) => (ms as u128) & 0x0000_FFFF_FFFF_FFFF_FFFF_FFFF_FFFFu128,
                // unobserved: no data yet → try first to gather samples.
                // Sort key is smaller than any realistic observed latency.
                None => 0u128,
            };
            ((healthy as u128) << 64) | latency_key
        });
        sorted
    }
}

/// Default fallback policy implementation.
pub struct DefaultFallbackPolicy {
    pub max_total_attempts: usize,
    pub deadline: Duration,
    pub retry_policy: RetryPolicy,
}

impl DefaultFallbackPolicy {
    pub fn new(max_total_attempts: usize, deadline: Duration, retry_policy: RetryPolicy) -> Self {
        Self {
            max_total_attempts,
            deadline,
            retry_policy,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(4, Duration::from_secs(120), RetryPolicy::with_defaults())
    }
}

impl FallbackPolicy for DefaultFallbackPolicy {
    fn classify(
        &self,
        error: &crate::Error,
        _target: &RoutingTarget,
        attempt: usize,
        max_attempts: usize,
        bytes_emitted: u64,
    ) -> FallbackDecision {
        // If we've already emitted stream bytes, don't retry/transfer
        if bytes_emitted > 0 {
            return FallbackDecision::Fail;
        }

        // Check total attempt budget
        if attempt >= max_attempts.min(self.max_total_attempts) {
            return FallbackDecision::Fail;
        }

        // Classify the error
        let class = classify_error(error);

        match class.class {
            ErrorClass::Transient => {
                // Try next target
                FallbackDecision::TryNext
            }
            ErrorClass::RateLimited => {
                // §3.4: RateLimited (429) → switch to the next target and
                // apply cooling on the current one. The caller (handler)
                // honors upstream `Retry-After` via HealthRegistry::apply_cooling
                // before re-attempting the same target via a later iteration
                // of the routing chain. We do *not* retry the same target
                // immediately, because the upstream just told us it is
                // saturated — hammering it again would amplify the pressure
                // the gateway is supposed to be relieving.
                FallbackDecision::TryNext
            }
            ErrorClass::Auth => {
                // Don't retry same account; TryNext will skip same-account targets
                FallbackDecision::TryNext
            }
            ErrorClass::BadRequest | ErrorClass::LossyOrCapability => {
                // Fail immediately
                FallbackDecision::Fail
            }
        }
    }
}

/// Classify a core error into an error class for fallback decisions.
pub fn classify_error(error: &crate::Error) -> ErrorClassification {
    let msg = error.to_string().to_lowercase();

    // Check for rate limiting
    if msg.contains("429") || msg.contains("rate limit") || msg.contains("rate_limited") {
        return ErrorClassification {
            class: ErrorClass::RateLimited,
            retry_after: None,
            http_status: Some(429),
        };
    }

    // Check for auth errors
    if msg.contains("401") || msg.contains("403") || msg.contains("unauthorized") {
        return ErrorClassification {
            class: ErrorClass::Auth,
            retry_after: None,
            http_status: Some(401),
        };
    }

    // Check for bad request
    if msg.contains("400") || msg.contains("422") || msg.contains("bad request") {
        return ErrorClassification {
            class: ErrorClass::BadRequest,
            retry_after: None,
            http_status: Some(400),
        };
    }

    // Check for lossy/capability
    if msg.contains("lossy") || msg.contains("capability") || msg.contains("unsupported") {
        return ErrorClassification {
            class: ErrorClass::LossyOrCapability,
            retry_after: None,
            http_status: None,
        };
    }

    // Default: transient (5xx, timeout, transport)
    ErrorClassification {
        class: ErrorClass::Transient,
        retry_after: None,
        http_status: None,
    }
}
