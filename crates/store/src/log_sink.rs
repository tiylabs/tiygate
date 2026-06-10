//! Pluggable log sink implementations.
//!
//! Default: stdout (Phase 1), SQLite partitioned table (Phase 4).

use async_trait::async_trait;
use tiygate_core::{EventSink, PipelineEvent, RequestEvent};

/// Stdout log sink for Phase 1 — writes JSON lines to stdout.
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
        let line = serde_json::to_string(event).unwrap_or_default();
        tracing::info!(target: "tiygate.event", "{}", line);
        Ok(())
    }

    async fn write_request_event(&self, event: &RequestEvent) -> Result<(), tiygate_core::Error> {
        let line = serde_json::to_string(event).unwrap_or_default();
        tracing::info!(target: "tiygate.request", "{}", line);
        Ok(())
    }

    async fn flush(&self) -> Result<(), tiygate_core::Error> {
        Ok(())
    }
}
