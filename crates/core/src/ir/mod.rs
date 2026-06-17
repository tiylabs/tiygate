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
    Reasoning {
        text: String,
        /// Provider-issued signature for the reasoning block (e.g. Anthropic
        /// extended-thinking `signature`). Required to replay the thinking
        /// block to the same provider on a later turn; absent for reasoning
        /// that originated from other protocols (OpenAI/Gemini), which must
        /// not be echoed back to Anthropic.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        /// Provider-issued item identifier for the reasoning block (e.g.
        /// OpenAI Responses `rs_...` reasoning item id). Required to replay
        /// the reasoning item to the same provider on a later turn (the
        /// Responses API rejects orphaned/idless reasoning items); absent for
        /// reasoning that originated from other protocols, which must not be
        /// echoed back to Responses with a fabricated id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
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

impl MediaSource {
    /// Parse a URL string, recognising `data:` URIs as inline media.
    ///
    /// For `data:[<mediatype>][;base64],<payload>` the MIME type is extracted
    /// from the header and the payload is stored as [`MediaSource::Inline`].
    /// All other URLs (including `https://…`) are stored as [`MediaSource::Url`].
    ///
    /// Returns `(source, resolved_mime_type)`.
    pub fn from_data_url(url: &str, fallback_mime: &str) -> (Self, String) {
        if let Some(rest) = url.strip_prefix("data:") {
            if let Some((header, data)) = rest.split_once(',') {
                let mime = if let Some((mime_part, _encoding)) = header.split_once(';') {
                    if mime_part.is_empty() {
                        fallback_mime
                    } else {
                        mime_part
                    }
                } else if header.is_empty() {
                    fallback_mime
                } else {
                    header
                };
                return (
                    MediaSource::Inline {
                        data: data.to_string(),
                    },
                    mime.to_string(),
                );
            }
        }
        (
            MediaSource::Url {
                url: url.to_string(),
            },
            fallback_mime.to_string(),
        )
    }
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
///
/// Carries both the high-level `stop_reason` and the richer Anthropic
/// `stop_details` object semantics (`type`/`category`/`explanation`) so
/// that refusal metadata survives a round-trip through the gateway.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StopDetails {
    /// The top-level stop reason (e.g. "end_turn", "tool_use", "refusal").
    pub stop_reason: String,
    /// The stop sequence that triggered the stop, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequence: Option<String>,
    /// The `stop_details.type` discriminator, when the upstream emits a
    /// structured `stop_details` object (e.g. "refusal").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The refusal category, when present in `stop_details`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// A human-readable explanation accompanying a refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
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

/// Why a streaming response was terminated before the upstream
/// naturally completed. Recorded on `UsageAccumulator::truncated`
/// so that disconnect-billing can distinguish "client cancelled"
/// from "gateway hit a timeout" without losing the partial usage
/// that was already accumulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationReason {
    /// The idle timer fired (no chunk received within the configured
    /// idle window). The accumulator's partial state is still billable.
    Idle,
    /// The total wall-clock timer fired (stream exceeded the configured
    /// total budget). Partial state is still billable.
    Total,
    /// The upstream connection returned an error mid-stream.
    /// Partial state is still billable for the bytes already received.
    UpstreamError,
}

impl TruncationReason {
    /// Stable lowercase string form for logging and persistence.
    pub fn as_str(&self) -> &'static str {
        match self {
            TruncationReason::Idle => "idle",
            TruncationReason::Total => "total",
            TruncationReason::UpstreamError => "upstream_error",
        }
    }
}

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
    /// If the stream was terminated by the gateway (idle / total /
    /// upstream error) instead of by a natural end-of-stream, this
    /// records the reason. `None` until either `mark_completed()` or
    /// `mark_truncated()` is called. Mutually exclusive with
    /// `completed == true` in the sense that the gateway never sets
    /// both flags — the last call wins.
    pub truncated: Option<TruncationReason>,
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
        self.truncated = None;
    }

    /// Mark the stream as truncated by a gateway-side event. The
    /// `completed` flag is forced to `false` so that downstream
    /// observers can distinguish "ended early" from "ended cleanly".
    pub fn mark_truncated(&mut self, reason: TruncationReason) {
        self.completed = false;
        self.truncated = Some(reason);
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

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn from_data_url_standard_base64_png() {
        let (src, mime) =
            MediaSource::from_data_url("data:image/png;base64,iVBORw0KGgo=", "image/*");
        assert!(matches!(src, MediaSource::Inline { data } if data == "iVBORw0KGgo="));
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn from_data_url_standard_base64_jpeg() {
        let (src, mime) = MediaSource::from_data_url("data:image/jpeg;base64,/9j/4AAQ", "image/*");
        assert!(matches!(src, MediaSource::Inline { data } if data == "/9j/4AAQ"));
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn from_data_url_missing_mime_with_base64() {
        // data:;base64,abc → fallback mime
        let (src, mime) = MediaSource::from_data_url("data:;base64,abc", "image/*");
        assert!(matches!(src, MediaSource::Inline { data } if data == "abc"));
        assert_eq!(mime, "image/*");
    }

    #[test]
    fn from_data_url_plain_text_no_base64() {
        // data:text/plain,hello → Inline with mime text/plain
        let (src, mime) = MediaSource::from_data_url("data:text/plain,hello", "image/*");
        assert!(matches!(src, MediaSource::Inline { data } if data == "hello"));
        assert_eq!(mime, "text/plain");
    }

    #[test]
    fn from_data_url_empty_header_no_encoding() {
        // data:,content → fallback mime
        let (src, mime) = MediaSource::from_data_url("data:,content", "image/*");
        assert!(matches!(src, MediaSource::Inline { data } if data == "content"));
        assert_eq!(mime, "image/*");
    }

    #[test]
    fn from_data_url_https_url_unchanged() {
        let (src, mime) = MediaSource::from_data_url("https://example.com/cat.png", "image/*");
        assert!(matches!(src, MediaSource::Url { url } if url == "https://example.com/cat.png"));
        assert_eq!(mime, "image/*");
    }

    #[test]
    fn from_data_url_empty_string() {
        let (src, mime) = MediaSource::from_data_url("", "image/*");
        assert!(matches!(src, MediaSource::Url { url } if url.is_empty()));
        assert_eq!(mime, "image/*");
    }

    #[test]
    fn from_data_url_malformed_no_comma() {
        // data:image/png;base64 (no comma) → treated as plain URL
        let (src, mime) = MediaSource::from_data_url("data:image/png;base64", "image/*");
        assert!(matches!(src, MediaSource::Url { .. }));
        assert_eq!(mime, "image/*");
    }

    #[test]
    fn from_data_url_data_with_commas_in_payload() {
        // Only the first comma splits header from payload
        let (src, mime) = MediaSource::from_data_url("data:image/svg+xml,<svg>,</svg>", "image/*");
        assert!(matches!(src, MediaSource::Inline { data } if data == "<svg>,</svg>"));
        assert_eq!(mime, "image/svg+xml");
    }
}
