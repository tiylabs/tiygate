//! Integration tests for the OpenAI Images codecs.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;

use serde_json::json;
use tiygate_core::{
    EndpointCapabilities, EndpointCodec, PassThroughPolicy, ProtocolEndpoint, ProtocolSuite,
    RawEnvelope, StreamDecoder, StreamEncoder,
};
use tiygate_protocols::images::{ImagesEditsCodec, ImagesGenerationsCodec};

fn make_raw_env() -> RawEnvelope {
    RawEnvelope {
        method: "POST".to_string(),
        path: "/v1/images/generations".to_string(),
        headers: HashMap::new(),
        body: None,
        original_body_size: 0,
        timestamp: chrono::Utc::now(),
    }
}

// ---------------------------------------------------------------------------
// ImagesGenerationsCodec
// ---------------------------------------------------------------------------

#[test]
fn test_generations_id() {
    let codec = ImagesGenerationsCodec::new();
    let id = codec.id();
    assert_eq!(id.suite, ProtocolSuite::OpenAiCompatible);
    assert_eq!(id.name, "images-generations");
    assert_eq!(id.version, "v1");
}

#[test]
fn test_generations_capabilities_streaming() {
    let codec = ImagesGenerationsCodec::new();
    let caps: &EndpointCapabilities = codec.capabilities();
    assert!(caps.streaming);
    assert!(caps.stream.server_sent_events);
    assert!(caps.stream.requires_stream_flag);
    assert!(!caps.multimodal);
    assert!(!caps.embeddings);
}

#[test]
fn test_generations_ingress_routes() {
    let codec = ImagesGenerationsCodec::new();
    let caps = codec.capabilities();
    assert_eq!(caps.ingress_routes, &[("POST", "/v1/images/generations")]);
}

#[test]
fn test_generations_pass_through_same_suite() {
    let codec = ImagesGenerationsCodec::new();
    let ingress =
        ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "images-generations", "v1");
    let egress = ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "chat-completions", "v1");
    assert_eq!(
        codec.pass_through_policy(&ingress, &egress),
        PassThroughPolicy::Passthrough
    );
}

#[test]
fn test_generations_pass_through_cross_suite() {
    let codec = ImagesGenerationsCodec::new();
    let ingress =
        ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "images-generations", "v1");
    let egress = ProtocolEndpoint::new(ProtocolSuite::AnthropicMessages, "messages", "2023-06-01");
    assert_eq!(
        codec.pass_through_policy(&ingress, &egress),
        PassThroughPolicy::Convert
    );
}

#[test]
fn test_generations_decode_request_extracts_model_and_stream() {
    let codec = ImagesGenerationsCodec::new();
    let env = make_raw_env();
    let body = json!({
        "model": "gpt-image-1",
        "prompt": "A cute sea otter",
        "n": 1,
        "size": "1024x1024",
        "stream": true,
    });
    let ir = codec.decode_request(body, &env).unwrap();
    assert_eq!(ir.model, "gpt-image-1");
    assert!(ir.stream);
    assert_eq!(
        ir.extensions.get("prompt").and_then(|v| v.as_str()),
        Some("A cute sea otter")
    );
}

#[test]
fn test_generations_decode_request_defaults_stream_false() {
    let codec = ImagesGenerationsCodec::new();
    let env = make_raw_env();
    let body = json!({"model": "dall-e-3", "prompt": "hello"});
    let ir = codec.decode_request(body, &env).unwrap();
    assert!(!ir.stream);
}

#[test]
fn test_generations_decode_request_preserves_extras() {
    let codec = ImagesGenerationsCodec::new();
    let env = make_raw_env();
    let body = json!({
        "model": "gpt-image-1",
        "prompt": "test",
        "size": "1024x1024",
        "quality": "hd",
        "n": 2,
    });
    let ir = codec.decode_request(body, &env).unwrap();
    let extras = ir.extensions.get("images_extra").unwrap();
    assert_eq!(extras["size"], json!("1024x1024"));
    assert_eq!(extras["quality"], json!("hd"));
    assert_eq!(extras["n"], json!(2));
}

#[test]
fn test_generations_encode_request_roundtrip() {
    let codec = ImagesGenerationsCodec::new();
    let env = make_raw_env();
    let original = json!({"model": "gpt-image-1", "prompt": "test", "size": "1024x1024"});
    let ir = codec.decode_request(original, &env).unwrap();
    let (encoded, _) = codec.encode_request(&ir).unwrap();
    assert_eq!(encoded["model"], json!("gpt-image-1"));
    assert_eq!(encoded["prompt"], json!("test"));
    assert_eq!(encoded["size"], json!("1024x1024"));
}

#[test]
fn test_generations_decode_response_extracts_usage() {
    let codec = ImagesGenerationsCodec::new();
    let body = json!({
        "created": 1700000000,
        "data": [{"b64_json": "abc"}],
        "usage": {"prompt_tokens": 10, "total_tokens": 15},
    });
    let ir = codec.decode_response(body).unwrap();
    let usage = ir.usage.unwrap();
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.total_tokens, 15);
}

#[test]
fn test_generations_decode_response_no_usage() {
    let codec = ImagesGenerationsCodec::new();
    let body = json!({"created": 1700000000, "data": [{"url": "https://example.com/img.png"}]});
    let ir = codec.decode_response(body).unwrap();
    assert!(ir.usage.is_none());
}

// ---------------------------------------------------------------------------
// ImagesEditsCodec
// ---------------------------------------------------------------------------

#[test]
fn test_edits_id() {
    let codec = ImagesEditsCodec::new();
    let id = codec.id();
    assert_eq!(id.suite, ProtocolSuite::OpenAiCompatible);
    assert_eq!(id.name, "images-edits");
    assert_eq!(id.version, "v1");
}

#[test]
fn test_edits_capabilities_multimodal() {
    let codec = ImagesEditsCodec::new();
    let caps = codec.capabilities();
    assert!(caps.multimodal);
    assert!(caps.streaming);
    assert!(!caps.embeddings);
}

#[test]
fn test_edits_ingress_routes() {
    let codec = ImagesEditsCodec::new();
    let caps = codec.capabilities();
    assert_eq!(caps.ingress_routes, &[("POST", "/v1/images/edits")]);
}

#[test]
fn test_edits_pass_through_same_suite() {
    let codec = ImagesEditsCodec::new();
    let ingress = ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "images-edits", "v1");
    let egress = ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "images-edits", "v1");
    assert_eq!(
        codec.pass_through_policy(&ingress, &egress),
        PassThroughPolicy::Passthrough
    );
}

#[test]
fn test_edits_decode_request_best_effort() {
    let codec = ImagesEditsCodec::new();
    let env = make_raw_env();
    // On the cross-protocol path, the handler might pass a JSON body.
    let body = json!({"model": "gpt-image-1", "prompt": "edit this", "stream": true});
    let ir = codec.decode_request(body, &env).unwrap();
    assert_eq!(ir.model, "gpt-image-1");
    assert!(ir.stream);
    assert_eq!(
        ir.extensions.get("body_kind").and_then(|v| v.as_str()),
        Some("multipart")
    );
}

// ---------------------------------------------------------------------------
// Stream encoder / decoder
// ---------------------------------------------------------------------------

#[test]
fn test_stream_encoder_done_marker() {
    let mut enc = tiygate_protocols::images::ImagesStreamEncoder::new();
    assert_eq!(enc.encode_done(), b"data: [DONE]\n\n");
}

#[test]
fn test_stream_encoder_error_frame() {
    let mut enc = tiygate_protocols::images::ImagesStreamEncoder::new();
    let frame = enc.encode_error(
        "test error",
        tiygate_core::ErrorClass::RateLimited,
        Some("test_code"),
    );
    let s = String::from_utf8(frame).unwrap();
    assert!(s.contains("\"type\":\"error\""));
    assert!(s.contains("test error"));
    assert!(s.contains("test_code"));
    assert!(s.contains("\"type\":\"rate_limit_error\""));
}

#[test]
fn test_stream_decoder_done_event() {
    let mut dec = tiygate_protocols::images::ImagesStreamDecoder::new();
    let parts = dec.feed("data: [DONE]").unwrap();
    assert_eq!(parts.len(), 1);
}

#[test]
fn test_stream_decoder_completed_event() {
    let mut dec = tiygate_protocols::images::ImagesStreamDecoder::new();
    let line = r#"data: {"type":"image_generation.completed","id":"img-123"}"#;
    let parts = dec.feed(line).unwrap();
    assert_eq!(parts.len(), 1);
}

#[test]
fn test_stream_decoder_edit_completed_event() {
    let mut dec = tiygate_protocols::images::ImagesStreamDecoder::new();
    let line = r#"data: {"type":"image_edit.completed","id":"edit-456"}"#;
    let parts = dec.feed(line).unwrap();
    assert_eq!(parts.len(), 1);
}

#[test]
fn test_stream_decoder_error_event() {
    let mut dec = tiygate_protocols::images::ImagesStreamDecoder::new();
    let line = r#"data: {"type":"error","error":{"message":"rate limited","code":"rate_limit"}}"#;
    let parts = dec.feed(line).unwrap();
    assert_eq!(parts.len(), 1);
}

#[test]
fn test_stream_decoder_ignores_partial_image() {
    let mut dec = tiygate_protocols::images::ImagesStreamDecoder::new();
    let line = r#"data: {"type":"image_generation.partial_image","b64_json":"abc"}"#;
    let parts = dec.feed(line).unwrap();
    assert!(parts.is_empty());
}

#[test]
fn test_stream_decoder_ignores_non_data_lines() {
    let mut dec = tiygate_protocols::images::ImagesStreamDecoder::new();
    assert!(dec.feed(": keepalive").unwrap().is_empty());
    assert!(dec.feed("").unwrap().is_empty());
    assert!(dec.feed("event: ping").unwrap().is_empty());
}
