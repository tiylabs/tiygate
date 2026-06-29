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
    /// Request-level metadata (e.g. Anthropic `metadata.user_id`, OpenAI
    /// `metadata` KV pairs, Gemini `labels`). Cross-protocol mappings may
    /// drop keys that the target protocol cannot express.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
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
    ReasoningDelta {
        text: String,
        /// Provider-issued reasoning item id (e.g. OpenAI Responses `rs_...`),
        /// surfaced during streaming so it survives to the stream boundary and
        /// can be replayed on later turns. `None` for protocols that do not
        /// carry a streaming reasoning id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// Encrypted reasoning content streamed alongside the delta (e.g.
        /// OpenAI Responses `reasoning.encrypted_content` carried on the
        /// reasoning output item). Must be replayed verbatim on subsequent
        /// turns; `None` for plain-text streaming reasoning.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
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
        /// Protocol-specific metadata collected during streaming that needs
        /// to survive the stream boundary (e.g. Gemini thought signatures).
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        extensions: HashMap<String, serde_json::Value>,
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
    Text {
        text: String,
        /// Citation / file-citation annotations attached to this text
        /// (e.g. OpenAI `annotations[]`, Gemini `groundingMetadata`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Vec<Annotation>>,
    },
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
        /// Encrypted reasoning content for cross-turn replay (e.g. OpenAI
        /// Responses `reasoning.encrypted_content`, Anthropic
        /// `redacted_thinking.data`). Carries opaque provider-specific
        /// encrypted data that must be replayed verbatim on subsequent turns.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
    },
    /// A tool call issued by the model.
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
        /// Responses-specific `call_id` that is distinct from the item `id`.
        /// When present, `id` is the item reference (e.g. `fc_xxx`) and
        /// `call_id` is the function-call identifier (e.g. `call_xxx`).
        /// When absent, `id` serves both roles (Chat Completions, Messages,
        /// Gemini all use a single identifier).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_id: Option<String>,
    },
    /// A tool result provided by the user/system.
    ToolResult {
        tool_call_id: String,
        name: String,
        content: String,
        /// Responses-specific item reference id for `function_call_output`.
        /// Required by the Responses HTTP API so each output item has a
        /// unique id that can be matched via `item_reference`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// A refusal from the model (OpenAI `message.refusal`, Responses
    /// `refusal` output item, Anthropic `stop_reason:"refusal"`).
    Refusal {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        category: Option<String>,
    },
    /// A multimodal media part (image, audio, document).
    Media {
        source: MediaSource,
        mime_type: String,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        metadata: HashMap<String, serde_json::Value>,
    },
}

/// Well-known metadata key for the OpenAI `image_url.detail` field (e.g.
/// `"high"`, `"low"`, `"auto"`). Stored in [`Content::Media::metadata`] so the
/// field survives protocol translation round-trips.
pub const IMAGE_DETAIL_KEY: &str = "detail";

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
    /// Reasoning / thinking configuration (OpenAI `reasoning_effort`,
    /// Anthropic `thinking`, Gemini `thinkingConfig`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
}

/// Reasoning / thinking effort level.
///
/// Six canonical levels that map across all protocols:
/// - **OpenAI Chat/Responses**: `reasoning_effort` / `reasoning.effort`
///   (minimal/low/medium/high/xhigh; Max clamps to xhigh)
/// - **Anthropic**: `thinking.output_config.effort` (low/medium/high/xhigh/max;
///   Minimal clamps to low) or `thinking.budget_tokens` (numeric)
/// - **Gemini**: `thinkingConfig.thinkingLevel` (minimal/low/medium/high;
///   XHigh/Max clamp to high) or `thinkingConfig.thinkingBudget` (numeric;
///   Gemini docs require the two fields not to be sent together)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingEffort {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

/// How reasoning is displayed to the client (Anthropic-specific).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingDisplay {
    Summarized,
    Omitted,
}

/// Reasoning / thinking configuration that maps across protocols.
///
/// - **OpenAI Chat**: `reasoning_effort` ↔ `effort`
/// - **OpenAI Responses**: `reasoning.effort` ↔ `effort`
/// - **Anthropic**: `thinking: {type, budget_tokens, display}` ↔
///   `budget_tokens` / `display`
/// - **Gemini**: `thinkingConfig: {includeThoughts, thinkingLevel}` or
///   `thinkingConfig: {includeThoughts, thinkingBudget}` ↔
///   `include_thoughts` / `effort` / `budget_tokens`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThinkingConfig {
    /// Effort level (maps to `reasoning_effort` / `reasoning.effort`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<ThinkingEffort>,
    /// Maximum thinking token budget (Anthropic `budget_tokens`,
    /// Gemini `thinkingBudget`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
    /// How reasoning is displayed (Anthropic `display`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<ThinkingDisplay>,
    /// Whether to include thought summaries in the response (Gemini
    /// `includeThoughts`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_thoughts: Option<bool>,
    /// Reasoning summary mode (OpenAI Responses `reasoning.summary`,
    /// e.g. "auto"). Controls whether the API returns a human-readable
    /// summary of the reasoning alongside encrypted content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl ThinkingConfig {
    /// Map an effort level to a canonical token budget.
    ///
    /// The values are chosen to span the union of all protocols' budget
    /// ranges (Anthropic 1024–64000+, Gemini 0–24576). Each protocol
    /// clamps as needed at encode time.
    pub fn effort_to_budget(effort: ThinkingEffort) -> u32 {
        match effort {
            ThinkingEffort::Minimal => 1024,
            ThinkingEffort::Low => 4096,
            ThinkingEffort::Medium => 10_000,
            ThinkingEffort::High => 32_000,
            ThinkingEffort::XHigh => 48_000,
            ThinkingEffort::Max => 64_000,
        }
    }

    /// Map a token budget back to the nearest effort level.
    ///
    /// The inverse of [`effort_to_budget`](Self::effort_to_budget), using
    /// range boundaries so that `budget_to_effort(effort_to_budget(e)) == e`.
    pub fn budget_to_effort(budget: u32) -> ThinkingEffort {
        match budget {
            0..=2047 => ThinkingEffort::Minimal,
            2048..=6143 => ThinkingEffort::Low,
            6144..=16383 => ThinkingEffort::Medium,
            16384..=39999 => ThinkingEffort::High,
            40000..=55999 => ThinkingEffort::XHigh,
            _ => ThinkingEffort::Max,
        }
    }
}

/// Citation or file-citation annotation attached to text content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Annotation {
    /// The kind of annotation.
    pub kind: AnnotationKind,
    /// Start index of the annotated span (OpenAI annotations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_index: Option<u32>,
    /// End index of the annotated span (OpenAI annotations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_index: Option<u32>,
    /// Title of the cited resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// URL of the cited resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// The kind of annotation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationKind {
    /// URL citation (OpenAI `url_citation`).
    UrlCitation,
    /// File citation (OpenAI `file_citation`).
    FileCitation,
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
    /// Raw request body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Original body size in bytes.
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
    /// The downstream client disconnected or cancelled the response
    /// before the upstream stream reached a natural end. Partial bytes
    /// already received are still captured for diagnostics.
    ClientDisconnect,
}

impl TruncationReason {
    /// Stable lowercase string form for logging and persistence.
    pub fn as_str(&self) -> &'static str {
        match self {
            TruncationReason::Idle => "idle",
            TruncationReason::Total => "total",
            TruncationReason::UpstreamError => "upstream_error",
            TruncationReason::ClientDisconnect => "client_disconnect",
        }
    }
}

/// An error frame detected inside an otherwise-successful upstream
/// stream (HTTP 200 with an embedded `{"error": ...}` SSE frame, e.g.
/// `service_unavailable_error`). The gateway cannot retry these because
/// the response headers are already committed to the client, but it
/// must still record the failure for health tracking and telemetry.
#[derive(Debug, Clone, Default)]
pub struct UpstreamStreamError {
    /// Human-readable error message extracted from the error frame.
    pub message: String,
    /// Protocol-specific error code/type (e.g. `service_unavailable_error`,
    /// `overloaded_error`, `429`).
    pub code: Option<String>,
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
    /// If an error frame was detected inside an otherwise-successful
    /// upstream stream (HTTP 200 + embedded error SSE frame), this
    /// holds the error details so downstream observers (capture guard,
    /// telemetry) can record the failure.
    pub upstream_error: Option<UpstreamStreamError>,
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

    /// Record an error frame detected inside the upstream SSE stream
    /// (HTTP 200 with an embedded `{"error": ...}` frame). The first
    /// error wins — subsequent calls are ignored so the original
    /// cause is preserved.
    pub fn set_upstream_error(&mut self, message: &str, code: Option<&str>) {
        if self.upstream_error.is_none() {
            self.upstream_error = Some(UpstreamStreamError {
                message: message.to_string(),
                code: code.map(String::from),
            });
        }
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
    fn thinking_effort_budget_roundtrip() {
        // effort_to_budget → budget_to_effort must be identity for all levels.
        for effort in [
            ThinkingEffort::Minimal,
            ThinkingEffort::Low,
            ThinkingEffort::Medium,
            ThinkingEffort::High,
            ThinkingEffort::XHigh,
            ThinkingEffort::Max,
        ] {
            let budget = ThinkingConfig::effort_to_budget(effort);
            assert_eq!(
                ThinkingConfig::budget_to_effort(budget),
                effort,
                "roundtrip failed for {effort:?} (budget={budget})"
            );
        }
    }

    #[test]
    fn thinking_effort_budget_ordering() {
        // Budget must monotonically increase with effort level.
        let budgets: Vec<u32> = [
            ThinkingEffort::Minimal,
            ThinkingEffort::Low,
            ThinkingEffort::Medium,
            ThinkingEffort::High,
            ThinkingEffort::XHigh,
            ThinkingEffort::Max,
        ]
        .iter()
        .map(|e| ThinkingConfig::effort_to_budget(*e))
        .collect();
        for w in budgets.windows(2) {
            assert!(
                w[0] < w[1],
                "budgets must be strictly increasing: {budgets:?}"
            );
        }
    }

    #[test]
    fn thinking_effort_serde_lowercase() {
        // serde rename_all = "lowercase" must produce the expected wire forms.
        assert_eq!(
            serde_json::to_string(&ThinkingEffort::Minimal).unwrap(),
            "\"minimal\""
        );
        assert_eq!(
            serde_json::to_string(&ThinkingEffort::XHigh).unwrap(),
            "\"xhigh\""
        );
        assert_eq!(
            serde_json::to_string(&ThinkingEffort::Max).unwrap(),
            "\"max\""
        );
        assert_eq!(
            serde_json::from_str::<ThinkingEffort>("\"xhigh\"").unwrap(),
            ThinkingEffort::XHigh
        );
    }

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
