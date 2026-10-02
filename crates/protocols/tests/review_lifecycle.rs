//! Independent lifecycle and loss-guard invariants for the follow-up fixes.
use serde_json::{json, Value};
use tiygate_core::{EndpointCodec, FinishReason, RawEnvelope, StreamPart};
use tiygate_protocols::{
    chat_completions::ChatCompletionsCodec, gemini::GeminiCodec, messages::MessagesCodec,
    responses::ResponsesCodec,
};
fn codecs() -> Vec<Box<dyn EndpointCodec>> {
    vec![
        Box::new(ChatCompletionsCodec::new()),
        Box::new(MessagesCodec::new()),
        Box::new(ResponsesCodec::new()),
        Box::new(GeminiCodec::new()),
    ]
}
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
#[test]
fn refusal_text_survives_every_target_once() -> Result<(), Box<dyn std::error::Error>> {
    for target in codecs() {
        let mut decoder = ResponsesCodec::new().stream_decoder();
        let mut encoder = target.stream_encoder();
        let mut bytes = Vec::new();
        for line in [
            r#"data: {"type":"response.created","response":{"id":"r"}}"#,
            r#"data: {"type":"response.refusal.delta","item_id":"msg","content_index":0,"delta":"denied"}"#,
            r#"data: {"type":"response.refusal.done","item_id":"msg","content_index":0,"refusal":"denied"}"#,
            r#"data: {"type":"response.completed","response":{"id":"r","status":"completed","output":[{"id":"msg","type":"message","content":[{"type":"refusal","refusal":"denied"}]}]}}"#,
        ] {
            for part in decoder.feed(line)? {
                bytes.extend(encoder.encode_part(&part)?);
            }
        }
        let text = String::from_utf8(bytes)?;
        let events: Vec<Value> = text
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let delivered: String = events
            .iter()
            .filter_map(|e| match target.id().suite {
                tiygate_core::ProtocolSuite::OpenAiCompatible => {
                    e["choices"][0]["delta"]["refusal"].as_str()
                }
                tiygate_core::ProtocolSuite::OpenAiResponses => {
                    if e["type"] == "response.refusal.delta" {
                        e["delta"].as_str()
                    } else {
                        None
                    }
                }
                tiygate_core::ProtocolSuite::AnthropicMessages => e["delta"]["text"].as_str(),
                tiygate_core::ProtocolSuite::GoogleGemini => {
                    e["candidates"][0]["content"]["parts"][0]["text"].as_str()
                }
            })
            .collect();
        assert_eq!(delivered, "denied", "{}: {text}", target.id());
    }
    Ok(())
}
#[test]
fn strict_defaults_are_preserved_between_openai_protocols() -> Result<(), Box<dyn std::error::Error>>
{
    let chat = ChatCompletionsCodec::new();
    let responses = ResponsesCodec::new();
    let ir = chat.decode_request(json!({"model":"m","messages":[],"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}]}), &env())?;
    assert_eq!(
        responses.encode_request(&ir)?.0["tools"][0]["strict"],
        false
    );
    let ir = responses.decode_request(json!({"model":"m","input":"hi","tools":[{"type":"function","name":"f","parameters":{"type":"object"}}]}), &env())?;
    assert_eq!(
        chat.encode_request(&ir)?.0["tools"][0]["function"]["strict"],
        true
    );
    let gemini = GeminiCodec::new();
    assert!(tiygate_core::protocol::lossy::check_lossy_conversion(
        &ir,
        gemini.id(),
        gemini.capabilities()
    )
    .is_err());
    Ok(())
}
#[test]
fn tool_done_payload_completes_a_prefix_without_duplication(
) -> Result<(), Box<dyn std::error::Error>> {
    let mut d = ResponsesCodec::new().stream_decoder();
    d.feed(r#"data: {"type":"response.output_item.added","item":{"type":"function_call","id":"fc","call_id":"a","name":"f","arguments":""}}"#)?;
    let prefix=d.feed(r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc","delta":"{\"x\":"}"#)?;
    let suffix=d.feed(r#"data: {"type":"response.function_call_arguments.done","item_id":"fc","arguments":"{\"x\":1}"}"#)?;
    let repeated=d.feed(r#"data: {"type":"response.output_item.done","item":{"id":"fc","type":"function_call","arguments":"{\"x\":1}"}}"#)?;
    let arguments: String = prefix
        .iter()
        .chain(&suffix)
        .chain(&repeated)
        .filter_map(|p| {
            if let StreamPart::ToolCallDelta { arguments, .. } = p {
                Some(arguments.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(arguments, "{\"x\":1}");
    Ok(())
}
#[test]
fn inconsistent_or_malformed_completed_arguments_are_errors(
) -> Result<(), Box<dyn std::error::Error>> {
    for arguments in ["{\"y\":2}", "{\"x\":"] {
        let mut d = ResponsesCodec::new().stream_decoder();
        d.feed(r#"data: {"type":"response.output_item.added","item":{"type":"function_call","id":"fc","call_id":"a","name":"f","arguments":""}}"#)?;
        d.feed(r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc","delta":"{\"x\":"}"#)?;
        assert!(d.feed(&format!("data: {}",json!({"type":"response.function_call_arguments.done","item_id":"fc","arguments":arguments}))).is_err());
    }
    Ok(())
}
#[test]
fn all_encoders_stop_semantic_content_after_terminal() -> Result<(), Box<dyn std::error::Error>> {
    for codec in codecs() {
        let mut encoder = codec.stream_encoder();
        encoder.encode_part(&StreamPart::Finish {
            reason: FinishReason::Stop,
        })?;
        encoder.encode_part(&StreamPart::ResponseCompleted {
            id: "r".into(),
            status: "completed".into(),
            usage: None,
            extensions: Default::default(),
        })?;
        assert!(encoder
            .encode_part(&StreamPart::TextDelta {
                text: "late".into()
            })?
            .is_empty());
    }
    Ok(())
}
#[test]
fn incomplete_keeps_content_filter_and_never_becomes_tool_success(
) -> Result<(), Box<dyn std::error::Error>> {
    let mut decoder = ResponsesCodec::new().stream_decoder();
    decoder.feed(r#"data: {"type":"response.output_item.added","item":{"type":"function_call","id":"fc","call_id":"a","name":"f","arguments":""}}"#)?;
    let parts = decoder.feed(r#"data: {"type":"response.incomplete","response":{"id":"r","status":"incomplete","incomplete_details":{"reason":"content_filter"}}}"#)?;
    assert!(parts.iter().any(|part| matches!(
        part,
        StreamPart::Finish {
            reason: FinishReason::ContentFilter
        }
    )));
    let mut encoder = ResponsesCodec::new().stream_encoder();
    let mut bytes = Vec::new();
    for part in parts {
        bytes.extend(encoder.encode_part(&part)?);
    }
    let output = String::from_utf8(bytes)?;
    assert!(output.contains("response.incomplete"));
    assert!(output.contains("content_filter"));
    Ok(())
}
#[test]
fn state_and_unrepresentable_controls_are_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let responses = ResponsesCodec::new();
    let ir = responses.decode_request(
        json!({"model":"m","input":"hi","conversation":{"id":"conv_1"}}),
        &env(),
    )?;
    let messages = MessagesCodec::new();
    assert!(tiygate_core::protocol::lossy::check_lossy_conversion(
        &ir,
        messages.id(),
        messages.capabilities()
    )
    .is_err());
    let chat = ChatCompletionsCodec::new();
    let ir = chat.decode_request(
        json!({"model":"m","n":2,"messages":[{"role":"user","content":"hi"}]}),
        &env(),
    )?;
    assert!(tiygate_core::protocol::lossy::check_lossy_conversion(
        &ir,
        responses.id(),
        responses.capabilities()
    )
    .is_err());
    let ir=chat.decode_request(json!({"model":"m","parallel_tool_calls":false,"messages":[{"role":"user","content":"hi"}]}),&env())?;
    let gemini = GeminiCodec::new();
    assert!(tiygate_core::protocol::lossy::check_lossy_conversion(
        &ir,
        gemini.id(),
        gemini.capabilities()
    )
    .is_err());
    Ok(())
}
