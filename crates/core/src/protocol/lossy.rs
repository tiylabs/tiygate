//! Cross-protocol lossy conversion detection.
//!
//! §3.2 of the design requires the gateway to reject cross-protocol conversions
//! that would silently drop fields. Per the §8 acceptance criteria, the
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
use crate::protocol::structured_output::validate_response_format_for_target;
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
    /// Request contains non-function hosted tools unsupported by the target.
    HostedTools,
    /// Request contains OpenAI `type: "custom"` tool definitions unsupported by the target.
    CustomTools,
    /// Request contains Responses program state or program caller links.
    ProgrammaticToolCalling,
    /// Request uses output verbosity unsupported by the target.
    Verbosity,
    /// Request carries explicit content-block cache breakpoints unsupported by the target.
    PromptCacheBreakpoint,
    /// Request has `extended_reasoning` (Anthropic-style thinking blocks) but
    /// the egress protocol cannot carry reasoning parts.
    ExtendedReasoning,
    /// Request uses OpenAI Responses multi-agent beta state that only Responses
    /// can express (top-level `multi_agent` and/or multi-agent input items).
    MultiAgent,
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
            Self::HostedTools => "hosted_tools",
            Self::CustomTools => "custom_tools",
            Self::ProgrammaticToolCalling => "programmatic_tool_calling",
            Self::Verbosity => "verbosity",
            Self::PromptCacheBreakpoint => "prompt_cache_breakpoint",
            Self::ExtendedReasoning => "extended_reasoning",
            Self::MultiAgent => "multi_agent",
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

    // 3. tool_choice=required — IR exposes this via extensions["tool_choice"].
    // Gated on `tool_choice_required` (not `parallel_tool_calls`), because
    // Anthropic supports required via `{type:"any"}` but not concurrent fan-out.
    let has_required_choice = request
        .extensions
        .get("tool_choice")
        .and_then(|v| v.as_str())
        .map(|s| s == "required")
        .unwrap_or(false);
    if has_required_choice && !egress_caps.tool_choice_required {
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
    if has_specific_choice && !egress_caps.tool_choice_required {
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
                    return Err((dim, lossy_error(dim, egress, &hint)));
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
    if let Err(error) =
        validate_response_format_for_target(request.response_format.as_ref(), egress)
    {
        return Err((
            LossyDimension::StructuredOutput,
            lossy_error(LossyDimension::StructuredOutput, egress, &error.to_string()),
        ));
    }

    // 7. Hosted tools are first-class only on Responses. Do not silently
    // filter them from Chat/Messages/Gemini requests. Custom tools
    // (`type: "custom"`) are expressible on Chat and Responses but not on
    // Anthropic Messages or Gemini, so reject them on those egress paths
    // instead of silently dropping the tool definition.
    let openai_egress = matches!(
        egress.suite,
        crate::protocol::ProtocolSuite::OpenAiCompatible
            | crate::protocol::ProtocolSuite::OpenAiResponses
    );
    if request.tools.iter().any(|tool| tool.is_hosted()) && !egress_caps.hosted_tools {
        return Err((
            LossyDimension::HostedTools,
            lossy_error(
                LossyDimension::HostedTools,
                egress,
                "non-function hosted tool definition",
            ),
        ));
    }
    if request.tools.iter().any(|tool| tool.is_custom()) && !openai_egress {
        return Err((
            LossyDimension::CustomTools,
            lossy_error(
                LossyDimension::CustomTools,
                egress,
                "custom tool definition (OpenAI-only)",
            ),
        ));
    }

    // 8. Programmatic Tool Calling state cannot be flattened without losing
    // replay state or caller ancestry.
    let has_programmatic_state = request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .any(|content| {
            matches!(
                content,
                Content::Program { .. }
                    | Content::ProgramOutput { .. }
                    | Content::ToolCall {
                        caller: Some(_),
                        ..
                    }
                    | Content::ToolResult {
                        caller: Some(_),
                        ..
                    }
            )
        });
    let tool_config_uses_programmatic_callers = request.tools.iter().any(|tool| {
        tool.is_function()
            && tool
                .config
                .as_ref()
                .and_then(|config| config.get("allowed_callers"))
                .is_some()
    });
    if (has_programmatic_state || tool_config_uses_programmatic_callers)
        && !egress_caps.programmatic_tool_calling
    {
        return Err((
            LossyDimension::ProgrammaticToolCalling,
            lossy_error(
                LossyDimension::ProgrammaticToolCalling,
                egress,
                "program/program_output/caller relationship",
            ),
        ));
    }

    if request.params.verbosity.is_some() && !openai_egress {
        return Err((
            LossyDimension::Verbosity,
            lossy_error(LossyDimension::Verbosity, egress, "text verbosity"),
        ));
    }

    let has_breakpoint = request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .any(|content| {
            matches!(
                content,
                Content::Text {
                    prompt_cache_breakpoint: Some(_),
                    ..
                } | Content::Media {
                    prompt_cache_breakpoint: Some(_),
                    ..
                }
            )
        });
    if has_breakpoint && !openai_egress {
        return Err((
            LossyDimension::PromptCacheBreakpoint,
            lossy_error(
                LossyDimension::PromptCacheBreakpoint,
                egress,
                "explicit content-block cache breakpoint",
            ),
        ));
    }

    // 9. Extended reasoning — request contains Reasoning content blocks (e.g.
    // Anthropic thinking) but target cannot carry reasoning.
    let has_reasoning = request
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .any(|c| matches!(c, Content::Reasoning { .. }));
    if has_reasoning && !egress_caps.extended_reasoning {
        return Err((
            LossyDimension::ExtendedReasoning,
            lossy_error(
                LossyDimension::ExtendedReasoning,
                egress,
                "reasoning content",
            ),
        ));
    }

    // `reasoning.mode` and `reasoning.context` are Responses-only request
    // controls. Unlike replayed reasoning content, they live in generation
    // parameters, so the check above cannot see them. Reject crossings to
    // every other suite rather than allowing their encoders to silently drop
    // the controls.
    let has_responses_reasoning_controls = request
        .params
        .thinking
        .as_ref()
        .is_some_and(|thinking| thinking.mode.is_some() || thinking.context.is_some());
    if has_responses_reasoning_controls
        && egress.suite != crate::protocol::ProtocolSuite::OpenAiResponses
    {
        return Err((
            LossyDimension::ExtendedReasoning,
            lossy_error(
                LossyDimension::ExtendedReasoning,
                egress,
                "Responses-only reasoning.mode or reasoning.context",
            ),
        ));
    }

    // 10. Multi-agent Beta is Responses-only. Detect either the top-level
    // `multi_agent` config bag or opaque multi-agent input items; reject when
    // the egress suite cannot express them (do not silently drop).
    let has_multi_agent_config = request
        .extensions
        .get("responses_extra")
        .and_then(|v| v.get("multi_agent"))
        .is_some();
    let has_multi_agent_items = request
        .extensions
        .get("multi_agent_items")
        .and_then(|v| v.as_array())
        .is_some_and(|items| !items.is_empty());
    if (has_multi_agent_config || has_multi_agent_items)
        && !matches!(
            egress.suite,
            crate::protocol::ProtocolSuite::OpenAiResponses
        )
    {
        return Err((
            LossyDimension::MultiAgent,
            lossy_error(
                LossyDimension::MultiAgent,
                egress,
                "multi_agent config or multi_agent_call items (Responses-only)",
            ),
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
