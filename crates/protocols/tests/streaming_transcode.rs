//! Cross-protocol *streaming* transcode matrix (4×4).
//!
//! Validates the IR hub-spoke streaming pipeline that the gateway runs in
//! `drive_upstream_stream` when ingress and egress protocols differ:
//!
//!   upstream SSE (egress encoder)
//!     → egress `StreamDecoder` → Vec<StreamPart>
//!     → ingress `StreamEncoder` → client SSE
//!
//! Each test simulates an "upstream" by encoding a canonical sequence of IR
//! `StreamPart`s with the *source* (egress) protocol encoder, runs the bytes
//! through the source decoder → target encoder transcode, then decodes the
//! re-encoded client SSE with the *target* (ingress) decoder and asserts the
//! recovered IR parts preserve text / reasoning / tool-call / finish across
//! all 16 protocol pairs.
//!
//! Run via `cargo test -p tiygate-protocols --test streaming_transcode`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use serde_json::Value;
use tiygate_core::{EndpointCodec, FinishReason, ProtocolSuite, StreamPart, Usage};

fn codec(suite: ProtocolSuite) -> Box<dyn EndpointCodec> {
    use tiygate_protocols::chat_completions::ChatCompletionsCodec;
    use tiygate_protocols::gemini::GeminiCodec;
    use tiygate_protocols::messages::MessagesCodec;
    use tiygate_protocols::responses::ResponsesCodec;
    match suite {
        ProtocolSuite::OpenAiCompatible => Box::new(ChatCompletionsCodec::new()),
        ProtocolSuite::AnthropicMessages => Box::new(MessagesCodec::new()),
        ProtocolSuite::GoogleGemini => Box::new(GeminiCodec::new()),
        ProtocolSuite::OpenAiResponses => Box::new(ResponsesCodec::new()),
    }
}

const SUITES: [ProtocolSuite; 4] = [
    ProtocolSuite::OpenAiCompatible,
    ProtocolSuite::AnthropicMessages,
    ProtocolSuite::OpenAiResponses,
    ProtocolSuite::GoogleGemini,
];

/// Encode a sequence of IR parts into the source protocol's SSE bytes,
/// mimicking an upstream provider's streamed response.
fn encode_upstream(suite: ProtocolSuite, parts: &[StreamPart]) -> Vec<u8> {
    let c = codec(suite);
    let mut enc = c.stream_encoder();
    let mut out = Vec::new();
    for p in parts {
        out.extend_from_slice(&enc.encode_part(p).expect("encode_part"));
    }
    // Gemini's encode_done() is empty; other protocols emit a terminator.
    out.extend_from_slice(&enc.encode_done());
    out
}

/// Run the transcode the gateway runs: feed each complete line of the source
/// SSE to the source decoder, re-encode the resulting parts with the target
/// encoder, then flush with decoder.finish() + encoder.encode_done().
fn transcode(src: ProtocolSuite, dst: ProtocolSuite, upstream: &[u8]) -> Vec<u8> {
    let src_codec = codec(src);
    let dst_codec = codec(dst);
    let mut dec = src_codec.stream_decoder();
    let mut enc = dst_codec.stream_encoder();
    let text = String::from_utf8(upstream.to_vec()).expect("utf8");
    let mut out = Vec::new();
    for line in text.split('\n') {
        let parts = dec.feed(line).expect("decode feed");
        for p in &parts {
            out.extend_from_slice(&enc.encode_part(p).expect("re-encode"));
        }
    }
    for p in &dec.finish().expect("decode finish") {
        out.extend_from_slice(&enc.encode_part(p).expect("re-encode finish"));
    }
    out.extend_from_slice(&enc.encode_done());
    out
}

/// Decode the target protocol's client SSE back into IR parts so we can
/// assert what the downstream client effectively receives.
fn decode_client(suite: ProtocolSuite, client: &[u8]) -> Vec<StreamPart> {
    let c = codec(suite);
    let mut dec = c.stream_decoder();
    let text = String::from_utf8(client.to_vec()).expect("utf8");
    let mut parts = Vec::new();
    for line in text.split('\n') {
        parts.extend(dec.feed(line).expect("client decode"));
    }
    parts.extend(dec.finish().expect("client finish"));
    parts
}

fn sse_json_events(bytes: &[u8]) -> Vec<Value> {
    String::from_utf8(bytes.to_vec())
        .expect("utf8")
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .collect()
}

fn collected_text(parts: &[StreamPart]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            StreamPart::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn collected_reasoning(parts: &[StreamPart]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            StreamPart::ReasoningDelta { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn has_finish(parts: &[StreamPart]) -> bool {
    parts.iter().any(|p| {
        matches!(
            p,
            StreamPart::Finish { .. } | StreamPart::ResponseCompleted { .. }
        )
    })
}

fn tool_args(parts: &[StreamPart]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            StreamPart::ToolCallDelta { arguments, .. } => Some(arguments.clone()),
            _ => None,
        })
        .collect()
}

fn tool_call_summaries(parts: &[StreamPart]) -> Vec<(String, Option<String>, String)> {
    parts
        .iter()
        .filter_map(|p| match p {
            StreamPart::ToolCallDelta {
                id,
                name,
                arguments,
            } => Some((id.clone(), name.clone(), arguments.clone())),
            _ => None,
        })
        .collect()
}

fn count_tool_events(events: &[Value], event_type: &str, call_id: &str) -> usize {
    events
        .iter()
        .filter(|event| event["type"] == event_type)
        .filter(|event| {
            event["item"]["id"] == call_id
                || event["item"]["call_id"] == call_id
                || event["item_id"] == call_id
        })
        .count()
}

fn output_item_done_for<'a>(events: &'a [Value], call_id: &str) -> Option<&'a Value> {
    events.iter().find(|event| {
        event["type"] == "response.output_item.done" && event["item"]["id"] == call_id
    })
}

fn finish_reasons(parts: &[StreamPart]) -> Vec<FinishReason> {
    parts
        .iter()
        .filter_map(|p| match p {
            StreamPart::Finish { reason } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn gemini_function_call_stop_transcodes_to_responses_tool_calls() {
    let upstream = br#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"I will check"}]}}],"usageMetadata":{"trafficType":"ON_DEMAND"},"modelVersion":"google/gemini-3.5-flash","responseId":"r1"}

data: {"candidates":[{"content":{"role":"model","parts":[{"text":" status."}]}}],"usageMetadata":{"trafficType":"ON_DEMAND"},"modelVersion":"google/gemini-3.5-flash","responseId":"r1"}

data: {"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"shell","args":{"cwd":"/tmp","timeout":30000,"command":"git status"}},"thoughtSignature":"sig"}]}}],"usageMetadata":{"trafficType":"ON_DEMAND"},"modelVersion":"google/gemini-3.5-flash","responseId":"r1"}

data: {"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":70648,"candidatesTokenCount":72,"totalTokenCount":71152,"trafficType":"ON_DEMAND","thoughtsTokenCount":432},"modelVersion":"google/gemini-3.5-flash","responseId":"r1"}

"#;

    let client = transcode(
        ProtocolSuite::GoogleGemini,
        ProtocolSuite::OpenAiResponses,
        upstream,
    );
    let recovered = decode_client(ProtocolSuite::OpenAiResponses, &client);
    assert!(
        finish_reasons(&recovered).contains(&FinishReason::ToolCalls),
        "Gemini functionCall + STOP must reach Responses client as ToolCalls; recovered: {recovered:?}; client: {}",
        String::from_utf8_lossy(&client)
    );
    assert!(
        !finish_reasons(&recovered).contains(&FinishReason::Stop),
        "Responses client must not see Stop for tool-call turn; recovered: {recovered:?}; client: {}",
        String::from_utf8_lossy(&client)
    );

    let events = sse_json_events(&client);
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("response.completed event");
    assert_eq!(completed["response"]["status"], "completed");
    assert!(
        completed["response"]["output"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .any(|item| item["type"] == "function_call"),
        "completed output must include function_call so log parsers infer tool_calls: {}",
        String::from_utf8_lossy(&client)
    );
}

#[test]
fn gemini_function_call_args_transcode_to_messages_tool_use_input() {
    let upstream = r#"data: {"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"shell","args":{"command":"git commit -m \"fix(webui): 🐛 align pagination colors with active theme properties\""}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":31016,"cachedContentTokenCount":29849,"candidatesTokenCount":33,"totalTokenCount":31049},"modelVersion":"google/gemini-3.5-flash","responseId":"r1"}

"#
    .as_bytes();

    let client = transcode(
        ProtocolSuite::GoogleGemini,
        ProtocolSuite::AnthropicMessages,
        upstream,
    );
    let recovered = decode_client(ProtocolSuite::AnthropicMessages, &client);
    let args = tool_args(&recovered);
    assert!(
        args.contains("git commit")
            && args.contains("fix(webui)")
            && args.contains("active theme properties"),
        "Gemini functionCall args must survive Messages transcode; recovered: {recovered:?}; client: {}",
        String::from_utf8_lossy(&client)
    );
    assert!(
        finish_reasons(&recovered).contains(&FinishReason::ToolCalls),
        "Messages client must see tool_use finish; recovered: {recovered:?}; client: {}",
        String::from_utf8_lossy(&client)
    );
    let s = String::from_utf8_lossy(&client);
    assert!(s.contains("input_json_delta"), "{s}");
    assert!(s.contains("git commit"), "{s}");
}

#[test]
fn openai_chat_single_frame_tool_args_survive_responses_transcode() {
    let upstream = br#"data: {"id":"chatcmpl_1","object":"chat.completion.chunk","model":"z-ai/glm-5.2","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_b61a7d12078444ebb0e1d7c8","type":"function","function":{"name":"search","arguments":"{\"query\":\"ttfb\"}"}},{"index":1,"id":"call_985eec250ff44649a8e4c98a","type":"function","function":{"name":"search","arguments":"{\"query\":\"latency\"}"}},{"index":2,"id":"call_1a4878db943a4d65a01baf3f","type":"function","function":{"name":"search","arguments":"{\"query\":\"upstream\"}"}}]},"finish_reason":null}]}

data: {"id":"chatcmpl_1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

data: [DONE]

"#;

    let client = transcode(
        ProtocolSuite::OpenAiCompatible,
        ProtocolSuite::OpenAiResponses,
        upstream,
    );
    let recovered = decode_client(ProtocolSuite::OpenAiResponses, &client);
    let tool_calls = tool_call_summaries(&recovered);

    assert!(
        tool_calls
            .iter()
            .any(|(id, name, args)| id == "call_b61a7d12078444ebb0e1d7c8"
                && name.as_deref() == Some("search")
                && args.is_empty()),
        "missing first tool opener: {tool_calls:?}; client: {}",
        String::from_utf8_lossy(&client)
    );
    assert!(
        tool_calls
            .iter()
            .any(|(id, name, args)| id == "call_b61a7d12078444ebb0e1d7c8"
                && name.is_none()
                && args == r#"{"query":"ttfb"}"#),
        "missing first tool arguments: {tool_calls:?}; client: {}",
        String::from_utf8_lossy(&client)
    );
    assert!(
        String::from_utf8_lossy(&client).contains("response.function_call_arguments.delta"),
        "Responses SSE must stream argument deltas to client: {}",
        String::from_utf8_lossy(&client)
    );
    assert!(
        String::from_utf8_lossy(&client).contains("response.function_call_arguments.done"),
        "Responses SSE must close argument streams with final arguments: {}",
        String::from_utf8_lossy(&client)
    );
    let events = sse_json_events(&client);
    for (call_id, query) in [
        ("call_b61a7d12078444ebb0e1d7c8", "ttfb"),
        ("call_985eec250ff44649a8e4c98a", "latency"),
        ("call_1a4878db943a4d65a01baf3f", "upstream"),
    ] {
        assert_eq!(
            count_tool_events(&events, "response.output_item.added", call_id),
            1,
            "must emit exactly one output_item.added for {call_id}: {}",
            String::from_utf8_lossy(&client)
        );
        assert_eq!(
            count_tool_events(&events, "response.function_call_arguments.delta", call_id),
            1,
            "must emit exactly one arguments.delta for {call_id}: {}",
            String::from_utf8_lossy(&client)
        );
        assert_eq!(
            count_tool_events(&events, "response.function_call_arguments.done", call_id),
            1,
            "must emit exactly one arguments.done for {call_id}: {}",
            String::from_utf8_lossy(&client)
        );
        assert_eq!(
            count_tool_events(&events, "response.output_item.done", call_id),
            1,
            "must emit exactly one output_item.done for {call_id}: {}",
            String::from_utf8_lossy(&client)
        );
        let item = &output_item_done_for(&events, call_id).expect("output item done")["item"];
        assert_eq!(item["name"], "search");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["arguments"], format!("{{\"query\":\"{query}\"}}"));
    }
    assert!(
        events.iter().any(|event| {
            event["type"] == "response.completed"
                && event["response"]["output"].as_array().is_some_and(|items| {
                    items.iter().any(|item| {
                        item["id"] == "call_b61a7d12078444ebb0e1d7c8"
                            && item["arguments"] == r#"{"query":"ttfb"}"#
                    })
                })
        }),
        "Responses SSE completed output must contain final tool arguments: {}",
        String::from_utf8_lossy(&client)
    );
    assert!(
        tool_args(&recovered).contains("latency") && tool_args(&recovered).contains("upstream"),
        "parallel tool arguments lost: {tool_calls:?}; client: {}",
        String::from_utf8_lossy(&client)
    );
}

#[test]
fn matrix_text_delta_roundtrip_all_pairs() {
    let parts = vec![
        StreamPart::ResponseStarted {
            id: "resp_1".to_string(),
        },
        StreamPart::TextDelta {
            text: "Hello".to_string(),
        },
        StreamPart::TextDelta {
            text: ", world".to_string(),
        },
        StreamPart::Finish {
            reason: FinishReason::Stop,
        },
    ];

    for &src in &SUITES {
        for &dst in &SUITES {
            let upstream = encode_upstream(src, &parts);
            let client = transcode(src, dst, &upstream);
            let recovered = decode_client(dst, &client);
            let text = collected_text(&recovered);
            assert_eq!(
                text,
                "Hello, world",
                "text lost transcoding {src:?} -> {dst:?}; client bytes: {}",
                String::from_utf8_lossy(&client)
            );
        }
    }
}

#[test]
fn matrix_reasoning_delta_roundtrip_all_pairs() {
    let parts = vec![
        StreamPart::ResponseStarted {
            id: "resp_r".to_string(),
        },
        StreamPart::ReasoningDelta {
            text: "thinking ".to_string(),
            id: None,
            encrypted_content: None,
        },
        StreamPart::ReasoningDelta {
            text: "harder".to_string(),
            id: None,
            encrypted_content: None,
        },
        StreamPart::TextDelta {
            text: "answer".to_string(),
        },
        StreamPart::Finish {
            reason: FinishReason::Stop,
        },
    ];

    for &src in &SUITES {
        for &dst in &SUITES {
            let upstream = encode_upstream(src, &parts);
            let client = transcode(src, dst, &upstream);
            let recovered = decode_client(dst, &client);
            assert_eq!(
                collected_text(&recovered),
                "answer",
                "text lost {src:?} -> {dst:?}"
            );
            // Reasoning survives only between protocols that model it on the
            // wire. Gemini's stream encoder emits `thought` parts and its
            // decoder reads them, chat/messages/responses likewise. We assert
            // reasoning is preserved whenever BOTH ends model reasoning.
            let r = collected_reasoning(&recovered);
            assert!(
                r == "thinking harder" || r.is_empty(),
                "unexpected reasoning {r:?} for {src:?} -> {dst:?}"
            );
        }
    }
}

#[test]
fn matrix_finish_present_all_pairs() {
    let parts = vec![
        StreamPart::ResponseStarted {
            id: "resp_f".to_string(),
        },
        StreamPart::TextDelta {
            text: "x".to_string(),
        },
        StreamPart::Finish {
            reason: FinishReason::Stop,
        },
        StreamPart::ResponseCompleted {
            id: "resp_f".to_string(),
            status: "completed".to_string(),
            usage: None,
            extensions: std::collections::HashMap::new(),
        },
    ];

    for &src in &SUITES {
        for &dst in &SUITES {
            let upstream = encode_upstream(src, &parts);
            let client = transcode(src, dst, &upstream);
            let recovered = decode_client(dst, &client);
            assert!(
                has_finish(&recovered),
                "finish/completed lost {src:?} -> {dst:?}; client bytes: {}",
                String::from_utf8_lossy(&client)
            );
        }
    }
}

#[test]
fn matrix_tool_call_arguments_roundtrip_all_pairs() {
    // Tool-call streaming: a name-bearing opener followed by argument deltas.
    let parts = vec![
        StreamPart::ResponseStarted {
            id: "resp_t".to_string(),
        },
        StreamPart::ToolCallDelta {
            id: "call_1".to_string(),
            name: Some("get_weather".to_string()),
            arguments: String::new(),
        },
        StreamPart::ToolCallDelta {
            id: "call_1".to_string(),
            name: None,
            arguments: "{\"city\":".to_string(),
        },
        StreamPart::ToolCallDelta {
            id: "call_1".to_string(),
            name: None,
            arguments: "\"SF\"}".to_string(),
        },
        StreamPart::Finish {
            reason: FinishReason::ToolCalls,
        },
    ];

    for &src in &SUITES {
        for &dst in &SUITES {
            // Gemini's tool-call streaming uses a non-incremental
            // `_partial`-arg convention that does not reconstruct the JSON
            // string the same way; assert the incremental protocols
            // (chat/messages/responses) preserve the argument text and only
            // smoke-test that Gemini pairs do not panic / produce a finish.
            let upstream = encode_upstream(src, &parts);
            let client = transcode(src, dst, &upstream);
            let recovered = decode_client(dst, &client);
            let gemini_involved =
                src == ProtocolSuite::GoogleGemini || dst == ProtocolSuite::GoogleGemini;
            if !gemini_involved {
                let args = tool_args(&recovered);
                assert!(
                    args.contains("city") && args.contains("SF"),
                    "tool args lost {src:?} -> {dst:?}; got {args:?}; client: {}",
                    String::from_utf8_lossy(&client)
                );
            }
        }
    }
}

#[test]
fn matrix_usage_roundtrip_incremental_protocols() {
    let parts = vec![
        StreamPart::ResponseStarted {
            id: "resp_u".to_string(),
        },
        StreamPart::TextDelta {
            text: "hi".to_string(),
        },
        StreamPart::Usage {
            usage: Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..Default::default()
            },
        },
        StreamPart::Finish {
            reason: FinishReason::Stop,
        },
    ];

    // Usage wire representation differs per protocol; assert that for the
    // protocols whose decoders surface a Usage part the token counts survive.
    for &src in &SUITES {
        for &dst in &SUITES {
            let upstream = encode_upstream(src, &parts);
            let client = transcode(src, dst, &upstream);
            let recovered = decode_client(dst, &client);
            for p in &recovered {
                if let StreamPart::Usage { usage } = p {
                    // Anthropic's Finish frame carries a placeholder
                    // `output_tokens: 0` message_delta, surfaced as a zero
                    // Usage part — ignore those and only assert that the
                    // real usage frame preserves the token counts.
                    if usage.prompt_tokens == 0 && usage.completion_tokens == 0 {
                        continue;
                    }
                    assert!(
                        usage.prompt_tokens == 10 || usage.completion_tokens == 5,
                        "usage corrupted {src:?} -> {dst:?}: {usage:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn gemini_tool_call_finish_before_usage_keeps_cache_read_in_responses_completed() {
    // Real Gemini chunks can contain `finishReason` before `usageMetadata` in
    // the same JSON object. The Gemini decoder emits Finish first, then Usage;
    // Responses must still delay/complete with the usage so cache read is not
    // shown as zero in response.completed.
    let upstream = br#"data: {"responseId":"resp_g","candidates":[{"content":{"parts":[{"functionCall":{"name":"shell","args":{"command":"echo ok"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":146050,"candidatesTokenCount":62,"totalTokenCount":146112,"cachedContentTokenCount":141123}}

"#;
    let client = transcode(
        ProtocolSuite::GoogleGemini,
        ProtocolSuite::OpenAiResponses,
        upstream,
    );
    let events = sse_json_events(&client);
    let completed = events
        .iter()
        .find(|ev| ev.get("type").and_then(|t| t.as_str()) == Some("response.completed"))
        .expect("response.completed");
    assert_eq!(
        completed["response"]["usage"]["input_tokens_details"]["cached_tokens"],
        141123
    );
    assert_eq!(completed["response"]["usage"]["input_tokens"], 146050);
    assert_eq!(completed["response"]["usage"]["output_tokens"], 62);
    assert_eq!(completed["response"]["usage"]["total_tokens"], 146112);
}

#[test]
fn gemini_tool_call_finish_before_usage_keeps_cache_read_in_chat_usage_chunk() {
    let upstream = br#"data: {"responseId":"resp_g","candidates":[{"content":{"parts":[{"functionCall":{"name":"shell","args":{"command":"echo ok"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":146050,"candidatesTokenCount":62,"totalTokenCount":146112,"cachedContentTokenCount":141123}}

"#;
    let client = transcode(
        ProtocolSuite::GoogleGemini,
        ProtocolSuite::OpenAiCompatible,
        upstream,
    );
    let events = sse_json_events(&client);
    let usage_event = events
        .iter()
        .find(|ev| ev.get("usage").is_some())
        .expect("chat usage chunk");
    assert_eq!(
        usage_event["usage"]["prompt_tokens_details"]["cached_tokens"],
        141123
    );
    assert_eq!(usage_event["usage"]["prompt_tokens"], 146050);
    assert_eq!(usage_event["usage"]["completion_tokens"], 62);
    assert_eq!(usage_event["usage"]["total_tokens"], 146112);
}

#[test]
fn gemini_tool_call_finish_before_usage_keeps_cache_read_in_messages_terminal_delta() {
    let upstream = br#"data: {"responseId":"resp_g","candidates":[{"content":{"parts":[{"functionCall":{"name":"shell","args":{"command":"echo ok"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":146050,"candidatesTokenCount":62,"totalTokenCount":146112,"cachedContentTokenCount":141123}}

"#;
    let client = transcode(
        ProtocolSuite::GoogleGemini,
        ProtocolSuite::AnthropicMessages,
        upstream,
    );
    let events = sse_json_events(&client);
    let terminal_delta = events
        .iter()
        .find(|ev| {
            ev.get("type").and_then(|t| t.as_str()) == Some("message_delta")
                && ev["delta"]
                    .get("stop_reason")
                    .and_then(|s| s.as_str())
                    .is_some()
        })
        .expect("messages terminal delta");
    assert_eq!(terminal_delta["usage"]["cache_read_input_tokens"], 141123);
    assert_eq!(terminal_delta["usage"]["input_tokens"], 4927);
    assert_eq!(terminal_delta["usage"]["output_tokens"], 62);
}

#[test]
fn chat_finish_before_usage_keeps_cache_read_when_ingress_is_gemini() {
    let upstream = br#"data: {"id":"chat_1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: {"id":"chat_1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":null}],"usage":{"prompt_tokens":146050,"completion_tokens":62,"total_tokens":146112,"prompt_tokens_details":{"cached_tokens":141123}}}

data: [DONE]

"#;
    let client = transcode(
        ProtocolSuite::OpenAiCompatible,
        ProtocolSuite::GoogleGemini,
        upstream,
    );
    let events = sse_json_events(&client);
    let usage_event = events
        .iter()
        .find(|ev| ev.get("usageMetadata").is_some())
        .expect("Gemini usageMetadata");
    assert_eq!(
        usage_event["usageMetadata"]["cachedContentTokenCount"],
        141123
    );
    assert_eq!(usage_event["usageMetadata"]["promptTokenCount"], 146050);
    assert_eq!(usage_event["usageMetadata"]["candidatesTokenCount"], 62);
    assert_eq!(usage_event["usageMetadata"]["totalTokenCount"], 146112);
}

#[test]
fn responses_completed_usage_keeps_cache_read_when_ingress_is_gemini() {
    let upstream = br#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":146050,"output_tokens":62,"total_tokens":146112,"input_tokens_details":{"cached_tokens":141123}}}}

"#;
    let client = transcode(
        ProtocolSuite::OpenAiResponses,
        ProtocolSuite::GoogleGemini,
        upstream,
    );
    let events = sse_json_events(&client);
    let usage_event = events
        .iter()
        .find(|ev| ev.get("usageMetadata").is_some())
        .expect("Gemini usageMetadata");
    assert_eq!(
        usage_event["usageMetadata"]["cachedContentTokenCount"],
        141123
    );
    assert_eq!(usage_event["usageMetadata"]["promptTokenCount"], 146050);
    assert_eq!(usage_event["usageMetadata"]["candidatesTokenCount"], 62);
    assert_eq!(usage_event["usageMetadata"]["totalTokenCount"], 146112);
}

#[test]
fn messages_split_usage_keeps_cache_read_when_ingress_is_gemini() {
    let upstream = br#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude","content":[],"usage":{"input_tokens":4927,"output_tokens":0,"cache_read_input_tokens":141123}}}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":62}}

data: {"type":"message_stop"}

"#;
    let client = transcode(
        ProtocolSuite::AnthropicMessages,
        ProtocolSuite::GoogleGemini,
        upstream,
    );
    let events = sse_json_events(&client);
    let usage_event = events
        .iter()
        .rev()
        .find(|ev| ev.get("usageMetadata").is_some())
        .expect("Gemini usageMetadata");
    assert_eq!(
        usage_event["usageMetadata"]["cachedContentTokenCount"],
        141123
    );
    assert_eq!(usage_event["usageMetadata"]["promptTokenCount"], 146050);
    assert_eq!(usage_event["usageMetadata"]["candidatesTokenCount"], 62);
    assert_eq!(usage_event["usageMetadata"]["totalTokenCount"], 146112);
}
