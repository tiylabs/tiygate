//! Google Gemini generateContent protocol codec.
//! Implements bidirectional conversion for Google's Gemini API.

use http::HeaderMap;
use serde_json::{json, Value};

use tiygate_core::{
    Content, EndpointCapabilities, EndpointCodec, FinishReason, IrRequest, IrResponse, Message,
    ProtocolEndpoint, ProtocolSuite, RawEnvelope, Role, StreamDecoder, StreamEncoder, StreamPart,
    Tool, Usage,
};

pub struct GeminiCodec {
    id: ProtocolEndpoint,
    capabilities: EndpointCapabilities,
}

impl Default for GeminiCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl GeminiCodec {
    pub fn new() -> Self {
        Self {
            id: ProtocolEndpoint::new(ProtocolSuite::GoogleGemini, "generateContent", "v1beta"),
            capabilities: EndpointCapabilities {
                streaming: true,
                tools: true,
                reasoning: true,
                embeddings: false,
                force_upstream_stream: false,
                override_model_in_body: false,
                ingress_routes: &[("POST", "/v1beta/models/{model}:generateContent")],
                multimodal: true,
                structured_output: true,
                function_calling: true,
                // §1 of docs/protocol-capability-matrix.md: chat→gemini
                // parallel tool calls are lossy (Gemini's functionCall parts
                // can carry multiple calls, but the chat-completions
                // concurrent-fan-out semantics are not preserved). Mark as
                // unsupported so `check_lossy_conversion` rejects the crossing.
                parallel_tool_calls: false,
                extended_reasoning: true,
                deterministic_seed: false,
                // Gemini does not support tool_choice=required (see §1 of matrix)
                tool_choice_required: false,
                stream: tiygate_core::StreamCaps {
                    server_sent_events: true,
                    usage_in_stream: true,
                    requires_stream_flag: false,
                },
                unknown_field_policy: tiygate_core::protocol::UnknownFieldPolicy::Drop,
                lossy_default_reject: true,
            },
        }
    }
}

impl EndpointCodec for GeminiCodec {
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
        let stream = body
            .get("_stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let system = body["system_instruction"]["parts"].as_array().map(|parts| {
            parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        });

        let mut messages = Vec::new();
        if let Some(contents) = body["contents"].as_array() {
            for item in contents {
                let role = match item["role"].as_str().unwrap_or("user") {
                    "user" => Role::User,
                    "model" => Role::Assistant,
                    "function" => Role::Tool,
                    _ => Role::User,
                };
                let content = if let Some(parts) = item["parts"].as_array() {
                    let mut cp = Vec::new();
                    for part in parts {
                        if let Some(text) = part["text"].as_str() {
                            cp.push(Content::Text {
                                text: text.to_string(),
                            });
                        } else if let Some(fc) = part.get("functionCall") {
                            cp.push(Content::ToolCall {
                                id: String::new(),
                                name: fc["name"].as_str().unwrap_or("").to_string(),
                                arguments: fc["args"].clone(),
                            });
                        } else if let Some(fr) = part.get("functionResponse") {
                            cp.push(Content::ToolResult {
                                tool_call_id: String::new(),
                                name: fr["name"].as_str().unwrap_or("").to_string(),
                                content: fr["response"]
                                    .as_object()
                                    .map(|o| serde_json::to_string(o).unwrap_or_default())
                                    .unwrap_or_default(),
                            });
                        } else if let Some(id) = part.get("inlineData") {
                            cp.push(Content::Media {
                                source: tiygate_core::ir::MediaSource::Inline {
                                    data: id["data"].as_str().unwrap_or("").to_string(),
                                },
                                mime_type: id["mimeType"]
                                    .as_str()
                                    .unwrap_or("application/octet-stream")
                                    .to_string(),
                                metadata: Default::default(),
                            });
                        } else if let Some(fd) = part.get("fileData") {
                            cp.push(Content::Media {
                                source: tiygate_core::ir::MediaSource::Url {
                                    url: fd["fileUri"].as_str().unwrap_or("").to_string(),
                                },
                                mime_type: fd["mimeType"]
                                    .as_str()
                                    .unwrap_or("application/octet-stream")
                                    .to_string(),
                                metadata: Default::default(),
                            });
                        }
                    }
                    cp
                } else {
                    vec![Content::Text {
                        text: String::new(),
                    }]
                };
                messages.push(Message { role, content });
            }
        }

        let tools: Vec<Tool> = if let Some(tools_arr) = body["tools"].as_array() {
            tools_arr
                .iter()
                .flat_map(|t| {
                    t["functionDeclarations"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .map(|fd| Tool {
                                    name: fd["name"].as_str().unwrap_or("").to_string(),
                                    description: fd["description"].as_str().map(String::from),
                                    parameters: fd["parameters"].as_object().map(|p| json!(p)),
                                    required: false,
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .collect()
        } else {
            Vec::new()
        };

        let gc = &body["generationConfig"];
        let params = tiygate_core::GenerationParams {
            max_tokens: gc["maxOutputTokens"].as_u64().map(|v| v as u32),
            temperature: gc["temperature"].as_f64().map(|v| v as f32),
            top_p: gc["topP"].as_f64().map(|v| v as f32),
            top_k: gc["topK"].as_u64().map(|v| v as u32),
            stop: gc["stopSequences"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            ..Default::default()
        };

        Ok(IrRequest {
            model,
            system,
            messages,
            tools,
            params,
            response_format: None,
            stream,
            ingress_protocol: self.id.clone(),
            extensions: Default::default(),
        })
    }

    fn encode_response(&self, ir: &IrResponse) -> Result<Value, tiygate_core::Error> {
        let mut parts = Vec::new();
        for c in &ir.content {
            match c {
                Content::Text { text } => parts.push(json!({"text": text})),
                Content::Reasoning { text } => parts.push(json!({"thought": text})),
                Content::ToolCall {
                    id: _,
                    name,
                    arguments,
                } => {
                    parts.push(json!({"functionCall": {"name": name, "args": arguments}}));
                }
                _ => {}
            }
        }
        let mut response = json!({"candidates": [{"content": {"role": "model", "parts": parts}}]});
        if let Some(id) = &ir.response_id {
            response["responseId"] = json!(id);
        }
        if let Some(fr) = &ir.finish_reason {
            response["candidates"][0]["finishReason"] = json!(match fr {
                FinishReason::Stop => "STOP",
                FinishReason::Length => "MAX_TOKENS",
                FinishReason::ContentFilter => "SAFETY",
                FinishReason::ToolCalls => "STOP",
                _ => "STOP",
            });
        }
        if let Some(usage) = &ir.usage {
            response["usageMetadata"] = json!({
                "promptTokenCount": usage.prompt_tokens,
                "candidatesTokenCount": usage.completion_tokens,
                "totalTokenCount": usage.total_tokens,
            });
            if let Some(rt) = usage.reasoning_tokens {
                response["usageMetadata"]["thoughtsTokenCount"] = json!(rt);
            }
            if let Some(cr) = usage.cache_read_tokens {
                if cr > 0 {
                    response["usageMetadata"]["cachedContentTokenCount"] = json!(cr);
                }
            }
        }
        Ok(response)
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(GeminiStreamEncoder)
    }
    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(GeminiStreamDecoder::new())
    }

    fn encode_request(&self, ir: &IrRequest) -> Result<(Value, HeaderMap), tiygate_core::Error> {
        let mut body = json!({});
        if let Some(sys) = &ir.system {
            body["system_instruction"] = json!({"parts": [{"text": sys}]});
        }
        let mut contents = Vec::new();
        for msg in &ir.messages {
            let role_str = match msg.role {
                Role::User => "user",
                Role::Assistant => "model",
                Role::Tool => "function",
                Role::System => "user",
            };
            let mut parts = Vec::new();
            for c in &msg.content {
                match c {
                    Content::Text { text } => parts.push(json!({"text": text})),
                    Content::ToolCall {
                        id: _,
                        name,
                        arguments,
                    } => {
                        parts.push(json!({"functionCall": {"name": name, "args": arguments}}));
                    }
                    Content::ToolResult {
                        tool_call_id: _,
                        name,
                        content,
                    } => {
                        let response_obj: Value =
                            serde_json::from_str(content).unwrap_or(json!({"output": content}));
                        parts.push(
                            json!({"functionResponse": {"name": name, "response": response_obj}}),
                        );
                    }
                    Content::Media {
                        source, mime_type, ..
                    } => match source {
                        tiygate_core::ir::MediaSource::Url { url } => {
                            parts
                                .push(json!({"fileData": {"fileUri": url, "mimeType": mime_type}}));
                        }
                        tiygate_core::ir::MediaSource::Inline { data } => {
                            parts
                                .push(json!({"inlineData": {"mimeType": mime_type, "data": data}}));
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            if !parts.is_empty() {
                contents.push(json!({"role": role_str, "parts": parts}));
            }
        }
        body["contents"] = json!(contents);

        let mut gc = json!({});
        let mut has_gc = false;
        if let Some(mt) = ir.params.max_tokens {
            gc["maxOutputTokens"] = json!(mt);
            has_gc = true;
        }
        if let Some(t) = ir.params.temperature {
            gc["temperature"] = json!(t);
            has_gc = true;
        }
        if let Some(p) = ir.params.top_p {
            gc["topP"] = json!(p);
            has_gc = true;
        }
        if let Some(k) = ir.params.top_k {
            gc["topK"] = json!(k);
            has_gc = true;
        }
        if !ir.params.stop.is_empty() {
            gc["stopSequences"] = json!(ir.params.stop);
            has_gc = true;
        }
        // Gemini structured output: responseSchema in generationConfig
        // https://ai.google.dev/gemini-api/docs/structured-output
        match &ir.response_format {
            Some(tiygate_core::ResponseFormat::JsonSchema { schema, .. }) => {
                gc["responseSchema"] = schema.clone();
                gc["responseMimeType"] = json!("application/json");
                has_gc = true;
            }
            Some(tiygate_core::ResponseFormat::JsonObject) => {
                gc["responseMimeType"] = json!("application/json");
                has_gc = true;
            }
            _ => {}
        }
        if has_gc {
            body["generationConfig"] = gc;
        }

        if !ir.tools.is_empty() {
            let declarations: Vec<Value> = ir.tools.iter().map(|t| json!({"name": t.name, "description": t.description, "parameters": t.parameters})).collect();
            body["tools"] = json!([{"functionDeclarations": declarations}]);
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        Ok((body, headers))
    }

    fn decode_response(&self, body: Value) -> Result<IrResponse, tiygate_core::Error> {
        let response_id = body["responseId"].as_str().map(String::from);
        let mut content = Vec::new();
        // Collect thoughtSignatures for Gemini 3 multi-turn preservation.
        // Gemini 3 requires thoughtSignature on functionCalls;
        // missing signatures cause 400 errors.
        // https://ai.google.dev/gemini-api/docs/thought-signatures
        let mut extensions = std::collections::HashMap::new();
        let mut thought_signatures: Vec<serde_json::Value> = Vec::new();
        if let Some(candidates) = body["candidates"].as_array() {
            for candidate in candidates {
                if let Some(c) = candidate.get("content") {
                    if let Some(parts) = c["parts"].as_array() {
                        for part in parts {
                            if let Some(sig) = part.get("thoughtSignature") {
                                thought_signatures.push(sig.clone());
                            }
                            if let Some(text) = part["text"].as_str() {
                                content.push(Content::Text {
                                    text: text.to_string(),
                                });
                            } else if let Some(t) = part["thought"].as_str() {
                                content.push(Content::Reasoning {
                                    text: t.to_string(),
                                });
                            } else if let Some(t) = part["thought"]["text"].as_str() {
                                content.push(Content::Reasoning {
                                    text: t.to_string(),
                                });
                            } else if let Some(fc) = part.get("functionCall") {
                                content.push(Content::ToolCall {
                                    id: fc["id"].as_str().unwrap_or("").to_string(),
                                    name: fc["name"].as_str().unwrap_or("").to_string(),
                                    arguments: fc["args"].clone(),
                                });
                            }
                        }
                    }
                }
            }
        }
        if !thought_signatures.is_empty() {
            extensions.insert(
                "gemini_thought_signatures".to_string(),
                json!(thought_signatures),
            );
        }
        let finish_reason = body["candidates"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|c| c["finishReason"].as_str())
            .map(|s| match s {
                "STOP" => FinishReason::Stop,
                "MAX_TOKENS" => FinishReason::Length,
                "SAFETY" => FinishReason::ContentFilter,
                other => FinishReason::Other(other.to_string()),
            });
        let usage = body.get("usageMetadata").map(|u| Usage {
            prompt_tokens: u["promptTokenCount"].as_u64().unwrap_or(0),
            completion_tokens: u["candidatesTokenCount"].as_u64().unwrap_or(0),
            total_tokens: u["totalTokenCount"].as_u64().unwrap_or(0),
            reasoning_tokens: u["thoughtsTokenCount"].as_u64(),
            cache_read_tokens: u["cachedContentTokenCount"].as_u64(),
            ..Default::default()
        });
        Ok(IrResponse {
            content,
            usage,
            finish_reason,
            response_id,
            stop_details: None,
            extensions,
        })
    }
}

pub struct GeminiStreamEncoder;
impl StreamEncoder for GeminiStreamEncoder {
    fn encode_part(&mut self, part: &StreamPart) -> Result<Vec<u8>, tiygate_core::Error> {
        let chunk = match part {
            StreamPart::TextDelta { text } => format!(
                "data: {}\n\n",
                json!({"candidates": [{"content": {"role": "model", "parts": [{"text": text}]}}]})
            ),
            StreamPart::ReasoningDelta { text } => format!(
                "data: {}\n\n",
                json!({"candidates": [{"content": {"parts": [{"thought": text}]}}]})
            ),
            StreamPart::ToolCallDelta {
                name, arguments, ..
            } => {
                if let Some(n) = name {
                    format!(
                        "data: {}\n\n",
                        json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": n, "args": json!({})}}]}}]})
                    )
                } else {
                    format!(
                        "data: {}\n\n",
                        json!({"candidates": [{"content": {"parts": [{"functionCall": {"args": {"_partial": arguments}}}]}}]})
                    )
                }
            }
            StreamPart::Usage { usage } => {
                let mut um = json!({
                    "promptTokenCount": usage.prompt_tokens,
                    "candidatesTokenCount": usage.completion_tokens,
                    "totalTokenCount": usage.total_tokens,
                });
                if let Some(rt) = usage.reasoning_tokens {
                    if rt > 0 {
                        um["thoughtsTokenCount"] = json!(rt);
                    }
                }
                if let Some(cr) = usage.cache_read_tokens {
                    if cr > 0 {
                        um["cachedContentTokenCount"] = json!(cr);
                    }
                }
                format!("data: {}\n\n", json!({"usageMetadata": um}))
            }
            StreamPart::Finish { reason } => {
                let fr = match reason {
                    FinishReason::Stop => "STOP",
                    FinishReason::Length => "MAX_TOKENS",
                    FinishReason::ContentFilter => "SAFETY",
                    FinishReason::ToolCalls => "TOOL_CALLS",
                    FinishReason::Other(_) => "STOP",
                };
                format!(
                    "data: {}\n\n",
                    json!({"candidates": [{"finishReason": fr}]})
                )
            }
            StreamPart::Error { message, .. } => format!(
                "data: {}\n\n",
                json!({"error": {"message": message, "status": "INTERNAL"}})
            ),
            StreamPart::ResponseStarted { id } => {
                format!("data: {}\n\n", json!({"responseId": id}))
            }
            StreamPart::ResponseCompleted { id, .. } => {
                format!("data: {}\n\n", json!({"responseId": id, "done": true}))
            }
        };
        Ok(chunk.into_bytes())
    }
    fn encode_error(&mut self, message: &str, _code: Option<&str>) -> Vec<u8> {
        format!(
            "data: {}\n\n",
            json!({"error": {"message": message, "status": "INTERNAL"}})
        )
        .into_bytes()
    }
    fn encode_done(&mut self) -> Vec<u8> {
        Vec::new()
    }
}

pub struct GeminiStreamDecoder {
    response_id: Option<String>,
}
impl Default for GeminiStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl GeminiStreamDecoder {
    pub fn new() -> Self {
        Self { response_id: None }
    }
}

impl StreamDecoder for GeminiStreamDecoder {
    fn feed(&mut self, line: &str) -> Result<Vec<StreamPart>, tiygate_core::Error> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(vec![]);
        }
        let data = if let Some(s) = line.strip_prefix("data: ") {
            s
        } else {
            return Ok(vec![]);
        };
        let data = if data.starts_with('[') {
            serde_json::from_str::<Vec<Value>>(data)
                .ok()
                .and_then(|a| a.first().cloned())
                .map(|v| serde_json::to_string(&v).unwrap_or_default())
                .unwrap_or_default()
        } else {
            data.to_string()
        };

        let event: Value = serde_json::from_str(&data)
            .map_err(|e| tiygate_core::Error::Codec(format!("Gemini SSE: {}", e)))?;
        let mut parts = Vec::new();

        if self.response_id.is_none() {
            if let Some(id) = event["responseId"].as_str() {
                self.response_id = Some(id.to_string());
                parts.push(StreamPart::ResponseStarted { id: id.to_string() });
            }
        }
        if event.get("error").is_some() {
            parts.push(StreamPart::Error {
                message: event["error"]["message"]
                    .as_str()
                    .unwrap_or("Unknown")
                    .to_string(),
                code: event["error"]["status"].as_str().map(String::from),
            });
            return Ok(parts);
        }
        if let Some(candidates) = event["candidates"].as_array() {
            for c in candidates {
                if let Some(parts_arr) = c["content"]["parts"].as_array() {
                    for p in parts_arr {
                        if let Some(text) = p["text"].as_str() {
                            parts.push(StreamPart::TextDelta {
                                text: text.to_string(),
                            });
                        }
                        if let Some(t) = p["thought"]
                            .as_str()
                            .or_else(|| p["thought"]["text"].as_str())
                        {
                            parts.push(StreamPart::ReasoningDelta {
                                text: t.to_string(),
                            });
                        }
                        if let Some(fc) = p.get("functionCall") {
                            parts.push(StreamPart::ToolCallDelta {
                                id: String::new(),
                                name: fc["name"].as_str().map(String::from),
                                arguments: serde_json::to_string(&fc["args"]).unwrap_or_default(),
                            });
                        }
                    }
                }
                if let Some(fr) = c["finishReason"].as_str() {
                    let reason = match fr {
                        "STOP" => FinishReason::Stop,
                        "MAX_TOKENS" => FinishReason::Length,
                        "SAFETY" => FinishReason::ContentFilter,
                        o => FinishReason::Other(o.to_string()),
                    };
                    parts.push(StreamPart::Finish { reason });
                }
            }
        }
        if let Some(usage) = event.get("usageMetadata") {
            parts.push(StreamPart::Usage {
                usage: Usage {
                    prompt_tokens: usage["promptTokenCount"].as_u64().unwrap_or(0),
                    completion_tokens: usage["candidatesTokenCount"].as_u64().unwrap_or(0),
                    total_tokens: usage["totalTokenCount"].as_u64().unwrap_or(0),
                    ..Default::default()
                },
            });
        }
        Ok(parts)
    }

    fn finish(&mut self) -> Result<Vec<StreamPart>, tiygate_core::Error> {
        if let Some(id) = self.response_id.take() {
            Ok(vec![StreamPart::ResponseCompleted {
                id,
                status: "completed".to_string(),
                usage: None,
            }])
        } else {
            Ok(vec![])
        }
    }
}

inventory::submit! { tiygate_core::CodecRegistration { make: || Box::new(GeminiCodec::new()) } }

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make_raw_env() -> RawEnvelope {
        RawEnvelope {
            method: "POST".to_string(),
            path: "/v1beta/models/gemini:generateContent".to_string(),
            headers: std::collections::HashMap::new(),
            body: None,
            truncated: false,
            original_body_size: 0,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_decode_basic_request() {
        let codec = GeminiCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "models/gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "Hello"}]}],
            "generationConfig": {"maxOutputTokens": 100}
        });
        let ir = codec.decode_request(body, &env).unwrap();
        assert_eq!(ir.model, "models/gemini-2.0-flash");
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.params.max_tokens, Some(100));
    }

    #[test]
    fn test_encode_response() {
        let codec = GeminiCodec::new();
        let ir = IrResponse {
            content: vec![Content::Text {
                text: "Hi!".to_string(),
            }],
            usage: Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 3,
                total_tokens: 8,
                ..Default::default()
            }),
            finish_reason: Some(FinishReason::Stop),
            response_id: None,
            stop_details: None,
            extensions: Default::default(),
        };
        let encoded = codec.encode_response(&ir).unwrap();
        assert_eq!(
            encoded["candidates"][0]["content"]["parts"][0]["text"],
            "Hi!"
        );
        assert_eq!(encoded["usageMetadata"]["promptTokenCount"], 5);
    }

    #[test]
    fn test_stream_encoder_error_frame() {
        let mut encoder = GeminiStreamEncoder;
        let err = encoder.encode_error("rate limit", Some("429"));
        let s = String::from_utf8_lossy(&err);
        assert!(s.contains("error"));
        assert!(s.contains("rate limit"));
    }

    #[test]
    fn test_stream_encoder_all_variants() {
        let mut encoder = GeminiStreamEncoder;
        let variants: &[StreamPart] = &[
            StreamPart::ResponseStarted {
                id: "r1".to_string(),
            },
            StreamPart::TextDelta {
                text: "hi".to_string(),
            },
            StreamPart::ReasoningDelta {
                text: "think".to_string(),
            },
            StreamPart::ToolCallDelta {
                id: "t1".to_string(),
                name: Some("f".to_string()),
                arguments: "{}".to_string(),
            },
            StreamPart::Usage {
                usage: Usage::default(),
            },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
            StreamPart::Error {
                message: "e".to_string(),
                code: Some("500".to_string()),
            },
            StreamPart::ResponseCompleted {
                id: "r1".to_string(),
                status: "ok".to_string(),
                usage: None,
            },
        ];
        for v in variants {
            assert!(encoder.encode_part(v).is_ok());
        }
    }

    #[test]
    fn test_snapshot_decode_request() {
        let codec = GeminiCodec::new();
        let env = make_raw_env();
        let body = json!({"model": "models/gemini-2.0-flash", "contents": [{"role": "user", "parts": [{"text": "Hello"}]}]});
        let ir = codec.decode_request(body, &env).unwrap();
        insta::assert_debug_snapshot!(ir);
    }

    #[test]
    fn test_codec_capabilities() {
        let codec = GeminiCodec::new();
        assert!(codec.capabilities().streaming);
        assert!(codec.capabilities().tools);
        assert!(codec.capabilities().lossy_default_reject);
    }

    #[test]
    fn test_stream_usage_includes_total_and_cached() {
        // Gemini 流式 Usage 帧带 totalTokenCount / thoughtsTokenCount / cachedContentTokenCount
        let mut enc = GeminiStreamEncoder;
        let usage = Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            reasoning_tokens: Some(20),
            cache_read_tokens: Some(8),
            cache_write_tokens: None,
        };
        let bytes = enc.encode_part(&StreamPart::Usage { usage }).unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\"totalTokenCount\":15"));
        assert!(s.contains("\"thoughtsTokenCount\":20"));
        assert!(s.contains("\"cachedContentTokenCount\":8"));
    }
}
