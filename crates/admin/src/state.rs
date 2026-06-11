//! Shared application state for the Admin API handlers.

use std::sync::Arc;

use std::collections::HashMap;
use tokio::sync::Mutex;

use tiygate_store::config_store::DbConfigStore;
use tiygate_store::db::DbPool;

/// Application state passed to every Admin API handler.
#[derive(Clone)]
pub struct AdminState {
    pub store: Arc<DbConfigStore>,
    pub pool: Arc<DbPool>,
    /// Optional reference to the data-plane health registry so
    /// the admin API can report per-target circuit-breaker
    /// status (§4.4 / §8 acceptance).
    pub health: Option<Arc<tiygate_core::routing::HealthRegistry>>,
    /// Optional reference to the live quota counter so the admin
    /// API can report real-time per-key usage (§4.6). When the
    /// control plane runs without a quota backend wired in this is
    /// `None` and the single-key GET handler omits live usage.
    pub quota: Option<Arc<dyn tiygate_core::quota::QuotaCounter>>,
    /// In-memory store of OAuth 2.0 authorization-code flow
    /// state. The `start` handler mints a `state` nonce, the
    /// `callback` handler validates the incoming `state` query
    /// parameter against this map, and the entry is removed
    /// once the callback is processed (success or failure). The
    /// map is process-local; multi-replica deployments must
    /// place an external store (Redis, DB) behind this — Phase 5+.
    ///
    /// We use `tokio::sync::Mutex` (not `parking_lot::RwLock`)
    /// because the admin handlers are async and the lock must
    /// be `Send + Sync` across `.await` points.
    pub oauth_pending: Arc<Mutex<HashMap<String, OAuthPendingFlow>>>,
}

/// A pending OAuth 2.0 authorization-code flow awaiting the
/// provider's redirect. The `state` value is the CSRF-protection
/// nonce minted by the `start` handler; the `verifier` is the
/// PKCE code verifier that the `callback` handler passes back
/// to the token endpoint. The `provider_id` is the upstream
/// provider the flow is bound to.
#[derive(Clone)]
pub struct OAuthPendingFlow {
    pub provider_id: String,
    /// The PKCE code-verifier secret (string form). The provider
    /// will trade this + the `code` query param for an access
    /// token. Per RFC 7636, the verifier is a high-entropy
    /// random string the client generated; we hold the original
    /// value in memory until the callback completes.
    pub verifier: String,
}

impl AdminState {
    pub fn new(
        store: Arc<DbConfigStore>,
        pool: Arc<DbPool>,
        health: Option<Arc<tiygate_core::routing::HealthRegistry>>,
    ) -> Self {
        Self {
            store,
            pool,
            health,
            quota: None,
            oauth_pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Attach a live quota counter so the single-key GET handler can
    /// surface real-time usage. Returns `self` for chaining.
    pub fn with_quota(
        mut self,
        quota: Option<Arc<dyn tiygate_core::quota::QuotaCounter>>,
    ) -> Self {
        self.quota = quota;
        self
    }
}
