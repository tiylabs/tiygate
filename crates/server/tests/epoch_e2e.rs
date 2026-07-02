//! End-to-end test that the data-plane epoch polling actually
//! refreshes the routing table on an admin write.
//!
//! This test does *not* stand up the full axum server; it exercises
//! the same `DbConfigStore` + `spawn_epoch_poll` path that
//! `app::App` wires up at boot.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use tiygate_store::config_store::DbConfigStore;
use tiygate_store::db;
use tiygate_store::models::{AuthMode, RouteTarget};
use tiygate_store::settings_keys;

#[tokio::test]
async fn data_plane_sees_admin_writes_via_epoch_poll() {
    // Boot an in-memory DB + a control-plane-equivalent config
    // store. The control plane in `app::App` uses the same
    // construction.
    let pool = Arc::new(db::open_pool("sqlite::memory:").await.expect("pool"));
    db::run_migrations(&pool).await.expect("migrate");
    let store = Arc::new(DbConfigStore::new((*pool).clone(), None));
    store.refresh().await.expect("initial refresh");

    // The data plane holds a `ConfigStore` clone it received at
    // boot. Epoch polling will eventually swap that for a fresh
    // snapshot.
    let initial = store.config_store();
    assert!(initial.routing_table.routes.is_empty());

    // Use a fast epoch poll interval (100ms) so the test does not
    // sleep for 2s.
    store
        .set_setting(settings_keys::EPOCH_POLL_INTERVAL_SECS, "1")
        .await
        .expect("set interval");

    let handle = tiygate_store::retention::spawn_epoch_poll(store.clone());

    // Run the retention task too so the test exercises both
    // background loops simultaneously (it would otherwise hang
    // forever in a clean test process). Disable actual cleanup.
    store
        .set_setting(settings_keys::RETENTION_INTERVAL_SECS, "3600")
        .await
        .expect("set retention interval");
    store
        .set_setting(settings_keys::RETENTION_LOG_RETENTION_DAYS, "0")
        .await
        .expect("set retention days");
    let _retention = tiygate_store::retention::spawn(pool.clone(), store.clone());

    // Perform an admin write — create a provider + a route.
    store
        .upsert_provider(
            "openai",
            "OpenAI",
            "openai",
            "https://api.openai.com/v1",
            "",
            Some("sk-test"),
            AuthMode::ApiKey,
            None,
            serde_json::json!({}),
            true,
        )
        .await
        .expect("upsert provider");
    store
        .upsert_route(
            "route-1",
            "gpt-4o",
            &[RouteTarget {
                provider_id: "openai".to_string(),
                model_id: "gpt-4o".to_string(),
                weight: 1.0,
                enabled: true,
                account_label: None,
                api_key_override: None,
                api_base_override: None,
            }],
            None,
            true,
        )
        .await
        .expect("upsert route");

    // Give the epoch poller at most 1s to pick up the change.
    let mut observed = false;
    for _ in 0..20 {
        let current = store.config_store();
        if current.routing_table.routes.contains_key("gpt-4o") {
            observed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        observed,
        "epoch poll should have refreshed the data-plane snapshot"
    );

    // Stop the background task before the test exits so it does
    // not outlive the test process.
    handle.stop().await;
}
