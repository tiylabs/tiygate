//! Cross-protocol lossy conversion detection.
//!
//! §3.2 of the design requires the gateway to reject cross-protocol conversions
//! that would silently drop fields. Per the §8 phase 3 acceptance criteria, the
//! runtime check must cover, at minimum:
//!
//! - Tool calling: `tools` without `function_calling` support, `parallel_tool_calls`
//!   when target cannot express it, `tool_choice=required`, `tool_choice` pinned
//!   to a specific function when target cannot express it.
//! - Multimodal: inline audio/video/file-id when target cannot carry the format.
//! - Reasoning / structured output: `response_format` when target lacks structured
//!   output; `extended_reasoning` (Anthropic-style thinking) when target cannot
//!   express it.
//! - Determinism: `seed` is a one-way lossy drop, not a rejection — see
//!   `seed` handling below.
//!
//! The capability matrix in `docs/protocol-capability-matrix.md` is the single
//! source of truth for which dimensions are lossy vs unsupported per protocol
//! pair. This module is the *runtime* expression of that matrix — keeping the
//! two in lock-step is enforced by the test suite under
//! `crates/protocols/tests/cross_protocol.rs`.
//!
//! Per §3.2 the gateway deliberately does **not** ship a per-route `allow_lossy`
//! escape hatch: a lossy combination is rejected outright, full stop.

use crate::ir::{Content, IrRequest, MediaSource, ResponseFormat};
use crate::protocol::{EndpointCapabilities, Error, ProtocolEndpoint};

/// A dimension-level lossy conversion check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossyDimension {
    /// Request has tools but the egress protocol cannot express tool/function calls.
    ToolCalling,
    /// Request has `parallel_tool_calls` semantics but the egress protocol cannot
    /// express parallel tool calls.
    ParallelToolCalls,
    /// Request has `tool_choice=required` semantics but the egress protocol cannot
    /// express that.
    ToolChoiceRequired,
    /// Request pins `tool_choice` to a specific function name but the egress
    /// protocol can only express it as `auto`/`any`/`required`.
    ToolChoiceSpecific,
    /// Request contains a media part whose `MediaSource` kind is not expressible
    /// on the egress protocol (e.g. URL → Anthropic, file_id → non-Responses).
    MediaSourceUnsupported,
    /// Request has `response_format` constraints but the egress protocol does
    /// not support structured output.
    StructuredOutput,
    /// Request has `extended_reasoning` (Anthropic-style thinking blocks) but
    /// the egress protocol cannot carry reasoning parts.
    ExtendedReasoning,
}

impl LossyDimension {
    /// Human-readable label for diagnostic output.
    pub fn label(&self) -> &'static str {
        match self {
            Self::ToolCalling => "tool_calling",
            Self::ParallelToolCalls => "parallel_tool_calls",
            Self::ToolChoiceRequired => "tool_choice=required",
            Self::ToolChoiceSpecific => "tool_choice=specific_function",
            Self::MediaSourceUnsupported => "media_source",
            Self::StructuredOutput => "response_format (structured output)",
            Self::ExtendedReasoning => "extended_reasoning",
        }
    }
}

/// Inspect an IR request and the egress protocol's capabilities, returning the
/// first lossy dimension that would be silently dropped on conversion. Returns
/// `Ok(())` when the conversion is lossless (or only loses dimensions the
/// caller is willing to drop, e.g. `seed`).
///
/// ## Determinism: `seed` is a drop, not a rejection
///
/// `IrRequest.params.seed` only has a defined carrier on `chat_completions`
/// (the OpenAI-compatible path). When sending to a target that does not
/// support `deterministic_seed`, we drop the field on the egress side; this
/// matches `protocol-capability-matrix.md` §4 ("seed → 其他协议 → 丢弃
/// seed（有损但不拒绝，seed 丢弃不影响语义正确性）").
pub fn check_lossy_conversion(
    request: &IrRequest,
    egress: &ProtocolEndpoint,
    egress_caps: &EndpointCapabilities,
) -> Result<(), (LossyDimension, Error)> {
    // 1. Tool calling — request has tools but target can't call functions.
    if !request.tools.is_empty() && !egress_caps.function_calling {
        return Err((
            LossyDimension::ToolCalling,
            lossy_error(LossyDimension::ToolCalling, egress, "tools"),
        ));
    }

    // 2. Parallel tool calls — IR doesn't model parallel_tool_calls as a first-class
    // field, but `Tool::required` is the closest analog. The chat-completions decoder
    // sets this when the original request had `parallel_tool_calls: true` paired
    // with `tool_choice != none`. When any tool is `required` but the egress
    // protocol cannot express parallel invocations, reject.
    let has_required_tools = request.tools.iter().any(|t| t.required);
    if has_required_tools && !egress_caps.parallel_tool_calls {
        return Err((
            LossyDimension::ParallelToolCalls,
            lossy_error(
                LossyDimension::ParallelToolCalls,
                egress,
                "tools marked required (parallel_tool_calls)",
            ),
        ));
    }

    // 3. tool_choice=required — IR exposes this via the `required` flag on at
    // least one tool, captured from `tool_choice: "required"`. Distinct from
    // (2) only when the request *also* disables parallel_tool_calls at the
    // protocol level; we conservatively attribute to ToolChoiceRequired.
    let has_required_choice = request
        .extensions
        .get("tool_choice")
        .and_then(|v| v.as_str())
        .map(|s| s == "required")
        .unwrap_or(false);
    if has_required_choice && !egress_caps.parallel_tool_calls {
        return Err((
            LossyDimension::ToolChoiceRequired,
            lossy_error(
                LossyDimension::ToolChoiceRequired,
                egress,
                "tool_choice=required",
            ),
        ));
    }

    // 4. tool_choice pinned to a specific function name. Stored under
    // extensions["tool_choice"] = {type: "function", function: {name: "x"}}.
    let has_specific_choice = request
        .extensions
        .get("tool_choice")
        .and_then(|v| v.get("type"))
        .and_then(|v| v.as_str())
        .map(|s| s == "function")
        .unwrap_or(false);
    if has_specific_choice && !egress_caps.parallel_tool_calls {
        return Err((
            LossyDimension::ToolChoiceSpecific,
            lossy_error(
                LossyDimension::ToolChoiceSpecific,
                egress,
                "tool_choice={type:function,name:...}",
            ),
        ));
    }

    // 5. Multimodal — scan all message contents for media parts whose source
    // kind is not expressible on the egress protocol.
    for msg in &request.messages {
        for content in &msg.content {
            if let Content::Media { source, .. } = content {
                if let Some(dim) = media_source_dimension(source, egress, egress_caps) {
                    let hint = format!("media part with kind {:?}", media_kind(source));
                    return Err((
                        dim,
                        lossy_error(dim, egress, &hint),
                    ));
                }
            }
        }
    }

    // 6. Structured output — response_format constrained but target doesn't
    // support it (Anthropic is the canonical example: no json_schema/json_object).
    if !matches!(request.response_format, None | Some(ResponseFormat::Text))
        && !egress_caps.structured_output
    {
        return Err((
            LossyDimension::StructuredOutput,
            lossy_error(
                LossyDimension::StructuredOutput,
                egress,
                "response_format (json_schema/json_object)",
            ),
        ));
    }

    // 7. Extended reasoning — request contains Reasoning content blocks (e.g.
    // Anthropic thinking) but target cannot carry reasoning.
    let has_reasoning = request
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .any(|c| matches!(c, Content::Reasoning { .. }));
    if has_reasoning && !egress_caps.extended_reasoning {
        return Err((
            LossyDimension::ExtendedReasoning,
            lossy_error(LossyDimension::ExtendedReasoning, egress, "reasoning content"),
        ));
    }

    Ok(())
}

/// Classify a single `MediaSource` against the egress protocol's media carrier
/// expectations. Returns `Some(dim)` when the source is not expressible.
///
/// We follow `protocol-capability-matrix.md` §2:
/// - `chat_completions`: inline image only; URL is fine; no audio/video; no file_id.
/// - `messages` (Anthropic): inline image/document; URL is lossy; no audio/video/file_id.
/// - `responses`: inline image/audio; URL; file_id; no video.
/// - `gemini`: inline image/audio/video/pdf; URL; no file_id.
fn media_source_dimension(
    source: &MediaSource,
    egress: &ProtocolEndpoint,
    caps: &EndpointCapabilities,
) -> Option<LossyDimension> {
    if !caps.multimodal {
        // Egress protocol cannot carry media at all — every media part is lossy.
        return Some(LossyDimension::MediaSourceUnsupported);
    }
    match (source, egress.suite) {
        (MediaSource::Inline { .. }, _) => None, // always expressible when caps.multimodal
        (MediaSource::Url { .. }, crate::protocol::ProtocolSuite::AnthropicMessages) => {
            // Anthropic requires pre-downloaded inline base64; URL would be silently dropped.
            Some(LossyDimension::MediaSourceUnsupported)
        }
        (MediaSource::Url { .. }, _) => None,
        (MediaSource::FileId { .. }, crate::protocol::ProtocolSuite::OpenAiResponses) => None,
        (MediaSource::FileId { .. }, _) => {
            // file_id is a Responses-only construct; other suites have no equivalent.
            Some(LossyDimension::MediaSourceUnsupported)
        }
    }
}

fn media_kind(source: &MediaSource) -> &'static str {
    match source {
        MediaSource::Inline { .. } => "inline",
        MediaSource::Url { .. } => "url",
        MediaSource::FileId { .. } => "file_id",
    }
}

fn lossy_error(dim: LossyDimension, egress: &ProtocolEndpoint, hint: &str) -> Error {
    Error::LossyRejection(format!(
        "{} not supported by target protocol {} (egress={}/{}, hint: {})",
        dim.label(),
        egress.suite.label(),
        egress.name,
        egress.version,
        hint
    ))
}
