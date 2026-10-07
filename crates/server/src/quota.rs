//! Quota wiring: canonical traits/in-memory counters from core and the
//! concrete Redis adapter from store. The server's `redis-quota` feature
//! enables Redis I/O without introducing a database dependency into core.
pub use tiygate_core::quota::{
    InMemoryQuota, QuotaCounter, QuotaDecision, QuotaError, QuotaKind, QuotaSpec,
};
pub use tiygate_store::quota::{RedisQuota, RedisQuotaConfig};
