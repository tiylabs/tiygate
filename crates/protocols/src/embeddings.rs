//! OpenAI Embeddings protocol codec.
//! Implements bidirectional conversion for the Embeddings API (non-streaming).

use http::HeaderMap;
use serde_json::{json, Value};
use std::collections::HashMap;

use tiygate_core::{
    Content, EndpointCapabilities, EndpointCodec, ErrorClass, FinishReason, IrRequest, IrResponse,
    Message, ProtocolEndpoint, ProtocolSuite, RawEnvelope, Role, StreamDecoder, StreamEncoder,
    StreamPart, Usage,
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

pub struct EmbeddingsCodec {
    id: ProtocolEndpoint,
    capabilities: EndpointCapabilities,
}

impl Default for EmbeddingsCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl EmbeddingsCodec {
    pub fn new() -> Self {
        Self {
            id: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "embeddings", "v1"),
            capabilities: EndpointCapabilities {
                streaming: false,
                tools: false,
                reasoning: false,
                embeddings: true,
                force_upstream_stream: false,
                override_model_in_body: false,
                ingress_routes: &[("POST", "/v1/embeddings")],
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
                    server_sent_events: false,
                    usage_in_stream: false,
                    requires_stream_flag: false,
                },
                unknown_field_policy: tiygate_core::protocol::UnknownFieldPolicy::Drop,
                lossy_default_reject: true,
            },
        }
    }
}

impl EndpointCodec for EmbeddingsCodec {
    fn id(&self) -> &ProtocolEndpoint {
        &self.id
    }

    fn capabilities(&self) -> &EndpointCapabilities {
        &self.capabilities
    }

    fn decode_request(
        &self,
        body: Value,
        _env: &RawEnvelope,
    ) -> Result<IrRequest, tiygate_core::Error> {
        let model = body["model"].as_str().unwrap_or("unknown").to_string();

        // OpenAI embeddings API: `dimensions` parameter (text-embedding-3 and later)
        // Controls the output embedding vector size. Preserved via extensions.
        let mut extensions = HashMap::new();
        if let Some(dims) = body.get("dimensions") {
            extensions.insert("dimensions".to_string(), dims.clone());
        }
        // `encoding_format` — "float" or "base64" — preserved in extensions
        if let Some(enc) = body.get("encoding_format").and_then(|v| v.as_str()) {
            extensions.insert("encoding_format".to_string(), json!(enc));
        }
        // `user` — OpenAI's end-user identifier — preserved in extensions
        if let Some(u) = body.get("user").and_then(|v| v.as_str()) {
            extensions.insert("user".to_string(), json!(u));
        }

        let input_text = if let Some(s) = body["input"].as_str() {
            s.to_string()
        } else if let Some(arr) = body["input"].as_array() {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            String::new()
        };

        let messages = vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: input_text,
                annotations: None,
                prompt_cache_breakpoint: None,
            }],
        }];

        Ok(IrRequest {
            model,
            system: None,
            messages,
            tools: vec![],
            params: Default::default(),
            response_format: None,
            stream: false,
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::OpenAiCompatible,
                "embeddings",
                "v1",
            ),
            metadata: None,
            extensions,
        })
    }

    fn encode_response(&self, ir: &IrResponse) -> Result<Value, tiygate_core::Error> {
        let usage = ir.usage.as_ref();
        Ok(json!({
            "object": "list",
            "data": [],
            "model": "embedding-model",
            "usage": {
                "prompt_tokens": usage.map_or(0, |u| u.prompt_tokens),
                "total_tokens": usage.map_or(0, |u| u.total_tokens),
            }
        }))
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(EmbeddingsStreamEncoder)
    }

    fn encode_request(&self, ir: &IrRequest) -> Result<(Value, HeaderMap), tiygate_core::Error> {
        let input = ir
            .messages
            .iter()
            .filter_map(|m| {
                m.content.iter().find_map(|c| match c {
                    Content::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok((
            json!({ "model": ir.model, "input": input }),
            HeaderMap::new(),
        ))
    }

    fn decode_response(&self, body: Value) -> Result<IrResponse, tiygate_core::Error> {
        let usage = body["usage"].as_object().map(|u| Usage {
            prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens: u
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            total_tokens: u["total_tokens"].as_u64().unwrap_or(0),
            ..Default::default()
        });

        Ok(IrResponse {
            content: vec![],
            usage,
            finish_reason: Some(FinishReason::Stop),
            response_id: body["id"].as_str().map(String::from),
            stop_details: None,
            extensions: Default::default(),
        })
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(EmbeddingsStreamDecoder)
    }
}

pub struct EmbeddingsStreamEncoder;
impl StreamEncoder for EmbeddingsStreamEncoder {
    fn encode_part(&mut self, _part: &StreamPart) -> Result<Vec<u8>, tiygate_core::Error> {
        Ok(Vec::new())
    }
    fn encode_error(
        &mut self,
        message: &str,
        class: ErrorClass,
        upstream_code: Option<&str>,
    ) -> Vec<u8> {
        let mut err = json!({"message": message, "type": error_type_for_class(class)});
        if let Some(c) = upstream_code {
            err["code"] = json!(c);
        }
        json!({"error": err}).to_string().into_bytes()
    }
    fn encode_done(&mut self) -> Vec<u8> {
        b"data: [DONE]\n\n".to_vec()
    }
}

pub struct EmbeddingsStreamDecoder;
impl StreamDecoder for EmbeddingsStreamDecoder {
    fn feed(&mut self, _line: &str) -> Result<Vec<StreamPart>, tiygate_core::Error> {
        Ok(Vec::new())
    }
    fn finish(&mut self) -> Result<Vec<StreamPart>, tiygate_core::Error> {
        Ok(Vec::new())
    }
}

inventory::submit! {
    tiygate_core::CodecRegistration { make: || Box::new(EmbeddingsCodec::new()) }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make_raw_env() -> RawEnvelope {
        RawEnvelope {
            method: "POST".to_string(),
            path: "/v1/embeddings".to_string(),
            headers: std::collections::HashMap::new(),
            body: None,
            original_body_size: 0,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_decode_embedding_request() {
        let codec = EmbeddingsCodec::new();
        let env = make_raw_env();
        let body = json!({"model": "text-embedding-3-small", "input": "Hello world"});
        let ir = codec.decode_request(body, &env).unwrap();
        assert_eq!(ir.model, "text-embedding-3-small");
        assert_eq!(ir.messages.len(), 1);
    }

    #[test]
    fn test_decode_embedding_request_batch() {
        let codec = EmbeddingsCodec::new();
        let env = make_raw_env();
        let body = json!({"model": "text-embedding-3-small", "input": ["Hello", "World"]});
        let ir = codec.decode_request(body, &env).unwrap();
        assert_eq!(ir.messages.len(), 1);
    }

    #[test]
    fn test_encode_request_roundtrip() {
        let codec = EmbeddingsCodec::new();
        let env = make_raw_env();
        let original = json!({"model": "text-embedding-3-small", "input": "test"});
        let ir = codec.decode_request(original.clone(), &env).unwrap();
        let (re_encoded, _) = codec.encode_request(&ir).unwrap();
        let ir2 = codec.decode_request(re_encoded, &env).unwrap();
        assert_eq!(ir.model, ir2.model);
        assert_eq!(ir.messages.len(), ir2.messages.len());
    }

    #[test]
    fn test_snapshot_decode_request() {
        let codec = EmbeddingsCodec::new();
        let env = make_raw_env();
        let body = json!({"model": "text-embedding-3-small", "input": "Hello world"});
        let ir = codec.decode_request(body, &env).unwrap();
        insta::assert_debug_snapshot!(ir);
    }

    #[test]
    fn test_codec_capabilities() {
        let codec = EmbeddingsCodec::new();
        assert!(codec.capabilities().embeddings);
        assert!(!codec.capabilities().streaming);
        assert!(!codec.capabilities().tools);
    }
}
