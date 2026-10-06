//! Original wire regressions for TG-PROTO-053 through TG-PROTO-061.
//! Assertions inspect native output independently of the target decoder.
use serde_json::{json, Value};
use tiygate_core::{EndpointCodec, FinishReason, RawEnvelope};
use tiygate_protocols::{
    chat_completions::ChatCompletionsCodec, gemini::GeminiCodec, messages::MessagesCodec,
    responses::ResponsesCodec,
};
type TestResult = Result<(), Box<dyn std::error::Error>>;
fn env() -> RawEnvelope {
    RawEnvelope {
        method: "POST".into(),
        path: "/test".into(),
        headers: Default::default(),
        body: None,
        original_body_size: 0,
        timestamp: chrono::Utc::now(),
    }
}
fn guard(ir: &tiygate_core::IrRequest, target: &dyn EndpointCodec) -> bool {
    tiygate_core::protocol::lossy::check_lossy_conversion(ir, target.id(), target.capabilities())
        .is_ok()
}
fn refusal() -> Result<tiygate_core::IrResponse, tiygate_core::Error> {
    ChatCompletionsCodec::new().decode_response(json!({"id":"r","choices":[{"message":{"role":"assistant","content":null,"refusal":"拒绝内容"},"finish_reason":"content_filter"}]}))
}
fn messages_tool_wire(arguments: &str, reason: &str) -> Vec<Value> {
    vec![
        json!({"type":"message_start","message":{"id":"r"}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c","name":"f","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":arguments}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":reason}}),
        json!({"type":"message_stop"}),
    ]
}
#[test]
fn messages_malformed_stream_tool_cannot_complete_successfully() -> TestResult {
    let mut decoder = MessagesCodec::new().stream_decoder();
    let mut encoder = ResponsesCodec::new().stream_encoder();
    let mut wire = Vec::new();
    for frame in messages_tool_wire("{\"x\":", "tool_use") {
        let parts = match decoder.feed(&format!("data: {frame}")) {
            Ok(parts) => parts,
            Err(_) => return Ok(()),
        };
        for part in parts {
            match encoder.encode_part(&part) {
                Ok(bytes) => wire.extend(bytes),
                Err(_) => return Ok(()),
            }
        }
    }
    assert!(!String::from_utf8(wire)?.contains("\"type\":\"response.completed\""));
    Ok(())
}
#[test]
fn messages_length_truncation_remains_incomplete() -> TestResult {
    let mut decoder = MessagesCodec::new().stream_decoder();
    let mut encoder = ResponsesCodec::new().stream_encoder();
    let mut wire = Vec::new();
    for frame in messages_tool_wire("{\"x\":", "max_tokens") {
        for part in decoder.feed(&format!("data: {frame}"))? {
            wire.extend(encoder.encode_part(&part)?);
        }
    }
    let wire = String::from_utf8(wire)?;
    assert!(wire.contains("\"type\":\"response.incomplete\""));
    assert!(!wire.contains("\"type\":\"response.completed\""));
    Ok(())
}
#[test]
fn responses_incomplete_tool_preserves_reason() -> TestResult {
    let codec = ResponsesCodec::new();
    for (reason, finish, wire_finish) in [
        ("max_output_tokens", FinishReason::Length, "length"),
        (
            "content_filter",
            FinishReason::ContentFilter,
            "content_filter",
        ),
    ] {
        let ir=codec.decode_response(json!({"id":"r","status":"incomplete","incomplete_details":{"reason":reason},"output":[{"type":"function_call","id":"fc","call_id":"c","name":"f","arguments":"{}","status":"incomplete"}]}))?;
        assert_eq!(ir.finish_reason, Some(finish));
        assert_eq!(
            ChatCompletionsCodec::new().encode_response(&ir)?["choices"][0]["finish_reason"],
            wire_finish
        );
        let native = codec.encode_response(&ir)?;
        assert_eq!(native["status"], "incomplete");
        assert_eq!(native["incomplete_details"]["reason"], reason);
        assert_eq!(native["output"][0]["status"], "incomplete");
    }
    Ok(())
}
#[test]
fn messages_nonstream_refusal_preserves_text() -> TestResult {
    let out = MessagesCodec::new().encode_response(&refusal()?)?;
    assert_eq!(out["content"][0]["text"], "拒绝内容");
    assert_eq!(out["stop_reason"], "refusal");
    Ok(())
}
#[test]
fn responses_nonstream_refusal_uses_message_content() -> TestResult {
    let out = ResponsesCodec::new().encode_response(&refusal()?)?;
    assert_eq!(out["output"][0]["type"], "message");
    assert_eq!(out["output"][0]["role"], "assistant");
    assert!(out["output"][0]["id"].is_string());
    assert_eq!(
        out["output"][0]["content"][0],
        json!({"type":"refusal","refusal":"拒绝内容"})
    );
    Ok(())
}
#[test]
fn responses_system_and_developer_keep_instruction_level() -> TestResult {
    for role in ["system", "developer"] {
        let ir=ResponsesCodec::new().decode_request(json!({"model":"m","instructions":"base","input":[{"role":role,"content":"只用中文回答"},{"role":"user","content":"hello"}]}),&env())?;
        let target = MessagesCodec::new();
        assert!(guard(&ir, &target));
        let (out, _) = target.encode_request(&ir)?;
        assert!(out["system"].to_string().contains("只用中文回答"));
        assert!(!out["messages"].to_string().contains("只用中文回答"));
        assert!(out["system"].to_string().contains("base"));
        let target = GeminiCodec::new();
        assert!(guard(&ir, &target));
        let (out, _) = target.encode_request(&ir)?;
        assert!(out["systemInstruction"]
            .to_string()
            .contains("只用中文回答"));
        assert!(!out["contents"].to_string().contains("只用中文回答"));
    }
    Ok(())
}
#[test]
fn messages_hosted_tool_keeps_native_config_and_rejects_crossings() -> TestResult {
    let codec = MessagesCodec::new();
    let tool = json!({"type":"web_search_20250305","name":"web_search","max_uses":1,"allowed_domains":["example.com"],"allowed_callers":["direct"]});
    let ir=codec.decode_request(json!({"model":"m","max_tokens":100,"messages":[{"role":"user","content":"search"}],"tools":[tool.clone()]}),&env())?;
    for target in [
        Box::new(ChatCompletionsCodec::new()) as Box<dyn EndpointCodec>,
        Box::new(ResponsesCodec::new()),
        Box::new(GeminiCodec::new()),
    ] {
        assert!(!guard(&ir, target.as_ref()));
    }
    assert!(guard(&ir, &codec));
    let (out, _) = codec.encode_request(&ir)?;
    assert_eq!(out["tools"][0], tool);
    Ok(())
}
#[test]
fn messages_function_config_preserves_native_callers() -> TestResult {
    let codec = MessagesCodec::new();
    let ir=codec.decode_request(json!({"model":"m","max_tokens":100,"tools":[{"name":"f","input_schema":{"type":"object"},"allowed_callers":["direct","code_execution_20250825"]}],"messages":[{"role":"user","content":"hi"}]}),&env())?;
    assert!(guard(&ir, &codec));
    assert!(!guard(&ir, &ChatCompletionsCodec::new()));
    let (out, _) = codec.encode_request(&ir)?;
    assert_eq!(
        out["tools"][0]["allowed_callers"],
        json!(["direct", "code_execution_20250825"])
    );
    Ok(())
}
#[test]
fn messages_native_thinking_modes_survive_reencode() -> TestResult {
    let codec = MessagesCodec::new();
    for thinking in [
        json!({"type":"adaptive"}),
        json!({"type":"adaptive","display":"omitted"}),
        json!({"type":"disabled"}),
        json!({"type":"enabled","budget_tokens":1024}),
    ] {
        let ir=codec.decode_request(json!({"model":"m","max_tokens":2048,"thinking":thinking.clone(),"messages":[{"role":"user","content":"hi"}]}),&env())?;
        let (out, _) = codec.encode_request(&ir)?;
        assert_eq!(out["thinking"], thinking);
    }
    Ok(())
}
#[test]
fn messages_native_cache_breakpoints_keep_position_and_ttl() -> TestResult {
    let codec = MessagesCodec::new();
    let system = json!([{"type":"text","text":"first"},{"type":"text","text":"rules","cache_control":{"type":"ephemeral","ttl":"1h"}}]);
    let content = json!([{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}},{"type":"text","text":"after"}]);
    let ir=codec.decode_request(json!({"model":"m","max_tokens":100,"system":system.clone(),"messages":[{"role":"user","content":content.clone()}],"tools":[{"name":"f","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral","ttl":"1h"}}]}),&env())?;
    let (out, _) = codec.encode_request(&ir)?;
    assert_eq!(out["system"], system);
    assert_eq!(out["messages"][0]["content"], content);
    assert_eq!(out["tools"][0]["cache_control"]["ttl"], "1h");
    Ok(())
}
#[test]
fn gemini_native_uppercase_thinking_levels_map_to_effort() -> TestResult {
    for level in ["MINIMAL", "LOW", "MEDIUM", "HIGH"] {
        let ir=GeminiCodec::new().decode_request(json!({"model":"gemini-3-pro","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingLevel":level}}}),&env())?;
        let target = ResponsesCodec::new();
        assert!(guard(&ir, &target));
        let (out, _) = target.encode_request(&ir)?;
        assert_eq!(out["reasoning"]["effort"], level.to_ascii_lowercase());
    }
    Ok(())
}
