//! OpenAI Chat Completions protocol codec.
//!
//! Implements bidirectional conversion between OpenAI's Chat Completions API
//! and the canonical IR. Supports both streaming (SSE) and non-streaming modes.

use http::HeaderMap;
use serde_json::{json, Value};

use tiygate_core::{
    Content, EndpointCapabilities, EndpointCodec, FinishReason, IrRequest, IrResponse, Message,
    ProtocolEndpoint, ProtocolSuite, RawEnvelope, Role, StreamDecoder, StreamEncoder, StreamPart,
    Tool, Usage,
};

/// Chat Completions protocol identity.
pub const CHAT_COMPLETIONS_ID: ProtocolEndpoint = ProtocolEndpoint {
    suite: ProtocolSuite::OpenAiCompatible,
    name: String::new(), // Set at construction
    version: String::new(),
};

/// The Chat Completions codec.
pub struct ChatCompletionsCodec {
    id: ProtocolEndpoint,
    capabilities: EndpointCapabilities,
}

impl Default for ChatCompletionsCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatCompletionsCodec {
    pub fn new() -> Self {
        Self {
            id: ProtocolEndpoint::new(ProtocolSuite::OpenAiCompatible, "chat-completions", "v1"),
            capabilities: EndpointCapabilities {
                streaming: true,
                tools: true,
                reasoning: true,
                embeddings: false,
                force_upstream_stream: false,
                override_model_in_body: false,
                ingress_routes: &[("POST", "/v1/chat/completions")],
                multimodal: true,
                structured_output: true,
                function_calling: true,
                parallel_tool_calls: true,
                extended_reasoning: false,
                deterministic_seed: true,
                tool_choice_required: true,
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

impl EndpointCodec for ChatCompletionsCodec {
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
        if let Some(arr) = body["messages"].as_array() {
            for msg in arr {
                let role: Role = match msg["role"].as_str().unwrap_or("user") {
                    "system" | "developer" => Role::System,
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    "tool" => Role::Tool,
                    _ => Role::User,
                };

                let content = if let Some(text) = msg["content"].as_str() {
                    vec![Content::Text {
                        text: text.to_string(),
                    }]
                } else if let Some(arr) = msg["content"].as_array() {
                    parse_content_array(arr, &role)
                } else if msg["content"].is_null() && msg["tool_calls"].is_array() {
                    // Tool call response from assistant
                    let mut parts = Vec::new();
                    for tc in msg["tool_calls"].as_array().unwrap_or(&vec![]) {
                        parts.push(Content::ToolCall {
                            id: tc["id"].as_str().unwrap_or("").to_string(),
                            name: tc["function"]["name"].as_str().unwrap_or("").to_string(),
                            arguments: tc["function"]["arguments"].clone(),
                        });
                    }
                    parts
                } else {
                    vec![Content::Text {
                        text: String::new(),
                    }]
                };

                messages.push(Message { role, content });
            }
        }

        // Extract system message if present
        let system = messages
            .iter()
            .find(|m| m.role == Role::System)
            .and_then(|m| match &m.content.first() {
                Some(Content::Text { text }) => Some(text.clone()),
                _ => None,
            });

        // Filter out system messages from the list
        let messages: Vec<Message> = messages
            .into_iter()
            .filter(|m| m.role != Role::System)
            .collect();

        // Parse tools
        let tools: Vec<Tool> = body["tools"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|t| Tool {
                        name: t["function"]["name"].as_str().unwrap_or("").to_string(),
                        description: t["function"]["description"].as_str().map(|s| s.to_string()),
                        parameters: Some(t["function"]["parameters"].clone()),
                        required: false,
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Parse tool_choice — store in extensions so lossy checks can inspect it.
        // https://developers.openai.com/api/docs/guides/function-calling#tool-choice
        // Allowed forms: "none", "auto", "required", {"type":"function","function":{"name":"x"}}
        let mut extensions = std::collections::HashMap::new();
        if let Some(tc) = body.get("tool_choice") {
            if let Some(s) = tc.as_str() {
                extensions.insert("tool_choice".to_string(), json!(s));
            } else if tc.is_object() {
                extensions.insert("tool_choice".to_string(), tc.clone());
            }
        }

        let params = tiygate_core::GenerationParams {
            max_tokens: body["max_tokens"].as_u64().map(|v| v as u32),
            temperature: body["temperature"].as_f64().map(|v| v as f32),
            top_p: body["top_p"].as_f64().map(|v| v as f32),
            frequency_penalty: body["frequency_penalty"].as_f64().map(|v| v as f32),
            presence_penalty: body["presence_penalty"].as_f64().map(|v| v as f32),
            seed: body["seed"].as_i64(),
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

        let response_format = if body.get("response_format").is_some() {
            let rf = &body["response_format"];
            match rf["type"].as_str() {
                Some("json_schema") => Some(tiygate_core::ResponseFormat::JsonSchema {
                    name: rf["json_schema"]["name"]
                        .as_str()
                        .unwrap_or("response")
                        .to_string(),
                    schema: rf["json_schema"]["schema"].clone(),
                    strict: rf["json_schema"]["strict"].as_bool(),
                }),
                Some("json_object") => Some(tiygate_core::ResponseFormat::JsonObject),
                _ => None,
            }
        } else {
            None
        };

        Ok(IrRequest {
            model,
            system,
            messages,
            tools,
            params,
            response_format,
            stream,
            ingress_protocol: self.id.clone(),
            extensions,
        })
    }

    fn encode_response(&self, ir: &IrResponse) -> Result<serde_json::Value, tiygate_core::Error> {
        let mut response = json!({
            "object": "chat.completion",
            "model": "",
        });

        if let Some(id) = &ir.response_id {
            response["id"] = json!(id);
        }

        let mut choices = Vec::new();
        let mut message_content = String::new();
        let mut tool_calls_json = Vec::new();

        for content in &ir.content {
            match content {
                Content::Text { text } => {
                    message_content.push_str(text);
                }
                Content::Reasoning { text: _ } => {
                    // OpenAI doesn't natively expose reasoning text in the content field
                    // (it goes into a separate reasoning_tokens field)
                }
                Content::ToolCall {
                    id,
                    name,
                    arguments,
                } => {
                    tool_calls_json.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(arguments).unwrap_or_default(),
                        }
                    }));
                }
                _ => {}
            }
        }

        let mut message = json!({
            "role": "assistant",
            "content": message_content,
        });

        if !tool_calls_json.is_empty() {
            message["tool_calls"] = json!(tool_calls_json);
        }

        choices.push(json!({
            "index": 0,
            "message": message,
            "finish_reason": ir.finish_reason.as_ref().map(|r| match r {
                FinishReason::Stop => "stop",
                FinishReason::Length => "length",
                FinishReason::ContentFilter => "content_filter",
                FinishReason::ToolCalls => "tool_calls",
                FinishReason::Other(_) => "stop",
            }),
        }));

        response["choices"] = json!(choices);

        if let Some(usage) = &ir.usage {
            // OpenAI 规范的 prompt_tokens 必须包含 cache 命中部分（即使 IR.prompt_tokens 不含）
            let cache_read = usage.cache_read_tokens.unwrap_or(0);
            let cache_write = usage.cache_write_tokens.unwrap_or(0);
            let prompt_for_openai = usage.prompt_tokens + cache_read + cache_write;
            let total_for_openai = prompt_for_openai + usage.completion_tokens;
            response["usage"] = json!({
                "prompt_tokens": prompt_for_openai,
                "completion_tokens": usage.completion_tokens,
                "total_tokens": total_for_openai,
            });
            if cache_read > 0 {
                let mut details = serde_json::Map::new();
                details.insert("cached_tokens".to_string(), json!(cache_read));
                response["usage"]["prompt_tokens_details"] = json!(details);
            }
            if let Some(rt) = usage.reasoning_tokens {
                response["usage"]["completion_tokens_details"] = json!({
                    "reasoning_tokens": rt,
                });
            }
        }

        Ok(response)
    }

    fn stream_encoder(&self) -> Box<dyn StreamEncoder> {
        Box::new(ChatCompletionsStreamEncoder::new())
    }

    fn pass_through_policy(
        &self,
        ingress: &tiygate_core::ProtocolEndpoint,
        egress: &tiygate_core::ProtocolEndpoint,
    ) -> tiygate_core::PassThroughPolicy {
        // Same protocol suite (e.g., OpenAI chat-completions in/out) →
        // forward raw bytes; no IR conversion needed.
        if ingress.suite == egress.suite {
            tiygate_core::PassThroughPolicy::Passthrough
        } else {
            tiygate_core::PassThroughPolicy::Convert
        }
    }

    fn encode_request(
        &self,
        ir: &IrRequest,
    ) -> Result<(serde_json::Value, HeaderMap), tiygate_core::Error> {
        let mut body = json!({
            "model": ir.model,
            "stream": ir.stream,
        });

        // Build messages array
        let mut messages = Vec::new();

        if let Some(system) = &ir.system {
            messages.push(json!({
                "role": "system",
                "content": system,
            }));
        }

        for msg in &ir.messages {
            let mut msg_json = json!({
                "role": match msg.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                    Role::System => "system",
                },
            });

            // Aggregate text-bearing parts for the `content` field. Reasoning
            // and tool-related parts are handled separately.
            let mut text_parts: Vec<Value> = Vec::new();
            let mut reasoning_text = String::new();
            let mut tool_calls_json: Vec<Value> = Vec::new();

            for content in &msg.content {
                match content {
                    Content::Text { text } => {
                        text_parts.push(json!({"type": "text", "text": text}));
                    }
                    Content::Reasoning { text } => {
                        // Per Deepseek thinking-mode spec, the assistant message
                        // carries `reasoning_content` as a sibling of `content`.
                        // When the same turn issues tool_calls, this MUST be
                        // echoed back to the API in every subsequent request
                        // (otherwise the API returns 400).
                        if !reasoning_text.is_empty() {
                            reasoning_text.push('\n');
                        }
                        reasoning_text.push_str(text);
                    }
                    Content::ToolCall {
                        id,
                        name,
                        arguments,
                    } => {
                        // Re-emit the tool call on the assistant message so the
                        // downstream API sees a self-consistent turn.
                        let args_str = match arguments {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        tool_calls_json.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": args_str,
                            }
                        }));
                    }
                    Content::Media {
                        source, mime_type, ..
                    } => match source {
                        tiygate_core::ir::MediaSource::Url { url } => text_parts.push(json!({
                            "type": "image_url",
                            "image_url": {"url": url}
                        })),
                        tiygate_core::ir::MediaSource::Inline { data } => text_parts.push(json!({
                            "type": "image_url",
                            "image_url": {"url": format!("data:{};base64,{}", mime_type, data)}
                        })),
                        _ => {}
                    },
                    Content::ToolResult {
                        tool_call_id,
                        name: _,
                        content,
                    } => {
                        // Tool results are a separate message in OpenAI format;
                        // emit them as their own {role:"tool", tool_call_id, content}
                        // object. This branch intentionally produces a full
                        // message, not a content part.
                        messages.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_call_id,
                            "content": content,
                        }));
                    }
                }
            }

            if text_parts.len() == 1 && text_parts[0].get("text").is_some() {
                msg_json["content"] = text_parts[0]["text"].clone();
            } else if !text_parts.is_empty() {
                msg_json["content"] = json!(text_parts);
            } else if !tool_calls_json.is_empty() || !reasoning_text.is_empty() {
                // Allow null/empty content when the turn is purely reasoning
                // + tool calls (Deepseek emits this exact shape).
                msg_json["content"] = Value::Null;
            } else {
                msg_json["content"] = json!("");
            }

            if !reasoning_text.is_empty() {
                msg_json["reasoning_content"] = json!(reasoning_text);
            }

            if !tool_calls_json.is_empty() {
                msg_json["tool_calls"] = json!(tool_calls_json);
            }

            // Avoid emitting a duplicate empty assistant message when the only
            // contribution was a ToolResult (already pushed as its own message).
            let has_real_content =
                !text_parts.is_empty() || !reasoning_text.is_empty() || !tool_calls_json.is_empty();
            if has_real_content || msg.content.is_empty() {
                messages.push(msg_json);
            }
        }

        body["messages"] = json!(messages);

        // Tools
        if !ir.tools.is_empty() {
            let tools: Vec<Value> = ir
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }

        // Generation params
        if let Some(mt) = ir.params.max_tokens {
            body["max_tokens"] = json!(mt);
        }
        if let Some(t) = ir.params.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(p) = ir.params.top_p {
            body["top_p"] = json!(p);
        }
        if let Some(f) = ir.params.frequency_penalty {
            body["frequency_penalty"] = json!(f);
        }
        if let Some(p) = ir.params.presence_penalty {
            body["presence_penalty"] = json!(p);
        }
        if let Some(s) = ir.params.seed {
            body["seed"] = json!(s);
        }
        if !ir.params.stop.is_empty() {
            body["stop"] = json!(ir.params.stop);
        }

        // Response format
        match &ir.response_format {
            Some(tiygate_core::ResponseFormat::JsonSchema {
                name,
                schema,
                strict,
            }) => {
                body["response_format"] = json!({
                    "type": "json_schema",
                    "json_schema": {
                        "name": name,
                        "schema": schema,
                        "strict": strict.unwrap_or(false),
                    }
                });
            }
            Some(tiygate_core::ResponseFormat::JsonObject) => {
                body["response_format"] = json!({"type": "json_object"});
            }
            _ => {}
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );

        Ok((body, headers))
    }

    fn decode_response(&self, body: serde_json::Value) -> Result<IrResponse, tiygate_core::Error> {
        let response_id = body["id"].as_str().map(String::from);

        let mut content = Vec::new();

        if let Some(choices) = body["choices"].as_array() {
            if let Some(choice) = choices.first() {
                let msg = &choice["message"];

                // Text content
                if let Some(text) = msg["content"].as_str() {
                    if !text.is_empty() {
                        content.push(Content::Text {
                            text: text.to_string(),
                        });
                    }
                }

                // Reasoning content (Deepseek thinking mode + OpenAI-compatible
                // models that expose CoT as a sibling of `content`).
                // Per Deepseek spec, when a turn contains tool_calls this MUST be
                // echoed back in subsequent requests, or the API returns 400.
                if let Some(reasoning_text) = msg["reasoning_content"].as_str() {
                    if !reasoning_text.is_empty() {
                        content.push(Content::Reasoning {
                            text: reasoning_text.to_string(),
                        });
                    }
                }

                // Tool calls
                if let Some(tc_arr) = msg["tool_calls"].as_array() {
                    for tc in tc_arr {
                        let args: serde_json::Value = serde_json::from_str(
                            tc["function"]["arguments"].as_str().unwrap_or("{}"),
                        )
                        .unwrap_or(json!({}));
                        content.push(Content::ToolCall {
                            id: tc["id"].as_str().unwrap_or("").to_string(),
                            name: tc["function"]["name"].as_str().unwrap_or("").to_string(),
                            arguments: args,
                        });
                    }
                }

                // Reasoning (from completion_tokens_details)
                if let Some(details) = body["usage"]["completion_tokens_details"].as_object() {
                    if let Some(rt) = details.get("reasoning_tokens") {
                        if rt.as_u64().unwrap_or(0) > 0 {
                            content.push(Content::Reasoning {
                                text: format!(
                                    "[{} reasoning tokens used]",
                                    rt.as_u64().unwrap_or(0)
                                ),
                            });
                        }
                    }
                }
            }
        }

        let finish_reason = body["choices"][0]["finish_reason"]
            .as_str()
            .map(|s| match s {
                "stop" => FinishReason::Stop,
                "length" => FinishReason::Length,
                "content_filter" => FinishReason::ContentFilter,
                "tool_calls" => FinishReason::ToolCalls,
                _ => FinishReason::Other(s.to_string()),
            });

        let usage = body.get("usage").map(|u| Usage {
            prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
            completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
            total_tokens: u["total_tokens"].as_u64().unwrap_or(0),
            reasoning_tokens: u["completion_tokens_details"]["reasoning_tokens"].as_u64(),
            cache_read_tokens: u["prompt_tokens_details"]["cached_tokens"].as_u64(),
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

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(ChatCompletionsStreamDecoder::new())
    }
}

// --- Stream Encoder ---

pub struct ChatCompletionsStreamEncoder {
    response_id: Option<String>,
}

impl Default for ChatCompletionsStreamEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatCompletionsStreamEncoder {
    pub fn new() -> Self {
        Self { response_id: None }
    }
}

impl StreamEncoder for ChatCompletionsStreamEncoder {
    fn encode_part(&mut self, part: &StreamPart) -> Result<Vec<u8>, tiygate_core::Error> {
        let chunk = match part {
            StreamPart::ResponseStarted { id } => {
                self.response_id = Some(id.clone());
                String::new() // OpenAI SSE doesn't need a start event
            }
            StreamPart::TextDelta { text } => {
                let id = self.response_id.as_deref().unwrap_or("");
                format!(
                    "data: {}\n\n",
                    json!({
                        "id": id,
                        "object": "chat.completion.chunk",
                        "choices": [{
                            "index": 0,
                            "delta": {"content": text},
                            "finish_reason": null,
                        }]
                    })
                )
            }
            StreamPart::ReasoningDelta { text } => {
                // Deepseek thinking mode streams the CoT as `reasoning_content`
                // in the SSE delta. Other OpenAI-compatible providers may use
                // a different field; we always emit `reasoning_content` so
                // downstream clients (and our own decoder) stay symmetric.
                let id = self.response_id.as_deref().unwrap_or("");
                format!(
                    "data: {}\n\n",
                    json!({
                        "id": id,
                        "object": "chat.completion.chunk",
                        "choices": [{
                            "index": 0,
                            "delta": {"reasoning_content": text},
                            "finish_reason": null,
                        }]
                    })
                )
            }
            StreamPart::ToolCallDelta {
                id,
                name,
                arguments,
            } => {
                let resp_id = self.response_id.as_deref().unwrap_or("");
                let mut delta = json!({
                    "tool_calls": [{
                        "index": 0,
                        "id": id,
                        "type": "function",
                        "function": {
                            "arguments": arguments,
                        }
                    }]
                });
                if let Some(n) = name {
                    delta["tool_calls"][0]["function"]["name"] = json!(n);
                }
                format!(
                    "data: {}\n\n",
                    json!({
                        "id": resp_id,
                        "object": "chat.completion.chunk",
                        "choices": [{
                            "index": 0,
                            "delta": delta,
                            "finish_reason": null,
                        }]
                    })
                )
            }
            StreamPart::Usage { usage } => {
                let id = self.response_id.as_deref().unwrap_or("");
                let mut usage_obj = json!({
                    "prompt_tokens": usage.prompt_tokens,
                    "completion_tokens": usage.completion_tokens,
                    "total_tokens": usage.total_tokens,
                });
                if let Some(cr) = usage.cache_read_tokens {
                    if cr > 0 {
                        usage_obj["prompt_tokens_details"] = json!({"cached_tokens": cr});
                    }
                }
                if let Some(rt) = usage.reasoning_tokens {
                    if rt > 0 {
                        usage_obj["completion_tokens_details"] = json!({"reasoning_tokens": rt});
                    }
                }
                format!(
                    "data: {}\n\n",
                    json!({
                        "id": id,
                        "object": "chat.completion.chunk",
                        "choices": [{
                            "index": 0,
                            "delta": {},
                            "finish_reason": null,
                        }],
                        "usage": usage_obj,
                    })
                )
            }
            StreamPart::Finish { reason } => {
                let id = self.response_id.as_deref().unwrap_or("");
                let reason_str = match reason {
                    FinishReason::Stop => "stop",
                    FinishReason::Length => "length",
                    FinishReason::ContentFilter => "content_filter",
                    FinishReason::ToolCalls => "tool_calls",
                    FinishReason::Other(_) => "stop",
                };
                format!(
                    "data: {}\n\n",
                    json!({
                        "id": id,
                        "object": "chat.completion.chunk",
                        "choices": [{
                            "index": 0,
                            "delta": {},
                            "finish_reason": reason_str,
                        }]
                    })
                )
            }
            StreamPart::ResponseCompleted { .. } => "data: [DONE]\n\n".to_string(),
            StreamPart::Error { message, code: _ } => {
                // Protocol-native error frame
                format!(
                    "data: {}\n\n",
                    json!({
                        "error": {
                            "message": message,
                            "type": "gateway_error",
                        }
                    })
                )
            }
        };

        Ok(chunk.into_bytes())
    }

    fn encode_error(&mut self, message: &str, _code: Option<&str>) -> Vec<u8> {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "error": {
                    "message": message,
                    "type": "gateway_error",
                }
            })
        )
        .into_bytes()
    }

    fn encode_done(&mut self) -> Vec<u8> {
        "data: [DONE]\n\n".to_string().into_bytes()
    }
}

// --- Stream Decoder (structure-dispatched via `object` field) ---

pub struct ChatCompletionsStreamDecoder {
    response_id: Option<String>,
    tool_call_id: Option<String>,
    tool_call_name: Option<String>,
}

impl Default for ChatCompletionsStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatCompletionsStreamDecoder {
    pub fn new() -> Self {
        Self {
            response_id: None,
            tool_call_id: None,
            tool_call_name: None,
        }
    }
}

impl StreamDecoder for ChatCompletionsStreamDecoder {
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

        let data = if let Some(stripped) = line.strip_prefix("data: ") {
            stripped
        } else {
            return Ok(vec![]);
        };

        let chunk: Value = serde_json::from_str(data)
            .map_err(|e| tiygate_core::Error::Codec(format!("Failed to parse SSE data: {}", e)))?;

        let mut parts = Vec::new();

        // Dispatch by object field (protocol-native type discriminator)
        match chunk["object"].as_str() {
            Some("chat.completion.chunk") | None => {
                // standard chunk or missing object field (backwards compat)

                // Extract response id
                if let Some(id) = chunk["id"].as_str() {
                    if self.response_id.is_none() {
                        self.response_id = Some(id.to_string());
                        parts.push(StreamPart::ResponseStarted { id: id.to_string() });
                    }
                }

                // Handle error (can appear in any chunk)
                if let Some(error) = chunk.get("error") {
                    parts.push(StreamPart::Error {
                        message: error["message"]
                            .as_str()
                            .unwrap_or("Unknown error")
                            .to_string(),
                        code: error["code"].as_str().map(String::from),
                    });
                    return Ok(parts);
                }

                // Handle choices
                if let Some(choices) = chunk["choices"].as_array() {
                    for choice in choices {
                        let delta = &choice["delta"];

                        if let Some(text) = delta["content"].as_str() {
                            if !text.is_empty() {
                                parts.push(StreamPart::TextDelta {
                                    text: text.to_string(),
                                });
                            }
                        }

                        // Deepseek thinking mode streams `reasoning_content` as
                        // a sibling of `content` inside `delta`. OpenAI's
                        // Responses API uses `reasoning_details`; we keep that
                        // fallback for non-Deepseek providers that emit it.
                        if let Some(text) = delta["reasoning_content"].as_str() {
                            if !text.is_empty() {
                                parts.push(StreamPart::ReasoningDelta {
                                    text: text.to_string(),
                                });
                            }
                        } else if let Some(reasoning) = delta.get("reasoning_details") {
                            if let Some(text) = reasoning["text"].as_str() {
                                parts.push(StreamPart::ReasoningDelta {
                                    text: text.to_string(),
                                });
                            }
                        }

                        if let Some(tool_calls) = delta["tool_calls"].as_array() {
                            for tc in tool_calls {
                                if let Some(tc_id) = tc["id"].as_str() {
                                    self.tool_call_id = Some(tc_id.to_string());
                                }
                                if let Some(tc_name) = tc["function"]["name"].as_str() {
                                    self.tool_call_name = Some(tc_name.to_string());
                                }
                                if let Some(args) = tc["function"]["arguments"].as_str() {
                                    parts.push(StreamPart::ToolCallDelta {
                                        id: self.tool_call_id.clone().unwrap_or_default(),
                                        name: self.tool_call_name.clone(),
                                        arguments: args.to_string(),
                                    });
                                }
                            }
                        }

                        if let Some(fr) = choice["finish_reason"].as_str() {
                            if fr != "null" && !fr.is_empty() {
                                let reason = match fr {
                                    "stop" => FinishReason::Stop,
                                    "length" => FinishReason::Length,
                                    "content_filter" => FinishReason::ContentFilter,
                                    "tool_calls" => FinishReason::ToolCalls,
                                    other => FinishReason::Other(other.to_string()),
                                };
                                parts.push(StreamPart::Finish { reason });
                            }
                        }
                    }
                }

                // Usage
                if let Some(usage) = chunk.get("usage") {
                    let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
                    let completion = usage["completion_tokens"].as_u64().unwrap_or(0);
                    let total = usage["total_tokens"]
                        .as_u64()
                        .unwrap_or(prompt + completion);
                    let cache_read = usage["prompt_tokens_details"]["cached_tokens"].as_u64();
                    let reasoning = usage["completion_tokens_details"]["reasoning_tokens"].as_u64();
                    parts.push(StreamPart::Usage {
                        usage: Usage {
                            prompt_tokens: prompt,
                            completion_tokens: completion,
                            total_tokens: total,
                            reasoning_tokens: reasoning,
                            cache_read_tokens: cache_read,
                            ..Default::default()
                        },
                    });
                }
            }
            Some("error") => {
                let error = &chunk["error"];
                parts.push(StreamPart::Error {
                    message: error["message"]
                        .as_str()
                        .unwrap_or("Unknown error")
                        .to_string(),
                    code: error["code"].as_str().map(String::from),
                });
            }
            Some(other) => {
                parts.push(StreamPart::Error {
                    message: format!("Unknown SSE object type: {}", other),
                    code: Some("unknown_object".to_string()),
                });
            }
        }

        Ok(parts)
    }

    fn finish(&mut self) -> Result<Vec<StreamPart>, tiygate_core::Error> {
        Ok(vec![])
    }
}

fn parse_content_array(arr: &[Value], role: &Role) -> Vec<Content> {
    let mut parts = Vec::new();
    for item in arr {
        parts.push(match item["type"].as_str() {
            Some("text") => Content::Text {
                text: item["text"].as_str().unwrap_or("").to_string(),
            },
            Some("image_url") => Content::Media {
                source: tiygate_core::ir::MediaSource::Url {
                    url: item["image_url"]["url"].as_str().unwrap_or("").to_string(),
                },
                mime_type: "image/*".to_string(),
                metadata: Default::default(),
            },
            Some("tool_use") | Some("tool_result") => {
                if *role == Role::Tool {
                    Content::ToolResult {
                        tool_call_id: item["tool_call_id"].as_str().unwrap_or("").to_string(),
                        name: String::new(),
                        content: item["content"].as_str().unwrap_or("").to_string(),
                    }
                } else {
                    Content::Text {
                        text: item["content"].as_str().unwrap_or("").to_string(),
                    }
                }
            }
            _ => Content::Text {
                text: item["text"].as_str().unwrap_or("").to_string(),
            },
        });
    }
    parts
}

inventory::submit! {
    tiygate_core::CodecRegistration {
        make: || Box::new(ChatCompletionsCodec::new()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make_basic_request() -> Value {
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hello"}
            ],
            "temperature": 0.7,
            "max_tokens": 100,
            "stream": false
        })
    }

    fn make_tool_request() -> Value {
        json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "What is the weather?"}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
                }
            }],
            "tool_choice": "auto",
            "stream": false
        })
    }

    fn make_raw_envelope() -> RawEnvelope {
        RawEnvelope {
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            headers: std::collections::HashMap::new(),
            body: None,
            truncated: false,
            original_body_size: 0,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_decode_basic_request() {
        let codec = ChatCompletionsCodec::new();
        let env = make_raw_envelope();
        let ir = codec.decode_request(make_basic_request(), &env).unwrap();
        assert_eq!(ir.model, "gpt-4o");
        // System message extracted into ir.system, only user message in ir.messages
        assert_eq!(ir.messages.len(), 1);
        assert!(ir.system.is_some());
        assert_eq!(ir.params.max_tokens, Some(100));
        assert_eq!(ir.params.temperature, Some(0.7));
    }

    #[test]
    fn test_decode_tool_request() {
        let codec = ChatCompletionsCodec::new();
        let env = make_raw_envelope();
        let ir = codec.decode_request(make_tool_request(), &env).unwrap();
        assert_eq!(ir.tools.len(), 1);
        assert_eq!(ir.tools[0].name, "get_weather");
    }

    #[test]
    fn test_decode_request_roundtrip() {
        let codec = ChatCompletionsCodec::new();
        let env = make_raw_envelope();
        let original = make_basic_request();

        // Decode to IR
        let ir = codec.decode_request(original.clone(), &env).unwrap();

        // Encode back to provider format
        let (re_encoded, _headers) = codec.encode_request(&ir).unwrap();

        // Decode again and compare semantic fields
        let ir2 = codec.decode_request(re_encoded, &env).unwrap();
        assert_eq!(ir.model, ir2.model);
        assert_eq!(ir.messages.len(), ir2.messages.len());
        assert_eq!(ir.params.max_tokens, ir2.params.max_tokens);
    }

    #[test]
    fn test_encode_response_non_streaming() {
        let codec = ChatCompletionsCodec::new();
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
            response_id: Some("resp-1".to_string()),
            stop_details: None,
            extensions: Default::default(),
        };

        let encoded = codec.encode_response(&ir).unwrap();
        let body = encoded.as_object().unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "Hello!");
        assert_eq!(body["usage"]["prompt_tokens"], 10);
        assert_eq!(body["usage"]["completion_tokens"], 5);
    }

    #[test]
    fn test_encode_response_with_tool_calls() {
        let codec = ChatCompletionsCodec::new();
        let ir = IrResponse {
            content: vec![Content::ToolCall {
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                arguments: json!({"city": "London"}),
            }],
            usage: None,
            finish_reason: Some(FinishReason::ToolCalls),
            response_id: None,
            stop_details: None,
            extensions: Default::default(),
        };

        let encoded = codec.encode_response(&ir).unwrap();
        let choice = &encoded["choices"][0]["message"];
        assert_eq!(choice["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(choice["tool_calls"][0]["id"], "call_1");
    }

    #[test]
    fn test_stream_encoder_error_frame() {
        let mut encoder = ChatCompletionsStreamEncoder::new();
        let err_bytes = encoder.encode_error("rate limit exceeded", Some("429"));
        let err_str = String::from_utf8_lossy(&err_bytes);
        // Must contain "error" — protocol-native error frame
        assert!(err_str.contains("error"));
        assert!(err_str.contains("rate limit exceeded"));
    }

    #[test]
    fn test_stream_encoder_all_variants() {
        let mut encoder = ChatCompletionsStreamEncoder::new();

        // Each StreamPart variant should produce non-empty output (or valid empty)
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
            let result = encoder.encode_part(variant);
            assert!(result.is_ok(), "encode_part failed for variant");
        }
    }

    #[test]
    fn test_stream_decoder_text_delta() {
        let mut decoder = ChatCompletionsStreamDecoder::new();
        let line = "data: {\"id\":\"r1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n";
        let parts = decoder.feed(line).unwrap();
        // First part is ResponseStarted (from id), second is TextDelta
        assert!(parts.len() >= 1);
        assert!(parts
            .iter()
            .any(|p| matches!(p, StreamPart::TextDelta { .. })));
    }

    #[test]
    fn test_stream_decoder_finish() {
        let mut decoder = ChatCompletionsStreamDecoder::new();
        let line =
            "data: {\"id\":\"r1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"finish_reason\":\"stop\"}]}\n";
        let parts = decoder.feed(line).unwrap();
        assert!(parts.iter().any(|p| matches!(p, StreamPart::Finish { .. })));
    }

    #[test]
    fn test_stream_decoder_error_frame() {
        let mut decoder = ChatCompletionsStreamDecoder::new();
        let line = "data: {\"error\":{\"message\":\"rate limit\",\"code\":\"429\"}}\n";
        let parts = decoder.feed(line).unwrap();
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            StreamPart::Error { message, code } => {
                assert!(message.contains("rate limit"));
                assert_eq!(code.as_deref(), Some("429"));
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    #[test]
    fn test_codec_capabilities() {
        let codec = ChatCompletionsCodec::new();
        let caps = codec.capabilities();
        assert!(caps.streaming);
        assert!(caps.tools);
        assert!(caps.function_calling);
        assert!(caps.parallel_tool_calls);
        assert!(caps.lossy_default_reject);
    }

    #[test]
    fn test_codec_id_matches() {
        let codec = ChatCompletionsCodec::new();
        assert_eq!(codec.id().suite, ProtocolSuite::OpenAiCompatible);
        assert!(codec.id().full_id().contains("chat-completions"));
    }

    #[test]
    fn test_snapshot_decode_request() {
        let codec = ChatCompletionsCodec::new();
        let env = make_raw_envelope();
        let ir = codec.decode_request(make_basic_request(), &env).unwrap();
        insta::assert_debug_snapshot!(ir);
    }

    #[test]
    fn test_snapshot_decode_tool_request() {
        let codec = ChatCompletionsCodec::new();
        let env = make_raw_envelope();
        let ir = codec.decode_request(make_tool_request(), &env).unwrap();
        insta::assert_debug_snapshot!(ir);
    }

    #[test]
    fn test_encode_response_includes_cached_tokens() {
        // IR 带 cache_read_tokens → Chat 输出 prompt_tokens_details.cached_tokens
        let codec = ChatCompletionsCodec::new();
        let ir = IrResponse {
            content: vec![Content::Text {
                text: "ok".to_string(),
            }],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                reasoning_tokens: Some(10),
                cache_read_tokens: Some(80),
                cache_write_tokens: None,
            }),
            finish_reason: Some(FinishReason::Stop),
            response_id: Some("r1".to_string()),
            stop_details: None,
            extensions: std::collections::HashMap::new(),
        };
        let encoded = codec.encode_response(&ir).unwrap();
        // OpenAI 规范：prompt_tokens 含 cache
        assert_eq!(encoded["usage"]["prompt_tokens"], 180);
        assert_eq!(encoded["usage"]["total_tokens"], 230);
        assert_eq!(
            encoded["usage"]["prompt_tokens_details"]["cached_tokens"],
            80
        );
        assert_eq!(
            encoded["usage"]["completion_tokens_details"]["reasoning_tokens"],
            10
        );
    }

    #[test]
    fn test_stream_usage_includes_cached_tokens() {
        // 流式 Usage 帧保留 cache + reasoning
        let mut enc = ChatCompletionsStreamEncoder::new();
        let usage = Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            reasoning_tokens: Some(20),
            cache_read_tokens: Some(80),
            cache_write_tokens: None,
        };
        let bytes = enc.encode_part(&StreamPart::Usage { usage }).unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\"cached_tokens\":80"));
        assert!(s.contains("\"reasoning_tokens\":20"));
    }
}
