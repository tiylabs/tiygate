//! Regression tests for the N×N protocol-conversion bug fixes.
//!
//! Covers four classes of defect that the earlier implementation had:
//!   1. Streaming decoders aborting on benign/unknown SSE events.
//!   2. Cache tokens being double-counted on decode→encode round-trips
//!      (OpenAI/Responses/Gemini `prompt_tokens` already includes cache).
//!   3. Streaming tool-call `index` collapsing to 0 for parallel calls.
//!   4. Anthropic streaming output missing content_block_start/stop framing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::Value;
use tiygate_core::{EndpointCodec, StreamDecoder, StreamEncoder, StreamPart};
use tiygate_protocols::chat_completions::{ChatCompletionsCodec, ChatCompletionsStreamEncoder};
use tiygate_protocols::gemini::GeminiCodec;
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
    assert!(parts
        .iter()
        .all(|p| !matches!(p, StreamPart::Error { .. })));
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
    assert_eq!(encoded["usageMetadata"]["promptTokenCount"], 200);
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
        })
        .unwrap();
    let second = enc
        .encode_part(&StreamPart::ToolCallDelta {
            id: "call_b".to_string(),
            name: Some("g".to_string()),
            arguments: String::new(),
        })
        .unwrap();
    let s1 = String::from_utf8_lossy(&first);
    let s2 = String::from_utf8_lossy(&second);
    assert!(s1.contains("\"index\":0"), "first call should be index 0: {s1}");
    assert!(s2.contains("\"index\":1"), "second call should be index 1: {s2}");

    // An argument fragment for call_b (empty id on some providers) appends to
    // the most-recent call (index 1).
    let frag = enc
        .encode_part(&StreamPart::ToolCallDelta {
            id: "call_b".to_string(),
            name: None,
            arguments: "{\"x\":1}".to_string(),
        })
        .unwrap();
    let s3 = String::from_utf8_lossy(&frag);
    assert!(s3.contains("\"index\":1"), "fragment should target index 1: {s3}");
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
        })
        .unwrap(),
    ));
    sse.push_str(&String::from_utf8_lossy(
        &enc.encode_part(&StreamPart::ToolCallDelta {
            id: "call_a".to_string(),
            name: None,
            arguments: "{\"x\":1}".to_string(),
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
    let bytes = enc.encode_part(&StreamPart::Usage { usage }).unwrap();
    let s = String::from_utf8_lossy(&bytes);
    assert!(s.contains("\"input_tokens\":1000"), "{s}");
    assert!(s.contains("\"total_tokens\":1050"), "{s}");
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
        truncated: false,
        original_body_size: 0,
        timestamp: chrono::Utc::now(),
    };
    let ir = codec.decode_request(body, &env).unwrap();
    let found = ir.messages.iter().flat_map(|m| &m.content).any(|c| {
        matches!(c, tiygate_core::Content::ToolResult { content, .. } if content == "result-payload")
    });
    assert!(found, "array tool_result content must be flattened, not dropped");
}
