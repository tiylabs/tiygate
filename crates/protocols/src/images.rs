//! OpenAI Images protocol codecs.
//!
//! Implements two endpoints that forward to upstream providers in raw-body
//! passthrough mode (no cross-protocol IR conversion):
//!
//! * `ImagesGenerationsCodec` — `POST /v1/images/generations` (JSON body).
//! * `ImagesEditsCodec` — `POST /v1/images/edits` (multipart/form-data body).
//!
//! Both codecs share `ProtocolSuite::OpenAiCompatible` so that same-suite
//! routing targets trigger verbatim byte forwarding via
//! `pass_through_policy`. The `decode_request` / `encode_request` /
//! `decode_response` / `encode_response` methods are minimal stubs — they
//! are only used on the cross-protocol (non-passthrough) path, which is not
//! the expected deployment for images endpoints.

use std::collections::HashMap;

use http::HeaderMap;
use serde_json::{json, Value};

use tiygate_core::{
    EndpointCapabilities, EndpointCodec, Error, ErrorClass, IrRequest, IrResponse,
    PassThroughPolicy, ProtocolEndpoint, ProtocolSuite, RawEnvelope, StreamDecoder, StreamEncoder,
    StreamPart,
};

/// Map an `ErrorClass` to the OpenAI-native `error.type` string.
fn error_type_for_class(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Transient => "server_error",
        ErrorClass::RateLimited => "rate_limit_error",
        ErrorClass::Auth => "authentication_error",
        ErrorClass::BadRequest => "invalid_request_error",
        ErrorClass::LossyOrCapability => "invalid_request_error",
        ErrorClass::ModelNotFound => "not_found_error",
        ErrorClass::DeadlineExceeded => "server_error",
        ErrorClass::UpstreamExhausted => "server_error",
        ErrorClass::AuthMissing => "authentication_error",
        ErrorClass::AuthInvalid => "authentication_error",
        ErrorClass::AuthDisabled => "permission_error",
        ErrorClass::Overloaded => "overloaded_error",
    }
}

// ---------------------------------------------------------------------------
// ImagesGenerationsCodec
// ---------------------------------------------------------------------------

/// Codec for `POST /v1/images/generations` (JSON body, optional SSE streaming).
pub struct ImagesGenerationsCodec {
    id: ProtocolEndpoint,
    capabilities: EndpointCapabilities,
}

impl Default for ImagesGenerationsCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl ImagesGenerationsCodec {
    pub fn new() -> Self {
        Self {
            id: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "images-generations", "v1"),
            capabilities: EndpointCapabilities {
                streaming: true,
                tools: false,
                reasoning: false,
                embeddings: false,
                force_upstream_stream: false,
                override_model_in_body: true,
                ingress_routes: &[("POST", "/v1/images/generations")],
                multimodal: false,
                structured_output: false,
                function_calling: false,
                parallel_tool_calls: false,
                hosted_tools: false,
                programmatic_tool_calling: false,
                extended_reasoning: false,
                deterministic_seed: false,
                tool_choice_required: false,
                stream: tiygate_core::StreamCaps {
                    server_sent_events: true,
                    usage_in_stream: true,
                    requires_stream_flag: true,
                },
                unknown_field_policy: tiygate_core::protocol::UnknownFieldPolicy::Drop,
                lossy_default_reject: true,
            },
        }
    }
}

impl EndpointCodec for ImagesGenerationsCodec {
    fn id(&self) -> &ProtocolEndpoint {
        &self.id
    }

    fn capabilities(&self) -> &EndpointCapabilities {
        &self.capabilities
    }

    fn decode_request(&self, body: Value, _env: &RawEnvelope) -> Result<IrRequest, Error> {
        let model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        let stream = body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let prompt = body.get("prompt").cloned().unwrap_or(Value::Null);

        let mut extensions = HashMap::new();
        extensions.insert("prompt".to_string(), prompt);
        // Preserve other generation parameters for the (non-passthrough)
        // encode path.
        if let Some(obj) = body.as_object() {
            let mut extras = serde_json::Map::new();
            for (k, v) in obj {
                if !matches!(k.as_str(), "model" | "prompt" | "stream") {
                    extras.insert(k.clone(), v.clone());
                }
            }
            if !extras.is_empty() {
                extensions.insert("images_extra".to_string(), Value::Object(extras));
            }
        }

        Ok(IrRequest {
            model,
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            params: tiygate_core::GenerationParams::default(),
            response_format: None,
            stream,
            ingress_protocol: self.id.clone(),
            metadata: None,
            extensions,
        })
    }

    fn encode_response(&self, ir: &IrResponse) -> Result<Value, Error> {
        // Minimal: surface usage if present; data array is not modelled in IR.
        let mut obj = serde_json::Map::new();
        obj.insert("object".to_string(), json!("list"));
        obj.insert("data".to_string(), json!([]));
        if let Some(ref usage) = ir.usage {
            obj.insert(
                "usage".to_string(),
                json!({
                    "prompt_tokens": usage.prompt_tokens,
                    "total_tokens": usage.total_tokens,
                }),
            );
        }
        Ok(Value::Object(obj))
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(ImagesStreamEncoder::new())
    }

    fn encode_request(&self, ir: &IrRequest) -> Result<(Value, HeaderMap), Error> {
        let mut obj = serde_json::Map::new();
        obj.insert("model".to_string(), json!(ir.model));
        if let Some(prompt) = ir.extensions.get("prompt") {
            obj.insert("prompt".to_string(), prompt.clone());
        }
        if ir.stream {
            obj.insert("stream".to_string(), json!(true));
        }
        if let Some(Value::Object(extras)) = ir.extensions.get("images_extra") {
            for (k, v) in extras {
                obj.insert(k.clone(), v.clone());
            }
        }
        Ok((Value::Object(obj), HeaderMap::new()))
    }

    fn decode_response(&self, body: Value) -> Result<IrResponse, Error> {
        let usage = body
            .get("usage")
            .filter(|usage| usage.is_object() && usage["total_tokens"].is_u64())
            .map(|u| {
                let prompt = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let total = u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                tiygate_core::Usage {
                    prompt_tokens: prompt,
                    completion_tokens: 0,
                    reasoning_tokens: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    total_tokens: total,
                }
            });

        Ok(IrResponse {
            content: Vec::new(),
            usage,
            finish_reason: None,
            response_id: None,
            stop_details: None,
            extensions: HashMap::new(),
        })
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(ImagesStreamDecoder::new())
    }

    fn pass_through_policy(
        &self,
        ingress: &ProtocolEndpoint,
        egress: &ProtocolEndpoint,
    ) -> PassThroughPolicy {
        if ingress.suite == egress.suite {
            PassThroughPolicy::Passthrough
        } else {
            PassThroughPolicy::Convert
        }
    }
}

// ---------------------------------------------------------------------------
// ImagesEditsCodec
// ---------------------------------------------------------------------------

/// Codec for `POST /v1/images/edits` (multipart/form-data body, optional SSE streaming).
///
/// In passthrough mode the raw multipart bytes are forwarded verbatim —
/// `decode_request` is only invoked on the cross-protocol path and receives
/// a best-effort placeholder body (the handler does not JSON-decode the
/// multipart payload).
pub struct ImagesEditsCodec {
    id: ProtocolEndpoint,
    capabilities: EndpointCapabilities,
}

impl Default for ImagesEditsCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl ImagesEditsCodec {
    pub fn new() -> Self {
        Self {
            id: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "images-edits", "v1"),
            capabilities: EndpointCapabilities {
                streaming: true,
                tools: false,
                reasoning: false,
                embeddings: false,
                force_upstream_stream: false,
                // Multipart re-encoding is not implemented in v1;
                // virtual→upstream model mapping is effectively
                // ignored for /v1/images/edits.
                override_model_in_body: false,
                ingress_routes: &[("POST", "/v1/images/edits")],
                multimodal: true,
                structured_output: false,
                function_calling: false,
                parallel_tool_calls: false,
                hosted_tools: false,
                programmatic_tool_calling: false,
                extended_reasoning: false,
                deterministic_seed: false,
                tool_choice_required: false,
                stream: tiygate_core::StreamCaps {
                    server_sent_events: true,
                    usage_in_stream: true,
                    requires_stream_flag: true,
                },
                unknown_field_policy: tiygate_core::protocol::UnknownFieldPolicy::Drop,
                lossy_default_reject: true,
            },
        }
    }
}

impl EndpointCodec for ImagesEditsCodec {
    fn id(&self) -> &ProtocolEndpoint {
        &self.id
    }

    fn capabilities(&self) -> &EndpointCapabilities {
        &self.capabilities
    }

    fn decode_request(&self, body: Value, _env: &RawEnvelope) -> Result<IrRequest, Error> {
        // Multipart bodies are not valid JSON; the handler extracts model
        // and stream flag directly from the raw bytes before calling this.
        // On the cross-protocol path we treat the body as a best-effort
        // JSON object.
        let model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        let stream = body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut extensions = HashMap::new();
        extensions.insert("body_kind".to_string(), json!("multipart"));
        if let Some(p) = body.get("prompt") {
            extensions.insert("prompt".to_string(), p.clone());
        }

        Ok(IrRequest {
            model,
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            params: tiygate_core::GenerationParams::default(),
            response_format: None,
            stream,
            ingress_protocol: self.id.clone(),
            metadata: None,
            extensions,
        })
    }

    fn encode_response(&self, ir: &IrResponse) -> Result<Value, Error> {
        let mut obj = serde_json::Map::new();
        obj.insert("object".to_string(), json!("list"));
        obj.insert("data".to_string(), json!([]));
        if let Some(ref usage) = ir.usage {
            obj.insert(
                "usage".to_string(),
                json!({
                    "prompt_tokens": usage.prompt_tokens,
                    "total_tokens": usage.total_tokens,
                }),
            );
        }
        Ok(Value::Object(obj))
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(ImagesStreamEncoder::new())
    }

    fn encode_request(&self, ir: &IrRequest) -> Result<(Value, HeaderMap), Error> {
        let mut obj = serde_json::Map::new();
        obj.insert("model".to_string(), json!(ir.model));
        if let Some(prompt) = ir.extensions.get("prompt") {
            obj.insert("prompt".to_string(), prompt.clone());
        }
        if ir.stream {
            obj.insert("stream".to_string(), json!(true));
        }
        Ok((Value::Object(obj), HeaderMap::new()))
    }

    fn decode_response(&self, body: Value) -> Result<IrResponse, Error> {
        let usage = body
            .get("usage")
            .filter(|usage| usage.is_object() && usage["total_tokens"].is_u64())
            .map(|u| {
                let prompt = u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                let total = u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                tiygate_core::Usage {
                    prompt_tokens: prompt,
                    completion_tokens: 0,
                    reasoning_tokens: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    total_tokens: total,
                }
            });

        Ok(IrResponse {
            content: Vec::new(),
            usage,
            finish_reason: None,
            response_id: None,
            stop_details: None,
            extensions: HashMap::new(),
        })
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(ImagesStreamDecoder::new())
    }

    fn pass_through_policy(
        &self,
        ingress: &ProtocolEndpoint,
        egress: &ProtocolEndpoint,
    ) -> PassThroughPolicy {
        if ingress.suite == egress.suite {
            PassThroughPolicy::Passthrough
        } else {
            PassThroughPolicy::Convert
        }
    }
}

// ---------------------------------------------------------------------------
// Stream encoder / decoder (shared by both codecs)
// ---------------------------------------------------------------------------

/// SSE stream encoder for images endpoints.
///
/// In passthrough mode `drive_upstream_stream` forwards upstream bytes
/// verbatim (`build_stream_transcode` returns `None` for same-suite), so
/// `encode_part` is never called. The error and done markers are only
/// injected when the gateway truncates a stream.
pub struct ImagesStreamEncoder {
    response_id: Option<String>,
}

impl Default for ImagesStreamEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ImagesStreamEncoder {
    pub fn new() -> Self {
        Self { response_id: None }
    }
}

impl StreamEncoder for ImagesStreamEncoder {
    fn encode_part(&mut self, part: &StreamPart) -> Result<Vec<u8>, Error> {
        match part {
            StreamPart::ResponseStarted { id } => {
                self.response_id = Some(id.clone());
                Ok(Vec::new())
            }
            StreamPart::ResponseCompleted { usage, .. } => {
                let mut payload = serde_json::Map::new();
                payload.insert("type".to_string(), json!("image_generation.completed"));
                if let Some(ref id) = self.response_id {
                    payload.insert("id".to_string(), json!(id));
                }
                if let Some(u) = usage {
                    payload.insert(
                        "usage".to_string(),
                        json!({
                            "prompt_tokens": u.prompt_tokens,
                            "total_tokens": u.total_tokens,
                        }),
                    );
                }
                Ok(format!("data: {}\n\n", Value::Object(payload)).into_bytes())
            }
            StreamPart::Error {
                message,
                class,
                upstream_code,
            } => Ok(self.encode_error(message, *class, upstream_code.as_deref())),
            _ => Ok(Vec::new()),
        }
    }

    fn encode_error(
        &mut self,
        message: &str,
        class: ErrorClass,
        upstream_code: Option<&str>,
    ) -> Vec<u8> {
        let mut error_obj = json!({
            "message": message,
            "type": error_type_for_class(class),
        });
        if let Some(c) = upstream_code {
            error_obj["code"] = json!(c);
        }
        let payload = json!({
            "type": "error",
            "error": error_obj,
        });
        format!("data: {}\n\n", payload).into_bytes()
    }

    fn encode_done(&mut self) -> Vec<u8> {
        b"data: [DONE]\n\n".to_vec()
    }
}

/// SSE stream decoder for images endpoints.
///
/// In passthrough mode (`build_stream_transcode` returns `None`) this
/// decoder is not exercised. It is provided for the cross-protocol
/// transcoding path and treats unknown event types gracefully.
pub struct ImagesStreamDecoder {
    saw_completed: bool,
}

impl Default for ImagesStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ImagesStreamDecoder {
    pub fn new() -> Self {
        Self {
            saw_completed: false,
        }
    }
}

impl StreamDecoder for ImagesStreamDecoder {
    fn feed(&mut self, line: &str) -> Result<Vec<StreamPart>, Error> {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with("data:") {
            return Ok(Vec::new());
        }
        let data = trimmed["data:".len()..].trim();
        if data == "[DONE]" {
            self.saw_completed = true;
            return Ok(vec![StreamPart::ResponseCompleted {
                id: String::new(),
                status: "completed".to_string(),
                usage: None,
                extensions: HashMap::new(),
            }]);
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Ok(Vec::new()),
        };
        let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match event_type {
            "image_generation.partial_image" | "image_edit.partial_image" => Ok(Vec::new()),
            "image_generation.completed" | "image_edit.completed" => {
                self.saw_completed = true;
                Ok(vec![StreamPart::ResponseCompleted {
                    id: value
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    status: "completed".to_string(),
                    usage: None,
                    extensions: HashMap::new(),
                }])
            }
            "error" => {
                let msg = value
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error")
                    .to_string();
                let code = value
                    .get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(|v| v.as_str());
                let class = tiygate_core::classify_upstream_error(None, code);
                Ok(vec![StreamPart::Error {
                    message: msg,
                    class,
                    upstream_code: code.map(String::from),
                }])
            }
            _ => Ok(Vec::new()),
        }
    }

    fn finish(&mut self) -> Result<Vec<StreamPart>, Error> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// Inventory registration
// ---------------------------------------------------------------------------

inventory::submit! {
    tiygate_core::CodecRegistration {
        make: || Box::new(ImagesGenerationsCodec::new()),
    }
}

inventory::submit! {
    tiygate_core::CodecRegistration {
        make: || Box::new(ImagesEditsCodec::new()),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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

    #[test]
    fn test_generations_capabilities() {
        let codec = ImagesGenerationsCodec::new();
        let caps = codec.capabilities();
        assert!(caps.streaming);
        assert!(!caps.multimodal);
        assert!(!caps.embeddings);
        assert!(!caps.tools);
        assert_eq!(caps.ingress_routes, &[("POST", "/v1/images/generations")]);
        assert!(caps.stream.server_sent_events);
        assert!(caps.stream.requires_stream_flag);
    }

    #[test]
    fn test_edits_capabilities() {
        let codec = ImagesEditsCodec::new();
        let caps = codec.capabilities();
        assert!(caps.streaming);
        assert!(caps.multimodal);
        assert!(!caps.embeddings);
        assert_eq!(caps.ingress_routes, &[("POST", "/v1/images/edits")]);
    }

    #[test]
    fn test_generations_pass_through_same_suite() {
        let codec = ImagesGenerationsCodec::new();
        let ingress =
            ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "images-generations", "v1");
        let egress =
            ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "images-generations", "v1");
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
        let egress =
            ProtocolEndpoint::new(ProtocolSuite::AnthropicMessages, "messages", "2023-06-01");
        assert_eq!(
            codec.pass_through_policy(&ingress, &egress),
            PassThroughPolicy::Convert
        );
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
    fn test_generations_decode_request() {
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
        assert!(ir.extensions.contains_key("images_extra"));
    }

    #[test]
    fn test_generations_decode_request_no_stream() {
        let codec = ImagesGenerationsCodec::new();
        let env = make_raw_env();
        let body = json!({"model": "dall-e-3", "prompt": "hello"});
        let ir = codec.decode_request(body, &env).unwrap();
        assert_eq!(ir.model, "dall-e-3");
        assert!(!ir.stream);
    }

    #[test]
    fn test_snapshot_generations_decode_request() {
        let codec = ImagesGenerationsCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-image-1",
            "prompt": "A cute sea otter",
            "size": "1024x1024",
        });
        let ir = codec.decode_request(body, &env).unwrap();
        // Serialize to JSON Value for deterministic snapshot output, since the
        // IR's `extensions` HashMap has non-deterministic iteration order.
        let snapshot = serde_json::to_value(&ir).unwrap();
        insta::assert_debug_snapshot!(snapshot);
    }

    #[test]
    fn test_stream_encoder_done() {
        let mut enc = ImagesStreamEncoder::new();
        assert_eq!(enc.encode_done(), b"data: [DONE]\n\n");
    }

    #[test]
    fn test_stream_encoder_error() {
        let mut enc = ImagesStreamEncoder::new();
        let frame = enc.encode_error("test error", ErrorClass::RateLimited, Some("test_code"));
        let s = String::from_utf8(frame).unwrap();
        assert!(s.contains("\"type\":\"error\""));
        assert!(s.contains("test error"));
        assert!(s.contains("test_code"));
        assert!(s.contains("\"type\":\"rate_limit_error\""));
    }

    #[test]
    fn test_stream_decoder_done() {
        let mut dec = ImagesStreamDecoder::new();
        let parts = dec.feed("data: [DONE]").unwrap();
        assert_eq!(parts.len(), 1);
        assert!(dec.saw_completed);
    }

    #[test]
    fn test_stream_decoder_completed_event() {
        let mut dec = ImagesStreamDecoder::new();
        let line = r#"data: {"type":"image_generation.completed","id":"img-123"}"#;
        let parts = dec.feed(line).unwrap();
        assert_eq!(parts.len(), 1);
        assert!(dec.saw_completed);
    }

    #[test]
    fn test_stream_decoder_ignores_unknown() {
        let mut dec = ImagesStreamDecoder::new();
        let parts = dec
            .feed(r#"data: {"type":"image_generation.partial_image"}"#)
            .unwrap();
        assert!(parts.is_empty());
        assert!(!dec.saw_completed);
    }

    #[test]
    fn test_generations_decode_response() {
        let codec = ImagesGenerationsCodec::new();
        let body = json!({
            "created": 1700000000,
            "data": [{"b64_json": "abc"}],
            "usage": {"prompt_tokens": 10, "total_tokens": 15},
        });
        let ir = codec.decode_response(body).unwrap();
        assert!(ir.usage.is_some());
        let usage = ir.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn test_generations_encode_request_roundtrip() {
        let codec = ImagesGenerationsCodec::new();
        let env = make_raw_env();
        let original = json!({"model": "gpt-image-1", "prompt": "test"});
        let ir = codec.decode_request(original, &env).unwrap();
        let (encoded, _) = codec.encode_request(&ir).unwrap();
        assert_eq!(encoded["model"], json!("gpt-image-1"));
        assert_eq!(encoded["prompt"], json!("test"));
    }
}
