//! Anthropic Messages protocol codec.
//!
//! Implements bidirectional conversion between Anthropic's Messages API
//! and the canonical IR. Supports both streaming (SSE) and non-streaming modes.

use http::HeaderMap;
use serde_json::{json, Value};

use tiygate_core::{
    Content, EndpointCapabilities, EndpointCodec, FinishReason, IrRequest, IrResponse, Message,
    ProtocolEndpoint, ProtocolSuite, RawEnvelope, Role, StreamDecoder, StreamEncoder, StreamPart,
    Tool, Usage,
};

pub struct MessagesCodec {
    id: ProtocolEndpoint,
    capabilities: EndpointCapabilities,
}

impl Default for MessagesCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl MessagesCodec {
    pub fn new() -> Self {
        Self {
            id: ProtocolEndpoint::new(ProtocolSuite::AnthropicMessages, "messages", "2023-06-01"),
            capabilities: EndpointCapabilities {
                streaming: true,
                tools: true,
                reasoning: true,
                embeddings: false,
                force_upstream_stream: false,
                override_model_in_body: false,
                ingress_routes: &[("POST", "/v1/messages")],
                multimodal: true,
                structured_output: false,
                function_calling: true,
                // §1 of docs/protocol-capability-matrix.md: parallel tool
                // calls are lossy when crossing chat→messages (Anthropic
                // models can return multiple tool_use blocks in one response,
                // but the chat-completions semantics of "fire N tools
                // concurrently" are not preserved). Mark as unsupported so
                // `check_lossy_conversion` rejects the crossing.
                parallel_tool_calls: false,
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

impl EndpointCodec for MessagesCodec {
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

        let mut messages = Vec::new();

        // System prompt (can be string or array of text blocks)
        let system = if let Some(sys) = body.get("system") {
            if let Some(s) = sys.as_str() {
                Some(s.to_string())
            } else if let Some(arr) = sys.as_array() {
                arr.iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into()
            } else {
                None
            }
        } else {
            None
        };

        // Parse messages
        if let Some(arr) = body["messages"].as_array() {
            for msg in arr {
                let role = match msg["role"].as_str().unwrap_or("user") {
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    _ => Role::User,
                };

                let content = if let Some(arr) = msg["content"].as_array() {
                    let mut parts = Vec::new();
                    for block in arr {
                        match block["type"].as_str() {
                            Some("text") => {
                                parts.push(Content::Text {
                                    text: block["text"].as_str().unwrap_or("").to_string(),
                                });
                            }
                            Some("tool_use") => {
                                parts.push(Content::ToolCall {
                                    id: block["id"].as_str().unwrap_or("").to_string(),
                                    name: block["name"].as_str().unwrap_or("").to_string(),
                                    arguments: block["input"].clone(),
                                });
                            }
                            Some("tool_result") => {
                                parts.push(Content::ToolResult {
                                    tool_call_id: block["tool_use_id"]
                                        .as_str()
                                        .unwrap_or("")
                                        .to_string(),
                                    name: String::new(),
                                    content: block["content"].as_str().unwrap_or("").to_string(),
                                });
                            }
                            Some("image") => {
                                let source = block["source"].clone();
                                if source["type"] == "url" {
                                    parts.push(Content::Media {
                                        source: tiygate_core::ir::MediaSource::Url {
                                            url: source["url"].as_str().unwrap_or("").to_string(),
                                        },
                                        mime_type: source["media_type"]
                                            .as_str()
                                            .unwrap_or("image/*")
                                            .to_string(),
                                        metadata: Default::default(),
                                    });
                                } else {
                                    parts.push(Content::Media {
                                        source: tiygate_core::ir::MediaSource::Inline {
                                            data: source["data"].as_str().unwrap_or("").to_string(),
                                        },
                                        mime_type: source["media_type"]
                                            .as_str()
                                            .unwrap_or("image/*")
                                            .to_string(),
                                        metadata: Default::default(),
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                    parts
                } else if let Some(text) = msg["content"].as_str() {
                    vec![Content::Text {
                        text: text.to_string(),
                    }]
                } else {
                    vec![]
                };

                messages.push(Message { role, content });
            }
        }

        // Parse tools
        let tools: Vec<Tool> = body["tools"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|t| Tool {
                        name: t["name"].as_str().unwrap_or("").to_string(),
                        description: t["description"].as_str().map(|s| s.to_string()),
                        parameters: Some(t["input_schema"].clone()),
                        required: false,
                    })
                    .collect()
            })
            .unwrap_or_default();

        let params = tiygate_core::GenerationParams {
            max_tokens: body["max_tokens"].as_u64().map(|v| v as u32),
            temperature: body["temperature"].as_f64().map(|v| v as f32),
            top_p: body["top_p"].as_f64().map(|v| v as f32),
            top_k: body["top_k"].as_u64().map(|v| v as u32),
            stop: body["stop_sequences"]
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

    fn encode_response(&self, ir: &IrResponse) -> Result<serde_json::Value, tiygate_core::Error> {
        let mut response = json!({
            "type": "message",
            "role": "assistant",
            "model": "",
        });

        if let Some(id) = &ir.response_id {
            response["id"] = json!(id);
        }

        let mut content_blocks = Vec::new();

        for c in &ir.content {
            match c {
                Content::Text { text } => {
                    content_blocks.push(json!({
                        "type": "text",
                        "text": text,
                    }));
                }
                Content::Reasoning { text } => {
                    content_blocks.push(json!({
                        "type": "thinking",
                        "thinking": text,
                    }));
                }
                Content::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    content_blocks.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": arguments,
                    }));
                }
                _ => {}
            }
        }

        response["content"] = json!(content_blocks);

        if let Some(fr) = &ir.finish_reason {
            response["stop_reason"] = json!(match fr {
                FinishReason::Stop => "end_turn",
                FinishReason::Length => "max_tokens",
                FinishReason::ToolCalls => "tool_use",
                FinishReason::ContentFilter => "content_filter",
                FinishReason::Other(_) => "end_turn",
            });
        }

        if let Some(sd) = &ir.stop_details {
            response["stop_reason"] = json!(&sd.stop_reason);
        }

        if let Some(usage) = &ir.usage {
            response["usage"] = json!({
                "input_tokens": usage.prompt_tokens,
                "output_tokens": usage.completion_tokens,
            });
            if let Some(ct) = usage.cache_read_tokens {
                response["usage"]["cache_read_input_tokens"] = json!(ct);
            }
            if let Some(cw) = usage.cache_write_tokens {
                response["usage"]["cache_creation_input_tokens"] = json!(cw);
            }
        }

        Ok(response)
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(MessagesStreamEncoder::new())
    }

    fn encode_request(
        &self,
        ir: &IrRequest,
    ) -> Result<(serde_json::Value, HeaderMap), tiygate_core::Error> {
        let mut body = json!({
            "model": ir.model,
            "stream": ir.stream,
            "max_tokens": ir.params.max_tokens.unwrap_or(1024),
        });

        // System prompt
        if let Some(sys) = &ir.system {
            body["system"] = json!(sys);
        }

        // Messages
        let messages: Vec<Value> = ir
            .messages
            .iter()
            .map(|msg| {
                let mut m = json!({
                    "role": match msg.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        _ => "user",
                    },
                });

                let blocks: Vec<Value> = msg
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(json!({"type": "text", "text": text})),
                        Content::ToolCall {
                            id,
                            name,
                            arguments,
                        } => Some(
                            json!({"type": "tool_use", "id": id, "name": name, "input": arguments}),
                        ),
                        Content::ToolResult {
                            tool_call_id,
                            name: _,
                            content,
                        } => Some(json!({
                            "type": "tool_result",
                            "tool_use_id": tool_call_id,
                            "content": content,
                        })),
                        Content::Media {
                            source, mime_type, ..
                        } => match source {
                            tiygate_core::ir::MediaSource::Url { url } => Some(json!({
                                "type": "image",
                                "source": {"type": "url", "url": url, "media_type": mime_type

}
                            })),
                            tiygate_core::ir::MediaSource::Inline { data } => Some(json!({
                                "type": "image",
                                "source": {"type": "base64", "media_type": mime_type, "data": data

}
                            })),
                            _ => None,
                        },
                        _ => None,
                    })
                    .collect();

                m["content"] = json!(blocks);
                m
            })
            .collect();

        body["messages"] = json!(messages);

        // Tools
        if !ir.tools.is_empty() {
            let tools: Vec<Value> = ir
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }

        // Other params
        if let Some(t) = ir.params.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(p) = ir.params.top_p {
            body["top_p"] = json!(p);
        }
        if let Some(k) = ir.params.top_k {
            body["top_k"] = json!(k);
        }
        if !ir.params.stop.is_empty() {
            body["stop_sequences"] = json!(ir.params.stop);
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            http::HeaderName::from_static("anthropic-version"),
            http::HeaderValue::from_static("2023-06-01"),
        );

        Ok((body, headers))
    }

    fn decode_response(&self, body: serde_json::Value) -> Result<IrResponse, tiygate_core::Error> {
        let response_id = body["id"].as_str().map(String::from);

        let mut content = Vec::new();

        if let Some(arr) = body["content"].as_array() {
            for block in arr {
                match block["type"].as_str() {
                    Some("text") => {
                        content.push(Content::Text {
                            text: block["text"].as_str().unwrap_or("").to_string(),
                        });
                    }
                    Some("thinking") => {
                        content.push(Content::Reasoning {
                            text: block["thinking"].as_str().unwrap_or("").to_string(),
                        });
                    }
                    Some("tool_use") => {
                        content.push(Content::ToolCall {
                            id: block["id"].as_str().unwrap_or("").to_string(),
                            name: block["name"].as_str().unwrap_or("").to_string(),
                            arguments: block["input"].clone(),
                        });
                    }
                    _ => {}
                }
            }
        }

        let finish_reason = body["stop_reason"].as_str().map(|s| match s {
            "end_turn" => FinishReason::Stop,
            "max_tokens" => FinishReason::Length,
            "tool_use" => FinishReason::ToolCalls,
            "content_filter" => FinishReason::ContentFilter,
            other => FinishReason::Other(other.to_string()),
        });

        let usage = body.get("usage").map(|u| Usage {
            prompt_tokens: u["input_tokens"].as_u64().unwrap_or(0),
            completion_tokens: u["output_tokens"].as_u64().unwrap_or(0),
            total_tokens: u["input_tokens"].as_u64().unwrap_or(0)
                + u["output_tokens"].as_u64().unwrap_or(0),
            cache_read_tokens: u["cache_read_input_tokens"].as_u64(),
            cache_write_tokens: u["cache_creation_input_tokens"].as_u64(),
            ..Default::default()
        });

        let stop_details = body["stop_reason"]
            .as_str()
            .map(|s| tiygate_core::ir::StopDetails {
                stop_reason: s.to_string(),
                stop_sequence: None,
            });

        Ok(IrResponse {
            content,
            usage,
            finish_reason,
            response_id,
            stop_details,
            extensions: Default::default(),
        })
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(MessagesStreamDecoder::new())
    }
}

// --- Stream Encoder (Anthropic SSE) ---

pub struct MessagesStreamEncoder {
    message_started: bool,
}

impl Default for MessagesStreamEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl MessagesStreamEncoder {
    pub fn new() -> Self {
        Self {
            message_started: false,
        }
    }
}

impl StreamEncoder for MessagesStreamEncoder {
    fn encode_part(&mut self, part: &StreamPart) -> Result<Vec<u8>, tiygate_core::Error> {
        let event = match part {
            StreamPart::ResponseStarted { id } => {
                let data = json!({
                    "type": "message_start",
                    "message": {"id": id, "type": "message", "role": "assistant", "model": "", "content": []},
                });
                self.message_started = true;
                format!(
                    "event: message_start\ndata: {}\n\n",
                    serde_json::to_string(&data).unwrap_or_default()
                )
            }
            StreamPart::TextDelta { text } => {
                let data = json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": text},
                });
                format!(
                    "event: content_block_delta\ndata: {}\n\n",
                    serde_json::to_string(&data).unwrap_or_default()
                )
            }
            StreamPart::ReasoningDelta { text } => {
                let data = json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": text},
                });
                format!(
                    "event: content_block_delta\ndata: {}\n\n",
                    serde_json::to_string(&data).unwrap_or_default()
                )
            }
            StreamPart::ToolCallDelta {
                id,
                name,
                arguments,
            } => {
                // For Anthropic, tool_use blocks are emitted with content_block_start then deltas
                if let Some(n) = name {
                    let data = json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": {"type": "tool_use", "id": id, "name": n, "input": json!({})},
                    });
                    format!(
                        "event: content_block_start\ndata: {}\n\n",
                        serde_json::to_string(&data).unwrap_or_default()
                    )
                } else {
                    let data = json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {"type": "input_json_delta", "partial_json": arguments},
                    });
                    format!(
                        "event: content_block_delta\ndata: {}\n\n",
                        serde_json::to_string(&data).unwrap_or_default()
                    )
                }
            }
            StreamPart::Usage { usage } => {
                let data = json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": null, "stop_sequence": null},
                    "usage": {"output_tokens": usage.completion_tokens},
                });
                format!(
                    "event: message_delta\ndata: {}\n\n",
                    serde_json::to_string(&data).unwrap_or_default()
                )
            }
            StreamPart::Finish { reason } => {
                let stop_reason = match reason {
                    FinishReason::Stop => "end_turn",
                    FinishReason::Length => "max_tokens",
                    FinishReason::ToolCalls => "tool_use",
                    _ => "end_turn",
                };
                let data = json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": stop_reason},
                    "usage": {"output_tokens": 0},
                });
                format!(
                    "event: message_delta\ndata: {}\n\n",
                    serde_json::to_string(&data).unwrap_or_default()
                )
            }
            StreamPart::ResponseCompleted { .. } => {
                let data = json!({"type": "message_stop"});
                format!(
                    "event: message_stop\ndata: {}\n\n",
                    serde_json::to_string(&data).unwrap_or_default()
                )
            }
            StreamPart::Error { message, code: _ } => {
                format!(
                    "event: error\ndata: {}\n\n",
                    json!({"type": "error", "error": {"type": "gateway_error", "message": message}})
                )
            }
        };

        Ok(event.into_bytes())
    }

    fn encode_error(&mut self, message: &str, _code: Option<&str>) -> Vec<u8> {
        format!(
            "event: error\ndata: {}\n\n",
            json!({"type": "error", "error": {"type": "gateway_error", "message": message}})
        )
        .into_bytes()
    }

    fn encode_done(&mut self) -> Vec<u8> {
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
            .to_string()
            .into_bytes()
    }
}

// --- Stream Decoder (explicit state machine) ---

pub struct MessagesStreamDecoder {
    response_id: Option<String>,
    current_block_type: Option<String>,
    tool_use_id: Option<String>,
    tool_use_name: Option<String>,
}

impl Default for MessagesStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl MessagesStreamDecoder {
    pub fn new() -> Self {
        Self {
            response_id: None,
            current_block_type: None,
            tool_use_id: None,
            tool_use_name: None,
        }
    }
}

impl StreamDecoder for MessagesStreamDecoder {
    fn feed(&mut self, line: &str) -> Result<Vec<StreamPart>, tiygate_core::Error> {
        let line = line.trim();
        if line.is_empty() || !line.starts_with("data: ") {
            return Ok(vec![]);
        }

        let data = line.strip_prefix("data: ").unwrap_or("");
        let event: Value = serde_json::from_str(data).map_err(|e| {
            tiygate_core::Error::Codec(format!("Failed to parse Anthropic SSE: {}", e))
        })?;

        let mut parts = Vec::new();

        match event["type"].as_str() {
            Some("message_start") => {
                if let Some(id) = event["message"]["id"].as_str() {
                    self.response_id = Some(id.to_string());
                    parts.push(StreamPart::ResponseStarted { id: id.to_string() });
                }
            }
            Some("content_block_start") => {
                let block = &event["content_block"];
                self.current_block_type = block["type"].as_str().map(String::from);
                match block["type"].as_str() {
                    Some("tool_use") => {
                        self.tool_use_id = block["id"].as_str().map(String::from);
                        self.tool_use_name = block["name"].as_str().map(String::from);
                        parts.push(StreamPart::ToolCallDelta {
                            id: self.tool_use_id.clone().unwrap_or_default(),
                            name: self.tool_use_name.clone(),
                            arguments: String::new(),
                        });
                    }
                    Some("text") => {
                        if let Some(text) = block["text"].as_str() {
                            parts.push(StreamPart::TextDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                    Some("thinking") => {
                        if let Some(thinking) = block["thinking"].as_str() {
                            parts.push(StreamPart::ReasoningDelta {
                                text: thinking.to_string(),
                            });
                        }
                    }
                    Some("image") => {
                        tracing::debug!("Anthropic image block in stream (not delta-ed)");
                    }
                    Some(other) => {
                        tracing::debug!(
                            "Unknown Anthropic content block type in stream: {}",
                            other
                        );
                    }
                    None => {
                        tracing::debug!("Anthropic content_block_start with no type field");
                    }
                }
            }
            Some("content_block_delta") => {
                let delta = &event["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        if let Some(text) = delta["text"].as_str() {
                            parts.push(StreamPart::TextDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(thinking) = delta["thinking"].as_str() {
                            parts.push(StreamPart::ReasoningDelta {
                                text: thinking.to_string(),
                            });
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(json) = delta["partial_json"].as_str() {
                            parts.push(StreamPart::ToolCallDelta {
                                id: self.tool_use_id.clone().unwrap_or_default(),
                                name: self.tool_use_name.clone(),
                                arguments: json.to_string(),
                            });
                        }
                    }
                    Some(other) => {
                        tracing::debug!("Unknown Anthropic content_block_delta type: {}", other);
                    }
                    None => {
                        tracing::debug!("Anthropic content_block_delta with no type field");
                    }
                }
            }
            Some("content_block_stop") => {
                self.current_block_type = None;
            }
            Some("message_delta") => {
                if let Some(usage) = event["usage"].as_object() {
                    parts.push(StreamPart::Usage {
                        usage: Usage {
                            completion_tokens: usage
                                .get("output_tokens")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0),
                            ..Default::default()
                        },
                    });
                }
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    let fr = match reason {
                        "end_turn" => FinishReason::Stop,
                        "max_tokens" => FinishReason::Length,
                        "tool_use" => FinishReason::ToolCalls,
                        other => FinishReason::Other(other.to_string()),
                    };
                    parts.push(StreamPart::Finish { reason: fr });
                }
            }
            Some("message_stop") => {
                parts.push(StreamPart::ResponseCompleted {
                    id: self.response_id.clone().unwrap_or_default(),
                    status: "completed".to_string(),
                    usage: None,
                });
            }
            Some("error") => {
                parts.push(StreamPart::Error {
                    message: event["error"]["message"]
                        .as_str()
                        .unwrap_or("Unknown error")
                        .to_string(),
                    code: event["error"]["type"].as_str().map(String::from),
                });
            }
            Some(other) => {
                parts.push(StreamPart::Error {
                    message: format!("Unknown Anthropic SSE event type: {}", other),
                    code: Some("unknown_event".to_string()),
                });
            }
            None => {
                parts.push(StreamPart::Error {
                    message: "Anthropic SSE event with no type field".to_string(),
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

inventory::submit! {
    tiygate_core::CodecRegistration {
        make: || Box::new(MessagesCodec::new()),


}
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make_basic_request() -> Value {
        json!({
                    "model": "claude-sonnet-4-20250514",
                    "max_tokens": 100,
                    "messages": [
                        {"role": "user", "content": "Hello"

        }
                    ],
                    "stream": false
                })
    }

    fn make_tool_request() -> Value {
        json!({
                    "model": "claude-sonnet-4-20250514",
                    "max_tokens": 200,
                    "messages": [
                        {"role": "user", "content": "What is the weather?"

        }
                    ],
                    "tools": [{
                        "name": "get_weather",
                        "description": "Get weather",
                        "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}

        }
                    }],
                    "stream": false
                })
    }

    fn make_raw_envelope() -> RawEnvelope {
        RawEnvelope {
            method: "POST".to_string(),
            path: "/v1/messages".to_string(),
            headers: std::collections::HashMap::new(),
            body: None,
            truncated: false,
            original_body_size: 0,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_decode_basic_request() {
        let codec = MessagesCodec::new();
        let env = make_raw_envelope();
        let ir = codec.decode_request(make_basic_request(), &env).unwrap();
        assert_eq!(ir.model, "claude-sonnet-4-20250514");
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.params.max_tokens, Some(100));
    }

    #[test]
    fn test_decode_tool_request() {
        let codec = MessagesCodec::new();
        let env = make_raw_envelope();
        let ir = codec.decode_request(make_tool_request(), &env).unwrap();
        assert_eq!(ir.tools.len(), 1);
        assert_eq!(ir.tools[0].name, "get_weather");
    }

    #[test]
    fn test_decode_request_roundtrip() {
        let codec = MessagesCodec::new();
        let env = make_raw_envelope();
        let original = make_basic_request();
        let ir = codec.decode_request(original.clone(), &env).unwrap();
        let (re_encoded, _headers) = codec.encode_request(&ir).unwrap();
        let ir2 = codec.decode_request(re_encoded, &env).unwrap();
        assert_eq!(ir.model, ir2.model);
        assert_eq!(ir.messages.len(), ir2.messages.len());
    }

    #[test]
    fn test_snapshot_decode_request() {
        let codec = MessagesCodec::new();
        let env = make_raw_envelope();
        let ir = codec.decode_request(make_basic_request(), &env).unwrap();
        insta::assert_debug_snapshot!(ir);
    }

    #[test]
    fn test_encode_response_non_streaming() {
        let codec = MessagesCodec::new();
        let ir = IrResponse {
            content: vec![Content::Text {
                text: "Hello!".to_string(),
            }],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..Default::default()
            }),
            finish_reason: Some(FinishReason::Stop),
            response_id: Some("msg_1".to_string()),
            stop_details: Some(tiygate_core::ir::StopDetails {
                stop_reason: "end_turn".to_string(),
                stop_sequence: None,
            }),
            extensions: Default::default(),
        };

        let encoded = codec.encode_response(&ir).unwrap();
        let body = encoded.as_object().unwrap();
        assert_eq!(body["id"], "msg_1");
        assert_eq!(body["type"], "message");
        assert!(body["content"].is_array());
        assert_eq!(body["usage"]["input_tokens"], 10);
        assert_eq!(body["usage"]["output_tokens"], 5);
    }

    #[test]
    fn test_encode_response_with_tool_use() {
        let codec = MessagesCodec::new();
        let ir = IrResponse {
            content: vec![Content::ToolCall {
                id: "toolu_1".to_string(),
                name: "get_weather".to_string(),
                arguments: json!({"city": "London"}),
            }],
            usage: None,
            finish_reason: Some(FinishReason::ToolCalls),
            response_id: Some("msg_2".to_string()),
            stop_details: None,
            extensions: Default::default(),
        };

        let encoded = codec.encode_response(&ir).unwrap();
        let content = &encoded["content"];
        assert_eq!(content[0]["type"], "tool_use");
        assert_eq!(content[0]["name"], "get_weather");
        assert_eq!(content[0]["id"], "toolu_1");
    }

    #[test]
    fn test_stream_encoder_error_frame() {
        let mut encoder = MessagesStreamEncoder::new();
        let err_bytes = encoder.encode_error("overloaded", Some("529"));
        let err_str = String::from_utf8_lossy(&err_bytes);
        // Must contain "error" — protocol-native error frame
        assert!(err_str.contains("error"));
        assert!(err_str.contains("overloaded"));
    }

    #[test]
    fn test_stream_encoder_all_variants() {
        let mut encoder = MessagesStreamEncoder::new();
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
                id: "tc1".to_string(),
                name: Some("fn".to_string()),
                arguments: "{}".to_string(),
            },
            StreamPart::Usage {
                usage: Usage::default(),
            },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
            StreamPart::Error {
                message: "err".to_string(),
                code: Some("500".to_string()),
            },
            StreamPart::ResponseCompleted {
                id: "r1".to_string(),
                status: "completed".to_string(),
                usage: None,
            },
        ];
        for variant in variants {
            assert!(encoder.encode_part(variant).is_ok());
        }
    }

    #[test]
    fn test_stream_decoder_message_start() {
        let mut decoder = MessagesStreamDecoder::new();
        let line = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-sonnet-4-20250514\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":10}}}\n";
        let parts = decoder.feed(line).unwrap();
        assert!(!parts.is_empty());
        assert!(matches!(parts[0], StreamPart::ResponseStarted { .. }));
    }

    #[test]
    fn test_stream_decoder_text_delta() {
        let mut decoder = MessagesStreamDecoder::new();
        let line = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n";
        let parts = decoder.feed(line).unwrap();
        assert!(parts
            .iter()
            .any(|p| matches!(p, StreamPart::TextDelta { .. })));
    }

    #[test]
    fn test_stream_decoder_error_frame() {
        let mut decoder = MessagesStreamDecoder::new();
        let line = "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n";
        let parts = decoder.feed(line).unwrap();
        assert!(parts.iter().any(|p| matches!(p, StreamPart::Error { .. })));
    }

    #[test]
    fn test_codec_capabilities() {
        let codec = MessagesCodec::new();
        let caps = codec.capabilities();
        assert!(caps.streaming);
        assert!(caps.tools);
        assert!(caps.reasoning);
        assert!(caps.extended_reasoning);
        assert!(caps.lossy_default_reject);
    }

    #[test]
    fn test_codec_id_matches() {
        let codec = MessagesCodec::new();
        assert_eq!(codec.id().suite, ProtocolSuite::AnthropicMessages);
        assert!(codec.id().full_id().contains("messages"));
    }
}
