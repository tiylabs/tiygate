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

        cfg
    }
}
