//! Target-local adaptive compatibility for OpenAI Responses tools.
//!
//! The public Responses request is allowed to contain namespace tools and
//! hosted tool-search declarations.  A number of OpenAI-compatible relays
//! only understand a flat `function` list, however.  This module deliberately
//! lives in the server egress layer: the canonical IR and the Responses codec
//! must not acquire assumptions about one concrete upstream dialect.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use futures::StreamExt;
use serde_json::Value;
use thiserror::Error;

use super::streaming::UpstreamByteStream;

const DEFAULT_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_TOOL_NAME_BYTES: usize = 64;

/// The remembered request dialect for one concrete upstream target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponsesToolMode {
    Native,
    FlatTools,
}

/// Request-scoped reverse mapping used to restore namespace information on the
/// way back from a flat target.  It is intentionally not stored in the
/// process-wide learned-mode cache.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct NamespaceReverseMap {
    entries: HashMap<String, NamespaceTool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NamespaceTool {
    namespace: String,
    name: String,
}

impl NamespaceReverseMap {
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn insert(&mut self, flat_name: String, namespace: String, name: String) {
        self.entries
            .insert(flat_name, NamespaceTool { namespace, name });
    }

    fn lookup(&self, name: &str) -> Option<&NamespaceTool> {
        self.entries.get(name)
    }
}

/// Result of adapting one request body.
#[derive(Debug, Clone)]
pub(crate) struct ResponsesToolAdaptation {
    pub(crate) body: Value,
    pub(crate) reverse_map: NamespaceReverseMap,
    pub(crate) changed: bool,
    pub(crate) lossy: bool,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub(crate) enum ResponsesToolAdapterError {
    #[error("Responses tool namespace collision after flattening: {0}")]
    Collision(String),
    #[error("Responses tool search is dynamic or client-executed and cannot be flattened safely")]
    DynamicToolSearch,
    #[error("Responses tool choice cannot be represented by a flat target")]
    UnsupportedToolChoice,
    #[error("Responses namespace tool `{0}` is missing a complete child tool definition")]
    InvalidNamespaceTool(String),
}

/// Per-process learned modes. The key is generated from the concrete target,
/// endpoint, model and transport, so one provider name never contaminates a
/// different account or relay endpoint.
#[derive(Debug)]
pub(crate) struct ResponsesToolCompatibility {
    modes: DashMap<String, (ResponsesToolMode, Instant)>,
    ttl: Duration,
}

impl Default for ResponsesToolCompatibility {
    fn default() -> Self {
        Self::new()
    }
}

impl ResponsesToolCompatibility {
    pub(crate) fn new() -> Self {
        Self {
            modes: DashMap::new(),
            ttl: DEFAULT_TTL,
        }
    }

    pub(crate) fn get(&self, key: &str) -> ResponsesToolMode {
        let Some(entry) = self.modes.get(key) else {
            return ResponsesToolMode::Native;
        };
        if entry.1.elapsed() > self.ttl {
            drop(entry);
            self.modes.remove(key);
            ResponsesToolMode::Native
        } else {
            entry.0
        }
    }

    pub(crate) fn remember_flat(&self, key: String) {
        self.modes
            .insert(key, (ResponsesToolMode::FlatTools, Instant::now()));
    }

    pub(crate) fn remember_native(&self, key: String) {
        self.modes
            .insert(key, (ResponsesToolMode::Native, Instant::now()));
    }

    pub(crate) fn clear(&self) {
        self.modes.clear();
    }

    #[cfg(test)]
    fn with_ttl(ttl: Duration) -> Self {
        Self {
            modes: DashMap::new(),
            ttl,
        }
    }
}

/// Stable target identity for the learned-mode map.
pub(crate) fn target_key(target: &tiygate_core::RoutingTarget) -> String {
    let transport = target
        .oauth
        .as_ref()
        .map(|oauth| format!("{:?}", oauth.upstream_transport))
        .unwrap_or_else(|| "http".to_string());
    let account = target.account_label.as_deref().or_else(|| {
        target
            .oauth
            .as_ref()
            .and_then(|oauth| oauth.account_id.as_deref())
    });
    let endpoint = target.api_protocol.full_id();
    [
        target.provider_id.as_str(),
        account.unwrap_or(""),
        target.effective_api_base(),
        endpoint.as_str(),
        target.model_id.as_str(),
        transport.as_str(),
    ]
    .iter()
    .map(|part| format!("{}:{}", part.len(), part))
    .collect::<Vec<_>>()
    .join("|")
}

/// Whether a request actually asks for a Responses-only tool dialect.
pub(crate) fn requires_adaptation(body: &Value) -> bool {
    let Some(object) = body.as_object() else {
        return false;
    };
    object
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|tool| {
                let kind = tool.get("type").and_then(Value::as_str);
                kind == Some("namespace")
                    || kind == Some("tool_search")
                    || tool.get("defer_loading").is_some()
            })
        })
        || object.get("additional_tools").is_some()
        || contains_namespace_or_search(object.get("input"))
}

fn contains_namespace_or_search(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    match value {
        Value::Array(items) => items
            .iter()
            .any(|item| contains_namespace_or_search(Some(item))),
        Value::Object(object) => {
            matches!(
                object.get("type").and_then(Value::as_str),
                Some("namespace")
                    | Some("tool_search")
                    | Some("tool_search_call")
                    | Some("tool_search_output")
            ) || object.get("namespace").is_some()
                || object.contains_key("additional_tools")
                || object
                    .values()
                    .any(|item| contains_namespace_or_search(Some(item)))
        }
        _ => false,
    }
}

/// Convert a Responses body to a flat-tool dialect. The function is pure and
/// idempotent: calling it on an already flat body leaves it unchanged.
pub(crate) fn flatten_request(
    body: &Value,
) -> Result<ResponsesToolAdaptation, ResponsesToolAdapterError> {
    let mut body = body.clone();
    let mut reverse_map = NamespaceReverseMap::default();
    let mut changed = false;
    let mut lossy = false;
    let mut names = HashMap::<String, String>::new();

    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        let original = std::mem::take(tools);
        let mut flattened = Vec::with_capacity(original.len());
        for tool in original {
            flatten_tool(
                tool,
                &mut flattened,
                &mut names,
                &mut reverse_map,
                &mut changed,
                &mut lossy,
            )?;
        }
        *tools = flattened;
    }

    if body.get("additional_tools").is_some() {
        let (tools, has_remaining) = {
            let additional_tools = body
                .get_mut("additional_tools")
                .and_then(Value::as_object_mut)
                .ok_or(ResponsesToolAdapterError::DynamicToolSearch)?;
            let tools = additional_tools.remove("tools");
            (tools, !additional_tools.is_empty())
        };
        if let Some(tools) = tools {
            let Some(items) = tools.as_array() else {
                return Err(ResponsesToolAdapterError::DynamicToolSearch);
            };
            let mut flattened = Vec::with_capacity(items.len());
            for tool in items.clone() {
                flatten_tool(
                    tool,
                    &mut flattened,
                    &mut names,
                    &mut reverse_map,
                    &mut changed,
                    &mut lossy,
                )?;
            }
            if let Some(top_tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
                top_tools.extend(flattened);
            } else {
                body["tools"] = Value::Array(flattened);
            }
            changed = true;
            lossy = true;
        }
        if has_remaining {
            // A remaining additional_tools object is a dynamic/client-side
            // orchestration contract. Removing it would silently change the
            // tool set, so fail closed.
            return Err(ResponsesToolAdapterError::DynamicToolSearch);
        }
        body.as_object_mut()
            .map(|object| object.remove("additional_tools"));
    }

    let mut nested_tools = Vec::new();
    if let Some(input) = body.get_mut("input") {
        extract_nested_additional_tools(input, &mut nested_tools)?;
    }
    if !nested_tools.is_empty() {
        let mut flattened = Vec::with_capacity(nested_tools.len());
        for tool in nested_tools {
            flatten_tool(
                tool,
                &mut flattened,
                &mut names,
                &mut reverse_map,
                &mut changed,
                &mut lossy,
            )?;
        }
        if let Some(top_tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
            top_tools.extend(flattened);
        } else {
            body["tools"] = Value::Array(flattened);
        }
        changed = true;
        lossy = true;
    }

    if let Some(input) = body.get_mut("input") {
        rewrite_history(input, &names, &mut changed, &mut lossy)?;
    }
    if let Some(tool_choice) = body.get_mut("tool_choice") {
        rewrite_tool_choice(tool_choice, &names, &mut changed, &mut lossy)?;
    }

    Ok(ResponsesToolAdaptation {
        body,
        reverse_map,
        changed,
        lossy,
    })
}

fn extract_nested_additional_tools(
    value: &mut Value,
    output: &mut Vec<Value>,
) -> Result<(), ResponsesToolAdapterError> {
    match value {
        Value::Array(items) => {
            for item in items {
                extract_nested_additional_tools(item, output)?;
            }
        }
        Value::Object(object) => {
            if let Some(additional) = object.remove("additional_tools") {
                let Some(mut additional) = additional.as_object().cloned() else {
                    return Err(ResponsesToolAdapterError::DynamicToolSearch);
                };
                let Some(tools) = additional.remove("tools") else {
                    return Err(ResponsesToolAdapterError::DynamicToolSearch);
                };
                let Some(tools) = tools.as_array() else {
                    return Err(ResponsesToolAdapterError::DynamicToolSearch);
                };
                if !additional.is_empty() {
                    return Err(ResponsesToolAdapterError::DynamicToolSearch);
                }
                output.extend(tools.iter().cloned());
            }
            for child in object.values_mut() {
                extract_nested_additional_tools(child, output)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn flatten_tool(
    mut tool: Value,
    output: &mut Vec<Value>,
    names: &mut HashMap<String, String>,
    reverse_map: &mut NamespaceReverseMap,
    changed: &mut bool,
    lossy: &mut bool,
) -> Result<(), ResponsesToolAdapterError> {
    let kind = tool.get("type").and_then(Value::as_str);
    if kind == Some("namespace") {
        let namespace = tool
            .get("namespace")
            .or_else(|| tool.get("name"))
            .and_then(Value::as_str)
            .ok_or_else(|| ResponsesToolAdapterError::InvalidNamespaceTool("<unnamed>".into()))?
            .to_string();
        let children = tool
            .get_mut("tools")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| ResponsesToolAdapterError::InvalidNamespaceTool(namespace.clone()))?;
        let children = std::mem::take(children);
        for child in children {
            let Some(child_object) = child.as_object() else {
                return Err(ResponsesToolAdapterError::InvalidNamespaceTool(
                    namespace.clone(),
                ));
            };
            let Some(local_name) = child_object.get("name").and_then(Value::as_str) else {
                return Err(ResponsesToolAdapterError::InvalidNamespaceTool(
                    namespace.clone(),
                ));
            };
            let flat_name = flatten_name(&namespace, local_name);
            if names.contains_key(&flat_name) {
                return Err(ResponsesToolAdapterError::Collision(flat_name));
            }
            names.insert(flat_name.clone(), namespace.clone());
            reverse_map.insert(flat_name.clone(), namespace.clone(), local_name.to_string());
            let mut flat = child.clone();
            let object = flat.as_object_mut().ok_or_else(|| {
                ResponsesToolAdapterError::InvalidNamespaceTool(namespace.clone())
            })?;
            object.insert("type".into(), Value::String("function".into()));
            object.insert("name".into(), Value::String(flat_name));
            object.remove("namespace");
            object.remove("defer_loading");
            output.push(flat);
        }
        *changed = true;
        *lossy = true;
        return Ok(());
    }
    if kind == Some("tool_search") {
        if tool
            .get("execution")
            .or_else(|| tool.get("mode"))
            .and_then(Value::as_str)
            .is_some_and(|value| matches!(value, "client" | "client_executed" | "dynamic"))
        {
            return Err(ResponsesToolAdapterError::DynamicToolSearch);
        }
        let Some(children) = tool.get("tools").and_then(Value::as_array) else {
            return Err(ResponsesToolAdapterError::DynamicToolSearch);
        };
        for child in children.clone() {
            flatten_tool(child, output, names, reverse_map, changed, lossy)?;
        }
        *changed = true;
        *lossy = true;
        return Ok(());
    }

    if let Some(object) = tool.as_object_mut() {
        if object.get("defer_loading").is_some() {
            object.remove("defer_loading");
            *changed = true;
            *lossy = true;
        }
        if object.get("type").and_then(Value::as_str) == Some("function")
            && object.get("name").and_then(Value::as_str).is_none()
        {
            return Err(ResponsesToolAdapterError::InvalidNamespaceTool(
                "<unnamed>".into(),
            ));
        }
    }
    if let Some(name) = tool.get("name").and_then(Value::as_str) {
        if names.contains_key(name) {
            return Err(ResponsesToolAdapterError::Collision(name.to_string()));
        }
        names.insert(name.to_string(), String::new());
    }
    output.push(tool);
    Ok(())
}

fn flatten_name(namespace: &str, name: &str) -> String {
    let raw = format!("{namespace}__{name}");
    if raw.len() <= MAX_TOOL_NAME_BYTES {
        return raw;
    }
    use sha2::Digest;
    let digest = sha2::Sha256::digest(raw.as_bytes());
    let suffix = format!("__{}", hex::encode(&digest[..5]));
    let limit = MAX_TOOL_NAME_BYTES.saturating_sub(suffix.len());
    let mut end = limit.min(raw.len());
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &raw[..end], suffix)
}

fn rewrite_history(
    value: &mut Value,
    names: &HashMap<String, String>,
    changed: &mut bool,
    lossy: &mut bool,
) -> Result<(), ResponsesToolAdapterError> {
    match value {
        Value::Array(items) => {
            let mut retained = Vec::with_capacity(items.len());
            for mut item in std::mem::take(items) {
                let kind = item.get("type").and_then(Value::as_str);
                if matches!(kind, Some("tool_search_call") | Some("tool_search_output")) {
                    if item
                        .get("execution")
                        .or_else(|| item.get("mode"))
                        .and_then(Value::as_str)
                        .is_some_and(|value| {
                            matches!(value, "client" | "client_executed" | "dynamic")
                        })
                    {
                        return Err(ResponsesToolAdapterError::DynamicToolSearch);
                    }
                    *changed = true;
                    *lossy = true;
                    continue;
                }
                rewrite_history(&mut item, names, changed, lossy)?;
                retained.push(item);
            }
            *items = retained;
        }
        Value::Object(object) => {
            let kind = object.get("type").and_then(Value::as_str);
            if matches!(
                kind,
                Some("function_call")
                    | Some("custom_tool_call")
                    | Some("tool_call")
                    | Some("mcp_tool_call")
            ) {
                if let Some(namespace) = object.get("namespace").and_then(Value::as_str) {
                    let local = object.get("name").and_then(Value::as_str).unwrap_or("");
                    let flat = flatten_name(namespace, local);
                    if !names.contains_key(&flat) {
                        return Err(ResponsesToolAdapterError::UnsupportedToolChoice);
                    }
                    object.insert("name".into(), Value::String(flat));
                    object.remove("namespace");
                    *changed = true;
                    *lossy = true;
                }
            }
            for child in object.values_mut() {
                rewrite_history(child, names, changed, lossy)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn rewrite_tool_choice(
    value: &mut Value,
    names: &HashMap<String, String>,
    changed: &mut bool,
    lossy: &mut bool,
) -> Result<(), ResponsesToolAdapterError> {
    let Some(object) = value.as_object_mut() else {
        return Ok(());
    };
    if object.get("type").and_then(Value::as_str) == Some("namespace") {
        *value = Value::String("auto".into());
        *changed = true;
        *lossy = true;
        return Ok(());
    }
    if let Some(namespace) = object.get("namespace").and_then(Value::as_str) {
        let local = object.get("name").and_then(Value::as_str).unwrap_or("");
        let flat = flatten_name(namespace, local);
        if !names.contains_key(&flat) {
            return Err(ResponsesToolAdapterError::UnsupportedToolChoice);
        }
        object.insert("name".into(), Value::String(flat));
        object.remove("namespace");
        *changed = true;
        *lossy = true;
    }
    Ok(())
}

/// Restore namespace/name fields in a Responses JSON response. The recursive
/// walk handles output items and the nested `response.completed.response`
/// shape without touching unrelated message metadata.
pub(crate) fn restore_response(body: &mut Value, reverse_map: &NamespaceReverseMap) {
    if reverse_map.is_empty() {
        return;
    }
    restore_value(body, reverse_map);
}

fn restore_value(value: &mut Value, reverse_map: &NamespaceReverseMap) {
    match value {
        Value::Array(items) => {
            for item in items {
                restore_value(item, reverse_map);
            }
        }
        Value::Object(object) => {
            if matches!(
                object.get("type").and_then(Value::as_str),
                Some("function_call")
                    | Some("custom_tool_call")
                    | Some("tool_call")
                    | Some("mcp_tool_call")
            ) {
                if let Some(name) = object.get("name").and_then(Value::as_str) {
                    if let Some(original) = reverse_map.lookup(name) {
                        object.insert(
                            "namespace".into(),
                            Value::String(original.namespace.clone()),
                        );
                        object.insert("name".into(), Value::String(original.name.clone()));
                    }
                }
            }
            for child in object.values_mut() {
                restore_value(child, reverse_map);
            }
        }
        _ => {}
    }
}

/// Restore a flat Responses SSE stream without buffering the complete model
/// response. Events are buffered only until their blank-line SSE delimiter;
/// once an output item is forwarded, no retry is possible upstream.
pub(crate) fn restore_sse_stream(
    mut upstream: UpstreamByteStream,
    reverse_map: NamespaceReverseMap,
) -> UpstreamByteStream {
    Box::pin(async_stream::stream! {
        let mut buffer = Vec::new();
        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(bytes) => {
                    buffer.extend_from_slice(&bytes);
                    while let Some(end) = event_end(&buffer) {
                        let event = buffer.drain(..end).collect::<Vec<_>>();
                        yield Ok(restore_sse_event(event, &reverse_map));
                    }
                }
                Err(error) => {
                    yield Err(error);
                    return;
                }
            }
        }
        if !buffer.is_empty() {
            yield Ok(restore_sse_event(buffer, &reverse_map));
        }
    })
}

fn event_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| index + 2)
        .or_else(|| {
            buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|index| index + 4)
        })
}

fn restore_sse_event(mut event: Vec<u8>, reverse_map: &NamespaceReverseMap) -> bytes::Bytes {
    let text = String::from_utf8_lossy(&event).to_string();
    let mut output = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let (prefix, payload) = if let Some(payload) = line.strip_prefix("data:") {
            ("data:", payload)
        } else {
            output.push_str(line);
            continue;
        };
        let newline = if payload.ends_with('\n') { "\n" } else { "" };
        let raw = payload.trim_end_matches(['\r', '\n']).trim();
        if raw.is_empty() || raw == "[DONE]" {
            output.push_str(line);
            continue;
        }
        let Ok(mut value) = serde_json::from_str::<Value>(raw) else {
            output.push_str(line);
            continue;
        };
        restore_response(&mut value, reverse_map);
        output.push_str(prefix);
        output.push(' ');
        output.push_str(&value.to_string());
        output.push_str(newline);
    }
    event.clear();
    event.extend_from_slice(output.as_bytes());
    bytes::Bytes::from(event)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    fn namespace_body() -> Value {
        json!({
            "model": "m",
            "input": [{"type":"function_call","namespace":"collaboration","name":"spawn_agent","call_id":"c1"}],
            "tools": [{"type":"namespace","namespace":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"spawn","parameters":{"type":"object"}}]}],
            "tool_choice": {"type":"function","namespace":"collaboration","name":"spawn_agent"}
        })
    }

    #[test]
    fn flatten_and_restore_namespace() {
        let adaptation = flatten_request(&namespace_body()).unwrap();
        assert_eq!(
            adaptation.body["tools"][0]["name"],
            "collaboration__spawn_agent"
        );
        assert_eq!(
            adaptation.body["input"][0]["name"],
            "collaboration__spawn_agent"
        );
        assert_eq!(
            adaptation.body["tool_choice"]["name"],
            "collaboration__spawn_agent"
        );
        let mut response =
            json!({"output":[{"type":"function_call","name":"collaboration__spawn_agent"}]});
        restore_response(&mut response, &adaptation.reverse_map);
        assert_eq!(response["output"][0]["namespace"], "collaboration");
        assert_eq!(response["output"][0]["name"], "spawn_agent");
    }

    #[test]
    fn collisions_are_rejected() {
        let body = json!({"tools":[
            {"type":"function","name":"a__b"},
            {"type":"namespace","namespace":"a","tools":[{"type":"function","name":"b"}]}
        ]});
        assert!(matches!(
            flatten_request(&body),
            Err(ResponsesToolAdapterError::Collision(_))
        ));
    }

    #[test]
    fn long_names_are_utf8_safe_and_bounded() {
        let body = json!({"tools":[{"type":"namespace","namespace":"命名空间命名空间命名空间","tools":[{"type":"function","name":"工具工具工具工具工具工具工具工具"}]}]});
        let adaptation = flatten_request(&body).unwrap();
        let name = adaptation.body["tools"][0]["name"].as_str().unwrap();
        assert!(name.len() <= MAX_TOOL_NAME_BYTES);
        assert!(std::str::from_utf8(name.as_bytes()).is_ok());
    }

    #[test]
    fn dynamic_search_is_not_silently_deleted() {
        let body = json!({"tools":[{"type":"tool_search"}]});
        assert!(matches!(
            flatten_request(&body),
            Err(ResponsesToolAdapterError::DynamicToolSearch)
        ));
    }

    #[test]
    fn nested_additional_tools_are_promoted() {
        let body = json!({
            "input": [{
                "type": "message",
                "additional_tools": {
                    "tools": [{"type": "namespace", "namespace": "ops", "tools": [{"name": "run"}]}]
                }
            }]
        });
        let adaptation = flatten_request(&body).unwrap();
        assert_eq!(adaptation.body["tools"][0]["name"], "ops__run");
        assert!(adaptation.body["input"][0]
            .get("additional_tools")
            .is_none());
    }

    #[test]
    fn learned_mode_expires() {
        let cache = ResponsesToolCompatibility::with_ttl(Duration::from_millis(0));
        cache.remember_flat("key".into());
        assert_eq!(cache.get("key"), ResponsesToolMode::Native);
    }
}
