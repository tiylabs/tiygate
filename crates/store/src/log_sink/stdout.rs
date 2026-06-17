//! Stdout / tracing-based event sink. Kept for Phase 4 backwards
//! compatibility — the data plane still emits pipeline events to
//! stdout in dev / test runs.

use async_trait::async_trait;
use tiygate_core::{EventSink, PipelineEvent, RequestEvent};

/// Write events as JSON lines to stdout (via the `tracing` JSON
/// subscriber installed in `main`).
pub struct StdoutSink;

impl Default for StdoutSink {
    fn default() -> Self {
        Self::new()
    }
}

impl StdoutSink {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl EventSink for StdoutSink {
    async fn write_event(&self, event: &PipelineEvent) -> Result<(), tiygate_core::Error> {
        let line = serde_json::to_string(event).map_err(|e| {
            tiygate_core::Error::Telemetry(format!("serialize pipeline event: {e}"))
        })?;
        tracing::info!(target: "tiygate.event", "{}", line);
        Ok(())
    }

    async fn write_request_event(&self, event: &RequestEvent) -> Result<(), tiygate_core::Error> {
        let line = serde_json::to_string(event)
            .map_err(|e| tiygate_core::Error::Telemetry(format!("serialize request event: {e}")))?;
        tracing::info!(target: "tiygate.request", "{}", line);
        Ok(())
    }

    async fn flush(&self) -> Result<(), tiygate_core::Error> {
        Ok(())
    }
}
