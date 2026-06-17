//! Hook-based pipeline for AI Gateway request processing.
//!
//! The pipeline uses a hook system with compile-time
//! isolation between different protocol types (LLM, MCP, ACP).
//! Each stage has its own trait; hooks register at specific stages.

use std::sync::Arc;
use std::time::Instant;

use crate::ir::{IrRequest, IrResponse, RawEnvelope, StreamPart};
use crate::routing::RoutingTarget;

/// The context carried through the pipeline for a single request.
#[derive(Debug, Clone)]
pub struct PipelineContext {
    /// Unique request identifier (UUID v7).
    pub request_id: String,
    /// When the request entered the pipeline.
    pub start_time: Instant,
    /// The canonical IR request.
    pub ir_request: IrRequest,
    /// The raw envelope (for audit/debug).
    pub raw_envelope: Option<Arc<RawEnvelope>>,
    /// Number of bytes emitted to the client so far (for stream idempotency).
    pub bytes_emitted: u64,
    /// The current hop number in the routing chain (0-indexed).
    pub hop: usize,
    /// Extension data that hooks can read/write.
    pub extensions: std::collections::HashMap<String, serde_json::Value>,
}

impl PipelineContext {
    /// Create a new pipeline context for a request.
    pub fn new(
        request_id: String,
        ir_request: IrRequest,
        raw_envelope: Option<RawEnvelope>,
    ) -> Self {
        Self {
            request_id,
            start_time: Instant::now(),
            ir_request,
            raw_envelope: raw_envelope.map(Arc::new),
            bytes_emitted: 0,
            hop: 0,
            extensions: Default::default(),
        }
    }
}

/// The pipeline stage identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PipelineStage {
    /// Before routing — auth, rate limiting, guardrails.
    PreRequest,
    /// Route resolution — modify routing chain.
    Route,
    /// Execution — call the upstream provider.
    Execute,
    /// Stream processing — intercept/modify stream parts.
    Stream,
    /// Settlement — metering, billing.
    Settle,
    /// Observation — logging, metrics (never blocks).
    Observe,
}

/// Decision from a pre-request hook.
#[derive(Debug, Clone)]
pub enum HookDecision {
    /// Allow the request to proceed.
    Allow,
    /// Deny the request with an error message.
    Deny {
        message: String,
        code: Option<String>,
        http_status: u16,
    },
}

/// Action taken on a stream part.
#[derive(Debug, Clone)]
pub enum StreamAction {
    /// Pass the part through unchanged.
    Pass,
    /// Replace the part with a different one.
    Replace(StreamPart),
    /// Drop this part (don't send to client).
    Drop,
    /// Abort the entire stream.
    Abort { message: String },
}

/// Which stream events a hook is interested in.
#[derive(Debug, Clone, Default)]
pub struct StreamInterest {
    pub text_delta: bool,
    pub reasoning_delta: bool,
    pub tool_call_delta: bool,
    pub usage: bool,
    pub finish: bool,
    pub error: bool,
}

/// Stage 1: Pre-request hooks (auth, rate limiting, guardrails).
///
/// The first Deny decision short-circuits the pipeline.
#[async_trait::async_trait]
pub trait PreRequestHook: Send + Sync {
    /// Check the request before routing. Return `Allow` to proceed or `Deny` to reject.
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision, crate::Error>;
}

/// Stage 2: Route hooks — modify the routing chain.
#[async_trait::async_trait]
pub trait RouteHook: Send + Sync {
    /// Modify the routing chain before execution.
    /// Hooks can insert, remove, or reorder targets.
    async fn resolve(
        &self,
        chain: &mut Vec<RoutingTarget>,
        ctx: &mut PipelineContext,
    ) -> Result<(), crate::Error>;
}

/// Stage 3: Execution hooks — observe execution and control fallback.
#[async_trait::async_trait]
pub trait ExecutionHook: Send + Sync {
    /// Called on successful execution against a target.
    async fn on_success(
        &self,
        target: &RoutingTarget,
        result: &IrResponse,
        ctx: &PipelineContext,
    ) -> Result<(), crate::Error>;

    /// Called on execution failure. Returns a fallback decision.
    async fn on_failure(
        &self,
        target: &RoutingTarget,
        error: &crate::Error,
        ctx: &PipelineContext,
    ) -> Result<crate::routing::FallbackDecision, crate::Error>;
}

/// Stage 4: Stream hooks — intercept and modify stream parts.
#[async_trait::async_trait]
pub trait StreamHook: Send + Sync {
    /// Which stream events this hook is interested in.
    fn interest(&self) -> StreamInterest {
        StreamInterest::default()
    }

    /// Process a stream part. Return an action.
    async fn on_part(
        &self,
        part: &StreamPart,
        ctx: &PipelineContext,
    ) -> Result<StreamAction, crate::Error>;

    /// Called when the stream ends.
    async fn on_stream_end(&self, ctx: &PipelineContext) -> Result<(), crate::Error>;
}

/// Stage 5: Settlement — metering, billing, usage recording.
#[async_trait::async_trait]
pub trait SettlementRecorder: Send + Sync {
    /// Record the settlement for a completed request.
    async fn record(
        &self,
        ctx: &PipelineContext,
        response: Option<&IrResponse>,
        usage: Option<&crate::ir::Usage>,
    ) -> Result<(), crate::Error>;
}

/// Cross-cutting: Observation hooks — read-only, errors are swallowed.
#[async_trait::async_trait]
pub trait ObserveHook: Send + Sync {
    /// Called after a pipeline stage completes.
    async fn after_stage(
        &self,
        stage: PipelineStage,
        ctx: &PipelineContext,
    ) -> Result<(), crate::Error>;

    /// Called when a hop (execution attempt) starts.
    async fn on_hop_start(
        &self,
        target: &RoutingTarget,
        ctx: &PipelineContext,
    ) -> Result<(), crate::Error>;

    /// Called when a hop ends.
    async fn on_hop_end(
        &self,
        target: &RoutingTarget,
        result: std::result::Result<&IrResponse, &crate::Error>,
        ctx: &PipelineContext,
    ) -> Result<(), crate::Error>;

    /// Called for each stream part.
    async fn on_stream_part(
        &self,
        part: &StreamPart,
        ctx: &PipelineContext,
    ) -> Result<(), crate::Error>;

    /// Called when the request completes (success or failure).
    async fn on_request_end(
        &self,
        outcome: std::result::Result<&IrResponse, &crate::Error>,
        ctx: &PipelineContext,
    ) -> Result<(), crate::Error>;
}
