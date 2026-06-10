//! TiyGate Core — Canonical IR, traits, and pipeline definitions.
//!
//! This crate defines the foundational abstractions for the AI Gateway:
//! - Canonical IR types (`IrRequest`, `IrResponse`, `StreamPart`, `RawEnvelope`)
//! - Protocol codec traits with three-segment identities
//! - Provider/Executor/AuthApplier traits
//! - Hook-based pipeline stages
//! - Routing and health management
//! - Telemetry event types
//!
//! # Design Principle
//! `core` has zero dependencies on concrete providers, protocols, or databases.
//! All implementations register against the traits defined here.

pub mod ir;
pub mod pipeline;
pub mod protocol;
pub mod provider;
pub mod routing;
pub mod telemetry;
mod tests;

// Re-export key types
pub use ir::{
    Content, FinishReason, GenerationParams, IrRequest, IrResponse, Message, RawEnvelope,
    ResponseFormat, Role, StreamPart, Tool, Usage, UsageAccumulator,
};
pub use pipeline::{
    ExecutionHook, HookDecision, ObserveHook, PipelineContext, PipelineStage, PreRequestHook,
    RouteHook, SettlementRecorder, StreamAction, StreamHook, StreamInterest,
};
pub use protocol::{
    CodecRegistration, EndpointCapabilities, EndpointCodec, Error, PassThroughPolicy,
    ProtocolEndpoint, ProtocolSuite, StreamCaps, StreamDecoder, StreamEncoder, StreamPartStream,
};
pub use provider::{
    AuthApplier, AuthMode, Executor, Provider, ProviderMetadata, ProviderRegistration,
};
pub use routing::{
    classify_error, DefaultFallbackPolicy, ErrorClass, ErrorClassification, FallbackDecision,
    FallbackPolicy, HealthRegistry, LatencyStrategy, RetryPolicy, RoutingTable, RoutingTarget,
    RoutingTargetHealth, Strategy,
};
pub use telemetry::{EventSink, PipelineEvent, RequestEvent, TelemetryBus};
