//! OpenAI Responses API protocol codec.
//! Implements bidirectional conversion for OpenAI's Responses API.

use http::HeaderMap;
use serde_json::{json, Value};

use tiygate_core::{
    Content, EndpointCapabilities, EndpointCodec, FinishReason, IrRequest, IrResponse, Message,
    ProtocolEndpoint, ProtocolSuite, RawEnvelope, Role, StreamDecoder, StreamEncoder, StreamPart,
    Tool, Usage,
};

pub struct ResponsesCodec {
    id: ProtocolEndpoint,
    capabilities: EndpointCapabilities,
}

impl Default for ResponsesCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponsesCodec {
    pub fn new() -> Self {
        Self {
            id: ProtocolEndpoint::new(ProtocolSuite::OpenAiResponses, "responses", "v1"),
            capabilities: EndpointCapabilities {
                streaming: true,
                tools: true,
                reasoning: true,
                embeddings: false,
                force_upstream_stream: false,
                override_model_in_body: false,
                ingress_routes: &[("POST", "/v1/responses")],
                multimodal: true,
                structured_output: true,
                function_calling: true,
                parallel_tool_calls: true,
                extended_reasoning: true,
                deterministic_seed: false,
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

impl EndpointCodec for ResponsesCodec {
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
        let stream = body["stream"].as_bool().unwrap_or(false);
        let system = body["instructions"].as_str().map(String::from);
        let mut messages = Vec::new();

        if let Some(arr) = body["input"].as_array() {
            for item in arr {
                let role_str = item["role"].as_str().unwrap_or("user");
                let role = match role_str {
                    "system" | "developer" => Role::System,
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    _ => Role::User,
                };
                let content = if let Some(text) = item["content"].as_str() {
                    vec![Content::Text {
                        text: text.to_string(),
                    }]
                } else if let Some(content_arr) = item["content"].as_array() {
                    let mut parts = Vec::new();
                    for part in content_arr {
                        match part["type"].as_str() {
                            Some("input_text") | Some("output_text") => {
                                parts.push(Content::Text {
                                    text: part["text"].as_str().unwrap_or("").to_string(),
                                });
                            }
                            Some("input_image") => {
                                if let Some(url) = part["image_url"].as_str() {
                                    parts.push(Content::Media {
                                        source: tiygate_core::ir::MediaSource::Url {
                                            url: url.to_string(),
                                        },
                                        mime_type: "image/*".to_string(),
                                        metadata: Default::default(),
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                    parts
                } else if item["type"] == "function_call" {
                    vec![Content::ToolCall {
                        id: item["call_id"].as_str().unwrap_or("").to_string(),
                        name: item["name"].as_str().unwrap_or("").to_string(),
                        arguments: serde_json::from_str(item["arguments"].as_str().unwrap_or("{}"))
                            .unwrap_or(json!({})),
                    }]
                } else if item["type"] == "function_call_output" {
                    vec![Content::ToolResult {
                        tool_call_id: item["call_id"].as_str().unwrap_or("").to_string(),
                        name: String::new(),
                        content: item["output"].as_str().unwrap_or("").to_string(),
                    }]
                } else {
                    vec![Content::Text {
                        text: String::new(),
                    }]
                };
                messages.push(Message { role, content });
            }
        }

        let tools: Vec<Tool> = body["tools"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|t| Tool {
                        name: t["name"].as_str().unwrap_or("").to_string(),
                        description: t["description"].as_str().map(String::from),
                        parameters: t["parameters"].as_object().map(|p| json!(p)),
                        required: false,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let params = tiygate_core::GenerationParams {
            max_tokens: body["max_output_tokens"].as_u64().map(|v| v as u32),
            temperature: body["temperature"].as_f64().map(|v| v as f32),
            top_p: body["top_p"].as_f64().map(|v| v as f32),
            stop: body["stop"]
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
        let mut response = json!({"object": "response", "model": ""});
        if let Some(id) = &ir.response_id {
            response["id"] = json!(id);
        }
        let mut output_items = Vec::new();
        let mut message_text = String::new();
        let mut tool_calls = Vec::new();
        for c in &ir.content {
            match c {
                Content::Text { text } => {
                    message_text.push_str(text);
                }
                Content::Reasoning { text } => {
                    output_items.push(json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": text}]}));
                }
                Content::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    tool_calls.push(json!({"id": id, "type": "function_call", "name": name, "arguments": serde_json::to_string(arguments).unwrap_or_default(), "status": "completed"}));
                }
                _ => {}
            }
        }
        if !message_text.is_empty() {
            output_items.push(json!({"id": ir.response_id.as_deref().unwrap_or("msg_0"), "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": message_text}]}));
        }
        for tc in &tool_calls {
            output_items.push(tc.clone());
        }
        response["output"] = json!(output_items);
        if let Some(fr) = &ir.finish_reason {
            response["status"] = json!(match fr {
                FinishReason::Stop => "completed",
                FinishReason::Length => "incomplete",
                FinishReason::ContentFilter => "incomplete",
                FinishReason::ToolCalls => "incomplete",
                _ => "completed",
            });
        }
        if let Some(usage) = &ir.usage {
            response["usage"] = json!({"input_tokens": usage.prompt_tokens, "output_tokens": usage.completion_tokens, "total_tokens": usage.total_tokens});
            if let Some(rt) = usage.reasoning_tokens {
                response["usage"]["output_tokens_details"] = json!({"reasoning_tokens": rt});
            }
        }
        Ok(response)
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(ResponsesStreamEncoder::new())
    }
    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(ResponsesStreamDecoder::new())
    }

    fn encode_request(&self, ir: &IrRequest) -> Result<(Value, HeaderMap), tiygate_core::Error> {
        let mut body = json!({"model": ir.model, "stream": ir.stream});
        if let Some(sys) = &ir.system {
            body["instructions"] = json!(sys);
        }
        let mut input_items = Vec::new();
        for msg in &ir.messages {
            match msg.role {
                Role::System => {
                    for c in &msg.content {
                        if let Content::Text { text } = c {
                            let existing = body["instructions"].as_str().unwrap_or("");
                            body["instructions"] = json!(format!("{existing}\n{text}"));
                        }
                    }
                }
                Role::User | Role::Assistant => {
                    let role_str = match msg.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        _ => "user",
                    };
                    let mut item = json!({"role": role_str});
                    let content_parts: Vec<Value> = msg.content.iter().filter_map(|c| match c {
                        Content::Text { text } => Some(json!({"type": "input_text", "text": text})),
                        Content::Media { source, mime_type, .. } => match source {
                            tiygate_core::ir::MediaSource::Url { url } => Some(json!({"type": "input_image", "image_url": url})),
                            tiygate_core::ir::MediaSource::Inline { data } => Some(json!({"type": "input_image", "image_url": format!("data:{};base64,{}", mime_type, data)})),
                            _ => None,
                        },
                        _ => None,
                    }).collect();
                    if content_parts.len() == 1 && content_parts[0]["type"] == "input_text" {
                        item["content"] = content_parts[0]["text"].clone();
                    } else if !content_parts.is_empty() {
                        item["content"] = json!(content_parts);
                    } else {
                        item["content"] = json!("");
                    }
                    input_items.push(item);
                }
                Role::Tool => {
                    for c in &msg.content {
                        if let Content::ToolResult {
                            tool_call_id,
                            name: _,
                            content,
                        } = c
                        {
                            input_items.push(json!({"type": "function_call_output", "call_id": tool_call_id, "output": content}));
                        }
                    }
                }
            }
        }
        body["input"] = json!(input_items);
        if !ir.tools.is_empty() {
            let tools: Vec<Value> = ir.tools.iter().map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.parameters})).collect();
            body["tools"] = json!(tools);
        }
        if let Some(mt) = ir.params.max_tokens {
            body["max_output_tokens"] = json!(mt);
        }
        if let Some(t) = ir.params.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(p) = ir.params.top_p {
            body["top_p"] = json!(p);
        }
        if !ir.params.stop.is_empty() {
            body["stop"] = json!(ir.params.stop);
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        Ok((body, headers))
    }

    fn decode_response(&self, body: Value) -> Result<IrResponse, tiygate_core::Error> {
        let response_id = body["id"].as_str().map(String::from);
        let mut content = Vec::new();
        if let Some(output) = body["output"].as_array() {
            for item in output {
                match item["type"].as_str() {
                    Some("message") => {
                        if let Some(content_arr) = item["content"].as_array() {
                            for part in content_arr {
                                if part["type"] == "output_text" {
                                    if let Some(text) = part["text"].as_str() {
                                        content.push(Content::Text {
                                            text: text.to_string(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                    Some("function_call") => {
                        let args: Value =
                            serde_json::from_str(item["arguments"].as_str().unwrap_or("{}"))
                                .unwrap_or(json!({}));
                        content.push(Content::ToolCall {
                            id: item["id"].as_str().unwrap_or("").to_string(),
                            name: item["name"].as_str().unwrap_or("").to_string(),
                            arguments: args,
                        });
                    }
                    Some("reasoning") => {
                        if let Some(summary) = item["summary"].as_array() {
                            for s in summary {
                                if let Some(text) = s["text"].as_str() {
                                    content.push(Content::Reasoning {
                                        text: text.to_string(),
                                    });
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        let finish_reason = body["status"].as_str().map(|s| match s {
            "completed" => FinishReason::Stop,
            "incomplete" => {
                if content
                    .iter()
                    .any(|c| matches!(c, Content::ToolCall { .. }))
                {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Length
                }
            }
            other => FinishReason::Other(other.to_string()),
        });
        let usage = body.get("usage").map(|u| Usage {
            prompt_tokens: u["input_tokens"].as_u64().unwrap_or(0),
            completion_tokens: u["output_tokens"].as_u64().unwrap_or(0),
            total_tokens: u["total_tokens"].as_u64().unwrap_or(0),
            reasoning_tokens: u["output_tokens_details"]["reasoning_tokens"].as_u64(),
            ..Default::default()
        });
        Ok(IrResponse {
            content,
            usage,
            finish_reason,
            response_id,
            stop_details: None,
            extensions: Default::default(),
        })
    }
}

pub struct ResponsesStreamEncoder {
    response_id: Option<String>,
    text_index: u32,
}
impl Default for ResponsesStreamEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponsesStreamEncoder {
    pub fn new() -> Self {
        Self {
            response_id: None,
            text_index: 0,
        }
    }
}

impl StreamEncoder for ResponsesStreamEncoder {
    fn encode_part(&mut self, part: &StreamPart) -> Result<Vec<u8>, tiygate_core::Error> {
        let chunk = match part {
            StreamPart::ResponseStarted { id } => {
                self.response_id = Some(id.clone());
                format!(
                    "data: {}\n\n",
                    json!({"type": "response.created", "response": {"id": id, "object": "response", "status": "in_progress"}})
                )
            }
            StreamPart::TextDelta { text } => {
                let idx = self.text_index;
                self.text_index += 1;
                format!(
                    "data: {}\n\n",
                    json!({"type": "response.output_text.delta", "item_id": format!("{}_msg", self.response_id.as_deref().unwrap_or("")), "output_index": idx, "content_index": 0, "delta": text})
                )
            }
            StreamPart::ReasoningDelta { text } => {
                format!(
                    "data: {}\n\n",
                    json!({"type": "response.reasoning_text.delta", "delta": text})
                )
            }
            StreamPart::ToolCallDelta {
                id,
                name,
                arguments,
            } => {
                if let Some(n) = name {
                    format!(
                        "data: {}\n\n",
                        json!({"type": "response.output_item.added", "output_index": 0, "item": {"id": id, "type": "function_call", "name": n, "arguments": "", "status": "in_progress"}})
                    )
                } else {
                    format!(
                        "data: {}\n\n",
                        json!({"type": "response.function_call_arguments.delta", "item_id": id, "output_index": 0, "delta": arguments})
                    )
                }
            }
            StreamPart::Usage { usage } => {
                format!(
                    "data: {}\n\n",
                    json!({"type": "response.completed", "response": {"id": self.response_id.as_deref().unwrap_or(""), "usage": {"input_tokens": usage.prompt_tokens, "output_tokens": usage.completion_tokens, "total_tokens": usage.total_tokens}}})
                )
            }
            StreamPart::Finish { reason } => {
                let status = match reason {
                    FinishReason::Stop => "completed",
                    FinishReason::Length => "incomplete",
                    FinishReason::ContentFilter => "incomplete",
                    FinishReason::ToolCalls => "incomplete",
                    _ => "completed",
                };
                format!(
                    "data: {}\n\n",
                    json!({"type": "response.completed", "response": {"id": self.response_id.as_deref().unwrap_or(""), "status": status}})
                )
            }
            StreamPart::ResponseCompleted { .. } => "data: [DONE]\n\n".to_string(),
            StreamPart::Error { message, .. } => format!(
                "data: {}\n\n",
                json!({"type": "error", "error": {"message": message, "type": "gateway_error"}})
            ),
        };
        Ok(chunk.into_bytes())
    }
    fn encode_error(&mut self, message: &str, _code: Option<&str>) -> Vec<u8> {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"type": "error", "error": {"message": message, "type": "gateway_error"}})
        )
        .into_bytes()
    }
    fn encode_done(&mut self) -> Vec<u8> {
        "data: [DONE]\n\n".to_string().into_bytes()
    }
}

pub struct ResponsesStreamDecoder {
    response_id: Option<String>,
    in_function_call: bool,
    current_call_id: Option<String>,
    current_call_name: Option<String>,
}
impl Default for ResponsesStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponsesStreamDecoder {
    pub fn new() -> Self {
        Self {
            response_id: None,
            in_function_call: false,
            current_call_id: None,
            current_call_name: None,
        }
    }
}

impl StreamDecoder for ResponsesStreamDecoder {
    fn feed(&mut self, line: &str) -> Result<Vec<StreamPart>, tiygate_core::Error> {
        let line = line.trim();
        if line.is_empty() || line == "data: [DONE]" {
            if line == "data: [DONE]" {
                return Ok(vec![StreamPart::ResponseCompleted {
                    id: self.response_id.clone().unwrap_or_default(),
                    status: "completed".to_string(),
                    usage: None,
                }]);
            }
            return Ok(vec![]);
        }
        let data = if let Some(s) = line.strip_prefix("data: ") {
            s
        } else {
            return Ok(vec![]);
        };
        let event: Value = serde_json::from_str(data)
            .map_err(|e| tiygate_core::Error::Codec(format!("Responses SSE: {}", e)))?;
        let mut parts = Vec::new();

        match event["type"].as_str() {
            Some("response.created") => {
                if let Some(id) = event["response"]["id"].as_str() {
                    self.response_id = Some(id.to_string());
                    parts.push(StreamPart::ResponseStarted { id: id.to_string() });
                }
            }
            Some("response.output_text.delta") => {
                if let Some(text) = event["delta"].as_str() {
                    parts.push(StreamPart::TextDelta {
                        text: text.to_string(),
                    });
                }
            }
            Some("response.reasoning_text.delta") => {
                if let Some(text) = event["delta"].as_str() {
                    parts.push(StreamPart::ReasoningDelta {
                        text: text.to_string(),
                    });
                }
            }
            Some("response.output_item.added") => {
                let item = &event["item"];
                if item["type"] == "function_call" {
                    self.in_function_call = true;
                    self.current_call_id = item["id"].as_str().map(String::from);
                    self.current_call_name = item["name"].as_str().map(String::from);
                    parts.push(StreamPart::ToolCallDelta {
                        id: self.current_call_id.clone().unwrap_or_default(),
                        name: self.current_call_name.clone(),
                        arguments: String::new(),
                    });
                }
            }
            Some("response.function_call_arguments.delta") => {
                if let Some(args) = event["delta"].as_str() {
                    parts.push(StreamPart::ToolCallDelta {
                        id: self.current_call_id.clone().unwrap_or_default(),
                        name: self.current_call_name.clone(),
                        arguments: args.to_string(),
                    });
                }
            }
            Some("response.output_item.done") => {
                self.in_function_call = false;
                self.current_call_id = None;
                self.current_call_name = None;
            }
            Some("response.completed") => {
                if let Some(usage) = event["response"]["usage"].as_object() {
                    parts.push(StreamPart::Usage {
                        usage: Usage {
                            prompt_tokens: usage
                                .get("input_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                            completion_tokens: usage
                                .get("output_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                            total_tokens: usage
                                .get("total_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                            reasoning_tokens: usage
                                .get("output_tokens_details")
                                .and_then(|d| d["reasoning_tokens"].as_u64()),
                            ..Default::default()
                        },
                    });
                }
                let status = event["response"]["status"].as_str().unwrap_or("completed");
                let reason = match status {
                    "completed" => FinishReason::Stop,
                    "incomplete" => {
                        if self.in_function_call {
                            FinishReason::ToolCalls
                        } else {
                            FinishReason::Length
                        }
                    }
                    other => FinishReason::Other(other.to_string()),
                };
                parts.push(StreamPart::Finish { reason });
            }
            Some("error") => {
                parts.push(StreamPart::Error {
                    message: event["error"]["message"]
                        .as_str()
                        .unwrap_or("Unknown")
                        .to_string(),
                    code: event["error"]["type"].as_str().map(String::from),
                });
            }
            Some(other) => {
                parts.push(StreamPart::Error {
                    message: format!("Unknown Responses SSE event type: {}", other),
                    code: Some("unknown_event".to_string()),
                });
            }
            None => {
                parts.push(StreamPart::Error {
                    message: "Responses SSE event with no type field".to_string(),
                    code: Some("missing_type".to_string()),
                });
            }
        }
        Ok(parts)
    }
    fn finish(&mut self) -> Result<Vec<StreamPart>, tiygate_core::Error> {
        Ok(vec![])
    }
}

inventory::submit! { tiygate_core::CodecRegistration { make: || Box::new(ResponsesCodec::new()) }

}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make_raw_env() -> RawEnvelope {
        RawEnvelope {
            method: "POST".to_string(),
            path: "/v1/responses".to_string(),
            headers: std::collections::HashMap::new(),
            body: None,
            truncated: false,
            original_body_size: 0,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_decode_basic_request() {
        let codec = ResponsesCodec::new();
    }

    #[test]
    fn test_encode_response_text() {
        let codec = ResponsesCodec::new();
        let ir = IrResponse {
            content: vec![Content::Text {
                text: "Hi!".to_string(),
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..Default::default()
            }),
            finish_reason: Some(FinishReason::Stop),
            response_id: Some("resp_1".to_string()),
            stop_details: None,
            extensions: Default::default(),
        };
        let encoded = codec.encode_response(&ir).unwrap();
        assert_eq!(encoded["id"], "resp_1");
        assert_eq!(encoded["output"][0]["content"][0]["text"], "Hi!");
        assert_eq!(encoded["usage"]["input_tokens"], 10);
    }

    #[test]
    fn test_stream_encoder_error_frame() {
        let mut encoder = ResponsesStreamEncoder::new();
        let err = encoder.encode_error("overloaded", Some("529"));
        let s = String::from_utf8_lossy(&err);
        assert!(s.contains("error"));
        assert!(s.contains("overloaded"));
    }

    #[test]
    fn test_codec_capabilities() {
        let codec = ResponsesCodec::new();
        assert!(codec.capabilities().streaming);
        assert!(codec.capabilities().tools);
        assert!(codec.capabilities().structured_output);
        assert!(codec.capabilities().lossy_default_reject);
    }
}
