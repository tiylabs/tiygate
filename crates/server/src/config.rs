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
/// The enum now lives in `tiygate_core::routing` so the routing table and the
/// persistence layer can carry a strongly typed per-route strategy override.
/// We re-export it here to keep the existing `crate::config::RoutingStrategyName`
/// references working unchanged.
pub use tiygate_core::routing::RoutingStrategyName;

/// Payload detail archive configuration. When enabled, the control
/// plane starts a background worker that uploads large request detail
/// fields to an S3-compatible object store and clears the DB copies
/// only after upload succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadArchiveConfig {
    pub enabled: bool,
    pub s3_endpoint: Option<String>,
    pub s3_region: String,
    pub s3_bucket: Option<String>,
    pub s3_access_key_id: Option<String>,
    pub s3_secret_access_key: Option<String>,
    pub s3_prefix: String,
    pub s3_force_path_style: bool,
    pub scan_interval_secs: u64,
    pub batch_size: usize,
    pub concurrency: usize,
    pub timeout_secs: u64,
    pub max_retries: i32,
}

impl Default for PayloadArchiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            s3_endpoint: None,
            s3_region: "us-east-1".to_string(),
            s3_bucket: None,
            s3_access_key_id: None,
            s3_secret_access_key: None,
            s3_prefix: String::new(),
            s3_force_path_style: true,
            scan_interval_secs: 60,
            batch_size: 100,
            concurrency: 4,
            timeout_secs: 30,
            max_retries: 5,
        }
    }
}

impl PayloadArchiveConfig {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("TIYGATE_PAYLOAD_ARCHIVE_ENABLED") {
            cfg.enabled = parse_bool(&v);
        }
        cfg.s3_endpoint = non_empty_env("TIYGATE_PAYLOAD_ARCHIVE_S3_ENDPOINT");
        if let Some(v) = non_empty_env("TIYGATE_PAYLOAD_ARCHIVE_S3_REGION") {
            cfg.s3_region = v;
        }
        cfg.s3_bucket = non_empty_env("TIYGATE_PAYLOAD_ARCHIVE_S3_BUCKET");
        cfg.s3_access_key_id = non_empty_env("TIYGATE_PAYLOAD_ARCHIVE_S3_ACCESS_KEY_ID");
        cfg.s3_secret_access_key = non_empty_env("TIYGATE_PAYLOAD_ARCHIVE_S3_SECRET_ACCESS_KEY");
        if let Some(v) = non_empty_env("TIYGATE_PAYLOAD_ARCHIVE_S3_PREFIX") {
            cfg.s3_prefix = v;
        }
        if let Ok(v) = std::env::var("TIYGATE_PAYLOAD_ARCHIVE_S3_FORCE_PATH_STYLE") {
            cfg.s3_force_path_style = parse_bool(&v);
        }
        if let Some(n) = parse_env("TIYGATE_PAYLOAD_ARCHIVE_SCAN_INTERVAL_SECS") {
            cfg.scan_interval_secs = n;
        }
        if let Some(n) = parse_env("TIYGATE_PAYLOAD_ARCHIVE_BATCH_SIZE") {
            cfg.batch_size = n;
        }
        if let Some(n) = parse_env("TIYGATE_PAYLOAD_ARCHIVE_CONCURRENCY") {
            cfg.concurrency = n;
        }
        if let Some(n) = parse_env("TIYGATE_PAYLOAD_ARCHIVE_TIMEOUT_SECS") {
            cfg.timeout_secs = n;
        }
        if let Some(n) = parse_env("TIYGATE_PAYLOAD_ARCHIVE_MAX_RETRIES") {
            cfg.max_retries = n;
        }
        cfg
    }

    pub fn is_complete(&self) -> bool {
        !self.enabled
            || (self.s3_endpoint.is_some()
                && self.s3_bucket.is_some()
                && self.s3_access_key_id.is_some()
                && self.s3_secret_access_key.is_some())
    }
}

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
    /// TCP keepalive probe interval for the shared upstream HTTP
    /// client, in seconds. Enables OS-level TCP keepalive so half-dead
    /// connections (silently reaped by a peer or middlebox) are
    /// detected before they are reused for a long streaming response.
    /// Set to 0 to disable. Default: 60s. Set via
    /// `TIYGATE_UPSTREAM_TCP_KEEPALIVE_SECS`.
    pub upstream_tcp_keepalive_secs: u64,
    /// Idle timeout for pooled upstream connections, in seconds. Idle
    /// connections older than this are dropped from the pool, lowering
    /// the chance of reusing a stale connection. Set to 0 to disable.
    /// Default: 90s. Set via `TIYGATE_UPSTREAM_POOL_IDLE_TIMEOUT_SECS`.
    pub upstream_pool_idle_timeout_secs: u64,
    /// Whether to disable Nagle's algorithm (TCP_NODELAY) on upstream
    /// connections, reducing forwarding latency for small SSE frames.
    /// Default: true. Set via `TIYGATE_UPSTREAM_TCP_NODELAY`.
    pub upstream_tcp_nodelay: bool,
    /// Default routing strategy (default `Weighted`, per §3.4).
    ///
    /// This is the gateway-wide fallback: a virtual model whose route
    /// carries no strategy override (`routing_strategy = NULL`) is served
    /// with this strategy. Set via `TIYGATE_ROUTING_STRATEGY`.
    pub routing_strategy: RoutingStrategyName,
    /// Database URL for the control plane. When unset, the server
    /// runs in legacy in-memory mode (no admin router, no log
    /// retention, no quota counters).
    pub database_url: Option<String>,
    /// Whether to capture inline media (base64) inside raw envelopes
    /// (default `false` — store metadata only, per §4.1).
    pub raw_envelope_capture_media: bool,
    /// Whether to require a valid API key on every data-plane request
    /// (default `true`). When `true`, requests without a credential,
    /// with an unknown credential, or with a disabled credential are
    /// rejected with 401/403 before reaching the upstream. When
    /// `false`, the gateway falls back to the legacy anonymous path
    /// (unlimited quota, no 401/403). Set via
    /// `TIYGATE_REQUIRE_API_KEY`.
    pub require_api_key: bool,
    /// Payload detail archiving to S3-compatible object storage.
    pub payload_archive: PayloadArchiveConfig,
    /// Extra header names appended to the request-direction (client →
    /// provider) forwarding denylist, on top of the hardcoded
    /// defaults. Lowercase, set via `TIYGATE_FORWARD_REQUEST_HEADER_DENY`.
    pub forward_request_header_deny_extra: Vec<String>,
    /// Extra header names appended to the response-direction (provider
    /// → client) forwarding denylist, on top of the hardcoded
    /// defaults. Lowercase, set via `TIYGATE_FORWARD_RESPONSE_HEADER_DENY`.
    pub forward_response_header_deny_extra: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:3000".to_string(),
            mode: DeployMode::All,
            max_request_body_bytes: 10 * 1024 * 1024, // 10 MiB
            max_multimodal_body_bytes: 64 * 1024 * 1024, // 64 MiB
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
            upstream_tcp_keepalive_secs: 60,
            upstream_pool_idle_timeout_secs: 90,
            upstream_tcp_nodelay: true,
            routing_strategy: RoutingStrategyName::Weighted,
            database_url: None,
            raw_envelope_capture_media: false,
            require_api_key: true,
            payload_archive: PayloadArchiveConfig::default(),
            forward_request_header_deny_extra: Vec::new(),
            forward_response_header_deny_extra: Vec::new(),
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
        if let Ok(v) = std::env::var("TIYGATE_MAX_MULTIMODAL_BODY_BYTES") {
            if let Ok(n) = v.parse() {
                cfg.max_multimodal_body_bytes = n;
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
        if let Ok(v) = std::env::var("TIYGATE_UPSTREAM_TCP_KEEPALIVE_SECS") {
            if let Ok(n) = v.parse() {
                cfg.upstream_tcp_keepalive_secs = n;
            }
        }
        if let Ok(v) = std::env::var("TIYGATE_UPSTREAM_POOL_IDLE_TIMEOUT_SECS") {
            if let Ok(n) = v.parse() {
                cfg.upstream_pool_idle_timeout_secs = n;
            }
        }
        if let Ok(v) = std::env::var("TIYGATE_UPSTREAM_TCP_NODELAY") {
            cfg.upstream_tcp_nodelay = parse_bool(&v);
        }
        if let Ok(v) = std::env::var("TIYGATE_DATABASE_URL") {
            let trimmed = v.trim().to_string();
            if !trimmed.is_empty() {
                cfg.database_url = Some(trimmed);
            }
        }
        if let Ok(v) = std::env::var("TIYGATE_RAW_ENVELOPE_CAPTURE_MEDIA") {
            cfg.raw_envelope_capture_media = parse_bool(&v);
        }
        if let Ok(v) = std::env::var("TIYGATE_REQUIRE_API_KEY") {
            cfg.require_api_key = parse_bool(&v);
        }
        cfg.payload_archive = PayloadArchiveConfig::from_env();
        if let Ok(v) = std::env::var("TIYGATE_FORWARD_REQUEST_HEADER_DENY") {
            cfg.forward_request_header_deny_extra = parse_header_list(&v);
        }
        if let Ok(v) = std::env::var("TIYGATE_FORWARD_RESPONSE_HEADER_DENY") {
            cfg.forward_response_header_deny_extra = parse_header_list(&v);
        }

        cfg
    }
}

fn parse_bool(raw: &str) -> bool {
    matches!(
        raw.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn parse_env<T>(name: &str) -> Option<T>
where
    T: std::str::FromStr,
{
    std::env::var(name).ok()?.parse().ok()
}

/// Parse a comma-separated header-name list into a normalized
/// (trimmed, lowercased, non-empty) `Vec<String>`.
fn parse_header_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}
