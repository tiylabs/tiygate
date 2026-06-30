//! Canonical settings key constants and typed-read helpers.
//!
//! Every migratable runtime parameter lives in the `settings` table
//! under a dotted `gateway.<category>.<name>` key. This module is the
//! single source of truth for those key strings so the store, admin,
//! server bootstrap, and background tasks never drift.

use crate::config_store::{DbConfigStore, StoreError};

// --- Retention ---
pub const RETENTION_INTERVAL_SECS: &str = "gateway.retention.interval_secs";
pub const RETENTION_LOG_RETENTION_DAYS: &str = "gateway.retention.log_retention_days";

// --- SQLite maintenance ---
pub const SQLITE_MAINTENANCE_ENABLED: &str = "gateway.sqlite_maintenance.enabled";
pub const SQLITE_MAINTENANCE_INTERVAL_SECS: &str = "gateway.sqlite_maintenance.interval_secs";
pub const SQLITE_MAINTENANCE_VACUUM_ENABLED: &str = "gateway.sqlite_maintenance.vacuum_enabled";
pub const SQLITE_MAINTENANCE_MIN_FREELIST_PAGES: &str =
    "gateway.sqlite_maintenance.min_freelist_pages";
pub const SQLITE_MAINTENANCE_MIN_FREE_RATIO_PERCENT: &str =
    "gateway.sqlite_maintenance.min_free_ratio_percent";

// --- Epoch poll ---
pub const EPOCH_POLL_INTERVAL_SECS: &str = "gateway.epoch_poll.interval_secs";

// --- Token stats ---
pub const TOKEN_STATS_INTERVAL_SECS: &str = "gateway.token_stats.interval_secs";
pub const TOKEN_STATS_LOOKBACK_DAYS: &str = "gateway.token_stats.lookback_days";

// --- Payload archive ---
pub const ARCHIVE_ENABLED: &str = "gateway.archive.enabled";
pub const ARCHIVE_S3_ENDPOINT: &str = "gateway.archive.s3_endpoint";
pub const ARCHIVE_S3_REGION: &str = "gateway.archive.s3_region";
pub const ARCHIVE_S3_BUCKET: &str = "gateway.archive.s3_bucket";
pub const ARCHIVE_S3_ACCESS_KEY_ID: &str = "gateway.archive.s3_access_key_id";
pub const ARCHIVE_S3_SECRET_ACCESS_KEY: &str = "gateway.archive.s3_secret_access_key";
pub const ARCHIVE_S3_PREFIX: &str = "gateway.archive.s3_prefix";
pub const ARCHIVE_S3_FORCE_PATH_STYLE: &str = "gateway.archive.s3_force_path_style";
pub const ARCHIVE_SCAN_INTERVAL_SECS: &str = "gateway.archive.scan_interval_secs";
pub const ARCHIVE_BATCH_SIZE: &str = "gateway.archive.batch_size";
pub const ARCHIVE_CONCURRENCY: &str = "gateway.archive.concurrency";
pub const ARCHIVE_TIMEOUT_SECS: &str = "gateway.archive.timeout_secs";
pub const ARCHIVE_MAX_RETRIES: &str = "gateway.archive.max_retries";

// --- Routing ---
pub const ROUTING_DEFAULT_STRATEGY: &str = "gateway.routing.default_strategy";

// --- Ingress ---
pub const INGRESS_MAX_BODY_BYTES: &str = "gateway.ingress.max_body_bytes";
pub const INGRESS_MAX_INFLIGHT: &str = "gateway.ingress.max_inflight";
pub const INGRESS_MAX_QUEUE_DEPTH: &str = "gateway.ingress.max_queue_depth";
pub const INGRESS_ACQUIRE_TIMEOUT_SECS: &str = "gateway.ingress.acquire_timeout_secs";
pub const INGRESS_RAW_ENVELOPE_CAPTURE_MEDIA: &str = "gateway.ingress.raw_envelope_capture_media";
pub const INGRESS_REQUIRE_API_KEY: &str = "gateway.ingress.require_api_key";

// --- Upstream ---
pub const UPSTREAM_STREAM_IDLE_TIMEOUT_SECS: &str = "gateway.upstream.stream_idle_timeout_secs";
pub const UPSTREAM_STREAM_TOTAL_TIMEOUT_SECS: &str = "gateway.upstream.stream_total_timeout_secs";
/// Time-to-first-byte timeout for upstream streaming requests, in
/// seconds. When non-zero, the streaming branch wraps
/// `client.execute()` in `tokio::time::timeout` so a non-responsive
/// upstream (no response headers) is bounded independently of the
/// streaming idle timer. Set to 0 to disable. Default: 120s.
pub const UPSTREAM_TTFB_TIMEOUT_SECS: &str = "gateway.upstream.ttfb_timeout_secs";
pub const UPSTREAM_TCP_KEEPALIVE_SECS: &str = "gateway.upstream.tcp_keepalive_secs";
pub const UPSTREAM_POOL_IDLE_TIMEOUT_SECS: &str = "gateway.upstream.pool_idle_timeout_secs";
pub const UPSTREAM_TCP_NODELAY: &str = "gateway.upstream.tcp_nodelay";

// --- Forward header deny lists ---
pub const FORWARD_REQUEST_HEADER_DENY: &str = "gateway.forward.request_header_deny";
pub const FORWARD_RESPONSE_HEADER_DENY: &str = "gateway.forward.response_header_deny";

/// All non-encrypted settings keys, in stable order. Used by the
/// bootstrap path to seed defaults and by tests.
pub const PLAIN_KEYS: &[&str] = &[
    RETENTION_INTERVAL_SECS,
    RETENTION_LOG_RETENTION_DAYS,
    SQLITE_MAINTENANCE_ENABLED,
    SQLITE_MAINTENANCE_INTERVAL_SECS,
    SQLITE_MAINTENANCE_VACUUM_ENABLED,
    SQLITE_MAINTENANCE_MIN_FREELIST_PAGES,
    SQLITE_MAINTENANCE_MIN_FREE_RATIO_PERCENT,
    EPOCH_POLL_INTERVAL_SECS,
    TOKEN_STATS_INTERVAL_SECS,
    TOKEN_STATS_LOOKBACK_DAYS,
    ARCHIVE_ENABLED,
    ARCHIVE_S3_ENDPOINT,
    ARCHIVE_S3_REGION,
    ARCHIVE_S3_BUCKET,
    ARCHIVE_S3_PREFIX,
    ARCHIVE_S3_FORCE_PATH_STYLE,
    ARCHIVE_SCAN_INTERVAL_SECS,
    ARCHIVE_BATCH_SIZE,
    ARCHIVE_CONCURRENCY,
    ARCHIVE_TIMEOUT_SECS,
    ARCHIVE_MAX_RETRIES,
    ROUTING_DEFAULT_STRATEGY,
    INGRESS_MAX_BODY_BYTES,
    INGRESS_MAX_INFLIGHT,
    INGRESS_MAX_QUEUE_DEPTH,
    INGRESS_ACQUIRE_TIMEOUT_SECS,
    INGRESS_RAW_ENVELOPE_CAPTURE_MEDIA,
    INGRESS_REQUIRE_API_KEY,
    UPSTREAM_STREAM_IDLE_TIMEOUT_SECS,
    UPSTREAM_STREAM_TOTAL_TIMEOUT_SECS,
    UPSTREAM_TTFB_TIMEOUT_SECS,
    UPSTREAM_TCP_KEEPALIVE_SECS,
    UPSTREAM_POOL_IDLE_TIMEOUT_SECS,
    UPSTREAM_TCP_NODELAY,
    FORWARD_REQUEST_HEADER_DENY,
    FORWARD_RESPONSE_HEADER_DENY,
];

/// All encrypted settings keys.
pub const ENCRYPTED_KEYS: &[&str] = &[ARCHIVE_S3_ACCESS_KEY_ID, ARCHIVE_S3_SECRET_ACCESS_KEY];

/// Returns true when the key stores an encrypted blob.
pub fn is_encrypted_key(key: &str) -> bool {
    ENCRYPTED_KEYS.contains(&key)
}

// --- typed read helpers ---

/// Read a setting as `u64`. Falls back to `default` when the key is
/// absent or unparseable.
pub async fn get_u64(store: &DbConfigStore, key: &str, default: u64) -> u64 {
    match store.get_setting(key).await {
        Ok(Some(v)) => v.parse().unwrap_or(default),
        _ => default,
    }
}

/// Read a setting as `i64`. Falls back to `default` when absent or
/// unparseable.
pub async fn get_i64(store: &DbConfigStore, key: &str, default: i64) -> i64 {
    match store.get_setting(key).await {
        Ok(Some(v)) => v.parse().unwrap_or(default),
        _ => default,
    }
}

/// Read a setting as `usize`.
pub async fn get_usize(store: &DbConfigStore, key: &str, default: usize) -> usize {
    match store.get_setting(key).await {
        Ok(Some(v)) => v.parse().unwrap_or(default),
        _ => default,
    }
}

/// Read a setting as `bool` ("true"/"false", case-insensitive).
pub async fn get_bool(store: &DbConfigStore, key: &str, default: bool) -> bool {
    match store.get_setting(key).await {
        Ok(Some(v)) => v.eq_ignore_ascii_case("true"),
        _ => default,
    }
}

/// Read a setting as `String`, returning `default` when absent.
pub async fn get_string(store: &DbConfigStore, key: &str, default: &str) -> String {
    match store.get_setting(key).await {
        Ok(Some(v)) => v,
        _ => default.to_string(),
    }
}

/// Read a setting as an optional `String`: `None` when the key is
/// absent or empty.
pub async fn get_opt_string(store: &DbConfigStore, key: &str) -> Option<String> {
    match store.get_setting(key).await {
        Ok(Some(v)) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Read a comma-separated setting into a `Vec<String>`.
pub async fn get_string_list(store: &DbConfigStore, key: &str, default: &[String]) -> Vec<String> {
    match store.get_setting(key).await {
        Ok(Some(v)) => {
            if v.is_empty() {
                default.to_vec()
            } else {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            }
        }
        _ => default.to_vec(),
    }
}

/// Best-effort write: store a plain setting value. Errors are
/// returned so the caller can decide whether to abort bootstrap.
pub async fn set_str(store: &DbConfigStore, key: &str, value: &str) -> Result<(), StoreError> {
    store.set_setting(key, value).await
}
