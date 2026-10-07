//! Original wire regressions for the 2026-10-07 branch delta review.
use serde_json::{json, Value};
use tiygate_core::{EndpointCodec, RawEnvelope, StreamPart};
use tiygate_protocols::{
    chat_completions::ChatCompletionsCodec, gemini::GeminiCodec, messages::MessagesCodec,
    responses::ResponsesCodec,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn env() -> RawEnvelope {
    RawEnvelope {
        method: "POST".into(),
        path: "/v1/responses".into(),
        headers: Default::default(),
        body: None,
        original_body_size: 0,
        timestamp: chrono::Utc::now(),
    }
}

fn events(wire: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(wire)
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}

#[test]
fn implicit_strict_normalizes_nested_function_schemas() -> TestResult {
    let schema = json!({
        "type": "object",
        "properties": {
            "x": {"type": "string"},
            "nested": {"type": "object", "properties": {"n": {"type": "integer"}}},
            "list": {"type": "array", "items": {"$ref": "#/$defs/Entry"}}
        },
        "$defs": {"Entry": {"type": "object", "properties": {"value": {"type": "string"}}}}
    });
    let input = json!({"model":"m", "input":"hi", "tools":[
        {"type":"function", "name":"f", "parameters":schema}
    ]});
    let ir = ResponsesCodec::new().decode_request(input.clone(), &env())?;
    let out = ChatCompletionsCodec::new().encode_request(&ir)?.0;
    let function = &out["tools"][0]["function"];
    assert_eq!(function["strict"], true);
    let parameters = &function["parameters"];
    assert_eq!(parameters["additionalProperties"], false);
    assert_eq!(parameters["required"], json!(["list", "nested", "x"]));
    assert_eq!(
        parameters["properties"]["nested"]["additionalProperties"],
        false
    );
    assert_eq!(parameters["properties"]["nested"]["required"], json!(["n"]));
    assert_eq!(parameters["$defs"]["Entry"]["additionalProperties"], false);
    assert_eq!(parameters["$defs"]["Entry"]["required"], json!(["value"]));
    assert_eq!(
        parameters["properties"]["list"]["items"]["$ref"],
        "#/$defs/Entry"
    );
    let messages = MessagesCodec::new().encode_request(&ir)?.0;
    assert_eq!(messages["tools"][0]["strict"], true);
    assert_eq!(messages["tools"][0]["input_schema"], *parameters);
    let native = ResponsesCodec::new().encode_request(&ir)?.0;
    assert!(native["tools"][0].get("strict").is_none());
    assert_eq!(native["tools"][0]["parameters"], schema);
    assert!(tiygate_core::protocol::lossy::check_lossy_conversion(
        &ir,
        GeminiCodec::new().id(),
        GeminiCodec::new().capabilities()
    )
    .is_err());
    Ok(())
}

#[test]
fn implicit_strict_falls_back_for_unsupported_schemas() -> TestResult {
    for schema in [
        json!({"type":"object","properties":{"x":{"oneOf":[{"type":"string"},{"type":"number"}]}}}),
        json!({"type":"object","anyOf":[
            {"type":"object","properties":{"x":{"type":"string"}}},
            {"type":"object","properties":{"y":{"type":"number"}}}
        ]}),
        json!({"type":"object","properties":{"x":{"type":"string","format":"uri"}}}),
        json!({"type":"object","additionalProperties":{"type":"string"}}),
    ] {
        let ir = ResponsesCodec::new().decode_request(
            json!({"model":"m","input":"hi","tools":[{"type":"function","name":"f","parameters":schema}]}),
            &env(),
        )?;
        let out = ChatCompletionsCodec::new().encode_request(&ir)?.0;
        assert_eq!(out["tools"][0]["function"]["strict"], false);
        assert_eq!(out["tools"][0]["function"]["parameters"], schema);
        let native = ResponsesCodec::new().encode_request(&ir)?.0;
        assert!(native["tools"][0].get("strict").is_none());
        assert_eq!(native["tools"][0]["parameters"], schema);
    }
    Ok(())
}

#[test]
fn implicit_strict_respects_schema_size_and_depth_limits() -> TestResult {
    let mut deep = json!({"type":"string"});
    for _ in 0..11 {
        deep = json!({"type":"object","properties":{"child":deep}});
    }
    let large_enum = json!({
        "type":"object","properties":{"x":{
            "type":"integer","enum":(0..1001).collect::<Vec<_>>()
        }}
    });
    for schema in [deep, large_enum] {
        let ir = ResponsesCodec::new().decode_request(
            json!({"model":"m","input":"hi","tools":[{"type":"function","name":"f","parameters":schema}]}),
            &env(),
        )?;
        let out = ChatCompletionsCodec::new().encode_request(&ir)?.0;
        assert_eq!(out["tools"][0]["function"]["strict"], false);
        assert_eq!(out["tools"][0]["function"]["parameters"], schema);
    }
    Ok(())
}

#[test]
fn implicit_strict_missing_parameters_keeps_native_omission() -> TestResult {
    let ir = ResponsesCodec::new().decode_request(
        json!({"model":"m","input":"hi","tools":[{"type":"function","name":"f"}]}),
        &env(),
    )?;
    let native = ResponsesCodec::new().encode_request(&ir)?.0;
    assert!(native["tools"][0].get("strict").is_none());
    assert!(native["tools"][0].get("parameters").is_none());
    let chat = ChatCompletionsCodec::new().encode_request(&ir)?.0;
    assert_eq!(chat["tools"][0]["function"]["strict"], true);
    assert_eq!(
        chat["tools"][0]["function"]["parameters"],
        json!({
            "type":"object","properties":{},"required":[],"additionalProperties":false
        })
    );
    Ok(())
}

#[test]
fn explicit_and_null_strict_keep_native_provenance() -> TestResult {
    let schema = json!({"type":"object","properties":{"x":{"type":"string"}}});
    for strict in [Value::Null, json!(false), json!(true)] {
        let ir = ResponsesCodec::new().decode_request(
            json!({"model":"m","input":"hi","tools":[
                {"type":"function","name":"f","parameters":schema,"strict":strict}
            ]}),
            &env(),
        )?;
        let native = ResponsesCodec::new().encode_request(&ir)?.0;
        assert_eq!(native["tools"][0]["strict"], strict);
        assert_eq!(native["tools"][0]["parameters"], schema);
        let chat = ChatCompletionsCodec::new().encode_request(&ir)?.0;
        if strict.is_null() {
            assert_eq!(chat["tools"][0]["function"]["strict"], true);
            assert_eq!(
                chat["tools"][0]["function"]["parameters"]["additionalProperties"],
                false
            );
        } else {
            assert_eq!(chat["tools"][0]["function"]["strict"], strict);
            assert_eq!(chat["tools"][0]["function"]["parameters"], schema);
        }
    }
    Ok(())
}

#[test]
fn native_schema_replay_does_not_undo_canonical_edits() -> TestResult {
    let mut ir = ResponsesCodec::new().decode_request(
        json!({"model":"m","input":"hi","tools":[
            {"type":"function","name":"f","parameters":{"type":"object"}}
        ]}),
        &env(),
    )?;
    ir.tools[0].parameters = Some(json!({
        "type":"object","properties":{"new":{"type":"string"}},
        "required":["new"],"additionalProperties":false
    }));
    let native = ResponsesCodec::new().encode_request(&ir)?.0;
    assert_eq!(
        native["tools"][0]["parameters"],
        ir.tools[0].parameters.clone().unwrap_or_default()
    );
    assert_eq!(native["tools"][0]["strict"], true);
    Ok(())
}

#[test]
fn gemini_without_id_completes_once_with_latest_usage() -> TestResult {
    let mut decoder = GeminiCodec::new().stream_decoder();
    let mut encoder = ResponsesCodec::new().stream_encoder();
    let mut wire = Vec::new();
    for frame in [
        json!({"candidates":[{"content":{"parts":[{"text":"done"}]},"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}),
        json!({"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":7,"totalTokenCount":8}}),
    ] {
        for part in decoder.feed(&format!("data: {frame}"))? {
            wire.extend(encoder.encode_part(&part)?);
        }
    }
    for part in decoder.finish()? {
        wire.extend(encoder.encode_part(&part)?);
    }
    assert!(decoder.finish()?.is_empty());
    let completed: Vec<_> = events(&wire)
        .into_iter()
        .filter(|event| event["type"] == "response.completed")
        .collect();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0]["response"]["usage"]["output_tokens"], 7);
    assert!(wire.ends_with(b"data: [DONE]\n\n"));
    Ok(())
}

#[test]
fn gemini_no_id_usage_or_error_does_not_complete() -> TestResult {
    let mut decoder = GeminiCodec::new().stream_decoder();
    decoder.feed("data: {\"usageMetadata\":{\"totalTokenCount\":1}}")?;
    assert!(decoder.finish()?.is_empty());
    decoder.feed("data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}")?;
    decoder.feed("data: {\"error\":{\"message\":\"failed\",\"status\":\"INTERNAL\"}}")?;
    assert!(decoder.finish()?.is_empty());
    Ok(())
}

#[test]
fn gemini_error_after_finish_does_not_complete_with_id() -> TestResult {
    let mut decoder = GeminiCodec::new().stream_decoder();
    decoder.feed("data: {\"responseId\":\"r\",\"candidates\":[{\"finishReason\":\"STOP\"}]}")?;
    decoder.feed("data: {\"error\":{\"message\":\"failed\",\"status\":\"INTERNAL\"}}")?;
    assert!(decoder.finish()?.is_empty());
    assert!(decoder
        .feed("data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"late\"}]}}]}")?
        .is_empty());
    Ok(())
}

#[test]
fn gemini_tools_accept_separate_trailing_whitespace() -> TestResult {
    let mut decoder = ChatCompletionsCodec::new().stream_decoder();
    let mut encoder = GeminiCodec::new().stream_encoder();
    let mut wire = Vec::new();
    for frame in [
        json!({"id":"r","choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"call","type":"function","function":{"name":"f","arguments":"{}"}}
        ]}}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"\n \t"}}
        ]}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ] {
        for part in decoder.feed(&format!("data: {frame}"))? {
            wire.extend(encoder.encode_part(&part)?);
        }
    }
    let frames = events(&wire);
    let calls: Vec<_> = frames
        .iter()
        .filter(|event| {
            event["candidates"][0]["content"]["parts"][0]
                .get("functionCall")
                .is_some()
        })
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["candidates"][0]["content"]["parts"][0]["functionCall"]["args"],
        json!({})
    );
    assert!(frames
        .iter()
        .any(|event| event["candidates"][0]["finishReason"] == "STOP"));
    assert!(!frames.iter().any(|event| event.get("error").is_some()));
    Ok(())
}

#[test]
fn gemini_tools_still_reject_semantic_suffixes() -> TestResult {
    let mut encoder = GeminiCodec::new().stream_encoder();
    encoder.encode_part(&StreamPart::ToolCallDelta {
        id: "call".into(),
        name: Some("f".into()),
        arguments: "{}".into(),
        wire_type: None,
        item_id: None,
        caller: None,
    })?;
    assert!(encoder
        .encode_part(&StreamPart::ToolCallDelta {
            id: "call".into(),
            name: None,
            arguments: "{}".into(),
            wire_type: None,
            item_id: None,
            caller: None
        })
        .is_err());
    assert!(encoder
        .encode_part(&StreamPart::ToolCallDelta {
            id: "call".into(),
            name: None,
            arguments: "\u{00a0}".into(),
            wire_type: None,
            item_id: None,
            caller: None
        })
        .is_err());
    Ok(())
}

