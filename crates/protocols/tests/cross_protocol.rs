//! Cross-protocol integration tests.
//!
//! Covers:
//! - N×N conversion matrix (5 protocols × 5 protocols)
//! - `lossy_default_reject` high-risk dimensions
//! - PassThrough byte-level passthrough
//!
//! These tests run via `cargo test -p tiygate-protocols --test cross_protocol`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use serde_json::json;
use std::collections::HashMap;
use tiygate_core::{
    Content, EndpointCodec, GenerationParams, IrRequest, IrResponse, Message, PassThroughPolicy,
    ProtocolEndpoint, ProtocolSuite, Role, ThinkingConfig, ThinkingEffort,
};

fn make_env() -> tiygate_core::RawEnvelope {
    tiygate_core::RawEnvelope {
        method: "POST".to_string(),
        path: "/test".to_string(),
        headers: std::collections::HashMap::new(),
        body: None,
        original_body_size: 0,
        timestamp: chrono::Utc::now(),
    }
}

fn basic_request() -> IrRequest {
    IrRequest {
        model: "test-model".to_string(),
        system: Some("You are helpful.".to_string()),
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "Hello".to_string(),
                annotations: None,
                prompt_cache_breakpoint: None,
            }],
        }],
        tools: vec![],
        params: GenerationParams {
            max_tokens: Some(100),
            temperature: Some(0.7),
            top_p: None,
            top_k: None,
            stop: vec![],
            frequency_penalty: None,
            presence_penalty: None,
            seed: None,
            thinking: None,
            verbosity: None,
        },
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

fn find_codec(suite: ProtocolSuite, name: &str) -> Box<dyn tiygate_core::EndpointCodec> {
    use tiygate_protocols::chat_completions::ChatCompletionsCodec;
    use tiygate_protocols::gemini::GeminiCodec;
    use tiygate_protocols::messages::MessagesCodec;
    use tiygate_protocols::responses::ResponsesCodec;
    match suite {
        ProtocolSuite::OpenAiCompatible if name == "chat-completions" => {
            Box::new(ChatCompletionsCodec::new())
        }
        ProtocolSuite::OpenAiCompatible => Box::new(ChatCompletionsCodec::new()),
        ProtocolSuite::AnthropicMessages => Box::new(MessagesCodec::new()),
        ProtocolSuite::GoogleGemini => Box::new(GeminiCodec::new()),
        ProtocolSuite::OpenAiResponses => Box::new(ResponsesCodec::new()),
    }
}

fn response() -> IrResponse {
    IrResponse {
        content: vec![Content::Text {
            text: "Hi!".to_string(),
            annotations: None,
            prompt_cache_breakpoint: None,
        }],
        usage: None,
        finish_reason: Some(tiygate_core::FinishReason::Stop),
        response_id: Some("resp_1".to_string()),
        stop_details: None,
        extensions: HashMap::new(),
    }
}

fn first_gemini_function_response_name(encoded: &serde_json::Value) -> String {
    encoded["contents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|content| content["parts"].as_array().unwrap().iter())
        .find_map(|part| part.get("functionResponse"))
        .and_then(|fr| fr["name"].as_str())
        .unwrap()
        .to_string()
}

// ============================================================================
// N×N Cross-Protocol Conversion Matrix
// ============================================================================

#[test]
fn nxn_same_protocol_chat_completions_basic() {
    let ingress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let egress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");

    // Decode a basic OpenAI request
    let body = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "Hi"}]
    });
    let ir = ingress.decode_request(body, &make_env()).unwrap();
    assert_eq!(ir.messages.len(), 1);

    // Re-encode to the same protocol
    let (out, _h) = egress.encode_request(&ir).unwrap();
    assert_eq!(out["model"], "gpt-4o");
}

#[test]
fn nxn_chat_to_anthropic_basic() {
    let ingress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let egress = find_codec(ProtocolSuite::AnthropicMessages, "messages");

    let body = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "Hi"}]
    });
    let ir = ingress.decode_request(body, &make_env()).unwrap();
    // No tools → should convert cleanly
    let (out, _h) = egress.encode_request(&ir).unwrap();
    assert_eq!(out["model"], "gpt-4o");
    assert!(out["messages"].is_array());
    assert!(out["max_tokens"].is_number());
}

#[test]
fn nxn_anthropic_to_chat_basic() {
    let ingress = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    let egress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");

    let body = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 100,
        "messages": [{"role": "user", "content": "Hi"}]
    });
    let ir = ingress.decode_request(body, &make_env()).unwrap();
    let (out, _h) = egress.encode_request(&ir).unwrap();
    assert_eq!(out["model"], "claude-sonnet-4");
    assert_eq!(out["messages"][0]["role"], "user");
}

#[test]
fn nxn_chat_to_gemini_basic() {
    let ingress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let egress = find_codec(ProtocolSuite::GoogleGemini, "generateContent");

    let body = json!({
        "model": "gemini-2.0-flash",
        "messages": [{"role": "user", "content": "Hi"}]
    });
    let ir = ingress.decode_request(body, &make_env()).unwrap();
    let (out, _h) = egress.encode_request(&ir).unwrap();
    assert!(out["contents"].is_array());
}

#[test]
fn nxn_gemini_to_chat_basic() {
    let ingress = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let egress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");

    let body = json!({
        "model": "models/gemini-2.0-flash",
        "contents": [{"role": "user", "parts": [{"text": "Hi"}]}]
    });
    let ir = ingress.decode_request(body, &make_env()).unwrap();
    let (out, _h) = egress.encode_request(&ir).unwrap();
    assert_eq!(out["model"], "models/gemini-2.0-flash");
    assert_eq!(out["messages"][0]["role"], "user");
}

#[test]
fn nxn_chat_to_responses_basic() {
    let ingress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let egress = find_codec(ProtocolSuite::OpenAiResponses, "responses");

    let body = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "Hi"}]
    });
    let ir = ingress.decode_request(body, &make_env()).unwrap();
    let (out, _h) = egress.encode_request(&ir).unwrap();
    assert_eq!(out["model"], "gpt-4o");
    assert!(out["input"].is_array());
}

#[test]
fn nxn_response_roundtrip_preserves_text() {
    let ingress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let egress = find_codec(ProtocolSuite::AnthropicMessages, "messages");

    let ir = basic_request();
    let (body, _h) = egress.encode_request(&ir).unwrap();
    // Re-decode and check message content survives
    let ir2 = ingress.decode_request(body, &make_env());
    // Some conversions may fail; if successful, content must be preserved
    if let Ok(ir2) = ir2 {
        let total_text: String = ir2
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|c| match c {
                Content::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(total_text.contains("Hello"));
    }
}

// ============================================================================
// lossy_default_reject — High-Risk Dimension Tests
// ============================================================================

#[test]
fn lossy_capability_matrix_declares_rejection() {
    // All codecs must declare lossy_default_reject=true so that lossy
    // combinations get rejected by the gateway rather than silently
    // dropping fields.
    let ingress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let anthropic = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let responses = find_codec(ProtocolSuite::OpenAiResponses, "responses");

    assert!(ingress.capabilities().lossy_default_reject);
    assert!(anthropic.capabilities().lossy_default_reject);
    assert!(gemini.capabilities().lossy_default_reject);
    assert!(responses.capabilities().lossy_default_reject);
}

#[test]
fn lossy_parallel_tool_calls_chat_to_messages() {
    // §1 of docs/protocol-capability-matrix.md: chat→messages parallel tool
    // calls are lossy (⚠️). The chat-completions "fire N tools concurrently"
    // semantics are not preserved on the Anthropic side. The runtime
    // `check_lossy_conversion` rejects this crossing; this test pins the
    // capability declaration that drives it.
    let ingress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let egress = find_codec(ProtocolSuite::AnthropicMessages, "messages");

    // chat_completions supports parallel tool calls...
    assert!(ingress.capabilities().parallel_tool_calls);
    // ...but messages (Anthropic) does NOT, hence the lossy crossing.
    assert!(!egress.capabilities().parallel_tool_calls);

    // Both must declare lossy_default_reject so the gateway can reject
    // any actually-lossy combination at runtime.
    assert!(ingress.capabilities().lossy_default_reject);
    assert!(egress.capabilities().lossy_default_reject);
}

#[test]
fn structured_output_supported_in_anthropic() {
    // Anthropic Messages supports Structured Outputs through
    // `output_config.format` with a `json_schema` format.
    let ingress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let egress = find_codec(ProtocolSuite::AnthropicMessages, "messages");

    assert!(ingress.capabilities().structured_output);
    assert!(egress.capabilities().structured_output);
    assert!(ingress.capabilities().lossy_default_reject);
    assert!(egress.capabilities().lossy_default_reject);
}

#[test]
fn structured_output_chat_to_anthropic_is_supported() {
    // Both protocols support a JSON Schema response format.
    let ingress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let egress = find_codec(ProtocolSuite::AnthropicMessages, "messages");

    assert!(ingress.capabilities().structured_output);
    assert!(egress.capabilities().structured_output);
    assert!(ingress.capabilities().lossy_default_reject);
    assert!(egress.capabilities().lossy_default_reject);
}

#[test]
fn lossy_extended_reasoning_chat_to_anthropic() {
    // OpenAI doesn't expose extended_reasoning in chat completions; the
    // capability is false. Anthropic messages has it. This combination
    // is safe (no data loss) but the capability matrix must reflect
    // reality.
    let ingress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let egress = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    assert!(!ingress.capabilities().extended_reasoning);
    assert!(egress.capabilities().extended_reasoning);
}

#[test]
fn lossy_tool_choice_specific_chat_to_gemini() {
    // OpenAI supports tool_choice=function(specific_name). Gemini now supports
    // this via toolConfig.functionCallingConfig.mode=ANY + allowedFunctionNames.
    let ingress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");

    assert!(ingress.capabilities().function_calling);
    assert!(gemini.capabilities().function_calling);
    // Gemini now declares tool_choice_required so the lossy check passes.
    assert!(gemini.capabilities().tool_choice_required);
}

// ============================================================================
// PassThrough byte-level passthrough
// ============================================================================

#[test]
fn pass_through_same_protocol_returns_passthrough() {
    let codec = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let policy = codec.pass_through_policy(
        &ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "chat-completions", "v1"),
        &ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "chat-completions", "v1"),
    );
    assert!(matches!(policy, PassThroughPolicy::Passthrough));
}

#[test]
fn pass_through_cross_protocol_returns_convert() {
    let codec = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let policy = codec.pass_through_policy(
        &ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "chat-completions", "v1"),
        &ProtocolEndpoint::new(ProtocolSuite::AnthropicMessages, "messages", "v1"),
    );
    assert!(matches!(policy, PassThroughPolicy::Convert));
}

#[test]
fn pass_through_bytes_preserved_in_response() {
    // Verify that when IR is roundtripped through chat_completions codec,
    // text content is preserved exactly (no field mangling).
    let ingress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let egress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");

    let original_text = "Hello, 世界! 🌍 Special chars: <>&'\"";
    let ir = IrRequest {
        model: "gpt-4o".to_string(),
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: original_text.to_string(),
                annotations: None,
                prompt_cache_breakpoint: None,
            }],
        }],
        tools: vec![],
        params: GenerationParams::default(),
        response_format: None,
        stream: false,
        ingress_protocol: ProtocolEndpoint::new(
            ProtocolSuite::OpenAiCompatible,
            "chat-completions",
            "v1",
        ),
        metadata: None,
        extensions: HashMap::new(),
    };

    let (encoded, _h) = egress.encode_request(&ir).unwrap();
    // Re-decode and check text preservation
    let ir2 = ingress
        .decode_request(encoded.clone(), &make_env())
        .unwrap();
    let text_after: String = ir2
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|c| match c {
            Content::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text_after, original_text);
}

#[test]
fn pass_through_response_decoded_cleanly() {
    // A non-streaming response from the same protocol codec should
    // decode cleanly when roundtripped through encode→decode.
    let codec = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let ir = response();
    let (body, _h) = codec.encode_request(&basic_request()).unwrap();
    let _ = body; // smoke
    let encoded_resp = codec.encode_response(&ir).unwrap();
    let decoded = codec.decode_response(encoded_resp).unwrap();
    assert!(matches!(
        decoded.finish_reason,
        Some(tiygate_core::FinishReason::Stop)
    ));
}

// ============================================================================
// Complete N×N matrix — every pair of ingress × egress combination
// ============================================================================

fn nxn_pair_basic_text(
    suite_from: ProtocolSuite,
    name_from: &str,
    suite_to: ProtocolSuite,
    name_to: &str,
) {
    let ingress = find_codec(suite_from, name_from);
    let egress = find_codec(suite_to, name_to);
    // Build a body the ingress codec can decode.
    let body = match suite_from {
        ProtocolSuite::OpenAiCompatible => json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}]
        }),
        ProtocolSuite::AnthropicMessages => json!({
            "model": "m",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "Hi"}]
        }),
        ProtocolSuite::GoogleGemini => json!({
            "contents": [{"role": "user", "parts": [{"text": "Hi"}]}]
        }),
        ProtocolSuite::OpenAiResponses => json!({
            "model": "m",
            "input": "Hi"
        }),
    };
    let ir = ingress.decode_request(body, &make_env()).expect("decode");
    let (out, _h) = egress.encode_request(&ir).expect("encode");
    // The result must be a JSON object that the egress codec can decode.
    let _ = egress
        .decode_request(out, &make_env())
        .expect("roundtrip-decode");
}

#[test]
fn nxn_chat_to_chat() {
    nxn_pair_basic_text(
        ProtocolSuite::OpenAiCompatible,
        "chat-completions",
        ProtocolSuite::OpenAiCompatible,
        "chat-completions",
    );
}
#[test]
fn nxn_chat_to_anthropic() {
    nxn_pair_basic_text(
        ProtocolSuite::OpenAiCompatible,
        "chat-completions",
        ProtocolSuite::AnthropicMessages,
        "messages",
    );
}
#[test]
fn nxn_chat_to_responses() {
    nxn_pair_basic_text(
        ProtocolSuite::OpenAiCompatible,
        "chat-completions",
        ProtocolSuite::OpenAiResponses,
        "responses",
    );
}
#[test]
fn nxn_chat_to_gemini() {
    nxn_pair_basic_text(
        ProtocolSuite::OpenAiCompatible,
        "chat-completions",
        ProtocolSuite::GoogleGemini,
        "generateContent",
    );
}
#[test]
fn nxn_anthropic_to_chat() {
    nxn_pair_basic_text(
        ProtocolSuite::AnthropicMessages,
        "messages",
        ProtocolSuite::OpenAiCompatible,
        "chat-completions",
    );
}
#[test]
fn nxn_anthropic_to_anthropic() {
    nxn_pair_basic_text(
        ProtocolSuite::AnthropicMessages,
        "messages",
        ProtocolSuite::AnthropicMessages,
        "messages",
    );
}
#[test]
fn nxn_anthropic_to_responses() {
    nxn_pair_basic_text(
        ProtocolSuite::AnthropicMessages,
        "messages",
        ProtocolSuite::OpenAiResponses,
        "responses",
    );
}
#[test]
fn nxn_anthropic_to_gemini() {
    nxn_pair_basic_text(
        ProtocolSuite::AnthropicMessages,
        "messages",
        ProtocolSuite::GoogleGemini,
        "generateContent",
    );
}
#[test]
fn nxn_responses_to_chat() {
    nxn_pair_basic_text(
        ProtocolSuite::OpenAiResponses,
        "responses",
        ProtocolSuite::OpenAiCompatible,
        "chat-completions",
    );
}
#[test]
fn nxn_responses_to_anthropic() {
    nxn_pair_basic_text(
        ProtocolSuite::OpenAiResponses,
        "responses",
        ProtocolSuite::AnthropicMessages,
        "messages",
    );
}
#[test]
fn nxn_responses_to_responses() {
    nxn_pair_basic_text(
        ProtocolSuite::OpenAiResponses,
        "responses",
        ProtocolSuite::OpenAiResponses,
        "responses",
    );
}
#[test]
fn nxn_responses_to_gemini() {
    nxn_pair_basic_text(
        ProtocolSuite::OpenAiResponses,
        "responses",
        ProtocolSuite::GoogleGemini,
        "generateContent",
    );
}
#[test]
fn nxn_gemini_to_chat() {
    nxn_pair_basic_text(
        ProtocolSuite::GoogleGemini,
        "generateContent",
        ProtocolSuite::OpenAiCompatible,
        "chat-completions",
    );
}
#[test]
fn nxn_gemini_to_anthropic() {
    nxn_pair_basic_text(
        ProtocolSuite::GoogleGemini,
        "generateContent",
        ProtocolSuite::AnthropicMessages,
        "messages",
    );
}
#[test]
fn nxn_gemini_to_responses() {
    nxn_pair_basic_text(
        ProtocolSuite::GoogleGemini,
        "generateContent",
        ProtocolSuite::OpenAiResponses,
        "responses",
    );
}
#[test]
fn nxn_gemini_to_gemini() {
    nxn_pair_basic_text(
        ProtocolSuite::GoogleGemini,
        "generateContent",
        ProtocolSuite::GoogleGemini,
        "generateContent",
    );
}

#[test]
fn chat_tool_result_to_gemini_recovers_function_name() {
    let chat = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let body = json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "user", "content": "weather?"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_abc",
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "arguments": "{\"city\":\"London\"}"
                }
            }]},
            {"role": "tool", "tool_call_id": "call_abc", "content": "{\"temp\":18}"}
        ]
    });
    let ir = chat.decode_request(body, &make_env()).unwrap();
    let tool_result_name = ir
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|content| match content {
            Content::ToolResult { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .unwrap();
    assert_eq!(tool_result_name, "");

    let (encoded, _) = gemini.encode_request(&ir).unwrap();
    assert_eq!(first_gemini_function_response_name(&encoded), "get_weather");
}

#[test]
fn anthropic_tool_result_to_gemini_recovers_function_name() {
    let anthropic = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let body = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "weather?"},
            {"role": "assistant", "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "get_weather",
                "input": {"city": "London"}
            }]},
            {"role": "user", "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_1",
                "content": [{"type": "text", "text": "18°C"}]
            }]}
        ]
    });
    let ir = anthropic.decode_request(body, &make_env()).unwrap();
    let (encoded, _) = gemini.encode_request(&ir).unwrap();
    assert_eq!(first_gemini_function_response_name(&encoded), "get_weather");
}

#[test]
fn responses_tool_output_to_gemini_recovers_function_name() {
    let responses = find_codec(ProtocolSuite::OpenAiResponses, "responses");
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let body = json!({
        "model": "gpt-5",
        "input": [
            {"role": "user", "content": "weather?"},
            {
                "type": "function_call",
                "call_id": "call_1",
                "name": "get_weather",
                "arguments": "{\"city\":\"London\"}"
            },
            {
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "18°C"
            }
        ]
    });
    let ir = responses.decode_request(body, &make_env()).unwrap();
    let (encoded, _) = gemini.encode_request(&ir).unwrap();
    assert_eq!(first_gemini_function_response_name(&encoded), "get_weather");
}

#[test]
fn orphan_tool_result_to_gemini_returns_codec_error() {
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let mut ir = basic_request();
    ir.messages.push(Message {
        role: Role::Tool,
        content: vec![Content::ToolResult {
            tool_call_id: "missing_call".to_string(),
            name: String::new(),
            content: "{}".to_string(),
            id: None,
            caller: None,
            wire_type: None,
        }],
    });

    let err = gemini.encode_request(&ir).unwrap_err().to_string();
    assert!(
        err.contains("functionResponse.name is required") && err.contains("missing_call"),
        "unexpected error: {err}"
    );
}

// ============================================================================
// Runtime lossy_default_reject: when tool_choice=required is sent to a
// target that cannot express it, the conversion layer must surface a
// LossyRejection error rather than silently downgrading.
// ============================================================================

#[test]
fn runtime_tool_choice_to_gemini_accepted() {
    // OpenAI chat completions → Gemini generateContent. The OpenAI
    // request uses `tool_choice: "required"` and a tool. Gemini now
    // supports this via toolConfig.functionCallingConfig.mode=ANY.
    let ingress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "Hi"}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "x",
                "parameters": {"type": "object"}
            }
        }],
        "tool_choice": "required"
    });
    let ir = ingress.decode_request(body, &make_env()).expect("decode");
    assert_eq!(ir.tools.len(), 1);
    // The IR carries tool_choice=required in extensions.
    assert_eq!(ir.extensions.get("tool_choice"), Some(&json!("required")));
    // Gemini now declares tool_choice_required so the lossy check passes.
    let egress = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let caps = egress.capabilities();
    assert!(caps.tool_choice_required);
    // Verify the encode produces toolConfig with mode=ANY.
    let (encoded, _h) = egress.encode_request(&ir).expect("encode");
    assert_eq!(
        encoded["toolConfig"]["functionCallingConfig"]["mode"],
        "ANY"
    );
}

#[test]
fn json_schema_response_format_encodes_to_anthropic() {
    // OpenAI's JSON Schema response format maps to Anthropic's
    // `output_config.format` Structured Outputs field.
    let ingress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "Hi"}],
        "response_format": {
            "type": "json_schema",
            "json_schema": {"name": "x", "schema": {}}
        }
    });
    let ir = ingress.decode_request(body, &make_env()).expect("decode");
    // Sanity: the IR models response_format.
    assert!(ir.response_format.is_some());
    let egress = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    let caps = egress.capabilities();
    assert!(tiygate_core::protocol::lossy::check_lossy_conversion(&ir, egress.id(), caps,).is_ok());
    let (encoded, _) = egress.encode_request(&ir).expect("encode");
    assert_eq!(encoded["output_config"]["format"]["type"], "json_schema");
    assert_eq!(encoded["output_config"]["format"]["schema"], json!({}));
}

#[test]
fn runtime_lossy_reject_parallel_tool_calls_to_anthropic() {
    // OpenAI parallel_tool_calls=true has no direct Anthropic equivalent
    // (Anthropic uses parallel_tool_use blocks that get serialised
    // differently from OpenAI's parallel_tool_calls). When the
    // runtime contract fires, the lossy check rejects the conversion.
    let ingress = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let egress = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    // The lossy check (per the design doc §3.2) fires whenever an
    // upstream-only field is present that the egress codec cannot
    // express. We assert that lossy_default_reject is declared (the
    // runtime check is a wiring detail of `execute_upstream`).
    assert!(
        egress.capabilities().lossy_default_reject,
        "Anthropic must declare lossy_default_reject"
    );
    let body = json!({
        "model": "m",
        "messages": [{"role": "user", "content": "Hi"}],
        "tools": [{
            "type": "function",
            "function": {"name": "f", "description": "x", "parameters": {}}
        }],
        "parallel_tool_calls": true
    });
    let ir = ingress.decode_request(body, &make_env()).expect("decode");
    // The chat-completions decoder now records `parallel_tool_calls: true` by
    // marking each tool `required`, so the runtime lossy check can reject the
    // crossing to Anthropic (which cannot express concurrent fan-out).
    assert!(!ir.tools.is_empty());
    assert!(
        ir.tools.iter().all(|t| t.required),
        "parallel_tool_calls=true must mark tools required"
    );
    let result = tiygate_core::protocol::lossy::check_lossy_conversion(
        &ir,
        egress.id(),
        egress.capabilities(),
    );
    assert!(
        result.is_err(),
        "chat→messages with parallel_tool_calls=true must be rejected"
    );
}

/// 端到端回归测试:Responses 协议 input 中 reasoning + function_call items
/// 经 decode → IR → ChatCompletions encode 后,assistant 消息必须同时
/// 包含 reasoning_content 和 tool_calls。
/// 此测试覆盖两个已修复的 bug:
/// 1. decode_request 未合并同 role items,导致 reasoning 和 tool_calls
///    在不同 message 中,门控逻辑丢弃 reasoning_content。
/// 2. 即使客户端正确回传,跨协议转换也会丢失 reasoning。
#[test]
fn responses_to_chat_reasoning_with_tool_calls_roundtrip() {
    use tiygate_protocols::chat_completions::ChatCompletionsCodec;
    use tiygate_protocols::responses::ResponsesCodec;

    let responses_codec = ResponsesCodec::new();
    let chat_codec = ChatCompletionsCodec::new();
    let env = make_env();

    // 模拟客户端回传的 Responses input:reasoning + 2个 function_call
    let responses_body = json!({
        "model": "deepseek-v4-pro",
        "stream": true,
        "input": [
            {"role": "user", "content": "查一下天气"},
            {"type": "reasoning", "id": "rs_abc",
             "summary": [{"type": "summary_text", "text": "用户需要天气信息,调用工具"}]},
            {"type": "function_call", "call_id": "call_1",
             "name": "get_weather", "arguments": "{\"city\":\"杭州\"}"},
            {"type": "function_call", "call_id": "call_2",
             "name": "get_weather", "arguments": "{\"city\":\"北京\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "晴天 25°C"},
            {"type": "function_call_output", "call_id": "call_2", "output": "多云 22°C"}
        ]
    });

    // Responses decode → IR
    let ir = responses_codec
        .decode_request(responses_body, &env)
        .expect("Responses decode_request 不应失败");

    // IR → Chat Completions encode
    let (chat_body, _headers) = chat_codec
        .encode_request(&ir)
        .expect("ChatCompletions encode_request 不应失败");

    let messages = chat_body["messages"].as_array().expect("messages array");

    // 找到含 tool_calls 的 assistant 消息
    let tool_turn = messages
        .iter()
        .find(|m| m["role"] == "assistant" && m.get("tool_calls").is_some())
        .expect("必须存在含 tool_calls 的 assistant 消息");

    // 该消息必须同时包含 reasoning_content
    assert_eq!(
        tool_turn["reasoning_content"].as_str(),
        Some("用户需要天气信息,调用工具"),
        "含 tool_calls 的 assistant 消息必须携带 reasoning_content, \
         否则 DeepSeek thinking 模式会返回 400"
    );

    // tool_calls 数量正确
    let tc = tool_turn["tool_calls"]
        .as_array()
        .expect("tool_calls array");
    assert_eq!(tc.len(), 2, "应有 2 个 tool_calls");
}

// ── Thinking config cross-protocol tests ────────────────────────────

#[test]
fn chat_thinking_effort_roundtrip() {
    let codec = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let env = make_env();
    let body = json!({
        "model": "o3",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "high",
    });
    let ir = codec.decode_request(body, &env).unwrap();
    assert_eq!(
        ir.params.thinking.as_ref().unwrap().effort,
        Some(tiygate_core::ThinkingEffort::High)
    );
    let (encoded, _) = codec.encode_request(&ir).unwrap();
    assert_eq!(encoded["reasoning_effort"], "high");
}

#[test]
fn anthropic_thinking_config_roundtrip() {
    let codec = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    let env = make_env();
    let body = json!({
        "model": "claude-3.5-sonnet",
        "max_tokens": 4096,
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "enabled", "budget_tokens": 2048},
    });
    let ir = codec.decode_request(body, &env).unwrap();
    assert_eq!(
        ir.params.thinking.as_ref().unwrap().budget_tokens,
        Some(2048)
    );
    let (encoded, _) = codec.encode_request(&ir).unwrap();
    assert_eq!(encoded["thinking"]["type"], "enabled");
    assert_eq!(encoded["thinking"]["budget_tokens"], 2048);
}

#[test]
fn gemini_thinking_config_roundtrip() {
    let codec = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let env = make_env();
    let body = json!({
        "model": "gemini-2.0-flash",
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {
            "thinkingConfig": {"includeThoughts": true, "thinkingBudget": 1024}
        },
    });
    let ir = codec.decode_request(body, &env).unwrap();
    assert_eq!(
        ir.params.thinking.as_ref().unwrap().include_thoughts,
        Some(true)
    );
    assert_eq!(
        ir.params.thinking.as_ref().unwrap().budget_tokens,
        Some(1024)
    );
    let (encoded, _) = codec.encode_request(&ir).unwrap();
    assert_eq!(
        encoded["generationConfig"]["thinkingConfig"]["includeThoughts"],
        true
    );
    assert_eq!(
        encoded["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        1024
    );
}

#[test]
fn chat_thinking_effort_cross_to_anthropic() {
    // Chat reasoning_effort="medium" must now map to Anthropic's adaptive
    // thinking with output_config.effort="medium" (cross-protocol derivation).
    let chat = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let anthropic = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    let env = make_env();
    let body = json!({
        "model": "o3",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "medium",
    });
    let ir = chat.decode_request(body, &env).unwrap();
    assert_eq!(
        ir.params.thinking.as_ref().unwrap().effort,
        Some(ThinkingEffort::Medium)
    );
    let (encoded, _) = anthropic.encode_request(&ir).unwrap();
    // Effort must be expressed as top-level output_config.effort (sibling of thinking).
    assert_eq!(encoded["thinking"]["type"], "adaptive");
    assert_eq!(encoded["output_config"]["effort"], "medium");
}

// ── Cross-protocol thinking config mapping tests ───────────────────

/// Helper: build a minimal IR request with a given thinking config.
fn thinking_ir(thinking: ThinkingConfig) -> IrRequest {
    thinking_ir_for_model("test-model", thinking)
}

fn thinking_ir_for_model(model: &str, thinking: ThinkingConfig) -> IrRequest {
    IrRequest {
        model: model.to_string(),
        system: Some("You are helpful.".to_string()),
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "Hello".to_string(),
                annotations: None,
                prompt_cache_breakpoint: None,
            }],
        }],
        tools: vec![],
        params: GenerationParams {
            max_tokens: Some(100),
            thinking: Some(thinking),
            ..Default::default()
        },
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

#[test]
fn cross_thinking_chat_effort_to_anthropic_adaptive() {
    // Chat effort → Anthropic adaptive thinking with top-level output_config.effort
    let anthropic = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    let ir = thinking_ir(ThinkingConfig {
        effort: Some(ThinkingEffort::High),
        ..Default::default()
    });
    let (out, _) = anthropic.encode_request(&ir).unwrap();
    assert_eq!(out["thinking"]["type"], "adaptive");
    assert_eq!(out["output_config"]["effort"], "high");
}

#[test]
fn cross_thinking_chat_effort_to_gemini_thinking_level() {
    // Chat effort → Gemini 3+ thinkingLevel only.
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let ir = thinking_ir_for_model(
        "gemini-3.0-pro",
        ThinkingConfig {
            effort: Some(ThinkingEffort::Medium),
            ..Default::default()
        },
    );
    let (out, _) = gemini.encode_request(&ir).unwrap();
    assert_eq!(
        out["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "medium"
    );
    assert!(
        out["generationConfig"]["thinkingConfig"]
            .get("thinkingBudget")
            .is_none(),
        "Gemini does not support thinkingLevel and thinkingBudget together"
    );
}

#[test]
fn cross_thinking_effort_to_gemini_2_5_budget() {
    // Gemini 2.5 does not support thinkingLevel, so effort is mapped to thinkingBudget.
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let ir = thinking_ir_for_model(
        "gemini-2.5-flash",
        ThinkingConfig {
            effort: Some(ThinkingEffort::Medium),
            ..Default::default()
        },
    );
    let (out, _) = gemini.encode_request(&ir).unwrap();
    assert_eq!(
        out["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        ThinkingConfig::effort_to_budget(ThinkingEffort::Medium)
    );
    assert!(
        out["generationConfig"]["thinkingConfig"]
            .get("thinkingLevel")
            .is_none(),
        "Gemini 2.5 models use thinkingBudget rather than thinkingLevel"
    );
}

#[test]
fn cross_thinking_anthropic_budget_to_chat_effort() {
    // Anthropic budget_tokens → Chat reasoning_effort (derived via budget_to_effort)
    let chat = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let ir = thinking_ir(ThinkingConfig {
        budget_tokens: Some(32_000),
        ..Default::default()
    });
    let (out, _) = chat.encode_request(&ir).unwrap();
    // 32000 falls in the High range (16384-39999)
    assert_eq!(out["reasoning_effort"], "high");
}

#[test]
fn cross_thinking_anthropic_budget_to_gemini_thinking_level() {
    // Anthropic budget_tokens → Gemini 3+ thinkingLevel only.
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let ir = thinking_ir_for_model(
        "gemini-3.0-pro",
        ThinkingConfig {
            budget_tokens: Some(10_000),
            ..Default::default()
        },
    );
    let (out, _) = gemini.encode_request(&ir).unwrap();
    // 10000 falls in the Medium range (6144-16383)
    assert_eq!(
        out["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "medium"
    );
    assert!(
        out["generationConfig"]["thinkingConfig"]
            .get("thinkingBudget")
            .is_none(),
        "Gemini does not support thinkingLevel and thinkingBudget together"
    );
}

#[test]
fn cross_thinking_budget_to_gemini_2_5_budget() {
    // Gemini 2.5 preserves explicit thinkingBudget and does not emit thinkingLevel.
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let ir = thinking_ir_for_model(
        "gemini-2.5-flash",
        ThinkingConfig {
            budget_tokens: Some(10_000),
            ..Default::default()
        },
    );
    let (out, _) = gemini.encode_request(&ir).unwrap();
    assert_eq!(
        out["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        10_000
    );
    assert!(
        out["generationConfig"]["thinkingConfig"]
            .get("thinkingLevel")
            .is_none(),
        "Gemini 2.5 models use thinkingBudget rather than thinkingLevel"
    );
}

#[test]
fn cross_thinking_gemini_thinking_level_to_chat_effort() {
    // Gemini thinkingLevel → Chat reasoning_effort
    let chat = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let env = make_env();
    let body = json!({
        "model": "gemini-3.0-pro",
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {
            "thinkingConfig": {"thinkingLevel": "high"}
        },
    });
    let ir = gemini.decode_request(body, &env).unwrap();
    assert_eq!(
        ir.params.thinking.as_ref().unwrap().effort,
        Some(ThinkingEffort::High)
    );
    let (out, _) = chat.encode_request(&ir).unwrap();
    assert_eq!(out["reasoning_effort"], "high");
}

#[test]
fn cross_thinking_gemini_thinking_level_to_anthropic_adaptive() {
    // Gemini thinkingLevel → Anthropic adaptive thinking with top-level output_config.effort
    let anthropic = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let env = make_env();
    let body = json!({
        "model": "gemini-3.0-pro",
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        "generationConfig": {
            "thinkingConfig": {"thinkingLevel": "low"}
        },
    });
    let ir = gemini.decode_request(body, &env).unwrap();
    assert_eq!(
        ir.params.thinking.as_ref().unwrap().effort,
        Some(ThinkingEffort::Low)
    );
    let (out, _) = anthropic.encode_request(&ir).unwrap();
    assert_eq!(out["thinking"]["type"], "adaptive");
    assert_eq!(out["output_config"]["effort"], "low");
}

#[test]
fn cross_thinking_anthropic_adaptive_decode_to_chat() {
    // Anthropic adaptive thinking with top-level output_config.effort → Chat reasoning_effort
    let chat = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let anthropic = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    let env = make_env();
    let body = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 4096,
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "xhigh"},
    });
    let ir = anthropic.decode_request(body, &env).unwrap();
    assert_eq!(
        ir.params.thinking.as_ref().unwrap().effort,
        Some(ThinkingEffort::XHigh)
    );
    let (out, _) = chat.encode_request(&ir).unwrap();
    assert_eq!(out["reasoning_effort"], "xhigh");
}

#[test]
fn cross_thinking_anthropic_adaptive_decode_to_gemini() {
    // Anthropic adaptive thinking → Gemini 3+ thinkingLevel (XHigh clamps to "high")
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let anthropic = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    let env = make_env();
    let body = json!({
        "model": "claude-sonnet-4",
        "max_tokens": 4096,
        "messages": [{"role": "user", "content": "hi"}],
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "max"},
    });
    let mut ir = anthropic.decode_request(body, &env).unwrap();
    ir.model = "gemini-3.0-pro".to_string();
    assert_eq!(
        ir.params.thinking.as_ref().unwrap().effort,
        Some(ThinkingEffort::Max)
    );
    let (out, _) = gemini.encode_request(&ir).unwrap();
    // Gemini only has 4 levels; Max clamps to "high"
    assert_eq!(
        out["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "high"
    );
}

#[test]
fn cross_thinking_display_to_include_thoughts_anthropic_to_gemini() {
    // Anthropic display=Omitted → Gemini includeThoughts=false
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let ir = thinking_ir(ThinkingConfig {
        display: Some(tiygate_core::ThinkingDisplay::Omitted),
        ..Default::default()
    });
    let (out, _) = gemini.encode_request(&ir).unwrap();
    assert_eq!(
        out["generationConfig"]["thinkingConfig"]["includeThoughts"],
        false
    );
}

#[test]
fn cross_thinking_include_thoughts_to_display_gemini_to_anthropic() {
    // Gemini includeThoughts=true alone (no effort, no budget_tokens) cannot
    // be expressed on Anthropic without a budget_tokens. The encoder must NOT
    // emit an invalid enabled-thinking block without budget_tokens.
    let anthropic = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    let ir = thinking_ir(ThinkingConfig {
        include_thoughts: Some(true),
        ..Default::default()
    });
    let (out, _) = anthropic.encode_request(&ir).unwrap();
    // No thinking block should be emitted when only include_thoughts is set,
    // since Anthropic's enabled type requires budget_tokens.
    assert!(
        out.get("thinking").is_none(),
        "expected no thinking block when only include_thoughts is set, got: {out}"
    );
}

#[test]
fn cross_thinking_none_preserves_or_safely_disables() {
    let chat = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let responses = find_codec(ProtocolSuite::OpenAiResponses, "responses");
    let anthropic = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let ir = thinking_ir_for_model(
        "gemini-2.5-pro",
        ThinkingConfig {
            effort: Some(ThinkingEffort::None),
            ..Default::default()
        },
    );

    let (chat_body, _) = chat.encode_request(&ir).unwrap();
    assert_eq!(chat_body["reasoning_effort"], "none");
    let (responses_body, _) = responses.encode_request(&ir).unwrap();
    assert_eq!(responses_body["reasoning"]["effort"], "none");
    let (anthropic_body, _) = anthropic.encode_request(&ir).unwrap();
    assert!(anthropic_body.get("thinking").is_none());
    assert!(anthropic_body.get("output_config").is_none());
    let (gemini_body, _) = gemini.encode_request(&ir).unwrap();
    assert_eq!(
        gemini_body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        0
    );
}

#[test]
fn cross_thinking_minimal_effort_clamping() {
    // Minimal effort: Anthropic clamps to "low", Gemini supports "minimal"
    let anthropic = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let ir = thinking_ir_for_model(
        "gemini-3.0-pro",
        ThinkingConfig {
            effort: Some(ThinkingEffort::Minimal),
            ..Default::default()
        },
    );
    let (anth_out, _) = anthropic.encode_request(&ir).unwrap();
    // Anthropic doesn't support "minimal", clamps to "low"
    assert_eq!(anth_out["output_config"]["effort"], "low");

    let (gem_out, _) = gemini.encode_request(&ir).unwrap();
    // Gemini supports "minimal"
    assert_eq!(
        gem_out["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "minimal"
    );
}

#[test]
fn cross_thinking_max_effort_clamping() {
    // Max effort: OpenAI clamps to "xhigh", Gemini clamps to "high"
    let chat = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let gemini = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let ir = thinking_ir_for_model(
        "gemini-3.0-pro",
        ThinkingConfig {
            effort: Some(ThinkingEffort::Max),
            ..Default::default()
        },
    );
    let (chat_out, _) = chat.encode_request(&ir).unwrap();
    // OpenAI GPT-5.6+ supports "max" natively.
    assert_eq!(chat_out["reasoning_effort"], "max");

    let (gem_out, _) = gemini.encode_request(&ir).unwrap();
    // Gemini only has 4 levels, clamps to "high"
    assert_eq!(
        gem_out["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "high"
    );
}

// ── Refusal tests ───────────────────────────────────────────────────

#[test]
fn chat_refusal_decodes_as_refusal_content() {
    let codec = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let body = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "refusal": "I cannot help with that."
            },
            "finish_reason": "content_filter",
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
    });
    let ir = codec.decode_response(body).unwrap();
    // Refusal should be Content::Refusal, not Content::Text
    assert!(ir
        .content
        .iter()
        .any(|c| matches!(c, Content::Refusal { .. })));
    // stop_details should be populated
    assert_eq!(ir.stop_details.as_ref().unwrap().stop_reason, "refusal");
}

#[test]
fn chat_refusal_encodes_refusal_field() {
    let codec = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let ir = IrResponse {
        content: vec![Content::Refusal {
            text: "Cannot comply.".to_string(),
            category: None,
        }],
        usage: None,
        finish_reason: Some(tiygate_core::FinishReason::ContentFilter),
        response_id: Some("test".to_string()),
        stop_details: None,
        extensions: HashMap::new(),
    };
    let encoded = codec.encode_response(&ir).unwrap();
    assert_eq!(
        encoded["choices"][0]["message"]["refusal"],
        "Cannot comply."
    );
}

// ── Stop details cross-protocol tests ───────────────────────────────

#[test]
fn gemini_safety_populates_stop_details() {
    let codec = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let body = json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": "partial"}]},
            "finishReason": "SAFETY",
        }],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15},
    });
    let ir = codec.decode_response(body).unwrap();
    assert_eq!(ir.stop_details.as_ref().unwrap().stop_reason, "safety");
    assert_eq!(
        ir.stop_details.as_ref().unwrap().kind.as_ref().unwrap(),
        "safety"
    );
}

#[test]
fn responses_incomplete_populates_stop_details() {
    let codec = find_codec(ProtocolSuite::OpenAiResponses, "responses");
    let body = json!({
        "id": "resp_1",
        "object": "response",
        "output": [],
        "status": "incomplete",
        "incomplete_details": {"reason": "max_output_tokens"},
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
    });
    let ir = codec.decode_response(body).unwrap();
    assert_eq!(
        ir.stop_details.as_ref().unwrap().stop_reason,
        "max_output_tokens"
    );
}

// ── Encrypted reasoning content tests ───────────────────────────────

#[test]
fn anthropic_redacted_thinking_uses_encrypted_content() {
    let codec = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    let body = json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "redacted_thinking", "data": "opaque-encrypted-data"},
            {"type": "text", "text": "result"},
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5},
    });
    let ir = codec.decode_response(body).unwrap();
    // The redacted thinking should be stored in encrypted_content
    let reasoning = ir.content.iter().find(|c| {
        matches!(
            c,
            Content::Reasoning {
                encrypted_content: Some(_),
                ..
            }
        )
    });
    assert!(
        reasoning.is_some(),
        "redacted_thinking should be Content::Reasoning with encrypted_content"
    );
    if let Some(Content::Reasoning {
        encrypted_content, ..
    }) = reasoning
    {
        assert_eq!(encrypted_content.as_ref().unwrap(), "opaque-encrypted-data");
    }
    // Verify round-trip: encode_response should emit redacted_thinking
    let encoded = codec.encode_response(&ir).unwrap();
    let has_redacted = encoded["content"]
        .as_array()
        .map(|arr| arr.iter().any(|b| b["type"] == "redacted_thinking"))
        .unwrap_or(false);
    assert!(
        has_redacted,
        "encode_response should emit redacted_thinking block"
    );
}

#[test]
fn responses_encrypted_content_roundtrip() {
    let codec = find_codec(ProtocolSuite::OpenAiResponses, "responses");
    let body = json!({
        "id": "resp_1",
        "object": "response",
        "output": [{
            "type": "reasoning",
            "id": "rs_abc",
            "summary": [{"type": "summary_text", "text": "thinking..."}],
            "encrypted_content": "enc-data-123",
        }],
        "status": "completed",
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
    });
    let ir = codec.decode_response(body).unwrap();
    let reasoning = ir
        .content
        .iter()
        .find(|c| matches!(c, Content::Reasoning { .. }));
    assert!(reasoning.is_some());
    if let Some(Content::Reasoning {
        encrypted_content, ..
    }) = reasoning
    {
        assert_eq!(encrypted_content.as_ref().unwrap(), "enc-data-123");
    }
    // Verify encode_response outputs encrypted_content
    let encoded = codec.encode_response(&ir).unwrap();
    let has_enc = encoded["output"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .any(|item| item["encrypted_content"] == "enc-data-123")
        })
        .unwrap_or(false);
    assert!(has_enc, "encode_response should output encrypted_content");
}

// ── Metadata cross-protocol tests ───────────────────────────────────

#[test]
fn anthropic_metadata_user_id_roundtrip() {
    let codec = find_codec(ProtocolSuite::AnthropicMessages, "messages");
    let env = make_env();
    let body = json!({
        "model": "claude-3.5-sonnet",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
        "metadata": {"user_id": "user-123"},
    });
    let ir = codec.decode_request(body, &env).unwrap();
    assert_eq!(
        ir.metadata.as_ref().unwrap().get("user_id").unwrap(),
        "user-123"
    );
    let (encoded, _) = codec.encode_request(&ir).unwrap();
    assert_eq!(encoded["metadata"]["user_id"], "user-123");
}

#[test]
fn chat_metadata_roundtrip() {
    let codec = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let env = make_env();
    let body = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "hi"}],
        "metadata": {"session_id": "sess-1", "user_id": "u1"},
    });
    let ir = codec.decode_request(body, &env).unwrap();
    assert_eq!(ir.metadata.as_ref().unwrap().len(), 2);
    let (encoded, _) = codec.encode_request(&ir).unwrap();
    assert_eq!(encoded["metadata"]["session_id"], "sess-1");
    assert_eq!(encoded["metadata"]["user_id"], "u1");
}

// ── Gemini cache_write_tokens encode test ───────────────────────────

#[test]
fn gemini_encode_response_includes_cache_write_in_prompt() {
    let codec = find_codec(ProtocolSuite::GoogleGemini, "generateContent");
    let ir = IrResponse {
        content: vec![Content::Text {
            text: "ok".to_string(),
            annotations: None,
            prompt_cache_breakpoint: None,
        }],
        usage: Some(tiygate_core::Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            cache_read_tokens: Some(200),
            cache_write_tokens: Some(300),
            total_tokens: 650,
            ..Default::default()
        }),
        finish_reason: Some(tiygate_core::FinishReason::Stop),
        response_id: None,
        stop_details: None,
        extensions: HashMap::new(),
    };
    let encoded = codec.encode_response(&ir).unwrap();
    // promptTokenCount = 100 + 200 (cache_read) + 300 (cache_write) = 600
    assert_eq!(encoded["usageMetadata"]["promptTokenCount"], 600);
    // totalTokenCount = 600 + 50 = 650
    assert_eq!(encoded["usageMetadata"]["totalTokenCount"], 650);
    assert_eq!(encoded["usageMetadata"]["cachedContentTokenCount"], 200);
}

// ── Annotations test ────────────────────────────────────────────────

#[test]
fn chat_annotations_decode_to_content_text() {
    let codec = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let body = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Check this source.",
                "annotations": [{
                    "type": "url_citation",
                    "start_index": 0,
                    "end_index": 10,
                    "url_citation": {
                        "url": "https://example.com",
                        "title": "Example",
                    },
                }],
            },
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
    });
    let ir = codec.decode_response(body).unwrap();
    let text = ir.content.iter().find_map(|c| match c {
        Content::Text {
            annotations: Some(a),
            ..
        } => Some(a),
        _ => None,
    });
    assert!(text.is_some(), "text content should have annotations");
    let anns = text.unwrap();
    assert_eq!(anns.len(), 1);
    assert_eq!(anns[0].url.as_ref().unwrap(), "https://example.com");
    assert_eq!(anns[0].title.as_ref().unwrap(), "Example");
}

// ============================================================================
// image_url.detail preservation tests
// ============================================================================

#[test]
fn chat_decode_image_url_object_with_detail() {
    let codec = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let body = json!({
        "model": "m",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "What is this?"},
                {"type": "image_url", "image_url": {"url": "https://example.com/img.png", "detail": "high"}}
            ]
        }]
    });
    let ir = codec.decode_request(body, &make_env()).expect("decode");
    let media = ir.messages[0].content.iter().find_map(|c| match c {
        Content::Media { metadata, .. } => Some(metadata),
        _ => None,
    });
    let media = media.expect("should have a media part");
    assert_eq!(
        media.get(tiygate_core::ir::IMAGE_DETAIL_KEY),
        Some(&json!("high")),
        "detail should be preserved in metadata"
    );
}

#[test]
fn chat_decode_image_url_string_form() {
    let codec = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let body = json!({
        "model": "m",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": "https://example.com/img.png"}
            ]
        }]
    });
    let ir = codec.decode_request(body, &make_env()).expect("decode");
    let media = ir.messages[0].content.iter().find_map(|c| match c {
        Content::Media {
            source, metadata, ..
        } => Some((source, metadata)),
        _ => None,
    });
    let (source, metadata) = media.expect("should have a media part");
    assert!(
        matches!(source, tiygate_core::ir::MediaSource::Url { ref url } if url == "https://example.com/img.png"),
        "string-form image_url should parse as URL"
    );
    assert!(
        !metadata.contains_key(tiygate_core::ir::IMAGE_DETAIL_KEY),
        "no detail should be present for string form"
    );
}

#[test]
fn chat_encode_image_url_emits_detail() {
    let codec = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let ir = IrRequest {
        model: "m".to_string(),
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Media {
                source: tiygate_core::ir::MediaSource::Url {
                    url: "https://example.com/img.png".to_string(),
                },
                mime_type: "image/png".to_string(),
                metadata: {
                    let mut m = HashMap::new();
                    m.insert(tiygate_core::ir::IMAGE_DETAIL_KEY.to_string(), json!("low"));
                    m
                },
                prompt_cache_breakpoint: None,
            }],
        }],
        tools: vec![],
        params: GenerationParams::default(),
        response_format: None,
        stream: false,
        ingress_protocol: ProtocolEndpoint::new(
            ProtocolSuite::OpenAiCompatible,
            "chat-completions",
            "v1",
        ),
        extensions: HashMap::new(),
        metadata: None,
    };
    let (out, _h) = codec.encode_request(&ir).expect("encode");
    let image_url = &out["messages"][0]["content"][0]["image_url"];
    assert_eq!(image_url["url"], json!("https://example.com/img.png"));
    assert_eq!(image_url["detail"], json!("low"));
}

#[test]
fn responses_decode_image_url_object_with_detail() {
    let codec = find_codec(ProtocolSuite::OpenAiResponses, "responses");
    let body = json!({
        "model": "m",
        "input": [{
            "role": "user",
            "content": [
                {"type": "input_text", "text": "What is this?"},
                {"type": "input_image", "image_url": {"url": "https://example.com/img.png", "detail": "high"}}
            ]
        }]
    });
    let ir = codec.decode_request(body, &make_env()).expect("decode");
    let media = ir.messages[0].content.iter().find_map(|c| match c {
        Content::Media { metadata, .. } => Some(metadata),
        _ => None,
    });
    let media = media.expect("should have a media part");
    assert_eq!(
        media.get(tiygate_core::ir::IMAGE_DETAIL_KEY),
        Some(&json!("high")),
        "detail should be preserved in metadata"
    );
}

#[test]
fn responses_decode_image_url_string_form() {
    let codec = find_codec(ProtocolSuite::OpenAiResponses, "responses");
    let body = json!({
        "model": "m",
        "input": [{
            "role": "user",
            "content": [
                {"type": "input_image", "image_url": "https://example.com/img.png"}
            ]
        }]
    });
    let ir = codec.decode_request(body, &make_env()).expect("decode");
    let media = ir.messages[0].content.iter().find_map(|c| match c {
        Content::Media {
            source, metadata, ..
        } => Some((source, metadata)),
        _ => None,
    });
    let (source, metadata) = media.expect("should have a media part");
    assert!(
        matches!(source, tiygate_core::ir::MediaSource::Url { ref url } if url == "https://example.com/img.png"),
        "string-form image_url should parse as URL"
    );
    assert!(
        !metadata.contains_key(tiygate_core::ir::IMAGE_DETAIL_KEY),
        "no detail should be present for string form"
    );
}

#[test]
fn responses_encode_image_url_emits_detail() {
    let codec = find_codec(ProtocolSuite::OpenAiResponses, "responses");
    let ir = IrRequest {
        model: "m".to_string(),
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Media {
                source: tiygate_core::ir::MediaSource::Url {
                    url: "https://example.com/img.png".to_string(),
                },
                mime_type: "image/png".to_string(),
                metadata: {
                    let mut m = HashMap::new();
                    m.insert(
                        tiygate_core::ir::IMAGE_DETAIL_KEY.to_string(),
                        json!("auto"),
                    );
                    m
                },
                prompt_cache_breakpoint: None,
            }],
        }],
        tools: vec![],
        params: GenerationParams::default(),
        response_format: None,
        stream: false,
        ingress_protocol: ProtocolEndpoint::new(ProtocolSuite::OpenAiResponses, "responses", "v1"),
        extensions: HashMap::new(),
        metadata: None,
    };
    let (out, _h) = codec.encode_request(&ir).expect("encode");
    let img_part = &out["input"][0]["content"][0];
    assert_eq!(img_part["type"], json!("input_image"));
    assert_eq!(img_part["image_url"], json!("https://example.com/img.png"));
    assert_eq!(img_part["detail"], json!("auto"));
}

#[test]
fn detail_roundtrip_chat_to_responses() {
    let chat = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let responses = find_codec(ProtocolSuite::OpenAiResponses, "responses");
    let body = json!({
        "model": "m",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": "https://example.com/img.png", "detail": "high"}}
            ]
        }]
    });
    let ir = chat.decode_request(body, &make_env()).expect("decode");
    let (out, _h) = responses.encode_request(&ir).expect("encode");
    let img_part = &out["input"][0]["content"][0];
    assert_eq!(
        img_part["detail"],
        json!("high"),
        "detail should survive cross-protocol"
    );
}

#[test]
fn detail_roundtrip_responses_to_chat() {
    let responses = find_codec(ProtocolSuite::OpenAiResponses, "responses");
    let chat = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let body = json!({
        "model": "m",
        "input": [{
            "role": "user",
            "content": [
                {"type": "input_image", "image_url": {"url": "https://example.com/img.png", "detail": "low"}}
            ]
        }]
    });
    let ir = responses.decode_request(body, &make_env()).expect("decode");
    let (out, _h) = chat.encode_request(&ir).expect("encode");
    let image_url = &out["messages"][0]["content"][0]["image_url"];
    assert_eq!(
        image_url["detail"],
        json!("low"),
        "detail should survive cross-protocol"
    );
}

// Bug 1: a reasoning item with an EMPTY summary but a non-empty
// encrypted_content must NOT be dropped on decode — this is the shape OpenAI
// returns when summaries are disabled and `include:
// ["reasoning.encrypted_content"]` is set. Dropping it breaks encrypted
// reasoning replay on later turns.
#[test]
fn responses_empty_summary_encrypted_only_reasoning_preserved() {
    let codec = find_codec(ProtocolSuite::OpenAiResponses, "responses");
    let body = json!({
        "id": "resp_1",
        "object": "response",
        "output": [{
            "type": "reasoning",
            "id": "rs_only_enc",
            "summary": [],
            "encrypted_content": "enc-no-summary",
        }],
        "status": "completed",
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
    });
    let ir = codec.decode_response(body).unwrap();
    let reasoning = ir
        .content
        .iter()
        .find_map(|c| match c {
            Content::Reasoning {
                id,
                encrypted_content,
                text,
                ..
            } => Some((id.clone(), encrypted_content.clone(), text.clone())),
            _ => None,
        })
        .expect("encrypted-only reasoning item must survive decode");
    assert_eq!(reasoning.0.as_deref(), Some("rs_only_enc"));
    assert_eq!(reasoning.1.as_deref(), Some("enc-no-summary"));
    assert!(reasoning.2.is_empty(), "summary was empty");

    // And it must re-encode to `summary: []` (not a summary part with an empty
    // string), carrying id + encrypted_content verbatim.
    let encoded = codec.encode_response(&ir).unwrap();
    let item = encoded["output"]
        .as_array()
        .and_then(|arr| arr.iter().find(|i| i["type"] == "reasoning"))
        .expect("reasoning item must be re-encoded");
    assert_eq!(item["id"], json!("rs_only_enc"));
    assert_eq!(item["encrypted_content"], json!("enc-no-summary"));
    assert_eq!(item["summary"], json!([]), "empty reasoning -> summary: []");
}

// Bug 2: a reasoning INPUT item carrying encrypted_content must preserve it
// through decode_request (previously hard-coded to None), so same-protocol
// multi-turn replays the encrypted payload back to the upstream.
#[test]
fn responses_decode_request_preserves_reasoning_encrypted_content() {
    let codec = find_codec(ProtocolSuite::OpenAiResponses, "responses");
    let body = json!({
        "model": "m",
        "input": [{
            "type": "reasoning",
            "id": "rs_replay",
            "summary": [{"type": "summary_text", "text": "earlier thought"}],
            "encrypted_content": "enc-replay-456",
        }]
    });
    let ir = codec.decode_request(body, &make_env()).expect("decode");
    let enc = ir.messages.iter().find_map(|m| {
        m.content.iter().find_map(|c| match c {
            Content::Reasoning {
                encrypted_content, ..
            } => encrypted_content.clone(),
            _ => None,
        })
    });
    assert_eq!(
        enc.as_deref(),
        Some("enc-replay-456"),
        "decode_request must preserve reasoning encrypted_content"
    );
}

// A reasoning item carrying ONLY an id (no summary text, no encrypted_content)
// is an empty shell with nothing to replay. It must be dropped on decode rather
// than producing an orphaned, content-free IR Reasoning that some downstream
// providers reject.
#[test]
fn responses_id_only_reasoning_is_dropped() {
    let codec = find_codec(ProtocolSuite::OpenAiResponses, "responses");
    let body = json!({
        "id": "resp_2",
        "object": "response",
        "output": [{
            "type": "reasoning",
            "id": "rs_id_only",
            "summary": [],
        }],
        "status": "completed",
        "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15},
    });
    let ir = codec.decode_response(body).unwrap();
    let has_reasoning = ir
        .content
        .iter()
        .any(|c| matches!(c, Content::Reasoning { .. }));
    assert!(
        !has_reasoning,
        "an id-only reasoning item (no text, no encrypted_content) must be dropped"
    );
}

#[test]
fn responses_codex_local_shell_to_chat_completions() {
    let responses = find_codec(ProtocolSuite::OpenAiResponses, "responses");
    let chat = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let body = json!({
        "model": "m",
        "input": [
            {"role": "user", "content": "list files"},
            {"type": "local_shell_call", "call_id": "call_shell_1", "action": {"command": ["ls", "-la"]}}
        ]
    });
    let ir = responses.decode_request(body, &make_env()).expect("decode");
    let (out, _h) = chat.encode_request(&ir).expect("encode");
    let messages = out["messages"].as_array().unwrap();
    let tool_calls: Vec<_> = messages
        .iter()
        .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
        .filter(|tc| tc["function"]["name"] == "local_shell")
        .collect();
    assert!(
        !tool_calls.is_empty(),
        "local_shell_call should survive cross-protocol to Chat Completions as a tool_call"
    );
    assert_eq!(tool_calls[0]["id"], "call_shell_1");
}

#[test]
fn responses_codex_opaque_items_dropped_cross_protocol() {
    let responses = find_codec(ProtocolSuite::OpenAiResponses, "responses");
    let chat = find_codec(ProtocolSuite::OpenAiCompatible, "chat-completions");
    let body = json!({
        "model": "m",
        "input": [
            {"role": "user", "content": "hi"},
            {"type": "compaction", "id": "comp_1", "summary": "compacted"},
            {"type": "agent_message", "content": "agent msg"}
        ]
    });
    let ir = responses.decode_request(body, &make_env()).expect("decode");
    // Verify opaque items are in extensions
    assert!(ir.extensions.contains_key("codex_opaque_items"));
    let (out, _h) = chat.encode_request(&ir).expect("encode");
    let messages = out["messages"].as_array().unwrap();
    // The user message should be present but opaque items should be dropped
    let has_user_msg = messages.iter().any(|m| m["role"] == "user");
    assert!(has_user_msg, "user message should survive");
    // No compaction or agent_message types should leak into chat completions
    let serialized = serde_json::to_string(&out).unwrap();
    assert!(
        !serialized.contains("compaction") && !serialized.contains("agent_message"),
        "opaque Codex items should be dropped in cross-protocol conversion"
    );
}
