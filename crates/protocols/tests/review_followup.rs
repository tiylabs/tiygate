use serde_json::{json, Value};
use tiygate_core::{EndpointCodec, RawEnvelope, StreamPart};
use tiygate_protocols::{
    chat_completions::ChatCompletionsCodec, messages::MessagesCodec, responses::ResponsesCodec,
};
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
fn convert(
    src: &dyn EndpointCodec,
    dst: &dyn EndpointCodec,
    input: Value,
) -> Result<Option<Value>, Box<dyn std::error::Error>> {
    let ir = src.decode_request(input, &env())?;
    if tiygate_core::protocol::lossy::check_lossy_conversion(&ir, dst.id(), dst.capabilities())
        .is_err()
    {
        return Ok(None);
    }
    Ok(Some(dst.encode_request(&ir)?.0))
}
fn transcode(
    src: &dyn EndpointCodec,
    dst: &dyn EndpointCodec,
    lines: &[&str],
) -> Result<String, Box<dyn std::error::Error>> {
    let mut decoder = src.stream_decoder();
    let mut encoder = dst.stream_encoder();
    let mut bytes = Vec::new();
    for line in lines {
        for part in decoder.feed(line)? {
            bytes.extend(encoder.encode_part(&part)?);
        }
    }
    for part in decoder.finish()? {
        bytes.extend(encoder.encode_part(&part)?);
    }
    Ok(String::from_utf8(bytes)?)
}
#[test]
fn chat_string_stop_preserved_or_rejected() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(out) = convert(
        &ChatCompletionsCodec::new(),
        &MessagesCodec::new(),
        json!({"model":"m","messages":[{"role":"user","content":"hi"}],"stop":"END"}),
    )? {
        println!("out={out}");
        assert_eq!(out["stop_sequences"], json!(["END"]));
    }
    Ok(())
}
#[test]
fn strict_function_chat_to_responses_preserved_or_rejected(
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(out) = convert(
        &ChatCompletionsCodec::new(),
        &ResponsesCodec::new(),
        json!({"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f","strict":true,"parameters":{"type":"object","properties":{"x":{"type":"integer"}},"required":["x"],"additionalProperties":false}}}]}),
    )? {
        println!("out={out}");
        assert_eq!(out["tools"][0]["strict"], true);
    }
    Ok(())
}
#[test]
fn strict_function_responses_to_chat_preserved_or_rejected(
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(out) = convert(
        &ResponsesCodec::new(),
        &ChatCompletionsCodec::new(),
        json!({"model":"m","input":"hi","tools":[{"type":"function","name":"f","strict":true,"parameters":{"type":"object","properties":{"x":{"type":"integer"}},"required":["x"],"additionalProperties":false}}]}),
    )? {
        println!("out={out}");
        assert_eq!(out["tools"][0]["function"]["strict"], true);
    }
    Ok(())
}
#[test]
fn non_strict_chat_tool_does_not_become_strict_responses() -> Result<(), Box<dyn std::error::Error>>
{
    if let Some(out) = convert(
        &ChatCompletionsCodec::new(),
        &ResponsesCodec::new(),
        json!({"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f","strict":false,"parameters":{"type":"object","properties":{"x":{"type":"integer"}}}}}]}),
    )? {
        println!("out={out}");
        assert_eq!(out["tools"][0]["strict"], false);
    }
    Ok(())
}
#[test]
fn previous_response_id_crossing_requires_rejection() -> Result<(), Box<dyn std::error::Error>> {
    let out = convert(
        &ResponsesCodec::new(),
        &MessagesCodec::new(),
        json!({"model":"m","input":"continue","previous_response_id":"resp_context"}),
    )?;
    println!("out={out:?}");
    assert!(
        out.is_none(),
        "cannot forward a continuation after deleting its only context reference"
    );
    Ok(())
}
#[test]
fn responses_stream_refusal_preserved() -> Result<(), Box<dyn std::error::Error>> {
    let out = transcode(
        &ResponsesCodec::new(),
        &ChatCompletionsCodec::new(),
        &[
            r#"data: {"type":"response.created","sequence_number":0,"response":{"id":"r","status":"in_progress"}}"#,
            r#"data: {"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"id":"msg","type":"message","role":"assistant","status":"in_progress","content":[]}}"#,
            r#"data: {"type":"response.content_part.added","sequence_number":2,"item_id":"msg","output_index":0,"content_index":0,"part":{"type":"refusal","refusal":""}}"#,
            r#"data: {"type":"response.refusal.delta","sequence_number":3,"item_id":"msg","output_index":0,"content_index":0,"delta":"I cannot assist"}"#,
            r#"data: {"type":"response.refusal.done","sequence_number":4,"item_id":"msg","output_index":0,"content_index":0,"refusal":"I cannot assist"}"#,
            r#"data: {"type":"response.content_part.done","sequence_number":5,"item_id":"msg","output_index":0,"content_index":0,"part":{"type":"refusal","refusal":"I cannot assist"}}"#,
            r#"data: {"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":{"id":"msg","type":"message","role":"assistant","status":"completed","content":[{"type":"refusal","refusal":"I cannot assist"}]}}"#,
            r#"data: {"type":"response.completed","sequence_number":7,"response":{"id":"r","status":"completed","output":[{"id":"msg","type":"message","role":"assistant","status":"completed","content":[{"type":"refusal","refusal":"I cannot assist"}]}]}}"#,
        ],
    )?;
    println!("{out}");
    assert!(
        out.contains("I cannot assist"),
        "streamed refusal disappeared"
    );
    Ok(())
}
#[test]
fn chat_multiple_stream_choices_not_merged() -> Result<(), Box<dyn std::error::Error>> {
    let mut decoder = ChatCompletionsCodec::new().stream_decoder();
    assert!(decoder.feed(r#"data: {"id":"r","choices":[{"index":0,"delta":{"content":"alternative A"}},{"index":1,"delta":{"content":"alternative B"}}]}"#).is_err(),
        "IR conversion must reject independent completions before merging them");
    Ok(())
}
#[test]
fn incomplete_response_uses_native_incomplete_event() -> Result<(), Box<dyn std::error::Error>> {
    let out = transcode(
        &ChatCompletionsCodec::new(),
        &ResponsesCodec::new(),
        &[
            r#"data: {"id":"r","choices":[{"index":0,"delta":{"content":"partial"}}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#,
            "data: [DONE]",
        ],
    )?;
    println!("{out}");
    assert!(out.contains("\"type\":\"response.incomplete\""));
    assert!(!out.contains("\"type\":\"response.completed\""));
    Ok(())
}
#[test]
fn malformed_nonstream_tool_arguments_not_replaced_with_empty_object(
) -> Result<(), Box<dyn std::error::Error>> {
    let src = ChatCompletionsCodec::new();
    let ir=src.decode_response(json!({"id":"r","choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"a","type":"function","function":{"name":"f","arguments":"{\"x\":"}}]},"finish_reason":"tool_calls"}]}));
    if let Ok(ir) = ir {
        let out = MessagesCodec::new().encode_response(&ir)?;
        println!("out={out}");
        assert_ne!(
            out["content"][0]["input"],
            json!({}),
            "malformed argument bytes must not become a valid empty tool invocation"
        );
    }
    Ok(())
}
#[test]
fn messages_disable_parallel_preserved_or_rejected() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(out) = convert(
        &MessagesCodec::new(),
        &ChatCompletionsCodec::new(),
        json!({"model":"m","max_tokens":100,"messages":[{"role":"user","content":"hi"}],"tools":[{"name":"f","input_schema":{"type":"object"}}],"tool_choice":{"type":"auto","disable_parallel_tool_use":true}}),
    )? {
        println!("out={out}");
        assert_eq!(out["parallel_tool_calls"], false);
    }
    Ok(())
}
#[test]
fn responses_function_done_keeps_full_arguments() -> Result<(), Box<dyn std::error::Error>> {
    let out = transcode(
        &ResponsesCodec::new(),
        &MessagesCodec::new(),
        &[
            r#"data: {"type":"response.created","response":{"id":"r"}}"#,
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"id":"fc","type":"function_call","call_id":"a","name":"f","arguments":""}}"#,
            r#"data: {"type":"response.function_call_arguments.done","item_id":"fc","output_index":0,"arguments":"{\"x\":1}"}"#,
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"id":"fc","type":"function_call","call_id":"a","name":"f","arguments":"{\"x\":1}"}}"#,
            r#"data: {"type":"response.completed","response":{"id":"r","status":"completed","output":[]}}"#,
        ],
    )?;
    println!("{out}");
    assert!(
        out.contains("partial_json") && out.contains("\\\"x\\\""),
        "complete tool arguments from done event lost"
    );
    Ok(())
}
#[test]
fn post_terminal_responses_content_is_rejected_or_ignored() -> Result<(), Box<dyn std::error::Error>>
{
    let mut decoder = ResponsesCodec::new().stream_decoder();
    decoder.feed(
        r#"data: {"type":"response.completed","response":{"id":"r","status":"completed"}}"#,
    )?;
    if let Ok(parts) = decoder.feed(r#"data: {"type":"response.output_text.delta","delta":"late"}"#)
    {
        println!("parts={parts:?}");
        assert!(
            !parts
                .iter()
                .any(|p| matches!(p, StreamPart::TextDelta { .. })),
            "content emitted after terminal"
        );
    }
    Ok(())
}
