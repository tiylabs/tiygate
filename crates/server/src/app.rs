//! Gateway application assembly and lifecycle.

use std::sync::Arc;

use axum::Router;
use tiygate_core::{HealthRegistry, TelemetryBus};
use tiygate_store::config::ConfigStore;

use crate::config::ServerConfig;
use crate::telemetry::ChannelTelemetryBus;

/// The TiyGate application — holds all components.
pub struct App {
    pub config: ConfigStore,
    pub health: Arc<HealthRegistry>,
    pub server_config: ServerConfig,
    /// Async telemetry bus for pipeline / request events.
    /// Held so the spawned drain task is not orphaned for the lifetime of
    /// the process. Cloned and passed to ingress.
    pub telemetry: ChannelTelemetryBus,
}

impl App {
    pub async fn new() -> anyhow::Result<Self> {
        let config = ConfigStore::from_env();
        let health = Arc::new(HealthRegistry::with_defaults());
        let server_config = ServerConfig::from_env();

        // Default to a stdout sink for Phase 1 / local development. The
        // sink can be swapped to a partitioned OLTP sink in Phase 4
        // without changing the bus wiring.
        let sink: Arc<dyn tiygate_core::EventSink> =
            Arc::new(crate::telemetry::StdoutTelemetrySink::new());
        let telemetry =
            ChannelTelemetryBus::spawn(sink, crate::telemetry::DEFAULT_TELEMETRY_CHANNEL_CAPACITY);

        tracing::info!(
            "TiyGate initialized with {} routes",
            config.routing_table.routes.len()
        );

        Ok(Self {
            config,
            health,
            server_config,
            telemetry,
        })
    }

    pub fn router(&self) -> Router {
        crate::ingress::router_with_telemetry(
            self.config.clone(),
            self.health.clone(),
            &self.server_config,
            Arc::new(self.telemetry.clone()),
        )
    }
}
