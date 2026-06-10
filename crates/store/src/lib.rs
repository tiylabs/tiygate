//! TiyGate Store — Configuration storage and log persistence.
//!
//! Manages OLTP configuration (providers, routes, API keys) and
//! provides pluggable log sinks (default: SQLite/PostgreSQL partitioned tables).

pub mod config;
pub mod encryption;
pub mod log_sink;
