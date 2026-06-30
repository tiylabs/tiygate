//! TiyGate Store — configuration storage, encrypted secrets, and the
//! pluggable log-sink surface.
//!
//! Phase 4 (产品化) introduces:
//! * A sqlx-backed config store with two independent migration
//!   sequences (config + log) so configuration and telemetry data
//!   can be evolved separately (design doc §4.3).
//! * AES-256-GCM encryption of provider API keys and OAuth
//!   refresh tokens, keyed by an environment-supplied master key and
//!   HKDF-derived per-purpose subkeys (design doc §4.5).
//! * A pluggable log sink trait — the `OltpSink` writes to the
//!   `request_logs` table; the legacy `StdoutSink` is kept for
//!   dev / debugging flows.
//! * Background retention cleanup (design doc §4.3) and an audit
//!   log for admin write operations.

pub mod archive;
pub mod audit;
pub mod config_store;
pub mod db;
pub mod encryption;
pub mod keys;
pub mod log_sink;
pub mod model_catalog;
pub mod models;
pub mod retention;
pub mod settings_keys;
pub mod sqlite_maintenance;
pub mod token_stats;

// Re-export the legacy in-memory `ConfigStore` so existing callers
// (ingress.rs, app.rs) keep working without churn. The DB-backed
// store lives under `config_store::DbConfigStore`; production code
// reaches it through `App`.
pub mod config {
    pub use super::config_store::ConfigStore;
}
