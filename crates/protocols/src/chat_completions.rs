//! OpenAI Chat Completions protocol codec.
//!
//! Implements bidirectional conversion between OpenAI's Chat Completions API
//! and the canonical IR. Supports both streaming (SSE) and non-streaming modes.

use http::HeaderMap;
use serde_json::{json, Value};

use tiygate_core::{
    Content, EndpointCapabilities, EndpointCodec, ErrorClass, FinishReason, IrRequest, IrResponse,
    Message, ProtocolEndpoint, ProtocolSuite, RawEnvelope, Role, StreamDecoder, StreamEncoder,
    StreamPart, Tool, Usage,
};

/// Map an `ErrorClass` to the OpenAI-native `error.type` string.
///
/// This mapping table is shared by `encode_part` (streaming error frames)
/// and `encode_error_body` (non-streaming error responses).
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
                    // "function" is the legacy OpenAI role for tool results,
                    // superseded by "tool"; map both so legacy clients work.
                    "tool" | "function" => Role::Tool,
                    _ => Role::User,
                };

                let mut content = if role == Role::Tool {
                    // OpenAI tool messages carry the result as a top-level
                    // string `content` with the `tool_call_id` as a sibling
                    // field. Without this branch the string path below would
                    // turn it into a plain `Content::Text`, dropping the
                    // `tool_call_id` and producing an Anthropic/Gemini request
                    // whose `tool_use` blocks have no matching `tool_result`
                    // (upstream 400 invalid_params). The content may also be an
                    // array of content parts in newer OpenAI variants, which
                    // `parse_content_array` already handles for Role::Tool.
                    if let Some(arr) = msg["content"].as_array() {
                        parse_content_array(arr, &role)
                    } else {
                        vec![Content::ToolResult {
                            tool_call_id: msg["tool_call_id"].as_str().unwrap_or("").to_string(),
                            name: msg["name"].as_str().unwrap_or("").to_string(),
                            content: msg["content"].as_str().unwrap_or("").to_string(),
                            id: None,
                        }]
                    }
                } else if let Some(text) = msg["content"].as_str() {
                    // A non-empty assistant text alongside tool_calls is common;
                    // skip empty strings to avoid emitting blank text blocks.
                    if text.is_empty() {
                        Vec::new()
                    } else {
                        vec![Content::Text {
                            text: text.to_string(),
                            annotations: None,
                        }]
                    }
                } else if let Some(arr) = msg["content"].as_array() {
                    parse_content_array(arr, &role)
                } else {
                    Vec::new()
                };

                // Deepseek / many OpenAI-compatible providers carry the
                // assistant's chain-of-thought as `reasoning_content` (Deepseek)
                // or `reasoning` (some proxies) alongside `content`. Preserve it
                // as a Reasoning block so multi-turn replays keep the thinking.
                if let Some(rc) = msg["reasoning_content"]
                    .as_str()
                    .or_else(|| msg["reasoning"].as_str())
                    .or_else(|| msg["thinking"].as_str())
                {
                    if !rc.is_empty() {
                        content.insert(
                            0,
                            Content::Reasoning {
                                text: rc.to_string(),
                                signature: None,
                                id: None,
                                encrypted_content: None,
                            },
                        );
                    }
                }

                // OpenAI assistant messages may carry `tool_calls` ALONGSIDE a
                // textual `content` (string or array), not only when content is
                // null. Parse them independently and append so the resulting IR
                // keeps both the assistant text and the ToolCall blocks. Missing
                // this caused the re-encoded Anthropic request to omit the
                // `tool_use` block, so a later `tool_result` referenced an
                // unknown id (upstream 400 invalid_params).
                if let Some(tool_calls) = msg["tool_calls"].as_array() {
                    for tc in tool_calls {
                        let arguments = match &tc["function"]["arguments"] {
                            // OpenAI sends arguments as a JSON string; parse to
                            // object. If it is not valid JSON, preserve the raw
                            // string as a JSON string value rather than dropping
                            // it to `{}`, so non-standard payloads survive.
                            serde_json::Value::String(s) => serde_json::from_str(s)
                                .unwrap_or_else(|_| serde_json::Value::String(s.clone())),
                            // Some compatible providers may already send an object.
                            serde_json::Value::Null => json!({}),
                            other => other.clone(),
                        };
                        content.push(Content::ToolCall {
                            id: tc["id"].as_str().unwrap_or("").to_string(),
                            name: tc["function"]["name"].as_str().unwrap_or("").to_string(),
                            arguments,
                            call_id: None,
                        });
                    }
                }

                // Guarantee at least one content block so downstream encoders
                // never emit an empty `content` array.
                if content.is_empty() {
                    content.push(Content::Text {
                        text: String::new(),
                        annotations: None,
                    });
                }

                messages.push(Message { role, content });
            }
        }

        // Extract system message(s) if present. OpenAI permits multiple
        // system/developer messages anywhere in the list; concatenate all of
        // their text parts so none are lost. The previous implementation only
        // captured the first text part of the first system message.
        let system_chunks: Vec<String> = messages
            .iter()
            .filter(|m| m.role == Role::System)
            .flat_map(|m| {
                m.content.iter().filter_map(|c| match c {
                    Content::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
            })
            .collect();
        let system = if system_chunks.is_empty() {
            None
        } else {
            Some(system_chunks.join("\n"))
        };

        // Filter out system messages from the list
        let messages: Vec<Message> = messages
            .into_iter()
            .filter(|m| m.role != Role::System)
            .collect();

        // Parse tools
        // OpenAI's `parallel_tool_calls` (default true when tools are present)
        // has no representation in the IR beyond `Tool.required`. The lossy
        // check treats any required tool as "parallel tool calls requested" so
        // it can reject crossings to protocols (Anthropic/Gemini) that cannot
        // express concurrent fan-out. We only set it when tools are actually
        // offered and tool usage is not disabled via tool_choice="none".
        let tool_choice_is_none = body
            .get("tool_choice")
            .and_then(|v| v.as_str())
            .map(|s| s == "none")
            .unwrap_or(false);
        // Per docs/protocol-capability-matrix.md §1, the chat→messages /
        // chat→gemini crossing is only rejected when the request EXPLICITLY
        // opts into `parallel_tool_calls=true`. A plain tools request (flag
        // absent) must still convert, so we default to false here rather than
        // mirroring OpenAI's API-level default of true.
        let parallel_tool_calls = body["parallel_tool_calls"].as_bool().unwrap_or(false);
        let mark_required = parallel_tool_calls && !tool_choice_is_none;
        let tools: Vec<Tool> = body["tools"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|t| Tool {
                        name: t["function"]["name"].as_str().unwrap_or("").to_string(),
                        description: t["function"]["description"].as_str().map(|s| s.to_string()),
                        parameters: Some(t["function"]["parameters"].clone()),
                        required: mark_required,
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

        // Pass through OpenAI-specific top-level fields that the IR does not
        // model explicitly, so a same-protocol (or chat-compatible) re-encode
        // is lossless. Stored under a protocol-prefixed key to avoid clashing
        // with the semantically-modeled extensions (tool_choice/text/etc.).
        {
            let mut extra = serde_json::Map::new();
            for key in [
                "parallel_tool_calls",
                "n",
                "logit_bias",
                "logprobs",
                "top_logprobs",
                "user",
                "reasoning_effort",
                "stream_options",
                "response_format",
                "service_tier",
                "store",
                "metadata",
                "prompt_cache_key",
                "prompt_cache_retention",
            ] {
                if let Some(v) = body.get(key) {
                    extra.insert(key.to_string(), v.clone());
                }
            }
            if !extra.is_empty() {
                extensions.insert("openai_extra".to_string(), json!(extra));
            }
        }

        let params = tiygate_core::GenerationParams {
            // OpenAI 已弃用 max_tokens，推荐使用 max_completion_tokens（o-series 必须）
            max_tokens: body["max_completion_tokens"]
                .as_u64()
                .or_else(|| body["max_tokens"].as_u64())
                .map(|v| v as u32),
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
            thinking: body["reasoning_effort"].as_str().map(|s| {
                use tiygate_core::ThinkingEffort;
                let effort = match s {
                    "minimal" => ThinkingEffort::Minimal,
                    "low" => ThinkingEffort::Low,
                    "medium" => ThinkingEffort::Medium,
                    "high" => ThinkingEffort::High,
                    "xhigh" => ThinkingEffort::XHigh,
                    "max" => ThinkingEffort::Max,
                    _ => ThinkingEffort::High,
                };
                tiygate_core::ThinkingConfig {
                    effort: Some(effort),
                    ..Default::default()
                }
            }),
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
            metadata: body.get("metadata").and_then(|m| m.as_object()).map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            }),
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
        let mut message_refusal = String::new();
        let mut message_annotations: Vec<Value> = Vec::new();
        let mut tool_calls_json = Vec::new();

        for content in &ir.content {
            match content {
                Content::Text { text, annotations } => {
                    message_content.push_str(text);
                    if let Some(ref anns) = annotations {
                        for a in anns {
                            let ann_json = match a.kind {
                                tiygate_core::AnnotationKind::UrlCitation => {
                                    let mut obj =
                                        json!({"type": "url_citation", "url_citation": {}});
                                    if let Some(ref url) = a.url {
                                        obj["url_citation"]["url"] = json!(url);
                                    }
                                    if let Some(ref title) = a.title {
                                        obj["url_citation"]["title"] = json!(title);
                                    }
                                    if let Some(si) = a.start_index {
                                        obj["start_index"] = json!(si);
                                    }
                                    if let Some(ei) = a.end_index {
                                        obj["end_index"] = json!(ei);
                                    }
                                    obj
                                }
                                tiygate_core::AnnotationKind::FileCitation => {
                                    json!({"type": "file_citation", "file_citation": {"filename": a.title}})
                                }
                            };
                            message_annotations.push(ann_json);
                        }
                    }
                }
                Content::Reasoning { text: _, .. } => {
                    // OpenAI doesn't natively expose reasoning text in the content field
                    // (it goes into a separate reasoning_tokens field)
                }
                Content::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
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
                Content::Refusal { text, .. } => {
                    if !message_refusal.is_empty() {
                        message_refusal.push('\n');
                    }
                    message_refusal.push_str(text);
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
        if !message_refusal.is_empty() {
            message["refusal"] = json!(message_refusal);
        }
        if !message_annotations.is_empty() {
            message["annotations"] = json!(message_annotations);
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
                    Content::Text { text, .. } => {
                        text_parts.push(json!({"type": "text", "text": text}));
                    }
                    Content::Reasoning { text, .. } => {
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
                        ..
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
                        source,
                        mime_type,
                        metadata,
                        ..
                    } => match source {
                        tiygate_core::ir::MediaSource::Url { url } => {
                            let mut img = json!({"url": url});
                            if let Some(d) = metadata.get(tiygate_core::ir::IMAGE_DETAIL_KEY) {
                                img["detail"] = d.clone();
                            }
                            text_parts.push(json!({
                                "type": "image_url",
                                "image_url": img
                            }));
                        }
                        tiygate_core::ir::MediaSource::Inline { data } => {
                            let mut img =
                                json!({"url": format!("data:{};base64,{}", mime_type, data)});
                            if let Some(d) = metadata.get(tiygate_core::ir::IMAGE_DETAIL_KEY) {
                                img["detail"] = d.clone();
                            }
                            text_parts.push(json!({
                                "type": "image_url",
                                "image_url": img
                            }));
                        }
                        _ => {}
                    },
                    Content::ToolResult {
                        tool_call_id,
                        name: _,
                        content,
                        ..
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
                    Content::Refusal { text, .. } => {
                        text_parts.push(json!({"type": "text", "text": text}));
                    }
                }
            }

            if text_parts.len() == 1 && text_parts[0].get("text").is_some() {
                msg_json["content"] = text_parts[0]["text"].clone();
            } else if !text_parts.is_empty() {
                msg_json["content"] = json!(text_parts);
            } else if !tool_calls_json.is_empty() {
                // Allow null/empty content when the turn is purely reasoning
                // + tool calls (Deepseek emits this exact shape).
                msg_json["content"] = Value::Null;
            } else {
                msg_json["content"] = json!("");
            }

            // Reasoning is only echoed back alongside a tool-call turn (see the
            // DeepSeek thinking-with-tools rules above). It is therefore also
            // only meaningful to emit reasoning_content when tool_calls exist.
            if !reasoning_text.is_empty() && !tool_calls_json.is_empty() {
                // DeepSeek thinking-with-tools 有两条相反规则:
                // 1. 当 assistant 轮包含 tool_calls 时,reasoning_content 必须
                //    回传,否则报 "The reasoning_content in the thinking mode
                //    must be passed back to the API"。
                // 2. 普通(无 tool_calls)多轮必须移除 reasoning_content,否则
                //    报 400 (reasoning_content included)。
                // 因此仅在该轮含 tool_calls 时回传,纯文本多轮丢弃。
                msg_json["reasoning_content"] = json!(reasoning_text);
            }

            if !tool_calls_json.is_empty() {
                msg_json["tool_calls"] = json!(tool_calls_json);
            }

            // Avoid emitting a duplicate empty assistant message when the only
            // contribution was a ToolResult (already pushed as its own message)
            // or a pure-reasoning turn (reasoning is dropped without tool_calls,
            // so it would otherwise yield a bogus empty assistant message).
            let has_real_content = !text_parts.is_empty() || !tool_calls_json.is_empty();
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
            // OpenAI o-series models require max_completion_tokens;
            // max_tokens is deprecated in the Chat Completions spec.
            // Emit both so the request works across all model families.
            body["max_completion_tokens"] = json!(mt);
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
        // Thinking config: output reasoning_effort from params.thinking.effort.
        // Cross-protocol derivation: when effort is missing but budget_tokens is
        // present (e.g. from Anthropic/Gemini), derive effort from the budget.
        if let Some(ref thinking) = ir.params.thinking {
            let effort = thinking.effort.or_else(|| {
                thinking
                    .budget_tokens
                    .map(tiygate_core::ThinkingConfig::budget_to_effort)
            });
            if let Some(effort) = effort {
                // OpenAI supports minimal/low/medium/high/xhigh; Max clamps to
                // "xhigh" since OpenAI has no "max" effort level.
                body["reasoning_effort"] = json!(match effort {
                    tiygate_core::ThinkingEffort::Minimal => "minimal",
                    tiygate_core::ThinkingEffort::Low => "low",
                    tiygate_core::ThinkingEffort::Medium => "medium",
                    tiygate_core::ThinkingEffort::High => "high",
                    tiygate_core::ThinkingEffort::XHigh => "xhigh",
                    tiygate_core::ThinkingEffort::Max => "xhigh",
                });
            }
        }

        // Metadata: output from ir.metadata as JSON object
        if let Some(ref metadata) = ir.metadata {
            if !metadata.is_empty() {
                let mut meta = serde_json::Map::new();
                for (k, v) in metadata {
                    meta.insert(k.clone(), json!(v));
                }
                body["metadata"] = json!(meta);
            }
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

        // Replay OpenAI-specific top-level fields captured at decode time.
        // Only fields not already set by the modeled path above are written,
        // so explicit params win over the passthrough copy.
        if let Some(extra) = ir
            .extensions
            .get("openai_extra")
            .and_then(|v| v.as_object())
        {
            for (k, v) in extra {
                if body.get(k).is_none() {
                    body[k] = v.clone();
                }
            }
        }

        // When streaming, request usage in the final chunk so the gateway can
        // bill streamed responses. Respect an explicit stream_options the
        // client already provided.
        if ir.stream {
            let include = body
                .get("stream_options")
                .and_then(|o| o.get("include_usage"))
                .and_then(|v| v.as_bool());
            if include != Some(true) {
                let opts = body
                    .as_object_mut()
                    .and_then(|o| o.get_mut("stream_options"));
                match opts {
                    Some(o) if o.is_object() => {
                        o["include_usage"] = json!(true);
                    }
                    _ => {
                        body["stream_options"] = json!({"include_usage": true});
                    }
                }
            }
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
                        let annotations = parse_openai_annotations(&msg["annotations"]);
                        content.push(Content::Text {
                            text: text.to_string(),
                            annotations,
                        });
                    }
                }

                // Refusal content (OpenAI spec: message.refusal)
                // https://platform.openai.com/docs/api-reference/chat/object
                if let Some(refusal_text) = msg["refusal"].as_str() {
                    if !refusal_text.is_empty() {
                        content.push(Content::Refusal {
                            text: refusal_text.to_string(),
                            category: None,
                        });
                    }
                }

                // Reasoning content — supports multiple vendor-specific field names:
                // - reasoning_content (DeepSeek, Moonshot/Kimi, Qwen)
                // - reasoning (some OpenRouter passthroughs)
                // - thinking (Aliyun)
                // Per Deepseek spec, when a turn contains tool_calls this MUST be
                // echoed back in subsequent requests, or the API returns 400.
                let reasoning_text = ["reasoning_content", "reasoning", "thinking"]
                    .iter()
                    .find_map(|key| msg[*key].as_str().filter(|s| !s.is_empty()));
                if let Some(text) = reasoning_text {
                    content.push(Content::Reasoning {
                        text: text.to_string(),
                        signature: None,
                        id: None,
                        encrypted_content: None,
                    });
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
                            call_id: None,
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
                                signature: None,
                                id: None,
                                encrypted_content: None,
                            });
                        }
                    }
                }
            }
        }

        let has_refusal = body["choices"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|c| c["message"]["refusal"].as_str())
            .is_some_and(|r| !r.is_empty());

        let finish_reason = body["choices"][0]["finish_reason"]
            .as_str()
            .map(|s| match s {
                "stop" => FinishReason::Stop,
                "length" => FinishReason::Length,
                "content_filter" => FinishReason::ContentFilter,
                "tool_calls" => FinishReason::ToolCalls,
                // Legacy OpenAI function-calling finish reason, superseded by
                // "tool_calls" but still emitted by older models/proxies.
                "function_call" => FinishReason::ToolCalls,
                _ => FinishReason::Other(s.to_string()),
            })
            // A `refusal` field signals content-filter regardless of what
            // `finish_reason` claims, so the caller can surface the refusal.
            .map(|fr| {
                if has_refusal {
                    FinishReason::ContentFilter
                } else {
                    fr
                }
            });

        // Populate stop_details when the upstream signals a content filter
        // or refusal, so cross-protocol targets can surface the reason.
        let stop_details = if has_refusal {
            Some(tiygate_core::ir::StopDetails {
                stop_reason: "refusal".to_string(),
                kind: Some("refusal".to_string()),
                ..Default::default()
            })
        } else {
            body["choices"][0]["finish_reason"]
                .as_str()
                .filter(|&s| s == "content_filter")
                .map(|_| tiygate_core::ir::StopDetails {
                    stop_reason: "content_filter".to_string(),
                    kind: Some("content_filter".to_string()),
                    ..Default::default()
                })
        };

        // Guard against `"usage": null` — `body.get("usage")` returns
        // `Some(Value::Null)` for null, which would produce a zero-valued
        // `Usage` and shadow real usage on cross-protocol re-encode.
        let usage = body
            .get("usage")
            .filter(|u| {
                u.is_object()
                    && (u["prompt_tokens"].is_u64()
                        || u["completion_tokens"].is_u64()
                        || u["total_tokens"].is_u64())
            })
            .map(|u| {
                let cache_read = u["prompt_tokens_details"]["cached_tokens"].as_u64();
                // OpenAI's `prompt_tokens` INCLUDES the cached portion. The IR
                // convention is that `prompt_tokens` is the NON-cached prompt and
                // cache lives in its own bucket, so subtract the cache here. This
                // prevents double-counting when the IR is later re-encoded to a
                // protocol whose encoder adds the cache back into prompt_tokens.
                let raw_prompt = u["prompt_tokens"].as_u64().unwrap_or(0);
                let prompt_excl_cache = raw_prompt.saturating_sub(cache_read.unwrap_or(0));
                Usage {
                    prompt_tokens: prompt_excl_cache,
                    completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
                    total_tokens: u["total_tokens"].as_u64().unwrap_or(0),
                    reasoning_tokens: u["completion_tokens_details"]["reasoning_tokens"].as_u64(),
                    cache_read_tokens: cache_read,
                    ..Default::default()
                }
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
        Box::new(ChatCompletionsStreamDecoder::new())
    }
}

// --- Stream Encoder ---

pub struct ChatCompletionsStreamEncoder {
    response_id: Option<String>,
    /// Maps a tool-call `id` to a stable, monotonically-assigned
    /// `tool_calls[].index`. OpenAI clients reassemble streamed tool calls by
    /// this index, so two distinct tool calls must NOT share index 0.
    tool_call_indices: std::collections::HashMap<String, usize>,
}

impl Default for ChatCompletionsStreamEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatCompletionsStreamEncoder {
    pub fn new() -> Self {
        Self {
            response_id: None,
            tool_call_indices: std::collections::HashMap::new(),
        }
    }

    /// Resolve the stable `tool_calls[].index` for a given tool-call id,
    /// allocating the next index the first time an id is seen. An empty id
    /// (some providers omit it on argument-only fragments) falls back to the
    /// most-recently allocated index so fragments append to the open call.
    fn tool_call_index(&mut self, id: &str) -> usize {
        if id.is_empty() {
            return self.tool_call_indices.len().saturating_sub(1);
        }
        let next = self.tool_call_indices.len();
        *self.tool_call_indices.entry(id.to_string()).or_insert(next)
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
            StreamPart::ReasoningDelta { text, .. } => {
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
                let tc_index = self.tool_call_index(id);
                let resp_id = self.response_id.clone().unwrap_or_default();
                let mut delta = json!({
                    "tool_calls": [{
                        "index": tc_index,
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
                // The IR keeps prompt_tokens cache-free; OpenAI's wire format
                // requires prompt_tokens to INCLUDE cache. Mirror the
                // non-stream encoder so streamed usage stays self-consistent
                // (prompt + completion == total).
                let cache_read = usage.cache_read_tokens.unwrap_or(0);
                let cache_write = usage.cache_write_tokens.unwrap_or(0);
                let prompt_for_openai = usage.prompt_tokens + cache_read + cache_write;
                let mut usage_obj = json!({
                    "prompt_tokens": prompt_for_openai,
                    "completion_tokens": usage.completion_tokens,
                    "total_tokens": prompt_for_openai + usage.completion_tokens,
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
            StreamPart::Error {
                message,
                class,
                upstream_code,
            } => {
                // Protocol-native error frame
                let mut err = json!({"message": message, "type": error_type_for_class(*class)});
                if let Some(c) = upstream_code {
                    err["code"] = json!(c);
                }
                format!("data: {}\n\n", json!({"error": err}))
            }
        };

        Ok(chunk.into_bytes())
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
        format!("data: {}\n\ndata: [DONE]\n\n", json!({"error": err})).into_bytes()
    }

    fn encode_done(&mut self) -> Vec<u8> {
        "data: [DONE]\n\n".to_string().into_bytes()
    }
}

// --- Stream Decoder (structure-dispatched via `object` field) ---

pub struct ChatCompletionsStreamDecoder {
    response_id: Option<String>,
    /// Per-index tool-call state `(id, name)`, tracked by the OpenAI
    /// `tool_calls[].index` field so that parallel tool calls keep their
    /// argument fragments bound to the correct call id. Earlier code used a
    /// single `Option<String>` which mis-attributed arg deltas when more than
    /// one tool call streamed concurrently.
    tool_calls: Vec<(String, String)>,
    /// Whether a `choices[].finish_reason` produced a `Finish` in-band.
    saw_finish: bool,
    /// Whether any `tool_calls` delta was observed. Used to infer
    /// `Finish(ToolCalls)` on `[DONE]` when a proxy ends a tool-call turn
    /// without sending `finish_reason: "tool_calls"`.
    saw_tool_calls: bool,
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
            tool_calls: Vec::new(),
            saw_finish: false,
            saw_tool_calls: false,
        }
    }

    /// Ensure `self.tool_calls` has a slot for `index`, then return a mutable
    /// reference to it.
    fn slot(&mut self, index: usize) -> &mut (String, String) {
        if self.tool_calls.len() <= index {
            self.tool_calls
                .resize_with(index + 1, || (String::new(), String::new()));
        }
        &mut self.tool_calls[index]
    }
}

impl StreamDecoder for ChatCompletionsStreamDecoder {
    fn feed(&mut self, line: &str) -> Result<Vec<StreamPart>, tiygate_core::Error> {
        let line = line.trim();
        if line.is_empty() || line == "data: [DONE]" {
            if line == "data: [DONE]" {
                let mut parts = Vec::new();
                // Tool-call turn fallback: some OpenAI-compatible proxies end a
                // tool-call turn with `[DONE]` but never send the
                // `finish_reason: "tool_calls"` chunk. Without this the IR
                // carries no `Finish`, so billing/observability lose the
                // tool-call stop semantics. Infer it from the observed
                // tool_calls deltas. Only synthesize when no in-band `Finish`
                // was seen, to avoid double terminators.
                if !self.saw_finish && self.saw_tool_calls {
                    parts.push(StreamPart::Finish {
                        reason: FinishReason::ToolCalls,
                    });
                }
                parts.push(StreamPart::ResponseCompleted {
                    id: self.response_id.clone().unwrap_or_default(),
                    status: "completed".to_string(),
                    usage: None,
                    extensions: std::collections::HashMap::new(),
                });
                return Ok(parts);
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
                    let code = error["code"].as_str().or_else(|| error["type"].as_str());
                    let upstream_code = code.map(String::from);
                    let class = tiygate_core::classify_upstream_error(None, code);
                    parts.push(StreamPart::Error {
                        message: error["message"]
                            .as_str()
                            .unwrap_or("Unknown error")
                            .to_string(),
                        class,
                        upstream_code,
                    });
                    return Ok(parts);
                }

                // Handle choices
                let mut pending_finishes = Vec::new();
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
                        // a sibling of `content` inside `delta`. DeepSeek v4-pro
                        // uses just `reasoning`. OpenAI's Responses API uses
                        // `reasoning_details`. Try all three, matching the
                        // non-streaming decoder (L134-136, L748).
                        if let Some(text) = delta["reasoning_content"]
                            .as_str()
                            .or_else(|| delta["reasoning"].as_str())
                        {
                            if !text.is_empty() {
                                parts.push(StreamPart::ReasoningDelta {
                                    text: text.to_string(),
                                    id: None,
                                    encrypted_content: None,
                                });
                            }
                        } else if let Some(reasoning) = delta.get("reasoning_details") {
                            if let Some(text) = reasoning["text"].as_str() {
                                parts.push(StreamPart::ReasoningDelta {
                                    text: text.to_string(),
                                    id: None,
                                    encrypted_content: None,
                                });
                            }
                        }

                        if let Some(tool_calls) = delta["tool_calls"].as_array() {
                            for tc in tool_calls {
                                self.saw_tool_calls = true;
                                // OpenAI streams parallel tool calls
                                // interleaved, distinguished by `index`. Track
                                // id/name per index so argument fragments are
                                // bound to the right call.
                                let index = tc["index"].as_u64().unwrap_or(0) as usize;
                                if let Some(tc_id) = tc["id"].as_str() {
                                    if !tc_id.is_empty() {
                                        self.slot(index).0 = tc_id.to_string();
                                    }
                                }
                                let tc_name = tc["function"]["name"]
                                    .as_str()
                                    .filter(|s| !s.is_empty())
                                    .map(String::from);
                                if let Some(ref n) = tc_name {
                                    self.slot(index).1 = n.clone();
                                }
                                let tc_args =
                                    tc["function"]["arguments"].as_str().map(String::from);
                                let id = self.slot(index).0.clone();
                                // Emit the opener (name present) and argument
                                // fragments (name absent) as DISTINCT deltas so
                                // cross-protocol encoders that key on
                                // `name == None` (Anthropic `input_json_delta`,
                                // Responses `function_call_arguments.delta`)
                                // receive the argument stream. The retained
                                // name must NOT leak onto arg-only deltas — that
                                // suppresses the argument frames.
                                match (tc_name, tc_args) {
                                    (Some(n), Some(args)) => {
                                        parts.push(StreamPart::ToolCallDelta {
                                            id: id.clone(),
                                            name: Some(n),
                                            arguments: String::new(),
                                        });
                                        if !args.is_empty() {
                                            parts.push(StreamPart::ToolCallDelta {
                                                id,
                                                name: None,
                                                arguments: args,
                                            });
                                        }
                                    }
                                    (Some(n), None) => {
                                        parts.push(StreamPart::ToolCallDelta {
                                            id,
                                            name: Some(n),
                                            arguments: String::new(),
                                        });
                                    }
                                    (None, Some(args)) => {
                                        parts.push(StreamPart::ToolCallDelta {
                                            id,
                                            name: None,
                                            arguments: args,
                                        });
                                    }
                                    (None, None) => {}
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
                                    "function_call" => FinishReason::ToolCalls,
                                    other => FinishReason::Other(other.to_string()),
                                };
                                pending_finishes.push(StreamPart::Finish { reason });
                                self.saw_finish = true;
                            }
                        }
                    }
                }

                // Usage — guard against `"usage": null` which OpenAI sends on
                // every non-final chunk when `stream_options.include_usage` is
                // true.  `chunk.get("usage")` returns `Some(Value::Null)` for
                // null, so without an object + token-field check we would push
                // a zero-valued `Usage` part that poisons cross-protocol
                // encoders (e.g. Responses defers completed on the first
                // non-null usage, emitting all-zeros before real usage arrives).
                if let Some(usage) = chunk.get("usage") {
                    if usage.is_object()
                        && (usage["prompt_tokens"].is_u64()
                            || usage["completion_tokens"].is_u64()
                            || usage["total_tokens"].is_u64())
                    {
                        let raw_prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
                        let completion = usage["completion_tokens"].as_u64().unwrap_or(0);
                        let total = usage["total_tokens"]
                            .as_u64()
                            .unwrap_or(raw_prompt + completion);
                        let cache_read = usage["prompt_tokens_details"]["cached_tokens"].as_u64();
                        let reasoning =
                            usage["completion_tokens_details"]["reasoning_tokens"].as_u64();
                        // OpenAI's prompt_tokens includes cache; the IR convention
                        // keeps prompt_tokens cache-free. Subtract to avoid double
                        // counting on re-encode.
                        let prompt = raw_prompt.saturating_sub(cache_read.unwrap_or(0));
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

                // OpenAI-compatible providers often put `finish_reason` and
                // final `usage` on the same chunk, with `finish_reason` inside
                // `choices` before top-level `usage`. Responses streaming
                // encodes usage inside the terminal `response.completed`, so
                // the IR order must be Usage → Finish; otherwise the Responses
                // encoder completes before it has seen the stashed usage.
                parts.extend(pending_finishes);
            }
            Some("error") => {
                let error = &chunk["error"];
                let code = error["code"].as_str().or_else(|| error["type"].as_str());
                let upstream_code = code.map(String::from);
                let class = tiygate_core::classify_upstream_error(None, code);
                parts.push(StreamPart::Error {
                    message: error["message"]
                        .as_str()
                        .unwrap_or("Unknown error")
                        .to_string(),
                    class,
                    upstream_code,
                });
            }
            Some(_other) => {
                // Unknown / vendor-specific object types (keepalive pings,
                // experimental chunk shapes, etc.) must NOT abort the stream.
                // Per the capability matrix's UnknownFieldPolicy::Drop, we
                // silently ignore them instead of injecting an error frame.
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
                annotations: None,
            },
            Some("image_url") => {
                // Accept both the standard object form
                // `{"image_url": {"url": "...", "detail": "..."}}` and the
                // legacy string form `{"image_url": "data:..."}`.
                let (raw_url, detail) = if let Some(s) = item["image_url"].as_str() {
                    (s, None)
                } else {
                    (
                        item["image_url"]["url"].as_str().unwrap_or(""),
                        item["image_url"]["detail"].as_str(),
                    )
                };
                let (source, mime_type) =
                    tiygate_core::ir::MediaSource::from_data_url(raw_url, "image/*");
                let mut metadata = std::collections::HashMap::<String, serde_json::Value>::new();
                if let Some(d) = detail {
                    metadata.insert(
                        tiygate_core::ir::IMAGE_DETAIL_KEY.to_string(),
                        serde_json::Value::String(d.to_string()),
                    );
                }
                Content::Media {
                    source,
                    mime_type,
                    metadata,
                }
            }
            Some("tool_use") | Some("tool_result") => {
                if *role == Role::Tool {
                    Content::ToolResult {
                        tool_call_id: item["tool_call_id"].as_str().unwrap_or("").to_string(),
                        name: String::new(),
                        content: item["content"].as_str().unwrap_or("").to_string(),
                        id: None,
                    }
                } else {
                    Content::Text {
                        text: item["content"].as_str().unwrap_or("").to_string(),
                        annotations: None,
                    }
                }
            }
            _ => Content::Text {
                text: item["text"].as_str().unwrap_or("").to_string(),
                annotations: None,
            },
        });
    }
    parts
}

/// Parse OpenAI-style annotations array into IR annotations.
fn parse_openai_annotations(annotations: &Value) -> Option<Vec<tiygate_core::Annotation>> {
    let arr = annotations.as_array()?;
    let result: Vec<tiygate_core::Annotation> = arr
        .iter()
        .filter_map(|a| {
            let type_str = a["type"].as_str()?;
            let kind = match type_str {
                "url_citation" => tiygate_core::AnnotationKind::UrlCitation,
                "file_citation" => tiygate_core::AnnotationKind::FileCitation,
                _ => return None,
            };
            Some(tiygate_core::Annotation {
                kind,
                start_index: a["start_index"].as_u64().map(|v| v as u32),
                end_index: a["end_index"].as_u64().map(|v| v as u32),
                title: a["url_citation"]["title"]
                    .as_str()
                    .or_else(|| a["file_citation"]["filename"].as_str())
                    .map(String::from),
                url: a["url_citation"]["url"].as_str().map(String::from),
            })
        })
        .collect();
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

inventory::submit! {
    tiygate_core::CodecRegistration {
        make: || Box::new(ChatCompletionsCodec::new()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn make_env() -> RawEnvelope {
        RawEnvelope {
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            headers: std::collections::HashMap::new(),
            body: None,
            original_body_size: 0,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_extra_passthrough_and_include_usage() {
        // 中低影响回归:extra 字段(parallel_tool_calls/n/user 等)往返无损,
        // 且 stream=true 时注入 stream_options.include_usage。
        let codec = ChatCompletionsCodec::new();
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "parallel_tool_calls": true,
            "n": 2,
            "user": "u1",
            "logprobs": true,
        });
        let ir = codec.decode_request(body, &make_env()).unwrap();
        let (out, _h) = codec.encode_request(&ir).unwrap();
        assert_eq!(out["parallel_tool_calls"], true);
        assert_eq!(out["n"], 2);
        assert_eq!(out["user"], "u1");
        assert_eq!(out["logprobs"], true);
        assert_eq!(out["stream_options"]["include_usage"], true);
    }

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
    fn test_decode_tool_result_message() {
        // Regression: an OpenAI `role:tool` message carries the result as a
        // top-level string `content` with `tool_call_id` as a sibling field.
        // It must decode to Content::ToolResult (not Content::Text), otherwise
        // re-encoding to Anthropic/Gemini drops the matching tool_result block
        // and the upstream rejects with 400 invalid_params.
        let codec = ChatCompletionsCodec::new();
        let env = make_raw_envelope();
        let req = json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "List files."},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_abc",
                    "type": "function",
                    "function": {"name": "list", "arguments": "{\"path\":\".\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_abc", "name": "list", "content": "{\"ok\":true}"}
            ]
        });
        let ir = codec.decode_request(req, &env).unwrap();
        // messages: user, assistant(tool_call), tool(tool_result)
        let tool_msg = ir
            .messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("tool message present");
        match &tool_msg.content[0] {
            Content::ToolResult {
                tool_call_id,
                content,
                ..
            } => {
                assert_eq!(tool_call_id, "call_abc");
                assert_eq!(content, "{\"ok\":true}");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn test_decode_assistant_text_with_tool_calls() {
        // Regression: an OpenAI assistant message may carry BOTH a string
        // `content` and `tool_calls`. Both must survive into the IR, otherwise
        // the re-encoded Anthropic request omits the `tool_use` block and a
        // later `tool_result` references an unknown id (upstream 400).
        let codec = ChatCompletionsCodec::new();
        let env = make_raw_envelope();
        let req = json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "List files."},
                {"role": "assistant", "content": "Let me check.", "tool_calls": [{
                    "id": "call_xyz",
                    "type": "function",
                    "function": {"name": "list", "arguments": "{\"path\":\".\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_xyz", "content": "{\"ok\":true}"}
            ]
        });
        let ir = codec.decode_request(req, &env).unwrap();
        let asst = ir
            .messages
            .iter()
            .find(|m| m.role == Role::Assistant)
            .expect("assistant message present");
        // Expect both the text and the tool call, in order.
        assert_eq!(asst.content.len(), 2);
        assert!(matches!(&asst.content[0], Content::Text { text , ..} if text == "Let me check."));
        match &asst.content[1] {
            Content::ToolCall { id, name, .. } => {
                assert_eq!(id, "call_xyz");
                assert_eq!(name, "list");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn test_decode_assistant_null_content_with_tool_calls() {
        // assistant with null content + tool_calls should yield ONLY the tool
        // call (no empty text block).
        let codec = ChatCompletionsCodec::new();
        let env = make_raw_envelope();
        let req = json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "List."},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "list", "arguments": "{}"}
                }]}
            ]
        });
        let ir = codec.decode_request(req, &env).unwrap();
        let asst = ir
            .messages
            .iter()
            .find(|m| m.role == Role::Assistant)
            .expect("assistant message present");
        assert_eq!(asst.content.len(), 1);
        assert!(matches!(&asst.content[0], Content::ToolCall { id, .. } if id == "call_1"));
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
                annotations: None,
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
                call_id: None,
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
        let err_bytes =
            encoder.encode_error("rate limit exceeded", ErrorClass::RateLimited, Some("429"));
        let err_str = String::from_utf8_lossy(&err_bytes);
        // Must contain "error" — protocol-native error frame
        assert!(err_str.contains("error"));
        assert!(err_str.contains("rate limit exceeded"));
        // type must be the OpenAI-native rate_limit_error, not gateway_error
        assert!(err_str.contains("\"type\":\"rate_limit_error\""));
        assert!(!err_str.contains("gateway_error"));
        // upstream code is transparently passed through to error.code
        assert!(err_str.contains("\"code\":\"429\""));
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
                id: None,
                encrypted_content: None,
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
                class: ErrorClass::Transient,
                upstream_code: Some("500".to_string()),
            },
            StreamPart::ResponseCompleted {
                id: "r1".to_string(),
                status: "completed".to_string(),
                usage: None,
                extensions: std::collections::HashMap::new(),
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
        assert!(!parts.is_empty());
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
    fn test_stream_decoder_parallel_tool_calls() {
        // 致命项2 回归:两个并行 tool_calls 交替流式,各自 arg 增量必须
        // 绑定到正确的 id(按 index 跟踪)。
        let mut decoder = ChatCompletionsStreamDecoder::new();
        // index 0 opener
        decoder
            .feed("data: {\"id\":\"r1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"function\":{\"name\":\"fa\",\"arguments\":\"\"}}]}}]}\n")
            .unwrap();
        // index 1 opener
        decoder
            .feed("data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"function\":{\"name\":\"fb\",\"arguments\":\"\"}}]}}]}\n")
            .unwrap();
        // interleaved arg fragments (no id, only index)
        let p0 = decoder
            .feed("data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"x\\\":1}\"}}]}}]}\n")
            .unwrap();
        let p1 = decoder
            .feed("data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"{\\\"y\\\":2}\"}}]}}]}\n")
            .unwrap();
        let arg0 = p0.iter().find_map(|p| match p {
            StreamPart::ToolCallDelta {
                id,
                name: None,
                arguments,
            } => Some((id.clone(), arguments.clone())),
            _ => None,
        });
        let arg1 = p1.iter().find_map(|p| match p {
            StreamPart::ToolCallDelta {
                id,
                name: None,
                arguments,
            } => Some((id.clone(), arguments.clone())),
            _ => None,
        });
        assert_eq!(arg0, Some(("call_a".to_string(), "{\"x\":1}".to_string())));
        assert_eq!(arg1, Some(("call_b".to_string(), "{\"y\":2}".to_string())));
    }

    #[test]
    fn test_stream_decoder_tool_call_empty_name_in_arg_deltas() {
        // 回归:部分 OpenAI 兼容上游(如 GLM)在每个参数 delta 中都带
        // `name: ""`(空字符串)而非省略 name 字段。解码器必须将空字符串
        // 视为"无 name"(argument fragment),否则每个 delta 都被当作
        // opener,导致跨协议编码器为每个碎片创建一个独立的 tool_use block。
        let mut decoder = ChatCompletionsStreamDecoder::new();

        // 第一个 delta: name="read", arguments="{\""
        let p1 = decoder
            .feed(r#"data: {"id":"r1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_x","function":{"name":"read","arguments":"{\""}}]}}]}"#)
            .unwrap();

        // 第二个 delta: name=""(空), arguments="\"path\""
        let p2 = decoder
            .feed(r#"data: {"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_x","function":{"name":"","arguments":"\"path\""}}]}}]}"#)
            .unwrap();

        // 第一个 delta 应产生 opener(name=Some) + arg fragment(name=None)
        let openers1: Vec<_> = p1
            .iter()
            .filter_map(|p| match p {
                StreamPart::ToolCallDelta { name: Some(n), .. } => Some(n.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            openers1,
            vec!["read".to_string()],
            "第一个 delta 应有一个 opener(name=read)"
        );

        // 第二个 delta 不应产生任何 opener
        let openers2: Vec<_> = p2
            .iter()
            .filter_map(|p| match p {
                StreamPart::ToolCallDelta { name: Some(n), .. } => Some(n.clone()),
                _ => None,
            })
            .collect();
        assert!(
            openers2.is_empty(),
            "name=\"\" 的 delta 不应产生 opener,实际产生了: {:?}",
            openers2
        );

        // 第二个 delta 应产生一个 argument fragment
        let args2: Vec<_> = p2
            .iter()
            .filter_map(|p| match p {
                StreamPart::ToolCallDelta {
                    name: None,
                    arguments,
                    ..
                } => Some(arguments.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            args2,
            vec!["\"path\"".to_string()],
            "第二个 delta 应产生一个 argument fragment"
        );
    }

    #[test]
    fn test_stream_decoder_error_frame() {
        let mut decoder = ChatCompletionsStreamDecoder::new();
        let line = "data: {\"error\":{\"message\":\"rate limit\",\"code\":\"429\"}}\n";
        let parts = decoder.feed(line).unwrap();
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            StreamPart::Error {
                message,
                class,
                upstream_code,
            } => {
                assert!(message.contains("rate limit"));
                assert_eq!(*class, ErrorClass::RateLimited);
                assert_eq!(upstream_code.as_deref(), Some("429"));
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
                annotations: None,
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

    /// DeepSeek thinking-with-tools 门控:
    /// - 含 tool_calls 的 assistant 轮 → reasoning_content 必须回传。
    /// - 纯文本(无 tool_calls)多轮 → reasoning_content 必须丢弃,否则 400。
    #[test]
    fn test_encode_request_reasoning_content_gated_by_tool_calls() {
        use tiygate_core::{GenerationParams, IrRequest, Message};

        let codec = ChatCompletionsCodec::new();
        let ir = IrRequest {
            model: "deepseek-reasoner".to_string(),
            system: None,
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "天气?".to_string(),
                        annotations: None,
                    }],
                },
                // 含 tool_calls 的轮:reasoning_content 应回传
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Content::Reasoning {
                            text: "先查天气工具".to_string(),
                            signature: None,
                            id: None,
                            encrypted_content: None,
                        },
                        Content::ToolCall {
                            id: "call_1".to_string(),
                            name: "get_weather".to_string(),
                            arguments: json!({"city": "杭州"}),
                            call_id: None,
                        },
                    ],
                },
                Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult {
                        tool_call_id: "call_1".to_string(),
                        name: "get_weather".to_string(),
                        content: "sunny".to_string(),
                        id: None,
                    }],
                },
                // 纯文本轮:reasoning_content 应丢弃
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Content::Reasoning {
                            text: "整理答案".to_string(),
                            signature: None,
                            id: None,
                            encrypted_content: None,
                        },
                        Content::Text {
                            text: "杭州今天晴。".to_string(),
                            annotations: None,
                        },
                    ],
                },
            ],
            tools: vec![],
            params: GenerationParams::default(),
            response_format: None,
            stream: false,
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::OpenAiCompatible,
                "chat-completions",
                "v1",
            ),
            metadata: None,
            extensions: Default::default(),
        };

        let (body, _h) = codec.encode_request(&ir).unwrap();
        let msgs = body["messages"].as_array().unwrap();

        // 工具调用轮:带 tool_calls + reasoning_content
        let tool_turn = msgs
            .iter()
            .find(|m| m.get("tool_calls").is_some())
            .expect("tool-call assistant turn present");
        assert_eq!(
            tool_turn["reasoning_content"], "先查天气工具",
            "含 tool_calls 的轮必须回传 reasoning_content"
        );

        // 纯文本轮:有正文但绝不带 reasoning_content
        let text_turn = msgs
            .iter()
            .find(|m| m["content"] == "杭州今天晴。")
            .expect("plain-text assistant turn present");
        assert!(
            text_turn.get("reasoning_content").is_none(),
            "纯文本多轮不得回传 reasoning_content(否则 DeepSeek 400)"
        );
    }

    /// DeepSeek v4-pro 的流式 SSE 使用 `reasoning` 字段名(而非 `reasoning_content`)。
    /// 流式解码器必须同时识别两种字段名,否则 reasoning delta 被丢弃,
    /// 导致下一轮缺少 reasoning_content 被 DeepSeek 400 拒绝。
    #[test]
    fn test_stream_decoder_reasoning_field_name_variants() {
        // 1. reasoning_content (DeepSeek R1 / 标准)
        let mut dec = ChatCompletionsStreamDecoder::new();
        let parts = dec
            .feed(r#"data: {"id":"r1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"reasoning_content":"thinking deeply"}}]}"#)
            .unwrap();
        let rc_text: String = parts
            .iter()
            .filter_map(|p| match p {
                StreamPart::ReasoningDelta { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            rc_text, "thinking deeply",
            "reasoning_content 字段必须被解码为 ReasoningDelta"
        );

        // 2. reasoning (DeepSeek v4-pro)
        let mut dec2 = ChatCompletionsStreamDecoder::new();
        let parts2 = dec2
            .feed(r#"data: {"id":"r1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"reasoning":"let me analyze"}}]}"#)
            .unwrap();
        let r_text: String = parts2
            .iter()
            .filter_map(|p| match p {
                StreamPart::ReasoningDelta { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            r_text, "let me analyze",
            "reasoning 字段(DeepSeek v4-pro)必须被解码为 ReasoningDelta"
        );

        // 3. 空 reasoning 不应产生 delta
        let mut dec3 = ChatCompletionsStreamDecoder::new();
        let parts3 = dec3
            .feed(r#"data: {"id":"r1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"reasoning":""}}]}"#)
            .unwrap();
        let empty_text: String = parts3
            .iter()
            .filter_map(|p| match p {
                StreamPart::ReasoningDelta { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            empty_text.is_empty(),
            "空 reasoning 不应产生 ReasoningDelta"
        );

        // 4. reasoning_content 优先于 reasoning(同时存在时)
        let mut dec4 = ChatCompletionsStreamDecoder::new();
        let parts4 = dec4
            .feed(r#"data: {"id":"r1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"reasoning_content":"primary","reasoning":"fallback"}}]}"#)
            .unwrap();
        let prio_text: String = parts4
            .iter()
            .filter_map(|p| match p {
                StreamPart::ReasoningDelta { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(prio_text, "primary", "reasoning_content 应优先于 reasoning");
    }

    /// 回归:`"usage": null` 出现在非最终 chunk 时,解码器不得产出
    /// `StreamPart::Usage`。OpenAI 兼容流在 `include_usage: true` 时每个
    /// 中间 chunk 都带 `usage: null`,若不守卫则生成全零 Usage,在跨协议
    /// 转码(chat → responses)时提前触发 `response.completed` 并丢弃后续
    /// 真实 usage。
    #[test]
    fn test_stream_decoder_null_usage_does_not_emit_usage_part() {
        let mut dec = ChatCompletionsStreamDecoder::new();

        // 1. 内容 chunk 带 `"usage": null` — 不应产出 Usage
        let parts = dec
            .feed(r#"data: {"id":"r1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"hi"}}],"usage":null}"#)
            .unwrap();
        let has_usage = parts.iter().any(|p| matches!(p, StreamPart::Usage { .. }));
        assert!(!has_usage, "null usage must not produce StreamPart::Usage");

        // 2. finish chunk 带 `"usage": null` — 仍不应产出 Usage
        let parts = dec
            .feed(r#"data: {"id":"r1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":null}"#)
            .unwrap();
        let has_usage = parts.iter().any(|p| matches!(p, StreamPart::Usage { .. }));
        assert!(
            !has_usage,
            "null usage on finish chunk must not produce StreamPart::Usage"
        );

        // 3. 真实 usage chunk — 必须产出 Usage 且值正确
        let parts = dec
            .feed(r#"data: {"id":"r1","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":77200,"completion_tokens":28,"total_tokens":77228,"prompt_tokens_details":{"cached_tokens":76937}}}"#)
            .unwrap();
        let usage_part = parts.iter().find_map(|p| match p {
            StreamPart::Usage { usage } => Some(usage),
            _ => None,
        });
        let usage = usage_part.expect("real usage chunk must produce StreamPart::Usage");
        assert_eq!(usage.prompt_tokens, 263); // 77200 - 76937
        assert_eq!(usage.completion_tokens, 28);
        assert_eq!(usage.total_tokens, 77228);
        assert_eq!(usage.cache_read_tokens, Some(76937));
    }

    /// 非流式 `decode_response` 对 `"usage": null` 应返回 `None`,
    /// 而非全零 `Some(Usage::default())`。
    #[test]
    fn test_decode_response_null_usage_returns_none() {
        let codec = ChatCompletionsCodec::new();
        let body = serde_json::json!({
            "id": "r1",
            "object": "chat.completion",
            "model": "test-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
            "usage": null
        });
        let ir = codec.decode_response(body).unwrap();
        assert!(
            ir.usage.is_none(),
            "null usage must decode to None, not a zero-valued Usage"
        );
    }
}
