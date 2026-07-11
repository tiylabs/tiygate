//! Protocol layer — Three-segment identity, codec traits, and capabilities.
//!
//! Protocol identity follows a three-segment model: `{suite}/{name}/{version}`.
//! Codecs are registered via `inventory` for decentralized discovery.
//! The `EndpointCapabilities` struct drives routing decisions and lossy-conversion rejections.

use std::fmt;
use std::pin::Pin;

use futures::Stream;
use http::HeaderMap;
use serde::{Deserialize, Serialize};

use crate::ir::{IrRequest, IrResponse, RawEnvelope, StreamPart};
use crate::routing::ErrorClass;

/// A protocol suite — the broad family of protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolSuite {
    /// OpenAI Chat Completions API family.
    OpenAiCompatible,
    /// OpenAI Responses API.
    OpenAiResponses,
    /// Anthropic Messages API.
    AnthropicMessages,
    /// Google Gemini API.
    GoogleGemini,
}

impl ProtocolSuite {
    /// Lowercase, kebab-case label suitable for diagnostic strings.
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAiCompatible => "openai-compatible",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::GoogleGemini => "google-gemini",
        }
    }

    /// The canonical `(name, version)` pair for this suite's primary endpoint.
    ///
    /// Used when building a default [`ProtocolEndpoint`] for a suite without
    /// hard-coding the OpenAI Chat Completions identity everywhere. Keeping
    /// this aligned with the registered codecs avoids producing mismatched
    /// identities such as `anthropic-messages/chat-completions/v1`.
    pub fn default_endpoint_id(self) -> (&'static str, &'static str) {
        match self {
            Self::OpenAiCompatible => ("chat-completions", "v1"),
            Self::OpenAiResponses => ("responses", "v1"),
            Self::AnthropicMessages => ("messages", "2023-06-01"),
            Self::GoogleGemini => ("generateContent", "v1beta"),
        }
    }

    /// Build the canonical default [`ProtocolEndpoint`] for this suite.
    pub fn default_endpoint(self) -> ProtocolEndpoint {
        let (name, version) = self.default_endpoint_id();
        ProtocolEndpoint::new(self, name, version)
    }

    /// The upstream HTTP path suffix for this suite's primary chat-style
    /// endpoint, appended to the provider's `api_base`.
    ///
    /// This lets the egress layer address the correct upstream route based
    /// on the *target* protocol (the provider's protocol) rather than the
    /// ingress entrypoint. Returns `None` for suites whose path is not a
    /// fixed suffix — Google Gemini encodes the model and method in the URL
    /// (`/v1beta/models/{model}:generateContent`) and must be built
    /// separately by the caller.
    pub fn upstream_path_suffix(self) -> Option<&'static str> {
        match self {
            Self::OpenAiCompatible => Some("/chat/completions"),
            Self::OpenAiResponses => Some("/responses"),
            Self::AnthropicMessages => Some("/messages"),
            Self::GoogleGemini => None,
        }
    }
}

/// Three-segment protocol identity: `{suite}/{name}/{version}`.
///
/// Examples:
/// - `openai_compatible/chat-completions/v1`
/// - `anthropic_messages/messages/2023-06-01`
/// - `google_gemini/generate_content/v1beta`
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProtocolEndpoint {
    /// The protocol suite / family.
    pub suite: ProtocolSuite,
    /// The specific protocol name within the suite.
    pub name: String,
    /// The protocol version.
    pub version: String,
}

impl ProtocolEndpoint {
    /// Create a new protocol endpoint identifier.
    pub fn new(suite: ProtocolSuite, name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            suite,
            name: name.into(),
            version: version.into(),
        }
    }

    /// The canonical string form: `suite/name/version`.
    pub fn canonical(&self) -> String {
        format!("{:?}/{}", self.suite, self.name)
            .to_lowercase()
            .replace('_', "-")
    }

    /// The full identifier with version.
    pub fn full_id(&self) -> String {
        format!("{}/{}", self.canonical(), self.version)
    }
}

impl fmt::Display for ProtocolEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.full_id())
    }
}

/// Streaming capabilities of a protocol endpoint.
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamCaps {
    /// Supports Server-Sent Events streaming.
    pub server_sent_events: bool,
    /// Reports usage during streaming.
    pub usage_in_stream: bool,
    /// Requires a stream flag in the request body.
    pub requires_stream_flag: bool,
}

/// Policy for handling unknown / vendor-specific fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownFieldPolicy {
    /// Forward unknown fields.
    Supported,
    /// Drop unknown fields.
    Drop,
}

/// Rich declaration of endpoint capabilities.
///
/// Used by the routing layer to determine protocol compatibility,
/// pass-through eligibility, and lossy conversion decisions.
#[derive(Debug, Clone)]
pub struct EndpointCapabilities {
    /// Whether this endpoint supports streaming.
    pub streaming: bool,
    /// Whether this endpoint supports tool/function calling.
    pub tools: bool,
    /// Whether this endpoint supports reasoning/thinking output.
    pub reasoning: bool,
    /// Whether this endpoint is for embeddings (not chat).
    pub embeddings: bool,
    /// Whether the upstream requires stream=true in the body.
    pub force_upstream_stream: bool,
    /// Whether to override the model name in the request body.
    pub override_model_in_body: bool,
    /// Ingress HTTP routes: (method, path) pairs.
    pub ingress_routes: &'static [(&'static str, &'static str)],
    /// Whether this endpoint supports multimodal input (images, audio, etc.).
    pub multimodal: bool,
    /// Whether this endpoint supports structured output (JSON mode).
    pub structured_output: bool,
    /// Whether this endpoint supports function calling.
    pub function_calling: bool,
    /// Whether this endpoint supports parallel tool calls.
    pub parallel_tool_calls: bool,
    /// Whether this endpoint supports non-function hosted tool definitions.
    pub hosted_tools: bool,
    /// Whether this endpoint supports Responses Programmatic Tool Calling.
    pub programmatic_tool_calling: bool,
    /// Whether this endpoint supports extended reasoning.
    pub extended_reasoning: bool,
    /// Whether this endpoint supports deterministic seeds.
    pub deterministic_seed: bool,
    /// Whether this endpoint supports `tool_choice="required"` / `{type:"any"}`.
    /// Separate from `parallel_tool_calls` because Anthropic supports
    /// required tool choice but not OpenAI-style concurrent fan-out.
    pub tool_choice_required: bool,
    /// Streaming-specific capabilities.
    pub stream: StreamCaps,
    /// How to handle unknown vendor fields.
    pub unknown_field_policy: UnknownFieldPolicy,
    /// If true, cross-protocol conversions that would lose data are rejected.
    pub lossy_default_reject: bool,
}

impl EndpointCapabilities {
    /// All-false empty capabilities (most restrictive).
    pub const EMPTY: Self = Self {
        streaming: false,
        tools: false,
        reasoning: false,
        embeddings: false,
        force_upstream_stream: false,
        override_model_in_body: false,
        ingress_routes: &[],
        multimodal: false,
        structured_output: false,
        function_calling: false,
        parallel_tool_calls: false,
        hosted_tools: false,
        programmatic_tool_calling: false,
        extended_reasoning: false,
        deterministic_seed: false,
        tool_choice_required: false,
        stream: StreamCaps {
            server_sent_events: false,
            usage_in_stream: false,
            requires_stream_flag: false,
        },
        unknown_field_policy: UnknownFieldPolicy::Drop,
        lossy_default_reject: true,
    };

    /// Standard chat endpoint capabilities.
    pub const CHAT_STANDARD: Self = Self {
        streaming: true,
        tools: true,
        reasoning: true,
        embeddings: false,
        force_upstream_stream: false,
        override_model_in_body: false,
        ingress_routes: &[("POST", "/v1/chat/completions")],
        multimodal: true,
        structured_output: true,
        function_calling: true,
        parallel_tool_calls: true,
        hosted_tools: false,
        programmatic_tool_calling: false,
        extended_reasoning: false,
        deterministic_seed: false,
        tool_choice_required: true,
        stream: StreamCaps {
            server_sent_events: true,
            usage_in_stream: true,
            requires_stream_flag: true,
        },
        unknown_field_policy: UnknownFieldPolicy::Drop,
        lossy_default_reject: true,
    };
}

/// Stream of IR stream parts, boxed for trait objects.
pub type StreamPartStream =
    Pin<Box<dyn Stream<Item = std::result::Result<StreamPart, Error>> + Send>>;

/// Encodes IR stream parts into protocol-specific SSE/JSON bytes.
pub trait StreamEncoder: Send {
    /// Encode a single stream part into protocol-specific bytes.
    fn encode_part(&mut self, part: &StreamPart) -> Result<Vec<u8>, crate::Error>;
    /// Encode an error into the protocol's native error frame.
    ///
    /// `class` drives the protocol-native `error.type` / `error.status`
    /// field; `upstream_code` is optionally emitted as `error.code`
    /// for debugging in protocols that support it (OpenAI).
    fn encode_error(
        &mut self,
        message: &str,
        class: ErrorClass,
        upstream_code: Option<&str>,
    ) -> Vec<u8>;
    /// Encode a stream-completion signal.
    fn encode_done(&mut self) -> Vec<u8>;
}

/// Decodes protocol-specific stream bytes into IR stream parts.
///
/// Must be an explicit state machine — wildcard `_ =>` catch-all branches
/// are forbidden as they silently swallow unknown events.
pub trait StreamDecoder: Send {
    /// Feed a line/chunk of protocol data; return zero or more stream parts.
    fn feed(&mut self, line: &str) -> Result<Vec<StreamPart>, crate::Error>;
    /// Signal end of stream; return any remaining parts.
    fn finish(&mut self) -> Result<Vec<StreamPart>, crate::Error>;
}

/// Pass-through policy for same-protocol, no-mutation requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassThroughPolicy {
    /// Pass through the raw bytes without IR conversion.
    Passthrough,
    /// Must go through IR conversion.
    Convert,
}

/// The core protocol codec trait.
///
/// Each protocol implementation provides a pair of codecs (ingress/egress)
/// that translate between the protocol's native format and the canonical IR.
pub trait EndpointCodec: Send + Sync {
    /// The protocol identity.
    fn id(&self) -> &ProtocolEndpoint;
    /// The endpoint capabilities declaration.
    fn capabilities(&self) -> &EndpointCapabilities;

    // --- Ingress (decode from client) ---

    /// Decode a client request body into canonical IR.
    fn decode_request(
        &self,
        body: serde_json::Value,
        env: &RawEnvelope,
    ) -> Result<IrRequest, crate::Error>;
    /// Encode an IR response for the client.
    fn encode_response(&self, ir: &IrResponse) -> Result<serde_json::Value, crate::Error>;
    /// Get a stream encoder for the client.
    fn stream_encoder(&self) -> Box<dyn StreamEncoder>;

    // --- Egress (encode for upstream) ---

    /// Encode IR request for the upstream provider.
    fn encode_request(
        &self,
        ir: &IrRequest,
    ) -> Result<(serde_json::Value, HeaderMap), crate::Error>;
    /// Decode an upstream response into canonical IR.
    fn decode_response(&self, body: serde_json::Value) -> Result<IrResponse, crate::Error>;
    /// Get a stream decoder for upstream responses.
    fn stream_decoder(&self) -> Box<dyn StreamDecoder>;

    /// Determine whether a request can be passed through without conversion.
    fn pass_through_policy(
        &self,
        _ingress: &ProtocolEndpoint,
        _egress: &ProtocolEndpoint,
    ) -> PassThroughPolicy {
        PassThroughPolicy::Convert
    }
}

/// Error type used throughout the core crate.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("protocol codec error: {0}")]
    Codec(String),
    #[error("pipeline error: {0}")]
    Pipeline(String),
    #[error("routing error: {0}")]
    Routing(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("executor error: {0}")]
    Executor(String),
    #[error("auth error: {0}")]
    Auth(String),
    #[error("telemetry error: {0}")]
    Telemetry(String),
    #[error("lossy conversion rejected: {0}")]
    LossyRejection(String),
    #[error("{0}")]
    Other(String),
}

// Re-export as crate::Error
pub type CodecResult<T> = std::result::Result<T, Error>;

/// Decentralized codec registration via `inventory`.
pub struct CodecRegistration {
    /// Factory function to create a codec instance.
    pub make: fn() -> Box<dyn EndpointCodec>,
}

inventory::collect!(CodecRegistration);

pub mod lossy;
pub mod structured_output;
