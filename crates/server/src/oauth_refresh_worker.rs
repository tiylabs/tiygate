//! Stateless OAuth credential keepalive worker.
//!
//! Every gateway instance may run this loop. PostgreSQL instances compete on
//! provider-scoped advisory locks and SQLite is coordinated by the shared
//! process-local manager, so no leader, heartbeat, or durable lease is needed.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::{JoinHandle, JoinSet};
use tracing::{info, warn};

use crate::oauth_manager::{OAuthRefreshOutcome, OAuthTokenManager};

pub struct OAuthRefreshWorkerHandle {
    handle: JoinHandle<()>,
}

impl OAuthRefreshWorkerHandle {
    pub async fn stop(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

pub fn spawn(
    manager: Arc<OAuthTokenManager>,
    store: Arc<tiygate_store::config_store::DbConfigStore>,
) -> OAuthRefreshWorkerHandle {
    let handle = tokio::spawn(async move {
        info!("OAuth credential keepalive worker started");
        loop {
            let enabled = tiygate_store::settings_keys::get_bool(
                store.as_ref(),
                tiygate_store::settings_keys::OAUTH_KEEPALIVE_ENABLED,
                true,
            )
            .await;
            let scan_interval_secs = tiygate_store::settings_keys::get_u64(
                store.as_ref(),
                tiygate_store::settings_keys::OAUTH_KEEPALIVE_SCAN_INTERVAL_SECS,
                60,
            )
            .await
            .max(1);
            let concurrency = tiygate_store::settings_keys::get_usize(
                store.as_ref(),
                tiygate_store::settings_keys::OAUTH_KEEPALIVE_CONCURRENCY,
                4,
            )
            .await
            .clamp(1, 32);

            if enabled {
                match manager
                    .due_keepalive_provider_ids((concurrency * 4) as i64)
                    .await
                {
                    Ok(provider_ids) => {
                        let mut pending = provider_ids.into_iter();
                        let mut joins = JoinSet::new();
                        for _ in 0..concurrency {
                            let Some(provider_id) = pending.next() else {
                                break;
                            };
                            let manager = manager.clone();
                            joins.spawn(async move {
                                let result = manager.try_keepalive_provider(&provider_id).await;
                                (provider_id, result)
                            });
                        }
                        while let Some(joined) = joins.join_next().await {
                            match joined {
                                Ok((provider_id, Ok(OAuthRefreshOutcome::Refreshed))) => {
                                    info!(provider = %provider_id, "OAuth credential keepalive completed");
                                }
                                Ok((_, Ok(_))) => {}
                                Ok((provider_id, Err(error))) => {
                                    warn!(provider = %provider_id, error = %error, "OAuth credential keepalive failed");
                                }
                                Err(error) => {
                                    warn!(error = %error, "OAuth credential keepalive task join failed");
                                }
                            }
                            if let Some(provider_id) = pending.next() {
                                let manager = manager.clone();
                                joins.spawn(async move {
                                    let result = manager.try_keepalive_provider(&provider_id).await;
                                    (provider_id, result)
                                });
                            }
                        }
                    }
                    Err(error) => {
                        warn!(error = %error, "listing OAuth keepalive candidates failed")
                    }
                }
            }

            tokio::time::sleep(Duration::from_secs(scan_interval_secs)).await;
        }
    });
    OAuthRefreshWorkerHandle { handle }
}
