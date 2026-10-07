//! Original synthetic wire regressions from the 2026-10-02 protocol review.
//! Assertions inspect native wire fields independently of the target decoder.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{json, Value};
use tiygate_core::{Content, EndpointCodec, RawEnvelope, StreamPart};
use tiygate_protocols::{
    chat_completions::ChatCompletionsCodec, gemini::GeminiCodec, messages::MessagesCodec,
    responses::ResponsesCodec,
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
fn codecs() -> Vec<Box<dyn EndpointCodec>> {
    vec![
        Box::new(ChatCompletionsCodec::new()),
        Box::new(MessagesCodec::new()),
        Box::new(ResponsesCodec::new()),
        Box::new(GeminiCodec::new()),
    ]
}
fn events(wire: &[u8]) -> Vec<Value> {
    String::from_utf8(wire.to_vec())
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect()
}
#[test]
fn matrix_16_external_text_requests_responses_streams() {
    let cs = codecs();
    let reqs = [
        json!({"model":"m","messages":[{"role":"user","content":"你好"}]}),
        json!({"model":"m","max_tokens":100,"messages":[{"role":"user","content":"你好"}]}),
        json!({"model":"m","input":[{"role":"user","content":"你好"}]}),
        json!({"model":"m","contents":[{"role":"user","parts":[{"text":"你好"}]}]}),
    ];
    let resps = [
        json!({"id":"r","choices":[{"index":0,"message":{"role":"assistant","content":"答复"},"finish_reason":"stop"}]}),
        json!({"id":"r","type":"message","role":"assistant","content":[{"type":"text","text":"答复"}],"stop_reason":"end_turn"}),
        json!({"id":"r","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"答复"}]}]}),
        json!({"responseId":"r","candidates":[{"content":{"role":"model","parts":[{"text":"答复"}]},"finishReason":"STOP"}]}),
    ];
    let streams = [
        vec![
            r#"data: {"id":"r","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"答复"},"finish_reason":null}]}"#,
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "data: [DONE]",
        ],
        vec![
            r#"data: {"type":"message_start","message":{"id":"r"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"答复"}}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            r#"data: {"type":"message_stop"}"#,
        ],
        vec![
            r#"data: {"type":"response.created","response":{"id":"r"}}"#,
            r#"data: {"type":"response.output_text.delta","delta":"答复"}"#,
            r#"data: {"type":"response.completed","response":{"id":"r","status":"completed"}}"#,
        ],
        vec![
            r#"data: {"responseId":"r","candidates":[{"content":{"parts":[{"text":"答复"}]},"finishReason":"STOP"}]}"#,
        ],
    ];
    for (i, src) in cs.iter().enumerate() {
        for (j, dst) in cs.iter().enumerate() {
            let ir = src.decode_request(reqs[i].clone(), &env()).unwrap();
            let (out, _) = dst.encode_request(&ir).unwrap();
            assert!(out.to_string().contains("你好"), "request {i}->{j}: {out}");
            let ir = src.decode_response(resps[i].clone()).unwrap();
            let out = dst.encode_response(&ir).unwrap();
            assert!(out.to_string().contains("答复"), "response {i}->{j}: {out}");
            let mut dec = src.stream_decoder();
            let mut enc = dst.stream_encoder();
            let mut out = Vec::new();
            for l in &streams[i] {
                for p in dec.feed(l).unwrap() {
                    out.extend(enc.encode_part(&p).unwrap());
                }
            }
            for p in dec.finish().unwrap() {
                out.extend(enc.encode_part(&p).unwrap());
            }
            let es = events(&out);
            let text: String = es
                .iter()
                .filter_map(|e| match j {
                    0 => e["choices"][0]["delta"]["content"].as_str(),
                    1 => {
                        if e["delta"]["type"] == "text_delta" {
                            e["delta"]["text"].as_str()
                        } else {
                            None
                        }
                    }
                    2 => {
                        if e["type"] == "response.output_text.delta" {
                            e["delta"].as_str()
                        } else {
                            None
                        }
                    }
                    _ => e["candidates"][0]["content"]["parts"][0]["text"].as_str(),
                })
                .collect();
            assert_eq!(text, "答复", "stream {i}->{j}");
            println!("matrix {i}->{j}: request/response/stream text PASS");
        }
    }
}
#[test]
fn chat_array_tool_result_preserves_reference() {
    let c = ChatCompletionsCodec::new();
    let dst = MessagesCodec::new();
    let ir=c.decode_request(json!({"model":"m","messages":[{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"f","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call_1","content":[{"type":"text","text":"result"}]}]}),&env()).unwrap();
    let (out, _) = dst.encode_request(&ir).unwrap();
    println!("{out}");
    assert_eq!(out["messages"][1]["content"][0]["tool_use_id"], "call_1");
}
#[test]
fn messages_redacted_thinking_replay_survives() {
    let c = MessagesCodec::new();
    let ir=c.decode_request(json!({"model":"m","messages":[{"role":"assistant","content":[{"type":"redacted_thinking","data":"opaque"},{"type":"text","text":"answer"}]}]}),&env()).unwrap();
    let (out, _) = c.encode_request(&ir).unwrap();
    println!("{out}");
    assert_eq!(
        out["messages"][0]["content"][0]["type"],
        "redacted_thinking"
    );
    assert_eq!(out["messages"][0]["content"][0]["data"], "opaque");
}
#[test]
fn messages_tool_error_flag_preserved_or_rejected() {
    let c = MessagesCodec::new();
    let ir=c.decode_request(json!({"model":"m","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"permission denied","is_error":true}]}]}),&env()).unwrap();
    let (out, _) = c.encode_request(&ir).unwrap();
    println!("{out}");
    assert_eq!(out["messages"][0]["content"][0]["is_error"], true);
}
#[test]
fn inline_audio_to_messages_guard_rejects() {
    let src = GeminiCodec::new();
    let dst = MessagesCodec::new();
    let ir=src.decode_request(json!({"model":"m","contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"audio/wav","data":"UklGRg=="}}]}]}),&env()).unwrap();
    let guard =
        tiygate_core::protocol::lossy::check_lossy_conversion(&ir, dst.id(), dst.capabilities());
    assert!(
        guard.is_err(),
        "audio must be rejected before it becomes an Anthropic image"
    );
}
#[test]
fn gemini_repeated_same_name_calls_have_unique_ids() {
    let c = GeminiCodec::new();
    let ir=c.decode_response(json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"f","args":{"x":1}}},{"functionCall":{"name":"f","args":{"x":2}}}]},"finishReason":"STOP"}]})).unwrap();
    let ids: Vec<_> = ir
        .content
        .iter()
        .filter_map(|c| {
            if let Content::ToolCall { id, .. } = c {
                Some(id)
            } else {
                None
            }
        })
        .collect();
    println!("ids={ids:?}");
    assert_ne!(ids[0], ids[1]);
}
#[test]
fn gemini_usage_before_finish_does_not_complete_early() {
    let c = GeminiCodec::new();
    let mut d = c.stream_decoder();
    let dst = ResponsesCodec::new();
    let mut e = dst.stream_encoder();
    let mut wire = Vec::new();
    for p in d.feed(r#"data: {"responseId":"r","candidates":[{"content":{"parts":[{"text":"first"}]}}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":1,"totalTokenCount":6}}"#).unwrap(){wire.extend(e.encode_part(&p).unwrap());}
    println!("{}", String::from_utf8_lossy(&wire));
    assert!(
        !events(&wire)
            .iter()
            .any(|e| e["type"] == "response.completed"),
        "usage is not a finish signal"
    );
}
#[test]
fn chat_error_followed_by_done_does_not_become_responses_success() {
    let c = ChatCompletionsCodec::new();
    let mut d = c.stream_decoder();
    let dst = ResponsesCodec::new();
    let mut e = dst.stream_encoder();
    let mut wire = Vec::new();
    for l in [
        r#"data: {"error":{"message":"boom","type":"server_error"}}"#,
        "data: [DONE]",
    ] {
        for p in d.feed(l).unwrap() {
            wire.extend(e.encode_part(&p).unwrap());
        }
    }
    println!("{}", String::from_utf8_lossy(&wire));
    assert!(!events(&wire)
        .iter()
        .any(|e| e["type"] == "response.completed" && e["response"]["status"] == "completed"));
}
#[test]
fn responses_incomplete_preserves_usage() {
    let c = ResponsesCodec::new();
    let mut d = c.stream_decoder();
    let parts=d.feed(r#"data: {"type":"response.incomplete","response":{"id":"r","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#).unwrap();
    println!("{parts:?}");
    assert!(parts
        .iter()
        .any(|p| matches!(p,StreamPart::Usage{usage} if usage.total_tokens==15)));
}
#[test]
fn chat_interleaved_tools_to_messages_keep_argument_identity() {
    let mut d = ChatCompletionsCodec::new().stream_decoder();
    let mut e = MessagesCodec::new().stream_encoder();
    let mut wire = Vec::new();
    for l in [
        r#"data: {"id":"r","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"f","arguments":""}},{"index":1,"id":"b","function":{"name":"g","arguments":""}}]}}]}"#,
        r#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"x\":1}"}},{"index":1,"function":{"arguments":"{\"y\":2}"}}]}}]}"#,
    ] {
        for p in d.feed(l).unwrap() {
            wire.extend(e.encode_part(&p).unwrap());
        }
    }
    let es = events(&wire);
    println!("{}", String::from_utf8_lossy(&wire));
    let idx = es.iter().find(|e| e["content_block"]["id"] == "a").unwrap()["index"].clone();
    let delta = es
        .iter()
        .find(|e| e["delta"]["partial_json"] == "{\"x\":1}")
        .unwrap();
    assert_eq!(delta["index"], idx, "a arguments must not go to b");
}
#[test]
fn gemini_request_signature_replayed_verbatim() {
    let c = GeminiCodec::new();
    let ir=c.decode_request(json!({"model":"gemini-3-pro","contents":[{"role":"model","parts":[{"functionCall":{"id":"a","name":"f","args":{}},"thoughtSignature":"provider_sig"}]}]}),&env()).unwrap();
    let (out, _) = c.encode_request(&ir).unwrap();
    println!("{out}");
    assert_eq!(
        out["contents"][0]["parts"][0]["thoughtSignature"],
        "provider_sig"
    );
}
#[test]
fn gemini_multiple_candidates_not_concatenated() {
    let c = GeminiCodec::new();
    let result=c.decode_response(json!({"candidates":[{"index":0,"content":{"parts":[{"text":"alternative A"}]},"finishReason":"STOP"},{"index":1,"content":{"parts":[{"text":"alternative B"}]},"finishReason":"STOP"}]}));
    if let Ok(ir) = result {
        let out = ChatCompletionsCodec::new().encode_response(&ir).unwrap();
        println!("{out}");
        let s = out["choices"][0]["message"]["content"].as_str().unwrap();
        assert!(
            !(s.contains("alternative A") && s.contains("alternative B")),
            "alternative answers must not be merged"
        );
    }
}
#[test]
fn responses_nested_refusal_is_preserved() {
    let c = ResponsesCodec::new();
    let ir=c.decode_response(json!({"id":"r","status":"completed","output":[{"id":"msg","type":"message","role":"assistant","content":[{"type":"refusal","refusal":"I cannot assist"}]}]})).unwrap();
    let out = ChatCompletionsCodec::new().encode_response(&ir).unwrap();
    println!("{out}");
    assert_eq!(out["choices"][0]["message"]["refusal"], "I cannot assist");
}
#[test]
fn gemini_response_json_schema_does_not_degrade_to_json_object() {
    let src = GeminiCodec::new();
    let dst = ChatCompletionsCodec::new();
    let ir=src.decode_request(json!({"model":"m","contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"responseMimeType":"application/json","responseJsonSchema":{"type":"object","properties":{"x":{"type":"integer","minimum":10}},"required":["x"]}}}),&env()).unwrap();
    let (out, _) = dst.encode_request(&ir).unwrap();
    println!("{out}");
    assert_eq!(out["response_format"]["type"], "json_schema");
}
#[test]
fn gemini_camelcase_system_instruction_survives_conversion() {
    let src = GeminiCodec::new();
    let dst = ChatCompletionsCodec::new();
    let ir=src.decode_request(json!({"model":"m","systemInstruction":{"parts":[{"text":"system-rule-marker"}]},"contents":[{"role":"user","parts":[{"text":"hi"}]}]}),&env()).unwrap();
    let (out, _) = dst.encode_request(&ir).unwrap();
    println!("{out}");
    assert!(out.to_string().contains("system-rule-marker"));
}
#[test]
fn gemini_native_function_ids_survive_reencode() {
    let c = GeminiCodec::new();
    let ir=c.decode_request(json!({"model":"gemini-2.5-pro","contents":[{"role":"model","parts":[{"functionCall":{"id":"a","name":"f","args":{}}}]},{"role":"user","parts":[{"functionResponse":{"id":"a","name":"f","response":{"ok":true}}}]}]}),&env()).unwrap();
    let (out, _) = c.encode_request(&ir).unwrap();
    println!("{out}");
    assert_eq!(out["contents"][0]["parts"][0]["functionCall"]["id"], "a");
    assert_eq!(
        out["contents"][1]["parts"][0]["functionResponse"]["id"],
        "a"
    );
}
#[test]
fn gemini_allowlist_keeps_all_allowed_tools_or_rejects() {
    let src = GeminiCodec::new();
    let dst = ChatCompletionsCodec::new();
    let ir=src.decode_request(json!({"model":"m","contents":[{"role":"user","parts":[{"text":"hi"}]}],"tools":[{"functionDeclarations":[{"name":"f","parameters":{"type":"object"}},{"name":"g","parameters":{"type":"object"}},{"name":"h","parameters":{"type":"object"}}]}],"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["f","g"]}}}),&env()).unwrap();
    let guard =
        tiygate_core::protocol::lossy::check_lossy_conversion(&ir, dst.id(), dst.capabilities());
    if guard.is_err() {
        return;
    }
    let (out, _) = dst.encode_request(&ir).unwrap();
    println!("{out}");
    let choice = &out["tool_choice"];
    let allowed = out["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(
        choice == "required" && allowed == vec!["f", "g"],
        "required f/g allowlist must survive; missing choice or retaining h weakens the contract"
    );
}
#[test]
fn gemini_thinking_usage_maps_to_openai_total_output() {
    let c = GeminiCodec::new();
    let ir=c.decode_response(json!({"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":10,"thoughtsTokenCount":20,"totalTokenCount":50}})).unwrap();
    let out = ChatCompletionsCodec::new().encode_response(&ir).unwrap();
    println!("{out}");
    assert_eq!(
        out["usage"]["completion_tokens"], 30,
        "OpenAI output includes reasoning tokens"
    );
    assert_eq!(out["usage"]["total_tokens"], 50);
}
#[test]
fn gemini_schema_constraints_preserved_or_rejected() {
    let src = ChatCompletionsCodec::new();
    let dst = GeminiCodec::new();
    let ir=src.decode_request(json!({"model":"gemini-2.5-pro","messages":[{"role":"user","content":"answer"}],"response_format":{"type":"json_schema","json_schema":{"name":"bounded","strict":true,"schema":{"type":"object","properties":{"x":{"type":"integer","minimum":10,"maximum":20}},"required":["x"],"additionalProperties":false}}}}),&env()).unwrap();
    let guard =
        tiygate_core::protocol::lossy::check_lossy_conversion(&ir, dst.id(), dst.capabilities());
    if guard.is_err() {
        return;
    }
    let (out, _) = dst.encode_request(&ir).unwrap();
    println!("guard={guard:?} out={out}");
    let schema = out["generationConfig"]
        .get("responseJsonSchema")
        .or_else(|| out["generationConfig"].get("responseSchema"))
        .unwrap();
    assert_eq!(schema["properties"]["x"]["minimum"], 10);
    assert_eq!(schema["properties"]["x"]["maximum"], 20);
    assert_eq!(schema["additionalProperties"], false);
}
#[test]
fn sse_data_without_optional_space_is_accepted() {
    let mut rejected = Vec::new();
    for c in codecs() {
        let mut d = c.stream_decoder();
        let l = match c.id().suite {
            tiygate_core::ProtocolSuite::OpenAiCompatible => {
                r#"data:{"choices":[{"delta":{"content":"hi"}}]}"#
            }
            tiygate_core::ProtocolSuite::AnthropicMessages => {
                r#"data:{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#
            }
            tiygate_core::ProtocolSuite::OpenAiResponses => {
                r#"data:{"type":"response.output_text.delta","delta":"hi"}"#
            }
            _ => r#"data:{"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#,
        };
        let parts = d.feed(l).unwrap();
        if !parts
            .iter()
            .any(|p| matches!(p,StreamPart::TextDelta{text} if text=="hi"))
        {
            rejected.push(c.id().to_string());
        }
    }
    println!("rejected valid no-space SSE: {rejected:?}");
    assert!(rejected.is_empty());
}

#[test]
fn error_tool_results_keep_failure_semantics_or_reject() {
    let src = MessagesCodec::new();
    let ir=src.decode_request(json!({"model":"m","messages":[{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"f","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"denied","is_error":true}]}]}),&env()).unwrap();
    let chat = ChatCompletionsCodec::new();
    assert!(tiygate_core::protocol::lossy::check_lossy_conversion(
        &ir,
        chat.id(),
        chat.capabilities()
    )
    .is_err());
    let gemini = GeminiCodec::new();
    let (out, _) = gemini.encode_request(&ir).unwrap();
    assert_eq!(
        out["contents"][1]["parts"][0]["functionResponse"]["response"]["error"],
        "denied"
    );
}

#[test]
fn gemini_unrepresentable_schema_is_rejected_not_weakened() {
    let src = ChatCompletionsCodec::new();
    let ir=src.decode_request(json!({"model":"m","messages":[{"role":"user","content":"hi"}],"response_format":{"type":"json_schema","json_schema":{"name":"x","schema":{"type":"object","properties":{"x":{"type":"integer","multipleOf":3}}}}}}),&env()).unwrap();
    assert!(GeminiCodec::new().encode_request(&ir).is_err());
}

#[test]
fn gemini_required_empty_allowlist_is_rejected() {
    assert!(GeminiCodec::new().decode_request(json!({"model":"m","tools":[{"functionDeclarations":[{"name":"f"}]}],"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["missing"]}}}),&env()).is_err());
}

#[test]
fn gemini_tool_schema_rejects_unsupported_constraints() {
    let src = ChatCompletionsCodec::new();
    let ir=src.decode_request(json!({"model":"m","tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object","properties":{"x":{"type":"integer","multipleOf":2}}}}}],"messages":[{"role":"user","content":"hi"}]}),&env()).unwrap();
    assert!(GeminiCodec::new().encode_request(&ir).is_err());
}

#[test]
fn gemini_stream_partial_arguments_are_buffered_and_malformed_is_error() {
    let mut encoder = GeminiCodec::new().stream_encoder();
    for (name, args) in [(Some("f".to_string()), ""), (None, "{\"x\":")] {
        assert!(encoder
            .encode_part(&StreamPart::ToolCallDelta {
                id: "a".into(),
                name,
                arguments: args.into(),
                wire_type: None,
                item_id: None,
                caller: None
            })
            .unwrap()
            .is_empty());
    }
    assert!(encoder
        .encode_part(&StreamPart::Finish {
            reason: tiygate_core::FinishReason::ToolCalls
        })
        .is_err());
}

#[test]
fn synthesized_ids_do_not_collide_with_name_suffixes() {
    let ir=GeminiCodec::new().decode_response(json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"f","args":{}}},{"functionCall":{"name":"f","args":{}}},{"functionCall":{"name":"f_1","args":{}}}]},"finishReason":"STOP"}]})).unwrap();
    let ids: std::collections::HashSet<_> = ir
        .content
        .iter()
        .filter_map(|c| {
            if let Content::ToolCall { id, .. } = c {
                Some(id)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(ids.len(), 3);
}

#[test]
fn no_space_done_preserves_termination() {
    let mut decoder = ChatCompletionsCodec::new().stream_decoder();
    assert!(decoder
        .feed("data:[DONE]")
        .unwrap()
        .iter()
        .any(|p| matches!(p, StreamPart::ResponseCompleted { .. })));
}

#[test]
fn gemini_error_object_is_not_double_wrapped() {
    let codec = GeminiCodec::new();
    let ir=codec.decode_request(json!({"model":"m","contents":[{"role":"model","parts":[{"functionCall":{"id":"a","name":"f","args":{}}}]},{"role":"user","parts":[{"functionResponse":{"id":"a","name":"f","response":{"error":{"code":"denied"}}}}]}]}),&env()).unwrap();
    let (out, _) = codec.encode_request(&ir).unwrap();
    assert_eq!(
        out["contents"][1]["parts"][0]["functionResponse"]["response"],
        json!({"error":{"code":"denied"}})
    );
}
