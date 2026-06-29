//! OpenAI Responses API protocol codec.
//! Implements bidirectional conversion for OpenAI's Responses API.

use http::HeaderMap;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};

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

fn responses_call_id(item: &Value) -> Option<&str> {
    item["call_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| item["id"].as_str().filter(|s| !s.is_empty()))
}

fn unique_responses_call_id(
    raw_id: &str,
    occurrence: usize,
    used_ids: &mut HashSet<String>,
) -> String {
    let base = if raw_id.is_empty() {
        format!("call_tiygate_{occurrence}")
    } else if occurrence == 0 {
        raw_id.to_string()
    } else {
        format!("{raw_id}_{occurrence}")
    };

    let mut candidate = base.clone();
    let mut collision = 1usize;
    while used_ids.contains(&candidate) {
        candidate = format!("{base}_{collision}");
        collision += 1;
    }
    used_ids.insert(candidate.clone());
    candidate
}

fn responses_function_call_output(
    tool_call_id: &str,
    content: &str,
    item_id: Option<&str>,
) -> Value {
    let mut v = json!({"type": "function_call_output", "call_id": tool_call_id, "output": content});
    if let Some(id) = item_id {
        v["id"] = json!(id);
    }
    v
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
        let mut messages: Vec<Message> = Vec::new();
        let mut codex_opaque_items: Vec<Value> = Vec::new();

        if let Some(arr) = body["input"].as_array() {
            let mut call_id_counts: HashMap<String, usize> = HashMap::new();
            let mut call_id_remap: HashMap<String, VecDeque<String>> = HashMap::new();
            let mut used_call_ids: HashSet<String> = HashSet::new();

            for item in arr {
                // Responses typed items (function_call, function_call_output,
                // reasoning) do NOT carry a `role` field — their semantic role
                // is implied by the item type. Determine role from `type` first
                // so these items map to the correct IR roles for cross-protocol
                // conversion (e.g. function_call → Assistant, which Anthropic
                // requires for tool_use blocks).
                let role = match item["type"].as_str() {
                    Some("function_call")
                    | Some("reasoning")
                    | Some("local_shell_call")
                    | Some("custom_tool_call") => Role::Assistant,
                    Some("function_call_output")
                    | Some("local_shell_call_output")
                    | Some("custom_tool_call_output") => Role::Tool,
                    _ => match item["role"].as_str().unwrap_or("user") {
                        "system" | "developer" => Role::System,
                        "user" => Role::User,
                        "assistant" => Role::Assistant,
                        "tool" => Role::Tool,
                        _ => Role::User,
                    },
                };
                let content = if matches!(
                    item["type"].as_str(),
                    Some("tool_search_call")
                        | Some("tool_search_output")
                        | Some("agent_message")
                        | Some("compaction")
                        | Some("compaction_trigger")
                        | Some("context_compaction")
                ) {
                    // Known Codex opaque item types: preserve the raw JSON
                    // for same-protocol replay. Cross-protocol egress drops
                    // these silently (no lossy rejection). Must be checked
                    // BEFORE the content-based branches because some opaque
                    // items (e.g. agent_message) carry a `content` field.
                    codex_opaque_items.push(item.clone());
                    vec![]
                } else if let Some(text) = item["content"].as_str() {
                    vec![Content::Text {
                        text: text.to_string(),
                        annotations: None,
                    }]
                } else if let Some(content_arr) = item["content"].as_array() {
                    let mut parts = Vec::new();
                    for part in content_arr {
                        match part["type"].as_str() {
                            Some("input_text") | Some("output_text") => {
                                parts.push(Content::Text {
                                    text: part["text"].as_str().unwrap_or("").to_string(),
                                    annotations: None,
                                });
                            }
                            Some("input_image") => {
                                // Accept both the string form
                                // `{"image_url": "data:..."}` and the object
                                // form `{"image_url": {"url": "...",
                                // "detail": "..."}}`.
                                let (raw_url, detail) = if let Some(s) = part["image_url"].as_str()
                                {
                                    (s, None)
                                } else if let Some(s) = part["image_url"]["url"].as_str() {
                                    (s, part["image_url"]["detail"].as_str())
                                } else {
                                    ("", None)
                                };
                                if !raw_url.is_empty() {
                                    let (source, mime_type) =
                                        tiygate_core::ir::MediaSource::from_data_url(
                                            raw_url, "image/*",
                                        );
                                    let mut metadata = std::collections::HashMap::<
                                        String,
                                        serde_json::Value,
                                    >::new();
                                    if let Some(d) = detail {
                                        metadata.insert(
                                            tiygate_core::ir::IMAGE_DETAIL_KEY.to_string(),
                                            serde_json::Value::String(d.to_string()),
                                        );
                                    }
                                    parts.push(Content::Media {
                                        source,
                                        mime_type,
                                        metadata,
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                    parts
                } else if item["type"] == "function_call" {
                    // Responses uses `call_id`; fall back to `id` for proxies
                    // that only emit the item id. Some clients emit duplicated
                    // ids (including `call_e3b0...`, a hash of empty input).
                    // Downstream protocols such as Anthropic require unique
                    // `tool_use.id` values, so normalize duplicates while
                    // recording an occurrence-ordered remap for following
                    // `function_call_output` items.
                    let raw_id = responses_call_id(item).unwrap_or("");
                    let occurrence = call_id_counts.entry(raw_id.to_string()).or_insert(0);
                    let id = unique_responses_call_id(raw_id, *occurrence, &mut used_call_ids);
                    *occurrence += 1;
                    call_id_remap
                        .entry(raw_id.to_string())
                        .or_default()
                        .push_back(id.clone());

                    // Preserve the Responses item `id` (item reference) when
                    // it differs from `call_id`. The IR `id` carries the
                    // call_id (used by all protocols) while `call_id` on the
                    // IR preserves the original Responses `call_id`, and the
                    // item ref is stored in `id`. When re-encoding for
                    // Responses, both are replayed.
                    let item_ref = item["id"].as_str().map(|s| s.to_string());
                    let original_call_id = item["call_id"].as_str().map(|s| s.to_string());
                    // If the item has both `id` (item ref) and `call_id`
                    // (function-call identifier), store the item ref in IR
                    // `id` and the call_id in IR `call_id`. Otherwise the
                    // deduped id serves both roles.
                    let (ir_id, ir_call_id) = if item_ref.is_some() && original_call_id.is_some() {
                        // IR `id` = item reference (e.g. `fc_xxx`)
                        // IR `call_id` = function-call id (e.g. `call_xxx`)
                        (
                            item_ref.clone().unwrap_or_default(),
                            original_call_id.clone(),
                        )
                    } else {
                        (id, None)
                    };

                    vec![Content::ToolCall {
                        id: ir_id,
                        name: item["name"].as_str().unwrap_or("").to_string(),
                        arguments: serde_json::from_str(item["arguments"].as_str().unwrap_or("{}"))
                            .unwrap_or(json!({})),
                        call_id: ir_call_id,
                    }]
                } else if item["type"] == "function_call_output" {
                    // `output` is usually a string but some clients send a
                    // structured object/array; serialize non-string outputs so
                    // the tool result content is not silently dropped.
                    let output = match &item["output"] {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => serde_json::to_string(other).unwrap_or_default(),
                    };
                    let raw_id = responses_call_id(item).unwrap_or("");
                    let tool_call_id = call_id_remap
                        .get_mut(raw_id)
                        .and_then(VecDeque::pop_front)
                        .unwrap_or_else(|| raw_id.to_string());
                    // Preserve the item's own `id` (item reference) so it
                    // can be replayed when re-encoding for Responses HTTP.
                    let item_id = item["id"].as_str().map(|s| s.to_string());
                    vec![Content::ToolResult {
                        tool_call_id,
                        name: String::new(),
                        content: output,
                        id: item_id,
                    }]
                } else if item["type"] == "reasoning" {
                    // Reasoning input item (replayed assistant chain-of-thought).
                    // Pull the text out of the `summary` array (or a plain
                    // `text` field) so the thinking survives a round-trip.
                    let text = item["summary"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|s| s["text"].as_str())
                                .collect::<Vec<_>>()
                                .join("")
                        })
                        .filter(|s| !s.is_empty())
                        .or_else(|| item["text"].as_str().map(String::from))
                        .unwrap_or_default();
                    vec![Content::Reasoning {
                        text,
                        signature: None,
                        id: item["id"].as_str().map(|s| s.to_string()),
                        // Preserve the client-supplied encrypted reasoning so it
                        // can be replayed verbatim to the upstream provider,
                        // mirroring decode_response. Dropping it here would
                        // strip the encrypted payload from same-protocol
                        // multi-turn replay.
                        encrypted_content: item["encrypted_content"]
                            .as_str()
                            .map(|s| s.to_string()),
                    }]
                } else if item["type"] == "local_shell_call" {
                    // Codex local_shell_call: map to a ToolCall so it
                    // survives cross-protocol conversion. The `action`
                    // object is serialized as the tool call arguments.
                    let id = responses_call_id(item).unwrap_or("").to_string();
                    let arguments = item.get("action").cloned().unwrap_or(json!({}));
                    vec![Content::ToolCall {
                        id: id.clone(),
                        call_id: Some(id),
                        name: "local_shell".to_string(),
                        arguments,
                    }]
                } else if item["type"] == "local_shell_call_output" {
                    let output = match &item["output"] {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => serde_json::to_string(other).unwrap_or_default(),
                    };
                    let raw_id = responses_call_id(item).unwrap_or("");
                    vec![Content::ToolResult {
                        tool_call_id: raw_id.to_string(),
                        name: String::new(),
                        content: output,
                        id: None,
                    }]
                } else if item["type"] == "custom_tool_call" {
                    // Codex custom_tool_call: map to a ToolCall with the
                    // tool name and input text wrapped as JSON arguments.
                    let id = responses_call_id(item).unwrap_or("").to_string();
                    let name = item["name"].as_str().unwrap_or("").to_string();
                    let input_text = item["input"].as_str().unwrap_or("").to_string();
                    vec![Content::ToolCall {
                        id: id.clone(),
                        call_id: Some(id),
                        name,
                        arguments: json!({"input": input_text}),
                    }]
                } else if item["type"] == "custom_tool_call_output" {
                    let output = match &item["output"] {
                        Value::String(s) => s.clone(),
                        Value::Null => String::new(),
                        other => serde_json::to_string(other).unwrap_or_default(),
                    };
                    let raw_id = responses_call_id(item).unwrap_or("");
                    vec![Content::ToolResult {
                        tool_call_id: raw_id.to_string(),
                        name: String::new(),
                        content: output,
                        id: None,
                    }]
                } else {
                    vec![Content::Text {
                        text: String::new(),
                        annotations: None,
                    }]
                };
                // Merge consecutive items with the same role into one Message
                // so that e.g. a reasoning item followed by function_call items
                // end up in the same IR assistant turn. This is critical for
                // cross-protocol conversion: the Chat Completions encoder gates
                // reasoning_content on the presence of tool_calls *within the
                // same message*, so splitting them would silently drop reasoning.
                if content.is_empty() {
                    // Opaque Codex items produce no IR content; skip message
                    // creation to avoid inserting empty placeholder messages.
                    continue;
                }
                if let Some(last) = messages.last_mut() {
                    if last.role == role {
                        last.content.extend(content);
                    } else {
                        messages.push(Message { role, content });
                    }
                } else {
                    messages.push(Message { role, content });
                }
            }
        } else if let Some(text) = body["input"].as_str() {
            // OpenAI Responses API allows `input` to be a plain string
            // (shorthand for a single user message). Normalize it into the
            // same IR structure as the array form.
            messages.push(Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: text.to_string(),
                    annotations: None,
                }],
            });
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
            thinking: body.get("reasoning").and_then(|r| {
                let effort = r.get("effort").and_then(|v| v.as_str()).map(|s| {
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
                let summary = r.get("summary").and_then(|v| v.as_str()).map(String::from);
                if effort.is_none() && summary.is_none() {
                    None
                } else {
                    Some(tiygate_core::ThinkingConfig {
                        effort,
                        summary,
                        ..Default::default()
                    })
                }
            }),
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
            // Store the full reasoning object for same-protocol replay.
            extensions.insert("reasoning_full".to_string(), re.clone());
        }

        // Preserve Responses-specific top-level fields the IR does not model so
        // a same-protocol re-encode is lossless. Stored under a prefixed key.
        {
            let mut extra = serde_json::Map::new();
            for key in [
                "metadata",
                "previous_response_id",
                "store",
                "parallel_tool_calls",
                "service_tier",
                "user",
                "truncation",
                "include",
                "prompt_cache_key",
                "prompt_cache_retention",
                "client_metadata",
            ] {
                if let Some(v) = body.get(key) {
                    extra.insert(key.to_string(), v.clone());
                }
            }
            if !extra.is_empty() {
                extensions.insert("responses_extra".to_string(), json!(extra));
            }
        }

        // Preserve Codex opaque input items (tool_search_call, agent_message,
        // compaction, etc.) for same-protocol replay. Cross-protocol egress
        // drops these silently.
        if !codex_opaque_items.is_empty() {
            extensions.insert("codex_opaque_items".to_string(), json!(codex_opaque_items));
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
            metadata: body.get("metadata").and_then(|m| m.as_object()).map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            }),
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
                Content::Text { text, .. } => {
                    message_text.push_str(text);
                }
                Content::Reasoning {
                    text,
                    id,
                    encrypted_content,
                    ..
                } => {
                    // Empty reasoning text re-encodes to `summary: []` (not a
                    // summary part with an empty string) so encrypted-only
                    // reasoning round-trips to the exact OpenAI wire shape.
                    let summary = if text.is_empty() {
                        json!([])
                    } else {
                        json!([{"type": "summary_text", "text": text}])
                    };
                    let mut item = json!({"type": "reasoning", "summary": summary});
                    if let Some(rid) = id {
                        item["id"] = json!(rid);
                    }
                    if let Some(enc) = encrypted_content {
                        item["encrypted_content"] = json!(enc);
                    }
                    output_items.push(item);
                }
                Content::ToolCall {
                    id,
                    name,
                    arguments,
                    call_id,
                } => {
                    // Use `call_id` when available (Responses round-trip),
                    // otherwise fall back to `id` (cross-protocol).
                    let wire_call_id = call_id.as_deref().unwrap_or(id);
                    let mut tc = json!({"type": "function_call", "call_id": wire_call_id, "name": name, "arguments": serde_json::to_string(arguments).unwrap_or_default(), "status": "completed"});
                    // Include the item reference `id` when available.
                    if call_id.is_some() {
                        tc["id"] = json!(id);
                    }
                    tool_calls.push(tc);
                }
                Content::Refusal { text, .. } => {
                    output_items.push(json!({"type": "refusal", "refusal": text}));
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
                        if let Content::Text { text, .. } = c {
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
                    let mut reasoning_items: Vec<Value> = Vec::new();
                    let mut tool_calls_json: Vec<Value> = Vec::new();
                    let mut tool_outputs_json: Vec<Value> = Vec::new();

                    for c in &msg.content {
                        match c {
                            Content::Text { text, .. } => {
                                text_parts.push(json!({"type": "input_text", "text": text}));
                            }
                            Content::Media {
                                source,
                                mime_type,
                                metadata,
                                ..
                            } => match source {
                                tiygate_core::ir::MediaSource::Url { url } => {
                                    let mut obj = json!({
                                        "type": "input_image",
                                        "image_url": url
                                    });
                                    if let Some(d) =
                                        metadata.get(tiygate_core::ir::IMAGE_DETAIL_KEY)
                                    {
                                        obj["detail"] = d.clone();
                                    }
                                    text_parts.push(obj);
                                }
                                tiygate_core::ir::MediaSource::Inline { data } => {
                                    let mut obj = json!({
                                        "type": "input_image",
                                        "image_url": format!("data:{};base64,{}", mime_type, data)
                                    });
                                    if let Some(d) =
                                        metadata.get(tiygate_core::ir::IMAGE_DETAIL_KEY)
                                    {
                                        obj["detail"] = d.clone();
                                    }
                                    text_parts.push(obj);
                                }
                                _ => {}
                            },
                            Content::Reasoning {
                                text,
                                id,
                                encrypted_content,
                                ..
                            } => {
                                // Responses API treats reasoning as a sibling
                                // output/input item, NOT as a content sub-part
                                // of the message. The OpenAI Responses spec
                                // (and the Deepseek thinking-mode spec it
                                // mirrors) requires that the reasoning item be
                                // echoed back alongside any function_call
                                // item the same turn produced — otherwise
                                // the request is rejected.
                                //
                                // Each reasoning block is emitted as its own
                                // item so a Responses-issued `id` (`rs_...`)
                                // can be replayed verbatim. The Responses API
                                // pairs each reasoning item with the following
                                // item by id; replaying the original id keeps
                                // same-protocol multi-turn from 400ing with
                                // "Item provided without its required preceding
                                // item of type reasoning". Cross-protocol
                                // reasoning has no Responses id (id == None),
                                // so we emit it idless rather than fabricate
                                // one.
                                let summary = if text.is_empty() {
                                    json!([])
                                } else {
                                    json!([{"type": "summary_text", "text": text}])
                                };
                                let mut item = json!({
                                    "type": "reasoning",
                                    "summary": summary,
                                });
                                if let Some(rid) = id {
                                    item["id"] = json!(rid);
                                }
                                if let Some(enc) = encrypted_content {
                                    item["encrypted_content"] = json!(enc);
                                }
                                reasoning_items.push(item);
                            }
                            Content::ToolCall {
                                id,
                                name,
                                arguments,
                                call_id,
                            } => {
                                let args_str = match arguments {
                                    serde_json::Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                };
                                // Use the dedicated `call_id` when present
                                // (Responses round-trip); otherwise `id` serves
                                // both roles (cross-protocol).
                                let wire_call_id = call_id.as_deref().unwrap_or(id);
                                let mut fc = json!({
                                    "type": "function_call",
                                    "call_id": wire_call_id,
                                    "name": name,
                                    "arguments": args_str,
                                });
                                // Include the item reference `id` when the IR
                                // carries a separate call_id.
                                if call_id.is_some() {
                                    fc["id"] = json!(id);
                                }
                                tool_calls_json.push(fc);
                            }
                            Content::ToolResult {
                                tool_call_id,
                                name: _,
                                content,
                                id,
                            } => {
                                // Cross-protocol Anthropic Messages carries
                                // `tool_result` blocks inside a user message,
                                // while Responses requires a sibling
                                // `function_call_output` input item. Preserve
                                // them regardless of the IR message role so
                                // prior function calls have matching outputs.
                                tool_outputs_json.push(
                                    responses_function_call_output(
                                        tool_call_id,
                                        content,
                                        id.as_deref(),
                                    ),
                                );
                            }
                            Content::Refusal { text, .. } => {
                                text_parts.push(json!({"type": "input_text", "text": text}));
                            }
                        }
                    }

                    for item in reasoning_items {
                        input_items.push(item);
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

                    for output in tool_outputs_json {
                        input_items.push(output);
                    }
                }
                Role::Tool => {
                    for c in &msg.content {
                        if let Content::ToolResult {
                            tool_call_id,
                            name: _,
                            content,
                            id,
                        } = c
                        {
                            input_items.push(responses_function_call_output(
                                tool_call_id,
                                content,
                                id.as_deref(),
                            ));
                        }
                    }
                }
            }
        }
        body["input"] = json!(input_items);
        // Restore Codex opaque input items (tool_search_call, agent_message,
        // compaction, etc.) from extensions for same-protocol replay.
        if let Some(opaque) = ir
            .extensions
            .get("codex_opaque_items")
            .and_then(|v| v.as_array())
        {
            if let Some(arr) = body["input"].as_array_mut() {
                for item in opaque {
                    arr.push(item.clone());
                }
            }
        }
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

        // Replay modeled Responses extensions captured at decode time.
        if let Some(tc) = ir.extensions.get("tool_choice") {
            body["tool_choice"] = tc.clone();
        }
        if let Some(tf) = ir.extensions.get("text") {
            body["text"] = tf.clone();
        }
        // Thinking config: output reasoning.effort from params.thinking
        // or from the legacy extensions["reasoning_effort"] fallback.
        // Cross-protocol derivation: when effort is missing but budget_tokens
        // is present (e.g. from Anthropic/Gemini), derive effort from budget.
        if body.get("reasoning").is_none() {
            // Same-protocol replay: if the full reasoning object was captured
            // at decode time, restore it verbatim (preserves summary, etc.).
            if let Some(re_full) = ir.extensions.get("reasoning_full") {
                body["reasoning"] = re_full.clone();
            } else if let Some(ref thinking) = ir.params.thinking {
                let effort = thinking.effort.or_else(|| {
                    thinking
                        .budget_tokens
                        .map(tiygate_core::ThinkingConfig::budget_to_effort)
                });
                if let Some(effort) = effort {
                    // OpenAI supports minimal/low/medium/high/xhigh; Max clamps
                    // to "xhigh" since OpenAI has no "max" effort level.
                    body["reasoning"] = json!({"effort": match effort {
                        tiygate_core::ThinkingEffort::Minimal => "minimal",
                        tiygate_core::ThinkingEffort::Low => "low",
                        tiygate_core::ThinkingEffort::Medium => "medium",
                        tiygate_core::ThinkingEffort::High => "high",
                        tiygate_core::ThinkingEffort::XHigh => "xhigh",
                        tiygate_core::ThinkingEffort::Max => "xhigh",
                    }});
                }
                // Attach reasoning.summary if present in params.thinking.
                if let Some(ref summary) = thinking.summary {
                    if let Some(obj) = body["reasoning"].as_object_mut() {
                        obj.insert("summary".to_string(), json!(summary));
                    } else {
                        body["reasoning"] = json!({"summary": summary});
                    }
                }
            } else if let Some(effort) = ir
                .extensions
                .get("reasoning_effort")
                .and_then(|v| v.as_str())
            {
                body["reasoning"] = json!({"effort": effort});
            }
        }
        // Replay Responses-specific top-level passthrough fields.
        if let Some(extra) = ir
            .extensions
            .get("responses_extra")
            .and_then(|v| v.as_object())
        {
            for (k, v) in extra {
                if body.get(k).is_none() {
                    body[k] = v.clone();
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
                                        let annotations = part.get("annotations")
                                            .and_then(|a| a.as_array())
                                            .map(|arr| {
                                                arr.iter()
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
                                                            title: a["url_citation"]["title"].as_str().or_else(|| a["file_citation"]["filename"].as_str()).map(String::from),
                                                            url: a["url_citation"]["url"].as_str().map(String::from),
                                                        })
                                                    })
                                                    .collect::<Vec<_>>()
                                            })
                                            .filter(|v: &Vec<_>| !v.is_empty());
                                        content.push(Content::Text {
                                            text: text.to_string(),
                                            annotations,
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
                        // Responses function_call items carry two distinct ids:
                        // `id` (item reference, e.g. `fc_xxx`) and `call_id`
                        // (function-call identifier, e.g. `call_xxx`). Both
                        // must be preserved in the IR so re-encoding for
                        // Responses HTTP reproduces a valid request.
                        let item_id = item["id"].as_str().unwrap_or("").to_string();
                        let call_id = item["call_id"].as_str().map(|s| s.to_string());
                        content.push(Content::ToolCall {
                            id: item_id,
                            name: item["name"].as_str().unwrap_or("").to_string(),
                            arguments: args,
                            call_id,
                        });
                    }
                    Some("reasoning") => {
                        // Join the summary parts into a single reasoning block
                        // so the Responses `id` maps to exactly one IR
                        // Reasoning content (and re-encodes to exactly one
                        // reasoning item, avoiding duplicate-id orphans).
                        let text = item["summary"]
                            .as_array()
                            .map(|summary| {
                                summary
                                    .iter()
                                    .filter_map(|s| s["text"].as_str())
                                    .collect::<Vec<_>>()
                                    .join("")
                            })
                            .unwrap_or_default();
                        let id = item["id"].as_str().map(|s| s.to_string());
                        let encrypted_content =
                            item["encrypted_content"].as_str().map(|s| s.to_string());
                        // Keep the reasoning item only when it carries a
                        // replayable payload — summary text or encrypted
                        // content. When `include:
                        // ["reasoning.encrypted_content"]` is set with summaries
                        // disabled, OpenAI returns `summary: []` plus an
                        // `encrypted_content`; dropping the item there would
                        // break encrypted reasoning replay on later turns.
                        //
                        // A lone `id` with neither text nor encrypted content is
                        // an empty shell with nothing to replay (and would
                        // re-encode to an orphaned reasoning item that some
                        // providers reject), so it is intentionally dropped.
                        if !text.is_empty() || encrypted_content.is_some() {
                            content.push(Content::Reasoning {
                                text,
                                signature: None,
                                id,
                                encrypted_content,
                            });
                        }
                    }
                    Some("refusal") => {
                        if let Some(text) = item["refusal"].as_str() {
                            if !text.is_empty() {
                                content.push(Content::Refusal {
                                    text: text.to_string(),
                                    category: None,
                                });
                            }
                        }
                    }
                    Some("local_shell_call") => {
                        // Codex local_shell_call output item: map to ToolCall.
                        let id = responses_call_id(item).unwrap_or("").to_string();
                        let arguments = item.get("action").cloned().unwrap_or(json!({}));
                        content.push(Content::ToolCall {
                            id: id.clone(),
                            call_id: Some(id),
                            name: "local_shell".to_string(),
                            arguments,
                        });
                    }
                    Some("custom_tool_call") => {
                        // Codex custom_tool_call output item: map to ToolCall.
                        let id = responses_call_id(item).unwrap_or("").to_string();
                        let name = item["name"].as_str().unwrap_or("").to_string();
                        let input_text = item["input"].as_str().unwrap_or("").to_string();
                        content.push(Content::ToolCall {
                            id: id.clone(),
                            call_id: Some(id),
                            name,
                            arguments: json!({"input": input_text}),
                        });
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
        // Populate stop_details on incomplete status
        let stop_details = if body["status"].as_str() == Some("incomplete") {
            let reason = body
                .get("incomplete_details")
                .and_then(|d| d["reason"].as_str())
                .unwrap_or("incomplete");
            Some(tiygate_core::ir::StopDetails {
                stop_reason: reason.to_string(),
                kind: Some(reason.to_string()),
                ..Default::default()
            })
        } else {
            None
        };
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
            stop_details,
            extensions: Default::default(),
        })
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
    /// Function-call ids in allocation order, used to emit deterministic
    /// terminal output items.
    tool_output_order: Vec<String>,
    /// Function names by call id for terminal `output_item.done` and
    /// `response.completed.response.output` reconstruction.
    tool_names: std::collections::HashMap<String, String>,
    /// Accumulated function-call argument JSON fragments by call id.
    tool_arguments: std::collections::HashMap<String, String>,
    /// Function-call ids whose terminal done events were already emitted.
    tool_done: std::collections::HashSet<String>,
    /// Monotonic sequence_number stamped on every emitted event, per the
    /// Responses streaming contract.
    sequence_number: u64,
    /// Whether `response.in_progress` has been emitted yet.
    in_progress_sent: bool,
    /// Usage stashed from a `StreamPart::Usage`, emitted inside the terminal
    /// `response.completed`. Emitting `response.completed` early (on Usage)
    /// terminated the stream prematurely for strict clients; we now defer it
    /// to the real `Finish`/`ResponseCompleted`.
    pending_usage: Option<Usage>,
    /// Whether a terminal `response.completed` has already been emitted, so we
    /// do not emit it twice when both `Finish` and `ResponseCompleted` arrive.
    completed_sent: bool,
    /// Status stashed from `Finish` when `pending_usage` was not yet available.
    /// When the upstream sends `finish_reason` and `usage` as separate SSE
    /// chunks (OpenAI-compatible: finish chunk → usage chunk → [DONE]), the
    /// `Finish` part arrives before `Usage`. If we emitted `response.completed`
    /// immediately on `Finish`, the usage would be lost. Instead we stash the
    /// status here and defer `completed_event` to `ResponseCompleted` (which
    /// arrives when the upstream sends `[DONE]`), by which point `Usage` has
    /// been stashed too.
    pending_finish_status: Option<String>,
    /// output_index assigned to the reasoning item (lazily allocated on the
    /// first ReasoningDelta), mirroring text_output_index.
    reasoning_output_index: Option<u32>,
    /// Accumulated reasoning text for `output_item.done` and `completed_event`.
    reasoning_text: String,
    /// Provider-issued reasoning item id carried on the IR ReasoningDelta, so
    /// the emitted reasoning output item replays the original `rs_...` id
    /// instead of a synthesized `{response_id}_rs`.
    reasoning_id: Option<String>,
    /// Encrypted reasoning content carried on the IR ReasoningDelta, echoed on
    /// the terminal reasoning `output_item.done` and the reconstructed
    /// `response.completed.output` item for cross-turn replay.
    reasoning_encrypted: Option<String>,
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
            tool_output_order: Vec::new(),
            tool_names: std::collections::HashMap::new(),
            tool_arguments: std::collections::HashMap::new(),
            tool_done: std::collections::HashSet::new(),
            sequence_number: 0,
            in_progress_sent: false,
            pending_usage: None,
            completed_sent: false,
            pending_finish_status: None,
            reasoning_output_index: None,
            reasoning_text: String::new(),
            reasoning_id: None,
            reasoning_encrypted: None,
        }
    }

    /// Allocate the next sequence number for an emitted event.
    fn next_seq(&mut self) -> u64 {
        let s = self.sequence_number;
        self.sequence_number += 1;
        s
    }

    /// The id used for the reasoning output item across all of its lifecycle
    /// events. Prefers the provider-issued `rs_...` id carried on the IR
    /// ReasoningDelta (so the item can be replayed verbatim on later turns),
    /// falling back to a synthesized `{response_id}_rs` when none is available.
    fn reasoning_item_id(&self) -> String {
        self.reasoning_id
            .clone()
            .unwrap_or_else(|| format!("{}_rs", self.response_id.as_deref().unwrap_or("")))
    }

    /// Format a Responses SSE event, injecting the `sequence_number`.
    fn event(&mut self, mut value: Value) -> String {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("sequence_number".to_string(), json!(self.next_seq()));
        }
        format!("data: {}\n\n", value)
    }

    fn open_tool_call(&mut self, id: &str, name: &str) -> String {
        let already_open = self.tool_output_indices.contains_key(id);
        let idx = if let Some(idx) = self.tool_output_indices.get(id).copied() {
            idx
        } else {
            let idx = self.next_output_index;
            self.next_output_index += 1;
            self.tool_output_indices.insert(id.to_string(), idx);
            self.tool_output_order.push(id.to_string());
            idx
        };
        self.tool_names
            .entry(id.to_string())
            .or_insert_with(|| name.to_string());
        self.tool_arguments.entry(id.to_string()).or_default();
        if already_open {
            return String::new();
        }
        self.event(json!({"type": "response.output_item.added", "output_index": idx, "item": {"id": id, "call_id": id, "type": "function_call", "name": name, "arguments": "", "status": "in_progress"}}))
    }

    fn append_tool_arguments(&mut self, id: &str, arguments: &str) -> String {
        let idx = self.tool_output_indices.get(id).copied().unwrap_or(0);
        self.tool_arguments
            .entry(id.to_string())
            .or_default()
            .push_str(arguments);
        self.event(json!({"type": "response.function_call_arguments.delta", "item_id": id, "output_index": idx, "delta": arguments}))
    }

    fn close_tool_calls(&mut self, status: &str) -> String {
        let mut out = String::new();
        for call_id in self.tool_output_order.clone() {
            if self.tool_done.contains(&call_id) {
                continue;
            }
            let idx = self.tool_output_indices.get(&call_id).copied().unwrap_or(0);
            let name = self.tool_names.get(&call_id).cloned().unwrap_or_default();
            let arguments = self
                .tool_arguments
                .get(&call_id)
                .cloned()
                .unwrap_or_default();
            out.push_str(&self.event(json!({"type": "response.function_call_arguments.done", "item_id": call_id, "output_index": idx, "arguments": arguments})));
            out.push_str(&self.event(json!({"type": "response.output_item.done", "output_index": idx, "item": {"id": call_id, "call_id": call_id, "type": "function_call", "name": name, "arguments": arguments, "status": status}})));
            self.tool_done.insert(call_id);
        }
        out
    }

    /// Build the terminal `response.completed` event (once), folding in any
    /// stashed usage and the given status.
    fn completed_event(&mut self, status: &str) -> String {
        let id = self.response_id.clone().unwrap_or_default();
        let mut response = json!({"id": id, "status": status});
        if let Some(usage) = self.pending_usage.take() {
            // IR prompt_tokens is cache-free; Responses requires input_tokens
            // to include cache. Re-add so streamed usage stays consistent.
            let cache_read = usage.cache_read_tokens.unwrap_or(0);
            let cache_write = usage.cache_write_tokens.unwrap_or(0);
            let input = usage.prompt_tokens + cache_read + cache_write;
            response["usage"] = json!({
                "input_tokens": input,
                "output_tokens": usage.completion_tokens,
                "total_tokens": input + usage.completion_tokens,
            });
            if cache_read > 0 {
                response["usage"]["input_tokens_details"] = json!({"cached_tokens": cache_read});
            }
            if let Some(rt) = usage.reasoning_tokens {
                if rt > 0 {
                    response["usage"]["output_tokens_details"] = json!({"reasoning_tokens": rt});
                }
            }
        }
        // Build the output array so clients can reconstruct items from
        // response.completed even when incremental events were missed.
        let mut output = Vec::<Value>::new();
        if self.reasoning_output_index.is_some() {
            let item_id = self.reasoning_item_id();
            let summary = if self.reasoning_text.is_empty() {
                json!([])
            } else {
                json!([{"type": "summary_text", "text": &self.reasoning_text}])
            };
            let mut item = json!({
                "id": item_id,
                "type": "reasoning",
                "status": status,
                "summary": summary,
            });
            if let Some(enc) = &self.reasoning_encrypted {
                item["encrypted_content"] = json!(enc);
            }
            output.push(item);
        }
        if self.text_output_index.is_some() {
            let item_id = format!("{}_msg", id);
            output.push(json!({
                "id": item_id,
                "type": "message",
                "role": "assistant",
                "status": status,
                "content": []
            }));
        }
        for call_id in self.tool_output_order.clone() {
            let name = self.tool_names.get(&call_id).cloned().unwrap_or_default();
            let arguments = self
                .tool_arguments
                .get(&call_id)
                .cloned()
                .unwrap_or_default();
            output.push(json!({
                "type": "function_call",
                "id": call_id,
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
                "status": status,
            }));
        }
        if !output.is_empty() {
            response["output"] = json!(output);
        }
        self.completed_sent = true;
        self.event(json!({"type": "response.completed", "response": response}))
    }
}

impl StreamEncoder for ResponsesStreamEncoder {
    fn encode_part(&mut self, part: &StreamPart) -> Result<Vec<u8>, tiygate_core::Error> {
        let chunk = match part {
            StreamPart::ResponseStarted { id } => {
                self.response_id = Some(id.clone());
                let created = self.event(json!({"type": "response.created", "response": {"id": id, "object": "response", "status": "in_progress"}}));
                // Emit response.in_progress right after created so strict
                // clients see the lifecycle transition.
                self.in_progress_sent = true;
                let in_progress = self.event(json!({"type": "response.in_progress", "response": {"id": id, "object": "response", "status": "in_progress"}}));
                format!("{created}{in_progress}")
            }
            StreamPart::TextDelta { text } => {
                // All text fragments belong to one assistant message item;
                // allocate its output_index once and emit the item.added +
                // content_part.added lifecycle on first use.
                let item_id = format!("{}_msg", self.response_id.as_deref().unwrap_or(""));
                let mut out = String::new();
                let idx = if let Some(i) = self.text_output_index {
                    i
                } else {
                    let i = self.next_output_index;
                    self.next_output_index += 1;
                    self.text_output_index = Some(i);
                    out.push_str(&self.event(json!({"type": "response.output_item.added", "output_index": i, "item": {"id": item_id, "type": "message", "role": "assistant", "status": "in_progress", "content": []}})));
                    out.push_str(&self.event(json!({"type": "response.content_part.added", "output_index": i, "item_id": item_id, "content_index": 0, "part": {"type": "output_text", "text": ""}})));
                    i
                };
                out.push_str(&self.event(json!({"type": "response.output_text.delta", "item_id": item_id, "output_index": idx, "content_index": 0, "delta": text})));
                out
            }
            StreamPart::ReasoningDelta {
                text,
                id,
                encrypted_content,
            } => {
                // Latch the provider reasoning id / encrypted content the first
                // time each arrives so every lifecycle event (added → delta →
                // done → completed) uses the same identity and the encrypted
                // payload survives to the terminal item. Both use the same
                // first-wins policy: the id must stay stable because it is
                // already emitted on `output_item.added`, and `encrypted_content`
                // is a terminal artifact that OpenAI emits exactly once, so
                // first-wins and last-wins are equivalent in practice while
                // keeping the two fields symmetric.
                if self.reasoning_id.is_none() {
                    if let Some(rid) = id {
                        self.reasoning_id = Some(rid.clone());
                    }
                }
                if self.reasoning_encrypted.is_none() {
                    if let Some(enc) = encrypted_content {
                        self.reasoning_encrypted = Some(enc.clone());
                    }
                }
                let item_id = self.reasoning_item_id();
                let mut out = String::new();
                let idx = if let Some(i) = self.reasoning_output_index {
                    i
                } else {
                    let i = self.next_output_index;
                    self.next_output_index += 1;
                    self.reasoning_output_index = Some(i);
                    out.push_str(&self.event(json!({"type": "response.output_item.added", "output_index": i, "item": {"id": item_id, "type": "reasoning", "status": "in_progress", "summary": []}})));
                    out.push_str(&self.event(json!({"type": "response.reasoning_summary_part.added", "output_index": i, "item_id": item_id, "summary_index": 0, "part": {"type": "summary_text", "text": ""}})));
                    i
                };
                self.reasoning_text.push_str(text);
                // A zero-text delta (encrypted-only reasoning flushed at item
                // done) carries no summary delta — the encrypted payload rides
                // on the terminal output_item.done instead.
                if !text.is_empty() {
                    out.push_str(&self.event(json!({"type": "response.reasoning_summary_text.delta", "item_id": item_id, "output_index": idx, "summary_index": 0, "delta": text})));
                }
                out
            }
            StreamPart::ToolCallDelta {
                id,
                name,
                arguments,
            } => {
                if let Some(n) = name {
                    // Opener: allocate a distinct output_index for this call.
                    let mut out = self.open_tool_call(id, n);
                    if !arguments.is_empty() {
                        out.push_str(&self.append_tool_arguments(id, arguments));
                    }
                    out
                } else {
                    self.append_tool_arguments(id, arguments)
                }
            }
            StreamPart::Usage { usage } => {
                // Stash usage for the terminal response.completed instead of
                // emitting it early. If a Finish already arrived first (Gemini
                // can decode `finishReason` before same-frame `usageMetadata`,
                // and OpenAI-compatible streams may send finish before usage),
                // we now have both pieces and can safely complete immediately.
                self.pending_usage = Some(usage.clone());
                if !self.completed_sent {
                    if let Some(status) = self.pending_finish_status.take() {
                        self.completed_event(&status)
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                }
            }
            StreamPart::Finish { reason } => {
                if self.completed_sent || self.pending_finish_status.is_some() {
                    String::new()
                } else {
                    let status = match reason {
                        FinishReason::Stop => "completed",
                        FinishReason::Length => "incomplete",
                        FinishReason::ContentFilter => "incomplete",
                        FinishReason::ToolCalls => "completed",
                        _ => "completed",
                    };
                    // Close the open text item's lifecycle before completing.
                    let mut out = String::new();
                    // Close reasoning item lifecycle first (reasoning precedes
                    // text in the output sequence).
                    if let Some(idx) = self.reasoning_output_index {
                        let item_id = self.reasoning_item_id();
                        out.push_str(&self.event(json!({"type": "response.reasoning_summary_text.done", "output_index": idx, "item_id": item_id, "summary_index": 0})));
                        out.push_str(&self.event(json!({"type": "response.reasoning_summary_part.done", "output_index": idx, "item_id": item_id, "summary_index": 0})));
                        // Mirror completed_event: empty reasoning text re-encodes
                        // to `summary: []` (not a summary part with an empty
                        // string) so encrypted-only reasoning round-trips to the
                        // exact OpenAI wire shape on output_item.done too.
                        let summary = if self.reasoning_text.is_empty() {
                            json!([])
                        } else {
                            json!([{"type": "summary_text", "text": &self.reasoning_text}])
                        };
                        let mut done_item = json!({"id": item_id, "type": "reasoning", "status": status, "summary": summary});
                        if let Some(enc) = &self.reasoning_encrypted {
                            done_item["encrypted_content"] = json!(enc);
                        }
                        out.push_str(&self.event(json!({"type": "response.output_item.done", "output_index": idx, "item": done_item})));
                    }
                    if let Some(idx) = self.text_output_index {
                        let item_id = format!("{}_msg", self.response_id.as_deref().unwrap_or(""));
                        out.push_str(&self.event(json!({"type": "response.output_text.done", "output_index": idx, "item_id": item_id, "content_index": 0})));
                        out.push_str(&self.event(json!({"type": "response.content_part.done", "output_index": idx, "item_id": item_id, "content_index": 0})));
                        out.push_str(&self.event(json!({"type": "response.output_item.done", "output_index": idx, "item": {"id": item_id, "type": "message", "role": "assistant", "status": "completed"}})));
                    }
                    out.push_str(&self.close_tool_calls(status));
                    // When usage is already stashed (same-chunk finish+usage),
                    // emit response.completed immediately. Otherwise defer to
                    // ResponseCompleted so a late-arriving Usage is included.
                    if self.pending_usage.is_some() {
                        out.push_str(&self.completed_event(status));
                    } else {
                        self.pending_finish_status = Some(status.to_string());
                    }
                    out
                }
            }
            StreamPart::ResponseCompleted { .. } => {
                // If no Finish arrived, emit the terminal completed now so the
                // usage is not lost; then end the stream.
                let mut out = String::new();
                if !self.completed_sent {
                    let status = self
                        .pending_finish_status
                        .take()
                        .unwrap_or_else(|| "completed".to_string());
                    out.push_str(&self.close_tool_calls(&status));
                    out.push_str(&self.completed_event(&status));
                }
                out.push_str("data: [DONE]\n\n");
                out
            }
            StreamPart::Error { message, .. } => self.event(
                json!({"type": "error", "error": {"message": message, "type": "gateway_error"}}),
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
    /// Whether ANY `function_call` output item appeared during this response.
    /// Unlike `in_function_call` (which is reset on `response.output_item.done`),
    /// this latches for the whole stream so the terminal `response.completed`
    /// can be mapped to `FinishReason::ToolCalls`. OpenAI Responses reports
    /// `status: "completed"` even for tool-call turns — the only reliable
    /// signal that the turn ended to call a tool is the presence of a
    /// `function_call` output item, NOT the status.
    saw_function_call: bool,
    /// Reasoning item id captured from `response.output_item.added`
    /// (item.type == "reasoning"). Attached to the first `ReasoningDelta` of
    /// the item and then cleared, so the id survives the stream boundary
    /// without being repeated on every delta.
    pending_reasoning_id: Option<String>,
    /// Encrypted reasoning content captured from the reasoning output item
    /// (`response.output_item.added` or `.done`). Attached to a `ReasoningDelta`
    /// once and then cleared.
    pending_reasoning_encrypted: Option<String>,
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
            saw_function_call: false,
            pending_reasoning_id: None,
            pending_reasoning_encrypted: None,
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
                    extensions: HashMap::new(),
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
                    // Attach the reasoning id / encrypted content captured from
                    // the reasoning output item to the first delta, then clear
                    // it so it is not repeated on subsequent deltas.
                    parts.push(StreamPart::ReasoningDelta {
                        text: text.to_string(),
                        id: self.pending_reasoning_id.take(),
                        encrypted_content: self.pending_reasoning_encrypted.take(),
                    });
                }
            }
            Some("response.output_item.added") => {
                let item = &event["item"];
                if item["type"] == "function_call" {
                    self.in_function_call = true;
                    self.saw_function_call = true;
                    self.current_call_id = item["id"].as_str().map(String::from);
                    self.current_call_name = item["name"].as_str().map(String::from);
                    parts.push(StreamPart::ToolCallDelta {
                        id: self.current_call_id.clone().unwrap_or_default(),
                        name: self.current_call_name.clone(),
                        arguments: String::new(),
                    });
                } else if item["type"] == "reasoning" {
                    // Stash the reasoning item id / encrypted content so the
                    // first ReasoningDelta can carry them across the stream
                    // boundary. The added event normally has empty summaries,
                    // so the text itself still arrives via the delta events.
                    if let Some(id) = item["id"].as_str() {
                        self.pending_reasoning_id = Some(id.to_string());
                    }
                    if let Some(enc) = item["encrypted_content"].as_str() {
                        self.pending_reasoning_encrypted = Some(enc.to_string());
                    }
                } else if item["type"] == "local_shell_call" {
                    // Codex local_shell_call: treat as a tool call so the
                    // streaming finish_reason is ToolCalls, not Stop.
                    self.in_function_call = true;
                    self.saw_function_call = true;
                    let id = responses_call_id(item).unwrap_or("").to_string();
                    let action = item.get("action").cloned().unwrap_or(json!({}));
                    self.current_call_id = Some(id.clone());
                    self.current_call_name = Some("local_shell".to_string());
                    parts.push(StreamPart::ToolCallDelta {
                        id,
                        name: Some("local_shell".to_string()),
                        arguments: action.to_string(),
                    });
                } else if item["type"] == "custom_tool_call" {
                    // Codex custom_tool_call: treat as a tool call so the
                    // streaming finish_reason is ToolCalls, not Stop.
                    self.in_function_call = true;
                    self.saw_function_call = true;
                    let id = responses_call_id(item).unwrap_or("").to_string();
                    let name = item["name"].as_str().unwrap_or("").to_string();
                    let input_text = item["input"].as_str().unwrap_or("").to_string();
                    self.current_call_id = Some(id.clone());
                    self.current_call_name = Some(name.clone());
                    parts.push(StreamPart::ToolCallDelta {
                        id,
                        name: Some(name),
                        arguments: json!({"input": input_text}).to_string(),
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
                let item = &event["item"];
                if item["type"] == "reasoning" {
                    // The terminal reasoning item often carries the final
                    // encrypted_content (and id) that the `.added` event lacked.
                    // Capture it; if no ReasoningDelta consumed the pending
                    // payload (e.g. summaries disabled, encrypted-only
                    // reasoning), flush it on a zero-text delta so the
                    // encrypted reasoning is not lost.
                    if let Some(id) = item["id"].as_str() {
                        self.pending_reasoning_id = Some(id.to_string());
                    }
                    if let Some(enc) = item["encrypted_content"].as_str() {
                        self.pending_reasoning_encrypted = Some(enc.to_string());
                    }
                    if self.pending_reasoning_id.is_some()
                        || self.pending_reasoning_encrypted.is_some()
                    {
                        parts.push(StreamPart::ReasoningDelta {
                            text: String::new(),
                            id: self.pending_reasoning_id.take(),
                            encrypted_content: self.pending_reasoning_encrypted.take(),
                        });
                    }
                }
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
            Some("response.completed") | Some("response.done") => {
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
                    // OpenAI Responses reports `status: "completed"` even when
                    // the turn stopped to call a tool. A `function_call` output
                    // item is the authoritative signal, so prefer ToolCalls
                    // when one was seen — otherwise the cross-protocol encoder
                    // emits `finish_reason: "stop"` and the client never runs
                    // the tool.
                    "completed" => {
                        if self.saw_function_call {
                            FinishReason::ToolCalls
                        } else {
                            FinishReason::Stop
                        }
                    }
                    "incomplete" => {
                        if self.saw_function_call {
                            FinishReason::ToolCalls
                        } else {
                            FinishReason::Length
                        }
                    }
                    other => FinishReason::Other(other.to_string()),
                };
                parts.push(StreamPart::Finish { reason });
                // The Responses protocol terminates with `response.completed`
                // (it does NOT send a trailing `data: [DONE]`). Emit the IR
                // terminal `ResponseCompleted` so cross-protocol ingress
                // encoders (e.g. ChatCompletions -> `data: [DONE]`, Anthropic
                // -> `event: message_stop`) produce their protocol-native end
                // frame. Without this the client stream ends after the final
                // chunk with no terminator. Mirrors the Anthropic decoder,
                // which pushes `ResponseCompleted` on `message_stop`.
                parts.push(StreamPart::ResponseCompleted {
                    id: self.response_id.clone().unwrap_or_default(),
                    status: "completed".to_string(),
                    usage: None,
                    extensions: HashMap::new(),
                });
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
                let reason = if self.saw_function_call {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Length
                };
                parts.push(StreamPart::Finish { reason });
                // Same terminator rule as `response.completed`: this is a real
                // end-of-stream signal, so emit `ResponseCompleted` to drive
                // the ingress encoder's protocol-native end frame.
                parts.push(StreamPart::ResponseCompleted {
                    id: self.response_id.clone().unwrap_or_default(),
                    status: "incomplete".to_string(),
                    usage: None,
                    extensions: HashMap::new(),
                });
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn make_raw_env() -> RawEnvelope {
        RawEnvelope {
            method: "POST".to_string(),
            path: "/v1/responses".to_string(),
            headers: std::collections::HashMap::new(),
            body: None,
            original_body_size: 0,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn test_decode_basic_request() {
        let _codec = ResponsesCodec::new();
    }

    #[test]
    fn test_decode_string_input() {
        // OpenAI Responses API allows `input` to be a plain string.
        // The decoder must normalize it into a user message.
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-4o",
            "input": "Hello, who are you?",
        });
        let ir = codec.decode_request(body, &env).unwrap();
        assert_eq!(ir.messages.len(), 1);
        assert!(matches!(ir.messages[0].role, Role::User));
        assert!(matches!(
            &ir.messages[0].content[0],
            Content::Text { text, .. } if text == "Hello, who are you?"
        ));
    }

    #[test]
    fn test_decode_request_reasoning_input_item() {
        // 高影响回归:reasoning input item 必须解析为 Content::Reasoning。
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5",
            "input": [
                {"role": "user", "content": "hi"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "let me think"}]}
            ]
        });
        let ir = codec.decode_request(body, &env).unwrap();
        let has_reasoning = ir.messages.iter().any(|m| {
            m.content
                .iter()
                .any(|c| matches!(c, Content::Reasoning { text, .. } if text == "let me think"))
        });
        assert!(
            has_reasoning,
            "reasoning input item should decode to Reasoning"
        );
    }

    #[test]
    fn test_stream_encoder_usage_deferred_to_completed() {
        // 高影响回归:Usage 不再提前发 response.completed;只在 Finish 发一次。
        let mut enc = ResponsesStreamEncoder::new();
        let usage_bytes = enc
            .encode_part(&StreamPart::Usage {
                usage: Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    ..Default::default()
                },
            })
            .unwrap();
        // Usage alone must NOT emit response.completed.
        assert!(usage_bytes.is_empty());
        let finish_bytes = enc
            .encode_part(&StreamPart::Finish {
                reason: FinishReason::Stop,
            })
            .unwrap();
        let s = String::from_utf8_lossy(&finish_bytes);
        assert!(s.contains("response.completed"));
        assert!(s.contains("\"input_tokens\":10"));
        assert!(s.contains("sequence_number"));
    }

    #[test]
    fn test_stream_encoder_usage_after_finish_completes_with_cache_read() {
        let mut enc = ResponsesStreamEncoder::new();
        let finish_bytes = enc
            .encode_part(&StreamPart::Finish {
                reason: FinishReason::ToolCalls,
            })
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&finish_bytes).contains("response.completed"),
            "Finish before Usage must defer completed so usage can be included"
        );

        let usage_bytes = enc
            .encode_part(&StreamPart::Usage {
                usage: Usage {
                    prompt_tokens: 4927,
                    completion_tokens: 62,
                    total_tokens: 146112,
                    cache_read_tokens: Some(141123),
                    ..Default::default()
                },
            })
            .unwrap();
        let s = String::from_utf8_lossy(&usage_bytes);
        assert!(s.contains("\"type\":\"response.completed\""), "{s}");
        assert!(s.contains("\"input_tokens\":146050"), "{s}");
        assert!(s.contains("\"cached_tokens\":141123"), "{s}");
        assert!(s.contains("\"output_tokens\":62"), "{s}");
    }

    #[test]
    fn test_stream_encoder_function_call_item_includes_call_id() {
        let mut enc = ResponsesStreamEncoder::new();
        let bytes = enc
            .encode_part(&StreamPart::ToolCallDelta {
                id: "call_123".to_string(),
                name: Some("lookup".to_string()),
                arguments: String::new(),
            })
            .unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\"type\":\"response.output_item.added\""));
        assert!(s.contains("\"type\":\"function_call\""));
        assert!(
            s.contains("\"call_id\":\"call_123\""),
            "Responses stream function_call item must expose call_id for clients: {s}"
        );
    }

    #[test]
    fn test_stream_encoder_repeated_function_call_opener_is_deduped() {
        let mut enc = ResponsesStreamEncoder::new();
        let first = enc
            .encode_part(&StreamPart::ToolCallDelta {
                id: "call_123".to_string(),
                name: Some("lookup".to_string()),
                arguments: String::new(),
            })
            .unwrap();
        let second = enc
            .encode_part(&StreamPart::ToolCallDelta {
                id: "call_123".to_string(),
                name: Some("lookup".to_string()),
                arguments: String::new(),
            })
            .unwrap();

        assert!(String::from_utf8_lossy(&first).contains("response.output_item.added"));
        assert!(
            !String::from_utf8_lossy(&second).contains("response.output_item.added"),
            "repeated opener for the same call id must not emit duplicate output_item.added: {}",
            String::from_utf8_lossy(&second)
        );
    }

    #[test]
    fn test_stream_encoder_reasoning_lifecycle() {
        let mut enc = ResponsesStreamEncoder::new();
        // ResponseStarted
        let _ = enc
            .encode_part(&StreamPart::ResponseStarted {
                id: "resp_r1".to_string(),
            })
            .unwrap();
        // First ReasoningDelta — should emit output_item.added + summary_part.added + delta
        let bytes1 = enc
            .encode_part(&StreamPart::ReasoningDelta {
                text: "thinking".to_string(),
                id: None,
                encrypted_content: None,
            })
            .unwrap();
        let s1 = String::from_utf8_lossy(&bytes1);
        assert!(
            s1.contains("\"type\":\"response.output_item.added\""),
            "first reasoning delta must emit output_item.added: {s1}"
        );
        assert!(
            s1.contains("\"type\":\"reasoning\""),
            "output_item.added item must have type=reasoning: {s1}"
        );
        assert!(
            s1.contains("\"type\":\"response.reasoning_summary_part.added\""),
            "first reasoning delta must emit reasoning_summary_part.added: {s1}"
        );
        assert!(
            s1.contains("\"type\":\"response.reasoning_summary_text.delta\""),
            "reasoning delta must emit reasoning_summary_text.delta: {s1}"
        );
        assert!(
            s1.contains("\"delta\":\"thinking\""),
            "delta must contain the reasoning text: {s1}"
        );

        // Second ReasoningDelta — should NOT re-emit output_item.added
        let bytes2 = enc
            .encode_part(&StreamPart::ReasoningDelta {
                text: " harder".to_string(),
                id: None,
                encrypted_content: None,
            })
            .unwrap();
        let s2 = String::from_utf8_lossy(&bytes2);
        assert!(
            !s2.contains("response.output_item.added"),
            "subsequent reasoning delta must not re-emit output_item.added: {s2}"
        );
        assert!(
            s2.contains("\"delta\":\" harder\""),
            "second delta must contain text: {s2}"
        );

        // Usage with reasoning_tokens
        let _ = enc
            .encode_part(&StreamPart::Usage {
                usage: Usage {
                    prompt_tokens: 10,
                    completion_tokens: 20,
                    reasoning_tokens: Some(15),
                    ..Default::default()
                },
            })
            .unwrap();

        // Finish
        let finish_bytes = enc
            .encode_part(&StreamPart::Finish {
                reason: FinishReason::Stop,
            })
            .unwrap();
        let sf = String::from_utf8_lossy(&finish_bytes);

        // Reasoning done events
        assert!(
            sf.contains("\"type\":\"response.reasoning_summary_text.done\""),
            "finish must emit reasoning_summary_text.done: {sf}"
        );
        assert!(
            sf.contains("\"type\":\"response.reasoning_summary_part.done\""),
            "finish must emit reasoning_summary_part.done: {sf}"
        );
        // output_item.done with accumulated summary
        assert!(
            sf.contains("\"type\":\"response.output_item.done\""),
            "finish must emit output_item.done for reasoning: {sf}"
        );
        assert!(
            sf.contains("thinking harder"),
            "output_item.done must contain accumulated reasoning text: {sf}"
        );

        // response.completed with output array and reasoning_tokens
        assert!(
            sf.contains("\"type\":\"response.completed\""),
            "finish must emit response.completed: {sf}"
        );
        assert!(
            sf.contains("\"type\":\"reasoning\""),
            "completed output must contain reasoning item: {sf}"
        );
        assert!(
            sf.contains("\"reasoning_tokens\":15"),
            "completed usage must include reasoning_tokens: {sf}"
        );
    }

    #[test]
    fn test_stream_encoder_encrypted_only_reasoning_empty_summary() {
        // Encrypted-only reasoning: zero text delta carries no summary delta;
        // both output_item.done and response.completed must emit `summary: []`
        // (not a summary part with an empty string) and preserve the
        // encrypted_content + provider id.
        let mut enc = ResponsesStreamEncoder::new();
        let _ = enc
            .encode_part(&StreamPart::ResponseStarted {
                id: "resp_e1".to_string(),
            })
            .unwrap();
        let _ = enc
            .encode_part(&StreamPart::ReasoningDelta {
                text: String::new(),
                id: Some("rs_enc1".to_string()),
                encrypted_content: Some("enc-blob".to_string()),
            })
            .unwrap();
        let _ = enc
            .encode_part(&StreamPart::Usage {
                usage: Usage {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    reasoning_tokens: Some(1),
                    ..Default::default()
                },
            })
            .unwrap();
        let finish_bytes = enc
            .encode_part(&StreamPart::Finish {
                reason: FinishReason::Stop,
            })
            .unwrap();
        let sf = String::from_utf8_lossy(&finish_bytes);

        // output_item.done uses the provider-issued rs_... id
        assert!(
            sf.contains("\"id\":\"rs_enc1\""),
            "output_item.done must use provider reasoning id: {sf}"
        );
        assert!(
            sf.contains("\"encrypted_content\":\"enc-blob\""),
            "output_item.done must carry encrypted_content: {sf}"
        );
        // No summary_text delta emitted for zero-text reasoning
        assert!(
            !sf.contains("response.reasoning_summary_text.delta"),
            "zero-text reasoning must not emit summary_text.delta: {sf}"
        );
        // summary: [] must appear (not summary_text with empty string)
        assert!(
            sf.contains("\"summary\":[]"),
            "output_item.done must emit summary: [] for encrypted-only reasoning: {sf}"
        );
        assert!(
            !sf.contains("\"text\":\"\""),
            "must not emit an empty-string summary_text part: {sf}"
        );
    }

    #[test]
    fn test_encode_response_text() {
        let codec = ResponsesCodec::new();
        let ir = IrResponse {
            content: vec![Content::Text {
                text: "Hi!".to_string(),
                annotations: None,
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

    /// Anthropic Messages represents tool results as `tool_result` content
    /// blocks inside a user message. When routing that history to the OpenAI
    /// Responses API, those blocks must become sibling `function_call_output`
    /// input items; otherwise Responses rejects the request with 400
    /// `No tool output found for function call ...`.
    #[test]
    fn test_encode_request_preserves_anthropic_tool_results_for_responses() {
        let anthropic = crate::messages::MessagesCodec::new();
        let responses = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "openai/gpt-5.5",
            "stream": true,
            "max_tokens": 128000,
            "messages": [
                {"role": "user", "content": "请搜索并总结。"},
                {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "我先查询资料。"},
                        {"type": "tool_use", "id": "fc_1", "name": "web_search", "input": {"query": "a"}},
                        {"type": "tool_use", "id": "fc_2", "name": "web_search", "input": {"query": "b"}}
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "下面是搜索结果。"},
                        {"type": "tool_result", "tool_use_id": "fc_1", "content": "result-a"},
                        {"type": "tool_result", "tool_use_id": "fc_2", "content": [{"type": "text", "text": "result-b"}]}
                    ]
                }
            ],
            "tools": [
                {"name": "web_search", "description": "search", "input_schema": {"type": "object"}}
            ]
        });

        let ir = anthropic.decode_request(body, &env).unwrap();
        let (encoded, _) = responses.encode_request(&ir).unwrap();
        let input = encoded["input"]
            .as_array()
            .expect("Responses input[] present");

        let function_calls: Vec<&Value> = input
            .iter()
            .filter(|item| item["type"] == "function_call")
            .collect();
        let function_outputs: Vec<&Value> = input
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .collect();

        assert_eq!(function_calls.len(), 2, "both tool_use blocks survive");
        assert_eq!(function_outputs.len(), 2, "both tool_result blocks survive");
        assert_eq!(function_outputs[0]["call_id"], "fc_1");
        assert_eq!(function_outputs[0]["output"], "result-a");
        assert_eq!(function_outputs[1]["call_id"], "fc_2");
        assert_eq!(function_outputs[1]["output"], "result-b");

        let first_call_idx = input
            .iter()
            .position(|item| item["type"] == "function_call")
            .expect("function_call present");
        let mixed_user_text_idx = input
            .iter()
            .position(|item| item["role"] == "user" && item["content"] == "下面是搜索结果。")
            .expect("mixed user text message present");
        let first_output_idx = input
            .iter()
            .position(|item| item["type"] == "function_call_output")
            .expect("function_call_output present");
        assert!(
            first_output_idx > first_call_idx,
            "tool outputs must follow the tool calls they answer"
        );
        assert!(
            mixed_user_text_idx < first_output_idx,
            "text that shares an Anthropic user message with tool_result must keep natural order"
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
                        annotations: None,
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![
                        Content::Reasoning {
                            text: "我需要先查日期再查天气。".to_string(),
                            signature: None,
                            id: None,
                            encrypted_content: None,
                        },
                        Content::ToolCall {
                            id: "call_1".to_string(),
                            name: "get_weather".to_string(),
                            arguments: serde_json::json!({"location": "杭州"}),
                            call_id: None,
                        },
                    ],
                },
                Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult {
                        tool_call_id: "call_1".to_string(),
                        name: "get_weather".to_string(),
                        content: "cloudy".to_string(),
                        id: None,
                    }],
                },
            ],
            tools: vec![],
            params: tiygate_core::GenerationParams::default(),
            stream: false,
            response_format: None,
            metadata: None,
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
                    signature: None,
                    id: None,
                    encrypted_content: None,
                }],
            }],
            tools: vec![],
            params: tiygate_core::GenerationParams::default(),
            stream: false,
            response_format: None,
            metadata: None,
            extensions: Default::default(),
        };
        let (body, _) = codec.encode_request(&ir).unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "reasoning");
    }

    /// Same-protocol (Responses → Responses) round-trip must preserve the
    /// reasoning item `id` (`rs_...`). The Responses API pairs each reasoning
    /// item with the following item by id; losing the id causes a 400
    /// "Item provided without its required preceding item of type reasoning"
    /// on the next turn. Cross-protocol reasoning (id == None) must be emitted
    /// without a fabricated id.
    #[test]
    fn test_responses_reasoning_id_roundtrip() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        // A request replaying a prior Responses turn: reasoning item carries
        // its original `rs_...` id, followed by a function_call.
        let body = json!({
            "model": "gpt-5",
            "input": [
                {"role": "user", "content": "weather?"},
                {
                    "type": "reasoning",
                    "id": "rs_abc123",
                    "summary": [{"type": "summary_text", "text": "check the tool"}]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": "{}"
                }
            ]
        });
        let ir = codec.decode_request(body, &env).unwrap();
        // The reasoning id must survive into the IR.
        let captured_id = ir.messages.iter().find_map(|m| {
            m.content.iter().find_map(|c| match c {
                Content::Reasoning { id, .. } => id.clone(),
                _ => None,
            })
        });
        assert_eq!(
            captured_id.as_deref(),
            Some("rs_abc123"),
            "reasoning id 应被解析进 IR"
        );

        // Re-encode: the reasoning item must replay the exact id.
        let (re, _) = codec.encode_request(&ir).unwrap();
        let input = re["input"].as_array().unwrap();
        let reasoning = input
            .iter()
            .find(|i| i["type"] == "reasoning")
            .expect("reasoning item present");
        assert_eq!(reasoning["id"], "rs_abc123", "reasoning id 必须原样回传");
        assert_eq!(reasoning["summary"][0]["text"], "check the tool");
    }

    #[test]
    fn test_responses_duplicate_call_ids_are_normalized_for_tool_results() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let duplicate = "call_e3b0c44298fc1c149afbf4c8996fb92427a";
        let body = json!({
            "model": "minimax/minimax-m3",
            "input": [
                {"type": "message", "role": "user", "content": "review this"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "I'll inspect it."}]},
                {"type": "function_call", "call_id": duplicate, "name": "git_status", "arguments": "{}"},
                {"type": "function_call", "call_id": duplicate, "name": "git_diff", "arguments": "{\"path\":\"crates/store/src/log_sink/oltp.rs\"}"},
                {"type": "function_call_output", "call_id": duplicate, "output": "status output"},
                {"type": "function_call_output", "call_id": duplicate, "output": "diff output"}
            ]
        });

        let ir = codec.decode_request(body, &env).unwrap();

        let tool_call_ids: Vec<String> = ir
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                Content::ToolCall { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            tool_call_ids,
            vec![duplicate.to_string(), format!("{duplicate}_1")]
        );

        let tool_result_ids: Vec<String> = ir
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                Content::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_result_ids, tool_call_ids);

        let anthropic = crate::messages::MessagesCodec::new();
        let (encoded, _) = anthropic.encode_request(&ir).unwrap();
        let messages = encoded["messages"].as_array().unwrap();
        let assistant_tool_ids: Vec<String> = messages
            .iter()
            .filter(|m| m["role"] == "assistant")
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter(|block| block["type"] == "tool_use")
            .filter_map(|block| block["id"].as_str().map(String::from))
            .collect();
        assert_eq!(assistant_tool_ids, tool_call_ids);
    }

    /// Cross-protocol reasoning (no Responses id) must be emitted WITHOUT an
    /// `id` field — fabricating one would be rejected by the Responses API.
    #[test]
    fn test_responses_cross_protocol_reasoning_has_no_id() {
        let codec = ResponsesCodec::new();
        let ir = tiygate_core::IrRequest {
            model: "gpt-5".to_string(),
            system: None,
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::AnthropicMessages,
                "messages",
                "2023-06-01",
            ),
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![Content::Reasoning {
                    text: "from anthropic".to_string(),
                    signature: Some("sig_anthropic".to_string()),
                    id: None,
                    encrypted_content: None,
                }],
            }],
            tools: vec![],
            params: tiygate_core::GenerationParams::default(),
            stream: false,
            response_format: None,
            metadata: None,
            extensions: Default::default(),
        };
        let (body, _) = codec.encode_request(&ir).unwrap();
        let input = body["input"].as_array().unwrap();
        let reasoning = input
            .iter()
            .find(|i| i["type"] == "reasoning")
            .expect("reasoning item present");
        assert!(
            reasoning.get("id").is_none(),
            "跨协议 reasoning 不应带 id(避免伪造 id 被 400)"
        );
        // Anthropic 的 signature 不得泄漏到 Responses 的 reasoning item。
        assert!(reasoning.get("signature").is_none());
    }

    /// Responses decode_request 必须将连续的同 role input items 合并到
    /// 同一个 IR Message 中。如果 reasoning 和 function_call 被拆分为
    /// 独立的 Message,Chat Completions encode_request 的门控逻辑
    /// `!reasoning_text.is_empty() && !tool_calls_json.is_empty()`
    /// 无法在同一个 message 中同时看到两者,导致 reasoning_content
    /// 被丢弃,DeepSeek 400。
    #[test]
    fn test_decode_request_merges_consecutive_same_role_items() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        // 模拟客户端回传: reasoning + 2个 function_call + 2个 function_call_output
        let body = json!({
            "model": "deepseek-v4-pro",
            "input": [
                {"role": "user", "content": "天气?"},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "需要查天气"}]},
                {"type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"杭州\"}"},
                {"type": "function_call", "call_id": "call_2", "name": "get_weather", "arguments": "{\"city\":\"北京\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "晴天"},
                {"type": "function_call_output", "call_id": "call_2", "output": "多云"}
            ]
        });
        let ir = codec.decode_request(body, &env).unwrap();

        // reasoning(Assistant) + function_call(Assistant) + function_call(Assistant)
        // 应合并为一个 Assistant message
        let assistant_msgs: Vec<_> = ir
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .collect();
        assert_eq!(
            assistant_msgs.len(),
            1,
            "连续的 reasoning + function_call items 必须合并为一个 assistant message, 实际: {}",
            assistant_msgs.len()
        );

        let content = &assistant_msgs[0].content;
        let has_reasoning = content
            .iter()
            .any(|c| matches!(c, Content::Reasoning { .. }));
        let tool_call_count = content
            .iter()
            .filter(|c| matches!(c, Content::ToolCall { .. }))
            .count();
        assert!(
            has_reasoning,
            "合并后的 assistant message 必须包含 Reasoning"
        );
        assert_eq!(
            tool_call_count, 2,
            "合并后的 assistant message 必须包含 2 个 ToolCall"
        );

        // function_call_output(Tool) + function_call_output(Tool)
        // 也应合并为一个 Tool message
        let tool_msgs: Vec<_> = ir
            .messages
            .iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert_eq!(
            tool_msgs.len(),
            1,
            "连续的 function_call_output items 必须合并为一个 tool message, 实际: {}",
            tool_msgs.len()
        );
        assert_eq!(tool_msgs[0].content.len(), 2, "tool message 应含 2 个结果");
    }

    /// 不同 role 的 items 不应被合并:user → assistant → tool 保持分离。
    #[test]
    fn test_decode_request_does_not_merge_different_roles() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5",
            "input": [
                {"role": "user", "content": "hi"},
                {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "ok"},
                {"role": "user", "content": "thanks"}
            ]
        });
        let ir = codec.decode_request(body, &env).unwrap();
        assert_eq!(
            ir.messages.len(),
            4,
            "不同 role 的 items 不应合并: user(1) + assistant(1) + tool(1) + user(1) = 4"
        );
        assert_eq!(ir.messages[0].role, Role::User);
        assert_eq!(ir.messages[1].role, Role::Assistant);
        assert_eq!(ir.messages[2].role, Role::Tool);
        assert_eq!(ir.messages[3].role, Role::User);
    }

    #[test]
    fn test_decode_local_shell_call_input_item() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5",
            "input": [
                {"role": "user", "content": "list files"},
                {"type": "local_shell_call", "call_id": "call_shell_1", "action": {"command": ["ls", "-la"]}}
            ]
        });
        let ir = codec.decode_request(body, &env).unwrap();
        let tool_call = ir
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .find(|c| matches!(c, Content::ToolCall { name, .. } if name == "local_shell"))
            .expect("local_shell_call should map to ToolCall");
        if let Content::ToolCall {
            id,
            name,
            arguments,
            call_id: _,
        } = tool_call
        {
            assert_eq!(id, "call_shell_1");
            assert_eq!(name, "local_shell");
            assert_eq!(arguments["command"], json!(["ls", "-la"]));
        }
    }

    #[test]
    fn test_decode_custom_tool_call_input_item() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5",
            "input": [
                {"role": "user", "content": "run custom tool"},
                {"type": "custom_tool_call", "call_id": "call_custom_1", "name": "my_tool", "input": "some input text"}
            ]
        });
        let ir = codec.decode_request(body, &env).unwrap();
        let tool_call = ir
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .find(|c| matches!(c, Content::ToolCall { name, .. } if name == "my_tool"))
            .expect("custom_tool_call should map to ToolCall");
        if let Content::ToolCall {
            id,
            name,
            arguments,
            call_id: _,
        } = tool_call
        {
            assert_eq!(id, "call_custom_1");
            assert_eq!(name, "my_tool");
            assert_eq!(arguments["input"], "some input text");
        }
    }

    #[test]
    fn test_decode_codex_opaque_items_preserved() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5",
            "input": [
                {"role": "user", "content": "hi"},
                {"type": "tool_search_call", "call_id": "ts_1", "query": "find tools"},
                {"type": "agent_message", "content": "agent response"},
                {"type": "compaction", "id": "comp_1", "summary": "compacted"}
            ]
        });
        let ir = codec.decode_request(body, &env).unwrap();
        let opaque = ir
            .extensions
            .get("codex_opaque_items")
            .and_then(|v| v.as_array())
            .expect("codex_opaque_items should be in extensions");
        assert_eq!(opaque.len(), 3, "should have 3 opaque items");
        assert_eq!(opaque[0]["type"], "tool_search_call");
        assert_eq!(opaque[1]["type"], "agent_message");
        assert_eq!(opaque[2]["type"], "compaction");
    }

    #[test]
    fn test_encode_codex_opaque_items_replayed() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5",
            "input": [
                {"role": "user", "content": "hi"},
                {"type": "compaction", "id": "comp_1", "summary": "compacted"}
            ]
        });
        let ir = codec.decode_request(body, &env).unwrap();
        let (re, _) = codec.encode_request(&ir).unwrap();
        let input = re["input"].as_array().unwrap();
        let compaction = input
            .iter()
            .find(|i| i["type"] == "compaction")
            .expect("compaction item should be replayed in encode");
        assert_eq!(compaction["id"], "comp_1");
    }

    #[test]
    fn test_decode_client_metadata_passthrough() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5",
            "input": "hi",
            "client_metadata": {"session_id": "abc123", "version": "1.0"}
        });
        let ir = codec.decode_request(body, &env).unwrap();
        let extra = ir
            .extensions
            .get("responses_extra")
            .and_then(|v| v.as_object())
            .expect("responses_extra should exist");
        assert!(
            extra.contains_key("client_metadata"),
            "client_metadata should be in responses_extra"
        );
        assert_eq!(extra["client_metadata"]["session_id"], "abc123");
    }

    #[test]
    fn test_decode_reasoning_summary() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5",
            "input": "hi",
            "reasoning": {"effort": "high", "summary": "auto"}
        });
        let ir = codec.decode_request(body, &env).unwrap();
        let thinking = ir.params.thinking.as_ref().expect("thinking should be set");
        assert_eq!(thinking.summary.as_deref(), Some("auto"));
        // reasoning_full should also be stored for same-protocol replay
        let re_full = ir
            .extensions
            .get("reasoning_full")
            .expect("reasoning_full should be in extensions");
        assert_eq!(re_full["summary"], "auto");
    }

    #[test]
    fn test_encode_reasoning_summary() {
        let codec = ResponsesCodec::new();
        let ir = tiygate_core::IrRequest {
            model: "gpt-5".to_string(),
            system: None,
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::AnthropicMessages,
                "messages",
                "2023-06-01",
            ),
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "hi".to_string(),
                    annotations: None,
                }],
            }],
            tools: vec![],
            params: tiygate_core::GenerationParams {
                thinking: Some(tiygate_core::ThinkingConfig {
                    effort: Some(tiygate_core::ThinkingEffort::High),
                    summary: Some("auto".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            stream: false,
            response_format: None,
            metadata: None,
            extensions: Default::default(),
        };
        let (body, _) = codec.encode_request(&ir).unwrap();
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(
            body["reasoning"]["summary"], "auto",
            "summary should be written to body"
        );
    }

    #[test]
    fn test_encode_reasoning_full_replay() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5",
            "input": "hi",
            "reasoning": {"effort": "medium", "summary": "auto", "generate_summary": "detailed"}
        });
        let ir = codec.decode_request(body, &env).unwrap();
        let (re, _) = codec.encode_request(&ir).unwrap();
        // Same-protocol replay should use the full reasoning object
        assert_eq!(re["reasoning"]["effort"], "medium");
        assert_eq!(re["reasoning"]["summary"], "auto");
        assert_eq!(re["reasoning"]["generate_summary"], "detailed");
    }

    #[test]
    fn test_stream_decoder_codex_local_shell_call_finish_reason() {
        let mut dec = ResponsesStreamDecoder::new();
        // Simulate a Codex streaming response with a local_shell_call item
        dec.feed(r#"data: {"type":"response.created","response":{"id":"resp_1","object":"response","status":"in_progress"}}"#).unwrap();
        dec.feed(r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"local_shell_call","call_id":"call_shell_1","action":{"command":["ls"]}}}"#).unwrap();
        let parts = dec.feed(r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed"}}"#).unwrap();
        // The finish reason should be ToolCalls because saw_function_call was set
        let has_tool_calls_finish = parts.iter().any(|p| {
            matches!(
                p,
                StreamPart::Finish {
                    reason: FinishReason::ToolCalls
                }
            )
        });
        assert!(
            has_tool_calls_finish,
            "Codex local_shell_call stream should produce FinishReason::ToolCalls, got: {:?}",
            parts
        );
    }
}
