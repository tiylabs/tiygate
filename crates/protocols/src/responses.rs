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

        // Preserve protocol-specific fields in extensions for round-trip fidelity:
        // - tool_choice: "auto" | "required" | {"type":"function","name":"x"}
        // - text.format: structured output configuration
        // - reasoning.effort: reasoning depth control
        let mut extensions = std::collections::HashMap::new();
        if let Some(tc) = body.get("tool_choice") {
            extensions.insert("tool_choice".to_string(), tc.clone());
        }
        if let Some(tf) = body.get("text") {
            extensions.insert("text".to_string(), tf.clone());
        }
        if let Some(re) = body.get("reasoning") {
            if let Some(effort) = re.get("effort").and_then(|v| v.as_str()) {
                extensions.insert("reasoning_effort".to_string(), json!(effort));
            }
        }

        Ok(IrRequest {
            model,
            system,
            messages,
            tools,
            params,
            response_format: None,
            stream,
            ingress_protocol: self.id.clone(),
            extensions,
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
                    tool_calls.push(json!({"call_id": id, "type": "function_call", "name": name, "arguments": serde_json::to_string(arguments).unwrap_or_default(), "status": "completed"}));
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
            // OpenAI Responses 规范：input_tokens 必须含 cache 命中，所以从其他协议流入时
            // codec 内部把 cache_* 累加进 input_tokens
            let cache_read = usage.cache_read_tokens.unwrap_or(0);
            let cache_write = usage.cache_write_tokens.unwrap_or(0);
            let prompt_for_responses = usage.prompt_tokens + cache_read + cache_write;
            let total_for_responses = prompt_for_responses + usage.completion_tokens;
            response["usage"] = json!({
                "input_tokens": prompt_for_responses,
                "output_tokens": usage.completion_tokens,
                "total_tokens": total_for_responses,
            });
            if cache_read > 0 {
                response["usage"]["input_tokens_details"] = json!({"cached_tokens": cache_read});
            }
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
                    let mut text_parts: Vec<Value> = Vec::new();
                    let mut reasoning_text = String::new();
                    let mut tool_calls_json: Vec<Value> = Vec::new();

                    for c in &msg.content {
                        match c {
                            Content::Text { text } => {
                                text_parts.push(json!({"type": "input_text", "text": text}));
                            }
                            Content::Media {
                                source, mime_type, ..
                            } => match source {
                                tiygate_core::ir::MediaSource::Url { url } => {
                                    text_parts
                                        .push(json!({"type": "input_image", "image_url": url}));
                                }
                                tiygate_core::ir::MediaSource::Inline { data } => {
                                    text_parts.push(json!({
                                        "type": "input_image",
                                        "image_url": format!("data:{};base64,{}", mime_type, data)
                                    }));
                                }
                                _ => {}
                            },
                            Content::Reasoning { text } => {
                                // Responses API treats reasoning as a sibling
                                // output/input item, NOT as a content sub-part
                                // of the message. The OpenAI Responses spec
                                // (and the Deepseek thinking-mode spec it
                                // mirrors) requires that the reasoning item be
                                // echoed back alongside any function_call
                                // item the same turn produced — otherwise
                                // the request is rejected.
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
                                let args_str = match arguments {
                                    serde_json::Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                };
                                tool_calls_json.push(json!({
                                    "type": "function_call",
                                    "call_id": id,
                                    "name": name,
                                    "arguments": args_str,
                                    "status": "completed",
                                }));
                            }
                            Content::ToolResult { .. } => {
                                // Tool results live in their own input item
                                // and are pushed in the Role::Tool branch.
                            }
                        }
                    }

                    if !reasoning_text.is_empty() {
                        input_items.push(json!({
                            "type": "reasoning",
                            "summary": [{"type": "summary_text", "text": reasoning_text}],
                        }));
                    }

                    for tc in tool_calls_json {
                        input_items.push(tc);
                    }

                    // Only emit a message item if there is text/image content.
                    // A turn that is purely reasoning + function_call (the
                    // shape Responses actually returns) is fully represented
                    // by the items above.
                    if !text_parts.is_empty() {
                        let mut item = json!({"role": role_str});
                        if text_parts.len() == 1
                            && text_parts[0]
                                .get("type")
                                .map(|v| v == "input_text")
                                .unwrap_or(false)
                        {
                            item["content"] = text_parts[0]["text"].clone();
                        } else {
                            item["content"] = json!(text_parts);
                        }
                        input_items.push(item);
                    }
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
        let usage = body.get("usage").map(|u| {
            let cache_read = u["input_tokens_details"]["cached_tokens"].as_u64();
            // Responses' `input_tokens` includes the cached portion; the IR
            // convention keeps prompt_tokens cache-free. Subtract to avoid
            // double-counting when re-encoded downstream.
            let raw_input = u["input_tokens"].as_u64().unwrap_or(0);
            Usage {
                prompt_tokens: raw_input.saturating_sub(cache_read.unwrap_or(0)),
                completion_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                total_tokens: u["total_tokens"].as_u64().unwrap_or(0),
                reasoning_tokens: u["output_tokens_details"]["reasoning_tokens"].as_u64(),
                cache_read_tokens: cache_read,
                ..Default::default()
            }
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
    /// Next output item index to allocate. The text message and each function
    /// call occupy distinct output_index slots so a Responses client can
    /// reassemble them independently.
    next_output_index: u32,
    /// output_index assigned to the assistant text message (lazily allocated
    /// on the first TextDelta), so all text fragments share one index.
    text_output_index: Option<u32>,
    /// Maps a function-call id to its allocated output_index, so argument
    /// fragments target the correct call.
    tool_output_indices: std::collections::HashMap<String, u32>,
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
            next_output_index: 0,
            text_output_index: None,
            tool_output_indices: std::collections::HashMap::new(),
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
                // All text fragments belong to one assistant message item;
                // allocate its output_index once and reuse it.
                let idx = *self.text_output_index.get_or_insert_with(|| {
                    let i = self.next_output_index;
                    self.next_output_index += 1;
                    i
                });
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
                    // Opener: allocate a distinct output_index for this call.
                    let idx = self.next_output_index;
                    self.next_output_index += 1;
                    self.tool_output_indices.insert(id.clone(), idx);
                    format!(
                        "data: {}\n\n",
                        json!({"type": "response.output_item.added", "output_index": idx, "item": {"id": id, "type": "function_call", "name": n, "arguments": "", "status": "in_progress"}})
                    )
                } else {
                    let idx = self.tool_output_indices.get(id).copied().unwrap_or(0);
                    format!(
                        "data: {}\n\n",
                        json!({"type": "response.function_call_arguments.delta", "item_id": id, "output_index": idx, "delta": arguments})
                    )
                }
            }
            StreamPart::Usage { usage } => {
                // IR prompt_tokens is cache-free; Responses requires input_tokens
                // to include cache. Re-add so streamed usage stays consistent.
                let cache_read = usage.cache_read_tokens.unwrap_or(0);
                let cache_write = usage.cache_write_tokens.unwrap_or(0);
                let input_for_responses = usage.prompt_tokens + cache_read + cache_write;
                format!(
                    "data: {}\n\n",
                    json!({"type": "response.completed", "response": {"id": self.response_id.as_deref().unwrap_or(""), "usage": {"input_tokens": input_for_responses, "output_tokens": usage.completion_tokens, "total_tokens": input_for_responses + usage.completion_tokens}}})
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
            Some("response.reasoning_text.delta")
            | Some("response.reasoning_summary_text.delta") => {
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
                    // Argument fragment: `name: None` so cross-protocol
                    // encoders route this to their argument-delta event
                    // instead of re-opening the tool-call block.
                    parts.push(StreamPart::ToolCallDelta {
                        id: self.current_call_id.clone().unwrap_or_default(),
                        name: None,
                        arguments: args.to_string(),
                    });
                }
            }
            Some("response.output_item.done") => {
                self.in_function_call = false;
                self.current_call_id = None;
                self.current_call_name = None;
            }
            // Lifecycle / bookkeeping events that carry no IR-relevant payload.
            // OpenAI Responses streams interleave many of these; they must be
            // consumed silently (NOT turned into error frames) so the stream
            // is not corrupted. See the Responses streaming event reference.
            Some("response.in_progress")
            | Some("response.content_part.added")
            | Some("response.content_part.done")
            | Some("response.output_text.done")
            | Some("response.output_text.annotation.added")
            | Some("response.function_call_arguments.done")
            | Some("response.reasoning_text.done")
            | Some("response.reasoning_summary_text.done")
            | Some("response.reasoning_summary_part.added")
            | Some("response.reasoning_summary_part.done")
            | Some("response.queued") => {
                // no-op: lifecycle marker
            }
            Some("response.completed") => {
                if let Some(usage) = event["response"]["usage"].as_object() {
                    let cache_read = usage
                        .get("input_tokens_details")
                        .and_then(|d| d["cached_tokens"].as_u64());
                    let raw_input = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    parts.push(StreamPart::Usage {
                        usage: Usage {
                            // input_tokens includes cache; IR keeps it cache-free.
                            prompt_tokens: raw_input.saturating_sub(cache_read.unwrap_or(0)),
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
                            cache_read_tokens: cache_read,
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
            Some("error") | Some("response.failed") => {
                let err = if event.get("error").is_some() {
                    &event["error"]
                } else {
                    &event["response"]["error"]
                };
                parts.push(StreamPart::Error {
                    message: err["message"].as_str().unwrap_or("Unknown").to_string(),
                    code: err["type"].as_str().map(String::from),
                });
            }
            Some("response.incomplete") => {
                let reason = if self.in_function_call {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Length
                };
                parts.push(StreamPart::Finish { reason });
            }
            Some(_other) => {
                // Unknown / future Responses event types must NOT abort the
                // stream. Ignore per UnknownFieldPolicy::Drop.
            }
            None => {
                // SSE comment/keepalive lines without a `type` field are ignored.
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

    #[test]
    fn test_encode_response_includes_cached_tokens() {
        // IR 带 cache → Responses 输出 input_tokens_details.cached_tokens
        let codec = ResponsesCodec::new();
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
        // OpenAI Responses 规范：input_tokens 含 cache
        assert_eq!(encoded["usage"]["input_tokens"], 180);
        assert_eq!(encoded["usage"]["total_tokens"], 230);
        assert_eq!(
            encoded["usage"]["input_tokens_details"]["cached_tokens"],
            80
        );
        assert_eq!(
            encoded["usage"]["output_tokens_details"]["reasoning_tokens"],
            10
        );
    }

    /// Reasoning + function_call on a single assistant turn must round-trip
    /// through `encode_request` as siblings in the `input[]` array, so the
    /// Responses API receives the reasoning item it requires to continue the
    /// chain-of-thought. Regression test for the gap where Reasoning content
    /// was silently dropped during request encoding.
    #[test]
    fn test_encode_request_echoes_reasoning_alongside_tool_call() {
        let codec = ResponsesCodec::new();
        let ir = tiygate_core::IrRequest {
            model: "gpt-5".to_string(),
            system: None,
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::OpenAiCompatible,
                "chat-completions",
                "v1",
            ),
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "杭州明天天气？".to_string(),
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Content::Reasoning {
                            text: "我需要先查日期再查天气。".to_string(),
                        },
                        Content::ToolCall {
                            id: "call_1".to_string(),
                            name: "get_weather".to_string(),
                            arguments: serde_json::json!({"location": "杭州"}),
                        },
                    ],
                },
                Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult {
                        tool_call_id: "call_1".to_string(),
                        name: "get_weather".to_string(),
                        content: "cloudy".to_string(),
                    }],
                },
            ],
            tools: vec![],
            params: tiygate_core::GenerationParams::default(),
            stream: false,
            response_format: None,
            extensions: Default::default(),
        };
        let (body, _) = codec.encode_request(&ir).unwrap();
        let input = body["input"].as_array().expect("input[] present");

        // Must contain, in order: user message, reasoning item, function_call
        // item, function_call_output item. The reasoning item MUST sit
        // *before* the function_call it justifies, matching the wire format
        // Responses returns.
        // (User/assistant message items have no `type` discriminator —
        // they're identified by `role`. Reasoning/function_call items are
        // identified by `type`.)
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input.len(), 4, "no extra items beyond the four above");

        let reasoning = &input[1];
        assert_eq!(reasoning["type"], "reasoning");
        assert_eq!(reasoning["summary"][0]["type"], "summary_text");
        assert_eq!(reasoning["summary"][0]["text"], "我需要先查日期再查天气。");

        let fc = &input[2];
        assert_eq!(fc["type"], "function_call");
        assert_eq!(fc["call_id"], "call_1");
        assert_eq!(fc["name"], "get_weather");
    }

    /// When an assistant turn is purely reasoning (no text, no tool call) the
    /// encoder must still emit the reasoning item, and must NOT emit an empty
    /// message item in its place.
    #[test]
    fn test_encode_request_emits_reasoning_only_turn() {
        let codec = ResponsesCodec::new();
        let ir = tiygate_core::IrRequest {
            model: "gpt-5".to_string(),
            system: None,
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::OpenAiCompatible,
                "chat-completions",
                "v1",
            ),
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![Content::Reasoning {
                    text: "thinking...".to_string(),
                }],
            }],
            tools: vec![],
            params: tiygate_core::GenerationParams::default(),
            stream: false,
            response_format: None,
            extensions: Default::default(),
        };
        let (body, _) = codec.encode_request(&ir).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "reasoning");
    }
}
