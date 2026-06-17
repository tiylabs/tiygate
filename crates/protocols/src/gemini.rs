//! Google Gemini generateContent protocol codec.
//! Implements bidirectional conversion for Google's Gemini API.

use http::HeaderMap;
use serde_json::{json, Value};

use tiygate_core::{
    Content, EndpointCapabilities, EndpointCodec, FinishReason, IrRequest, IrResponse, Message,
    ProtocolEndpoint, ProtocolSuite, RawEnvelope, Role, StreamDecoder, StreamEncoder, StreamPart,
    Tool, Usage,
};

/// Synthesize a deterministic tool-call id for Gemini function calls.
///
/// Gemini's `functionCall` / `functionResponse` parts have no explicit call
/// id; they are paired by function name. To let cross-protocol targets
/// (OpenAI/Anthropic) pair a call with its result, we derive a stable id from
/// the name. Using a fixed prefix keeps it recognizable in logs.
fn synth_gemini_call_id(name: &str) -> String {
    if name.is_empty() {
        String::new()
    } else {
        format!("gemini_call_{name}")
    }
}

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
                        if part["thought"].as_bool() == Some(true) {
                            // Gemini standard reasoning: text flagged thought:true
                            if let Some(text) = part["text"].as_str() {
                                cp.push(Content::Reasoning {
                                    text: text.to_string(),
                                    signature: None,
                                    id: None,
                                });
                            }
                        } else if let Some(text) = part["text"].as_str() {
                            cp.push(Content::Text {
                                text: text.to_string(),
                            });
                        } else if let Some(fc) = part.get("functionCall") {
                            let name = fc["name"].as_str().unwrap_or("").to_string();
                            cp.push(Content::ToolCall {
                                // Prefer Gemini's native call id when present
                                // (Gemini 3); otherwise synthesize a
                                // deterministic id from the function name so
                                // cross-protocol targets (OpenAI/Anthropic) can
                                // pair the call with its result. Gemini itself
                                // pairs functionCall/functionResponse by name.
                                id: fc["id"]
                                    .as_str()
                                    .filter(|s| !s.is_empty())
                                    .map(String::from)
                                    .unwrap_or_else(|| synth_gemini_call_id(&name)),
                                name,
                                arguments: fc["args"].clone(),
                            });
                        } else if let Some(fr) = part.get("functionResponse") {
                            let name = fr["name"].as_str().unwrap_or("").to_string();
                            cp.push(Content::ToolResult {
                                tool_call_id: fr["id"]
                                    .as_str()
                                    .filter(|s| !s.is_empty())
                                    .map(String::from)
                                    .unwrap_or_else(|| synth_gemini_call_id(&name)),
                                name: name.clone(),
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

        // Parse inbound structured-output config. Gemini carries it in
        // generationConfig.responseSchema (+ responseMimeType=application/json).
        let response_format = if let Some(schema) = gc.get("responseSchema") {
            if !schema.is_null() {
                Some(tiygate_core::ResponseFormat::JsonSchema {
                    name: "response".to_string(),
                    schema: schema.clone(),
                    strict: None,
                })
            } else {
                None
            }
        } else if gc["responseMimeType"].as_str() == Some("application/json") {
            Some(tiygate_core::ResponseFormat::JsonObject)
        } else {
            None
        };

        // Preserve Google-specific top-level fields the IR does not model so a
        // same-protocol re-encode is lossless. Stored under a prefixed key.
        let mut extensions = std::collections::HashMap::new();
        {
            let mut extra = serde_json::Map::new();
            for key in ["safetySettings", "toolConfig", "cachedContent", "labels"] {
                if let Some(v) = body.get(key) {
                    extra.insert(key.to_string(), v.clone());
                }
            }
            if !extra.is_empty() {
                extensions.insert("gemini_top_level".to_string(), json!(extra));
            }
        }

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

    fn encode_response(&self, ir: &IrResponse) -> Result<Value, tiygate_core::Error> {
        let mut parts = Vec::new();
        for c in &ir.content {
            match c {
                Content::Text { text } => parts.push(json!({"text": text})),
                Content::Reasoning { text, .. } => {
                    // Gemini's standard "thought" format is a text part flagged
                    // with `thought: true`, not `{"thought": text}`. Emit the
                    // standard shape so re-decoding (and downstream Gemini
                    // consumers) recognize it as reasoning.
                    parts.push(json!({"text": text, "thought": true}))
                }
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
        // Gemini 3 requires the `thoughtSignature` collected on a prior
        // assistant turn to be replayed on the corresponding functionCall part
        // of the next request, or the API rejects with 400. We stashed them in
        // `extensions["gemini_thought_signatures"]` (in order) during decode;
        // replay them onto functionCall parts in the same order here.
        let thought_signatures: Vec<Value> = ir
            .extensions
            .get("gemini_thought_signatures")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut sig_idx = 0usize;
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
                    Content::Reasoning { text, .. } => {
                        // Gemini standard reasoning format: text part flagged
                        // thought:true.
                        parts.push(json!({"text": text, "thought": true}));
                    }
                    Content::ToolCall {
                        id: _,
                        name,
                        arguments,
                    } => {
                        let mut part = json!({"functionCall": {"name": name, "args": arguments}});
                        // Replay the next stashed thoughtSignature, if any.
                        if let Some(sig) = thought_signatures.get(sig_idx) {
                            part["thoughtSignature"] = sig.clone();
                            sig_idx += 1;
                        }
                        parts.push(part);
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

        // Replay Google-specific top-level fields captured at decode time.
        if let Some(extra) = ir
            .extensions
            .get("gemini_top_level")
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
                            // Gemini's standard reasoning is a text part flagged
                            // `thought: true`. Check that BEFORE the plain-text
                            // branch so a flagged part lands in Reasoning, not
                            // Text. Also tolerate the non-standard
                            // `{"thought": "..."}` / `{"thought": {"text": ".."}}`
                            // shapes some proxies emit.
                            if part["thought"].as_bool() == Some(true) {
                                if let Some(text) = part["text"].as_str() {
                                    content.push(Content::Reasoning {
                                        text: text.to_string(),
                                        signature: None,
                                        id: None,
                                    });
                                }
                            } else if let Some(text) = part["text"].as_str() {
                                content.push(Content::Text {
                                    text: text.to_string(),
                                });
                            } else if let Some(t) = part["thought"].as_str() {
                                content.push(Content::Reasoning {
                                    text: t.to_string(),
                                    signature: None,
                                    id: None,
                                });
                            } else if let Some(t) = part["thought"]["text"].as_str() {
                                content.push(Content::Reasoning {
                                    text: t.to_string(),
                                    signature: None,
                                    id: None,
                                });
                            } else if let Some(fc) = part.get("functionCall") {
                                let name = fc["name"].as_str().unwrap_or("").to_string();
                                let id = fc["id"]
                                    .as_str()
                                    .filter(|s| !s.is_empty())
                                    .map(String::from)
                                    .unwrap_or_else(|| synth_gemini_call_id(&name));
                                content.push(Content::ToolCall {
                                    id,
                                    name,
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
        let usage = body.get("usageMetadata").map(|u| {
            let cache_read = u["cachedContentTokenCount"].as_u64();
            // Gemini's promptTokenCount includes cached content; the IR keeps
            // prompt_tokens cache-free to avoid double-counting on re-encode.
            let raw_prompt = u["promptTokenCount"].as_u64().unwrap_or(0);
            Usage {
                prompt_tokens: raw_prompt.saturating_sub(cache_read.unwrap_or(0)),
                completion_tokens: u["candidatesTokenCount"].as_u64().unwrap_or(0),
                total_tokens: u["totalTokenCount"].as_u64().unwrap_or(0),
                reasoning_tokens: u["thoughtsTokenCount"].as_u64(),
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
            extensions,
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
                json!({"candidates": [{"content": {"parts": [{"text": text, "thought": true}]}}]})
            ),
            StreamPart::ToolCallDelta {
                name, arguments, ..
            } => {
                // Gemini's streaming `functionCall` parts carry both the name
                // and the full `args` object in a single chunk; there is no
                // incremental argument-delta channel like OpenAI's. Parse the
                // accumulated `arguments` string into a JSON object (falling
                // back to an empty object only when it is empty / unparseable)
                // so the call's arguments are not lost.
                let args: Value = if arguments.trim().is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(arguments).unwrap_or_else(|_| json!({}))
                };
                let mut fc = json!({ "args": args });
                if let Some(n) = name {
                    fc["name"] = json!(n);
                }
                format!(
                    "data: {}\n\n",
                    json!({"candidates": [{"content": {"parts": [{"functionCall": fc}]}}]})
                )
            }
            StreamPart::Usage { usage } => {
                // IR prompt_tokens is cache-free; Gemini's promptTokenCount
                // includes cached content. Re-add so streamed usage matches the
                // non-stream encoder and totalTokenCount stays consistent.
                let cache_read = usage.cache_read_tokens.unwrap_or(0);
                let prompt_for_gemini = usage.prompt_tokens + cache_read;
                let mut um = json!({
                    "promptTokenCount": prompt_for_gemini,
                    "candidatesTokenCount": usage.completion_tokens,
                    "totalTokenCount": prompt_for_gemini + usage.completion_tokens,
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
    /// Whether a real terminal signal (`candidates[].finishReason`) was seen
    /// in-band. Used to drive the `usageMetadata` completion fallback: some
    /// proxies strip `finishReason` and only deliver `usageMetadata` on the
    /// final chunk, so without this fallback the stream would carry no
    /// `Finish` and the cross-protocol ingress encoder would never emit its
    /// protocol-native terminator.
    saw_finish: bool,
    /// Whether any `functionCall` part appeared during this response. Latches
    /// for the whole stream so the `usageMetadata` completion fallback can be
    /// mapped to `FinishReason::ToolCalls` instead of `Stop` — otherwise a
    /// proxy that strips `finishReason` on a tool-call turn would make the
    /// cross-protocol encoder emit `finish_reason: "stop"` and the client
    /// would never run the tool.
    saw_tool_calls: bool,
}
impl Default for GeminiStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl GeminiStreamDecoder {
    pub fn new() -> Self {
        Self {
            response_id: None,
            saw_finish: false,
            saw_tool_calls: false,
        }
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
                        // Standard Gemini reasoning: text flagged thought:true.
                        // Route flagged parts to ReasoningDelta and skip the
                        // plain TextDelta branch for them. Also tolerate the
                        // non-standard `{"thought": "..."}` shapes.
                        if p["thought"].as_bool() == Some(true) {
                            if let Some(text) = p["text"].as_str() {
                                parts.push(StreamPart::ReasoningDelta {
                                    text: text.to_string(),
                                });
                            }
                        } else {
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
                        }
                        if let Some(fc) = p.get("functionCall") {
                            self.saw_tool_calls = true;
                            let name = fc["name"].as_str().map(String::from);
                            // Prefer Gemini's native call id when present; else
                            // synthesize a deterministic id from the name so a
                            // cross-protocol target can pair call/result.
                            let id = fc["id"]
                                .as_str()
                                .filter(|s| !s.is_empty())
                                .map(String::from)
                                .unwrap_or_else(|| {
                                    name.as_deref()
                                        .map(synth_gemini_call_id)
                                        .unwrap_or_default()
                                });
                            parts.push(StreamPart::ToolCallDelta {
                                id,
                                name,
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
                    self.saw_finish = true;
                }
            }
        }
        if let Some(usage) = event.get("usageMetadata") {
            let cache_read = usage["cachedContentTokenCount"].as_u64();
            let raw_prompt = usage["promptTokenCount"].as_u64().unwrap_or(0);
            parts.push(StreamPart::Usage {
                usage: Usage {
                    // promptTokenCount includes cache; IR keeps it cache-free.
                    prompt_tokens: raw_prompt.saturating_sub(cache_read.unwrap_or(0)),
                    completion_tokens: usage["candidatesTokenCount"].as_u64().unwrap_or(0),
                    total_tokens: usage["totalTokenCount"].as_u64().unwrap_or(0),
                    reasoning_tokens: usage["thoughtsTokenCount"].as_u64(),
                    cache_read_tokens: cache_read,
                    ..Default::default()
                },
            });
            // Completion fallback: Gemini has no protocol-native end frame,
            // and some proxies strip `candidates[].finishReason`, leaving
            // `usageMetadata` as the only terminal signal. When we see usage
            // but never saw a `finishReason`, synthesize a terminator so the
            // cross-protocol ingress encoder still emits one. Prefer
            // `ToolCalls` when a `functionCall` was seen — mapping a tool-call
            // turn to `Stop` would make the client stop instead of running the
            // tool. Mark `saw_finish` so a later `finish()` bridge does not
            // double-count.
            if !self.saw_finish {
                parts.push(StreamPart::Finish {
                    reason: if self.saw_tool_calls {
                        FinishReason::ToolCalls
                    } else {
                        FinishReason::Stop
                    },
                });
                self.saw_finish = true;
            }
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
    fn test_response_schema_and_top_level_extras_roundtrip() {
        // 中低影响回归:responseSchema 入站解析为 JsonSchema;safetySettings 等
        // 顶层字段保留进 extensions 并在 encode 回写。
        let codec = GeminiCodec::new();
        let env = make_raw_env();
        let schema = json!({"type": "object", "properties": {"x": {"type": "string"}}});
        let body = json!({
            "model": "models/gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "generationConfig": {"responseSchema": schema, "responseMimeType": "application/json"},
            "safetySettings": [{"category": "HARM", "threshold": "BLOCK_NONE"}]
        });
        let ir = codec.decode_request(body, &env).unwrap();
        assert!(matches!(
            ir.response_format,
            Some(tiygate_core::ResponseFormat::JsonSchema { .. })
        ));
        assert!(ir.extensions.contains_key("gemini_top_level"));
        let (out, _h) = codec.encode_request(&ir).unwrap();
        assert_eq!(out["generationConfig"]["responseSchema"]["type"], "object");
        assert!(out["safetySettings"].is_array());
    }

    #[test]
    fn test_thought_signature_replay() {
        // 高影响回归:decode_response 收集的 thoughtSignature 必须在
        // encode_request 时重放到对应 functionCall part(Gemini 3 多轮闭环)。
        let codec = GeminiCodec::new();
        // Decode a response carrying a functionCall + thoughtSignature.
        let resp = json!({
            "candidates": [{
                "content": {"parts": [
                    {"functionCall": {"name": "get_weather", "args": {"city": "London"}}, "thoughtSignature": "sig_abc"}
                ]},
                "finishReason": "STOP"
            }]
        });
        let mut ir = codec.decode_response(resp).unwrap();
        assert_eq!(
            ir.extensions["gemini_thought_signatures"],
            json!(["sig_abc"])
        );
        // Build a request IR replaying that tool call as an assistant turn.
        let req = IrRequest {
            model: "gemini-3".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::Assistant,
                content: std::mem::take(&mut ir.content),
            }],
            tools: vec![],
            params: Default::default(),
            response_format: None,
            stream: false,
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::GoogleGemini,
                "generateContent",
                "v1beta",
            ),
            extensions: ir.extensions.clone(),
        };
        let (body, _h) = codec.encode_request(&req).unwrap();
        let part = &body["contents"][0]["parts"][0];
        assert_eq!(part["functionCall"]["name"], "get_weather");
        assert_eq!(part["thoughtSignature"], "sig_abc");
    }

    #[test]
    fn test_thinking_standard_format_roundtrip() {
        // 高影响回归:Gemini reasoning 用标准 {"text",thought:true} 编码,
        // 且能被 decode 识别回 Reasoning(不混入 text)。
        let codec = GeminiCodec::new();
        let ir = IrResponse {
            content: vec![
                Content::Reasoning {
                    text: "thinking...".to_string(),
                    signature: None,
                    id: None,
                },
                Content::Text {
                    text: "answer".to_string(),
                },
            ],
            usage: None,
            finish_reason: Some(FinishReason::Stop),
            response_id: None,
            stop_details: None,
            extensions: Default::default(),
        };
        let encoded = codec.encode_response(&ir).unwrap();
        let parts = encoded["candidates"][0]["content"]["parts"]
            .as_array()
            .unwrap();
        // First part is reasoning with thought:true flag.
        assert_eq!(parts[0]["text"], "thinking...");
        assert_eq!(parts[0]["thought"], true);
        // Re-decode and confirm it returns to Reasoning.
        let ir2 = codec.decode_response(encoded).unwrap();
        assert!(
            matches!(&ir2.content[0], Content::Reasoning { text, .. } if text == "thinking...")
        );
        assert!(matches!(&ir2.content[1], Content::Text { text } if text == "answer"));
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
        // IR prompt_tokens (10) is cache-free; encoder re-adds cache (8) →
        // promptTokenCount 18, totalTokenCount 18+5=23.
        assert!(s.contains("\"promptTokenCount\":18"));
        assert!(s.contains("\"totalTokenCount\":23"));
        assert!(s.contains("\"thoughtsTokenCount\":20"));
        assert!(s.contains("\"cachedContentTokenCount\":8"));
    }

    #[test]
    fn test_stream_tool_call_args_preserved() {
        // 致命项1 回归:流式 functionCall 必须带完整 args,而非硬编码 {}。
        let mut enc = GeminiStreamEncoder;
        let part = StreamPart::ToolCallDelta {
            id: "gemini_call_get_weather".to_string(),
            name: Some("get_weather".to_string()),
            arguments: r#"{"city":"London","unit":"c"}"#.to_string(),
        };
        let bytes = enc.encode_part(&part).unwrap();
        let s = String::from_utf8_lossy(&bytes);
        let json_part = s.strip_prefix("data: ").unwrap().trim();
        let v: Value = serde_json::from_str(json_part).unwrap();
        let fc = &v["candidates"][0]["content"]["parts"][0]["functionCall"];
        assert_eq!(fc["name"], "get_weather");
        assert_eq!(fc["args"]["city"], "London");
        assert_eq!(fc["args"]["unit"], "c");
    }

    #[test]
    fn test_stream_decoder_tool_call_id_synthesized() {
        // 流式 decoder 应填充 ToolCallDelta.id(原生缺失时按 name 合成)。
        let mut dec = GeminiStreamDecoder::new();
        let line = r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"name":"lookup","args":{"q":"x"}}}]}}]}"#;
        let parts = dec.feed(line).unwrap();
        let tc = parts
            .iter()
            .find_map(|p| match p {
                StreamPart::ToolCallDelta {
                    id,
                    name,
                    arguments,
                } => Some((id.clone(), name.clone(), arguments.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(tc.0, "gemini_call_lookup");
        assert_eq!(tc.1.as_deref(), Some("lookup"));
        assert!(tc.2.contains("\"q\":\"x\""));
    }
}
