//! Cross-protocol lossy conversion tests.
//!
//! The runtime check lives in `tiygate_core::protocol::lossy`; these tests
//! drive it with the real `EndpointCapabilities` of each codec. Whenever a
//! row or column changes in `docs/protocol-capability-matrix.md`, a matching
//! assertion should be added here so the runtime check and the documented
//! contract cannot drift apart.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::HashMap;
use tiygate_core::ir::{
    Content, MediaSource, PromptCacheBreakpoint, PromptCacheBreakpointMode, ResponseFormat,
};
use tiygate_core::protocol::lossy::{check_lossy_conversion, LossyDimension};
use tiygate_core::{
    EndpointCapabilities, EndpointCodec, IrRequest, Message, ProtocolEndpoint, ProtocolSuite, Role,
    Tool,
};
use tiygate_protocols::chat_completions::ChatCompletionsCodec;
use tiygate_protocols::gemini::GeminiCodec;
use tiygate_protocols::messages::MessagesCodec;
use tiygate_protocols::responses::ResponsesCodec;

fn req_with_tools() -> IrRequest {
    IrRequest {
        model: "m".to_string(),
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "hi".to_string(),
                annotations: None,
                prompt_cache_breakpoint: None,
            }],
        }],
        tools: vec![Tool {
            name: "get_weather".to_string(),
            description: Some("Get weather".to_string()),
            parameters: Some(serde_json::json!({})),
            required: false,
            ..Default::default()
        }],
        params: Default::default(),
        response_format: None,
        stream: false,
        ingress_protocol: ProtocolEndpoint::new(
            ProtocolSuite::OpenAiCompatible,
            "chat-completions",
            "v1",
        ),
        metadata: None,
        extensions: HashMap::new(),
    }
}

fn text_only_req() -> IrRequest {
    let mut r = req_with_tools();
    r.tools.clear();
    r
}

fn with_responses_reasoning_controls(req: &mut IrRequest) {
    req.params.thinking = Some(tiygate_core::ThinkingConfig {
        mode: Some("pro".to_string()),
        context: Some(serde_json::json!({"preserve": true})),
        ..Default::default()
    });
}

fn with_required_tool(req: &mut IrRequest) {
    if let Some(t) = req.tools.first_mut() {
        t.required = true;
    }
}

fn with_tool_choice_str(req: &mut IrRequest, val: &str) {
    req.extensions.insert(
        "tool_choice".to_string(),
        serde_json::Value::String(val.to_string()),
    );
}

fn with_specific_tool_choice(req: &mut IrRequest) {
    req.extensions.insert(
        "tool_choice".to_string(),
        serde_json::json!({"type": "function", "function": {"name": "x"}}),
    );
}

fn with_response_format(req: &mut IrRequest, rf: ResponseFormat) {
    req.response_format = Some(rf);
}

fn with_media_url(req: &mut IrRequest) {
    req.messages[0].content.push(Content::Media {
        source: MediaSource::Url {
            url: "https://example/cat.png".to_string(),
        },
        mime_type: "image/png".to_string(),
        metadata: HashMap::new(),
        prompt_cache_breakpoint: None,
    });
}

fn with_file_id_media(req: &mut IrRequest) {
    req.messages[0].content.push(Content::Media {
        source: MediaSource::FileId {
            id: "file_abc".to_string(),
        },
        mime_type: "image/png".to_string(),
        metadata: HashMap::new(),
        prompt_cache_breakpoint: None,
    });
}

fn with_inline_media(req: &mut IrRequest) {
    req.messages[0].content.push(Content::Media {
        source: MediaSource::Inline {
            data: "iVBORw0KGgo=".to_string(),
        },
        mime_type: "image/png".to_string(),
        metadata: HashMap::new(),
        prompt_cache_breakpoint: None,
    });
}

fn with_data_url_media(req: &mut IrRequest) {
    // Simulate what from_data_url produces for a data: URL
    let (source, mime_type) =
        MediaSource::from_data_url("data:image/png;base64,iVBORw0KGgo=", "image/*");
    req.messages[0].content.push(Content::Media {
        source,
        mime_type,
        metadata: HashMap::new(),
        prompt_cache_breakpoint: None,
    });
}

fn with_reasoning(req: &mut IrRequest) {
    req.messages[0].content.push(Content::Reasoning {
        text: "thinking...".to_string(),
        signature: None,
        id: None,
        encrypted_content: None,
    });
}

fn chat_caps() -> EndpointCapabilities {
    ChatCompletionsCodec::new().capabilities().clone()
}
fn messages_caps() -> EndpointCapabilities {
    MessagesCodec::new().capabilities().clone()
}
fn gemini_caps() -> EndpointCapabilities {
    GeminiCodec::new().capabilities().clone()
}
fn responses_caps() -> EndpointCapabilities {
    ResponsesCodec::new().capabilities().clone()
}
fn anthropic_endpoint() -> ProtocolEndpoint {
    ProtocolEndpoint::new(ProtocolSuite::AnthropicMessages, "messages", "2023-06-01")
}
fn gemini_endpoint() -> ProtocolEndpoint {
    ProtocolEndpoint::new(ProtocolSuite::GoogleGemini, "generateContent", "v1beta")
}
fn responses_endpoint() -> ProtocolEndpoint {
    ProtocolEndpoint::new(ProtocolSuite::OpenAiResponses, "responses", "v1")
}
fn chat_endpoint() -> ProtocolEndpoint {
    ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "chat-completions", "v1")
}

fn extract_dim(err: &Result<(), (LossyDimension, tiygate_core::Error)>) -> Option<LossyDimension> {
    err.as_ref().err().map(|(d, _)| *d)
}

// --- Dimension 1: tool_calling ---

#[test]
fn chat_to_anthropic_with_tools_passes() {
    // Anthropic supports tools via function_calling=true.
    let req = req_with_tools();
    assert!(check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps()).is_ok());
}

#[test]
fn chat_to_gemini_with_tools_passes() {
    let req = req_with_tools();
    assert!(check_lossy_conversion(&req, &gemini_endpoint(), &gemini_caps()).is_ok());
}

// --- Dimension 2/3/4: tool_choice forms ---

#[test]
fn required_tool_flag_still_rejected_by_parallel_tool_calls() {
    // Tool.required=true represents parallel_tool_calls semantics,
    // which Anthropic does not support. This is a separate dimension
    // from tool_choice=required, which Anthropic DOES support.
    let mut req = req_with_tools();
    with_required_tool(&mut req);
    let err = check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps());
    assert_eq!(extract_dim(&err), Some(LossyDimension::ParallelToolCalls));
}

#[test]
fn required_tool_to_anthropic_passes_via_tool_choice_required() {
    // Verify tool_choice=required is accepted by Anthropic
    // (gated on tool_choice_required=true, not parallel_tool_calls=false).
    let mut req = req_with_tools();
    with_tool_choice_str(&mut req, "required");
    let msg_caps = messages_caps();
    assert!(msg_caps.tool_choice_required);
    assert!(!msg_caps.parallel_tool_calls);
    assert!(check_lossy_conversion(&req, &anthropic_endpoint(), &msg_caps).is_ok());
}

#[test]
fn specific_tool_choice_to_anthropic_accepted() {
    // Anthropic supports tool_choice={type:"tool", name:"x"} natively.
    // This is gated on tool_choice_required (Anthropic=true).
    let mut req = req_with_tools();
    with_specific_tool_choice(&mut req);
    assert!(check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps()).is_ok());
}

#[test]
fn tool_choice_to_chat_completions_always_passes() {
    let mut req = req_with_tools();
    with_required_tool(&mut req);
    with_specific_tool_choice(&mut req);
    assert!(check_lossy_conversion(&req, &chat_endpoint(), &chat_caps()).is_ok());
}

#[test]
fn specific_tool_choice_to_gemini_accepted() {
    // Gemini now supports tool_choice=specific via
    // toolConfig.functionCallingConfig.mode=ANY + allowedFunctionNames.
    let mut req = req_with_tools();
    with_specific_tool_choice(&mut req);
    assert!(check_lossy_conversion(&req, &gemini_endpoint(), &gemini_caps()).is_ok());
}

#[test]
fn tool_choice_required_to_gemini_accepted() {
    // Gemini now supports tool_choice=required via
    // toolConfig.functionCallingConfig.mode=ANY.
    let mut req = req_with_tools();
    with_tool_choice_str(&mut req, "required");
    assert!(check_lossy_conversion(&req, &gemini_endpoint(), &gemini_caps()).is_ok());
}

// --- Dimension 5: media sources ---

#[test]
fn url_media_to_anthropic_rejected() {
    let mut req = text_only_req();
    with_media_url(&mut req);
    let err = check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps());
    assert_eq!(
        extract_dim(&err),
        Some(LossyDimension::MediaSourceUnsupported)
    );
}

#[test]
fn url_media_to_chat_completions_accepted() {
    let mut req = text_only_req();
    with_media_url(&mut req);
    assert!(check_lossy_conversion(&req, &chat_endpoint(), &chat_caps()).is_ok());
}

#[test]
fn file_id_media_to_responses_accepted() {
    let mut req = text_only_req();
    with_file_id_media(&mut req);
    assert!(check_lossy_conversion(&req, &responses_endpoint(), &responses_caps()).is_ok());
}

#[test]
fn file_id_media_to_anthropic_rejected() {
    let mut req = text_only_req();
    with_file_id_media(&mut req);
    let err = check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps());
    assert_eq!(
        extract_dim(&err),
        Some(LossyDimension::MediaSourceUnsupported)
    );
}

#[test]
fn inline_media_to_anthropic_accepted() {
    // Inline base64 is always accepted by Anthropic.
    let mut req = text_only_req();
    with_inline_media(&mut req);
    assert!(check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps()).is_ok());
}

#[test]
fn data_url_parsed_as_inline_passes_anthropic_lossy() {
    // A data: URL parsed by from_data_url becomes MediaSource::Inline,
    // which Anthropic accepts. This is the core scenario fixed by the
    // data-URL awareness patch.
    let mut req = text_only_req();
    with_data_url_media(&mut req);
    assert!(check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps()).is_ok());
}

// --- Dimension 6: structured output ---

#[test]
fn json_schema_to_anthropic_passes() {
    let mut req = text_only_req();
    with_response_format(
        &mut req,
        ResponseFormat::JsonSchema {
            name: "out".to_string(),
            schema: serde_json::json!({}),
            strict: Some(true),
        },
    );
    assert!(check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps()).is_ok());
}

#[test]
fn unsupported_json_schema_constraint_to_anthropic_is_rejected() {
    let mut req = text_only_req();
    with_response_format(
        &mut req,
        ResponseFormat::JsonSchema {
            name: "out".to_string(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {"score": {"type": "number", "minimum": 0}},
            }),
            strict: Some(true),
        },
    );
    let (_, error) =
        check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps()).unwrap_err();
    assert!(error.to_string().contains("/properties/score/minimum"));
}

#[test]
fn json_object_to_anthropic_passes() {
    let mut req = text_only_req();
    with_response_format(&mut req, ResponseFormat::JsonObject);
    assert!(check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps()).is_ok());
}

#[test]
fn text_response_format_always_passes() {
    let mut req = text_only_req();
    with_response_format(&mut req, ResponseFormat::Text);
    assert!(check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps()).is_ok());
}

// --- Dimension 7: extended reasoning ---

#[test]
fn reasoning_to_chat_completions_rejected() {
    let mut req = text_only_req();
    with_reasoning(&mut req);
    let err = check_lossy_conversion(&req, &chat_endpoint(), &chat_caps());
    assert_eq!(extract_dim(&err), Some(LossyDimension::ExtendedReasoning));
}

#[test]
fn reasoning_to_anthropic_passes() {
    let mut req = text_only_req();
    with_reasoning(&mut req);
    assert!(check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps()).is_ok());
}

// --- Sanity ---

#[test]
fn text_only_round_trip_never_rejected() {
    let req = text_only_req();
    for (label, endpoint, caps) in [
        ("chat", chat_endpoint(), chat_caps()),
        ("anthropic", anthropic_endpoint(), messages_caps()),
        ("gemini", gemini_endpoint(), gemini_caps()),
        ("responses", responses_endpoint(), responses_caps()),
    ] {
        let err = check_lossy_conversion(&req, &endpoint, &caps);
        assert!(
            err.is_ok(),
            "text-only request rejected at {label}: {err:?}"
        );
    }
}

#[test]
fn structured_output_is_not_reported_as_lossy_for_anthropic() {
    let mut req = text_only_req();
    with_response_format(
        &mut req,
        ResponseFormat::JsonSchema {
            name: "out".to_string(),
            schema: serde_json::json!({}),
            strict: None,
        },
    );
    assert!(check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps()).is_ok());
}

#[test]
fn hosted_and_programmatic_tools_are_rejected_outside_responses() {
    let mut hosted = text_only_req();
    hosted.tools.push(Tool {
        tool_type: Some("web_search".to_string()),
        ..Default::default()
    });
    let (dimension, _) =
        check_lossy_conversion(&hosted, &chat_endpoint(), &chat_caps()).unwrap_err();
    assert_eq!(dimension, LossyDimension::HostedTools);
    assert!(check_lossy_conversion(&hosted, &responses_endpoint(), &responses_caps()).is_ok());

    let mut allowed_callers = text_only_req();
    allowed_callers.tools.push(Tool {
        name: "lookup".to_string(),
        tool_type: Some("function".to_string()),
        config: Some(serde_json::json!({"allowed_callers": ["programmatic"]})),
        ..Default::default()
    });
    let (dimension, _) =
        check_lossy_conversion(&allowed_callers, &chat_endpoint(), &chat_caps()).unwrap_err();
    assert_eq!(dimension, LossyDimension::ProgrammaticToolCalling);
    assert!(
        check_lossy_conversion(&allowed_callers, &responses_endpoint(), &responses_caps()).is_ok()
    );

    let mut programmatic = text_only_req();
    programmatic.messages[0].content.push(Content::Program {
        id: "prog_1".to_string(),
        call_id: "call_prog_1".to_string(),
        code: "return 1".to_string(),
        fingerprint: "fp_1".to_string(),
    });
    let (dimension, _) =
        check_lossy_conversion(&programmatic, &anthropic_endpoint(), &messages_caps()).unwrap_err();
    assert_eq!(dimension, LossyDimension::ProgrammaticToolCalling);
    assert!(
        check_lossy_conversion(&programmatic, &responses_endpoint(), &responses_caps()).is_ok()
    );
}

#[test]
fn custom_tools_rejected_outside_openai() {
    let mut custom = text_only_req();
    custom.tools.push(Tool {
        name: "code_exec".to_string(),
        tool_type: Some("custom".to_string()),
        config: Some(serde_json::json!({"format": {"type": "text"}})),
        ..Default::default()
    });
    // Custom tools are expressible on Chat and Responses.
    assert!(check_lossy_conversion(&custom, &chat_endpoint(), &chat_caps()).is_ok());
    assert!(check_lossy_conversion(&custom, &responses_endpoint(), &responses_caps()).is_ok());
    // Custom tools are NOT expressible on Anthropic Messages or Gemini —
    // reject instead of silently dropping.
    let (dim, _) =
        check_lossy_conversion(&custom, &anthropic_endpoint(), &messages_caps()).unwrap_err();
    assert_eq!(dim, LossyDimension::CustomTools);
    let (dim, _) = check_lossy_conversion(&custom, &gemini_endpoint(), &gemini_caps()).unwrap_err();
    assert_eq!(dim, LossyDimension::CustomTools);
}

// --- Codex extension: opaque items should not trigger lossy rejection ---

#[test]
fn codex_opaque_items_do_not_trigger_lossy_rejection() {
    let mut req = text_only_req();
    req.extensions.insert(
        "codex_opaque_items".to_string(),
        serde_json::json!([{"type": "compaction", "id": "comp_1"}]),
    );
    // Should pass to all protocols — opaque items are silently dropped, not rejected.
    for (label, endpoint, caps) in [
        ("chat", chat_endpoint(), chat_caps()),
        ("anthropic", anthropic_endpoint(), messages_caps()),
        ("gemini", gemini_endpoint(), gemini_caps()),
        ("responses", responses_endpoint(), responses_caps()),
    ] {
        let err = check_lossy_conversion(&req, &endpoint, &caps);
        assert!(
            err.is_ok(),
            "codex_opaque_items should not trigger lossy rejection at {label}: {err:?}"
        );
    }
}

#[test]
fn multi_agent_rejected_outside_responses() {
    let mut with_config = text_only_req();
    with_config.extensions.insert(
        "responses_extra".to_string(),
        serde_json::json!({
            "multi_agent": {
                "enabled": true,
                "max_concurrent_subagents": 4
            }
        }),
    );

    assert!(
        check_lossy_conversion(&with_config, &responses_endpoint(), &responses_caps()).is_ok(),
        "Responses should accept multi_agent config"
    );

    for (label, endpoint, caps) in [
        ("chat", chat_endpoint(), chat_caps()),
        ("anthropic", anthropic_endpoint(), messages_caps()),
        ("gemini", gemini_endpoint(), gemini_caps()),
    ] {
        let (dim, err) = check_lossy_conversion(&with_config, &endpoint, &caps).unwrap_err();
        assert_eq!(
            dim,
            LossyDimension::MultiAgent,
            "{label} should reject multi_agent config"
        );
        assert!(
            err.to_string().contains("multi_agent"),
            "{label} rejection should mention multi_agent: {err}"
        );
    }

    let mut with_items = text_only_req();
    with_items.extensions.insert(
        "multi_agent_items".to_string(),
        serde_json::json!([{"type": "multi_agent_call", "id": "ma_1", "name": "spawn_agent"}]),
    );
    assert!(
        check_lossy_conversion(&with_items, &responses_endpoint(), &responses_caps()).is_ok(),
        "Responses should accept multi_agent_items"
    );
    let (dim, _) = check_lossy_conversion(&with_items, &chat_endpoint(), &chat_caps()).unwrap_err();
    assert_eq!(dim, LossyDimension::MultiAgent);
}

// --- Prompt cache breakpoint ---

#[test]
fn prompt_cache_breakpoint_rejected_outside_openai() {
    let mut req = text_only_req();
    req.messages[0].content.push(Content::Text {
        text: "cached prefix".to_string(),
        annotations: None,
        prompt_cache_breakpoint: Some(PromptCacheBreakpoint {
            mode: PromptCacheBreakpointMode::Explicit,
        }),
    });

    // Chat and Responses carry the breakpoint on the canonical content block.
    assert!(
        check_lossy_conversion(&req, &chat_endpoint(), &chat_caps()).is_ok(),
        "Chat should accept prompt_cache_breakpoint"
    );
    assert!(
        check_lossy_conversion(&req, &responses_endpoint(), &responses_caps()).is_ok(),
        "Responses should accept prompt_cache_breakpoint"
    );

    // Anthropic and Gemini have no equivalent carrier — reject, not silently drop.
    let (dim, _) =
        check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps()).unwrap_err();
    assert_eq!(
        dim,
        LossyDimension::PromptCacheBreakpoint,
        "Anthropic should reject prompt_cache_breakpoint"
    );
    let (dim, _) = check_lossy_conversion(&req, &gemini_endpoint(), &gemini_caps()).unwrap_err();
    assert_eq!(
        dim,
        LossyDimension::PromptCacheBreakpoint,
        "Gemini should reject prompt_cache_breakpoint"
    );
}

#[test]
fn verbosity_rejected_outside_openai() {
    let mut req = text_only_req();
    req.params.verbosity = Some(tiygate_core::Verbosity::High);

    assert!(check_lossy_conversion(&req, &chat_endpoint(), &chat_caps()).is_ok());
    assert!(check_lossy_conversion(&req, &responses_endpoint(), &responses_caps()).is_ok());

    let (dim, _) =
        check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps()).unwrap_err();
    assert_eq!(dim, LossyDimension::Verbosity);
    let (dim, _) = check_lossy_conversion(&req, &gemini_endpoint(), &gemini_caps()).unwrap_err();
    assert_eq!(dim, LossyDimension::Verbosity);
}

#[test]
fn responses_reasoning_mode_and_context_rejected_outside_responses() {
    let mut req = text_only_req();
    with_responses_reasoning_controls(&mut req);

    assert!(check_lossy_conversion(&req, &responses_endpoint(), &responses_caps()).is_ok());

    for (endpoint, caps) in [
        (chat_endpoint(), chat_caps()),
        (anthropic_endpoint(), messages_caps()),
        (gemini_endpoint(), gemini_caps()),
    ] {
        let (dimension, _) = check_lossy_conversion(&req, &endpoint, &caps).unwrap_err();
        assert_eq!(dimension, LossyDimension::ExtendedReasoning);
    }
}
