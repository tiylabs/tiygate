//! Server configuration (CLI args, env vars, deployment mode).

use serde::{Deserialize, Serialize};

/// Deployment mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployMode {
    /// Single process — control + data plane in one.
    All,
    /// Data plane only.
    Proxy,
    /// Control plane only.
    Admin,
}

/// Routing strategy selector.
///
/// §3.4 names `Weighted` as the document-level default; we expose all four so
/// operators can pick the one that matches their traffic shape without code
/// changes. LatencyStrategy needs a `HealthRegistry` handle, which the
/// `strategy_arg` config string captures statically — the corresponding
/// strategy is constructed inside the handler where the registry is available.
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

/// Server configuration.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ServerConfig {
    /// Listen address.
    pub listen_addr: String,
    /// Deployment mode.
    pub mode: DeployMode,
    /// Max request body size in bytes (default 10 MiB).
    pub max_request_body_bytes: u64,
    /// Max multimodal body size in bytes (default 32 MiB).
    pub max_multimodal_body_bytes: u64,
    /// Request read timeout in seconds.
    pub request_read_timeout_secs: u64,
    /// Max concurrent requests.
    pub max_inflight_requests: usize,
    /// Max queue depth.
    pub max_queue_depth: usize,
    /// Queue acquire timeout in seconds.
    pub acquire_timeout_secs: u64,
    /// Drain timeout in seconds.
    pub drain_timeout_secs: u64,
    /// Idle timeout for upstream streaming responses, in seconds.
    /// The streaming handler emits a keepalive if no chunk arrives
    /// for this long and then closes the stream with a protocol-native
    /// end frame once the configured idle window has fully elapsed
    /// without activity. Default: 120s.
    pub upstream_stream_idle_timeout_secs: u64,
    /// Total wall-clock timeout for upstream streaming responses, in
    /// seconds. When the budget elapses the streaming handler closes
    /// the stream with a protocol-native error frame. Set to 0 to
    /// disable the total budget entirely. Default: 0 (disabled).
    pub upstream_stream_total_timeout_secs: u64,
    /// Routing strategy (default `Weighted`, per §3.4).
    pub routing_strategy: RoutingStrategyName,
    /// Database URL for the control plane. When unset, the server
    /// runs in legacy in-memory mode (no admin router, no log
    /// retention, no quota counters).
    pub database_url: Option<String>,
    /// Max bytes for the raw envelope body before truncation
    /// (default 256 KiB). Per design doc §4.1.
    pub raw_envelope_max_bytes: u64,
    /// Whether to capture inline media (base64) inside raw envelopes
    /// (default `false` — store metadata only, per §4.1).
    pub raw_envelope_capture_media: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:3000".to_string(),
            mode: DeployMode::All,
            max_request_body_bytes: 10 * 1024 * 1024, // 10 MiB
            max_multimodal_body_bytes: 32 * 1024 * 1024, // 32 MiB
            request_read_timeout_secs: 30,
            max_inflight_requests: 1024,
            max_queue_depth: 256,
            acquire_timeout_secs: 5,
            drain_timeout_secs: 30,
            // 120s idle gives a reasonable safety net against proxies
            // and slow upstreams; 0 disables the total budget so the
            // operator opts in to a wall-clock cap explicitly.
            upstream_stream_idle_timeout_secs: 120,
            upstream_stream_total_timeout_secs: 0,
            routing_strategy: RoutingStrategyName::Weighted,
            database_url: None,
            raw_envelope_max_bytes: 256 * 1024, // 256 KiB
            raw_envelope_capture_media: false,
        }
    }
}

impl ServerConfig {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(addr) = std::env::var("TIYGATE_LISTEN_ADDR") {
            cfg.listen_addr = addr;
        }
        if let Ok(mode) = std::env::var("TIYGATE_MODE") {
            cfg.mode = match mode.as_str() {
                "proxy" => DeployMode::Proxy,
                "admin" => DeployMode::Admin,
                _ => DeployMode::All,
            };
        }
        if let Ok(v) = std::env::var("TIYGATE_MAX_BODY_BYTES") {
            if let Ok(n) = v.parse() {
                cfg.max_request_body_bytes = n;
            }
        }
        if let Ok(v) = std::env::var("TIYGATE_MAX_INFLIGHT") {
            if let Ok(n) = v.parse() {
                cfg.max_inflight_requests = n;
            }
        }
        if let Ok(v) = std::env::var("TIYGATE_ROUTING_STRATEGY") {
            cfg.routing_strategy = match v.to_ascii_lowercase().as_str() {
                "weighted" => RoutingStrategyName::Weighted,
                "priority" => RoutingStrategyName::Priority,
                "cooldown" => RoutingStrategyName::Cooldown,
                "latency" => RoutingStrategyName::Latency,
                _ => RoutingStrategyName::Weighted,
            };
        }
        if let Ok(v) = std::env::var("TIYGATE_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS") {
            if let Ok(n) = v.parse() {
                cfg.upstream_stream_idle_timeout_secs = n;
            }
        }
        if let Ok(v) = std::env::var("TIYGATE_UPSTREAM_STREAM_TOTAL_TIMEOUT_SECS") {
            if let Ok(n) = v.parse() {
                cfg.upstream_stream_total_timeout_secs = n;
            }
        }
        if let Ok(v) = std::env::var("TIYGATE_DATABASE_URL") {
            let trimmed = v.trim().to_string();
            if !trimmed.is_empty() {
                cfg.database_url = Some(trimmed);
            }
        }
        if let Ok(v) = std::env::var("TIYGATE_RAW_ENVELOPE_MAX_BYTES") {
            if let Ok(n) = v.parse() {
                cfg.raw_envelope_max_bytes = n;
            }
        }
        if let Ok(v) = std::env::var("TIYGATE_RAW_ENVELOPE_CAPTURE_MEDIA") {
            cfg.raw_envelope_capture_media =
                matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
        }

        cfg
    }
}
