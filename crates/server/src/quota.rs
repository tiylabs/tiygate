//! Quota wiring — bridges the gateway to the `QuotaCounter` trait
//! implemented in `tiygate_core::quota`. The default production
//! backend is the in-memory counter; multi-replica deployments
//! swap in the Redis-backed implementation via
//! `QuotaCounter::from_env`.

pub use tiygate_core::quota::{
    InMemoryQuota, QuotaCounter, QuotaDecision, QuotaError, QuotaKind, QuotaSpec, RedisQuota,
    RedisQuotaConfig,
};
