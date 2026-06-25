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

pub mod header_forward;
pub mod ir;
pub mod pipeline;
pub mod protocol;
pub mod provider;
pub mod quota;
pub mod redaction;
pub mod routing;
pub mod telemetry;
mod tests;
pub mod tracing_ctx;

// Re-export key types
pub use header_forward::HeaderForwardPolicy;
pub use ir::{
    Annotation, AnnotationKind, Content, FinishReason, GenerationParams, IrRequest, IrResponse,
    Message, RawEnvelope, ResponseFormat, Role, StreamPart, ThinkingConfig, ThinkingDisplay,
    ThinkingEffort, Tool, TruncationReason, UpstreamStreamError, Usage, UsageAccumulator,
};
pub use pipeline::{
    ExecutionHook, HookDecision, ObserveHook, PipelineContext, PipelineStage, PreRequestHook,
    RouteHook, SettlementRecorder, StreamAction, StreamHook, StreamInterest,
};
pub use protocol::{
    CodecRegistration, EndpointCapabilities, EndpointCodec, Error, PassThroughPolicy,
    ProtocolEndpoint, ProtocolSuite, StreamCaps, StreamDecoder, StreamEncoder, StreamPartStream,
};
pub use provider::oauth::{OAuthTargetConfig, TokenRequestStyle};
pub use provider::{
    AuthApplier, AuthMode, Executor, Provider, ProviderMetadata, ProviderRegistration,
};
pub use routing::{
    classify_error, CooldownStrategy, DefaultFallbackPolicy, ErrorClass, ErrorClassification,
    FallbackDecision, FallbackPolicy, HealthRegistry, LatencyStrategy, PriorityStrategy,
    RetryPolicy, RouteEntry, RoutingStrategyName, RoutingTable, RoutingTarget, RoutingTargetHealth,
    Strategy, WeightedStrategy,
};
pub use telemetry::{
    ErrorTier, EventSink, ExchangeCapture, MicroUsd, PipelineEvent, PriceProvider,
    RequestErrorClass, RequestEvent, RequestStatus, TelemetryBus, TokenKind,
};
