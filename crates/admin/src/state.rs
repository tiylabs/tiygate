//! Shared application state for the Admin API handlers.

use std::sync::Arc;

use std::collections::HashMap;
use tokio::sync::Mutex;

use tiygate_auth::provider_oauth::OAuthCredentialService;
use tiygate_store::archive::PayloadArchiveClient;
use tiygate_store::config_store::DbConfigStore;
use tiygate_store::db::DbPool;

use crate::brute_force::{BruteForceConfig, BruteForceLimiter, InMemoryBruteForceLimiter};

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
    /// Optional S3-compatible payload archive reader used by replay
    /// requests when the DB row has already been archived.
    pub payload_archive: Option<Arc<dyn PayloadArchiveClient>>,
    /// Optional model catalog snapshot store used for status reads.
    pub model_catalog: Option<Arc<tiygate_store::model_catalog::ModelCatalogStore>>,
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
    /// Cluster-aware OAuth token service injected by the server. Production
    /// refresh paths use this service so Admin requests cannot bypass the same
    /// coordination used by the data plane and background worker.
    pub oauth_service: Option<Arc<dyn OAuthCredentialService>>,
    /// Brute-force protection limiter. Defaults to an in-memory
    /// limiter; the server binary may inject a Redis-backed one via
    /// [`AdminState::with_bf_limiter`] when `TIYGATE_REDIS_URL` is
    /// set.
    pub bf_limiter: Arc<dyn BruteForceLimiter>,
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
            payload_archive: None,
            model_catalog: None,
            oauth_pending: Arc::new(Mutex::new(HashMap::new())),
            oauth_service: None,
            bf_limiter: Arc::new(InMemoryBruteForceLimiter::new(BruteForceConfig::default())),
        }
    }

    /// Attach a live quota counter so the single-key GET handler can
    /// surface real-time usage. Returns `self` for chaining.
    pub fn with_quota(mut self, quota: Option<Arc<dyn tiygate_core::quota::QuotaCounter>>) -> Self {
        self.quota = quota;
        self
    }

    /// Attach a payload archive client for replay reads.
    pub fn with_payload_archive(mut self, archive: Option<Arc<dyn PayloadArchiveClient>>) -> Self {
        self.payload_archive = archive;
        self
    }

    /// Attach a model catalog store for status reads.
    pub fn with_model_catalog(
        mut self,
        catalog: Option<Arc<tiygate_store::model_catalog::ModelCatalogStore>>,
    ) -> Self {
        self.model_catalog = catalog;
        self
    }

    pub fn with_oauth_service(mut self, service: Arc<dyn OAuthCredentialService>) -> Self {
        self.oauth_service = Some(service);
        self
    }

    /// Attach a brute-force protection limiter. The server binary
    /// calls this with a Redis-backed limiter when
    /// `TIYGATE_REDIS_URL` is configured; the default is the
    /// in-memory limiter created in [`AdminState::new`].
    pub fn with_bf_limiter(mut self, limiter: Arc<dyn BruteForceLimiter>) -> Self {
        self.bf_limiter = limiter;
        self
    }
}
