//! Google Gemini generateContent protocol codec.
//! Implements bidirectional conversion for Google's Gemini API.

use http::HeaderMap;
use serde_json::{json, Value};

use tiygate_core::{
    Content, EndpointCapabilities, EndpointCodec, ErrorClass, FinishReason, IrRequest, IrResponse,
    Message, ProtocolEndpoint, ProtocolSuite, RawEnvelope, Role, StreamDecoder, StreamEncoder,
    StreamPart, Tool, Usage,
};

/// Map an `ErrorClass` to the Google Gemini-native `error.status` string
/// (google.rpc.Code enum name).
fn error_status_for_class(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Transient => "INTERNAL",
        ErrorClass::RateLimited => "RESOURCE_EXHAUSTED",
        ErrorClass::Auth => "UNAUTHENTICATED",
        ErrorClass::BadRequest => "INVALID_ARGUMENT",
        ErrorClass::LossyOrCapability => "FAILED_PRECONDITION",
        ErrorClass::ModelNotFound => "NOT_FOUND",
        ErrorClass::DeadlineExceeded => "DEADLINE_EXCEEDED",
        ErrorClass::UpstreamExhausted => "UNAVAILABLE",
        ErrorClass::AuthMissing => "UNAUTHENTICATED",
        ErrorClass::AuthInvalid => "UNAUTHENTICATED",
        ErrorClass::AuthDisabled => "PERMISSION_DENIED",
        ErrorClass::Overloaded => "UNAVAILABLE",
    }
}

/// Maximum value for Gemini's `thinkingBudget` field (0–24576).
const GEMINI_THINKING_BUDGET_MAX: u32 = 24576;

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

/// Recover the Gemini function name for a tool result.
///
/// Chat Completions, Anthropic Messages, and OpenAI Responses usually carry
/// the function name on the assistant tool call, not on the tool result. Gemini
/// requires `functionResponse.name`, so recover it from the prior matching IR
/// `ToolCall` before encoding.
fn lookup_tool_call_name(messages: &[Message], tool_call_id: &str) -> Option<String> {
    if !tool_call_id.is_empty() {
        for msg in messages.iter().rev() {
            if !matches!(msg.role, Role::Assistant) {
                continue;
            }
            for content in &msg.content {
                if let Content::ToolCall { id, name, .. } = content {
                    if id == tool_call_id && !name.is_empty() {
                        return Some(name.clone());
                    }
                }
            }
        }
    }

    tool_call_id
        .strip_prefix("gemini_call_")
        .filter(|name| !name.is_empty())
        .map(String::from)
}

/// Sentinel value injected as `thoughtSignature` when a Gemini 3 model
/// response is replayed but the real signature was lost (e.g. cross-protocol
/// ingress that does not carry Gemini extensions). Matches the
/// `skip_thought_signature_validator` constant used by @ai-sdk/google.
const SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";

/// Heuristic check for Gemini 3 models, which require `thoughtSignature` on
/// `functionCall` parts. Model IDs like `gemini-3-*` or `gemini-3.*` trigger
/// sentinel injection when a real signature is unavailable.
fn is_gemini3_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("gemini-3")
}

/// Heuristic check for Gemini models that support `thinkingLevel`.
///
/// Official Gemini docs use `thinkingBudget` for Gemini 2.5 models and
/// `thinkingLevel` for Gemini 3+ models. The two fields must not be sent
/// together in one request.
fn supports_gemini_thinking_level(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    let Some(start) = lower.find("gemini-") else {
        return false;
    };
    let version = &lower[start + "gemini-".len()..];
    let major: String = version.chars().take_while(|c| c.is_ascii_digit()).collect();

    major
        .parse::<u32>()
        .map(|major| major >= 3)
        .unwrap_or(false)
}

/// Convert a JSON Schema to Gemini's OpenAPI 3.0 subset, mirroring
/// @ai-sdk/google's `convertJSONSchemaToOpenAPISchema`.
///
/// Key transformations:
/// - `type: ["x", "null"]` → `anyOf: [{type:"x"}]` + `nullable: true`
/// - `const: v` → `enum: [v]`
/// - Empty object schema (`{type:"object"}` with no properties) → `{type:"object"}`
/// - `anyOf` containing a `null` type → collapse to `nullable: true` on the
///   non-null schema (or `anyOf` of non-null schemas + `nullable: true`)
/// - Recursive handling of `properties`, `items`, `allOf`, `anyOf`, `oneOf`
fn convert_json_schema_to_openapi(schema: &Value) -> Value {
    match schema {
        Value::Null => Value::Null,
        // JSON Schema `true` means "any value"; `false` means "never valid".
        // Gemini's OpenAPI subset does not accept boolean schemas, so we
        // emit an empty schema for `true` (matches @ai-sdk/google).
        Value::Bool(true) => json!({}),
        Value::Bool(false) => json!({"not": {}}),
        Value::Object(obj) => {
            // Check for empty object schema: {type:"object"} with no
            // properties (or empty properties) and no additionalProperties.
            let is_empty_object = obj
                .get("type")
                .and_then(|v| v.as_str())
                .map(|t| t == "object")
                .unwrap_or(false)
                && obj
                    .get("properties")
                    .map(|p| p.as_object().map(|o| o.is_empty()).unwrap_or(true))
                    .unwrap_or(true)
                && !obj.contains_key("additionalProperties");
            if is_empty_object {
                if let Some(desc) = obj.get("description") {
                    return json!({"type": "object", "description": desc});
                }
                return json!({"type": "object"});
            }

            let mut result = serde_json::Map::new();

            // Pass through description, required, format, minLength.
            if let Some(desc) = obj.get("description") {
                result.insert("description".to_string(), desc.clone());
            }
            if let Some(req) = obj.get("required") {
                result.insert("required".to_string(), req.clone());
            }
            if let Some(fmt) = obj.get("format") {
                result.insert("format".to_string(), fmt.clone());
            }
            if let Some(min_len) = obj.get("minLength") {
                result.insert("minLength".to_string(), min_len.clone());
            }

            // const → enum: [const]
            if let Some(const_val) = obj.get("const") {
                result.insert("enum".to_string(), json!([const_val]));
            }

            // type handling: string, array of types, or absent
            if let Some(type_val) = obj.get("type") {
                if let Some(type_str) = type_val.as_str() {
                    result.insert("type".to_string(), json!(type_str));
                } else if let Some(type_arr) = type_val.as_array() {
                    let has_null = type_arr.iter().any(|t| t.as_str() == Some("null"));
                    let non_null: Vec<&Value> = type_arr
                        .iter()
                        .filter(|t| t.as_str() != Some("null"))
                        .collect();
                    if non_null.is_empty() {
                        result.insert("type".to_string(), json!("null"));
                    } else if non_null.len() == 1 {
                        result.insert("type".to_string(), non_null[0].clone());
                        if has_null {
                            result.insert("nullable".to_string(), json!(true));
                        }
                    } else {
                        let any_of: Vec<Value> =
                            non_null.iter().map(|t| json!({"type": t})).collect();
                        result.insert("anyOf".to_string(), json!(any_of));
                        if has_null {
                            result.insert("nullable".to_string(), json!(true));
                        }
                    }
                }
            }

            // enum (explicit)
            if let Some(enum_vals) = obj.get("enum") {
                result.insert("enum".to_string(), enum_vals.clone());
            }

            // properties (recursive)
            if let Some(props) = obj.get("properties").and_then(|v| v.as_object()) {
                let converted: serde_json::Map<String, Value> = props
                    .iter()
                    .map(|(k, v)| (k.clone(), convert_json_schema_to_openapi(v)))
                    .collect();
                result.insert("properties".to_string(), json!(converted));
            }

            // items (recursive)
            if let Some(items) = obj.get("items") {
                if let Some(items_arr) = items.as_array() {
                    let converted: Vec<Value> = items_arr
                        .iter()
                        .map(convert_json_schema_to_openapi)
                        .collect();
                    result.insert("items".to_string(), json!(converted));
                } else {
                    result.insert("items".to_string(), convert_json_schema_to_openapi(items));
                }
            }

            // allOf (recursive)
            if let Some(all_of) = obj.get("allOf").and_then(|v| v.as_array()) {
                let converted: Vec<Value> =
                    all_of.iter().map(convert_json_schema_to_openapi).collect();
                result.insert("allOf".to_string(), json!(converted));
            }

            // anyOf (recursive, with null-collapsing)
            if let Some(any_of) = obj.get("anyOf").and_then(|v| v.as_array()) {
                let has_null = any_of.iter().any(|s| {
                    s.as_object()
                        .and_then(|o| o.get("type"))
                        .and_then(|v| v.as_str())
                        == Some("null")
                });
                if has_null {
                    let non_null: Vec<&Value> = any_of
                        .iter()
                        .filter(|s| {
                            s.as_object()
                                .and_then(|o| o.get("type"))
                                .and_then(|v| v.as_str())
                                != Some("null")
                        })
                        .collect();
                    if non_null.len() == 1 {
                        let converted = convert_json_schema_to_openapi(non_null[0]);
                        if let Some(conv_obj) = converted.as_object() {
                            for (k, v) in conv_obj {
                                result.insert(k.clone(), v.clone());
                            }
                        }
                        result.insert("nullable".to_string(), json!(true));
                    } else {
                        let converted: Vec<Value> = non_null
                            .iter()
                            .map(|s| convert_json_schema_to_openapi(s))
                            .collect();
                        result.insert("anyOf".to_string(), json!(converted));
                        result.insert("nullable".to_string(), json!(true));
                    }
                } else {
                    let converted: Vec<Value> =
                        any_of.iter().map(convert_json_schema_to_openapi).collect();
                    result.insert("anyOf".to_string(), json!(converted));
                }
            }

            // oneOf (recursive)
            if let Some(one_of) = obj.get("oneOf").and_then(|v| v.as_array()) {
                let converted: Vec<Value> =
                    one_of.iter().map(convert_json_schema_to_openapi).collect();
                result.insert("oneOf".to_string(), json!(converted));
            }

            json!(result)
        }
        other => other.clone(),
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
                hosted_tools: false,
                programmatic_tool_calling: false,
                extended_reasoning: true,
                deterministic_seed: false,
                // Gemini supports tool_choice=required via
                // toolConfig.functionCallingConfig.mode=ANY, and specific
                // function via allowedFunctionNames. See §1 of matrix.
                tool_choice_required: true,
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
                                    encrypted_content: None,
                                });
                            }
                        } else if let Some(text) = part["text"].as_str() {
                            cp.push(Content::Text {
                                text: text.to_string(),
                                annotations: None,
                                prompt_cache_breakpoint: None,
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
                                call_id: None,
                                caller: None,
                                wire_type: None,
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
                                id: None,
                                caller: None,
                                wire_type: None,
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
                                prompt_cache_breakpoint: None,
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
                                prompt_cache_breakpoint: None,
                            });
                        }
                    }
                    cp
                } else {
                    vec![Content::Text {
                        text: String::new(),
                        annotations: None,
                        prompt_cache_breakpoint: None,
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
                                    ..Default::default()
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

        // Parse thinkingConfig from generationConfig.
        // Supports both the legacy numeric fields (thinkingBudget,
        // includeThoughts) and the newer thinkingLevel (Gemini 3.x).
        let thinking = gc.get("thinkingConfig").and_then(|tc| {
            let include_thoughts = tc["includeThoughts"].as_bool();
            let budget_tokens = tc["thinkingBudget"].as_u64().map(|v| v as u32);
            // Parse thinkingLevel → effort (Gemini 3.x).
            // Gemini supports minimal/low/medium/high (4 levels).
            let effort = tc["thinkingLevel"].as_str().and_then(|s| {
                use tiygate_core::ThinkingEffort;
                match s {
                    "none" => Some(ThinkingEffort::None),
                    "minimal" => Some(ThinkingEffort::Minimal),
                    "low" => Some(ThinkingEffort::Low),
                    "medium" => Some(ThinkingEffort::Medium),
                    "high" => Some(ThinkingEffort::High),
                    _ => None,
                }
            });
            // Derive display from include_thoughts for cross-protocol
            // consistency (Anthropic's display is the semantic equivalent
            // of Gemini's includeThoughts).
            let display = include_thoughts.map(|i| match i {
                true => tiygate_core::ThinkingDisplay::Summarized,
                false => tiygate_core::ThinkingDisplay::Omitted,
            });
            if include_thoughts.is_none() && budget_tokens.is_none() && effort.is_none() {
                None
            } else {
                Some(tiygate_core::ThinkingConfig {
                    include_thoughts,
                    budget_tokens,
                    effort,
                    display,
                    summary: None,
                    ..Default::default()
                })
            }
        });
        let params = if let Some(thinking) = thinking {
            tiygate_core::GenerationParams {
                thinking: Some(thinking),
                ..params
            }
        } else {
            params
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

        // Parse toolConfig.functionCallingConfig into the normalized
        // extensions["tool_choice"] format so cross-protocol targets can
        // interpret it. Gemini's mode mapping:
        //   AUTO  → "auto"
        //   NONE  → "none"
        //   ANY   → "required" (or specific function if allowedFunctionNames)
        if let Some(mode) = body["toolConfig"]["functionCallingConfig"]["mode"].as_str() {
            let allowed =
                body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"].as_array();
            let normalized = match mode {
                "AUTO" => Some(json!("auto")),
                "NONE" => Some(json!("none")),
                "ANY" => {
                    if let Some(names) = allowed {
                        // Specific function pinning: use the first name.
                        if let Some(first) = names.first().and_then(|n| n.as_str()) {
                            Some(json!({"type": "function", "function": {"name": first}}))
                        } else {
                            Some(json!("required"))
                        }
                    } else {
                        Some(json!("required"))
                    }
                }
                _ => None,
            };
            if let Some(n) = normalized {
                extensions.insert("tool_choice".to_string(), n);
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
            metadata: body.get("labels").and_then(|l| l.as_object()).map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            }),
            extensions,
        })
    }

    fn encode_response(&self, ir: &IrResponse) -> Result<Value, tiygate_core::Error> {
        let mut parts = Vec::new();
        for c in &ir.content {
            match c {
                Content::Text { text, .. } => parts.push(json!({"text": text})),
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
                    ..
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
            // IR prompt_tokens is cache-free; Gemini's promptTokenCount
            // includes both cache_read and cache_write. Re-add both so the
            // wire value matches Gemini's convention and stays consistent
            // with the streaming encoder.
            let cache_read = usage.cache_read_tokens.unwrap_or(0);
            let cache_write = usage.cache_write_tokens.unwrap_or(0);
            let prompt_for_gemini = usage.prompt_tokens + cache_read + cache_write;
            response["usageMetadata"] = json!({
                "promptTokenCount": prompt_for_gemini,
                "candidatesTokenCount": usage.completion_tokens,
                "totalTokenCount": prompt_for_gemini + usage.completion_tokens,
            });
            if let Some(rt) = usage.reasoning_tokens {
                response["usageMetadata"]["thoughtsTokenCount"] = json!(rt);
            }
            if cache_read > 0 {
                response["usageMetadata"]["cachedContentTokenCount"] = json!(cache_read);
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
        tiygate_core::protocol::structured_output::validate_response_format_for_target(
            ir.response_format.as_ref(),
            self.id(),
        )
        .map_err(|error| tiygate_core::Error::Codec(error.to_string()))?;

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
                    Content::Text { text, .. } => parts.push(json!({"text": text})),
                    Content::Reasoning { text, .. } => {
                        // Gemini standard reasoning format: text part flagged
                        // thought:true.
                        parts.push(json!({"text": text, "thought": true}));
                    }
                    Content::ToolCall {
                        id: _,
                        name,
                        arguments,
                        ..
                    } => {
                        let mut part = json!({"functionCall": {"name": name, "args": arguments}});
                        // Replay the next stashed thoughtSignature, if any.
                        if let Some(sig) = thought_signatures.get(sig_idx) {
                            part["thoughtSignature"] = sig.clone();
                            sig_idx += 1;
                        } else if is_gemini3_model(&ir.model) {
                            // Gemini 3 requires a thoughtSignature on every
                            // functionCall part. When the real signature was
                            // lost (e.g. cross-protocol ingress that does not
                            // carry Gemini extensions), inject the sentinel so
                            // the API does not reject with 400.
                            part["thoughtSignature"] = json!(SKIP_THOUGHT_SIGNATURE_VALIDATOR);
                            sig_idx += 1;
                        }
                        parts.push(part);
                    }
                    Content::ToolResult {
                        tool_call_id,
                        name,
                        content,
                        ..
                    } => {
                        let response_obj: Value =
                            serde_json::from_str(content).unwrap_or(json!({"output": content}));
                        let resolved_name = if name.is_empty() {
                            lookup_tool_call_name(&ir.messages, tool_call_id).ok_or_else(|| {
                                tiygate_core::Error::Codec(format!(
                                    "Gemini functionResponse.name is required but missing; no prior ToolCall matched tool_call_id '{tool_call_id}'"
                                ))
                            })?
                        } else {
                            name.clone()
                        };
                        parts.push(json!({
                            "functionResponse": {
                                "name": resolved_name,
                                "response": response_obj
                            }
                        }));
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
                    Content::Refusal { text, .. } => {
                        parts.push(json!({"text": text}));
                    }
                    Content::Program { .. } | Content::ProgramOutput { .. } => {
                        // Rejected by the cross-protocol lossy guard.
                    }
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
        // Thinking config: output thinkingConfig from params.thinking.
        //
        // Cross-protocol derivation:
        // - Gemini 3+ uses thinkingLevel (derive from budget_tokens when needed)
        // - Gemini 2.5 and older use thinkingBudget (derive from effort when needed)
        // - include_thoughts ← display (derived when include_thoughts is missing)
        //
        // Official Gemini docs state that thinkingLevel and thinkingBudget
        // must not be used together. Gemini supports minimal/low/medium/high
        // for thinkingLevel; XHigh/Max clamp to "high". Gemini's
        // thinkingBudget range is 0–24576, so derived budgets are clamped.
        if let Some(ref thinking) = ir.params.thinking {
            // Derive include_thoughts from display when not set.
            let include_thoughts = thinking.include_thoughts.or_else(|| {
                thinking.display.map(|d| match d {
                    tiygate_core::ThinkingDisplay::Summarized => true,
                    tiygate_core::ThinkingDisplay::Omitted => false,
                })
            });
            // Derive effort from budget_tokens when not set.
            let effort = thinking.effort.or_else(|| {
                thinking
                    .budget_tokens
                    .map(tiygate_core::ThinkingConfig::budget_to_effort)
            });

            let mut tc = json!({});
            if let Some(include) = include_thoughts {
                tc["includeThoughts"] = json!(include);
            }
            if supports_gemini_thinking_level(&ir.model) {
                if let Some(effort) = effort {
                    // Gemini supports minimal/low/medium/high; XHigh/Max clamp to "high".
                    tc["thinkingLevel"] = json!(match effort {
                        tiygate_core::ThinkingEffort::None => "minimal",
                        tiygate_core::ThinkingEffort::Minimal => "minimal",
                        tiygate_core::ThinkingEffort::Low => "low",
                        tiygate_core::ThinkingEffort::Medium => "medium",
                        tiygate_core::ThinkingEffort::High => "high",
                        tiygate_core::ThinkingEffort::XHigh => "high",
                        tiygate_core::ThinkingEffort::Max => "high",
                    });
                }
            } else if let Some(budget) = thinking.budget_tokens {
                tc["thinkingBudget"] = json!(budget.min(GEMINI_THINKING_BUDGET_MAX));
            } else if let Some(effort) = effort {
                let derived = tiygate_core::ThinkingConfig::effort_to_budget(effort);
                tc["thinkingBudget"] = json!(derived.min(GEMINI_THINKING_BUDGET_MAX));
            }
            if tc.as_object().map(|m| !m.is_empty()).unwrap_or(false) {
                gc["thinkingConfig"] = tc;
                has_gc = true;
            }
        }
        // Gemini structured output: responseSchema in generationConfig
        // https://ai.google.dev/gemini-api/docs/structured-output
        match &ir.response_format {
            Some(tiygate_core::ResponseFormat::JsonSchema { schema, .. }) => {
                gc["responseSchema"] = convert_json_schema_to_openapi(schema);
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
            let declarations: Vec<Value> = ir
                .tools
                .iter()
                .filter(|t| t.is_function())
                .map(|t| {
                    let params = t
                        .parameters
                        .as_ref()
                        .map(convert_json_schema_to_openapi)
                        .unwrap_or(Value::Null);
                    json!({"name": t.name, "description": t.description, "parameters": params})
                })
                .collect();
            if !declarations.is_empty() {
                body["tools"] = json!([{"functionDeclarations": declarations}]);
            }
        }

        // Emit toolConfig from IR extensions["tool_choice"].
        // Maps the normalized tool_choice forms to Gemini's
        // toolConfig.functionCallingConfig:
        //   "auto"     → {mode: "AUTO"}
        //   "none"     → {mode: "NONE"}
        //   "required" → {mode: "ANY"}
        //   {type:"function", function:{name:"x"}} → {mode: "ANY", allowedFunctionNames: ["x"]}
        //   {type:"function", name:"x"} (Responses variant) → same as above
        // Guarded by body.get("toolConfig").is_none() so a same-protocol
        // passthrough toolConfig (from gemini_top_level) takes priority.
        if body.get("toolConfig").is_none() {
            if let Some(tc) = ir.extensions.get("tool_choice") {
                let fcc = if let Some(s) = tc.as_str() {
                    match s {
                        "auto" => Some(json!({"mode": "AUTO"})),
                        "none" => Some(json!({"mode": "NONE"})),
                        "required" => Some(json!({"mode": "ANY"})),
                        _ => None,
                    }
                } else if let Some(obj) = tc.as_object() {
                    if obj.get("type").and_then(|v| v.as_str()) == Some("function") {
                        // Try nested function.name (Chat/Anthropic) first,
                        // then flat name (Responses).
                        let name = obj["function"]["name"]
                            .as_str()
                            .or_else(|| obj["name"].as_str())
                            .unwrap_or("");
                        Some(json!({
                            "mode": "ANY",
                            "allowedFunctionNames": [name]
                        }))
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(fcc) = fcc {
                    body["toolConfig"] = json!({"functionCallingConfig": fcc});
                }
            }
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
        // Metadata: output from ir.metadata as Gemini labels
        if let Some(ref metadata) = ir.metadata {
            if !metadata.is_empty() && body.get("labels").is_none() {
                let mut labels = serde_json::Map::new();
                for (k, v) in metadata {
                    labels.insert(k.clone(), json!(v));
                }
                body["labels"] = json!(labels);
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
                                        encrypted_content: None,
                                    });
                                }
                            } else if let Some(text) = part["text"].as_str() {
                                content.push(Content::Text {
                                    text: text.to_string(),
                                    annotations: None,
                                    prompt_cache_breakpoint: None,
                                });
                            } else if let Some(t) = part["thought"].as_str() {
                                content.push(Content::Reasoning {
                                    text: t.to_string(),
                                    signature: None,
                                    id: None,
                                    encrypted_content: None,
                                });
                            } else if let Some(t) = part["thought"]["text"].as_str() {
                                content.push(Content::Reasoning {
                                    text: t.to_string(),
                                    signature: None,
                                    id: None,
                                    encrypted_content: None,
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
                                    call_id: None,
                                    caller: None,
                                    wire_type: None,
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
        // Parse groundingMetadata from the first candidate and attach
        // grounding citations as annotations to text content.
        if let Some(grounding) = body["candidates"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|c| c.get("groundingMetadata"))
        {
            if !grounding.is_null() {
                // Store full groundingMetadata in extensions for round-trip
                extensions.insert("gemini_grounding_metadata".to_string(), grounding.clone());
                // Extract URL citations from groundingChunks
                let annotations: Vec<tiygate_core::Annotation> = grounding["groundingChunks"]
                    .as_array()
                    .map(|chunks| {
                        chunks
                            .iter()
                            .filter_map(|chunk| {
                                let web = chunk.get("web")?;
                                Some(tiygate_core::Annotation {
                                    kind: tiygate_core::AnnotationKind::UrlCitation,
                                    start_index: None,
                                    end_index: None,
                                    title: web["title"].as_str().map(String::from),
                                    url: web["uri"].as_str().map(String::from),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if !annotations.is_empty() {
                    // Attach annotations to the last text content block
                    for c in content.iter_mut() {
                        if let Content::Text {
                            annotations: ref mut a,
                            ..
                        } = c
                        {
                            if a.is_none() {
                                *a = Some(annotations.clone());
                            }
                        }
                    }
                }
            }
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
        // Flip STOP → ToolCalls when the response contains function calls.
        // Gemini always returns finishReason=STOP even when the model emits
        // tool calls; cross-protocol targets (OpenAI/Anthropic) need
        // FinishReason::ToolCalls so the client runs the tool instead of
        // stopping. Mirrors the streaming decoder's `saw_tool_calls` latch.
        let finish_reason = if finish_reason == Some(FinishReason::Stop)
            && content
                .iter()
                .any(|c| matches!(c, Content::ToolCall { .. }))
        {
            Some(FinishReason::ToolCalls)
        } else {
            finish_reason
        };
        // Populate stop_details on SAFETY finish reason
        let stop_details = if body["candidates"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|c| c["finishReason"].as_str())
            == Some("SAFETY")
        {
            Some(tiygate_core::ir::StopDetails {
                stop_reason: "safety".to_string(),
                kind: Some("safety".to_string()),
                ..Default::default()
            })
        } else {
            None
        };
        let usage = body
            .get("usageMetadata")
            .filter(|usage| {
                usage.is_object()
                    && (usage["promptTokenCount"].is_u64()
                        || usage["candidatesTokenCount"].is_u64()
                        || usage["totalTokenCount"].is_u64())
            })
            .map(|u| {
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
            stop_details,
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
            StreamPart::ReasoningDelta { text, .. } => format!(
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
            // PTC is Responses-only; no Gemini wire carrier.
            StreamPart::ProgramDelta { .. } | StreamPart::ProgramOutputDelta { .. } => {
                String::new()
            }
            StreamPart::Usage { usage } => {
                // IR prompt_tokens is cache-free; Gemini's promptTokenCount
                // includes both cache_read and cache_write. Re-add both so
                // streamed usage matches the non-stream encoder and
                // totalTokenCount stays consistent.
                let cache_read = usage.cache_read_tokens.unwrap_or(0);
                let cache_write = usage.cache_write_tokens.unwrap_or(0);
                let prompt_for_gemini = usage.prompt_tokens + cache_read + cache_write;
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
                    FinishReason::ToolCalls => "STOP",
                    FinishReason::Other(_) => "STOP",
                };
                format!(
                    "data: {}\n\n",
                    json!({"candidates": [{"finishReason": fr}]})
                )
            }
            StreamPart::Error {
                message,
                class,
                upstream_code: _,
            } => {
                let status = error_status_for_class(*class);
                format!(
                    "data: {}\n\n",
                    json!({"error": {"message": message, "status": status}})
                )
            }
            StreamPart::ResponseStarted { id } => {
                format!("data: {}\n\n", json!({"responseId": id}))
            }
            StreamPart::ResponseCompleted { id, .. } => {
                format!("data: {}\n\n", json!({"responseId": id, "done": true}))
            }
        };
        Ok(chunk.into_bytes())
    }
    fn encode_error(
        &mut self,
        message: &str,
        class: ErrorClass,
        _upstream_code: Option<&str>,
    ) -> Vec<u8> {
        let status = error_status_for_class(class);
        format!(
            "data: {}\n\n",
            json!({"error": {"message": message, "status": status}})
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
    /// Collected `thoughtSignature` values from `functionCall` parts during
    /// streaming. Propagated via `ResponseCompleted.extensions` so the
    /// pipeline can carry them into the next request's IR extensions for
    /// Gemini 3 multi-turn thought-signature replay.
    thought_signatures: Vec<Value>,
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
            thought_signatures: Vec::new(),
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
        if let Some(error) = event
            .get("error")
            .and_then(Value::as_object)
            .filter(|error| !error.is_empty())
        {
            let code = error.get("status").and_then(Value::as_str);
            let class = tiygate_core::classify_upstream_error(None, code);
            parts.push(StreamPart::Error {
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown")
                    .to_string(),
                class,
                upstream_code: code.map(String::from),
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
                                    id: None,
                                    encrypted_content: None,
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
                                    id: None,
                                    encrypted_content: None,
                                });
                            }
                        }
                        if let Some(fc) = p.get("functionCall") {
                            self.saw_tool_calls = true;
                            // Collect thoughtSignature for Gemini 3 multi-turn
                            // preservation (propagated via ResponseCompleted).
                            if let Some(sig) = p.get("thoughtSignature") {
                                self.thought_signatures.push(sig.clone());
                            }
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
                            let arguments = if fc.get("args").is_some_and(|args| !args.is_null()) {
                                serde_json::to_string(&fc["args"]).unwrap_or_default()
                            } else {
                                "{}".to_string()
                            };
                            parts.push(StreamPart::ToolCallDelta {
                                id,
                                name,
                                arguments,
                                wire_type: None,
                                item_id: None,
                                caller: None,
                            });
                        }
                    }
                }
                if let Some(fr) = c["finishReason"].as_str() {
                    let reason = match fr {
                        "STOP" => {
                            if self.saw_tool_calls {
                                FinishReason::ToolCalls
                            } else {
                                FinishReason::Stop
                            }
                        }
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
            let has_token_usage = usage.get("promptTokenCount").is_some()
                || usage.get("candidatesTokenCount").is_some()
                || usage.get("totalTokenCount").is_some()
                || usage.get("cachedContentTokenCount").is_some()
                || usage.get("thoughtsTokenCount").is_some();
            if !has_token_usage {
                return Ok(parts);
            }
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
            let mut extensions = std::collections::HashMap::new();
            if !self.thought_signatures.is_empty() {
                extensions.insert(
                    "gemini_thought_signatures".to_string(),
                    json!(std::mem::take(&mut self.thought_signatures)),
                );
            }
            Ok(vec![StreamPart::ResponseCompleted {
                id,
                status: "completed".to_string(),
                usage: None,
                extensions,
            }])
        } else {
            Ok(vec![])
        }
    }
}

inventory::submit! { tiygate_core::CodecRegistration { make: || Box::new(GeminiCodec::new()) } }

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn make_raw_env() -> RawEnvelope {
        RawEnvelope {
            method: "POST".to_string(),
            path: "/v1beta/models/gemini:generateContent".to_string(),
            headers: std::collections::HashMap::new(),
            body: None,
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
                annotations: None,
                prompt_cache_breakpoint: None,
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
        let err = encoder.encode_error("rate limit", ErrorClass::RateLimited, None);
        let s = String::from_utf8_lossy(&err);
        assert!(s.contains("error"));
        assert!(s.contains("rate limit"));
        // status must be the Gemini-native RESOURCE_EXHAUSTED, not "429"
        assert!(s.contains("\"status\":\"RESOURCE_EXHAUSTED\""));
        assert!(!s.contains("\"status\":\"429\""));
        assert!(!s.contains("\"status\":\"INTERNAL\""));
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
                id: None,
                encrypted_content: None,
            },
            StreamPart::ToolCallDelta {
                id: "t1".to_string(),
                name: Some("f".to_string()),
                arguments: "{}".to_string(),
                wire_type: None,
                item_id: None,
                caller: None,
            },
            StreamPart::ProgramDelta {
                id: "prog_1".to_string(),
                call_id: "call_prog_1".to_string(),
                code: "x".to_string(),
                fingerprint: "fp".to_string(),
            },
            StreamPart::ProgramOutputDelta {
                id: "progo_1".to_string(),
                call_id: "call_prog_1".to_string(),
                result: "ok".to_string(),
                status: "completed".to_string(),
            },
            StreamPart::Usage {
                usage: Usage::default(),
            },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
            StreamPart::Error {
                message: "e".to_string(),
                class: ErrorClass::Transient,
                upstream_code: Some("500".to_string()),
            },
            StreamPart::ResponseCompleted {
                id: "r1".to_string(),
                status: "ok".to_string(),
                usage: None,
                extensions: std::collections::HashMap::new(),
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
            metadata: None,
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
                    encrypted_content: None,
                },
                Content::Text {
                    text: "answer".to_string(),
                    annotations: None,
                    prompt_cache_breakpoint: None,
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
        assert!(matches!(&ir2.content[1], Content::Text { text , ..} if text == "answer"));
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
            wire_type: None,
            item_id: None,
            caller: None,
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
    fn test_stream_decoder_error_null_is_ignored() {
        let mut decoder = GeminiStreamDecoder::new();
        let parts = decoder
            .feed(
                r#"data: {"responseId":"r1","error":null,"candidates":[{"content":{"parts":[{"text":"ok"}]}}]}"#,
            )
            .unwrap();
        assert!(parts
            .iter()
            .any(|part| matches!(part, StreamPart::TextDelta { text } if text == "ok")));
        assert!(!parts
            .iter()
            .any(|part| matches!(part, StreamPart::Error { .. })));
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
                    ..
                } => Some((id.clone(), name.clone(), arguments.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(tc.0, "gemini_call_lookup");
        assert_eq!(tc.1.as_deref(), Some("lookup"));
        assert!(tc.2.contains("\"q\":\"x\""));
    }

    #[test]
    fn test_decode_response_stop_with_tool_calls_flips_to_tool_calls() {
        // Item 1: Non-streaming STOP + tool calls → ToolCalls.
        // Gemini always returns finishReason=STOP even when the model emits
        // tool calls. The decoder must flip to ToolCalls so cross-protocol
        // targets (OpenAI/Anthropic) signal the client to run the tool.
        let codec = GeminiCodec::new();
        let resp = json!({
            "candidates": [{
                "content": {"parts": [
                    {"functionCall": {"name": "get_weather", "args": {"city": "London"}}}
                ]},
                "finishReason": "STOP"
            }]
        });
        let ir = codec.decode_response(resp).unwrap();
        assert_eq!(ir.finish_reason, Some(FinishReason::ToolCalls));
    }

    #[test]
    fn test_decode_response_stop_without_tool_calls_stays_stop() {
        // Non-streaming STOP without tool calls must remain STOP.
        let codec = GeminiCodec::new();
        let resp = json!({
            "candidates": [{
                "content": {"parts": [{"text": "Hello!"}]},
                "finishReason": "STOP"
            }]
        });
        let ir = codec.decode_response(resp).unwrap();
        assert_eq!(ir.finish_reason, Some(FinishReason::Stop));
    }

    #[test]
    fn test_stream_thought_signature_propagation() {
        // Item 2: Streaming thoughtSignature propagation.
        // The stream decoder must collect thoughtSignature values from
        // functionCall parts and emit them via ResponseCompleted.extensions.
        let mut dec = GeminiStreamDecoder::new();
        let line = r#"data: {"responseId":"resp_1","candidates":[{"content":{"parts":[{"functionCall":{"name":"f","args":{}},"thoughtSignature":"sig_123"}]}}]}"#;
        let _ = dec.feed(line).unwrap();
        let parts = dec.finish().unwrap();
        let ext = parts
            .iter()
            .find_map(|p| match p {
                StreamPart::ResponseCompleted { extensions, .. } => Some(extensions.clone()),
                _ => None,
            })
            .expect("ResponseCompleted");
        assert_eq!(
            ext.get("gemini_thought_signatures"),
            Some(&json!(["sig_123"]))
        );
    }

    #[test]
    fn test_thought_signature_sentinel_injection() {
        // Item 3: Sentinel injection for Gemini 3 models.
        // When a ToolCall is replayed without a real thoughtSignature on a
        // Gemini 3 model, the sentinel "skip_thought_signature_validator"
        // must be injected.
        let codec = GeminiCodec::new();
        let req = IrRequest {
            model: "gemini-3-pro".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![Content::ToolCall {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    arguments: json!({"city": "London"}),
                    call_id: None,
                    caller: None,
                    wire_type: None,
                }],
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
            metadata: None,
            extensions: std::collections::HashMap::new(),
        };
        let (body, _h) = codec.encode_request(&req).unwrap();
        let part = &body["contents"][0]["parts"][0];
        assert_eq!(part["functionCall"]["name"], "get_weather");
        assert_eq!(part["thoughtSignature"], SKIP_THOUGHT_SIGNATURE_VALIDATOR);
    }

    #[test]
    fn test_thought_signature_no_sentinel_for_non_gemini3() {
        // Non-Gemini-3 models must NOT get the sentinel.
        let codec = GeminiCodec::new();
        let req = IrRequest {
            model: "gemini-2.0-flash".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![Content::ToolCall {
                    id: "call_1".to_string(),
                    name: "get_weather".to_string(),
                    arguments: json!({}),
                    call_id: None,
                    caller: None,
                    wire_type: None,
                }],
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
            metadata: None,
            extensions: std::collections::HashMap::new(),
        };
        let (body, _h) = codec.encode_request(&req).unwrap();
        let part = &body["contents"][0]["parts"][0];
        assert!(part.get("thoughtSignature").is_none());
    }

    #[test]
    fn test_json_schema_to_openapi_conversion() {
        // Item 4: JSON Schema → OpenAPI Schema conversion.
        // Verify key transformations match @ai-sdk/google's behavior.
        // const → enum
        let schema = json!({"const": "hello"});
        let result = convert_json_schema_to_openapi(&schema);
        assert_eq!(result["enum"], json!(["hello"]));
        // type: ["string", "null"] → type: "string" + nullable: true
        let schema = json!({"type": ["string", "null"]});
        let result = convert_json_schema_to_openapi(&schema);
        assert_eq!(result["type"], "string");
        assert_eq!(result["nullable"], true);
        // anyOf with null → nullable + merged schema
        let schema = json!({"anyOf": [{"type": "string"}, {"type": "null"}]});
        let result = convert_json_schema_to_openapi(&schema);
        assert_eq!(result["type"], "string");
        assert_eq!(result["nullable"], true);
        // Empty object schema
        let schema = json!({"type": "object"});
        let result = convert_json_schema_to_openapi(&schema);
        assert_eq!(result["type"], "object");
        // Properties recursion
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "const": "abc"},
                "age": {"type": ["integer", "null"]}
            },
            "required": ["name"]
        });
        let result = convert_json_schema_to_openapi(&schema);
        assert_eq!(result["type"], "object");
        assert_eq!(result["required"], json!(["name"]));
        assert_eq!(result["properties"]["name"]["enum"], json!(["abc"]));
        assert_eq!(result["properties"]["age"]["type"], "integer");
        assert_eq!(result["properties"]["age"]["nullable"], true);
    }

    #[test]
    fn test_encode_request_applies_schema_conversion_to_response_schema() {
        // Verify that encode_request applies convert_json_schema_to_openapi
        // to responseSchema.
        let codec = GeminiCodec::new();
        let req = IrRequest {
            model: "gemini-2.0-flash".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "hi".to_string(),
                    annotations: None,
                    prompt_cache_breakpoint: None,
                }],
            }],
            tools: vec![],
            params: Default::default(),
            response_format: Some(tiygate_core::ResponseFormat::JsonSchema {
                name: "response".to_string(),
                schema: json!({"type": "object", "properties": {"x": {"const": 42}}}),
                strict: None,
            }),
            stream: false,
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::GoogleGemini,
                "generateContent",
                "v1beta",
            ),
            metadata: None,
            extensions: std::collections::HashMap::new(),
        };
        let (body, _h) = codec.encode_request(&req).unwrap();
        // The const should have been converted to enum.
        assert_eq!(
            body["generationConfig"]["responseSchema"]["properties"]["x"]["enum"],
            json!([42])
        );
    }

    #[test]
    fn test_encode_request_applies_schema_conversion_to_tool_parameters() {
        // Verify that encode_request applies convert_json_schema_to_openapi
        // to functionDeclarations parameters.
        let codec = GeminiCodec::new();
        let req = IrRequest {
            model: "gemini-2.0-flash".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "hi".to_string(),
                    annotations: None,
                    prompt_cache_breakpoint: None,
                }],
            }],
            tools: vec![Tool {
                name: "get_weather".to_string(),
                description: Some("Get weather".to_string()),
                parameters: Some(json!({
                    "type": "object",
                    "properties": {
                        "city": {"type": "string", "const": "London"}
                    }
                })),
                required: false,
                ..Default::default()
            }],
            params: Default::default(),
            response_format: None,
            stream: false,
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::GoogleGemini,
                "generateContent",
                "v1beta",
            ),
            metadata: None,
            extensions: std::collections::HashMap::new(),
        };
        let (body, _h) = codec.encode_request(&req).unwrap();
        let params = &body["tools"][0]["functionDeclarations"][0]["parameters"];
        assert_eq!(params["properties"]["city"]["enum"], json!(["London"]));
    }

    #[test]
    fn test_tool_config_from_ir_extensions_auto() {
        // Item 5: toolConfig from IR extensions["tool_choice"].
        let codec = GeminiCodec::new();
        let mut extensions = std::collections::HashMap::new();
        extensions.insert("tool_choice".to_string(), json!("auto"));
        let req = IrRequest {
            model: "gemini-2.0-flash".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "hi".to_string(),
                    annotations: None,
                    prompt_cache_breakpoint: None,
                }],
            }],
            tools: vec![Tool {
                name: "f".to_string(),
                description: None,
                parameters: None,
                required: false,
                ..Default::default()
            }],
            params: Default::default(),
            response_format: None,
            stream: false,
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::GoogleGemini,
                "generateContent",
                "v1beta",
            ),
            metadata: None,
            extensions,
        };
        let (body, _h) = codec.encode_request(&req).unwrap();
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");
    }

    #[test]
    fn test_tool_config_from_ir_extensions_required() {
        let codec = GeminiCodec::new();
        let mut extensions = std::collections::HashMap::new();
        extensions.insert("tool_choice".to_string(), json!("required"));
        let req = IrRequest {
            model: "gemini-2.0-flash".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "hi".to_string(),
                    annotations: None,
                    prompt_cache_breakpoint: None,
                }],
            }],
            tools: vec![Tool {
                name: "f".to_string(),
                description: None,
                parameters: None,
                required: false,
                ..Default::default()
            }],
            params: Default::default(),
            response_format: None,
            stream: false,
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::GoogleGemini,
                "generateContent",
                "v1beta",
            ),
            metadata: None,
            extensions,
        };
        let (body, _h) = codec.encode_request(&req).unwrap();
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
    }

    #[test]
    fn test_tool_config_from_ir_extensions_specific_function() {
        let codec = GeminiCodec::new();
        let mut extensions = std::collections::HashMap::new();
        extensions.insert(
            "tool_choice".to_string(),
            json!({"type": "function", "function": {"name": "get_weather"}}),
        );
        let req = IrRequest {
            model: "gemini-2.0-flash".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "hi".to_string(),
                    annotations: None,
                    prompt_cache_breakpoint: None,
                }],
            }],
            tools: vec![Tool {
                name: "get_weather".to_string(),
                description: None,
                parameters: None,
                required: false,
                ..Default::default()
            }],
            params: Default::default(),
            response_format: None,
            stream: false,
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::GoogleGemini,
                "generateContent",
                "v1beta",
            ),
            metadata: None,
            extensions,
        };
        let (body, _h) = codec.encode_request(&req).unwrap();
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
        assert_eq!(
            body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0],
            "get_weather"
        );
    }

    #[test]
    fn test_decode_request_parses_tool_config_to_extensions() {
        // Verify that decode_request parses Gemini's toolConfig into
        // the normalized extensions["tool_choice"] format.
        let codec = GeminiCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "toolConfig": {"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["f"]}}
        });
        let ir = codec.decode_request(body, &env).unwrap();
        assert_eq!(
            ir.extensions.get("tool_choice"),
            Some(&json!({"type": "function", "function": {"name": "f"}}))
        );
    }
}
