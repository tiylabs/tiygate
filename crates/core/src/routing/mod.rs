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
use crate::telemetry::RequestErrorClass;

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
    /// OAuth configuration. `Some` when the provider's `auth_mode`
    /// is `OAuth` and a refresh token is available. The data-plane
    /// auth path checks this field first; when `Some`, it uses the
    /// `OAuthTokenCache` instead of the static key path.
    #[serde(default, skip)]
    pub oauth: Option<crate::provider::oauth::OAuthTargetConfig>,
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

/// Routing strategy selector.
///
/// §3.4 names `Weighted` as the document-level default; we expose all four so
/// operators can pick the one that matches their traffic shape without code
/// changes. `Latency`/`Cooldown` need a `HealthRegistry` handle, which the
/// handler supplies when it constructs the concrete `Strategy`.
///
/// This type lives in `core` (rather than the server crate) so the routing
/// table — and the persistence layer that feeds it — can carry a strongly
/// typed per-route strategy override without depending on the server crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategyName {
    /// Weighted random shuffle (default per §3.4).
    #[default]
    Weighted,
    /// Sort by weight desc — useful for tiered providers.
    Priority,
    /// Prefer healthy targets, then by weight.
    Cooldown,
    /// Prefer healthy + lowest EWMA latency.
    Latency,
}

impl RoutingStrategyName {
    /// The canonical `snake_case` token for this strategy. Matches the
    /// serde representation and the persisted DB column value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Weighted => "weighted",
            Self::Priority => "priority",
            Self::Cooldown => "cooldown",
            Self::Latency => "latency",
        }
    }

    /// Parse a `snake_case` token into a strategy. Unknown tokens return
    /// `None`, letting callers fall back to the global default rather than
    /// failing the whole config load.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "weighted" => Some(Self::Weighted),
            "priority" => Some(Self::Priority),
            "cooldown" => Some(Self::Cooldown),
            "latency" => Some(Self::Latency),
            _ => None,
        }
    }
}

/// A single entry in the routing table: the ordered target chain for a
/// virtual model plus an optional per-route strategy override.
///
/// `strategy == None` means "inherit the gateway's default strategy"
/// (the `routing_strategy` configured on `ServerConfig`/`AppState`).
#[derive(Debug, Clone)]
pub struct RouteEntry {
    /// Ordered list of routing targets (fallback chain).
    pub targets: Vec<RoutingTarget>,
    /// Optional per-route strategy override. `None` → inherit default.
    pub strategy: Option<RoutingStrategyName>,
}

impl RouteEntry {
    /// Construct an entry with no strategy override (inherits default).
    pub fn new(targets: Vec<RoutingTarget>) -> Self {
        Self {
            targets,
            strategy: None,
        }
    }
}

/// A routing table mapping virtual model names to ordered target chains.
#[derive(Debug, Clone)]
pub struct RoutingTable {
    /// Virtual model name → route entry (target chain + optional strategy).
    pub routes: HashMap<String, RouteEntry>,
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
        self.routes.get(virtual_model).map(|e| e.targets.clone())
    }

    /// Look up the per-route strategy override for a virtual model.
    /// Returns `None` when the route is missing *or* carries no override
    /// — both cases mean "use the gateway default strategy".
    pub fn resolve_strategy(&self, virtual_model: &str) -> Option<RoutingStrategyName> {
        self.routes.get(virtual_model).and_then(|e| e.strategy)
    }

    /// Borrow the full route entry (targets + strategy) for a virtual model.
    pub fn resolve_entry(&self, virtual_model: &str) -> Option<&RouteEntry> {
        self.routes.get(virtual_model)
    }

    /// Register a route with no strategy override (inherits default).
    pub fn insert(&mut self, virtual_model: String, targets: Vec<RoutingTarget>) {
        self.routes.insert(virtual_model, RouteEntry::new(targets));
    }

    /// Register a route entry carrying targets and an optional strategy.
    pub fn insert_entry(&mut self, virtual_model: String, entry: RouteEntry) {
        self.routes.insert(virtual_model, entry);
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
/// failures, the target is circuit-broken with exponential backoff recovery:
/// each successive half-open probe failure escalates the recovery window
/// through `recovery_tiers` (e.g. 60s → 180s → 600s → 1800s).
/// A successful request during the half-open window restores health and
/// resets the backoff to the first tier.
///
/// State is per-instance only; not shared across replicas.
pub struct HealthRegistry {
    states: RwLock<HashMap<String, TargetHealth>>,
    latencies: RwLock<HashMap<String, LatencyEwma>>,
    failure_threshold: u32,
    recovery_tiers: Vec<Duration>,
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
    /// Create a new health registry with the given failure threshold and recovery tiers.
    ///
    /// `recovery_tiers` defines the escalating backoff durations after circuit-break.
    /// The first tier applies when `consecutive_failures` first reaches the threshold;
    /// each subsequent half-open probe failure advances to the next tier.
    /// The last tier is the ceiling and repeats indefinitely.
    ///
    /// # Panics
    /// Panics if `recovery_tiers` is empty.
    pub fn new(failure_threshold: u32, recovery_tiers: Vec<Duration>) -> Self {
        assert!(
            !recovery_tiers.is_empty(),
            "recovery_tiers must not be empty"
        );
        Self {
            states: RwLock::new(HashMap::new()),
            latencies: RwLock::new(HashMap::new()),
            failure_threshold,
            recovery_tiers,
        }
    }

    /// Create with defaults: 3 consecutive failures, exponential backoff 60s → 180s → 600s → 1800s.
    pub fn with_defaults() -> Self {
        Self::new(
            3,
            vec![
                Duration::from_secs(60),
                Duration::from_secs(180),
                Duration::from_secs(600),
                Duration::from_secs(1800),
            ],
        )
    }

    /// Compute the recovery duration for the current failure count.
    ///
    /// When `consecutive_failures` first reaches `failure_threshold`, the first
    /// tier is used. Each additional failure beyond the threshold advances one
    /// tier. The last tier is the ceiling.
    fn recovery_duration_for(&self, consecutive_failures: u32) -> Duration {
        let overflow = consecutive_failures.saturating_sub(self.failure_threshold) as usize;
        let tier_index = overflow.min(self.recovery_tiers.len() - 1);
        self.recovery_tiers[tier_index]
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
                    || state.last_failure_at.is_none_or(|t| {
                        now.duration_since(t)
                            > self.recovery_duration_for(state.consecutive_failures)
                    })
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
                        let recovery_until =
                            last_failure + self.recovery_duration_for(state.consecutive_failures);
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

    /// Return the consecutive failure count for a target.
    pub fn consecutive_failures(&self, target_key: &str) -> u32 {
        self.states
            .read()
            .get(target_key)
            .map(|s| s.consecutive_failures)
            .unwrap_or(0)
    }

    /// Return the cooling reason, if any.
    pub fn cooling_reason(&self, target_key: &str) -> Option<String> {
        self.states
            .read()
            .get(target_key)
            .and_then(|s| s.cooling_reason.clone())
    }

    /// Return the failure threshold that triggers circuit breaking.
    pub fn failure_threshold(&self) -> u32 {
        self.failure_threshold
    }

    /// Reset all health state (for testing).
    pub fn reset(&self) {
        self.states.write().clear();
        self.latencies.write().clear();
    }

    /// Return all target keys currently tracked in the registry.
    /// Used by the admin API to expose circuit-breaker status (§4.4).
    pub fn list_targets(&self) -> Vec<String> {
        self.states.read().keys().cloned().collect()
    }

    /// Record a successful request latency observation (in milliseconds).
    /// Uses EWMA with α=0.3 (recent observations weighted more heavily).
    pub fn record_latency_ms(&self, target_key: &str, latency_ms: u64) {
        let mut latencies = self.latencies.write();
        let entry = latencies
            .entry(target_key.to_string())
            .or_insert(LatencyEwma {
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
        self.latencies.read().get(target_key).map(|l| l.ewma as u64)
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

impl ErrorClass {
    /// Map this fallback-internal class to the canonical
    /// `RequestErrorClass` used in the request log / telemetry.
    pub fn to_request_class(self) -> RequestErrorClass {
        match self {
            Self::Transient => RequestErrorClass::Transient,
            Self::RateLimited => RequestErrorClass::RateLimited,
            Self::Auth => RequestErrorClass::UpstreamAuth,
            Self::BadRequest => RequestErrorClass::BadRequest,
            Self::LossyOrCapability => RequestErrorClass::LossyOrCapability,
        }
    }
}

/// Classification of an error for fallback purposes.
#[derive(Debug, Clone)]
pub struct ErrorClassification {
    /// The request-log canonical error class. This is the value
    /// persisted on `RequestEvent.error_class` and surfaced in the
    /// admin console.
    pub class: RequestErrorClass,
    /// The fallback-internal class used by `DefaultFallbackPolicy`
    /// to decide retry / transfer / fail. Kept alongside `class`
    /// so the policy match stays on the stable `ErrorClass` enum.
    pub fallback_class: ErrorClass,
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
        Self::new(10, Duration::from_secs(120), RetryPolicy::with_defaults())
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

        match class.fallback_class {
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
            class: RequestErrorClass::RateLimited,
            fallback_class: ErrorClass::RateLimited,
            retry_after: None,
            http_status: Some(429),
        };
    }

    // Check for auth errors
    if msg.contains("401") || msg.contains("403") || msg.contains("unauthorized") {
        return ErrorClassification {
            class: RequestErrorClass::UpstreamAuth,
            fallback_class: ErrorClass::Auth,
            retry_after: None,
            http_status: Some(401),
        };
    }

    // Check for bad request
    if msg.contains("400") || msg.contains("422") || msg.contains("bad request") {
        return ErrorClassification {
            class: RequestErrorClass::BadRequest,
            fallback_class: ErrorClass::BadRequest,
            retry_after: None,
            http_status: Some(400),
        };
    }

    // Check for lossy/capability
    if msg.contains("lossy") || msg.contains("capability") || msg.contains("unsupported") {
        return ErrorClassification {
            class: RequestErrorClass::LossyOrCapability,
            fallback_class: ErrorClass::LossyOrCapability,
            retry_after: None,
            http_status: None,
        };
    }

    // Default: transient (5xx, timeout, transport)
    ErrorClassification {
        class: RequestErrorClass::Transient,
        fallback_class: ErrorClass::Transient,
        retry_after: None,
        http_status: None,
    }
}
