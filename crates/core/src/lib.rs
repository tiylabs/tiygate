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

pub mod capability;
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
pub use capability::{
    canonicalize_api_base, capability_shape_hash, capability_shape_hash_from_ids,
    capability_shape_hash_from_requirements, compatibility_report, credential_scope_fingerprint,
    derive_ir_requirements, is_flat_required_shape, merge_exchange_requirements,
    resolve_capabilities, resolve_capabilities_with_matchers, target_instance_id, target_key,
    validate_capability_descriptors, validate_capability_observation, BaselineSupport,
    CanonicalTargetIdentity, CapabilityDescriptor, CapabilityId, CapabilityMatcher,
    CapabilityObservation, CapabilityRequirement, CapabilityRoutingMode, CapabilityScope,
    CapabilityState, CapabilityValue, CapabilityValueKind, CompatibilityReport, DiscoveryMethod,
    EvidenceSource, ExchangeRequirements, ImplementationStatus, PlannedTarget, PlannedTransform,
    ProtocolRequirementProvider, ProtocolTransformProvider, RequirementExpr, RequirementStrength,
    ResolvedCapability, ResolvedTargetCapabilities, RoutingEligibility, TargetIdentityError,
    TargetInstanceId, TargetKey, TransformId, WireProfileId, CAPABILITY_SHAPE_HASH_VERSION,
};
pub use header_forward::HeaderForwardPolicy;
pub use ir::{
    Annotation, AnnotationKind, Content, FinishReason, GenerationParams, IrRequest, IrResponse,
    Message, PromptCacheBreakpoint, PromptCacheBreakpointMode, RawEnvelope, ResponseFormat, Role,
    StreamPart, ThinkingConfig, ThinkingDisplay, ThinkingEffort, Tool, ToolCaller,
    TruncationReason, UpstreamStreamError, Usage, UsageAccumulator, Verbosity,
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
    classify_error, classify_structured, classify_upstream_error, CooldownStrategy,
    DefaultFallbackPolicy, ErrorClass, ErrorClassification, FallbackDecision, FallbackPolicy,
    HealthRegistry, LatencyStrategy, PriorityStrategy, RetryPolicy, RouteEntry,
    RoutingStrategyName, RoutingTable, RoutingTarget, RoutingTargetHealth, Strategy,
    WeightedStrategy,
};
pub use telemetry::{
    ErrorTier, EventSink, ExchangeCapture, MicroUsd, PipelineEvent, PriceProvider,
    RequestErrorClass, RequestEvent, RequestStatus, TelemetryBus, TokenKind,
};
