//! Cross-protocol lossy conversion tests.
//!
//! The runtime check lives in `tiygate_core::protocol::lossy`; these tests
//! drive it with the real `EndpointCapabilities` of each codec. Whenever a
//! row or column changes in `docs/protocol-capability-matrix.md`, a matching
//! assertion should be added here so the runtime check and the documented
//! contract cannot drift apart.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::HashMap;
use tiygate_core::ir::{Content, MediaSource, ResponseFormat};
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
            }],
        }],
        tools: vec![Tool {
            name: "get_weather".to_string(),
            description: Some("Get weather".to_string()),
            parameters: Some(serde_json::json!({})),
            required: false,
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
    });
}

fn with_file_id_media(req: &mut IrRequest) {
    req.messages[0].content.push(Content::Media {
        source: MediaSource::FileId {
            id: "file_abc".to_string(),
        },
        mime_type: "image/png".to_string(),
        metadata: HashMap::new(),
    });
}

fn with_inline_media(req: &mut IrRequest) {
    req.messages[0].content.push(Content::Media {
        source: MediaSource::Inline {
            data: "iVBORw0KGgo=".to_string(),
        },
        mime_type: "image/png".to_string(),
        metadata: HashMap::new(),
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
fn json_schema_to_anthropic_rejected() {
    let mut req = text_only_req();
    with_response_format(
        &mut req,
        ResponseFormat::JsonSchema {
            name: "out".to_string(),
            schema: serde_json::json!({}),
            strict: Some(true),
        },
    );
    let err = check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps());
    assert_eq!(extract_dim(&err), Some(LossyDimension::StructuredOutput));
}

#[test]
fn json_object_to_responses_passes() {
    let mut req = text_only_req();
    with_response_format(&mut req, ResponseFormat::JsonObject);
    assert!(check_lossy_conversion(&req, &responses_endpoint(), &responses_caps()).is_ok());
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
fn error_message_names_dimension() {
    let mut req = text_only_req();
    with_response_format(
        &mut req,
        ResponseFormat::JsonSchema {
            name: "out".to_string(),
            schema: serde_json::json!({}),
            strict: None,
        },
    );
    let (_, err) = check_lossy_conversion(&req, &anthropic_endpoint(), &messages_caps())
        .expect_err("expected lossy rejection");
    let msg = err.to_string();
    assert!(
        msg.contains("response_format"),
        "error should name the dimension; got: {msg}"
    );
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
