//! Regression tests for the N×N protocol-conversion bug fixes.
//!
//! Covers four classes of defect that the earlier implementation had:
//!   1. Streaming decoders aborting on benign/unknown SSE events.
//!   2. Cache tokens being double-counted on decode→encode round-trips
//!      (OpenAI/Responses/Gemini `prompt_tokens` already includes cache).
//!   3. Streaming tool-call `index` collapsing to 0 for parallel calls.
//!   4. Anthropic streaming output missing content_block_start/stop framing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::Value;
use tiygate_core::{EndpointCodec, StreamDecoder, StreamEncoder, StreamPart};
use tiygate_protocols::chat_completions::{
    ChatCompletionsCodec, ChatCompletionsStreamDecoder, ChatCompletionsStreamEncoder,
};
use tiygate_protocols::gemini::{GeminiCodec, GeminiStreamDecoder, GeminiStreamEncoder};
use tiygate_protocols::messages::{MessagesCodec, MessagesStreamDecoder, MessagesStreamEncoder};
use tiygate_protocols::responses::{ResponsesCodec, ResponsesStreamDecoder};

// =============================================================================
// 1. Streaming decoders must ignore benign / unknown events, not error out.
// =============================================================================

#[test]
fn anthropic_decoder_ignores_ping_event() {
    let mut dec = MessagesStreamDecoder::new();
    let parts = dec.feed("data: {\"type\":\"ping\"}").unwrap();
    assert!(
        parts.is_empty(),
        "Anthropic `ping` must be ignored, got: {:?}",
        parts
    );
    // And no error variant is produced for an unknown event type either.
    let parts = dec.feed("data: {\"type\":\"some_future_event\"}").unwrap();
    assert!(parts.iter().all(|p| !matches!(p, StreamPart::Error { .. })));
}

#[test]
fn responses_decoder_handles_full_lifecycle_without_errors() {
    let mut dec = ResponsesStreamDecoder::new();
    // A realistic interleaving of Responses lifecycle events that the old
    // decoder turned into error frames.
    let lines = [
        r#"data: {"type":"response.created","response":{"id":"resp_1"}}"#,
        r#"data: {"type":"response.in_progress","response":{"id":"resp_1"}}"#,
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m1"}}"#,
        r#"data: {"type":"response.content_part.added","output_index":0,"content_index":0}"#,
        r#"data: {"type":"response.output_text.delta","delta":"Hello"}"#,
        r#"data: {"type":"response.output_text.done","output_index":0}"#,
        r#"data: {"type":"response.content_part.done","output_index":0}"#,
        r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#,
    ];
    let mut text = String::new();
    for line in lines {
        for part in dec.feed(line).unwrap() {
            match part {
                StreamPart::Error { message, .. } => {
                    panic!("unexpected error frame: {message}")
                }
                StreamPart::TextDelta { text: t } => text.push_str(&t),
                _ => {}
            }
        }
    }
    assert_eq!(text, "Hello");
}

#[test]
fn responses_decoder_maps_failed_event_to_error() {
    let mut dec = ResponsesStreamDecoder::new();
    let line = r#"data: {"type":"response.failed","response":{"error":{"message":"boom","type":"server_error"}}}"#;
    let parts = dec.feed(line).unwrap();
    assert!(parts.iter().any(|p| matches!(
        p,
        StreamPart::Error { message, .. } if message == "boom"
    )));
}

// =============================================================================
// 2. Cache tokens must not be double-counted on decode→encode round-trips.
// =============================================================================

fn chat_codec() -> ChatCompletionsCodec {
    ChatCompletionsCodec::new()
}

#[test]
fn chat_decode_then_encode_does_not_double_count_cache() {
    // Upstream OpenAI: prompt_tokens (1000) already INCLUDES cached (800).
    let codec = chat_codec();
    let body: Value = serde_json::json!({
        "id": "x",
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "total_tokens": 1050,
            "prompt_tokens_details": {"cached_tokens": 800}
        }
    });
    let ir = codec.decode_response(body).unwrap();
    let u = ir.usage.as_ref().unwrap();
    // IR keeps prompt cache-free: 1000 - 800 = 200.
    assert_eq!(u.prompt_tokens, 200);
    assert_eq!(u.cache_read_tokens, Some(800));
    // Re-encoding to chat must reconstruct the original 1000, not 1800.
    let encoded = codec.encode_response(&ir).unwrap();
    assert_eq!(encoded["usage"]["prompt_tokens"], 1000);
    assert_eq!(
        encoded["usage"]["prompt_tokens_details"]["cached_tokens"],
        800
    );
}

#[test]
fn responses_decode_then_encode_does_not_double_count_cache() {
    let codec = ResponsesCodec::new();
    let body: Value = serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "output": [],
        "status": "completed",
        "usage": {
            "input_tokens": 1000,
            "output_tokens": 50,
            "total_tokens": 1050,
            "input_tokens_details": {"cached_tokens": 800}
        }
    });
    let ir = codec.decode_response(body).unwrap();
    assert_eq!(ir.usage.as_ref().unwrap().prompt_tokens, 200);
    let encoded = codec.encode_response(&ir).unwrap();
    assert_eq!(encoded["usage"]["input_tokens"], 1000);
}

#[test]
fn gemini_decode_then_encode_does_not_double_count_cache() {
    let codec = GeminiCodec::new();
    let body: Value = serde_json::json!({
        "candidates": [{"content": {"role": "model", "parts": [{"text": "ok"}]}, "finishReason": "STOP"}],
        "usageMetadata": {
            "promptTokenCount": 1000,
            "candidatesTokenCount": 50,
            "totalTokenCount": 1050,
            "cachedContentTokenCount": 800
        }
    });
    let ir = codec.decode_response(body).unwrap();
    assert_eq!(ir.usage.as_ref().unwrap().prompt_tokens, 200);
    let encoded = codec.encode_response(&ir).unwrap();
    // Non-streaming encoder now re-adds cache_read to promptTokenCount
    // (consistent with the streaming encoder and Gemini's wire convention).
    assert_eq!(encoded["usageMetadata"]["promptTokenCount"], 1000);
    assert_eq!(encoded["usageMetadata"]["totalTokenCount"], 1050);
    assert_eq!(encoded["usageMetadata"]["cachedContentTokenCount"], 800);
}

#[test]
fn chat_to_responses_cache_roundtrip_is_lossless() {
    // chat (cache included in prompt) → IR → responses (cache re-added).
    let chat = chat_codec();
    let responses = ResponsesCodec::new();
    let body: Value = serde_json::json!({
        "id": "x",
        "object": "chat.completion",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "total_tokens": 1050,
            "prompt_tokens_details": {"cached_tokens": 800}
        }
    });
    let ir = chat.decode_response(body).unwrap();
    let encoded = responses.encode_response(&ir).unwrap();
    // The Responses input_tokens must equal the original 1000, not 1800.
    assert_eq!(encoded["usage"]["input_tokens"], 1000);
    assert_eq!(
        encoded["usage"]["input_tokens_details"]["cached_tokens"],
        800
    );
}

// =============================================================================
// 3. Streaming tool-call index must be stable & distinct per call.
// =============================================================================

#[test]
fn chat_stream_encoder_assigns_distinct_tool_call_indices() {
    let mut enc = ChatCompletionsStreamEncoder::new();
    let _ = enc
        .encode_part(&StreamPart::ResponseStarted {
            id: "x".to_string(),
        })
        .unwrap();
    // Two distinct tool calls (different ids) must get index 0 and 1.
    let first = enc
        .encode_part(&StreamPart::ToolCallDelta {
            id: "call_a".to_string(),
            name: Some("f".to_string()),
            arguments: String::new(),
            wire_type: None,
            item_id: None,
            caller: None,
        })
        .unwrap();
    let second = enc
        .encode_part(&StreamPart::ToolCallDelta {
            id: "call_b".to_string(),
            name: Some("g".to_string()),
            arguments: String::new(),
            wire_type: None,
            item_id: None,
            caller: None,
        })
        .unwrap();
    let s1 = String::from_utf8_lossy(&first);
    let s2 = String::from_utf8_lossy(&second);
    assert!(
        s1.contains("\"index\":0"),
        "first call should be index 0: {s1}"
    );
    assert!(
        s2.contains("\"index\":1"),
        "second call should be index 1: {s2}"
    );

    // An argument fragment for call_b (empty id on some providers) appends to
    // the most-recent call (index 1).
    let frag = enc
        .encode_part(&StreamPart::ToolCallDelta {
            id: "call_b".to_string(),
            name: None,
            arguments: "{\"x\":1}".to_string(),
            wire_type: None,
            item_id: None,
            caller: None,
        })
        .unwrap();
    let s3 = String::from_utf8_lossy(&frag);
    assert!(
        s3.contains("\"index\":1"),
        "fragment should target index 1: {s3}"
    );
}

// =============================================================================
// 4. Anthropic stream encoder must emit content_block_start/stop framing.
// =============================================================================

#[test]
fn anthropic_stream_encoder_frames_blocks() {
    let mut enc = MessagesStreamEncoder::new();
    let mut sse = String::new();
    sse.push_str(&String::from_utf8_lossy(
        &enc.encode_part(&StreamPart::ResponseStarted {
            id: "msg_1".to_string(),
        })
        .unwrap(),
    ));
    // Text, then a tool call, then finish.
    sse.push_str(&String::from_utf8_lossy(
        &enc.encode_part(&StreamPart::TextDelta {
            text: "Hi".to_string(),
        })
        .unwrap(),
    ));
    sse.push_str(&String::from_utf8_lossy(
        &enc.encode_part(&StreamPart::ToolCallDelta {
            id: "call_a".to_string(),
            name: Some("f".to_string()),
            arguments: String::new(),
            wire_type: None,
            item_id: None,
            caller: None,
        })
        .unwrap(),
    ));
    sse.push_str(&String::from_utf8_lossy(
        &enc.encode_part(&StreamPart::ToolCallDelta {
            id: "call_a".to_string(),
            name: None,
            arguments: "{\"x\":1}".to_string(),
            wire_type: None,
            item_id: None,
            caller: None,
        })
        .unwrap(),
    ));
    sse.push_str(&String::from_utf8_lossy(
        &enc.encode_part(&StreamPart::Finish {
            reason: tiygate_core::FinishReason::ToolCalls,
        })
        .unwrap(),
    ));

    // The text block opens at index 0, the tool_use block at index 1, and both
    // are explicitly closed via content_block_stop.
    assert!(sse.contains("content_block_start"));
    assert!(sse.contains("\"index\":0"));
    assert!(sse.contains("\"index\":1"));
    assert!(sse.contains("content_block_stop"));
    // text_delta and input_json_delta target their respective blocks.
    assert!(sse.contains("text_delta"));
    assert!(sse.contains("input_json_delta"));
}

// =============================================================================
// 6. Streaming usage frames must stay self-consistent (prompt+completion=total)
//    and re-add cache that the decoder subtracted.
// =============================================================================

#[test]
fn chat_stream_usage_reincludes_cache_and_is_consistent() {
    let mut enc = ChatCompletionsStreamEncoder::new();
    // IR keeps prompt cache-free (200) with cache_read 800.
    let usage = tiygate_core::Usage {
        prompt_tokens: 200,
        completion_tokens: 50,
        total_tokens: 1050,
        reasoning_tokens: None,
        cache_read_tokens: Some(800),
        cache_write_tokens: None,
    };
    let bytes = enc.encode_part(&StreamPart::Usage { usage }).unwrap();
    let s = String::from_utf8_lossy(&bytes);
    // prompt_tokens must re-include cache (200+800=1000) and total stays 1050.
    assert!(s.contains("\"prompt_tokens\":1000"), "{s}");
    assert!(s.contains("\"total_tokens\":1050"), "{s}");
    assert!(s.contains("\"cached_tokens\":800"), "{s}");
}

#[test]
fn responses_stream_usage_reincludes_cache() {
    use tiygate_core::StreamEncoder as _;
    let mut enc = tiygate_protocols::responses::ResponsesStreamEncoder::new();
    let usage = tiygate_core::Usage {
        prompt_tokens: 200,
        completion_tokens: 50,
        total_tokens: 1050,
        reasoning_tokens: None,
        cache_read_tokens: Some(800),
        cache_write_tokens: None,
    };
    // Usage is now stashed and emitted inside the terminal response.completed
    // (emitting it early would terminate the stream prematurely). Feed Usage
    // then Finish and assert the completed frame carries the re-included cache.
    let _ = enc.encode_part(&StreamPart::Usage { usage }).unwrap();
    let bytes = enc
        .encode_part(&StreamPart::Finish {
            reason: tiygate_core::FinishReason::Stop,
        })
        .unwrap();
    let s = String::from_utf8_lossy(&bytes);
    assert!(s.contains("response.completed"), "{s}");
    assert!(s.contains("\"input_tokens\":1000"), "{s}");
    assert!(s.contains("\"total_tokens\":1050"), "{s}");
}

#[test]
fn chat_final_chunk_finish_and_usage_transcodes_to_responses_completed_usage() {
    use tiygate_core::StreamEncoder as _;
    let mut chat_dec = ChatCompletionsStreamDecoder::new();
    let mut responses_enc = tiygate_protocols::responses::ResponsesStreamEncoder::new();
    let lines = [
        r#"data: {"id":"chatcmpl_1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl_1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":24602,"completion_tokens":142,"total_tokens":24744,"prompt_tokens_details":{"cached_tokens":24320},"completion_tokens_details":{"reasoning_tokens":18}}}"#,
    ];

    let mut out = Vec::new();
    for line in lines {
        let parts = chat_dec.feed(line).unwrap();
        for part in &parts {
            out.extend_from_slice(&responses_enc.encode_part(part).unwrap());
        }
    }
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("\"type\":\"response.completed\""), "{s}");
    assert!(s.contains("\"usage\""), "{s}");
    assert!(s.contains("\"input_tokens\":24602"), "{s}");
    assert!(s.contains("\"output_tokens\":142"), "{s}");
    assert!(s.contains("\"total_tokens\":24744"), "{s}");
    assert!(s.contains("\"cached_tokens\":24320"), "{s}");
    assert!(s.contains("\"reasoning_tokens\":18"), "{s}");
}

#[test]
fn chat_finish_and_usage_in_separate_chunks_transcodes_to_responses_completed_usage() {
    // Regression: OpenAI-compatible providers (ZenMux/GLM) send finish_reason
    // and usage in SEPARATE chunks (finish chunk → usage chunk → [DONE]).
    // The Responses encoder must defer response.completed until the usage
    // chunk has been stashed, otherwise response.completed has no usage.
    use tiygate_core::StreamEncoder as _;
    let mut chat_dec = ChatCompletionsStreamDecoder::new();
    let mut responses_enc = tiygate_protocols::responses::ResponsesStreamEncoder::new();
    let lines = [
        r#"data: {"id":"chatcmpl_1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
        r#"data: {"id":"chatcmpl_1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        r#"data: {"id":"chatcmpl_1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{}}],"usage":{"prompt_tokens":51725,"completion_tokens":947,"total_tokens":52672,"prompt_tokens_details":{"cached_tokens":51392},"completion_tokens_details":{"reasoning_tokens":706}}}"#,
        "data: [DONE]",
    ];

    let mut out = Vec::new();
    for line in lines {
        let parts = chat_dec.feed(line).unwrap();
        for part in &parts {
            out.extend_from_slice(&responses_enc.encode_part(part).unwrap());
        }
    }
    let s = String::from_utf8_lossy(&out);
    assert!(s.contains("\"type\":\"response.completed\""), "{s}");
    assert!(s.contains("\"usage\""), "{s}");
    assert!(s.contains("\"input_tokens\":51725"), "{s}");
    assert!(s.contains("\"output_tokens\":947"), "{s}");
    assert!(s.contains("\"total_tokens\":52672"), "{s}");
    assert!(s.contains("\"cached_tokens\":51392"), "{s}");
    assert!(s.contains("\"reasoning_tokens\":706"), "{s}");
}

#[test]
fn anthropic_decode_tool_result_array_content() {
    let codec = MessagesCodec::new();
    let body: Value = serde_json::json!({
        "model": "claude",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "tu_1",
                "content": [{"type": "text", "text": "result-payload"}]
            }]
        }]
    });
    let env = tiygate_core::RawEnvelope {
        method: "POST".to_string(),
        path: "/v1/messages".to_string(),
        headers: std::collections::HashMap::new(),
        body: None,
        original_body_size: 0,
        timestamp: chrono::Utc::now(),
    };
    let ir = codec.decode_request(body, &env).unwrap();
    let found = ir.messages.iter().flat_map(|m| &m.content).any(|c| {
        matches!(c, tiygate_core::Content::ToolResult { content, .. } if content == "result-payload")
    });
    assert!(
        found,
        "array tool_result content must be flattened, not dropped"
    );
}

// =============================================================================
// 5. Stream-terminator fallbacks: decoders must synthesize a single, correct
//    terminal IR sequence even when upstream proxies strip/omit the native
//    end signal. Each test asserts "exactly one Finish" to guard against
//    double-termination regressions.
// =============================================================================

use tiygate_core::FinishReason;

fn count_finish(parts: &[StreamPart]) -> usize {
    parts
        .iter()
        .filter(|p| matches!(p, StreamPart::Finish { .. }))
        .count()
}

fn count_completed(parts: &[StreamPart]) -> usize {
    parts
        .iter()
        .filter(|p| matches!(p, StreamPart::ResponseCompleted { .. }))
        .count()
}

#[test]
fn gemini_decoder_ignores_traffic_type_only_usage_metadata() {
    // Gemini can include usageMetadata on every chunk with only trafficType.
    // That is not a terminal usage frame and must not synthesize Finish(Stop)
    // before a later functionCall arrives.
    let mut dec = GeminiStreamDecoder::new();
    let parts = dec
        .feed(r#"data: {"responseId":"r1","candidates":[{"content":{"parts":[{"text":"hi"}]}}],"usageMetadata":{"trafficType":"ON_DEMAND"}}"#)
        .unwrap();
    assert_eq!(
        count_finish(&parts),
        0,
        "trafficType-only usageMetadata must not synthesize Finish, got: {parts:?}"
    );
    assert!(
        parts.iter().all(|p| !matches!(p, StreamPart::Usage { .. })),
        "trafficType-only usageMetadata must not emit zero Usage, got: {parts:?}"
    );
}

#[test]
fn gemini_decoder_synthesizes_finish_on_usage_only() {
    // Proxy stripped `finishReason`; only `usageMetadata` arrives. The decoder
    // must synthesize exactly one Finish(Stop) so the cross-protocol ingress
    // encoder can still emit a terminator.
    let mut dec = GeminiStreamDecoder::new();
    let _ = dec
        .feed(r#"data: {"responseId":"r1","candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#)
        .unwrap();
    let parts = dec
        .feed(r#"data: {"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3,"totalTokenCount":8}}"#)
        .unwrap();
    assert_eq!(
        count_finish(&parts),
        1,
        "usage-only frame must synthesize exactly one Finish, got: {parts:?}"
    );
    assert!(matches!(
        parts
            .iter()
            .find(|p| matches!(p, StreamPart::Finish { .. })),
        Some(StreamPart::Finish {
            reason: FinishReason::Stop
        })
    ));
}

#[test]
fn gemini_decoder_usage_only_fallback_maps_tool_call_to_tool_calls() {
    // Proxy stripped `finishReason` on a tool-call turn; only `usageMetadata`
    // arrives. The fallback must map to ToolCalls, NOT Stop, or the client
    // would stop instead of running the tool.
    let mut dec = GeminiStreamDecoder::new();
    let _ = dec
        .feed(r#"data: {"responseId":"r1","candidates":[{"content":{"parts":[{"functionCall":{"name":"shell","args":{"cmd":"ls"}}}]}}]}"#)
        .unwrap();
    let parts = dec
        .feed(r#"data: {"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3,"totalTokenCount":8}}"#)
        .unwrap();
    assert!(
        matches!(
            parts
                .iter()
                .find(|p| matches!(p, StreamPart::Finish { .. })),
            Some(StreamPart::Finish {
                reason: FinishReason::ToolCalls
            })
        ),
        "usage-only fallback after functionCall must map to ToolCalls, got: {parts:?}"
    );
}

#[test]
fn gemini_decoder_stop_with_function_call_maps_to_tool_calls() {
    // Native Gemini streams use finishReason=STOP even when the candidate
    // contains a functionCall. Cross-protocol clients need ToolCalls, not Stop,
    // otherwise they will not execute the tool.
    let mut dec = GeminiStreamDecoder::new();
    let parts = dec
        .feed(r#"data: {"responseId":"r1","candidates":[{"content":{"parts":[{"functionCall":{"name":"shell","args":{"cmd":"ls"}}}]},"finishReason":"STOP"}]}"#)
        .unwrap();
    assert!(
        matches!(
            parts
                .iter()
                .find(|p| matches!(p, StreamPart::Finish { .. })),
            Some(StreamPart::Finish {
                reason: FinishReason::ToolCalls
            })
        ),
        "STOP with functionCall must map to ToolCalls, got: {parts:?}"
    );
}

#[test]
fn gemini_decoder_stop_after_prior_function_call_maps_to_tool_calls() {
    // The functionCall and finishReason may arrive in separate SSE events; the
    // decoder must latch that a tool call occurred before mapping STOP.
    let mut dec = GeminiStreamDecoder::new();
    let _ = dec
        .feed(r#"data: {"responseId":"r1","candidates":[{"content":{"parts":[{"functionCall":{"name":"shell","args":{"cmd":"ls"}}}]}}]}"#)
        .unwrap();
    let parts = dec
        .feed(r#"data: {"candidates":[{"finishReason":"STOP"}]}"#)
        .unwrap();
    assert!(
        matches!(
            parts
                .iter()
                .find(|p| matches!(p, StreamPart::Finish { .. })),
            Some(StreamPart::Finish {
                reason: FinishReason::ToolCalls
            })
        ),
        "STOP after a prior functionCall must map to ToolCalls, got: {parts:?}"
    );
}

#[test]
fn gemini_stream_encoder_tool_calls_finish_emits_stop() {
    // Gemini has no TOOL_CALLS finishReason; tool-call turns are represented as
    // functionCall parts plus STOP on the wire.
    let mut enc = GeminiStreamEncoder;
    let bytes = enc
        .encode_part(&StreamPart::Finish {
            reason: FinishReason::ToolCalls,
        })
        .unwrap();
    let s = String::from_utf8_lossy(&bytes);
    assert!(s.contains("\"finishReason\":\"STOP\""), "{s}");
    assert!(!s.contains("TOOL_CALLS"), "{s}");
}

#[test]
fn gemini_decoder_no_duplicate_finish_when_finishreason_present() {
    // finishReason and usageMetadata in the SAME event: exactly one Finish,
    // not two (the usage fallback must be gated by `saw_finish`).
    let mut dec = GeminiStreamDecoder::new();
    let parts = dec
        .feed(r#"data: {"responseId":"r1","candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"hi"}]}}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3,"totalTokenCount":8}}"#)
        .unwrap();
    assert_eq!(
        count_finish(&parts),
        1,
        "same-event finishReason+usage must yield exactly one Finish, got: {parts:?}"
    );
}

#[test]
fn chat_completions_decoder_infers_tool_calls_finish_on_done() {
    // Proxy ended a tool-call turn with `[DONE]` but never sent
    // `finish_reason: "tool_calls"`. Decoder must infer Finish(ToolCalls).
    let mut dec = ChatCompletionsStreamDecoder::new();
    let _ = dec
        .feed(r#"data: {"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"f","arguments":"{}"}}]}}]}"#)
        .unwrap();
    let parts = dec.feed("data: [DONE]").unwrap();
    assert_eq!(
        count_finish(&parts),
        1,
        "[DONE] after tool_calls must infer exactly one Finish, got: {parts:?}"
    );
    assert!(matches!(
        parts
            .iter()
            .find(|p| matches!(p, StreamPart::Finish { .. })),
        Some(StreamPart::Finish {
            reason: FinishReason::ToolCalls
        })
    ));
    assert_eq!(
        count_completed(&parts),
        1,
        "must still emit ResponseCompleted"
    );
}

#[test]
fn chat_completions_decoder_no_duplicate_finish_when_reason_seen() {
    // finish_reason already delivered in-band; `[DONE]` must NOT add a second
    // Finish.
    let mut dec = ChatCompletionsStreamDecoder::new();
    let _ = dec
        .feed(r#"data: {"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"f","arguments":"{}"}}]}}]}"#)
        .unwrap();
    let mid = dec
        .feed(r#"data: {"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#)
        .unwrap();
    assert_eq!(count_finish(&mid), 1);
    let parts = dec.feed("data: [DONE]").unwrap();
    assert_eq!(
        count_finish(&parts),
        0,
        "[DONE] must not duplicate a Finish already seen, got: {parts:?}"
    );
    assert_eq!(count_completed(&parts), 1);
}

#[test]
fn anthropic_decoder_synthesizes_finish_on_bare_message_stop() {
    // Older Anthropic / proxy ends with `message_stop` and no preceding
    // `message_delta.stop_reason`. Decoder must synthesize Finish(Stop).
    let mut dec = MessagesStreamDecoder::new();
    let _ = dec
        .feed(r#"data: {"type":"message_start","message":{"id":"m1","usage":{"input_tokens":3,"output_tokens":0}}}"#)
        .unwrap();
    let parts = dec.feed(r#"data: {"type":"message_stop"}"#).unwrap();
    assert_eq!(
        count_finish(&parts),
        1,
        "bare message_stop must synthesize exactly one Finish, got: {parts:?}"
    );
    assert_eq!(count_completed(&parts), 1);
}

#[test]
fn anthropic_bare_message_stop_after_tool_use_maps_to_tool_calls() {
    // A `tool_use` block streamed, then `message_stop` arrives without a
    // preceding stop_reason. The fallback must map to ToolCalls, NOT Stop.
    let mut dec = MessagesStreamDecoder::new();
    let _ = dec
        .feed(r#"data: {"type":"message_start","message":{"id":"m1","usage":{"input_tokens":3,"output_tokens":0}}}"#)
        .unwrap();
    let _ = dec
        .feed(r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu_1","name":"shell"}}"#)
        .unwrap();
    let parts = dec.feed(r#"data: {"type":"message_stop"}"#).unwrap();
    assert!(
        matches!(
            parts
                .iter()
                .find(|p| matches!(p, StreamPart::Finish { .. })),
            Some(StreamPart::Finish {
                reason: FinishReason::ToolCalls
            })
        ),
        "bare message_stop after tool_use must map to ToolCalls, got: {parts:?}"
    );
}

#[test]
fn anthropic_decoder_no_duplicate_finish_after_stop_reason() {
    // stop_reason delivered via message_delta; message_stop must NOT add a
    // second Finish.
    let mut dec = MessagesStreamDecoder::new();
    let mid = dec
        .feed(r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#)
        .unwrap();
    assert_eq!(count_finish(&mid), 1);
    let parts = dec.feed(r#"data: {"type":"message_stop"}"#).unwrap();
    assert_eq!(
        count_finish(&parts),
        0,
        "message_stop must not duplicate Finish, got: {parts:?}"
    );
    assert_eq!(count_completed(&parts), 1);
}

#[test]
fn anthropic_decoder_maps_stop_sequence_to_stop() {
    // Streaming `stop_sequence` must map to Stop, matching the non-streaming
    // decode path.
    let mut dec = MessagesStreamDecoder::new();
    let parts = dec
        .feed(r#"data: {"type":"message_delta","delta":{"stop_reason":"stop_sequence"}}"#)
        .unwrap();
    assert!(
        matches!(
            parts
                .iter()
                .find(|p| matches!(p, StreamPart::Finish { .. })),
            Some(StreamPart::Finish {
                reason: FinishReason::Stop
            })
        ),
        "stop_sequence must map to Stop, got: {parts:?}"
    );
}

#[test]
fn responses_decoder_accepts_response_done_alias() {
    // `response.done` must behave identically to `response.completed`.
    let mut dec = ResponsesStreamDecoder::new();
    let _ = dec
        .feed(r#"data: {"type":"response.created","response":{"id":"resp_1"}}"#)
        .unwrap();
    let parts = dec
        .feed(r#"data: {"type":"response.done","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#)
        .unwrap();
    assert_eq!(
        count_finish(&parts),
        1,
        "response.done must produce exactly one Finish, got: {parts:?}"
    );
    assert_eq!(
        count_completed(&parts),
        1,
        "response.done must produce exactly one ResponseCompleted, got: {parts:?}"
    );
}

#[test]
fn responses_decoder_maps_function_call_completed_to_tool_calls() {
    // Regression: OpenAI Responses reports `status: "completed"` even on a
    // tool-call turn. The decoder must emit Finish(ToolCalls) — NOT Stop —
    // when a `function_call` output item appeared, otherwise the cross-protocol
    // encoder produces `finish_reason: "stop"` and the client never runs the
    // tool.
    let mut dec = ResponsesStreamDecoder::new();
    let lines = [
        r#"data: {"type":"response.created","response":{"id":"resp_1"}}"#,
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_1","name":"shell"}}"#,
        r#"data: {"type":"response.function_call_arguments.delta","delta":"{\"cmd\":"}"#,
        r#"data: {"type":"response.function_call_arguments.done","output_index":0}"#,
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc_1","name":"shell"}}"#,
        r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed"}}"#,
    ];
    let mut all = Vec::new();
    for line in lines {
        all.extend(dec.feed(line).unwrap());
    }
    let finish = all.iter().find(|p| matches!(p, StreamPart::Finish { .. }));
    assert!(
        matches!(
            finish,
            Some(StreamPart::Finish {
                reason: FinishReason::ToolCalls
            })
        ),
        "function_call turn with status=completed must map to ToolCalls, got: {finish:?}"
    );
}

#[test]
fn responses_decoder_maps_plain_completed_to_stop() {
    // Counter-case: a turn with no function_call must still map to Stop.
    let mut dec = ResponsesStreamDecoder::new();
    let lines = [
        r#"data: {"type":"response.created","response":{"id":"resp_1"}}"#,
        r#"data: {"type":"response.output_text.delta","delta":"hi"}"#,
        r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed"}}"#,
    ];
    let mut all = Vec::new();
    for line in lines {
        all.extend(dec.feed(line).unwrap());
    }
    let finish = all.iter().find(|p| matches!(p, StreamPart::Finish { .. }));
    assert!(
        matches!(
            finish,
            Some(StreamPart::Finish {
                reason: FinishReason::Stop
            })
        ),
        "plain completed turn must map to Stop, got: {finish:?}"
    );
}
