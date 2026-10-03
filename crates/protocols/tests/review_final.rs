//! Original synthetic wire regressions for TG-PROTO-029 and 037–050.
use serde_json::{json, Value};
use tiygate_core::protocol::lossy::check_lossy_conversion;
use tiygate_core::{EndpointCodec, RawEnvelope, StreamPart};
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
fn convert(
    src: &dyn EndpointCodec,
    dst: &dyn EndpointCodec,
    body: Value,
) -> Result<Value, tiygate_core::Error> {
    let ir = src.decode_request(body, &env())?;
    check_lossy_conversion(&ir, dst.id(), dst.capabilities()).map_err(|(_, e)| e)?;
    let result = dst.encode_request(&ir)?.0;
    println!("out={result}");
    Ok(result)
}
fn stream(
    src: &dyn EndpointCodec,
    dst: &dyn EndpointCodec,
    events: &[Value],
) -> Result<Vec<Value>, tiygate_core::Error> {
    let mut dec = src.stream_decoder();
    let mut enc = dst.stream_encoder();
    let mut bytes = Vec::new();
    for e in events {
        for p in dec.feed(&format!("data: {e}"))? {
            bytes.extend(enc.encode_part(&p)?);
        }
    }
    for p in dec.finish()? {
        bytes.extend(enc.encode_part(&p)?);
    }
    let text = String::from_utf8_lossy(&bytes);
    println!("wire={text}");
    Ok(text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect())
}
#[test]
fn responses_tool_result_to_gemini_matches_call_id() -> TestResult {
    let ir = ResponsesCodec::new().decode_request(
        json!({"model":"m","input":[
            {"type":"function_call","id":"fc_item","call_id":"call_fn","name":"f","arguments":"{}"},
            {"type":"function_call_output","call_id":"call_fn","output":"ok"}
        ]}),
        &env(),
    )?;
    let out = GeminiCodec::new().encode_request(&ir)?.0;
    println!("out={out}");
    assert_eq!(
        out["contents"][1]["parts"][0]["functionResponse"]["name"],
        "f"
    );
    Ok(())
}
#[test]
fn chat_specific_tool_to_responses_uses_flat_name() -> TestResult {
    let out = convert(
        &ChatCompletionsCodec::new(),
        &ResponsesCodec::new(),
        json!({"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],"tool_choice":{"type":"function","function":{"name":"f"}}}),
    )?;
    assert_eq!(out["tool_choice"], json!({"type":"function","name":"f"}));
    Ok(())
}
#[test]
fn responses_specific_tool_to_messages_preserves_name() -> TestResult {
    let out = convert(
        &ResponsesCodec::new(),
        &MessagesCodec::new(),
        json!({"model":"m","input":"hi","tools":[{"type":"function","name":"f","parameters":{"type":"object"}}],"tool_choice":{"type":"function","name":"f"}}),
    )?;
    assert_eq!(out["tool_choice"], json!({"type":"tool","name":"f"}));
    Ok(())
}
#[test]
fn messages_tool_image_to_responses_preserved_or_rejected() -> TestResult {
    let result = convert(
        &MessagesCodec::new(),
        &ResponsesCodec::new(),
        json!({"model":"m","max_tokens":100,"messages":[
          {"role":"assistant","content":[{"type":"tool_use","id":"call","name":"f","input":{}}]},
          {"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":[{"type":"text","text":"screenshot"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1n"}}]}]}
        ]}),
    );
    if let Ok(out) = result {
        let result = &out["input"][1]["output"];
        assert!(
            result.is_array()
                && result
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| v["type"] == "input_image")),
            "tool image converted to text: {result}"
        );
    }
    Ok(())
}
#[test]
fn gemini_tool_result_text_order_to_chat_preserved() -> TestResult {
    let out = convert(
        &GeminiCodec::new(),
        &ChatCompletionsCodec::new(),
        json!({"model":"m","contents":[
          {"role":"model","parts":[{"functionCall":{"id":"call","name":"f","args":{}}}]},
          {"role":"user","parts":[{"text":"before"},{"functionResponse":{"id":"call","name":"f","response":{"output":"result"}}},{"text":"after"}]}
        ]}),
    )?;
    let wire = out["messages"].to_string();
    let before = wire.find("before").ok_or("no before")?;
    let result = wire.find("result").ok_or("no result")?;
    let after = wire.find("after").ok_or("no after")?;
    assert!(
        before < result && result < after,
        "reordered conversation: {wire}"
    );
    assert_eq!(out["messages"][1]["role"], "user");
    assert_eq!(out["messages"][2]["role"], "tool");
    assert_eq!(out["messages"][2]["tool_call_id"], "call");
    assert_eq!(out["messages"][3]["role"], "user");
    Ok(())
}
#[test]
fn gemini_response_schema_normalizes_enum_and_nullable() -> TestResult {
    let out = convert(
        &GeminiCodec::new(),
        &ResponsesCodec::new(),
        json!({"model":"m","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"responseMimeType":"application/json","responseSchema":{"type":"OBJECT","properties":{"x":{"type":"STRING","nullable":true}},"required":["x"]}}}),
    )?;
    let schema = &out["text"]["format"]["schema"];
    assert_eq!(schema["type"], "object");
    assert!(
        schema["properties"]["x"]["type"] == json!(["string", "null"])
            || schema["properties"]["x"]["anyOf"].is_array()
    );
    Ok(())
}
#[test]
fn chat_custom_tool_stream_preserves_freeform_input() -> TestResult {
    let es = stream(
        &ChatCompletionsCodec::new(),
        &ResponsesCodec::new(),
        &[
            json!({"id":"r","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call","type":"custom","custom":{"name":"patch","input":"hello"}}]},"finish_reason":null}]}),
            json!({"object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ],
    )?;
    assert!(
        es.iter()
            .any(|e| e["item"]["type"] == "custom_tool_call" && e["item"]["name"] == "patch"),
        "custom tool missing: {es:?}"
    );
    Ok(())
}
#[test]
fn gemini_nonstream_refusal_text_survives() -> TestResult {
    let ir=ResponsesCodec::new().decode_response(json!({"id":"r","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"Cannot assist"}]}]}))?;
    let out = GeminiCodec::new().encode_response(&ir)?;
    println!("out={out}");
    assert!(out.to_string().contains("Cannot assist"));
    Ok(())
}
#[test]
fn messages_stream_refusal_stop_reason_survives() -> TestResult {
    let es = stream(
        &MessagesCodec::new(),
        &ChatCompletionsCodec::new(),
        &[
            json!({"type":"message_start","message":{"id":"m"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"refusal"},"usage":{"output_tokens":0}}),
            json!({"type":"message_stop"}),
        ],
    )?;
    assert!(es
        .iter()
        .any(|e| e["choices"][0]["finish_reason"] == "content_filter"));
    Ok(())
}
#[test]
fn chat_stream_incomplete_tool_json_not_completed_successfully() -> TestResult {
    let src = ChatCompletionsCodec::new();
    let dst = ResponsesCodec::new();
    let mut dec = src.stream_decoder();
    let mut enc = dst.stream_encoder();
    let mut out = Vec::new();
    for line in [
       "data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"x\\\":\"}}]},\"finish_reason\":null}]}",
       "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}", "data: [DONE]"
    ] {
      let parts=match dec.feed(line){Ok(v)=>v,Err(_)=>return Ok(())};
      for p in parts { match enc.encode_part(&p){Ok(v)=>out.extend(v),Err(_)=>return Ok(())} }
    }
    let wire = String::from_utf8(out)?;
    println!("wire={wire}");
    assert!(
        !wire.contains("\"type\":\"response.completed\""),
        "malformed arguments reported completed"
    );
    Ok(())
}
#[test]
fn chat_repeated_tool_name_does_not_reopen_same_call() -> TestResult {
    let result = stream(
        &ChatCompletionsCodec::new(),
        &MessagesCodec::new(),
        &[
            json!({"id":"r","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"f","arguments":"{"}}]}}]}),
            json!({"id":"r","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"f","arguments":"}"}}]}}]}),
        ],
    );
    let events = result?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "content_block_start")
            .count(),
        1
    );
    let arguments: String = events
        .iter()
        .filter_map(|event| event["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(arguments, "{}");
    Ok(())
}
#[test]
fn malformed_responses_history_not_replaced_with_empty_object() -> TestResult {
    let result = convert(
        &ResponsesCodec::new(),
        &MessagesCodec::new(),
        json!({"model":"m","input":[{"type":"function_call","id":"fc","call_id":"call","name":"f","arguments":"{\"x\":"}]}),
    );
    assert!(
        result.is_err(),
        "malformed history must be explicitly rejected: {result:?}"
    );
    Ok(())
}
#[test]
fn chat_late_tool_id_is_buffered_until_identity_available() -> TestResult {
    let result = stream(
        &ChatCompletionsCodec::new(),
        &MessagesCodec::new(),
        &[
            json!({"id":"r","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"f","arguments":"{"}}]}}]}),
            json!({"id":"r","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call","function":{"arguments":"}"}}]}}]}),
        ],
    );
    let events = result?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "content_block_start")
            .count(),
        1
    );
    let opener = events
        .iter()
        .find(|event| event["type"] == "content_block_start")
        .ok_or("missing tool opener")?;
    assert_eq!(opener["content_block"]["id"], "call");
    let arguments: String = events
        .iter()
        .filter_map(|event| event["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(arguments, "{}");
    Ok(())
}
#[test]
fn enormous_chat_tool_index_errors_without_panic() {
    let mut dec = ChatCompletionsCodec::new().stream_decoder();
    let line = format!(
        "data: {}",
        json!({"id":"r","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":u64::MAX,"id":"call","function":{"name":"f","arguments":"{}"}}]}}]})
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dec.feed(&line)));
    assert!(
        matches!(result, Ok(Err(_))),
        "unbounded index panicked or was accepted: {result:?}"
    );
}
#[test]
fn enormous_messages_tool_index_errors_without_panic() {
    let mut dec = MessagesCodec::new().stream_decoder();
    let line = format!(
        "data: {}",
        json!({"type":"content_block_start","index":u64::MAX,"content_block":{"type":"tool_use","id":"call","name":"f","input":{}}})
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dec.feed(&line)));
    assert!(
        matches!(result, Ok(Err(_))),
        "unbounded index panicked or was accepted: {result:?}"
    );
}
#[test]
fn gemini_tool_arguments_to_chat_are_json_text() -> TestResult {
    let ir = GeminiCodec::new().decode_request(
        json!({"model":"m","contents":[
          {"role":"model","parts":[{"functionCall":{"id":"call","name":"f","args":{"n":1}}}]}
        ]}),
        &env(),
    )?;
    let out = ChatCompletionsCodec::new().encode_request(&ir)?.0;
    println!("out={out}");
    assert!(out["messages"][0]["tool_calls"][0]["function"]["arguments"].is_string());
    Ok(())
}
#[test]
fn gemini_image_url_to_messages_rejects() -> TestResult {
    let ir=GeminiCodec::new().decode_request(json!({"model":"m","contents":[
      {"role":"user","parts":[{"fileData":{"fileUri":"https://example.test/image.png","mimeType":"image/png"}}]}
    ]}),&env())?;
    let messages = MessagesCodec::new();
    let guard = tiygate_core::protocol::lossy::check_lossy_conversion(
        &ir,
        messages.id(),
        messages.capabilities(),
    );
    println!("guard={guard:?}");
    assert!(
        guard.is_err(),
        "URL image silently converted to Anthropic inline path"
    );
    Ok(())
}
#[test]
fn content_filter_to_messages_stream_does_not_become_end_turn() -> TestResult {
    let mut enc = MessagesCodec::new().stream_encoder();
    enc.encode_part(&StreamPart::ResponseStarted { id: "r".into() })?;
    enc.encode_part(&StreamPart::Finish {
        reason: tiygate_core::FinishReason::ContentFilter,
    })?;
    let out = enc.encode_part(&StreamPart::ResponseCompleted {
        id: "r".into(),
        status: "incomplete".into(),
        usage: None,
        extensions: Default::default(),
    })?;
    let wire = String::from_utf8(out)?;
    println!("wire={wire}");
    assert!(wire.contains("refusal") || wire.contains("content_filter"));
    Ok(())
}

#[test]
fn gemini_native_parameters_normalized_to_json_schema() -> Result<(), Box<dyn std::error::Error>> {
    let src = GeminiCodec::new();
    let dst = ChatCompletionsCodec::new();
    let ir=src.decode_request(json!({"model":"m","contents":[{"role":"user","parts":[{"text":"hi"}]}],"tools":[{"functionDeclarations":[{"name":"f","parameters":{"type":"OBJECT","properties":{"x":{"type":"STRING","nullable":true}},"required":["x"]}}]}]}),&env())?;
    let out = dst.encode_request(&ir)?.0;
    println!("out={out}");
    assert_eq!(out["tools"][0]["function"]["parameters"]["type"], "object");
    Ok(())
}
#[test]
fn chat_custom_tool_definition_has_native_nested_carrier() -> Result<(), Box<dyn std::error::Error>>
{
    let codec = ChatCompletionsCodec::new();
    let ir=codec.decode_request(json!({"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"custom","custom":{"name":"patch","description":"apply patch","format":{"type":"text"}}}]}),&env())?;
    let out = ResponsesCodec::new().encode_request(&ir)?.0;
    println!("out={out}");
    assert_eq!(out["tools"][0]["name"], "patch");
    assert_eq!(out["tools"][0]["format"], json!({"type":"text"}));
    Ok(())
}
#[test]
fn responses_custom_tool_definition_to_chat_uses_custom_object(
) -> Result<(), Box<dyn std::error::Error>> {
    let ir=ResponsesCodec::new().decode_request(json!({"model":"m","input":"hi","tools":[{"type":"custom","name":"patch","format":{"type":"text"}}]}),&env())?;
    let out = ChatCompletionsCodec::new().encode_request(&ir)?.0;
    println!("out={out}");
    assert_eq!(out["tools"][0]["custom"]["name"], "patch");
    Ok(())
}
#[test]
fn gemini_json_schema_native_carrier_control() -> Result<(), Box<dyn std::error::Error>> {
    let src = GeminiCodec::new();
    let dst = ResponsesCodec::new();
    let schema =
        json!({"type":"object","properties":{"x":{"type":["string","null"]}},"required":["x"]});
    let ir=src.decode_request(json!({"model":"m","contents":[{"parts":[{"text":"hi"}]}],"generationConfig":{"responseJsonSchema":schema,"responseMimeType":"application/json"}}),&env())?;
    let out = dst.encode_request(&ir)?.0;
    assert_eq!(out["text"]["format"]["schema"], schema);
    Ok(())
}
#[test]
fn responses_call_id_to_chat_control() -> Result<(), Box<dyn std::error::Error>> {
    let ir=ResponsesCodec::new().decode_request(json!({"model":"m","input":[{"type":"function_call","id":"fc","call_id":"call","name":"f","arguments":"{}"},{"type":"function_call_output","call_id":"call","output":"ok"}]}),&env())?;
    let out = ChatCompletionsCodec::new().encode_request(&ir)?.0;
    assert_eq!(out["messages"][0]["tool_calls"][0]["id"], "call");
    assert_eq!(out["messages"][1]["tool_call_id"], "call");
    Ok(())
}
#[test]
fn malformed_history_reproduction_input_is_invalid_json() {
    assert!(serde_json::from_str::<Value>("{\"x\":").is_err());
}
#[test]
fn gemini_code_execution_to_chat_rejects_instead_of_dropping(
) -> Result<(), Box<dyn std::error::Error>> {
    let ir=GeminiCodec::new().decode_request(json!({"model":"m","contents":[{"parts":[{"text":"run code"}]}],"tools":[{"codeExecution":{}}]}),&env())?;
    let dst = ChatCompletionsCodec::new();
    let guard =
        tiygate_core::protocol::lossy::check_lossy_conversion(&ir, dst.id(), dst.capabilities());
    println!(
        "IR tools={:?} guard={guard:?} encoded={:?}",
        ir.tools,
        dst.encode_request(&ir)
    );
    assert!(
        guard.is_err(),
        "hosted execution tool silently removed before guard"
    );
    Ok(())
}

#[test]
fn named_choices_preserve_same_protocol_carriers() -> TestResult {
    let codecs: Vec<Box<dyn EndpointCodec>> = vec![
        Box::new(ChatCompletionsCodec::new()),
        Box::new(ResponsesCodec::new()),
    ];
    for (i, codec) in codecs.iter().enumerate() {
        let choice = if i == 0 {
            json!({"type":"function","function":{"name":"f"}})
        } else {
            json!({"type":"function","name":"f"})
        };
        let request = if i == 0 {
            json!({"model":"m","messages":[{"role":"user","content":"hi"}],"tool_choice":choice})
        } else {
            json!({"model":"m","input":"hi","tool_choice":choice})
        };
        let ir = codec.decode_request(request, &env())?;
        assert_eq!(codec.encode_request(&ir)?.0["tool_choice"], choice);
    }
    Ok(())
}
#[test]
fn schema_nullable_enum_and_instance_values_preserved() -> TestResult {
    let schema = json!({"type":"OBJECT","properties":{"x":{"type":"STRING","nullable":true,"enum":["a"]},"data":{"type":"OBJECT","default":{"type":"STRING","nullable":true}}}});
    let ir=GeminiCodec::new().decode_request(json!({"model":"m","contents":[{"parts":[{"text":"hi"}]}],"generationConfig":{"responseSchema":schema}}),&env())?;
    let out = ResponsesCodec::new().encode_request(&ir)?.0;
    let x = &out["text"]["format"]["schema"]["properties"]["x"];
    assert_eq!(x["anyOf"][0]["enum"], json!(["a"]));
    assert_eq!(x["anyOf"][1], json!({"type":"null"}));
    assert_eq!(
        out["text"]["format"]["schema"]["properties"]["data"]["default"],
        json!({"type":"STRING","nullable":true})
    );
    Ok(())
}
#[test]
fn native_hosted_tool_survives_gemini_reencode() -> TestResult {
    let codec = GeminiCodec::new();
    let ir=codec.decode_request(json!({"model":"m","contents":[{"parts":[{"text":"hi"}]}],"tools":[{"codeExecution":{}},{"googleSearch":{}}]}),&env())?;
    assert!(check_lossy_conversion(&ir, codec.id(), codec.capabilities()).is_ok());
    let out = codec.encode_request(&ir)?.0;
    assert!(out["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|t| t.get("codeExecution").is_some())));
    assert!(out["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|t| t.get("googleSearch").is_some())));
    let responses = ResponsesCodec::new();
    assert!(check_lossy_conversion(&ir, responses.id(), responses.capabilities()).is_err());
    Ok(())
}
#[test]
fn multimodal_tool_result_survives_messages_reencode() -> TestResult {
    let codec = MessagesCodec::new();
    let original = json!([{"type":"text","text":"screenshot"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1n"}}]);
    let ir=codec.decode_request(json!({"model":"m","max_tokens":100,"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":original}]}]}),&env())?;
    assert!(check_lossy_conversion(&ir, codec.id(), codec.capabilities()).is_ok());
    assert_eq!(
        codec.encode_request(&ir)?.0["messages"][0]["content"][0]["content"],
        original
    );
    Ok(())
}
#[test]
fn chat_identity_conflict_and_missing_identity_fail() -> TestResult {
    for second in [
        json!({"index":0,"id":"changed","function":{"name":"f","arguments":"{}"}}),
        json!({"index":0,"id":"call","function":{"name":"changed","arguments":"{}"}}),
    ] {
        let mut decoder = ChatCompletionsCodec::new().stream_decoder();
        decoder.feed(&format!("data: {}",json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"f","arguments":""}}]}}]})))?;
        assert!(decoder
            .feed(&format!(
                "data: {}",
                json!({"choices":[{"delta":{"tool_calls":[second]}}]})
            ))
            .is_err());
    }
    let mut decoder = ChatCompletionsCodec::new().stream_decoder();
    decoder.feed(&format!(
        "data: {}",
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}}]})
    ))?;
    assert!(decoder.feed("data: [DONE]").is_err());
    Ok(())
}
#[test]
fn custom_stream_both_directions_preserve_input() -> TestResult {
    let codecs: Vec<Box<dyn EndpointCodec>> = vec![
        Box::new(ChatCompletionsCodec::new()),
        Box::new(ResponsesCodec::new()),
    ];
    let events = [
        vec![
            json!({"id":"r","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call","type":"custom","custom":{"name":"patch","input":"hello"}}]}}]}),
        ],
        vec![
            json!({"type":"response.created","response":{"id":"r"}}),
            json!({"type":"response.output_item.added","item":{"id":"ct","call_id":"call","type":"custom_tool_call","name":"patch","input":"hello"}}),
        ],
    ];
    for (i, src) in codecs.iter().enumerate() {
        let out = stream(src.as_ref(), codecs[1 - i].as_ref(), &events[i])?;
        if i == 0 {
            assert!(out
                .iter()
                .any(|e| e["type"] == "response.custom_tool_call_input.delta"
                    && e["delta"] == "hello"));
        } else {
            assert!(out
                .iter()
                .any(|e| e["choices"][0]["delta"]["tool_calls"][0]["custom"]["input"] == "hello"));
        }
    }
    Ok(())
}

#[test]
fn sparse_and_invalid_tool_indices_are_bounded() -> TestResult {
    for index in [json!(-1), json!(1.5), json!(256), json!(1000000)] {
        let mut decoder = ChatCompletionsCodec::new().stream_decoder();
        let frame = format!(
            "data: {}",
            json!({"choices":[{"delta":{"tool_calls":[{"index":index,"id":"call","function":{"name":"f","arguments":"{}"}}]}}]})
        );
        assert!(decoder.feed(&frame).is_err());
    }
    for index in [json!(-1), json!(1.5), json!(1024), json!(1000000)] {
        let mut decoder = MessagesCodec::new().stream_decoder();
        let frame = format!(
            "data: {}",
            json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":"call","name":"f"}})
        );
        assert!(decoder.feed(&frame).is_err());
    }
    Ok(())
}
#[test]
fn tool_arguments_memory_is_bounded() -> TestResult {
    let mut decoder = ChatCompletionsCodec::new().stream_decoder();
    let arguments = "a".repeat(16 * 1024 * 1024 + 1);
    assert!(decoder.feed(&format!("data: {}",json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"f","arguments":arguments}}]}}]}))).is_err());
    Ok(())
}
#[test]
fn length_truncated_tool_preserves_incomplete() -> TestResult {
    let mut decoder = ChatCompletionsCodec::new().stream_decoder();
    decoder.feed(&format!("data: {}",json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"f","arguments":"{"}}]}}]})))?;
    decoder.feed(&format!(
        "data: {}",
        json!({"choices":[{"delta":{},"finish_reason":"length"}]})
    ))?;
    assert!(decoder
        .feed("data: [DONE]")?
        .iter()
        .any(|p| matches!(p,StreamPart::ResponseCompleted{status,..} if status=="incomplete")));
    Ok(())
}
#[test]
fn duplicate_ids_on_different_indices_are_rejected() -> TestResult {
    let mut decoder = ChatCompletionsCodec::new().stream_decoder();
    decoder.feed(&format!("data: {}",json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"f","arguments":"{}"}}]}}]})))?;
    assert!(decoder.feed(&format!("data: {}",json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call","function":{"name":"g","arguments":"{}"}}]}}]}))).is_err());
    Ok(())
}

#[test]
fn gemini_allowlist_does_not_hide_hosted_tools_from_guard() -> TestResult {
    let codec = GeminiCodec::new();
    let ir=codec.decode_request(json!({"model":"m","contents":[{"parts":[{"text":"hi"}]}],"tools":[{"functionDeclarations":[{"name":"f","parameters":{"type":"OBJECT"}}]},{"codeExecution":{}}],"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["f"]}}}),&env())?;
    assert!(ir
        .tools
        .iter()
        .any(|tool| tool.tool_type.as_deref() == Some("codeExecution")));
    let chat = ChatCompletionsCodec::new();
    assert!(check_lossy_conversion(&ir, chat.id(), chat.capabilities()).is_err());
    Ok(())
}
