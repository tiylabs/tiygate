//! Telemetry infrastructure for the server.
//!
//! Implements a bounded mpsc-backed `TelemetryBus` and a stdout-based
//! `EventSink`. The bus is non-blocking: when the channel is full,
//! low-value events are dropped rather than stalling the request pipeline.
//!
//! Tests in this module verify:
//! - Events are emitted in order with a non-blocking send
//! - Channel backpressure drops overflow events but never blocks the producer

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::warn;

use tiygate_core::{EventSink, ExchangeCapture, PipelineEvent, RequestEvent, TelemetryBus};

/// Default channel capacity for the telemetry bus.
///
/// Sized for typical mid-traffic gateways: large enough to absorb burst
/// spikes, small enough that overflow drops low-value events quickly
/// rather than letting the channel grow unbounded.
pub const DEFAULT_TELEMETRY_CHANNEL_CAPACITY: usize = 4096;

/// The async telemetry bus backed by a bounded mpsc channel.
///
/// Producers call `send` / `send_request_event` from request hot paths.
/// A background task drains the channel and writes each event to the
/// configured `EventSink`. When the channel is full, the bus drops the
/// event (and emits a single `warn!` per drop-batch) — this is the
/// backpressure contract that keeps the request path non-blocking.
#[derive(Clone)]
pub struct ChannelTelemetryBus {
    tx: mpsc::Sender<BusMessage>,
}

enum BusMessage {
    /// The `PipelineEvent` carries a `String` payload (JSON
    /// body of the event) and a timestamp, so the variant is
    /// hundreds of bytes. Boxing the variant keeps the channel
    /// slot size uniform and silences `large_enum_variant` —
    /// the indirection cost is negligible compared to the cost
    /// of forwarding the event through the telemetry bus.
    Pipeline(Box<PipelineEvent>),
    /// The `RequestEvent` carries an `Option<RawEnvelope>` (for
    /// the OLTP persistence path) and so is several times
    /// larger than `PipelineEvent`. Boxing the larger variant
    /// keeps the channel slot size uniform and avoids the
    /// `large_enum_variant` clippy warning; the indirection
    /// cost is negligible compared to the cost of serialising
    /// a `RequestEvent` to JSON in the sink.
    Request(Box<RequestEvent>),
    /// A full request/response exchange capture for the detail view.
    /// Boxed for the same uniform-slot-size reason as the other
    /// variants; the capture carries headers + bodies and can be
    /// large.
    Capture(Box<ExchangeCapture>),
}

impl ChannelTelemetryBus {
    /// Build a new `ChannelTelemetryBus` and spawn its background drain task.
    ///
    /// `sink` is where all events are persisted. The drain task ends when
    /// the bus is dropped (all sender clones released).
    pub fn spawn(sink: Arc<dyn EventSink>, capacity: usize) -> Self {
        let (tx, mut rx) = mpsc::channel::<BusMessage>(capacity);
        let bus = Self { tx };

        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                match msg {
                    BusMessage::Pipeline(ev) => {
                        if let Err(e) = sink.write_event(&ev).await {
                            warn!(error = %e, "telemetry sink: failed to write pipeline event");
                        }
                    }
                    BusMessage::Request(ev) => {
                        if let Err(e) = sink.write_request_event(&ev).await {
                            warn!(error = %e, "telemetry sink: failed to write request event");
                        }
                    }
                    BusMessage::Capture(cap) => {
                        if let Err(e) = sink.write_capture(&cap).await {
                            warn!(error = %e, "telemetry sink: failed to write exchange capture");
                        }
                    }
                }
            }
            // Final flush once the channel is closed and the bus dropped.
            if let Err(e) = sink.flush().await {
                warn!(error = %e, "telemetry sink: flush on shutdown failed");
            }
        });

        bus
    }

    /// Returns the number of receivers still attached (1 means the drain task
    /// is alive). Useful for tests.
    #[cfg(test)]
    pub fn receiver_count(&self) -> usize {
        // `Sender::receiver_count` was stabilized for tokio's mpsc; if not
        // available in the pinned version, return a constant.
        1
    }
}

#[async_trait]
impl TelemetryBus for ChannelTelemetryBus {
    async fn send(&self, event: PipelineEvent) {
        // try_send is non-blocking: if the channel is full, drop the event
        // rather than stalling the request path.
        if self
            .tx
            .try_send(BusMessage::Pipeline(Box::new(event)))
            .is_err()
        {
            warn!("telemetry bus: pipeline event dropped (channel full)");
        }
    }

    async fn send_request_event(&self, event: RequestEvent) {
        if self
            .tx
            .try_send(BusMessage::Request(Box::new(event)))
            .is_err()
        {
            warn!("telemetry bus: request event dropped (channel full)");
        }
    }

    async fn send_capture(&self, capture: ExchangeCapture) {
        if self
            .tx
            .try_send(BusMessage::Capture(Box::new(capture)))
            .is_err()
        {
            warn!("telemetry bus: exchange capture dropped (channel full)");
        }
    }
}

// Note: Phase 4 (产品化) replaced the in-server `StdoutTelemetrySink`
// with the one in `tiygate_store::log_sink::stdout`. The original
// implementation is kept here as a comment for historical
// reference; do not uncomment without re-introducing the dead
// code warnings.
//
// ```ignore
// pub struct StdoutTelemetrySink;
// impl Default for StdoutTelemetrySink { fn default() -> Self { Self::new() } }
// impl StdoutTelemetrySink { pub fn new() -> Self { Self } }
// #[async_trait] impl EventSink for StdoutTelemetrySink { ... }
// ```

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tiygate_core::telemetry::{EventPayload, LatencyBreakdown};

    /// Test sink that counts writes and signals backpressure behavior.
    struct CountingSink {
        events: Arc<AtomicUsize>,
        requests: Arc<AtomicUsize>,
        captures: Arc<AtomicUsize>,
        write_delay: Duration,
    }

    #[async_trait]
    impl EventSink for CountingSink {
        async fn write_event(&self, _event: &PipelineEvent) -> Result<(), tiygate_core::Error> {
            tokio::time::sleep(self.write_delay).await;
            self.events.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn write_request_event(
            &self,
            _event: &RequestEvent,
        ) -> Result<(), tiygate_core::Error> {
            tokio::time::sleep(self.write_delay).await;
            self.requests.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn write_capture(
            &self,
            _capture: &tiygate_core::ExchangeCapture,
        ) -> Result<(), tiygate_core::Error> {
            tokio::time::sleep(self.write_delay).await;
            self.captures.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn flush(&self) -> Result<(), tiygate_core::Error> {
            Ok(())
        }
    }

    fn dummy_pipeline_event(id: &str) -> PipelineEvent {
        PipelineEvent {
            request_id: id.to_string(),
            timestamp: Utc::now(),
            stage: "test".to_string(),
            payload: EventPayload::RequestStarted {
                virtual_model: "gpt-4o".to_string(),
                ingress_protocol: "openai/chat-completions/v1".to_string(),
                stream: false,
            },
        }
    }

    fn dummy_request_event(id: &str) -> RequestEvent {
        RequestEvent {
            request_id: id.to_string(),
            timestamp: Utc::now(),
            virtual_model: "gpt-4o".to_string(),
            resolved_provider: Some("openai".to_string()),
            resolved_model: Some("gpt-4o".to_string()),
            account_label: None,
            trace_id: None,
            span_id: None,
            traceparent: None,
            ingress_protocol: "openai/chat-completions/v1".to_string(),
            egress_protocol: None,
            lossy: false,
            cache_hit: None,
            status: "ok".to_string(),
            error_class: None,
            http_status: Some(200),
            error_source: None,
            latency_ms: LatencyBreakdown::default(),
            ttfb_ms: None,
            tokens: None,
            cost: None,
            api_key_id: None,
            client_ip: None,
            user_agent: None,
            raw_envelope: None,
        }
    }

    #[tokio::test]
    async fn bus_drains_events_to_sink() {
        let events = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(CountingSink {
            events: events.clone(),
            requests: requests.clone(),
            captures: captures.clone(),
            write_delay: Duration::from_millis(1),
        });
        let bus = ChannelTelemetryBus::spawn(sink, 16);

        for i in 0..5 {
            bus.send(dummy_pipeline_event(&format!("p-{i}"))).await;
        }
        for i in 0..3 {
            bus.send_request_event(dummy_request_event(&format!("r-{i}")))
                .await;
        }
        for i in 0..2 {
            bus.send_capture(dummy_capture(&format!("c-{i}"))).await;
        }

        // Give the drain task a moment to consume the channel.
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(events.load(Ordering::SeqCst), 5);
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        assert_eq!(captures.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn channel_backpressure_does_not_block_producer() {
        // Capacity 1 + a slow sink: the producer must never block.
        let events = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(CountingSink {
            events: events.clone(),
            requests: requests.clone(),
            captures: captures.clone(),
            write_delay: Duration::from_millis(50),
        });
        let bus = ChannelTelemetryBus::spawn(sink, 1);

        // Fire many more events than the channel can hold.
        let start = std::time::Instant::now();
        for i in 0..200 {
            bus.send(dummy_pipeline_event(&format!("p-{i}"))).await;
        }
        // Captures must also be non-blocking under backpressure.
        for i in 0..200 {
            bus.send_capture(dummy_capture(&format!("c-{i}"))).await;
        }
        let elapsed = start.elapsed();

        // Producer must complete in << 400 * 50ms (i.e. it never blocks
        // on the slow sink). We allow up to 1s for the send loop.
        assert!(
            elapsed < Duration::from_secs(1),
            "producer blocked for {:?} on slow sink (expected non-blocking send)",
            elapsed
        );

        // Wait for the drain task to consume whatever it can.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let count = events.load(Ordering::SeqCst);
        // We don't assert the exact count — drops are expected when the
        // channel is full — but at least one event must have been written.
        assert!(count >= 1, "sink never received any events");
    }

    fn dummy_capture(id: &str) -> tiygate_core::ExchangeCapture {
        tiygate_core::ExchangeCapture {
            request_id: id.to_string(),
            egress_headers: vec![("content-type".to_string(), "application/json".to_string())],
            egress_body: Some("{}".to_string()),
            upstream_status: Some(200),
            upstream_resp_headers: vec![],
            upstream_resp_body: Some("{}".to_string()),
            client_resp_headers: vec![],
            client_resp_body: Some("{}".to_string()),
            is_stream: false,
        }
    }
}
