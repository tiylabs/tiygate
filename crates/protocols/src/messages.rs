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

/// Flatten an Anthropic `tool_result.content` value into a plain string.
///
/// Anthropic accepts either a bare string OR an array of content blocks
/// (e.g. `[{"type":"text","text":"..."}]`). The previous implementation only
/// handled the string form and silently dropped array payloads. We now also
/// concatenate the `text` of any text blocks so multi-turn tool results
/// survive the round-trip.
fn flatten_anthropic_content(value: &Value) -> String {
    if let Some(s) = value.as_str() {
        return s.to_string();
    }
    if let Some(arr) = value.as_array() {
        let mut out = String::new();
        for block in arr {
            if let Some(text) = block["text"].as_str() {
                out.push_str(text);
            } else if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                // text block missing the field — skip gracefully
            } else if !block.is_null() {
                // Non-text block (image, etc.): preserve its JSON so the
                // information is not silently discarded.
                out.push_str(&block.to_string());
            }
        }
        return out;
    }
    String::new()
}

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
                // Anthropic supports tool_choice={type:"any"} (equivalent to
                // OpenAI's "required") but NOT openai-style concurrent fan-out.
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
                                    annotations: None,
                                });
                            }
                            Some("thinking") => {
                                // Anthropic extended thinking. Preserve the
                                // reasoning text so multi-turn replays keep the
                                // chain of thought.
                                parts.push(Content::Reasoning {
                                    text: block["thinking"].as_str().unwrap_or("").to_string(),
                                    signature: block["signature"].as_str().map(|s| s.to_string()),
                                    id: None,
                                    encrypted_content: None,
                                });
                            }
                            Some("redacted_thinking") => {
                                // Redacted thinking carries encrypted `data`
                                // rather than plain text; keep it as reasoning
                                // so the block survives the round-trip.
                                parts.push(Content::Reasoning {
                                    text: block["data"].as_str().unwrap_or("").to_string(),
                                    signature: None,
                                    id: None,
                                    encrypted_content: None,
                                });
                            }
                            Some("tool_use") => {
                                parts.push(Content::ToolCall {
                                    id: block["id"].as_str().unwrap_or("").to_string(),
                                    name: block["name"].as_str().unwrap_or("").to_string(),
                                    // Anthropic tool_use `input` defaults to an
                                    // empty object, never null, so downstream
                                    // re-encoding produces valid `{}` args.
                                    arguments: if block["input"].is_null() {
                                        json!({})
                                    } else {
                                        block["input"].clone()
                                    },
                                    call_id: None,
                                });
                            }
                            Some("tool_result") => {
                                parts.push(Content::ToolResult {
                                    tool_call_id: block["tool_use_id"]
                                        .as_str()
                                        .unwrap_or("")
                                        .to_string(),
                                    name: String::new(),
                                    content: flatten_anthropic_content(&block["content"]),
                                    id: None,
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
                        annotations: None,
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
            thinking: body.get("thinking").and_then(|t| {
                // Parse effort from the top-level output_config field
                // (output_config.effort). Per the Anthropic API schema,
                // output_config is a sibling of thinking, not a child.
                // We also check the legacy nested path
                // (thinking.output_config.effort) for backward compat.
                let effort = body
                    .get("output_config")
                    .and_then(|oc| oc.get("effort"))
                    .or_else(|| t.get("output_config").and_then(|oc| oc.get("effort")))
                    .and_then(|v| v.as_str())
                    .map(|s| {
                        use tiygate_core::ThinkingEffort;
                        match s {
                            "minimal" => ThinkingEffort::Minimal,
                            "low" => ThinkingEffort::Low,
                            "medium" => ThinkingEffort::Medium,
                            "high" => ThinkingEffort::High,
                            "xhigh" => ThinkingEffort::XHigh,
                            "max" => ThinkingEffort::Max,
                            _ => ThinkingEffort::High,
                        }
                    });
                let budget_tokens = t["budget_tokens"].as_u64().map(|v| v as u32);
                let display = t["display"].as_str().map(|s| match s {
                    "summarized" => tiygate_core::ThinkingDisplay::Summarized,
                    "omitted" => tiygate_core::ThinkingDisplay::Omitted,
                    _ => tiygate_core::ThinkingDisplay::Summarized,
                });
                // Derive include_thoughts from display for cross-protocol
                // consistency (Gemini's includeThoughts is the semantic
                // equivalent of Anthropic's display).
                let include_thoughts = display.map(|d| match d {
                    tiygate_core::ThinkingDisplay::Summarized => true,
                    tiygate_core::ThinkingDisplay::Omitted => false,
                });
                if effort.is_none() && budget_tokens.is_none() && display.is_none() {
                    None
                } else {
                    Some(tiygate_core::ThinkingConfig {
                        effort,
                        budget_tokens,
                        display,
                        include_thoughts,
                        summary: None,
                    })
                }
            }),
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
            metadata: body.get("metadata").and_then(|m| {
                m.get("user_id").and_then(|v| v.as_str()).map(|s| {
                    let mut map = std::collections::HashMap::new();
                    map.insert("user_id".to_string(), s.to_string());
                    map
                })
            }),
            extensions: {
                let mut ext = std::collections::HashMap::new();
                // Parse Anthropic native tool_choice into normalized extensions
                // format so cross-protocol targets can interpret it.
                if let Some(tc) = body.get("tool_choice") {
                    if let Some(tc_type) = tc["type"].as_str() {
                        let normalized = match tc_type {
                            "auto" => json!("auto"),
                            "any" => json!("required"),
                            "none" => json!("none"),
                            "tool" => {
                                let name = tc["name"].as_str().unwrap_or("");
                                json!({"type": "function", "function": {"name": name}})
                            }
                            _ => tc.clone(),
                        };
                        ext.insert("tool_choice".to_string(), normalized);
                    }
                }
                ext
            },
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
                Content::Text { text, .. } => {
                    content_blocks.push(json!({
                        "type": "text",
                        "text": text,
                    }));
                }
                Content::Reasoning {
                    text,
                    signature,
                    encrypted_content,
                    ..
                } => {
                    if let Some(encrypted) = encrypted_content {
                        // Redacted thinking: emit as redacted_thinking block
                        // with the opaque encrypted data.
                        content_blocks.push(json!({
                            "type": "redacted_thinking",
                            "data": encrypted,
                        }));
                    } else {
                        let mut block = json!({
                            "type": "thinking",
                            "thinking": text,
                        });
                        if let Some(sig) = signature {
                            block["signature"] = json!(sig);
                        }
                        content_blocks.push(block);
                    }
                }
                Content::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
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
            if let Some(seq) = &sd.stop_sequence {
                response["stop_sequence"] = json!(seq);
            }
            // Rebuild the structured `stop_details` object (refusals carry
            // type/category/explanation) when any of those fields are present.
            if sd.kind.is_some() || sd.category.is_some() || sd.explanation.is_some() {
                let mut details = json!({});
                if let Some(k) = &sd.kind {
                    details["type"] = json!(k);
                }
                if let Some(c) = &sd.category {
                    details["category"] = json!(c);
                }
                if let Some(e) = &sd.explanation {
                    details["explanation"] = json!(e);
                }
                response["stop_details"] = details;
            }
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
            // Messages 协议要求 max_tokens 必填；上游未提供时填充默认值 64k
            "max_tokens": ir.params.max_tokens.unwrap_or(65536),
        });

        // 写缓存（prompt caching）注入开关。
        //
        // Anthropic Messages 协议是「主动写缓存」：调用方需要在
        // `tools` / `system` / `messages` 的内容块上显式打 `cache_control`
        // 断点，上游才会写入 5min ephemeral 缓存。其它协议
        // (OpenAI-Compatible / Responses / Gemini) 没有这个概念，
        // 跨协议转换到 Messages 时缓存断点会丢失，导致无法命中缓存。
        //
        // 因此当 ingress 协议不是 AnthropicMessages 时（即发生了跨协议
        // 转换），按 Anthropic 缓存前缀层级 `tools → system → messages`
        // 自动注入 ephemeral 断点。当 ingress 本身就是 Messages 时，
        // 保持原样，尊重调用方自己的 cache_control 语义（同协议路径通常
        // 还会走 PassThrough 原样透传）。
        let inject_cache = ir.ingress_protocol.suite != ProtocolSuite::AnthropicMessages;

        // System prompt
        if let Some(sys) = &ir.system {
            // 空文本块不可缓存；仅在非空时注入断点。
            if inject_cache && !sys.is_empty() {
                body["system"] = json!([{
                    "type": "text",
                    "text": sys,
                    "cache_control": { "type": "ephemeral" },
                }]);
            } else {
                body["system"] = json!(sys);
            }
        }

        // Messages
        //
        // Anthropic requires messages to strictly alternate user/assistant and
        // forbids consecutive same-role messages. The IR uses four roles
        // (System/User/Assistant/Tool); Anthropic has only `user`/`assistant`
        // on the wire (System lives in the top-level `system` field, Tool
        // results are carried inside a `user` message). Mapping Tool/System to
        // `user` can therefore produce consecutive `user` messages (e.g.
        // user → assistant(tool_use) → tool(result) → tool(result)), which
        // Anthropic rejects with a 400. To stay valid we merge adjacent
        // messages that map to the same wire role by concatenating their
        // content blocks.
        fn wire_role(role: Role) -> &'static str {
            match role {
                Role::Assistant => "assistant",
                // user / tool / system → user on the wire
                _ => "user",
            }
        }

        fn blocks_for(msg: &Message) -> Vec<Value> {
            msg.content
                .iter()
                .filter_map(|c| match c {
                    Content::Text { text, .. } => Some(json!({"type": "text", "text": text})),
                    // Preserve reasoning as an Anthropic thinking block ONLY
                    // when it carries the provider's `signature`. Anthropic
                    // rejects thinking blocks without a valid signature (400
                    // `thinking.signature: Field required`), so reasoning that
                    // originated from another protocol (OpenAI/Gemini) — which
                    // has no Anthropic signature — is dropped on the request
                    // side rather than replayed.
                    Content::Reasoning {
                        text, signature, ..
                    } => signature
                        .as_ref()
                        .map(|sig| json!({"type": "thinking", "thinking": text, "signature": sig})),
                    Content::ToolCall {
                        id,
                        name,
                        arguments,
                        ..
                    } => Some(
                        json!({"type": "tool_use", "id": id, "name": name, "input": arguments}),
                    ),
                    Content::ToolResult {
                        tool_call_id,
                        name: _,
                        content,
                        ..
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
                            "source": {"type": "url", "url": url, "media_type": mime_type}
                        })),
                        tiygate_core::ir::MediaSource::Inline { data } => Some(json!({
                            "type": "image",
                            "source": {"type": "base64", "media_type": mime_type, "data": data}
                        })),
                        _ => None,
                    },
                    Content::Refusal { text, .. } => Some(json!({"type": "text", "text": text})),
                })
                .collect()
        }

        // First, fold the IR messages into merged (wire_role, blocks) groups.
        let mut merged: Vec<(&'static str, Vec<Value>)> = Vec::new();
        for msg in &ir.messages {
            let role = wire_role(msg.role);
            let mut blocks = blocks_for(msg);
            if blocks.is_empty() {
                continue;
            }
            match merged.last_mut() {
                Some((last_role, last_blocks)) if *last_role == role => {
                    last_blocks.append(&mut blocks);
                }
                _ => merged.push((role, blocks)),
            }
        }

        let merged_count = merged.len();
        let messages: Vec<Value> = merged
            .into_iter()
            .enumerate()
            .map(|(idx, (role, mut blocks))| {
                // 会话级缓存断点：在最后一条消息的最后一个内容块上打
                // ephemeral 断点，使整段 messages 前缀（含 tools、system）
                // 随对话增长被增量缓存。thinking 块不可直接缓存，但此处
                // 转换出的块均为可缓存类型（text / tool_use / tool_result /
                // image），故安全。
                if inject_cache && idx + 1 == merged_count {
                    if let Some(last) = blocks.last_mut() {
                        if let Some(obj) = last.as_object_mut() {
                            obj.insert("cache_control".to_string(), json!({ "type": "ephemeral" }));
                        }
                    }
                }
                json!({ "role": role, "content": blocks })
            })
            .collect();

        body["messages"] = json!(messages);

        // Tools
        if !ir.tools.is_empty() {
            let tool_count = ir.tools.len();
            let tools: Vec<Value> = ir
                .tools
                .iter()
                .enumerate()
                .map(|(idx, t)| {
                    let mut tool = json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    });
                    // 在最后一个 tool 上打断点即可缓存整段 tools 前缀
                    // （Anthropic 缓存层级中 tools 是第一级）。
                    if inject_cache && idx + 1 == tool_count {
                        tool["cache_control"] = json!({ "type": "ephemeral" });
                    }
                    tool
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
        // Thinking config: output Anthropic thinking block from params.thinking.
        //
        // Cross-protocol derivation:
        // - effort → adaptive thinking with top-level output_config.effort (new mechanism)
        // - budget_tokens (without effort) → enabled thinking with budget_tokens
        // - display ← include_thoughts (derived when display is missing)
        //
        // When effort is present (e.g. from OpenAI reasoning_effort), use the
        // adaptive thinking type so the effort level is expressed natively on
        // Anthropic. When only budget_tokens is present (e.g. same-protocol
        // round-trip), keep the traditional enabled type.
        if let Some(ref thinking) = ir.params.thinking {
            // Derive display from include_thoughts when display is not set.
            let display = thinking.display.or_else(|| {
                thinking.include_thoughts.map(|i| match i {
                    true => tiygate_core::ThinkingDisplay::Summarized,
                    false => tiygate_core::ThinkingDisplay::Omitted,
                })
            });

            if let Some(effort) = thinking.effort {
                // Effort-based adaptive thinking (new Anthropic mechanism).
                // Anthropic supports low/medium/high/xhigh/max; Minimal clamps
                // to "low" since Anthropic has no "minimal" effort level.
                //
                // Per the Anthropic API schema, `output_config` is a
                // top-level request body field (sibling of `thinking`),
                // NOT a nested child of `thinking`. The `thinking` object
                // only carries `type` and optional `display`.
                let mut t = json!({
                    "type": "adaptive",
                });
                if let Some(d) = display {
                    t["display"] = json!(match d {
                        tiygate_core::ThinkingDisplay::Summarized => "summarized",
                        tiygate_core::ThinkingDisplay::Omitted => "omitted",
                    });
                }
                body["thinking"] = t;
                body["output_config"] = json!({
                    "effort": match effort {
                        tiygate_core::ThinkingEffort::Minimal => "low",
                        tiygate_core::ThinkingEffort::Low => "low",
                        tiygate_core::ThinkingEffort::Medium => "medium",
                        tiygate_core::ThinkingEffort::High => "high",
                        tiygate_core::ThinkingEffort::XHigh => "xhigh",
                        tiygate_core::ThinkingEffort::Max => "max",
                    }
                });
            } else if let Some(budget) = thinking.budget_tokens {
                // Budget-based enabled thinking (traditional mechanism).
                // Anthropic's enabled type requires budget_tokens; when only
                // display/include_thoughts is set without a budget, we skip
                // emitting the block rather than sending an invalid request.
                let mut t = json!({"type": "enabled", "budget_tokens": budget});
                if let Some(d) = display {
                    t["display"] = json!(match d {
                        tiygate_core::ThinkingDisplay::Summarized => "summarized",
                        tiygate_core::ThinkingDisplay::Omitted => "omitted",
                    });
                }
                body["thinking"] = t;
            }
        }
        // tool_choice: convert normalized extensions format to Anthropic native
        if let Some(tc) = ir.extensions.get("tool_choice") {
            let anthropic_tc = if let Some(s) = tc.as_str() {
                match s {
                    "auto" => json!({"type": "auto"}),
                    "required" => json!({"type": "any"}),
                    "none" => json!({"type": "none"}),
                    _ => json!({"type": "auto"}),
                }
            } else if let Some(obj) = tc.as_object() {
                // Object form: {"type": "function", "function": {"name": "x"}}
                if obj.get("type").and_then(|v| v.as_str()) == Some("function") {
                    let name = obj["function"]["name"].as_str().unwrap_or("");
                    json!({"type": "tool", "name": name})
                } else {
                    tc.clone()
                }
            } else {
                json!({"type": "auto"})
            };
            body["tool_choice"] = anthropic_tc;
        }
        // Metadata: Anthropic only supports user_id
        if let Some(ref metadata) = ir.metadata {
            if let Some(user_id) = metadata.get("user_id") {
                body["metadata"] = json!({"user_id": user_id});
            }
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
        let extensions = std::collections::HashMap::new();

        if let Some(arr) = body["content"].as_array() {
            for block in arr {
                match block["type"].as_str() {
                    Some("text") => {
                        content.push(Content::Text {
                            text: block["text"].as_str().unwrap_or("").to_string(),
                            annotations: None,
                        });
                    }
                    Some("thinking") => {
                        content.push(Content::Reasoning {
                            text: block["thinking"].as_str().unwrap_or("").to_string(),
                            signature: block["signature"].as_str().map(|s| s.to_string()),
                            id: None,
                            encrypted_content: None,
                        });
                    }
                    // Anthropic may return `redacted_thinking` blocks when portions
                    // of thinking are safety-redacted. These carry an opaque `data`
                    // field and must be preserved/echoed in multi-turn conversations
                    // to avoid 400 errors.
                    // https://platform.claude.com/docs/en/build-with-claude/extended-thinking
                    Some("redacted_thinking") => {
                        let encrypted = block["data"].as_str().map(|s| s.to_string());
                        content.push(Content::Reasoning {
                            text: String::new(),
                            signature: None,
                            id: None,
                            encrypted_content: encrypted,
                        });
                    }
                    Some("tool_use") => {
                        content.push(Content::ToolCall {
                            id: block["id"].as_str().unwrap_or("").to_string(),
                            name: block["name"].as_str().unwrap_or("").to_string(),
                            arguments: block["input"].clone(),
                            call_id: None,
                        });
                    }
                    _ => {}
                }
            }
        }

        // redacted_thinking data is now stored in Content::Reasoning.encrypted_content
        // (migrated from the previous extensions["anthropic_redacted_thinking"] approach).

        let finish_reason = body["stop_reason"].as_str().map(|s| match s {
            "end_turn" => FinishReason::Stop,
            "stop_sequence" => FinishReason::Stop,
            "max_tokens" => FinishReason::Length,
            "tool_use" => FinishReason::ToolCalls,
            "content_filter" | "refusal" => FinishReason::ContentFilter,
            "pause_turn" => FinishReason::Stop,
            other => FinishReason::Other(other.to_string()),
        });

        let usage = body.get("usage").map(|u| {
            let input = u["input_tokens"].as_u64().unwrap_or(0);
            let output = u["output_tokens"].as_u64().unwrap_or(0);
            let cache_creation = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            let cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
            // Anthropic 协议无 total_tokens；按官方 spec 派生：
            //   total = input + cache_creation + cache_read + output
            // 优先用上游响应里若带的 total_tokens（部分 SDK/代理会注入）
            let total = u["total_tokens"]
                .as_u64()
                .unwrap_or(input + cache_creation + cache_read + output);
            let reasoning = u["output_tokens_details"]["thinking_tokens"].as_u64();
            let has_cache_creation_field = u.get("cache_creation_input_tokens").is_some();
            let has_cache_read_field = u.get("cache_read_input_tokens").is_some();
            Usage {
                prompt_tokens: input,
                completion_tokens: output,
                total_tokens: total,
                reasoning_tokens: reasoning,
                cache_read_tokens: if has_cache_read_field {
                    Some(cache_read)
                } else {
                    None
                },
                cache_write_tokens: if has_cache_creation_field {
                    Some(cache_creation)
                } else {
                    None
                },
            }
        });

        let stop_details = body["stop_reason"].as_str().map(|s| {
            // Anthropic may emit a structured `stop_details` object (e.g. for
            // refusals) carrying `type`/`category`/`explanation`. Capture all
            // of it so refusal metadata survives the round-trip. The top-level
            // `stop_sequence` (when stop_reason == "stop_sequence") is also
            // preserved.
            let sd = &body["stop_details"];
            tiygate_core::ir::StopDetails {
                stop_reason: s.to_string(),
                stop_sequence: body["stop_sequence"].as_str().map(String::from),
                kind: sd["type"].as_str().map(String::from),
                category: sd["category"].as_str().map(String::from),
                explanation: sd["explanation"].as_str().map(String::from),
            }
        });

        Ok(IrResponse {
            content,
            usage,
            finish_reason,
            response_id,
            stop_details,
            extensions,
        })
    }

    fn stream_decoder(&self) -> Box<dyn StreamDecoder> {
        Box::new(MessagesStreamDecoder::new())
    }

    fn pass_through_policy(
        &self,
        ingress: &tiygate_core::ProtocolEndpoint,
        egress: &tiygate_core::ProtocolEndpoint,
    ) -> tiygate_core::PassThroughPolicy {
        if ingress.suite == egress.suite {
            tiygate_core::PassThroughPolicy::Passthrough
        } else {
            tiygate_core::PassThroughPolicy::Convert
        }
    }
}

// --- Stream Encoder (Anthropic SSE) ---

pub struct MessagesStreamEncoder {
    message_started: bool,
    /// Index of the currently-open content block, if any.
    current_index: Option<usize>,
    /// Type of the currently-open block ("text" / "thinking" / "tool_use").
    current_kind: Option<&'static str>,
    /// Next content-block index to allocate.
    next_index: usize,
    /// Last seen usage from an upstream `StreamPart::Usage`. The Anthropic
    /// streaming protocol carries the final usage on the `message_delta`
    /// event that also sets `stop_reason`. When `Usage` arrives before
    /// `Finish`, the `Finish` handler reuses this usage instead of overwriting
    /// it with `{output_tokens: 0}`. When `Finish` arrives before `Usage`
    /// (Gemini can decode `finishReason` before same-frame `usageMetadata`),
    /// the stop reason is deferred until usage arrives so the terminal delta
    /// still carries real cache/read token counts.
    last_usage: Option<Usage>,
    pending_stop_reason: Option<&'static str>,
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
            current_index: None,
            current_kind: None,
            next_index: 0,
            last_usage: None,
            pending_stop_reason: None,
        }
    }

    /// Emit a `content_block_stop` for the open block, if any.
    fn close_block(&mut self) -> String {
        if let Some(idx) = self.current_index.take() {
            self.current_kind = None;
            let data = json!({"type": "content_block_stop", "index": idx});
            format!(
                "event: content_block_stop\ndata: {}\n\n",
                serde_json::to_string(&data).unwrap_or_default()
            )
        } else {
            String::new()
        }
    }

    /// Ensure a block of `kind` is open, closing a mismatched block and
    /// opening a new one (with `content_block_start`) as needed. Returns any
    /// SSE bytes that must be emitted before the caller's delta.
    fn ensure_block(&mut self, kind: &'static str, content_block: Value) -> String {
        if self.current_kind == Some(kind) {
            return String::new();
        }
        let mut out = self.close_block();
        out.push_str(&self.open_block(kind, content_block));
        out
    }

    /// Unconditionally open a new content block of `kind` at the next index,
    /// emitting its `content_block_start`. The caller is responsible for
    /// closing any previously-open block first. Used by the tool_use opener so
    /// two consecutive parallel tool calls each get their own block.
    fn open_block(&mut self, kind: &'static str, content_block: Value) -> String {
        let idx = self.next_index;
        self.next_index += 1;
        self.current_index = Some(idx);
        self.current_kind = Some(kind);
        let data = json!({
            "type": "content_block_start",
            "index": idx,
            "content_block": content_block,
        });
        format!(
            "event: content_block_start\ndata: {}\n\n",
            serde_json::to_string(&data).unwrap_or_default()
        )
    }
    fn usage_delta(&self, usage: &Usage, stop_reason: Option<&str>) -> String {
        let mut usage_obj = json!({"output_tokens": usage.completion_tokens});
        if usage.prompt_tokens > 0 {
            usage_obj["input_tokens"] = json!(usage.prompt_tokens);
        }
        if let Some(cw) = usage.cache_write_tokens {
            usage_obj["cache_creation_input_tokens"] = json!(cw);
        }
        if let Some(cr) = usage.cache_read_tokens {
            usage_obj["cache_read_input_tokens"] = json!(cr);
        }
        let data = json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": usage_obj,
        });
        format!(
            "event: message_delta\ndata: {}\n\n",
            serde_json::to_string(&data).unwrap_or_default()
        )
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
                // Open (or keep) a text block at the correct index, then emit
                // the delta against that same index.
                let mut out = self.ensure_block("text", json!({"type": "text", "text": ""}));
                let idx = self.current_index.unwrap_or(0);
                let data = json!({
                    "type": "content_block_delta",
                    "index": idx,
                    "delta": {"type": "text_delta", "text": text},
                });
                out.push_str(&format!(
                    "event: content_block_delta\ndata: {}\n\n",
                    serde_json::to_string(&data).unwrap_or_default()
                ));
                out
            }
            StreamPart::ReasoningDelta { text, .. } => {
                let mut out =
                    self.ensure_block("thinking", json!({"type": "thinking", "thinking": ""}));
                let idx = self.current_index.unwrap_or(0);
                let data = json!({
                    "type": "content_block_delta",
                    "index": idx,
                    "delta": {"type": "thinking_delta", "thinking": text},
                });
                out.push_str(&format!(
                    "event: content_block_delta\ndata: {}\n\n",
                    serde_json::to_string(&data).unwrap_or_default()
                ));
                out
            }
            StreamPart::ToolCallDelta {
                id,
                name,
                arguments,
            } => {
                // The opener carries `name`: always close any prior block and
                // open a FRESH tool_use block. Two consecutive openers (e.g.
                // parallel tool calls) both have kind "tool_use", so we cannot
                // rely on `ensure_block`'s same-kind short-circuit — that would
                // merge two distinct calls into one block. Force the
                // close+open here. Argument-only fragments (`name == None`)
                // append to the currently-open tool_use block.
                if let Some(n) = name {
                    let mut out = self.close_block();
                    out.push_str(&self.open_block(
                        "tool_use",
                        json!({"type": "tool_use", "id": id, "name": n, "input": {}}),
                    ));
                    if !arguments.is_empty() {
                        let idx = self.current_index.unwrap_or(0);
                        let data = json!({
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": {"type": "input_json_delta", "partial_json": arguments},
                        });
                        out.push_str(&format!(
                            "event: content_block_delta\ndata: {}\n\n",
                            serde_json::to_string(&data).unwrap_or_default()
                        ));
                    }
                    out
                } else {
                    let idx = self.current_index.unwrap_or(0);
                    let data = json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": {"type": "input_json_delta", "partial_json": arguments},
                    });
                    format!(
                        "event: content_block_delta\ndata: {}\n\n",
                        serde_json::to_string(&data).unwrap_or_default()
                    )
                }
            }
            StreamPart::Usage { usage } => {
                self.last_usage = Some(usage.clone());
                let stop_reason = self.pending_stop_reason.take();
                self.usage_delta(usage, stop_reason)
            }
            StreamPart::Finish { reason } => {
                let stop_reason = match reason {
                    FinishReason::Stop => "end_turn",
                    FinishReason::Length => "max_tokens",
                    FinishReason::ToolCalls => "tool_use",
                    _ => "end_turn",
                };
                // Close any open content block before signalling the stop
                // reason, per the Anthropic streaming contract.
                let mut out = self.close_block();
                if let Some(u) = &self.last_usage {
                    out.push_str(&self.usage_delta(u, Some(stop_reason)));
                } else {
                    // Defer the terminal message_delta until Usage arrives so
                    // Gemini/other sources that emit Finish before Usage do not
                    // produce a zero-token terminal delta that hides cache reads.
                    self.pending_stop_reason = Some(stop_reason);
                }
                out
            }
            StreamPart::ResponseCompleted { .. } => {
                let mut out = self.close_block();
                if let Some(stop_reason) = self.pending_stop_reason.take() {
                    let data = json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                        "usage": {"output_tokens": 0},
                    });
                    out.push_str(&format!(
                        "event: message_delta\ndata: {}\n\n",
                        serde_json::to_string(&data).unwrap_or_default()
                    ));
                }
                let data = json!({"type": "message_stop"});
                out.push_str(&format!(
                    "event: message_stop\ndata: {}\n\n",
                    serde_json::to_string(&data).unwrap_or_default()
                ));
                out
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
    /// Per-content-block tool_use id, indexed by Anthropic's `index` field.
    /// Anthropic streams parallel `tool_use` blocks distinguished by `index`;
    /// `input_json_delta` events carry only the `index`, so we must look up
    /// the id by index to bind argument fragments to the right call. A single
    /// `Option<String>` mis-attributed fragments when more than one tool_use
    /// block streamed.
    tool_use_ids: Vec<String>,
    /// Whether a `message_delta.stop_reason` produced a `Finish` in-band. Used
    /// to synthesize a `Finish(Stop)` on `message_stop` when an older
    /// Anthropic version / proxy omits the `message_delta` stop_reason.
    saw_finish: bool,
    /// Whether any `tool_use` content block appeared during this message.
    /// Latches for the whole stream so the bare-`message_stop` fallback can be
    /// mapped to `FinishReason::ToolCalls` instead of `Stop` — otherwise a
    /// truncated tool-call turn would make the client stop instead of running
    /// the tool.
    saw_tool_use: bool,
    /// Accumulated usage from `message_start` plus later `message_delta`.
    /// Anthropic streams usually split input/cache on message_start and final
    /// output on message_delta; emitting each delta independently would make a
    /// downstream encoder (Gemini/Chat/Responses) see a later zero-input usage
    /// that hides cache reads.
    usage_acc: Option<Usage>,
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
            tool_use_ids: Vec::new(),
            saw_finish: false,
            saw_tool_use: false,
            usage_acc: None,
        }
    }

    /// Record the tool_use id for a content block `index`.
    fn set_tool_use_id(&mut self, index: usize, id: String) {
        if self.tool_use_ids.len() <= index {
            self.tool_use_ids.resize_with(index + 1, String::new);
        }
        self.tool_use_ids[index] = id;
    }

    /// Look up the tool_use id previously recorded for a content block `index`.
    fn tool_use_id(&self, index: usize) -> String {
        self.tool_use_ids.get(index).cloned().unwrap_or_default()
    }

    fn merge_usage(&mut self, incoming: Usage) -> Usage {
        let mut merged = self.usage_acc.clone().unwrap_or_default();
        if incoming.prompt_tokens > 0 || merged.prompt_tokens == 0 {
            merged.prompt_tokens = incoming.prompt_tokens;
        }
        if incoming.completion_tokens > 0 || merged.completion_tokens == 0 {
            merged.completion_tokens = incoming.completion_tokens;
        }
        if incoming.reasoning_tokens.is_some() {
            merged.reasoning_tokens = incoming.reasoning_tokens;
        }
        if incoming.cache_read_tokens.is_some() {
            merged.cache_read_tokens = incoming.cache_read_tokens;
        }
        if incoming.cache_write_tokens.is_some() {
            merged.cache_write_tokens = incoming.cache_write_tokens;
        }
        let cache_read = merged.cache_read_tokens.unwrap_or(0);
        let cache_write = merged.cache_write_tokens.unwrap_or(0);
        merged.total_tokens =
            merged.prompt_tokens + cache_write + cache_read + merged.completion_tokens;
        self.usage_acc = Some(merged.clone());
        merged
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
                // Anthropic 流式在 message_start 给一次完整 usage
                if let Some(u) = event["message"]["usage"].as_object() {
                    let input = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    let output = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                    let cache_creation = u
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_read = u
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let has_cc_field = u.get("cache_creation_input_tokens").is_some();
                    let has_cr_field = u.get("cache_read_input_tokens").is_some();
                    let reasoning = u
                        .get("output_tokens_details")
                        .and_then(|v| v.get("thinking_tokens"))
                        .and_then(|v| v.as_u64());
                    let usage = self.merge_usage(Usage {
                        prompt_tokens: input,
                        completion_tokens: output,
                        total_tokens: input + cache_creation + cache_read + output,
                        reasoning_tokens: reasoning,
                        cache_read_tokens: if has_cr_field { Some(cache_read) } else { None },
                        cache_write_tokens: if has_cc_field {
                            Some(cache_creation)
                        } else {
                            None
                        },
                    });
                    parts.push(StreamPart::Usage { usage });
                }
            }
            Some("content_block_start") => {
                let block = &event["content_block"];
                let index = event["index"].as_u64().unwrap_or(0) as usize;
                self.current_block_type = block["type"].as_str().map(String::from);
                match block["type"].as_str() {
                    Some("tool_use") => {
                        self.saw_tool_use = true;
                        let id = block["id"].as_str().unwrap_or("").to_string();
                        let name = block["name"].as_str().map(String::from);
                        self.set_tool_use_id(index, id.clone());
                        parts.push(StreamPart::ToolCallDelta {
                            id,
                            name,
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
                                id: None,
                                encrypted_content: None,
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
                let index = event["index"].as_u64().unwrap_or(0) as usize;
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
                                id: None,
                                encrypted_content: None,
                            });
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(json) = delta["partial_json"].as_str() {
                            // Argument fragment: emit `name: None` so
                            // cross-protocol encoders that key on the opener
                            // (name present) vs. fragments (name absent) route
                            // this to their argument-delta event rather than
                            // re-opening the tool-call block. Bind the fragment
                            // to the id recorded for this block index so
                            // parallel tool_use blocks stay separated.
                            parts.push(StreamPart::ToolCallDelta {
                                id: self.tool_use_id(index),
                                name: None,
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
                    let output_tokens = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let input_tokens = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_creation = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let cache_read = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let has_cc_field = usage.get("cache_creation_input_tokens").is_some();
                    let has_cr_field = usage.get("cache_read_input_tokens").is_some();
                    let reasoning = usage
                        .get("output_tokens_details")
                        .and_then(|v| v.get("thinking_tokens"))
                        .and_then(|v| v.as_u64());
                    let usage = self.merge_usage(Usage {
                        prompt_tokens: input_tokens,
                        completion_tokens: output_tokens,
                        total_tokens: input_tokens + cache_creation + cache_read + output_tokens,
                        reasoning_tokens: reasoning,
                        cache_read_tokens: if has_cr_field { Some(cache_read) } else { None },
                        cache_write_tokens: if has_cc_field {
                            Some(cache_creation)
                        } else {
                            None
                        },
                    });
                    parts.push(StreamPart::Usage { usage });
                }
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    let fr = match reason {
                        "end_turn" => FinishReason::Stop,
                        // Matches the non-streaming `decode` mapping, which
                        // treats a stop-sequence hit as a clean stop.
                        "stop_sequence" => FinishReason::Stop,
                        "max_tokens" => FinishReason::Length,
                        "tool_use" => FinishReason::ToolCalls,
                        other => FinishReason::Other(other.to_string()),
                    };
                    parts.push(StreamPart::Finish { reason: fr });
                    self.saw_finish = true;
                }
            }
            Some("message_stop") => {
                // Fallback: some older Anthropic versions / proxies end with
                // `message_stop` without a preceding `message_delta.stop_reason`
                // (notably on truncation). Without a `Finish` the IR carries no
                // finish_reason. Synthesize a terminal reason — prefer
                // `ToolCalls` when a `tool_use` block was seen so a truncated
                // tool-call turn does not become a client-side `stop`.
                if !self.saw_finish {
                    parts.push(StreamPart::Finish {
                        reason: if self.saw_tool_use {
                            FinishReason::ToolCalls
                        } else {
                            FinishReason::Stop
                        },
                    });
                    self.saw_finish = true;
                }
                parts.push(StreamPart::ResponseCompleted {
                    id: self.response_id.clone().unwrap_or_default(),
                    status: "completed".to_string(),
                    usage: None,
                    extensions: std::collections::HashMap::new(),
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
            Some(_other) => {
                // Anthropic emits keepalive `ping` events and may introduce
                // new event types over time. These must NOT abort the stream;
                // ignore unrecognized events per UnknownFieldPolicy::Drop.
            }
            None => {
                // SSE comment/keepalive lines or events without a `type`
                // field are ignored rather than treated as fatal errors.
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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

    /// 跨协议（OpenAI-Compatible 等非 Anthropic ingress）转换到 Messages 时，
    /// 应按 `tools → system → messages` 层级注入 5min ephemeral 写缓存断点。
    #[test]
    fn test_encode_request_injects_cache_control_for_cross_protocol() {
        use tiygate_core::{GenerationParams, Message};

        let codec = MessagesCodec::new();
        let ir = IrRequest {
            model: "claude-sonnet-4".to_string(),
            system: Some("You are helpful.".to_string()),
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "Hello".to_string(),
                        annotations: None,
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![Content::Text {
                        text: "Hi!".to_string(),
                        annotations: None,
                    }],
                },
                Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "How are you?".to_string(),
                        annotations: None,
                    }],
                },
            ],
            tools: vec![
                tiygate_core::Tool {
                    name: "t1".to_string(),
                    description: Some("first".to_string()),
                    parameters: Some(json!({"type": "object"})),
                    required: false,
                },
                tiygate_core::Tool {
                    name: "t2".to_string(),
                    description: Some("second".to_string()),
                    parameters: Some(json!({"type": "object"})),
                    required: false,
                },
            ],
            params: GenerationParams {
                max_tokens: Some(100),
                ..Default::default()
            },
            response_format: None,
            stream: false,
            // 非 Anthropic ingress → 触发注入
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::OpenAiCompatible,
                "chat-completions",
                "v1",
            ),
            metadata: None,
            extensions: Default::default(),
        };

        let (body, _headers) = codec.encode_request(&ir).unwrap();

        // system 转为带断点的文本块数组
        assert_eq!(
            body["system"][0]["cache_control"]["type"], "ephemeral",
            "system 应注入 ephemeral 断点"
        );
        assert_eq!(body["system"][0]["text"], "You are helpful.");

        // 仅最后一个 tool 打断点
        let tools = body["tools"].as_array().unwrap();
        assert!(
            tools[0].get("cache_control").is_none(),
            "非最后 tool 不应有断点"
        );
        assert_eq!(
            tools[1]["cache_control"]["type"], "ephemeral",
            "最后一个 tool 应注入 ephemeral 断点"
        );

        // 仅最后一条消息的最后一个块打断点
        let messages = body["messages"].as_array().unwrap();
        assert!(
            messages[0]["content"][0].get("cache_control").is_none(),
            "非最后消息不应有断点"
        );
        let last = messages.last().unwrap();
        let last_block = last["content"].as_array().unwrap().last().unwrap();
        assert_eq!(
            last_block["cache_control"]["type"], "ephemeral",
            "最后消息的最后块应注入 ephemeral 断点"
        );
    }

    /// 同协议（Anthropic Messages ingress）不应注入断点，尊重调用方原始语义。
    #[test]
    fn test_encode_request_no_cache_control_for_same_protocol() {
        use tiygate_core::{GenerationParams, Message};

        let codec = MessagesCodec::new();
        let ir = IrRequest {
            model: "claude-sonnet-4".to_string(),
            system: Some("You are helpful.".to_string()),
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "Hello".to_string(),
                    annotations: None,
                }],
            }],
            tools: vec![],
            params: GenerationParams {
                max_tokens: Some(100),
                ..Default::default()
            },
            response_format: None,
            stream: false,
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::AnthropicMessages,
                "messages",
                "2023-06-01",
            ),
            metadata: None,
            extensions: Default::default(),
        };

        let (body, _headers) = codec.encode_request(&ir).unwrap();

        // system 维持纯字符串，无断点
        assert!(body["system"].is_string(), "同协议 system 应保持纯字符串");
        let last_block = body["messages"][0]["content"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        assert!(
            last_block.get("cache_control").is_none(),
            "同协议不应注入断点"
        );
    }

    #[test]
    fn test_encode_response_non_streaming() {
        let codec = MessagesCodec::new();
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
            response_id: Some("msg_1".to_string()),
            stop_details: Some(tiygate_core::ir::StopDetails {
                stop_reason: "end_turn".to_string(),
                ..Default::default()
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
                call_id: None,
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
                code: Some("500".to_string()),
            },
            StreamPart::ResponseCompleted {
                id: "r1".to_string(),
                status: "completed".to_string(),
                usage: None,
                extensions: std::collections::HashMap::new(),
            },
        ];
        for variant in variants {
            assert!(encoder.encode_part(variant).is_ok());
        }
    }

    #[test]
    fn test_stream_encoder_finish_preserves_usage() {
        // 回归:OpenAI 兼容上游常将 usage 和 finish_reason 放在同一个 chunk。
        // 解码器先发 StreamPart::Usage,再发 StreamPart::Finish。
        // MessagesStreamEncoder 的 Finish handler 之前硬编码
        // `usage: {output_tokens: 0}`,覆盖了 Usage handler 刚发出的真实
        // token 计数。客户端取最后一个 message_delta 的 usage,得到 0。
        // 修复后 Finish 应复用上次看到的 usage。
        let mut encoder = MessagesStreamEncoder::new();

        // Usage 先到
        let usage_bytes = encoder
            .encode_part(&StreamPart::Usage {
                usage: Usage {
                    prompt_tokens: 28,
                    completion_tokens: 45,
                    total_tokens: 13385,
                    cache_read_tokens: Some(13312),
                    ..Default::default()
                },
            })
            .unwrap();
        let usage_str = String::from_utf8_lossy(&usage_bytes);
        assert!(
            usage_str.contains("\"output_tokens\":45"),
            "Usage event should carry output_tokens=45, got: {}",
            usage_str
        );

        // Finish 后到 — 不应覆盖 usage 为 0
        let finish_bytes = encoder
            .encode_part(&StreamPart::Finish {
                reason: FinishReason::ToolCalls,
            })
            .unwrap();
        let finish_str = String::from_utf8_lossy(&finish_bytes);
        assert!(
            finish_str.contains("\"output_tokens\":45"),
            "Finish event must preserve output_tokens=45, got: {}",
            finish_str
        );
        assert!(
            finish_str.contains("\"stop_reason\":\"tool_use\""),
            "Finish event must carry stop_reason=tool_use, got: {}",
            finish_str
        );
        assert!(
            finish_str.contains("\"cache_read_input_tokens\":13312"),
            "Finish event must preserve cache_read_input_tokens=13312, got: {}",
            finish_str
        );
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
    fn test_encode_request_merges_consecutive_user_roles() {
        // 致命项3 回归:跨协议产生的 user→assistant(tool_use)→tool→tool
        // 序列必须合并为严格交替的 user/assistant,消除连续 user。
        let codec = MessagesCodec::new();
        let ir = IrRequest {
            model: "claude".to_string(),
            system: None,
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "hi".to_string(),
                        annotations: None,
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![Content::ToolCall {
                        id: "t1".to_string(),
                        name: "f".to_string(),
                        arguments: json!({}),
                        call_id: None,
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult {
                        tool_call_id: "t1".to_string(),
                        name: "f".to_string(),
                        content: "r1".to_string(),
                        id: None,
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult {
                        tool_call_id: "t2".to_string(),
                        name: "f".to_string(),
                        content: "r2".to_string(),
                        id: None,
                    }],
                },
            ],
            tools: vec![],
            params: Default::default(),
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
        // user, assistant, user(merged two tool results)
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[2]["role"], "user");
        // No two consecutive wire roles are equal.
        for w in msgs.windows(2) {
            assert_ne!(
                w[0]["role"], w[1]["role"],
                "连续同 wire-role 违反 Anthropic"
            );
        }
        // The merged user message carries both tool_result blocks.
        let last_blocks = msgs[2]["content"].as_array().unwrap();
        assert_eq!(last_blocks.len(), 2);
        assert_eq!(last_blocks[0]["type"], "tool_result");
        assert_eq!(last_blocks[1]["type"], "tool_result");
    }

    #[test]
    fn test_stream_decoder_parallel_tool_use() {
        // 致命项4 回归:两个并行 tool_use block 的 input_json_delta 必须
        // 按 index 绑定到正确 id。
        let mut dec = MessagesStreamDecoder::new();
        dec.feed("data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_a\",\"name\":\"fa\"}}\n").unwrap();
        dec.feed("data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_b\",\"name\":\"fb\"}}\n").unwrap();
        let p0 = dec.feed("data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"x\\\":1}\"}}\n").unwrap();
        let p1 = dec.feed("data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"y\\\":2}\"}}\n").unwrap();
        let a = p0.iter().find_map(|p| match p {
            StreamPart::ToolCallDelta {
                id,
                name: None,
                arguments,
            } => Some((id.clone(), arguments.clone())),
            _ => None,
        });
        let b = p1.iter().find_map(|p| match p {
            StreamPart::ToolCallDelta {
                id,
                name: None,
                arguments,
            } => Some((id.clone(), arguments.clone())),
            _ => None,
        });
        assert_eq!(a, Some(("tu_a".to_string(), "{\"x\":1}".to_string())));
        assert_eq!(b, Some(("tu_b".to_string(), "{\"y\":2}".to_string())));
    }

    #[test]
    fn test_stream_encoder_parallel_tool_use_separate_blocks() {
        // 流式 encoder:两个相邻 tool_use opener 必须生成不同 index 的块,
        // 不能合并。
        let mut enc = MessagesStreamEncoder::new();
        let mut all = String::new();
        for part in [
            StreamPart::ToolCallDelta {
                id: "a".to_string(),
                name: Some("fa".to_string()),
                arguments: String::new(),
            },
            StreamPart::ToolCallDelta {
                id: "b".to_string(),
                name: Some("fb".to_string()),
                arguments: String::new(),
            },
        ] {
            all.push_str(&String::from_utf8(enc.encode_part(&part).unwrap()).unwrap());
        }
        // Two distinct content_block_start with index 0 and 1, and a
        // content_block_stop between them.
        assert!(all.contains("\"index\":0"));
        assert!(all.contains("\"index\":1"));
        assert!(all.contains("content_block_stop"));
    }

    #[test]
    fn test_stream_encoder_tool_call_opener_preserves_arguments() {
        let mut enc = MessagesStreamEncoder::new();
        let part = StreamPart::ToolCallDelta {
            id: "gemini_call_shell".to_string(),
            name: Some("shell".to_string()),
            arguments: r#"{"command":"git status"}"#.to_string(),
        };
        let out = String::from_utf8(enc.encode_part(&part).unwrap()).unwrap();
        assert!(out.contains("content_block_start"));
        assert!(out.contains("\"type\":\"tool_use\""));
        assert!(out.contains("\"name\":\"shell\""));
        assert!(out.contains("content_block_delta"));
        assert!(out.contains("input_json_delta"));
        let delta = out
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .find(|event| event["type"] == "content_block_delta")
            .expect("content_block_delta");
        assert_eq!(
            delta["delta"]["partial_json"],
            r#"{"command":"git status"}"#
        );
    }

    #[test]
    fn test_stream_encoder_parallel_tool_use_preserves_opener_arguments() {
        let mut enc = MessagesStreamEncoder::new();
        let mut all = String::new();
        for part in [
            StreamPart::ToolCallDelta {
                id: "a".to_string(),
                name: Some("fa".to_string()),
                arguments: r#"{"x":1}"#.to_string(),
            },
            StreamPart::ToolCallDelta {
                id: "b".to_string(),
                name: Some("fb".to_string()),
                arguments: r#"{"y":2}"#.to_string(),
            },
        ] {
            all.push_str(&String::from_utf8(enc.encode_part(&part).unwrap()).unwrap());
        }
        let deltas: Vec<Value> = all
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|data| serde_json::from_str::<Value>(data).ok())
            .filter(|event| event["type"] == "content_block_delta")
            .collect();
        assert_eq!(deltas.len(), 2, "expected two argument deltas: {all}");
        assert_eq!(deltas[0]["index"], 0);
        assert_eq!(deltas[0]["delta"]["partial_json"], r#"{"x":1}"#);
        assert_eq!(deltas[1]["index"], 1);
        assert_eq!(deltas[1]["delta"]["partial_json"], r#"{"y":2}"#);
    }

    #[test]
    fn test_stop_details_refusal_roundtrip() {
        // 高影响回归:Anthropic refusal 的 stop_details(type/category/explanation)
        // 必须 decode 进 IR 再 encode 回去无损。
        let codec = MessagesCodec::new();
        let body = json!({
            "id": "msg_x",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "I can't help with that."}],
            "stop_reason": "refusal",
            "stop_details": {"type": "refusal", "category": "harmful", "explanation": "policy"},
            "usage": {"input_tokens": 5, "output_tokens": 3}
        });
        let ir = codec.decode_response(body).unwrap();
        let sd = ir.stop_details.as_ref().unwrap();
        assert_eq!(sd.stop_reason, "refusal");
        assert_eq!(sd.kind.as_deref(), Some("refusal"));
        assert_eq!(sd.category.as_deref(), Some("harmful"));
        assert_eq!(sd.explanation.as_deref(), Some("policy"));
        // Re-encode and confirm the structured object is rebuilt.
        let encoded = codec.encode_response(&ir).unwrap();
        assert_eq!(encoded["stop_reason"], "refusal");
        assert_eq!(encoded["stop_details"]["type"], "refusal");
        assert_eq!(encoded["stop_details"]["category"], "harmful");
        assert_eq!(encoded["stop_details"]["explanation"], "policy");
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

    #[test]
    fn test_encode_response_with_cache() {
        // Anthropic decode cache 字段后 encode 回原协议，验证 cache_* 与 input_tokens 都写入
        let codec = MessagesCodec::new();
        let body = json!({
            "id": "msg",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_creation_input_tokens": 100,
                "cache_read_input_tokens": 2000
            }
        });
        let ir = codec.decode_response(body).unwrap();
        // IR 必须完整保留 cache_*
        let u = ir.usage.as_ref().unwrap();
        assert_eq!(u.cache_read_tokens, Some(2000));
        assert_eq!(u.cache_write_tokens, Some(100));
        // total 派生公式：10 + 100 + 2000 + 5 = 2115
        assert_eq!(u.total_tokens, 2115);
        // encode 回协议体
        let encoded = codec.encode_response(&ir).unwrap();
        assert_eq!(encoded["usage"]["input_tokens"], 10);
        assert_eq!(encoded["usage"]["output_tokens"], 5);
        assert_eq!(encoded["usage"]["cache_read_input_tokens"], 2000);
        assert_eq!(encoded["usage"]["cache_creation_input_tokens"], 100);
    }

    /// 跨协议(OpenAI-Compatible ingress)历史里的 reasoning 没有 Anthropic
    /// signature,encode_request 时必须丢弃 thinking 块,否则 Anthropic 返回
    /// 400 `thinking.signature: Field required`。带 signature 的 reasoning 则
    /// 必须原样回传(Anthropic↔Anthropic 多轮思维链)。
    #[test]
    fn test_encode_request_thinking_signature_gating() {
        use tiygate_core::{GenerationParams, Message};

        let codec = MessagesCodec::new();
        let ir = IrRequest {
            model: "claude-opus-4-8".to_string(),
            system: None,
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: "hi".to_string(),
                        annotations: None,
                    }],
                },
                // reasoning 无 signature(来自 OpenAI 历史)→ 应被丢弃
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Content::Reasoning {
                            text: "unsigned thoughts".to_string(),
                            signature: None,
                            id: None,
                            encrypted_content: None,
                        },
                        Content::Text {
                            text: "answer A".to_string(),
                            annotations: None,
                        },
                    ],
                },
                // reasoning 带 signature(来自 Anthropic 历史)→ 应原样回传
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Content::Reasoning {
                            text: "signed thoughts".to_string(),
                            signature: Some("sig_xyz".to_string()),
                            id: None,
                            encrypted_content: None,
                        },
                        Content::Text {
                            text: "answer B".to_string(),
                            annotations: None,
                        },
                    ],
                },
            ],
            tools: vec![],
            params: GenerationParams {
                max_tokens: Some(100),
                ..Default::default()
            },
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

        let (body, _headers) = codec.encode_request(&ir).unwrap();
        let messages = body["messages"].as_array().unwrap();

        // 收集所有 thinking 块。
        let thinking_blocks: Vec<serde_json::Value> = messages
            .iter()
            .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
            .filter(|b| b["type"] == "thinking")
            .collect();

        // 无 signature 的 thinking 被丢弃,只剩带 signature 的那个。
        assert_eq!(
            thinking_blocks.len(),
            1,
            "只有带 signature 的 thinking 块应被保留"
        );
        assert_eq!(thinking_blocks[0]["thinking"], "signed thoughts");
        assert_eq!(thinking_blocks[0]["signature"], "sig_xyz");

        // text 块完整保留(reasoning 丢弃不影响正文)。
        let all_text: Vec<String> = messages
            .iter()
            .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str().map(|s| s.to_string()))
            .collect();
        assert!(all_text.iter().any(|t| t == "answer A"));
        assert!(all_text.iter().any(|t| t == "answer B"));
    }

    /// decode_request 必须把 Anthropic thinking 块的 `signature` 提取进 IR,
    /// 且 round-trip(decode → encode_request)后 signature 不丢失。
    #[test]
    fn test_decode_request_preserves_thinking_signature() {
        let codec = MessagesCodec::new();
        let env = make_raw_envelope();
        let body = json!({
            "model": "claude-opus-4-8",
            "max_tokens": 100,
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "deep", "signature": "sig_abc"},
                        {"type": "text", "text": "done"}
                    ]
                }
            ]
        });
        let ir = codec.decode_request(body, &env).unwrap();
        let has_signed = ir.messages.iter().any(|m| {
            m.content.iter().any(
                |c| matches!(c, Content::Reasoning { signature: Some(s), .. } if s == "sig_abc"),
            )
        });
        assert!(has_signed, "thinking signature 应被解析进 IR");

        // round-trip 回 Anthropic 请求体,signature 仍在。
        let (re, _h) = codec.encode_request(&ir).unwrap();
        let thinking = re["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
            .find(|b| b["type"] == "thinking")
            .expect("thinking block should survive round-trip");
        assert_eq!(thinking["signature"], "sig_abc");
    }
}
