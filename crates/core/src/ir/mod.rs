//! Canonical Intermediate Representation (IR) types.
//!
//! The IR is the universal format that all protocol codecs translate to/from.
//! It explicitly models text, reasoning, tool calls/results, and multimodal content.
//! Fields are designed to losslessly carry protocol-specific data through the gateway.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A canonical request from a downstream client, after protocol-specific decoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrRequest {
    /// The model identifier requested by the client (may be virtual).
    pub model: String,
    /// System-level instruction, separated from the message list.
    pub system: Option<String>,
    /// Ordered conversation messages.
    pub messages: Vec<Message>,
    /// Available tool definitions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// Sampling and generation parameters.
    #[serde(default)]
    pub params: GenerationParams,
    /// Response format constraints (e.g. JSON Schema).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    /// Whether the client requested streaming.
    #[serde(default)]
    pub stream: bool,
    /// The protocol used by the ingress request.
    pub ingress_protocol: ProtocolEndpoint,
    /// Extension fields for protocol-specific data.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extensions: HashMap<String, serde_json::Value>,
}

/// A canonical response to send back to the downstream client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrResponse {
    /// Ordered content blocks from the model.
    pub content: Vec<Content>,
    /// Token usage information.
    pub usage: Option<Usage>,
    /// Why the model stopped generating.
    pub finish_reason: Option<FinishReason>,
    /// Upstream provider's response identifier.
    pub response_id: Option<String>,
    /// Anthropic-style stop_details.
    pub stop_details: Option<StopDetails>,
    /// Extension fields for protocol-specific data.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extensions: HashMap<String, serde_json::Value>,
}

/// A single piece of the streaming response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamPart {
    /// An incremental text delta.
    TextDelta { text: String },
    /// An incremental reasoning/thinking delta.
    ReasoningDelta { text: String },
    /// A tool call being built incrementally.
    ToolCallDelta {
        id: String,
        name: Option<String>,
        arguments: String,
    },
    /// Token usage reported during streaming.
    Usage { usage: Usage },
    /// The response has started (carries the response id).
    ResponseStarted { id: String },
    /// The model has finished generating.
    Finish { reason: FinishReason },
    /// The complete response is done.
    ResponseCompleted {
        id: String,
        status: String,
        usage: Option<Usage>,
    },
    /// An error occurred during streaming.
    Error {
        message: String,
        code: Option<String>,
    },
}

/// A message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// The role of the message author.
    pub role: Role,
    /// Ordered content blocks in this message.
    pub content: Vec<Content>,
}

/// The role of a message author.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A typed content block within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// Plain text content.
    Text { text: String },
    /// Reasoning / chain-of-thought content.
    Reasoning { text: String },
    /// A tool call issued by the model.
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
    },
    /// A tool result provided by the user/system.
    ToolResult {
        tool_call_id: String,
        name: String,
        content: String,
    },
    /// A multimodal media part (image, audio, document).
    Media {
        source: MediaSource,
        mime_type: String,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        metadata: HashMap<String, serde_json::Value>,
    },
}

/// How media is carried in the request/response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MediaSource {
    /// Inline base64-encoded data.
    Inline { data: String },
    /// A URL reference.
    Url { url: String },
    /// A provider-specific file identifier.
    FileId { id: String },
}

/// A tool / function definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    /// The name of the function.
    pub name: String,
    /// A human-readable description.
    pub description: Option<String>,
    /// JSON Schema for the function parameters.
    pub parameters: Option<serde_json::Value>,
    /// Whether to require this tool call.
    #[serde(default)]
    pub required: bool,
}

/// Generation parameters (sampling, length control).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GenerationParams {
    /// Maximum tokens to generate.
    pub max_tokens: Option<u32>,
    /// Sampling temperature (0.0–2.0).
    pub temperature: Option<f32>,
    /// Nucleus sampling probability.
    pub top_p: Option<f32>,
    /// K-top sampling.
    pub top_k: Option<u32>,
    /// Stop sequences.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
    /// Penalty for token frequency.
    pub frequency_penalty: Option<f32>,
    /// Penalty for token presence.
    pub presence_penalty: Option<f32>,
    /// Seed for deterministic sampling.
    pub seed: Option<i64>,
}

/// Response format constraints.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Request JSON output with a specific schema.
    JsonSchema {
        name: String,
        schema: serde_json::Value,
        strict: Option<bool>,
    },
    /// Request valid JSON (no schema).
    JsonObject,
    /// Plain text (default).
    Text,
}

/// Token usage information.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt / input tokens.
    pub prompt_tokens: u64,
    /// Completion / output tokens.
    pub completion_tokens: u64,
    /// Reasoning / thinking tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Cache read tokens (prompt caching).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Cache write tokens (prompt caching).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Total tokens.
    pub total_tokens: u64,
}

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Natural stop or stop sequence hit.
    Stop,
    /// Maximum tokens reached.
    Length,
    /// Content filter triggered.
    ContentFilter,
    /// Tool call requested.
    ToolCalls,
    /// Other / unknown reason.
    Other(String),
}

/// Anthropic-style stop details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StopDetails {
    pub stop_reason: String,
    pub stop_sequence: Option<String>,
}

/// A raw snapshot of the original request/response for audit and replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEnvelope {
    /// The HTTP method.
    pub method: String,
    /// The URL path.
    pub path: String,
    /// Request headers (sensitive fields redacted).
    pub headers: HashMap<String, String>,
    /// Raw request body (may be truncated; see `truncated`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Whether the body was truncated.
    #[serde(default)]
    pub truncated: bool,
    /// Original body size in bytes before truncation.
    #[serde(default)]
    pub original_body_size: u64,
    /// When the request was received.
    pub timestamp: DateTime<Utc>,
}

// Re-export ProtocolEndpoint for IR use
use crate::protocol::ProtocolEndpoint;

/// Accumulates usage from streaming responses for billing when the
/// client disconnects mid-stream. Estimates token counts from
/// character counts as a fallback.
#[derive(Debug, Clone, Default)]
pub struct UsageAccumulator {
    /// Characters received so far.
    pub chars_received: usize,
    /// Number of reasoning/tool_call chars (higher token density).
    pub control_chars: usize,
    /// Whether the stream completed normally.
    pub completed: bool,
}

impl UsageAccumulator {
    /// Create a new accumulator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a received chunk of text.
    pub fn record_chunk(&mut self, text: &str) {
        self.chars_received += text.len();
    }

    /// Record a control/tool call delta.
    pub fn record_control(&mut self, text: &str) {
        self.control_chars += text.len();
    }

    /// Mark the stream as completed normally.
    pub fn mark_completed(&mut self) {
        self.completed = true;
    }

    /// Estimate usage from accumulated characters.
    /// Rough heuristic: ~4 chars per token for normal text,
    /// ~2 chars per token for structured/control output.
    pub fn estimate_usage(&self) -> Usage {
        let completion_tokens = (self.chars_received / 4).max(1) + (self.control_chars / 2).max(0);
        Usage {
            completion_tokens: completion_tokens as u64,
            total_tokens: completion_tokens as u64,
            ..Default::default()
        }
    }
}
