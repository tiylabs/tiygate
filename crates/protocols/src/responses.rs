//! OpenAI Responses API protocol codec.
//! Implements bidirectional conversion for OpenAI's Responses API.

use http::HeaderMap;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};

use tiygate_core::{
    Content, EndpointCapabilities, EndpointCodec, ErrorClass, FinishReason, IrRequest, IrResponse,
    Message, PromptCacheBreakpoint, ProtocolEndpoint, ProtocolSuite, RawEnvelope, Role,
    StreamDecoder, StreamEncoder, StreamPart, Tool, ToolCaller, Usage, Verbosity,
};

/// Map an `ErrorClass` to the OpenAI Responses-native `error.type` string.
/// Uses the same mapping as ChatCompletions (both are OpenAI family).
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
    caller: Option<&ToolCaller>,
) -> Value {
    let mut v = json!({"type": "function_call_output", "call_id": tool_call_id, "output": content});
    if let Some(id) = item_id {
        v["id"] = json!(id);
    }
    if let Some(caller) = caller {
        v["caller"] = json!(caller);
    }
    v
}

fn decode_tool_caller(item: &Value) -> Option<ToolCaller> {
    match item
        .get("caller")
        .and_then(|caller| caller["type"].as_str())
    {
        Some("direct") => Some(ToolCaller::Direct),
        Some("program") => {
            item["caller"]["caller_id"]
                .as_str()
                .map(|caller_id| ToolCaller::Program {
                    caller_id: caller_id.to_string(),
                })
        }
        _ => None,
    }
}

fn decode_prompt_cache_breakpoint(part: &Value) -> Option<PromptCacheBreakpoint> {
    (part["prompt_cache_breakpoint"]["mode"].as_str() == Some("explicit")).then_some(
        PromptCacheBreakpoint {
            mode: tiygate_core::PromptCacheBreakpointMode::Explicit,
        },
    )
}

/// Chat Completions labels custom calls as `custom`, while Responses uses
/// `custom_tool_call`. Both forms may enter the shared IR during a
/// cross-protocol conversion.
fn is_responses_custom_tool_call(wire_type: Option<&str>) -> bool {
    matches!(wire_type, Some("custom") | Some("custom_tool_call"))
}

/// Recover a custom tool's free-form input from either OpenAI protocol's IR
/// representation.
fn responses_custom_tool_input(arguments: &Value) -> String {
    arguments
        .get("input")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| arguments.as_str().map(String::from))
        .unwrap_or_else(|| arguments.to_string())
}

/// Map Responses `text.format` into the shared IR `response_format` so Chat ↔
/// Responses Convert can carry structured-output constraints both ways.
fn decode_text_format(text: &Value) -> Option<tiygate_core::ResponseFormat> {
    let format = text.get("format")?;
    match format["type"].as_str() {
        Some("json_schema") => {
            // Responses nests schema under `format` itself (name/schema/strict)
            // rather than under a `json_schema` wrapper like Chat Completions.
            let name = format["name"]
                .as_str()
                .or_else(|| format["json_schema"]["name"].as_str())
                .unwrap_or("response")
                .to_string();
            let schema = if format.get("schema").is_some_and(|s| !s.is_null()) {
                format["schema"].clone()
            } else if format
                .pointer("/json_schema/schema")
                .is_some_and(|s| !s.is_null())
            {
                format["json_schema"]["schema"].clone()
            } else {
                // Malformed json_schema without a real schema object — do not
                // promote a null/missing schema into IR.
                return None;
            };
            if !schema.is_object() {
                return None;
            }
            let strict = format["strict"]
                .as_bool()
                .or_else(|| format["json_schema"]["strict"].as_bool());
            Some(tiygate_core::ResponseFormat::JsonSchema {
                name,
                schema,
                strict,
            })
        }
        Some("json_object") => Some(tiygate_core::ResponseFormat::JsonObject),
        Some("text") => Some(tiygate_core::ResponseFormat::Text),
        _ => None,
    }
}

fn encode_text_format(format: &tiygate_core::ResponseFormat) -> Value {
    match format {
        tiygate_core::ResponseFormat::JsonSchema {
            name,
            schema,
            strict,
        } => {
            let mut obj = json!({
                "type": "json_schema",
                "name": name,
                "schema": schema,
            });
            if let Some(strict) = strict {
                obj["strict"] = json!(strict);
            }
            obj
        }
        tiygate_core::ResponseFormat::JsonObject => json!({"type": "json_object"}),
        tiygate_core::ResponseFormat::Text => json!({"type": "text"}),
    }
}

fn is_image_mime(mime_type: &str) -> bool {
    mime_type.starts_with("image/") || mime_type == "image/*" || mime_type.is_empty()
}

/// Encode IR media into a Responses content part. FileId sources map to
/// `input_image`/`input_file` with a top-level `file_id` field; Url/Inline
/// non-image media map to `input_file` with `file_url` / `file_data`.
fn encode_responses_media_part(
    source: &tiygate_core::ir::MediaSource,
    mime_type: &str,
    metadata: &std::collections::HashMap<String, Value>,
    prompt_cache_breakpoint: &Option<PromptCacheBreakpoint>,
) -> Option<Value> {
    let image = is_image_mime(mime_type);
    let mut part = match source {
        tiygate_core::ir::MediaSource::Url { url } => {
            if image {
                json!({
                    "type": "input_image",
                    "image_url": url,
                })
            } else {
                let mut obj = json!({
                    "type": "input_file",
                    "file_url": url,
                });
                if let Some(name) = metadata.get("filename").and_then(|v| v.as_str()) {
                    obj["filename"] = json!(name);
                }
                obj
            }
        }
        tiygate_core::ir::MediaSource::Inline { data } => {
            if image {
                json!({
                    "type": "input_image",
                    "image_url": format!("data:{mime_type};base64,{data}"),
                })
            } else {
                let mut obj = json!({
                    "type": "input_file",
                    "file_data": format!("data:{mime_type};base64,{data}"),
                });
                if let Some(name) = metadata.get("filename").and_then(|v| v.as_str()) {
                    obj["filename"] = json!(name);
                }
                obj
            }
        }
        tiygate_core::ir::MediaSource::FileId { id } => {
            if image {
                json!({
                    "type": "input_image",
                    "file_id": id,
                })
            } else {
                let mut obj = json!({
                    "type": "input_file",
                    "file_id": id,
                });
                if let Some(name) = metadata.get("filename").and_then(|v| v.as_str()) {
                    obj["filename"] = json!(name);
                }
                obj
            }
        }
    };
    if part["type"] == "input_image" {
        if let Some(detail) = metadata.get(tiygate_core::ir::IMAGE_DETAIL_KEY) {
            part["detail"] = detail.clone();
        }
    }
    if let Some(breakpoint) = prompt_cache_breakpoint {
        part["prompt_cache_breakpoint"] = json!(breakpoint);
    }
    Some(part)
}

/// Decode a Responses media content part (`input_image` / `input_file`) into IR.
fn decode_responses_media_part(part: &Value) -> Option<Content> {
    match part["type"].as_str() {
        Some("input_image") => {
            let detail = part["detail"]
                .as_str()
                .or_else(|| part["image_url"]["detail"].as_str());
            let mut metadata = std::collections::HashMap::<String, Value>::new();
            if let Some(d) = detail {
                metadata.insert(
                    tiygate_core::ir::IMAGE_DETAIL_KEY.to_string(),
                    Value::String(d.to_string()),
                );
            }
            if let Some(file_id) = part["file_id"].as_str().filter(|s| !s.is_empty()) {
                return Some(Content::Media {
                    source: tiygate_core::ir::MediaSource::FileId {
                        id: file_id.to_string(),
                    },
                    mime_type: "image/*".to_string(),
                    metadata,
                    prompt_cache_breakpoint: decode_prompt_cache_breakpoint(part),
                });
            }
            let raw_url = part["image_url"]
                .as_str()
                .or_else(|| part["image_url"]["url"].as_str())
                .unwrap_or_default();
            if raw_url.is_empty() {
                return None;
            }
            let (source, mime_type) =
                tiygate_core::ir::MediaSource::from_data_url(raw_url, "image/*");
            Some(Content::Media {
                source,
                mime_type,
                metadata,
                prompt_cache_breakpoint: decode_prompt_cache_breakpoint(part),
            })
        }
        Some("input_file") => {
            let mut metadata = std::collections::HashMap::<String, Value>::new();
            if let Some(name) = part["filename"].as_str().filter(|s| !s.is_empty()) {
                metadata.insert("filename".to_string(), Value::String(name.to_string()));
            }
            if let Some(file_id) = part["file_id"].as_str().filter(|s| !s.is_empty()) {
                return Some(Content::Media {
                    source: tiygate_core::ir::MediaSource::FileId {
                        id: file_id.to_string(),
                    },
                    mime_type: "application/octet-stream".to_string(),
                    metadata,
                    prompt_cache_breakpoint: decode_prompt_cache_breakpoint(part),
                });
            }
            if let Some(url) = part["file_url"].as_str().filter(|s| !s.is_empty()) {
                return Some(Content::Media {
                    source: tiygate_core::ir::MediaSource::Url {
                        url: url.to_string(),
                    },
                    mime_type: "application/octet-stream".to_string(),
                    metadata,
                    prompt_cache_breakpoint: decode_prompt_cache_breakpoint(part),
                });
            }
            if let Some(data) = part["file_data"].as_str().filter(|s| !s.is_empty()) {
                let (source, mime_type) =
                    tiygate_core::ir::MediaSource::from_data_url(data, "application/octet-stream");
                return Some(Content::Media {
                    source,
                    mime_type,
                    metadata,
                    prompt_cache_breakpoint: decode_prompt_cache_breakpoint(part),
                });
            }
            None
        }
        _ => None,
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
                hosted_tools: true,
                programmatic_tool_calling: true,
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
        // Ordered bag of Responses-only opaque input items (Codex + multi-agent).
        // Each entry is `{ "index": <original_input_index>, "item": <raw_json> }`
        // so same-protocol re-encode can restore original interleaving.
        let mut opaque_input_items: Vec<Value> = Vec::new();
        // Separate multi-agent presence bag for lossy rejection (content only).
        let mut multi_agent_items: Vec<Value> = Vec::new();

        if let Some(arr) = body["input"].as_array() {
            let mut call_id_counts: HashMap<String, usize> = HashMap::new();
            let mut call_id_remap: HashMap<String, VecDeque<String>> = HashMap::new();
            let mut used_call_ids: HashSet<String> = HashSet::new();
            // Once an opaque item is present, every modeled input item must
            // retain its own boundary. The opaque-item replay indexes refer to
            // the original input array, while coalescing consecutive messages
            // would reduce the modeled item count and shift later indexes.
            let preserve_input_item_boundaries = arr.iter().any(|item| {
                matches!(
                    item["type"].as_str(),
                    Some("tool_search_call")
                        | Some("tool_search_output")
                        | Some("agent_message")
                        | Some("compaction")
                        | Some("compaction_trigger")
                        | Some("context_compaction")
                        | Some("multi_agent_call")
                        | Some("multi_agent_call_output")
                )
            });
            // When an opaque item appears between two same-role messages,
            // keep those messages separate so ordered opaque replay can
            // reinsert the item at its original index.
            let mut break_role_merge = false;

            for (input_index, item) in arr.iter().enumerate() {
                // Responses typed items (function_call, function_call_output,
                // reasoning) do NOT carry a `role` field — their semantic role
                // is implied by the item type. Determine role from `type` first
                // so these items map to the correct IR roles for cross-protocol
                // conversion (e.g. function_call → Assistant, which Anthropic
                // requires for tool_use blocks).
                let role = match item["type"].as_str() {
                    Some("function_call")
                    | Some("program")
                    | Some("reasoning")
                    | Some("local_shell_call")
                    | Some("custom_tool_call") => Role::Assistant,
                    Some("function_call_output")
                    | Some("program_output")
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
                    // with its original input index for same-protocol replay.
                    // Cross-protocol egress drops these silently (no lossy
                    // rejection). Must be checked BEFORE the content-based
                    // branches because some opaque items (e.g. agent_message)
                    // carry a `content` field.
                    opaque_input_items.push(json!({
                        "index": input_index,
                        "item": item,
                    }));
                    vec![]
                } else if matches!(
                    item["type"].as_str(),
                    Some("multi_agent_call") | Some("multi_agent_call_output")
                ) {
                    // GPT-5.6 Multi-agent Beta items: keep raw JSON under a
                    // dedicated ordered extension so same-protocol re-encode
                    // restores original interleaving, and keep a content-only
                    // multi_agent_items bag so cross-protocol conversion can
                    // hard-reject (unlike codex opaque items which drop
                    // silently).
                    multi_agent_items.push(item.clone());
                    opaque_input_items.push(json!({
                        "index": input_index,
                        "item": item,
                    }));
                    vec![]
                } else if let Some(text) = item["content"].as_str() {
                    vec![Content::Text {
                        text: text.to_string(),
                        annotations: None,
                        prompt_cache_breakpoint: None,
                    }]
                } else if let Some(content_arr) = item["content"].as_array() {
                    let mut parts = Vec::new();
                    for part in content_arr {
                        match part["type"].as_str() {
                            Some("input_text") | Some("output_text") => {
                                parts.push(Content::Text {
                                    text: part["text"].as_str().unwrap_or("").to_string(),
                                    annotations: None,
                                    prompt_cache_breakpoint: decode_prompt_cache_breakpoint(part),
                                });
                            }
                            Some("input_image") | Some("input_file") => {
                                if let Some(media) = decode_responses_media_part(part) {
                                    parts.push(media);
                                }
                            }
                            _ => {}
                        }
                    }
                    parts
                } else if item["type"] == "program" {
                    vec![Content::Program {
                        id: item["id"].as_str().unwrap_or("").to_string(),
                        call_id: item["call_id"].as_str().unwrap_or("").to_string(),
                        code: item["code"].as_str().unwrap_or("").to_string(),
                        fingerprint: item["fingerprint"].as_str().unwrap_or("").to_string(),
                    }]
                } else if item["type"] == "program_output" {
                    vec![Content::ProgramOutput {
                        id: item["id"].as_str().unwrap_or("").to_string(),
                        call_id: item["call_id"].as_str().unwrap_or("").to_string(),
                        result: item["result"].as_str().unwrap_or("").to_string(),
                        status: item["status"].as_str().unwrap_or("completed").to_string(),
                    }]
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
                    // `id` and the *normalized unique* call_id in IR
                    // `call_id`. Cross-protocol encoders (Anthropic/Chat)
                    // pair tool_use/tool_result via
                    // `call_id.as_deref().unwrap_or(id)`, so the unique
                    // call_id must live in IR.call_id (and in the remap
                    // queue for following outputs). Using the raw
                    // (possibly duplicated) call_id here would leave
                    // ToolCall and ToolResult with mismatched pairing ids.
                    let (ir_id, ir_call_id) = if item_ref.is_some() && original_call_id.is_some() {
                        // IR `id` = item reference (e.g. `fc_xxx`)
                        // IR `call_id` = unique function-call id
                        (item_ref.clone().unwrap_or_default(), Some(id))
                    } else {
                        (id, None)
                    };

                    vec![Content::ToolCall {
                        id: ir_id,
                        name: item["name"].as_str().unwrap_or("").to_string(),
                        arguments: serde_json::from_str(item["arguments"].as_str().unwrap_or("{}"))
                            .unwrap_or(json!({})),
                        call_id: ir_call_id,
                        caller: decode_tool_caller(item),
                        wire_type: None,
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
                        caller: decode_tool_caller(item),
                        wire_type: None,
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
                        caller: None,
                        wire_type: Some("local_shell_call".to_string()),
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
                        caller: None,
                        wire_type: Some("local_shell_call_output".to_string()),
                    }]
                } else if item["type"] == "custom_tool_call" {
                    // Codex custom_tool_call: map to a ToolCall with the
                    // tool name and input text wrapped as JSON arguments.
                    // Preserve wire_type so same-protocol re-encode restores
                    // `custom_tool_call` rather than function_call.
                    let id = responses_call_id(item).unwrap_or("").to_string();
                    let name = item["name"].as_str().unwrap_or("").to_string();
                    let input_text = item["input"].as_str().unwrap_or("").to_string();
                    vec![Content::ToolCall {
                        id: id.clone(),
                        call_id: Some(id),
                        name,
                        arguments: json!({"input": input_text}),
                        caller: None,
                        wire_type: Some("custom_tool_call".to_string()),
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
                        caller: None,
                        wire_type: Some("custom_tool_call_output".to_string()),
                    }]
                } else {
                    vec![Content::Text {
                        text: String::new(),
                        annotations: None,
                        prompt_cache_breakpoint: None,
                    }]
                };
                // Merge consecutive items with the same role into one Message
                // so that e.g. a reasoning item followed by function_call items
                // end up in the same IR assistant turn. This is critical for
                // cross-protocol conversion: the Chat Completions encoder gates
                // reasoning_content on the presence of tool_calls *within the
                // same message*, so splitting them would silently drop reasoning.
                //
                // Exception: when an opaque Codex / multi-agent item sits between
                // two same-role messages, do NOT merge them — otherwise the
                // original input interleaving cannot be restored on re-encode.
                if content.is_empty() {
                    // Opaque Codex / multi-agent items produce no IR content;
                    // skip message creation and force the next same-role item
                    // to start a new message so ordered opaque replay stays
                    // index-aligned with modeled items.
                    break_role_merge = true;
                    continue;
                }
                if let Some(last) = messages.last_mut() {
                    if last.role == role && !break_role_merge && !preserve_input_item_boundaries {
                        last.content.extend(content);
                    } else {
                        messages.push(Message { role, content });
                    }
                } else {
                    messages.push(Message { role, content });
                }
                break_role_merge = false;
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
                    prompt_cache_breakpoint: None,
                }],
            });
        }

        let tools: Vec<Tool> = body["tools"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|t| {
                        let tool_type = t["type"].as_str().map(String::from);
                        match tool_type.as_deref() {
                            None | Some("function") => {
                                let mut config = t.as_object().cloned().unwrap_or_default();
                                for key in ["type", "name", "description", "parameters"] {
                                    config.remove(key);
                                }
                                Tool {
                                    name: t["name"].as_str().unwrap_or("").to_string(),
                                    description: t["description"].as_str().map(String::from),
                                    parameters: t["parameters"].as_object().map(|p| json!(p)),
                                    required: false,
                                    tool_type: if tool_type.as_deref() == Some("function") {
                                        Some("function".to_string())
                                    } else {
                                        None
                                    },
                                    config: (!config.is_empty()).then_some(Value::Object(config)),
                                }
                            }
                            Some("custom") => {
                                // OpenAI custom tools are first-class on Chat and
                                // Responses (not hosted tools). Keep remaining
                                // non-standard fields in config for round-trip.
                                let mut config = serde_json::Map::new();
                                if let Some(obj) = t.as_object() {
                                    for (k, v) in obj {
                                        if k != "type"
                                            && k != "name"
                                            && k != "description"
                                            && k != "parameters"
                                        {
                                            config.insert(k.clone(), v.clone());
                                        }
                                    }
                                }
                                Tool {
                                    name: t["name"].as_str().unwrap_or("").to_string(),
                                    description: t["description"].as_str().map(String::from),
                                    parameters: t.get("parameters").cloned(),
                                    required: false,
                                    tool_type: Some("custom".to_string()),
                                    config: if config.is_empty() {
                                        None
                                    } else {
                                        Some(Value::Object(config))
                                    },
                                }
                            }
                            _ => {
                                // Hosted tools: preserve type + remaining fields.
                                let mut config = t.as_object().cloned().unwrap_or_default();
                                config.remove("type");
                                let name = config
                                    .remove("name")
                                    .and_then(|v| v.as_str().map(String::from))
                                    .unwrap_or_default();
                                let description = config
                                    .remove("description")
                                    .and_then(|v| v.as_str().map(String::from));
                                let parameters = config.remove("parameters");
                                Tool {
                                    name,
                                    description,
                                    parameters,
                                    required: false,
                                    tool_type,
                                    config: if config.is_empty() {
                                        None
                                    } else {
                                        Some(Value::Object(config))
                                    },
                                }
                            }
                        }
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
                let effort = r.get("effort").and_then(|v| v.as_str()).and_then(|s| {
                    use tiygate_core::ThinkingEffort;
                    match s {
                        "none" => Some(ThinkingEffort::None),
                        "minimal" => Some(ThinkingEffort::Minimal),
                        "low" => Some(ThinkingEffort::Low),
                        "medium" => Some(ThinkingEffort::Medium),
                        "high" => Some(ThinkingEffort::High),
                        "xhigh" => Some(ThinkingEffort::XHigh),
                        "max" => Some(ThinkingEffort::Max),
                        _ => None,
                    }
                });
                let summary = r.get("summary").and_then(|v| v.as_str()).map(String::from);
                let mode = r.get("mode").and_then(|v| v.as_str()).map(String::from);
                let context = r.get("context").cloned();
                if effort.is_none() && summary.is_none() && mode.is_none() && context.is_none() {
                    None
                } else {
                    Some(tiygate_core::ThinkingConfig {
                        effort,
                        summary,
                        mode,
                        context,
                        ..Default::default()
                    })
                }
            }),
            verbosity: body["text"]["verbosity"]
                .as_str()
                .and_then(|value| match value {
                    "low" => Some(Verbosity::Low),
                    "medium" => Some(Verbosity::Medium),
                    "high" => Some(Verbosity::High),
                    _ => None,
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

        if let Some(safety_identifier) = body.get("safety_identifier") {
            extensions.insert(
                "openai.safety_identifier".to_string(),
                safety_identifier.clone(),
            );
        }
        if let Some(prompt_cache_options) = body.get("prompt_cache_options") {
            extensions.insert(
                "openai.prompt_cache_options".to_string(),
                prompt_cache_options.clone(),
            );
        }
        // Shared OpenAI cache-key extensions so Chat ↔ Responses conversion
        // preserves prompt_cache_key / prompt_cache_retention.
        if let Some(value) = body.get("prompt_cache_key") {
            extensions.insert("openai.prompt_cache_key".to_string(), value.clone());
        }
        if let Some(value) = body.get("prompt_cache_retention") {
            extensions.insert("openai.prompt_cache_retention".to_string(), value.clone());
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
                "prompt_cache_options",
                "client_metadata",
                "multi_agent",
            ] {
                if let Some(v) = body.get(key) {
                    extra.insert(key.to_string(), v.clone());
                }
            }
            if !extra.is_empty() {
                extensions.insert("responses_extra".to_string(), json!(extra));
            }
        }

        // Ordered opaque input items (Codex + multi-agent) for same-protocol
        // re-encode that preserves original input interleaving. Cross-protocol
        // egress drops these silently unless multi-agent hard-reject fires.
        if !opaque_input_items.is_empty() {
            extensions.insert(
                "responses_opaque_input_items".to_string(),
                json!(opaque_input_items),
            );
        }
        // Backward-compatible Codex bag (content only, no indices). Prefer the
        // ordered extension on encode; this remains for older IR snapshots.
        let codex_opaque_items: Vec<Value> = opaque_input_items
            .iter()
            .filter_map(|entry| {
                let item = entry.get("item")?;
                match item.get("type").and_then(|v| v.as_str()) {
                    Some("tool_search_call")
                    | Some("tool_search_output")
                    | Some("agent_message")
                    | Some("compaction")
                    | Some("compaction_trigger")
                    | Some("context_compaction") => Some(item.clone()),
                    _ => None,
                }
            })
            .collect();
        if !codex_opaque_items.is_empty() {
            extensions.insert("codex_opaque_items".to_string(), json!(codex_opaque_items));
        }

        // GPT-5.6 Multi-agent Beta input items (content only) so lossy guard
        // can hard-reject cross-protocol conversion.
        if !multi_agent_items.is_empty() {
            extensions.insert("multi_agent_items".to_string(), json!(multi_agent_items));
        }

        // Promote `text.format` into IR response_format so Chat ↔ Responses
        // Convert can rehydrate structured-output constraints. The full `text`
        // object remains in extensions for same-protocol fidelity.
        let response_format = body.get("text").and_then(decode_text_format);

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

    fn encode_response(&self, ir: &IrResponse) -> Result<Value, tiygate_core::Error> {
        let mut response = json!({"object": "response", "model": ""});
        if let Some(id) = &ir.response_id {
            response["id"] = json!(id);
        }
        let mut output_items = Vec::new();
        let mut pending_text = String::new();
        let flush_text = |pending: &mut String, output: &mut Vec<Value>| {
            if pending.is_empty() {
                return;
            }
            let index = output.len();
            output.push(json!({
                "id": format!("{}_msg_{index}", ir.response_id.as_deref().unwrap_or("msg")),
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": std::mem::take(pending)}]
            }));
        };
        for c in &ir.content {
            match c {
                Content::Text { text, .. } => {
                    pending_text.push_str(text);
                }
                Content::Reasoning {
                    text,
                    id,
                    encrypted_content,
                    ..
                } => {
                    flush_text(&mut pending_text, &mut output_items);
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
                    caller,
                    wire_type,
                } => {
                    flush_text(&mut pending_text, &mut output_items);
                    // Use `call_id` when available (Responses round-trip),
                    // otherwise fall back to `id` (cross-protocol).
                    let wire_call_id = call_id.as_deref().unwrap_or(id);
                    let item_type = wire_type.as_deref().unwrap_or("function_call");
                    let mut tc = if is_responses_custom_tool_call(wire_type.as_deref()) {
                        let input = responses_custom_tool_input(arguments);
                        json!({
                            "type": "custom_tool_call",
                            "call_id": wire_call_id,
                            "name": name,
                            "input": input,
                            "status": "completed"
                        })
                    } else if item_type == "local_shell_call" {
                        json!({
                            "type": "local_shell_call",
                            "call_id": wire_call_id,
                            "action": arguments,
                            "status": "completed"
                        })
                    } else {
                        json!({
                            "type": "function_call",
                            "call_id": wire_call_id,
                            "name": name,
                            "arguments": serde_json::to_string(arguments).unwrap_or_default(),
                            "status": "completed"
                        })
                    };
                    // Include the item reference `id` when available.
                    if call_id.is_some() {
                        tc["id"] = json!(id);
                    }
                    if let Some(caller) = caller {
                        tc["caller"] = json!(caller);
                    }
                    output_items.push(tc);
                }
                Content::Program {
                    id,
                    call_id,
                    code,
                    fingerprint,
                } => {
                    flush_text(&mut pending_text, &mut output_items);
                    output_items.push(json!({
                        "type": "program",
                        "id": id,
                        "call_id": call_id,
                        "code": code,
                        "fingerprint": fingerprint,
                    }));
                }
                Content::ProgramOutput {
                    id,
                    call_id,
                    result,
                    status,
                } => {
                    flush_text(&mut pending_text, &mut output_items);
                    output_items.push(json!({
                        "type": "program_output",
                        "id": id,
                        "call_id": call_id,
                        "result": result,
                        "status": status,
                    }));
                }
                Content::Refusal { text, .. } => {
                    flush_text(&mut pending_text, &mut output_items);
                    output_items.push(json!({"type": "refusal", "refusal": text}));
                }
                _ => {}
            }
        }
        flush_text(&mut pending_text, &mut output_items);
        if let Some(original_output) = ir
            .extensions
            .get("responses_original_output")
            .and_then(|value| value.as_array())
        {
            // Opaque output items can be interleaved with several modeled
            // message items. `IrResponse::content` intentionally flattens
            // those messages, so rebuilding then inserting opaque items by
            // their old indexes can move later text ahead of a hosted-tool
            // result. Same-protocol replay uses this source snapshot to retain
            // the exact item order and item boundaries.
            response["output"] = json!(original_output);
        } else {
            response["output"] = json!(output_items);
        }
        if let Some(fr) = &ir.finish_reason {
            response["status"] = json!(match fr {
                FinishReason::Stop => "completed",
                FinishReason::Length => "incomplete",
                FinishReason::ContentFilter => "incomplete",
                // OpenAI Responses reports `status: "completed"` for tool-call
                // turns.  The streaming encoder already maps ToolCalls →
                // "completed"; align the non-streaming encoder so cross-protocol
                // round-trips preserve the status.
                FinishReason::ToolCalls => "completed",
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
            let mut input_details = serde_json::Map::new();
            if cache_read > 0 {
                input_details.insert("cached_tokens".to_string(), json!(cache_read));
            }
            if cache_write > 0 {
                input_details.insert("cache_write_tokens".to_string(), json!(cache_write));
            }
            if !input_details.is_empty() {
                response["usage"]["input_tokens_details"] = json!(input_details);
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
        tiygate_core::protocol::structured_output::validate_response_format_for_target(
            ir.response_format.as_ref(),
            self.id(),
        )
        .map_err(|error| tiygate_core::Error::Codec(error.to_string()))?;

        let mut body = json!({"model": ir.model, "stream": ir.stream});
        if let Some(sys) = &ir.system {
            body["instructions"] = json!(sys);
        }
        let mut input_items = Vec::new();
        // Opaque input entries are indexed against the original Responses
        // input array. Keep developer messages as input items in this mode;
        // flattening them into `instructions` would remove an indexed item and
        // shift every opaque entry that follows it.
        let preserve_input_item_boundaries = ir
            .extensions
            .get("responses_opaque_input_items")
            .and_then(|value| value.as_array())
            .is_some_and(|items| !items.is_empty());
        for msg in &ir.messages {
            match msg.role {
                Role::System => {
                    let has_breakpoint = msg.content.iter().any(|content| {
                        matches!(
                            content,
                            Content::Text {
                                prompt_cache_breakpoint: Some(_),
                                ..
                            } | Content::Media {
                                prompt_cache_breakpoint: Some(_),
                                ..
                            }
                        )
                    });
                    if has_breakpoint || preserve_input_item_boundaries {
                        let mut content_parts = Vec::new();
                        for content in &msg.content {
                            match content {
                                Content::Text {
                                    text,
                                    prompt_cache_breakpoint,
                                    ..
                                } => {
                                    let mut part = json!({"type": "input_text", "text": text});
                                    if let Some(breakpoint) = prompt_cache_breakpoint {
                                        part["prompt_cache_breakpoint"] = json!(breakpoint);
                                    }
                                    content_parts.push(part);
                                }
                                Content::Media {
                                    source,
                                    mime_type,
                                    metadata,
                                    prompt_cache_breakpoint,
                                } => {
                                    if let Some(part) = encode_responses_media_part(
                                        source,
                                        mime_type,
                                        metadata,
                                        prompt_cache_breakpoint,
                                    ) {
                                        content_parts.push(part);
                                    }
                                }
                                _ => {}
                            }
                        }
                        if !content_parts.is_empty() {
                            input_items.push(json!({
                                "role": "developer",
                                "content": content_parts,
                            }));
                        }
                    } else {
                        for c in &msg.content {
                            if let Content::Text { text, .. } = c {
                                let existing = body["instructions"].as_str().unwrap_or("");
                                body["instructions"] = json!(format!("{existing}\n{text}"));
                            }
                        }
                    }
                }
                Role::User | Role::Assistant => {
                    let role_str = match msg.role {
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        _ => "user",
                    };
                    // Walk content in original order so PTC interleaving
                    // (reasoning / program / function_call / text / outputs)
                    // is preserved. Text and media parts are buffered and
                    // flushed as a single message item when a non-text item
                    // arrives or the turn ends.
                    let mut text_parts: Vec<Value> = Vec::new();

                    let flush_text_message =
                        |text_parts: &mut Vec<Value>,
                         input_items: &mut Vec<Value>,
                         role_str: &str| {
                            if text_parts.is_empty() {
                                return;
                            }
                            let parts = std::mem::take(text_parts);
                            let mut item = json!({"role": role_str});
                            if parts.len() == 1
                                && parts[0]
                                    .get("type")
                                    .map(|v| v == "input_text")
                                    .unwrap_or(false)
                                && parts[0].get("prompt_cache_breakpoint").is_none()
                            {
                                item["content"] = parts[0]["text"].clone();
                            } else {
                                item["content"] = json!(parts);
                            }
                            input_items.push(item);
                        };

                    for c in &msg.content {
                        match c {
                            Content::Text {
                                text,
                                prompt_cache_breakpoint,
                                ..
                            } => {
                                let mut part = json!({"type": "input_text", "text": text});
                                if let Some(breakpoint) = prompt_cache_breakpoint {
                                    part["prompt_cache_breakpoint"] = json!(breakpoint);
                                }
                                text_parts.push(part);
                            }
                            Content::Media {
                                source,
                                mime_type,
                                metadata,
                                prompt_cache_breakpoint,
                            } => {
                                if let Some(part) = encode_responses_media_part(
                                    source,
                                    mime_type,
                                    metadata,
                                    prompt_cache_breakpoint,
                                ) {
                                    text_parts.push(part);
                                }
                            }
                            Content::Reasoning {
                                text,
                                id,
                                encrypted_content,
                                ..
                            } => {
                                flush_text_message(&mut text_parts, &mut input_items, role_str);
                                // Responses API treats reasoning as a sibling
                                // input item. Replay the original `rs_...` id
                                // when present so multi-turn pairing stays valid.
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
                                input_items.push(item);
                            }
                            Content::ToolCall {
                                id,
                                name,
                                arguments,
                                call_id,
                                caller,
                                wire_type,
                            } => {
                                flush_text_message(&mut text_parts, &mut input_items, role_str);
                                let wire_call_id = call_id.as_deref().unwrap_or(id);
                                let item_type = wire_type.as_deref().unwrap_or("function_call");
                                let mut fc = if is_responses_custom_tool_call(wire_type.as_deref())
                                {
                                    let input = responses_custom_tool_input(arguments);
                                    json!({
                                        "type": "custom_tool_call",
                                        "call_id": wire_call_id,
                                        "name": name,
                                        "input": input,
                                    })
                                } else if item_type == "local_shell_call" {
                                    json!({
                                        "type": "local_shell_call",
                                        "call_id": wire_call_id,
                                        "action": arguments,
                                    })
                                } else {
                                    let args_str = match arguments {
                                        serde_json::Value::String(s) => s.clone(),
                                        other => other.to_string(),
                                    };
                                    json!({
                                        "type": "function_call",
                                        "call_id": wire_call_id,
                                        "name": name,
                                        "arguments": args_str,
                                    })
                                };
                                if call_id.is_some() {
                                    fc["id"] = json!(id);
                                }
                                if let Some(caller) = caller {
                                    fc["caller"] = json!(caller);
                                }
                                input_items.push(fc);
                            }
                            Content::ToolResult {
                                tool_call_id,
                                name: _,
                                content,
                                id,
                                caller,
                                wire_type,
                            } => {
                                flush_text_message(&mut text_parts, &mut input_items, role_str);
                                // Cross-protocol Anthropic Messages carries
                                // tool_result inside a user message; Responses
                                // requires a sibling function_call_output item.
                                // Preserve custom/local_shell wire types for
                                // same-protocol multi-turn fidelity.
                                if wire_type.as_deref() == Some("custom_tool_call_output") {
                                    let mut item = json!({
                                        "type": "custom_tool_call_output",
                                        "call_id": tool_call_id,
                                        "output": content,
                                    });
                                    if let Some(item_id) = id {
                                        item["id"] = json!(item_id);
                                    }
                                    input_items.push(item);
                                } else if wire_type.as_deref() == Some("local_shell_call_output") {
                                    let mut item = json!({
                                        "type": "local_shell_call_output",
                                        "call_id": tool_call_id,
                                        "output": content,
                                    });
                                    if let Some(item_id) = id {
                                        item["id"] = json!(item_id);
                                    }
                                    input_items.push(item);
                                } else {
                                    input_items.push(responses_function_call_output(
                                        tool_call_id,
                                        content,
                                        id.as_deref(),
                                        caller.as_ref(),
                                    ));
                                }
                            }
                            Content::Program {
                                id,
                                call_id,
                                code,
                                fingerprint,
                            } => {
                                flush_text_message(&mut text_parts, &mut input_items, role_str);
                                input_items.push(json!({
                                    "type": "program",
                                    "id": id,
                                    "call_id": call_id,
                                    "code": code,
                                    "fingerprint": fingerprint,
                                }));
                            }
                            Content::ProgramOutput {
                                id,
                                call_id,
                                result,
                                status,
                            } => {
                                flush_text_message(&mut text_parts, &mut input_items, role_str);
                                input_items.push(json!({
                                    "type": "program_output",
                                    "id": id,
                                    "call_id": call_id,
                                    "result": result,
                                    "status": status,
                                }));
                            }
                            Content::Refusal { text, .. } => {
                                text_parts.push(json!({"type": "input_text", "text": text}));
                            }
                        }
                    }

                    flush_text_message(&mut text_parts, &mut input_items, role_str);
                }
                Role::Tool => {
                    for c in &msg.content {
                        if let Content::ToolResult {
                            tool_call_id,
                            name: _,
                            content,
                            id,
                            caller,
                            wire_type,
                        } = c
                        {
                            if wire_type.as_deref() == Some("custom_tool_call_output") {
                                let mut item = json!({
                                    "type": "custom_tool_call_output",
                                    "call_id": tool_call_id,
                                    "output": content,
                                });
                                if let Some(item_id) = id {
                                    item["id"] = json!(item_id);
                                }
                                input_items.push(item);
                            } else if wire_type.as_deref() == Some("local_shell_call_output") {
                                let mut item = json!({
                                    "type": "local_shell_call_output",
                                    "call_id": tool_call_id,
                                    "output": content,
                                });
                                if let Some(item_id) = id {
                                    item["id"] = json!(item_id);
                                }
                                input_items.push(item);
                            } else {
                                input_items.push(responses_function_call_output(
                                    tool_call_id,
                                    content,
                                    id.as_deref(),
                                    caller.as_ref(),
                                ));
                            }
                        } else if let Content::ProgramOutput {
                            id,
                            call_id,
                            result,
                            status,
                        } = c
                        {
                            input_items.push(json!({
                                "type": "program_output",
                                "id": id,
                                "call_id": call_id,
                                "result": result,
                                "status": status,
                            }));
                        }
                    }
                }
            }
        }
        // Merge modeled IR messages with ordered opaque input items so
        // multi-agent / Codex items keep their original interleaving.
        let ordered_opaque: Vec<(usize, Value)> = ir
            .extensions
            .get("responses_opaque_input_items")
            .and_then(|v| v.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        let index = entry.get("index")?.as_u64()? as usize;
                        let item = entry.get("item")?.clone();
                        Some((index, item))
                    })
                    .collect()
            })
            .unwrap_or_else(|| {
                // Backward-compatible fallback: append-only bags from older IR.
                let mut fallback = Vec::new();
                let mut next = input_items.len();
                if let Some(items) = ir
                    .extensions
                    .get("codex_opaque_items")
                    .and_then(|v| v.as_array())
                {
                    for item in items {
                        fallback.push((next, item.clone()));
                        next += 1;
                    }
                }
                if let Some(items) = ir
                    .extensions
                    .get("multi_agent_items")
                    .and_then(|v| v.as_array())
                {
                    for item in items {
                        fallback.push((next, item.clone()));
                        next += 1;
                    }
                }
                fallback
            });
        if ordered_opaque.is_empty() {
            body["input"] = json!(input_items);
        } else {
            let mut merged = Vec::with_capacity(input_items.len() + ordered_opaque.len());
            let mut opaque_iter = ordered_opaque.into_iter().peekable();
            let mut modeled_idx = 0usize;
            let mut cursor = 0usize;
            while modeled_idx < input_items.len() || opaque_iter.peek().is_some() {
                if let Some((index, _)) = opaque_iter.peek() {
                    if *index == cursor || modeled_idx >= input_items.len() {
                        if let Some((_, item)) = opaque_iter.next() {
                            merged.push(item);
                            cursor += 1;
                        }
                        continue;
                    }
                }
                if modeled_idx < input_items.len() {
                    merged.push(input_items[modeled_idx].clone());
                    modeled_idx += 1;
                    cursor += 1;
                }
            }
            body["input"] = json!(merged);
        }
        if !ir.tools.is_empty() {
            let tools: Vec<Value> = ir
                .tools
                .iter()
                .map(|t| {
                    if t.is_function() {
                        let mut obj = json!({
                            "type": "function",
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters
                        });
                        if let Some(Value::Object(config)) = &t.config {
                            if let Some(map) = obj.as_object_mut() {
                                for (key, value) in config {
                                    map.entry(key.clone()).or_insert_with(|| value.clone());
                                }
                            }
                        }
                        obj
                    } else if t.is_custom() {
                        // Custom tools share the Chat/Responses top-level shape:
                        // { "type":"custom", "name", "description", ...config }
                        let mut obj = serde_json::Map::new();
                        obj.insert("type".to_string(), json!("custom"));
                        if !t.name.is_empty() {
                            obj.insert("name".to_string(), json!(t.name));
                        }
                        if let Some(ref desc) = t.description {
                            obj.insert("description".to_string(), json!(desc));
                        }
                        if let Some(ref params) = t.parameters {
                            obj.insert("parameters".to_string(), params.clone());
                        }
                        if let Some(Value::Object(cfg)) = &t.config {
                            for (k, v) in cfg {
                                obj.entry(k.clone()).or_insert_with(|| v.clone());
                            }
                        }
                        Value::Object(obj)
                    } else {
                        // Hosted tools: emit type + config fields, plus any
                        // name/description/parameters that were preserved.
                        let mut obj = serde_json::Map::new();
                        if let Some(ref ty) = t.tool_type {
                            obj.insert("type".to_string(), json!(ty));
                        }
                        if !t.name.is_empty() {
                            obj.insert("name".to_string(), json!(t.name));
                        }
                        if let Some(ref desc) = t.description {
                            obj.insert("description".to_string(), json!(desc));
                        }
                        if let Some(ref params) = t.parameters {
                            obj.insert("parameters".to_string(), params.clone());
                        }
                        if let Some(Value::Object(cfg)) = &t.config {
                            for (k, v) in cfg {
                                obj.entry(k.clone()).or_insert_with(|| v.clone());
                            }
                        }
                        Value::Object(obj)
                    }
                })
                .collect();
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
        // Synthesize text.format from IR response_format when the extension
        // path did not already carry a format (e.g. Chat → Responses Convert).
        if let Some(format) = ir.response_format.as_ref() {
            if !body["text"].is_object() {
                body["text"] = json!({});
            }
            if body["text"].get("format").is_none() {
                body["text"]["format"] = encode_text_format(format);
            }
        }
        if let Some(verbosity) = ir.params.verbosity {
            if !body["text"].is_object() {
                body["text"] = json!({});
            }
            body["text"]["verbosity"] = json!(match verbosity {
                Verbosity::Low => "low",
                Verbosity::Medium => "medium",
                Verbosity::High => "high",
            });
        }
        for (extension, field) in [
            ("openai.safety_identifier", "safety_identifier"),
            ("openai.prompt_cache_options", "prompt_cache_options"),
            ("openai.prompt_cache_key", "prompt_cache_key"),
            ("openai.prompt_cache_retention", "prompt_cache_retention"),
        ] {
            if let Some(value) = ir.extensions.get(extension) {
                body[field] = value.clone();
            }
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
                    // GPT-5.6+ supports max natively; emit the IR level as-is.
                    body["reasoning"] = json!({"effort": match effort {
                        tiygate_core::ThinkingEffort::None => "none",
                        tiygate_core::ThinkingEffort::Minimal => "minimal",
                        tiygate_core::ThinkingEffort::Low => "low",
                        tiygate_core::ThinkingEffort::Medium => "medium",
                        tiygate_core::ThinkingEffort::High => "high",
                        tiygate_core::ThinkingEffort::XHigh => "xhigh",
                        tiygate_core::ThinkingEffort::Max => "max",
                    }});
                }
                // Attach reasoning.summary / mode / context when present.
                let ensure_reasoning_obj = |body: &mut Value| {
                    if body.get("reasoning").is_none() {
                        body["reasoning"] = json!({});
                    }
                };
                if let Some(ref summary) = thinking.summary {
                    ensure_reasoning_obj(&mut body);
                    if let Some(obj) = body["reasoning"].as_object_mut() {
                        obj.insert("summary".to_string(), json!(summary));
                    }
                }
                if let Some(ref mode) = thinking.mode {
                    ensure_reasoning_obj(&mut body);
                    if let Some(obj) = body["reasoning"].as_object_mut() {
                        obj.insert("mode".to_string(), json!(mode));
                    }
                }
                if let Some(ref context) = thinking.context {
                    ensure_reasoning_obj(&mut body);
                    if let Some(obj) = body["reasoning"].as_object_mut() {
                        obj.insert("context".to_string(), context.clone());
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
        // `metadata` from responses_extra may contain non-string entries that
        // ir.metadata (HashMap<String, String>) cannot represent, so it takes
        // priority over the lossy IR subset; other fields follow the usual
        // "modeled path wins" rule.
        if let Some(extra) = ir
            .extensions
            .get("responses_extra")
            .and_then(|v| v.as_object())
        {
            for (k, v) in extra {
                if k == "metadata" || body.get(k).is_none() {
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
        // Ordered opaque output items (hosted tool results, multi-agent, etc.)
        // for same-protocol re-encode. Modeled content is rebuilt separately;
        // these entries keep original interleaving by index.
        let mut opaque_output_items: Vec<Value> = Vec::new();
        if let Some(output) = body["output"].as_array() {
            for (output_index, item) in output.iter().enumerate() {
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
                                            prompt_cache_breakpoint: None,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    Some("program") => {
                        content.push(Content::Program {
                            id: item["id"].as_str().unwrap_or("").to_string(),
                            call_id: item["call_id"].as_str().unwrap_or("").to_string(),
                            code: item["code"].as_str().unwrap_or("").to_string(),
                            fingerprint: item["fingerprint"].as_str().unwrap_or("").to_string(),
                        });
                    }
                    Some("program_output") => {
                        content.push(Content::ProgramOutput {
                            id: item["id"].as_str().unwrap_or("").to_string(),
                            call_id: item["call_id"].as_str().unwrap_or("").to_string(),
                            result: item["result"].as_str().unwrap_or("").to_string(),
                            status: item["status"].as_str().unwrap_or("completed").to_string(),
                        });
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
                            caller: decode_tool_caller(item),
                            wire_type: None,
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
                            caller: None,
                            wire_type: Some("local_shell_call".to_string()),
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
                            caller: None,
                            wire_type: Some("custom_tool_call".to_string()),
                        });
                    }
                    // Hosted tool / multi-agent / other provider-specific output
                    // items are not first-class IR content. Keep the raw wire
                    // object with its original index for same-protocol re-encode.
                    Some(_) | None => {
                        opaque_output_items.push(json!({
                            "index": output_index,
                            "item": item,
                        }));
                    }
                }
            }
        }
        // OpenAI Responses reports `status: "completed"` even for tool-call
        // turns — the only reliable signal that the turn ended to call a tool
        // is the presence of a `function_call` / `program` / `local_shell_call`
        // / `custom_tool_call` output item, NOT the status.  This mirrors the
        // streaming decoder's `saw_function_call` latch so the non-streaming
        // HTTP path emits `FinishReason::ToolCalls` for PTC turns, otherwise
        // the cross-protocol encoder produces `finish_reason: "stop"` and the
        // client never runs the tool.
        let has_tool_call = content
            .iter()
            .any(|c| matches!(c, Content::ToolCall { .. } | Content::Program { .. }));
        let finish_reason = body["status"].as_str().map(|s| match s {
            "completed" => {
                if has_tool_call {
                    FinishReason::ToolCalls
                } else {
                    FinishReason::Stop
                }
            }
            "incomplete" => {
                if has_tool_call {
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
        let usage = body
            .get("usage")
            .filter(|usage| {
                usage.is_object()
                    && (usage["input_tokens"].is_u64()
                        || usage["output_tokens"].is_u64()
                        || usage["total_tokens"].is_u64())
            })
            .map(|u| {
                let cache_read = u["input_tokens_details"]["cached_tokens"].as_u64();
                let cache_write = u["input_tokens_details"]["cache_write_tokens"].as_u64();
                // Responses' `input_tokens` includes the cached portion; the IR
                // convention keeps prompt_tokens cache-free. Subtract to avoid
                // double-counting when re-encoded downstream.
                let raw_input = u["input_tokens"].as_u64().unwrap_or(0);
                Usage {
                    prompt_tokens: raw_input
                        .saturating_sub(cache_read.unwrap_or(0))
                        .saturating_sub(cache_write.unwrap_or(0)),
                    completion_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                    total_tokens: u["total_tokens"].as_u64().unwrap_or(0),
                    reasoning_tokens: u["output_tokens_details"]["reasoning_tokens"].as_u64(),
                    cache_read_tokens: cache_read,
                    cache_write_tokens: cache_write,
                }
            });
        let mut extensions = std::collections::HashMap::new();
        if !opaque_output_items.is_empty() {
            extensions.insert(
                "responses_opaque_output_items".to_string(),
                json!(opaque_output_items),
            );
            // Preserve the full source layout for same-protocol replay. The
            // modeled IR intentionally flattens message content, so opaque
            // indexes alone are insufficient to reconstruct interleaving.
            extensions.insert(
                "responses_original_output".to_string(),
                body["output"].clone(),
            );
        }
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

pub struct ResponsesStreamEncoder {
    response_id: Option<String>,
    /// Next output item index to allocate. The text message and each function
    /// call occupy distinct output_index slots so a Responses client can
    /// reassemble them independently.
    next_output_index: u32,
    /// output_index assigned to the assistant text message (lazily allocated
    /// on the first TextDelta), so all text fragments share one index.
    text_output_index: Option<u32>,
    /// Accumulated assistant text used by terminal item snapshots.
    text: String,
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
    /// PTC (program / program_output) items emitted during the stream, paired
    /// with their allocated output indexes so the terminal snapshot preserves
    /// the same ordering as incremental lifecycle events.
    program_items: Vec<(u32, Value)>,
    /// Original Responses wire type per call_id (function_call/custom_tool_call/local_shell_call).
    tool_wire_types: HashMap<String, String>,
    /// Responses item id (fc_*) per call_id when distinct from the call id.
    tool_item_ids: HashMap<String, String>,
    /// PTC caller metadata per call_id.
    tool_callers: HashMap<String, ToolCaller>,
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
            text: String::new(),
            tool_output_indices: std::collections::HashMap::new(),
            tool_output_order: Vec::new(),
            tool_names: std::collections::HashMap::new(),
            tool_arguments: std::collections::HashMap::new(),
            tool_done: std::collections::HashSet::new(),
            sequence_number: 0,
            pending_usage: None,
            completed_sent: false,
            pending_finish_status: None,
            reasoning_output_index: None,
            reasoning_text: String::new(),
            reasoning_id: None,
            reasoning_encrypted: None,
            program_items: Vec::new(),
            tool_wire_types: HashMap::new(),
            tool_item_ids: HashMap::new(),
            tool_callers: HashMap::new(),
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

    fn open_tool_call(
        &mut self,
        id: &str,
        name: &str,
        wire_type: Option<&str>,
        item_id: Option<&str>,
        caller: Option<&ToolCaller>,
    ) -> String {
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
        if is_responses_custom_tool_call(wire_type) {
            self.tool_wire_types
                .entry(id.to_string())
                .or_insert_with(|| "custom_tool_call".to_string());
        } else if let Some(wt) = wire_type {
            self.tool_wire_types
                .entry(id.to_string())
                .or_insert_with(|| wt.to_string());
        }
        if let Some(iid) = item_id.filter(|v| !v.is_empty()) {
            self.tool_item_ids
                .entry(id.to_string())
                .or_insert_with(|| iid.to_string());
        }
        if let Some(c) = caller {
            self.tool_callers
                .entry(id.to_string())
                .or_insert_with(|| c.clone());
        }
        if already_open {
            return String::new();
        }
        let item_type = if is_responses_custom_tool_call(wire_type) {
            "custom_tool_call"
        } else {
            wire_type.unwrap_or("function_call")
        };
        let wire_item_id = self.tool_item_ids.get(id).map(String::as_str).unwrap_or(id);
        let mut item = if item_type == "custom_tool_call" {
            json!({"id": wire_item_id, "call_id": id, "type": "custom_tool_call", "name": name, "input": "", "status": "in_progress"})
        } else if item_type == "local_shell_call" {
            json!({"id": wire_item_id, "call_id": id, "type": "local_shell_call", "action": {}, "status": "in_progress"})
        } else {
            json!({"id": wire_item_id, "call_id": id, "type": "function_call", "name": name, "arguments": "", "status": "in_progress"})
        };
        if let Some(c) = self.tool_callers.get(id) {
            item["caller"] = json!(c);
        }
        self.event(json!({"type": "response.output_item.added", "output_index": idx, "item": item}))
    }

    fn append_tool_arguments(&mut self, id: &str, arguments: &str) -> String {
        let idx = self.tool_output_indices.get(id).copied().unwrap_or(0);
        self.tool_arguments
            .entry(id.to_string())
            .or_default()
            .push_str(arguments);
        let item_id = self.tool_item_ids.get(id).map(String::as_str).unwrap_or(id);
        let item_type = self
            .tool_wire_types
            .get(id)
            .map(String::as_str)
            .unwrap_or("function_call");
        let event_type = if item_type == "custom_tool_call" {
            "response.custom_tool_call_input.delta"
        } else {
            "response.function_call_arguments.delta"
        };
        self.event(json!({"type": event_type, "item_id": item_id, "output_index": idx, "delta": arguments}))
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
            let item_type = self
                .tool_wire_types
                .get(&call_id)
                .cloned()
                .unwrap_or_else(|| "function_call".to_string());
            let wire_item_id = self
                .tool_item_ids
                .get(&call_id)
                .cloned()
                .unwrap_or_else(|| call_id.clone());
            if item_type == "function_call" {
                out.push_str(&self.event(json!({"type": "response.function_call_arguments.done", "item_id": wire_item_id, "output_index": idx, "arguments": arguments})));
            }
            let mut item = if item_type == "custom_tool_call" {
                let input = serde_json::from_str::<Value>(&arguments)
                    .ok()
                    .map(|value| responses_custom_tool_input(&value))
                    .unwrap_or(arguments);
                out.push_str(&self.event(json!({"type": "response.custom_tool_call_input.done", "item_id": wire_item_id, "output_index": idx, "input": input.clone()})));
                json!({"id": wire_item_id, "call_id": call_id, "type": "custom_tool_call", "name": name, "input": input, "status": status})
            } else if item_type == "local_shell_call" {
                let action = serde_json::from_str::<Value>(&arguments).unwrap_or(json!({}));
                json!({"id": wire_item_id, "call_id": call_id, "type": "local_shell_call", "action": action, "status": status})
            } else {
                json!({"id": wire_item_id, "call_id": call_id, "type": "function_call", "name": name, "arguments": arguments, "status": status})
            };
            if let Some(caller) = self.tool_callers.get(&call_id) {
                item["caller"] = json!(caller);
            }
            out.push_str(&self.event(
                json!({"type": "response.output_item.done", "output_index": idx, "item": item}),
            ));
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
            let mut input_details = serde_json::Map::new();
            if cache_read > 0 {
                input_details.insert("cached_tokens".to_string(), json!(cache_read));
            }
            if cache_write > 0 {
                input_details.insert("cache_write_tokens".to_string(), json!(cache_write));
            }
            if !input_details.is_empty() {
                response["usage"]["input_tokens_details"] = json!(input_details);
            }
            if let Some(rt) = usage.reasoning_tokens {
                if rt > 0 {
                    response["usage"]["output_tokens_details"] = json!({"reasoning_tokens": rt});
                }
            }
        }
        // Build the output array in output_index order so clients reconstruct
        // the same item sequence even when incremental events were missed.
        let mut indexed_output = Vec::<(u32, Value)>::new();
        if let Some(index) = self.reasoning_output_index {
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
            indexed_output.push((index, item));
        }
        if let Some(index) = self.text_output_index {
            let item_id = format!("{}_msg", id);
            indexed_output.push((
                index,
                json!({
                    "id": item_id,
                    "type": "message",
                    "role": "assistant",
                    "status": status,
                    "content": [{
                        "type": "output_text",
                        "text": &self.text,
                        "annotations": []
                    }]
                }),
            ));
        }
        for (index, item) in &self.program_items {
            indexed_output.push((*index, item.clone()));
        }
        for call_id in self.tool_output_order.clone() {
            let name = self.tool_names.get(&call_id).cloned().unwrap_or_default();
            let arguments = self
                .tool_arguments
                .get(&call_id)
                .cloned()
                .unwrap_or_default();
            let item_type = self
                .tool_wire_types
                .get(&call_id)
                .map(String::as_str)
                .unwrap_or("function_call");
            let wire_item_id = self
                .tool_item_ids
                .get(&call_id)
                .cloned()
                .unwrap_or_else(|| call_id.clone());
            let mut item = if item_type == "custom_tool_call" {
                let input = serde_json::from_str::<Value>(&arguments)
                    .ok()
                    .and_then(|v| v.get("input").and_then(|x| x.as_str()).map(str::to_string))
                    .unwrap_or(arguments);
                json!({
                    "type": "custom_tool_call",
                    "id": wire_item_id,
                    "call_id": call_id,
                    "name": name,
                    "input": input,
                    "status": status,
                })
            } else if item_type == "local_shell_call" {
                let action = serde_json::from_str::<Value>(&arguments).unwrap_or(json!({}));
                json!({
                    "type": "local_shell_call",
                    "id": wire_item_id,
                    "call_id": call_id,
                    "action": action,
                    "status": status,
                })
            } else {
                json!({
                    "type": "function_call",
                    "id": wire_item_id,
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments,
                    "status": status,
                })
            };
            if let Some(caller) = self.tool_callers.get(&call_id) {
                item["caller"] = json!(caller);
            }
            let index = self
                .tool_output_indices
                .get(&call_id)
                .copied()
                .unwrap_or(u32::MAX);
            indexed_output.push((index, item));
        }
        indexed_output.sort_by_key(|(index, _)| *index);
        let output: Vec<Value> = indexed_output.into_iter().map(|(_, item)| item).collect();
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
                self.text.push_str(text);
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
                wire_type,
                item_id,
                caller,
            } => {
                if let Some(n) = name {
                    // Opener: allocate a distinct output_index for this call.
                    let mut out = self.open_tool_call(
                        id,
                        n,
                        wire_type.as_deref(),
                        item_id.as_deref(),
                        caller.as_ref(),
                    );
                    if !arguments.is_empty() {
                        out.push_str(&self.append_tool_arguments(id, arguments));
                    }
                    out
                } else {
                    self.append_tool_arguments(id, arguments)
                }
            }
            // Item-level PTC events (complete program / program_output items).
            StreamPart::ProgramDelta {
                id,
                call_id,
                code,
                fingerprint,
            } => {
                let idx = self.next_output_index;
                self.next_output_index += 1;
                let item = json!({
                    "type": "program",
                    "id": id,
                    "call_id": call_id,
                    "code": code,
                    "fingerprint": fingerprint,
                });
                self.program_items.push((idx, item.clone()));
                let mut out = self.event(json!({
                    "type": "response.output_item.added",
                    "output_index": idx,
                    "item": item.clone(),
                }));
                out.push_str(&self.event(json!({
                    "type": "response.output_item.done",
                    "output_index": idx,
                    "item": item,
                })));
                out
            }
            StreamPart::ProgramOutputDelta {
                id,
                call_id,
                result,
                status,
            } => {
                let idx = self.next_output_index;
                self.next_output_index += 1;
                let item = json!({
                    "type": "program_output",
                    "id": id,
                    "call_id": call_id,
                    "result": result,
                    "status": status,
                });
                self.program_items.push((idx, item.clone()));
                let mut out = self.event(json!({
                    "type": "response.output_item.added",
                    "output_index": idx,
                    "item": item.clone(),
                }));
                out.push_str(&self.event(json!({
                    "type": "response.output_item.done",
                    "output_index": idx,
                    "item": item,
                })));
                out
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
                        let text = self.text.clone();
                        out.push_str(&self.event(json!({"type": "response.output_text.done", "output_index": idx, "item_id": item_id, "content_index": 0, "text": text})));
                        out.push_str(&self.event(json!({"type": "response.content_part.done", "output_index": idx, "item_id": item_id, "content_index": 0, "part": {"type": "output_text", "text": text, "annotations": []}})));
                        out.push_str(&self.event(json!({"type": "response.output_item.done", "output_index": idx, "item": {"id": item_id, "type": "message", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": text, "annotations": []}]}})));
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
            StreamPart::Error {
                message,
                class,
                upstream_code,
            } => {
                let mut err = json!({"message": message, "type": error_type_for_class(*class)});
                if let Some(c) = upstream_code {
                    err["code"] = json!(c);
                }
                self.event(json!({"type": "error", "error": err}))
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
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"type": "error", "error": err})
        )
        .into_bytes()
    }
    fn encode_done(&mut self) -> Vec<u8> {
        "data: [DONE]\n\n".to_string().into_bytes()
    }
}

pub struct ResponsesStreamDecoder {
    response_id: Option<String>,
    /// Responses item id → canonical function call id/name. Argument deltas
    /// identify their target by item_id and may be interleaved.
    function_calls: HashMap<String, (String, Option<String>)>,
    /// Fallback for compatible providers that omit item_id on argument deltas.
    current_call_id: Option<String>,
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
    /// Accumulated custom-tool input by call id. The `.done` event repeats the
    /// complete input, so this lets us forward only a suffix that was not
    /// already delivered via `.delta`.
    custom_tool_inputs: HashMap<String, String>,
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
            function_calls: HashMap::new(),
            current_call_id: None,
            saw_function_call: false,
            pending_reasoning_id: None,
            pending_reasoning_encrypted: None,
            custom_tool_inputs: HashMap::new(),
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
                    self.saw_function_call = true;
                    let item_id = item["id"].as_str().unwrap_or("").to_string();
                    let call_id = item["call_id"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .or_else(|| item["id"].as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = item["name"].as_str().map(String::from);
                    let dual_item_id = if !item_id.is_empty() && item_id != call_id {
                        Some(item_id.clone())
                    } else {
                        None
                    };
                    self.function_calls
                        .insert(item_id, (call_id.clone(), name.clone()));
                    self.current_call_id = Some(call_id.clone());
                    parts.push(StreamPart::ToolCallDelta {
                        id: call_id,
                        name,
                        arguments: String::new(),
                        wire_type: None,
                        item_id: dual_item_id,
                        caller: decode_tool_caller(item),
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
                } else if item["type"] == "program" {
                    // Item-level PTC program (complete on added; done is ignored
                    // to avoid double emission of the same item). Latch as a
                    // tool-calling turn so finish_reason maps to ToolCalls.
                    self.saw_function_call = true;
                    parts.push(StreamPart::ProgramDelta {
                        id: item["id"].as_str().unwrap_or("").to_string(),
                        call_id: item["call_id"].as_str().unwrap_or("").to_string(),
                        code: item["code"].as_str().unwrap_or("").to_string(),
                        fingerprint: item["fingerprint"].as_str().unwrap_or("").to_string(),
                    });
                } else if item["type"] == "program_output" {
                    // program_output is a *result* item, not a tool-call
                    // request. Do NOT latch saw_function_call here — a lone
                    // program_output (e.g. upstream replaying prior-turn
                    // output) would otherwise force finish_reason to
                    // ToolCalls even though the model made no tool call this
                    // turn. Only the `program` item (the tool-call equivalent)
                    // latches above.
                    parts.push(StreamPart::ProgramOutputDelta {
                        id: item["id"].as_str().unwrap_or("").to_string(),
                        call_id: item["call_id"].as_str().unwrap_or("").to_string(),
                        result: item["result"].as_str().unwrap_or("").to_string(),
                        status: item["status"].as_str().unwrap_or("completed").to_string(),
                    });
                } else if item["type"] == "local_shell_call" {
                    // Codex local_shell_call: treat as a tool call so the
                    // streaming finish_reason is ToolCalls, not Stop.
                    self.saw_function_call = true;
                    let id = responses_call_id(item).unwrap_or("").to_string();
                    let action = item.get("action").cloned().unwrap_or(json!({}));
                    self.current_call_id = Some(id.clone());
                    parts.push(StreamPart::ToolCallDelta {
                        id,
                        name: Some("local_shell".to_string()),
                        arguments: action.to_string(),
                        wire_type: Some("local_shell_call".to_string()),
                        item_id: None,
                        caller: None,
                    });
                } else if item["type"] == "custom_tool_call" {
                    // Codex custom_tool_call: treat as a tool call so the
                    // streaming finish_reason is ToolCalls, not Stop.
                    self.saw_function_call = true;
                    let item_id = item["id"].as_str().unwrap_or("").to_string();
                    let id = responses_call_id(item).unwrap_or("").to_string();
                    let name = item["name"].as_str().unwrap_or("").to_string();
                    let input_text = item["input"].as_str().unwrap_or("").to_string();
                    let dual_item_id = if !item_id.is_empty() && item_id != id {
                        Some(item_id.clone())
                    } else {
                        None
                    };
                    self.function_calls
                        .insert(item_id, (id.clone(), Some(name.clone())));
                    if !input_text.is_empty() {
                        self.custom_tool_inputs
                            .insert(id.clone(), input_text.clone());
                    }
                    self.current_call_id = Some(id.clone());
                    parts.push(StreamPart::ToolCallDelta {
                        id,
                        name: Some(name),
                        arguments: input_text,
                        wire_type: Some("custom_tool_call".to_string()),
                        item_id: dual_item_id,
                        caller: decode_tool_caller(item),
                    });
                }
            }
            Some("response.function_call_arguments.delta") => {
                if let Some(args) = event["delta"].as_str() {
                    let id = event["item_id"]
                        .as_str()
                        .and_then(|item_id| self.function_calls.get(item_id))
                        .map(|(call_id, _)| call_id.clone())
                        .or_else(|| self.current_call_id.clone())
                        .unwrap_or_default();
                    // Argument fragment: `name: None` so cross-protocol
                    // encoders route this to their argument-delta event.
                    parts.push(StreamPart::ToolCallDelta {
                        id,
                        name: None,
                        arguments: args.to_string(),
                        wire_type: None,
                        item_id: None,
                        caller: None,
                    });
                }
            }
            Some("response.custom_tool_call_input.delta") => {
                if let Some(input) = event["delta"].as_str() {
                    let id = event["item_id"]
                        .as_str()
                        .and_then(|item_id| self.function_calls.get(item_id))
                        .map(|(call_id, _)| call_id.clone())
                        .or_else(|| self.current_call_id.clone())
                        .unwrap_or_default();
                    self.custom_tool_inputs
                        .entry(id.clone())
                        .or_default()
                        .push_str(input);
                    parts.push(StreamPart::ToolCallDelta {
                        id,
                        name: None,
                        arguments: input.to_string(),
                        wire_type: Some("custom_tool_call".to_string()),
                        item_id: None,
                        caller: None,
                    });
                }
            }
            Some("response.custom_tool_call_input.done") => {
                if let Some(input) = event["input"].as_str() {
                    let id = event["item_id"]
                        .as_str()
                        .and_then(|item_id| self.function_calls.get(item_id))
                        .map(|(call_id, _)| call_id.clone())
                        .or_else(|| self.current_call_id.clone())
                        .unwrap_or_default();
                    let delivered = self.custom_tool_inputs.remove(&id).unwrap_or_default();
                    let remaining = input.strip_prefix(&delivered).unwrap_or(input);
                    if !remaining.is_empty() {
                        parts.push(StreamPart::ToolCallDelta {
                            id,
                            name: None,
                            arguments: remaining.to_string(),
                            wire_type: Some("custom_tool_call".to_string()),
                            item_id: None,
                            caller: None,
                        });
                    }
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
                } else if item["type"] == "program" || item["type"] == "program_output" {
                    // Already emitted on output_item.added; ignore done to
                    // avoid duplicate ProgramDelta / ProgramOutputDelta.
                }
                if item["type"] == "function_call" || item["type"] == "custom_tool_call" {
                    if let Some(item_id) = item["id"].as_str() {
                        self.function_calls.remove(item_id);
                    }
                }
                self.current_call_id = None;
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
                    let cache_write = usage
                        .get("input_tokens_details")
                        .and_then(|d| d["cache_write_tokens"].as_u64());
                    let raw_input = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    parts.push(StreamPart::Usage {
                        usage: Usage {
                            // input_tokens includes cache; IR keeps it cache-free.
                            prompt_tokens: raw_input
                                .saturating_sub(cache_read.unwrap_or(0))
                                .saturating_sub(cache_write.unwrap_or(0)),
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
                            cache_write_tokens: cache_write,
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
                let code = err["type"].as_str();
                let class = tiygate_core::classify_upstream_error(None, code);
                parts.push(StreamPart::Error {
                    message: err["message"].as_str().unwrap_or("Unknown").to_string(),
                    class,
                    upstream_code: code.map(String::from),
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
    fn test_interleaved_ptc_wire_order_preserved() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        // Wire order: user → program → function_call → text message → program_output
        // must survive decode → encode without bucket reordering.
        let body = json!({
            "model": "gpt-5.6",
            "input": [
                {"role": "user", "content": "run"},
                {"type": "program", "id": "prog_1", "call_id": "call_prog_1", "code": "await tools.lookup({})", "fingerprint": "fp_1"},
                {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "lookup", "arguments": "{}", "caller": {"type": "program", "caller_id": "call_prog_1"}},
                {"role": "assistant", "content": "mid"},
                {"type": "program_output", "id": "progo_1", "call_id": "call_prog_1", "result": "done", "status": "completed"}
            ]
        });
        let ir = codec.decode_request(body, &env).unwrap();
        let (encoded, _) = codec.encode_request(&ir).unwrap();
        let input = encoded["input"].as_array().unwrap();
        let types: Vec<&str> = input
            .iter()
            .map(|item| {
                if let Some(t) = item.get("type").and_then(|v| v.as_str()) {
                    t
                } else if item.get("role").is_some() {
                    "message"
                } else {
                    "unknown"
                }
            })
            .collect();
        assert_eq!(
            types,
            vec![
                "message",
                "program",
                "function_call",
                "message",
                "program_output"
            ],
            "PTC interleaving order must be preserved: {types:?}"
        );
        assert_eq!(input[3]["content"], "mid");
    }

    #[test]
    fn test_stream_program_item_roundtrip() {
        let mut decoder = ResponsesStreamDecoder::new();
        let parts = decoder
            .feed(r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"program","id":"prog_1","call_id":"call_prog_1","code":"await tools.f({})","fingerprint":"fp_1"}}"#)
            .unwrap();
        assert!(matches!(
            &parts[0],
            StreamPart::ProgramDelta {
                id,
                call_id,
                code,
                fingerprint
            } if id == "prog_1" && call_id == "call_prog_1" && code.contains("tools.f") && fingerprint == "fp_1"
        ));
        let mut encoder = ResponsesStreamEncoder::new();
        let bytes = encoder.encode_part(&parts[0]).unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("\"type\":\"response.output_item.added\""));
        assert!(s.contains("\"type\":\"program\""));
        assert!(s.contains("\"call_id\":\"call_prog_1\""));
        assert!(s.contains("\"type\":\"response.output_item.done\""));
    }

    #[test]
    fn test_stream_program_only_turn_finish_reason_tool_calls() {
        let mut dec = ResponsesStreamDecoder::new();
        dec.feed(r#"data: {"type":"response.created","response":{"id":"resp_1","object":"response","status":"in_progress"}}"#)
            .unwrap();
        dec.feed(r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"program","id":"prog_1","call_id":"call_prog_1","code":"await tools.f({})","fingerprint":"fp_1"}}"#)
            .unwrap();
        let parts = dec
            .feed(r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed"}}"#)
            .unwrap();
        assert!(
            parts.iter().any(|p| matches!(
                p,
                StreamPart::Finish {
                    reason: FinishReason::ToolCalls
                }
            )),
            "program-only stream must finish with ToolCalls, got: {parts:?}"
        );
    }

    #[test]
    fn test_stream_lone_program_output_does_not_latch_tool_calls() {
        // A lone program_output (without a preceding program) is a *result*
        // item, not a tool-call request. finish_reason should NOT be
        // ToolCalls — the model made no tool call this turn.
        let mut dec = ResponsesStreamDecoder::new();
        dec.feed(r#"data: {"type":"response.created","response":{"id":"resp_1","object":"response","status":"in_progress"}}"#)
            .unwrap();
        dec.feed(r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"program_output","id":"progo_1","call_id":"call_prog_1","result":"ok","status":"completed"}}"#)
            .unwrap();
        let parts = dec
            .feed(r#"data: {"type":"response.completed","response":{"id":"resp_1","status":"completed"}}"#)
            .unwrap();
        assert!(
            !parts.iter().any(|p| matches!(
                p,
                StreamPart::Finish {
                    reason: FinishReason::ToolCalls
                }
            )),
            "lone program_output must NOT finish with ToolCalls, got: {parts:?}"
        );
    }

    #[test]
    fn test_stream_completed_event_includes_ptc_items() {
        // PTC items (program / program_output) must appear in the terminal
        // response.completed.output array so strict clients that reconstruct
        // from the snapshot don't lose them.
        let mut encoder = ResponsesStreamEncoder::new();
        encoder
            .encode_part(&StreamPart::ResponseStarted {
                id: "resp_1".to_string(),
            })
            .unwrap();
        encoder
            .encode_part(&StreamPart::ProgramDelta {
                id: "prog_1".to_string(),
                call_id: "call_prog_1".to_string(),
                code: "await tools.f({})".to_string(),
                fingerprint: "fp_1".to_string(),
            })
            .unwrap();
        encoder
            .encode_part(&StreamPart::ProgramOutputDelta {
                id: "progo_1".to_string(),
                call_id: "call_prog_1".to_string(),
                result: "ok".to_string(),
                status: "completed".to_string(),
            })
            .unwrap();
        let bytes = encoder
            .encode_part(&StreamPart::Finish {
                reason: FinishReason::ToolCalls,
            })
            .unwrap();
        // Finish defers response.completed when usage hasn't arrived yet.
        // Feed Usage so completed_event fires and includes the output array.
        let bytes2 = encoder
            .encode_part(&StreamPart::Usage {
                usage: Usage::default(),
            })
            .unwrap();
        let combined = [bytes.as_slice(), bytes2.as_slice()].concat();
        let s = String::from_utf8_lossy(&combined);
        // The terminal response.completed must carry both PTC items in output.
        assert!(
            s.contains("\"type\":\"response.completed\""),
            "must emit response.completed, got: {s}"
        );
        assert!(
            s.contains("\"type\":\"program\""),
            "completed output must include program item, got: {s}"
        );
        assert!(
            s.contains("\"type\":\"program_output\""),
            "completed output must include program_output item, got: {s}"
        );
        assert!(
            s.contains("\"id\":\"prog_1\""),
            "completed output must carry program id, got: {s}"
        );
        assert!(
            s.contains("\"id\":\"progo_1\""),
            "completed output must carry program_output id, got: {s}"
        );
    }

    #[test]
    fn test_stream_completed_output_preserves_output_index_order() {
        let mut encoder = ResponsesStreamEncoder::new();
        let _ = encoder
            .encode_part(&StreamPart::ResponseStarted {
                id: "resp_1".to_string(),
            })
            .unwrap();
        let _ = encoder
            .encode_part(&StreamPart::ProgramDelta {
                id: "prog_1".to_string(),
                call_id: "call_prog_1".to_string(),
                code: "return 1".to_string(),
                fingerprint: "fp".to_string(),
            })
            .unwrap();
        let _ = encoder
            .encode_part(&StreamPart::TextDelta {
                text: "after program".to_string(),
            })
            .unwrap();
        let _ = encoder
            .encode_part(&StreamPart::ToolCallDelta {
                id: "call_1".to_string(),
                name: Some("lookup".to_string()),
                arguments: "{}".to_string(),
                wire_type: None,
                item_id: Some("fc_1".to_string()),
                caller: None,
            })
            .unwrap();
        let _ = encoder
            .encode_part(&StreamPart::Finish {
                reason: FinishReason::ToolCalls,
            })
            .unwrap();
        let completed = String::from_utf8(
            encoder
                .encode_part(&StreamPart::Usage {
                    usage: Usage::default(),
                })
                .unwrap(),
        )
        .unwrap();
        let event: Value = completed
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|payload| serde_json::from_str(payload).ok())
            .find(|event: &Value| event["type"] == "response.completed")
            .expect("response.completed event");
        let types: Vec<&str> = event["response"]["output"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["type"].as_str())
            .collect();
        assert_eq!(types, vec!["program", "message", "function_call"]);
    }

    #[test]
    fn test_prompt_cache_key_from_chat_shared_extension() {
        let chat = crate::chat_completions::ChatCompletionsCodec::new();
        let responses = ResponsesCodec::new();
        let env = make_raw_env();
        let ir = chat
            .decode_request(
                json!({
                    "model": "gpt-5.6",
                    "messages": [{"role": "user", "content": "hi"}],
                    "prompt_cache_key": "shared-key",
                    "prompt_cache_retention": "in_memory"
                }),
                &env,
            )
            .unwrap();
        let (encoded, _) = responses.encode_request(&ir).unwrap();
        assert_eq!(encoded["prompt_cache_key"], "shared-key");
        assert_eq!(encoded["prompt_cache_retention"], "in_memory");
    }

    #[test]
    fn test_system_image_breakpoint_survives_chat_to_responses() {
        let chat = crate::chat_completions::ChatCompletionsCodec::new();
        let responses = ResponsesCodec::new();
        let env = make_raw_env();
        let ir = chat
            .decode_request(
                json!({
                    "model": "gpt-5.6",
                    "messages": [{
                        "role": "developer",
                        "content": [{
                            "type": "image_url",
                            "image_url": {"url": "https://example.com/system.png", "detail": "original"},
                            "prompt_cache_breakpoint": {"mode": "explicit"}
                        }]
                    }, {"role": "user", "content": "hello"}]
                }),
                &env,
            )
            .unwrap();
        let (encoded, _) = responses.encode_request(&ir).unwrap();
        assert_eq!(encoded["input"][0]["role"], "developer");
        assert_eq!(encoded["input"][0]["content"][0]["type"], "input_image");
        assert_eq!(encoded["input"][0]["content"][0]["detail"], "original");
        assert_eq!(
            encoded["input"][0]["content"][0]["prompt_cache_breakpoint"]["mode"],
            "explicit"
        );
    }

    #[test]
    fn test_responses_cache_write_usage_roundtrip() {
        let codec = ResponsesCodec::new();
        let decoded = codec
            .decode_response(json!({
                "id": "resp_1",
                "status": "completed",
                "output": [],
                "usage": {
                    "input_tokens": 100,
                    "output_tokens": 10,
                    "total_tokens": 110,
                    "input_tokens_details": {"cached_tokens": 20, "cache_write_tokens": 30}
                }
            }))
            .unwrap();
        let usage = decoded.usage.as_ref().unwrap();
        assert_eq!(usage.prompt_tokens, 50);
        assert_eq!(usage.cache_read_tokens, Some(20));
        assert_eq!(usage.cache_write_tokens, Some(30));
        let encoded = codec.encode_response(&decoded).unwrap();
        assert_eq!(encoded["usage"]["input_tokens"], 100);
        assert_eq!(
            encoded["usage"]["input_tokens_details"]["cached_tokens"],
            20
        );
        assert_eq!(
            encoded["usage"]["input_tokens_details"]["cache_write_tokens"],
            30
        );
    }

    #[test]
    fn test_programmatic_tool_calling_response_roundtrip() {
        let codec = ResponsesCodec::new();
        let body = json!({
            "id": "resp_1",
            "status": "completed",
            "output": [
                {"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "before"}]},
                {"type": "program", "id": "prog_1", "call_id": "call_prog_1", "code": "await tools.lookup({})", "fingerprint": "fp_1"},
                {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "lookup", "arguments": "{}", "caller": {"type": "program", "caller_id": "call_prog_1"}},
                {"type": "program_output", "id": "progo_1", "call_id": "call_prog_1", "result": "done", "status": "completed"},
                {"type": "message", "id": "msg_2", "role": "assistant", "content": [{"type": "output_text", "text": "after"}]}
            ]
        });
        let decoded = codec.decode_response(body).unwrap();
        assert!(matches!(
            &decoded.content[0],
            Content::Text { text, .. } if text == "before"
        ));
        assert!(matches!(
            &decoded.content[1],
            Content::Program { call_id, .. } if call_id == "call_prog_1"
        ));
        assert!(matches!(
            &decoded.content[2],
            Content::ToolCall { caller: Some(ToolCaller::Program { caller_id }), .. }
                if caller_id == "call_prog_1"
        ));
        assert!(matches!(
            &decoded.content[3],
            Content::ProgramOutput { result, .. } if result == "done"
        ));
        assert!(matches!(
            &decoded.content[4],
            Content::Text { text, .. } if text == "after"
        ));
        let encoded = codec.encode_response(&decoded).unwrap();
        assert_eq!(encoded["output"][0]["type"], "message");
        assert_eq!(encoded["output"][0]["content"][0]["text"], "before");
        assert_eq!(encoded["output"][1]["type"], "program");
        assert_eq!(encoded["output"][2]["caller"]["caller_id"], "call_prog_1");
        assert_eq!(encoded["output"][3]["type"], "program_output");
        assert_eq!(encoded["output"][4]["type"], "message");
        assert_eq!(encoded["output"][4]["content"][0]["text"], "after");
    }

    #[test]
    fn test_http_decode_ptc_finish_reason_is_tool_calls() {
        // Regression: HTTP decode_response must map `status:"completed"` to
        // `FinishReason::ToolCalls` when the output contains a `program` item,
        // mirroring the streaming decoder's `saw_function_call` latch.
        // Without this, a cross-protocol encoder emits `finish_reason:"stop"`
        // and the client never runs the tool.
        let codec = ResponsesCodec::new();
        let body = json!({
            "id": "resp_1",
            "status": "completed",
            "output": [
                {"type": "program", "id": "prog_1", "call_id": "call_prog_1", "code": "return 1", "fingerprint": "fp_1"}
            ]
        });
        let decoded = codec.decode_response(body).unwrap();
        assert_eq!(
            decoded.finish_reason,
            Some(FinishReason::ToolCalls),
            "PTC program response must map to ToolCalls, not Stop"
        );

        // A pure-text completed response must still map to Stop.
        let body_text = json!({
            "id": "resp_2",
            "status": "completed",
            "output": [
                {"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "hello"}]}
            ]
        });
        let decoded_text = codec.decode_response(body_text).unwrap();
        assert_eq!(
            decoded_text.finish_reason,
            Some(FinishReason::Stop),
            "pure-text response must map to Stop"
        );
    }

    #[test]
    fn test_http_encode_tool_calls_status_is_completed() {
        // Regression: encode_response must map ToolCalls → "completed" to
        // match the streaming encoder and OpenAI's wire behavior.
        let codec = ResponsesCodec::new();
        let ir = IrResponse {
            content: vec![Content::ToolCall {
                id: "fc_1".to_string(),
                call_id: Some("call_1".to_string()),
                name: "get_weather".to_string(),
                arguments: json!({}),
                caller: None,
                wire_type: None,
            }],
            finish_reason: Some(FinishReason::ToolCalls),
            usage: None,
            response_id: Some("resp_1".to_string()),
            stop_details: None,
            extensions: HashMap::new(),
        };
        let encoded = codec.encode_response(&ir).unwrap();
        assert_eq!(
            encoded["status"], "completed",
            "ToolCalls must encode to status 'completed', not 'incomplete'"
        );
    }

    #[test]
    fn test_programmatic_tool_calling_roundtrip() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5.6",
            "input": [
                {"role": "user", "content": "check inventory"},
                {"type": "program", "id": "prog_1", "call_id": "call_prog_1", "code": "await tools.lookup({})", "fingerprint": "fp_1"},
                {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "lookup", "arguments": "{}", "caller": {"type": "program", "caller_id": "call_prog_1"}},
                {"type": "function_call_output", "id": "fco_1", "call_id": "call_1", "output": "ok", "caller": {"type": "program", "caller_id": "call_prog_1"}},
                {"type": "program_output", "id": "progo_1", "call_id": "call_prog_1", "result": "done", "status": "completed"}
            ],
            "tools": [
                {"type": "programmatic_tool_calling"},
                {"type": "function", "name": "lookup", "parameters": {"type": "object"}, "allowed_callers": ["programmatic"], "strict": true}
            ]
        });
        let ir = codec.decode_request(body, &env).unwrap();
        assert_eq!(
            ir.tools[1].config.as_ref().unwrap()["allowed_callers"][0],
            "programmatic"
        );
        assert!(ir
            .messages
            .iter()
            .any(|message| message.content.iter().any(|content| {
                matches!(content, Content::Program { call_id, .. } if call_id == "call_prog_1")
            })));
        assert!(ir.messages.iter().any(|message| message.content.iter().any(|content| {
            matches!(content, Content::ToolCall { caller: Some(ToolCaller::Program { caller_id }), .. } if caller_id == "call_prog_1")
        })));
        let (encoded, _) = codec.encode_request(&ir).unwrap();
        let input = encoded["input"].as_array().unwrap();
        assert_eq!(input[1]["type"], "program");
        assert_eq!(input[2]["caller"]["caller_id"], "call_prog_1");
        assert_eq!(input[3]["caller"]["caller_id"], "call_prog_1");
        assert_eq!(input[4]["type"], "program_output");
        assert_eq!(encoded["tools"][1]["allowed_callers"][0], "programmatic");
        assert_eq!(encoded["tools"][1]["strict"], true);
    }

    #[test]
    fn test_gpt56_text_controls_and_breakpoints_roundtrip() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5.6",
            "input": [{
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "hello",
                    "prompt_cache_breakpoint": {"mode": "explicit"}
                }, {
                    "type": "input_image",
                    "image_url": "https://example.com/a.png",
                    "detail": "original"
                }]
            }],
            "reasoning": {"effort": "none", "mode": "pro", "context": "all_turns"},
            "text": {"verbosity": "high", "format": {"type": "text"}},
            "prompt_cache_options": {"mode": "explicit", "ttl": "30m"},
            "safety_identifier": "safe-user"
        });
        let ir = codec.decode_request(body, &env).unwrap();
        assert_eq!(
            ir.params.thinking.as_ref().and_then(|v| v.effort),
            Some(tiygate_core::ThinkingEffort::None)
        );
        assert_eq!(ir.params.verbosity, Some(Verbosity::High));
        assert!(matches!(
            ir.response_format,
            Some(tiygate_core::ResponseFormat::Text)
        ));
        let (encoded, _) = codec.encode_request(&ir).unwrap();
        assert_eq!(encoded["reasoning"]["effort"], "none");
        assert_eq!(encoded["text"]["verbosity"], "high");
        assert_eq!(encoded["text"]["format"]["type"], "text");
        assert_eq!(encoded["prompt_cache_options"]["mode"], "explicit");
        assert_eq!(encoded["safety_identifier"], "safe-user");
        assert_eq!(
            encoded["input"][0]["content"][0]["prompt_cache_breakpoint"]["mode"],
            "explicit"
        );
        assert_eq!(encoded["input"][0]["content"][1]["detail"], "original");
    }

    #[test]
    fn test_text_format_json_schema_roundtrip_and_chat_cross() {
        let responses = ResponsesCodec::new();
        let chat = crate::chat_completions::ChatCompletionsCodec::new();
        let env = make_raw_env();

        // Responses → IR → Responses
        let body = json!({
            "model": "gpt-5.6",
            "input": "hi",
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "answer",
                    "schema": {"type": "object", "properties": {"ok": {"type": "boolean"}}},
                    "strict": true
                }
            }
        });
        let ir = responses.decode_request(body, &env).unwrap();
        match &ir.response_format {
            Some(tiygate_core::ResponseFormat::JsonSchema { name, strict, .. }) => {
                assert_eq!(name, "answer");
                assert_eq!(*strict, Some(true));
            }
            other => panic!("expected JsonSchema, got {other:?}"),
        }
        let (encoded, _) = responses.encode_request(&ir).unwrap();
        assert_eq!(encoded["text"]["format"]["type"], "json_schema");
        assert_eq!(encoded["text"]["format"]["name"], "answer");
        assert_eq!(encoded["text"]["format"]["strict"], true);

        // Chat → IR → Responses must rehydrate text.format from IR response_format.
        let chat_ir = chat
            .decode_request(
                json!({
                    "model": "gpt-5.6",
                    "messages": [{"role": "user", "content": "hi"}],
                    "response_format": {
                        "type": "json_schema",
                        "json_schema": {
                            "name": "answer",
                            "schema": {"type": "object"},
                            "strict": true
                        }
                    }
                }),
                &env,
            )
            .unwrap();
        let (from_chat, _) = responses.encode_request(&chat_ir).unwrap();
        assert_eq!(from_chat["text"]["format"]["type"], "json_schema");
        assert_eq!(from_chat["text"]["format"]["name"], "answer");
        assert_eq!(from_chat["text"]["format"]["strict"], true);
    }

    #[test]
    fn test_file_id_media_roundtrip() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5.6",
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_image", "file_id": "file-img-1", "detail": "high"},
                    {"type": "input_file", "file_id": "file-doc-1"},
                    {"type": "input_file", "file_url": "https://example.com/a.pdf", "filename": "a.pdf"},
                    {"type": "input_file", "file_data": "data:application/pdf;base64,JVBERi0=", "filename": "b.pdf"}
                ]
            }]
        });
        let ir = codec.decode_request(body, &env).unwrap();
        assert!(ir.messages[0].content.iter().any(|c| matches!(
            c,
            Content::Media {
                source: tiygate_core::ir::MediaSource::FileId { id },
                ..
            } if id == "file-img-1"
        )));
        assert!(ir.messages[0].content.iter().any(|c| matches!(
            c,
            Content::Media {
                source: tiygate_core::ir::MediaSource::FileId { id },
                ..
            } if id == "file-doc-1"
        )));
        assert!(ir.messages[0].content.iter().any(|c| matches!(
            c,
            Content::Media {
                source: tiygate_core::ir::MediaSource::Url { url },
                ..
            } if url == "https://example.com/a.pdf"
        )));
        assert!(ir.messages[0].content.iter().any(|c| matches!(
            c,
            Content::Media {
                source: tiygate_core::ir::MediaSource::Inline { .. },
                mime_type,
                ..
            } if mime_type == "application/pdf"
        )));
        let (encoded, _) = codec.encode_request(&ir).unwrap();
        let parts = encoded["input"][0]["content"].as_array().unwrap();
        assert!(parts.iter().any(|p| {
            p["type"] == "input_image" && p["file_id"] == "file-img-1" && p["detail"] == "high"
        }));
        assert!(parts
            .iter()
            .any(|p| p["type"] == "input_file" && p["file_id"] == "file-doc-1"));
        assert!(parts.iter().any(|p| {
            p["type"] == "input_file"
                && p["file_url"] == "https://example.com/a.pdf"
                && p["filename"] == "a.pdf"
        }));
        assert!(parts.iter().any(|p| {
            p["type"] == "input_file"
                && p["file_data"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("data:application/pdf;base64,"))
                && p["filename"] == "b.pdf"
        }));
    }

    #[test]
    fn test_text_format_rejects_null_json_schema() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5.6",
            "input": "hi",
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "broken",
                    "schema": null
                }
            }
        });
        let ir = codec.decode_request(body, &env).unwrap();
        assert!(
            ir.response_format.is_none(),
            "null schema must not promote into IR response_format"
        );
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
    fn test_stream_decoder_interleaved_function_calls_use_item_id_and_call_id() {
        let mut decoder = ResponsesStreamDecoder::new();
        let first = decoder
            .feed(r#"data: {"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"one"}}"#)
            .unwrap();
        let second = decoder
            .feed(r#"data: {"type":"response.output_item.added","item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"two"}}"#)
            .unwrap();
        assert!(matches!(
            &first[0],
            StreamPart::ToolCallDelta { id, name: Some(name), .. }
                if id == "call_1" && name == "one"
        ));
        assert!(matches!(
            &second[0],
            StreamPart::ToolCallDelta { id, name: Some(name), .. }
                if id == "call_2" && name == "two"
        ));

        let first_delta = decoder
            .feed(r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"a\":"}"#)
            .unwrap();
        let second_delta = decoder
            .feed(r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_2","delta":"{\"b\":"}"#)
            .unwrap();
        assert!(matches!(
            &first_delta[0],
            StreamPart::ToolCallDelta { id, arguments, .. }
                if id == "call_1" && arguments == "{\"a\":"
        ));
        assert!(matches!(
            &second_delta[0],
            StreamPart::ToolCallDelta { id, arguments, .. }
                if id == "call_2" && arguments == "{\"b\":"
        ));
    }

    #[test]
    fn test_stream_encoder_completed_contains_full_text_snapshot() {
        let mut encoder = ResponsesStreamEncoder::new();
        let _ = encoder
            .encode_part(&StreamPart::ResponseStarted {
                id: "resp_1".to_string(),
            })
            .unwrap();
        let _ = encoder
            .encode_part(&StreamPart::TextDelta {
                text: "hello ".to_string(),
            })
            .unwrap();
        let _ = encoder
            .encode_part(&StreamPart::TextDelta {
                text: "world".to_string(),
            })
            .unwrap();
        let finish = String::from_utf8(
            encoder
                .encode_part(&StreamPart::Finish {
                    reason: FinishReason::Stop,
                })
                .unwrap(),
        )
        .unwrap();
        assert!(finish.contains("\"text\":\"hello world\""), "{finish}");
        let completed = String::from_utf8(
            encoder
                .encode_part(&StreamPart::ResponseCompleted {
                    id: "resp_1".to_string(),
                    status: "completed".to_string(),
                    usage: None,
                    extensions: HashMap::new(),
                })
                .unwrap(),
        )
        .unwrap();
        assert!(
            completed.contains("\"text\":\"hello world\""),
            "{completed}"
        );
    }

    #[test]
    fn test_decode_response_usage_null_is_none() {
        let codec = ResponsesCodec::new();
        let decoded = codec
            .decode_response(json!({
                "id": "resp_1",
                "status": "completed",
                "output": [],
                "usage": null
            }))
            .unwrap();
        assert!(decoded.usage.is_none());
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
    fn test_stream_function_call_preserves_caller_and_dual_ids() {
        let mut decoder = ResponsesStreamDecoder::new();
        let parts = decoder
            .feed(r#"data: {"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"lookup","arguments":"{}","caller":{"type":"program","caller_id":"call_prog_1"}}}"#)
            .unwrap();
        match &parts[0] {
            StreamPart::ToolCallDelta {
                id,
                name,
                item_id,
                caller,
                ..
            } => {
                assert_eq!(id, "call_1");
                assert_eq!(name.as_deref(), Some("lookup"));
                assert_eq!(item_id.as_deref(), Some("fc_1"));
                assert!(matches!(
                    caller,
                    Some(ToolCaller::Program { caller_id }) if caller_id == "call_prog_1"
                ));
            }
            other => panic!("expected ToolCallDelta, got {other:?}"),
        }

        let mut encoder = ResponsesStreamEncoder::new();
        let bytes = encoder.encode_part(&parts[0]).unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.contains(r#""id":"fc_1""#),
            "stream re-encode must keep item id: {s}"
        );
        assert!(
            s.contains(r#""call_id":"call_1""#),
            "stream re-encode must keep call_id: {s}"
        );
        assert!(
            s.contains(r#""caller":{"type":"program","caller_id":"call_prog_1"}"#)
                || (s.contains(r#""caller""#) && s.contains(r#""caller_id":"call_prog_1"#)),
            "stream re-encode must keep PTC caller: {s}"
        );

        let done = encoder
            .encode_part(&StreamPart::Finish {
                reason: FinishReason::ToolCalls,
            })
            .unwrap();
        let done2 = encoder
            .encode_part(&StreamPart::Usage {
                usage: Usage::default(),
            })
            .unwrap();
        let combined = [done.as_slice(), done2.as_slice()].concat();
        let completed = String::from_utf8_lossy(&combined);
        assert!(
            completed.contains(r#""id":"fc_1""#) && completed.contains(r#""call_id":"call_1""#),
            "response.completed must restore dual ids: {completed}"
        );
        assert!(
            completed.contains(r#""caller_id":"call_prog_1""#),
            "response.completed must restore caller: {completed}"
        );
    }

    #[test]
    fn test_hosted_tool_output_items_roundtrip() {
        let codec = ResponsesCodec::new();
        let body = json!({
            "id": "resp_hosted",
            "status": "completed",
            "output": [
                {
                    "type": "web_search_call",
                    "id": "ws_1",
                    "status": "completed",
                    "action": {"query": "gpt-5.6", "type": "search"}
                },
                {
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "found it"}]
                },
                {
                    "type": "file_search_call",
                    "id": "fs_1",
                    "status": "completed",
                    "queries": ["docs"]
                }
            ]
        });
        let decoded = codec.decode_response(body).unwrap();
        assert!(
            decoded
                .extensions
                .get("responses_opaque_output_items")
                .and_then(|v| v.as_array())
                .map(|a| a.len() == 2)
                .unwrap_or(false),
            "hosted output items must be bagged: {:?}",
            decoded.extensions
        );
        assert!(matches!(
            &decoded.content[0],
            Content::Text { text, .. } if text == "found it"
        ));
        let encoded = codec.encode_response(&decoded).unwrap();
        let output = encoded["output"].as_array().expect("output array");
        assert_eq!(
            output.len(),
            3,
            "hosted items + message must all reappear: {encoded}"
        );
        assert_eq!(output[0]["type"], "web_search_call");
        assert_eq!(output[0]["id"], "ws_1");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[1]["content"][0]["text"], "found it");
        assert_eq!(output[2]["type"], "file_search_call");
        assert_eq!(output[2]["id"], "fs_1");
    }

    #[test]
    fn test_stream_encoder_function_call_item_includes_call_id() {
        let mut enc = ResponsesStreamEncoder::new();
        let bytes = enc
            .encode_part(&StreamPart::ToolCallDelta {
                id: "call_123".to_string(),
                name: Some("lookup".to_string()),
                arguments: String::new(),
                wire_type: None,
                item_id: None,
                caller: None,
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
                wire_type: None,
                item_id: None,
                caller: None,
            })
            .unwrap();
        let second = enc
            .encode_part(&StreamPart::ToolCallDelta {
                id: "call_123".to_string(),
                name: Some("lookup".to_string()),
                arguments: String::new(),
                wire_type: None,
                item_id: None,
                caller: None,
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
                prompt_cache_breakpoint: None,
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
        let err = encoder.encode_error("overloaded", ErrorClass::Overloaded, None);
        let s = String::from_utf8_lossy(&err);
        assert!(s.contains("error"));
        assert!(s.contains("overloaded"));
        assert!(s.contains("\"type\":\"overloaded_error\""));
        assert!(!s.contains("gateway_error"));
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
                prompt_cache_breakpoint: None,
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
                        prompt_cache_breakpoint: None,
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
                            caller: None,
                            wire_type: None,
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
                        caller: None,
                        wire_type: None,
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
    fn test_responses_both_id_and_call_id_normalize_for_cross_protocol_pairing() {
        // When function_call items carry both `id` (item ref) and `call_id`
        // (function-call identifier) and the call_ids collide, the unique
        // remapped call_id must live in IR.call_id AND in ToolResult.tool_call_id.
        // Cross-protocol encoders (Anthropic/Chat) pair via
        // call_id.as_deref().unwrap_or(id). Using the raw (duplicated) call_id
        // would leave ToolCall and ToolResult mismatched → upstream 400.
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let duplicate = "call_shared";
        let body = json!({
            "model": "gpt-5.6",
            "input": [
                {"role": "user", "content": "review"},
                {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": duplicate,
                    "name": "git_status",
                    "arguments": "{}"
                },
                {
                    "type": "function_call",
                    "id": "fc_2",
                    "call_id": duplicate,
                    "name": "git_diff",
                    "arguments": "{\"path\":\"a.rs\"}"
                },
                {
                    "type": "function_call_output",
                    "call_id": duplicate,
                    "output": "status output"
                },
                {
                    "type": "function_call_output",
                    "call_id": duplicate,
                    "output": "diff output"
                }
            ]
        });
        let ir = codec.decode_request(body, &env).unwrap();

        let pairs: Vec<(String, Option<String>)> = ir
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                Content::ToolCall { id, call_id, .. } => Some((id.clone(), call_id.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(pairs.len(), 2);
        // IR.id keeps the item ref; IR.call_id is the unique function-call id.
        assert_eq!(pairs[0].0, "fc_1");
        assert_eq!(pairs[0].1.as_deref(), Some(duplicate));
        assert_eq!(pairs[1].0, "fc_2");
        let second_unique = format!("{duplicate}_1");
        assert_eq!(pairs[1].1.as_deref(), Some(second_unique.as_str()));

        let result_ids: Vec<String> = ir
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|c| match c {
                Content::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
                _ => None,
            })
            .collect();
        let expected = vec![pairs[0].1.clone().unwrap(), pairs[1].1.clone().unwrap()];
        assert_eq!(
            result_ids, expected,
            "ToolResult must use unique remapped call_ids"
        );

        // Anthropic cross-protocol pairing.
        let anthropic = crate::messages::MessagesCodec::new();
        let (a_enc, _) = anthropic.encode_request(&ir).unwrap();
        let a_msgs = a_enc["messages"].as_array().unwrap();
        let a_tool_use: Vec<String> = a_msgs
            .iter()
            .filter(|m| m["role"] == "assistant")
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter(|b| b["type"] == "tool_use")
            .filter_map(|b| b["id"].as_str().map(String::from))
            .collect();
        let a_tool_result: Vec<String> = a_msgs
            .iter()
            .filter(|m| m["role"] == "user")
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter(|b| b["type"] == "tool_result")
            .filter_map(|b| b["tool_use_id"].as_str().map(String::from))
            .collect();
        assert_eq!(a_tool_use, expected);
        assert_eq!(a_tool_result, expected);

        // Chat Completions cross-protocol pairing.
        let chat = crate::chat_completions::ChatCompletionsCodec::new();
        let (c_enc, _) = chat.encode_request(&ir).unwrap();
        let c_msgs = c_enc["messages"].as_array().unwrap();
        let c_tool_calls: Vec<String> = c_msgs
            .iter()
            .filter(|m| m["role"] == "assistant")
            .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
            .filter_map(|tc| tc["id"].as_str().map(String::from))
            .collect();
        let c_tool_results: Vec<String> = c_msgs
            .iter()
            .filter(|m| m["role"] == "tool")
            .filter_map(|m| m["tool_call_id"].as_str().map(String::from))
            .collect();
        assert_eq!(c_tool_calls, expected);
        assert_eq!(c_tool_results, expected);
    }

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
            ..
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
            ..
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
                    prompt_cache_breakpoint: None,
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
    fn test_reasoning_mode_context_max_roundtrip() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5.6",
            "input": "hi",
            "reasoning": {
                "effort": "max",
                "mode": "pro",
                "context": {"preserve": true},
                "summary": "auto"
            }
        });
        let ir = codec.decode_request(body, &env).unwrap();
        let thinking = ir.params.thinking.as_ref().expect("thinking present");
        assert_eq!(thinking.effort, Some(tiygate_core::ThinkingEffort::Max));
        assert_eq!(thinking.mode.as_deref(), Some("pro"));
        assert_eq!(thinking.context, Some(json!({"preserve": true})));
        assert_eq!(thinking.summary.as_deref(), Some("auto"));

        // Cross-protocol rebuild (no reasoning_full) must still emit all fields.
        let mut ir_cross = ir.clone();
        ir_cross.extensions.remove("reasoning_full");
        ir_cross.ingress_protocol =
            ProtocolEndpoint::new(ProtocolSuite::AnthropicMessages, "messages", "2023-06-01");
        let (encoded, _) = codec.encode_request(&ir_cross).unwrap();
        assert_eq!(encoded["reasoning"]["effort"], "max");
        assert_eq!(encoded["reasoning"]["mode"], "pro");
        assert_eq!(encoded["reasoning"]["context"]["preserve"], true);
        assert_eq!(encoded["reasoning"]["summary"], "auto");
    }

    #[test]
    fn test_hosted_tools_roundtrip() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5.6",
            "input": "search something",
            "tools": [
                {
                    "type": "function",
                    "name": "lookup",
                    "description": "fn",
                    "parameters": {"type": "object"}
                },
                {
                    "type": "web_search",
                    "search_context_size": "medium"
                },
                {
                    "type": "file_search",
                    "vector_store_ids": ["vs_1"]
                }
            ]
        });
        let ir = codec.decode_request(body, &env).unwrap();
        assert_eq!(ir.tools.len(), 3);
        assert!(ir.tools[0].is_function());
        assert_eq!(ir.tools[0].name, "lookup");
        assert_eq!(ir.tools[1].tool_type.as_deref(), Some("web_search"));
        assert_eq!(
            ir.tools[1]
                .config
                .as_ref()
                .and_then(|c| c.get("search_context_size")),
            Some(&json!("medium"))
        );
        assert_eq!(ir.tools[2].tool_type.as_deref(), Some("file_search"));

        let (encoded, _) = codec.encode_request(&ir).unwrap();
        let tools = encoded["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "lookup");
        assert_eq!(tools[1]["type"], "web_search");
        assert_eq!(tools[1]["search_context_size"], "medium");
        assert_eq!(tools[2]["type"], "file_search");
        assert_eq!(tools[2]["vector_store_ids"][0], "vs_1");
    }

    #[test]
    fn test_prompt_cache_options_passthrough() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5.6",
            "input": "hi",
            "prompt_cache_key": "k1",
            "prompt_cache_options": {"mode": "explicit", "ttl": "30m"}
        });
        let ir = codec.decode_request(body, &env).unwrap();
        let extra = ir
            .extensions
            .get("responses_extra")
            .and_then(|v| v.as_object())
            .expect("responses_extra");
        assert_eq!(extra["prompt_cache_key"], "k1");
        assert_eq!(extra["prompt_cache_options"]["mode"], "explicit");
        let (encoded, _) = codec.encode_request(&ir).unwrap();
        assert_eq!(encoded["prompt_cache_key"], "k1");
        assert_eq!(encoded["prompt_cache_options"]["mode"], "explicit");
        assert_eq!(encoded["prompt_cache_options"]["ttl"], "30m");
    }

    #[test]
    fn test_hosted_tools_hard_reject_on_chat_completions() {
        // Hosted tools (web_search/file_search/...) are Responses-only.
        // Cross-protocol conversion must hard-reject via check_lossy_conversion
        // rather than silently filtering them out of the Chat tools array.
        let responses = ResponsesCodec::new();
        let chat = crate::chat_completions::ChatCompletionsCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5.6",
            "input": "hi",
            "tools": [
                {"type": "function", "name": "lookup", "parameters": {"type": "object"}},
                {"type": "web_search"}
            ]
        });
        let ir = responses.decode_request(body, &env).unwrap();
        let err = tiygate_core::protocol::lossy::check_lossy_conversion(
            &ir,
            chat.id(),
            chat.capabilities(),
        )
        .expect_err("hosted tools must hard-reject on Chat Completions");
        assert_eq!(
            err.0,
            tiygate_core::protocol::lossy::LossyDimension::HostedTools
        );
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

    #[test]
    fn test_multi_agent_field_and_items_roundtrip() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5.6",
            "input": [
                {"role": "user", "content": "coordinate agents"},
                {
                    "type": "multi_agent_call",
                    "id": "ma_1",
                    "name": "spawn_agent",
                    "arguments": {"task": "research"}
                },
                {
                    "type": "multi_agent_call_output",
                    "id": "mao_1",
                    "call_id": "ma_1",
                    "output": {"agent_id": "/root/child1"}
                }
            ],
            "multi_agent": {
                "enabled": true,
                "max_concurrent_subagents": 4
            }
        });
        let ir = codec.decode_request(body, &env).unwrap();

        let extra = ir
            .extensions
            .get("responses_extra")
            .and_then(|v| v.as_object())
            .expect("responses_extra should exist");
        assert_eq!(extra["multi_agent"]["enabled"], true);
        assert_eq!(extra["multi_agent"]["max_concurrent_subagents"], 4);

        let items = ir
            .extensions
            .get("multi_agent_items")
            .and_then(|v| v.as_array())
            .expect("multi_agent_items should be in extensions");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "multi_agent_call");
        assert_eq!(items[1]["type"], "multi_agent_call_output");

        // Opaque multi-agent items must not create empty IR messages.
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.messages[0].role, Role::User);

        let (encoded, _) = codec.encode_request(&ir).unwrap();
        assert_eq!(encoded["multi_agent"]["enabled"], true);
        assert_eq!(encoded["multi_agent"]["max_concurrent_subagents"], 4);
        let input = encoded["input"].as_array().expect("input array");
        let types: Vec<&str> = input
            .iter()
            .map(|item| {
                item.get("type")
                    .and_then(|v| v.as_str())
                    .or_else(|| item.get("role").map(|_| "message"))
                    .unwrap_or("unknown")
            })
            .collect();
        assert_eq!(
            types,
            vec!["message", "multi_agent_call", "multi_agent_call_output"],
            "multi-agent items must keep original interleaving: {types:?}"
        );
    }

    #[test]
    fn test_custom_tool_definition_roundtrip_and_chat_cross() {
        let responses = ResponsesCodec::new();
        let chat = crate::chat_completions::ChatCompletionsCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5.6",
            "input": "hi",
            "tools": [{
                "type": "custom",
                "name": "code_exec",
                "description": "run code",
                "format": {"type": "text"}
            }]
        });
        let ir = responses.decode_request(body, &env).unwrap();
        assert!(ir.tools[0].is_custom());
        assert!(!ir.tools[0].is_hosted());
        assert_eq!(ir.tools[0].name, "code_exec");
        assert_eq!(
            ir.tools[0]
                .config
                .as_ref()
                .and_then(|c| c.get("format"))
                .and_then(|f| f.get("type")),
            Some(&json!("text"))
        );

        let (encoded, _) = responses.encode_request(&ir).unwrap();
        assert_eq!(encoded["tools"][0]["type"], "custom");
        assert_eq!(encoded["tools"][0]["name"], "code_exec");
        assert_eq!(encoded["tools"][0]["format"]["type"], "text");

        let (chat_encoded, _) = chat.encode_request(&ir).unwrap();
        assert_eq!(chat_encoded["tools"][0]["type"], "custom");
        assert_eq!(chat_encoded["tools"][0]["name"], "code_exec");
        assert_eq!(chat_encoded["tools"][0]["format"]["type"], "text");
    }

    #[test]
    fn test_custom_tool_call_wire_type_roundtrip() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5.6",
            "input": [
                {"role": "user", "content": "run custom tool"},
                {
                    "type": "custom_tool_call",
                    "call_id": "call_custom_1",
                    "name": "my_tool",
                    "input": "some input text"
                },
                {
                    "type": "custom_tool_call_output",
                    "call_id": "call_custom_1",
                    "output": "tool result"
                }
            ]
        });
        let ir = codec.decode_request(body, &env).unwrap();
        assert!(ir.messages.iter().any(|m| {
            m.content.iter().any(|c| {
                matches!(
                    c,
                    Content::ToolCall {
                        wire_type: Some(wt),
                        ..
                    } if wt == "custom_tool_call"
                )
            })
        }));
        assert!(ir.messages.iter().any(|m| {
            m.content.iter().any(|c| {
                matches!(
                    c,
                    Content::ToolResult {
                        wire_type: Some(wt),
                        ..
                    } if wt == "custom_tool_call_output"
                )
            })
        }));

        let (encoded, _) = codec.encode_request(&ir).unwrap();
        let input = encoded["input"].as_array().expect("input");
        assert!(
            input.iter().any(|item| item["type"] == "custom_tool_call"
                && item["name"] == "my_tool"
                && item["input"] == "some input text"),
            "custom_tool_call wire type must survive re-encode: {input:?}"
        );
        assert!(
            input
                .iter()
                .any(|item| item["type"] == "custom_tool_call_output"
                    && item["output"] == "tool result"),
            "custom_tool_call_output wire type must survive re-encode: {input:?}"
        );
    }

    #[test]
    fn test_stream_custom_tool_call_wire_type_fidelity() {
        // Streaming custom_tool_call must preserve wire_type through
        // decode → encode so same-protocol clients do not see a plain
        // function_call.
        let mut decoder = ResponsesStreamDecoder::new();
        let parts = decoder
            .feed(r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"custom_tool_call","id":"ctc_1","call_id":"call_custom_1","name":"my_tool","input":"hello"}}"#)
            .unwrap();
        assert!(
            matches!(
                &parts[0],
                StreamPart::ToolCallDelta { id, name: Some(name), wire_type: Some(wt), .. } if id == "call_custom_1" && name == "my_tool" && wt == "custom_tool_call"
            ),
            "stream decode must set wire_type=custom_tool_call, got: {parts:?}"
        );

        let mut encoder = ResponsesStreamEncoder::new();
        let bytes = encoder.encode_part(&parts[0]).unwrap();
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.contains(r#""type":"custom_tool_call""#),
            "stream re-encode must emit custom_tool_call, got: {s}"
        );
        assert!(
            !s.contains(r#""type":"function_call""#),
            "stream re-encode must not collapse custom_tool_call to function_call: {s}"
        );

        let delta = decoder
            .feed(r#"data: {"type":"response.custom_tool_call_input.delta","output_index":0,"item_id":"ctc_1","delta":" world"}"#)
            .unwrap();
        assert!(matches!(
            &delta[0],
            StreamPart::ToolCallDelta {
                id,
                name: None,
                arguments,
                wire_type: Some(wire_type),
                ..
            } if id == "call_custom_1" && arguments == " world" && wire_type == "custom_tool_call"
        ));
        let encoded_delta = encoder.encode_part(&delta[0]).unwrap();
        let encoded_delta = String::from_utf8_lossy(&encoded_delta);
        assert!(
            encoded_delta.contains(r#""type":"response.custom_tool_call_input.delta""#),
            "custom inputs must use their native delta event: {encoded_delta}"
        );
        assert!(
            !encoded_delta.contains(r#""type":"response.function_call_arguments.delta""#),
            "custom inputs must not use function-call delta events: {encoded_delta}"
        );

        let done = encoder
            .encode_part(&StreamPart::Finish {
                reason: FinishReason::ToolCalls,
            })
            .unwrap();
        let done = String::from_utf8_lossy(&done);
        assert!(
            done.contains(r#""type":"response.custom_tool_call_input.done""#),
            "custom inputs must emit their native done event: {done}"
        );

        let mut done_only_decoder = ResponsesStreamDecoder::new();
        done_only_decoder
            .feed(r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"custom_tool_call","id":"ctc_2","call_id":"call_custom_2","name":"my_tool","input":""}}"#)
            .unwrap();
        let done_only = done_only_decoder
            .feed(r#"data: {"type":"response.custom_tool_call_input.done","output_index":0,"item_id":"ctc_2","input":"final input"}"#)
            .unwrap();
        assert!(matches!(
            &done_only[0],
            StreamPart::ToolCallDelta {
                id,
                name: None,
                arguments,
                wire_type: Some(wire_type),
                ..
            } if id == "call_custom_2" && arguments == "final input" && wire_type == "custom_tool_call"
        ));
    }

    #[test]
    fn test_opaque_items_preserve_interleaving_order() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5.6",
            "input": [
                {"role": "user", "content": "before"},
                {"type": "compaction", "id": "comp_1", "summary": "compacted"},
                {"role": "user", "content": "after"},
                {
                    "type": "multi_agent_call",
                    "id": "ma_1",
                    "name": "spawn_agent",
                    "arguments": {"task": "research"}
                }
            ],
            "multi_agent": {"enabled": true}
        });
        let ir = codec.decode_request(body, &env).unwrap();
        let (encoded, _) = codec.encode_request(&ir).unwrap();
        let input = encoded["input"].as_array().expect("input");
        let types: Vec<&str> = input
            .iter()
            .map(|item| {
                item.get("type")
                    .and_then(|v| v.as_str())
                    .or_else(|| item.get("role").map(|_| "message"))
                    .unwrap_or("unknown")
            })
            .collect();
        assert_eq!(
            types,
            vec!["message", "compaction", "message", "multi_agent_call"],
            "opaque items must keep original interleaving: {types:?}"
        );
        assert_eq!(input[0]["content"], "before");
        assert_eq!(input[2]["content"], "after");
    }

    #[test]
    fn test_opaque_input_keeps_boundaries_before_later_item() {
        let codec = ResponsesCodec::new();
        let env = make_raw_env();
        let body = json!({
            "model": "gpt-5.6",
            "input": [
                {"role": "user", "content": "first"},
                {"role": "user", "content": "second"},
                {"type": "compaction", "id": "comp_1", "summary": "compact"},
                {"role": "user", "content": "third"}
            ]
        });

        let ir = codec.decode_request(body, &env).unwrap();
        let (encoded, _) = codec.encode_request(&ir).unwrap();
        let input = encoded["input"].as_array().expect("input array");
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["content"], "first");
        assert_eq!(input[1]["content"], "second");
        assert_eq!(input[2]["type"], "compaction");
        assert_eq!(input[3]["content"], "third");
    }

    #[test]
    fn test_opaque_output_preserves_later_message_order() {
        let codec = ResponsesCodec::new();
        let body = json!({
            "id": "resp_1",
            "status": "completed",
            "output": [
                {"type": "message", "id": "msg_1", "role": "assistant", "content": [{"type": "output_text", "text": "first"}]},
                {"type": "message", "id": "msg_2", "role": "assistant", "content": [{"type": "output_text", "text": "second"}]},
                {"type": "web_search_call", "id": "ws_1", "status": "completed", "action": {"query": "q"}},
                {"type": "message", "id": "msg_3", "role": "assistant", "content": [{"type": "output_text", "text": "third"}]}
            ]
        });

        let ir = codec.decode_response(body).unwrap();
        let encoded = codec.encode_response(&ir).unwrap();
        let output = encoded["output"].as_array().expect("output array");
        assert_eq!(output.len(), 4);
        assert_eq!(output[0]["content"][0]["text"], "first");
        assert_eq!(output[1]["content"][0]["text"], "second");
        assert_eq!(output[2]["type"], "web_search_call");
        assert_eq!(output[3]["content"][0]["text"], "third");
    }
}
