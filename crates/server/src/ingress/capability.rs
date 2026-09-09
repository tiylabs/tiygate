//! Built-in provider capability profiles.
//!
//! Each profile declares the field-level capability gaps of a provider on a
//! specific protocol endpoint, plus an optional structural validator for the
//! parts that do not fit a pure field rule (tool allow-lists, input-item
//! content checks, per-item transforms). See
//! `docs/adr/0001-declarative-provider-capability-profile.md`.
//!
//! These live in the server ingress layer (always compiled) rather than in the
//! optional `tiygate-providers` crate, because the egress sanitizer runs on the
//! always-on data path regardless of whether the `providers` feature is enabled.

use std::sync::OnceLock;

use serde_json::Value;
use tiygate_core::{
    convert_reasoning_to_text, is_encrypted_only_reasoning, is_meaningful, CapabilityProfile,
    CapabilityReject, FieldAction, FieldRule, MatchCond, ProtocolEndpoint, ProtocolSuite,
    StructureValidator,
};

/// The DeepSeek Responses capability profile.
///
/// Applied only when the egress target is the DeepSeek Responses endpoint
/// (see `headers::is_official_deepseek_responses_target`).
pub(super) fn deepseek_responses_profile() -> &'static CapabilityProfile {
    static PROFILE: OnceLock<CapabilityProfile> = OnceLock::new();
    PROFILE.get_or_init(build_deepseek_responses_profile)
}

fn build_deepseek_responses_profile() -> CapabilityProfile {
    CapabilityProfile {
        endpoint: ProtocolEndpoint::new(ProtocolSuite::OpenAiResponses, "deepseek-responses", "v1"),
        fields: deepseek_field_rules(),
        structure: Some(Box::new(DeepSeekResponsesValidator)),
        allow_overrides: true,
    }
}

/// Top-level and nested field rules. Strips run first (DeepSeek silently
/// ignores these), then meaningful-field rejects.
fn deepseek_field_rules() -> Vec<FieldRule> {
    let strip = |path: &str, drop_container_when_empty: bool| FieldRule {
        path: path.to_string(),
        action: FieldAction::Strip,
        convert: None,
        cond: MatchCond::Present,
        reason: format!("{path} is not parsed by DeepSeek and is removed"),
        extra: None,
        drop_container_when_empty,
    };
    let reject = |path: &str, cond: MatchCond, reason: &str| FieldRule {
        path: path.to_string(),
        action: FieldAction::Reject,
        convert: None,
        cond,
        reason: reason.to_string(),
        extra: None,
        drop_container_when_empty: false,
    };
    let unsupported =
        |path: &str, cond: MatchCond| reject(path, cond, &format!("{path} is not supported"));

    // Bind the vector to a local so the `vec!` macro tail sits in
    // statement position (a bare macro in return position trips the
    // rust-analyzer E0308 expansion false positive).
    let rules = vec![
        // DeepSeek manages context caching automatically and never parses these
        // non-semantic controls, so strip them instead of rejecting.
        strip("prompt_cache_key", false),
        strip("prompt_cache_retention", false),
        strip("prompt_cache_options", false),
        strip("metadata", false),
        strip("include", false),
        // DeepSeek accepts but ignores `text.verbosity` and `reasoning.summary`;
        // drop the container when it becomes empty.
        strip("text.verbosity", true),
        strip("reasoning.summary", true),
        // Semantic fields DeepSeek does not support.
        unsupported("previous_response_id", MatchCond::Meaningful),
        unsupported("conversation", MatchCond::Meaningful),
        unsupported("background", MatchCond::Meaningful),
        unsupported("max_tool_calls", MatchCond::Meaningful),
        unsupported("prompt", MatchCond::Meaningful),
        unsupported("service_tier", MatchCond::Meaningful),
        unsupported("safety_identifier", MatchCond::Meaningful),
        unsupported("context_management", MatchCond::Meaningful),
        unsupported("stream_options", MatchCond::Meaningful),
        unsupported("client_metadata", MatchCond::Meaningful),
        unsupported("multi_agent", MatchCond::Meaningful),
        unsupported("stop", MatchCond::Meaningful),
        unsupported("presence_penalty", MatchCond::Meaningful),
        unsupported("frequency_penalty", MatchCond::Meaningful),
        unsupported("seed", MatchCond::Meaningful),
        unsupported("n", MatchCond::Meaningful),
        unsupported("logprobs", MatchCond::Meaningful),
        // Conditional rejects.
        reject(
            "store",
            MatchCond::EqBool(true),
            "store=true is not supported",
        ),
        reject(
            "parallel_tool_calls",
            MatchCond::EqBool(false),
            "parallel_tool_calls=false is not supported; DeepSeek always enables it",
        ),
        reject(
            "truncation",
            MatchCond::ValueIn(vec!["disabled".to_string()]),
            "automatic truncation is not supported",
        ),
    ];
    rules
}

/// Structural validation: tool allow-lists and input-item content checks /
/// transforms that a pure field rule cannot express.
///
/// Visible to sibling ingress modules so the profile-selection logic in
/// `headers.rs` can re-attach the built-in hook when a provider-declared
/// capability overlay replaces the built-in field rules (the hook itself
/// is never overridable — overlays carry rules only).
pub(super) struct DeepSeekResponsesValidator;

impl StructureValidator for DeepSeekResponsesValidator {
    fn validate(&self, body: &mut Value) -> Result<bool, CapabilityReject> {
        validate_tools(body)?;
        prepare_input_items(body)
    }
}

fn reject(field: &str, message: impl Into<String>) -> CapabilityReject {
    let message = message.into();
    CapabilityReject {
        field: field.to_string(),
        action: FieldAction::Reject,
        reason: message.clone(),
        detail: message,
    }
}

/// Allowed tools: function, web search, and the `apply_patch` custom tool.
fn validate_tools(body: &Value) -> Result<(), CapabilityReject> {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return Ok(());
    };
    for tool in tools {
        match tool.get("type").and_then(Value::as_str) {
            Some("function") => {}
            Some("web_search") | Some("web_search_2025_08_26") => {
                if tool.get("search_context_size").is_some_and(is_meaningful)
                    || tool.get("user_location").is_some_and(is_meaningful)
                {
                    return Err(reject(
                        "web_search",
                        "web_search search_context_size and user_location are ignored",
                    ));
                }
            }
            Some("custom") if tool.get("name").and_then(Value::as_str) == Some("apply_patch") => {}
            Some("custom") => {
                return Err(reject(
                    "custom",
                    "only the apply_patch custom tool is supported",
                ));
            }
            Some(other) => {
                return Err(reject(
                    "tool",
                    format!("tool type {other} is not supported"),
                ));
            }
            None => return Err(reject("tool", "tool type is required")),
        }
    }
    Ok(())
}

/// Process `input[]` items: convert / drop reasoning items, validate message
/// and tool-output content, and enforce the `apply_patch` custom-tool rule.
fn prepare_input_items(body: &mut Value) -> Result<bool, CapabilityReject> {
    let mut mutated = false;
    let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return Ok(false);
    };
    let mut drop_indices: Vec<usize> = Vec::new();
    for (idx, item) in items.iter_mut().enumerate() {
        let item_type = item.get("type").and_then(Value::as_str);
        match item_type {
            Some("reasoning") => match prepare_reasoning(item)? {
                ReasoningOutcome::Drop => drop_indices.push(idx),
                ReasoningOutcome::Keep(changed) => mutated |= changed,
            },
            Some("message") | None if item.get("role").is_some() => {
                validate_message_content(item)?;
            }
            Some("function_call") | Some("web_search_call") => {}
            Some("function_call_output") | Some("custom_tool_call_output") => {
                validate_tool_output(item)?;
            }
            Some("custom_tool_call") => {
                if item.get("name").and_then(Value::as_str) != Some("apply_patch") {
                    return Err(reject(
                        "custom_tool_call",
                        "only apply_patch custom_tool_call items are supported",
                    ));
                }
            }
            Some(other) => {
                return Err(reject(
                    "input",
                    format!("input item type {other} is not supported"),
                ));
            }
            None => {
                return Err(reject(
                    "input",
                    "input item must have a supported type or role",
                ));
            }
        }
    }
    if !drop_indices.is_empty() {
        for idx in drop_indices.into_iter().rev() {
            items.remove(idx);
        }
        mutated = true;
    }
    Ok(mutated)
}

enum ReasoningOutcome {
    /// Keep the item; the bool reports whether its body was mutated.
    Keep(bool),
    /// Drop the whole item — nothing DeepSeek can consume remains.
    Drop,
}

fn prepare_reasoning(item: &mut Value) -> Result<ReasoningOutcome, CapabilityReject> {
    // DeepSeek cannot decrypt OpenAI-style encrypted reasoning. Strip the blob
    // and keep any plaintext; drop encrypted-only shells entirely.
    if item.get("encrypted_content").is_some_and(is_meaningful) && is_encrypted_only_reasoning(item)
    {
        return Ok(ReasoningOutcome::Drop);
    }
    if let Some(parts) = item.get("content").and_then(Value::as_array) {
        for part in parts {
            if part.get("type").and_then(Value::as_str) != Some("reasoning_text") {
                return Err(reject(
                    "reasoning",
                    "reasoning content only supports reasoning_text parts",
                ));
            }
        }
    }
    let has_content = item
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|parts| !parts.is_empty());
    if !has_content && summary_text(item).is_none() {
        return Err(reject(
            "reasoning",
            "reasoning input must contain reasoning_text content",
        ));
    }
    Ok(ReasoningOutcome::Keep(convert_reasoning_to_text(item)))
}

fn summary_text(item: &Value) -> Option<String> {
    item.get("summary")
        .and_then(Value::as_array)
        .map(|summary| {
            summary
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .filter(|text| !text.is_empty())
        .or_else(|| item.get("text").and_then(Value::as_str).map(str::to_string))
}

fn validate_message_content(item: &Value) -> Result<(), CapabilityReject> {
    let Some(parts) = item.get("content").and_then(Value::as_array) else {
        return Ok(());
    };
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text") | Some("output_text") | Some("input_image") => {}
            Some("input_file") => {
                return Err(reject("input_file", "input_file content is not supported"));
            }
            Some(other) => {
                return Err(reject(
                    "message",
                    format!("message content type {other} is not supported"),
                ));
            }
            None => {}
        }
    }
    Ok(())
}

fn validate_tool_output(item: &Value) -> Result<(), CapabilityReject> {
    let Some(parts) = item.get("output").and_then(Value::as_array) else {
        return Ok(());
    };
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text") | Some("input_image") => {}
            Some(other) => {
                return Err(reject(
                    "tool_output",
                    format!("tool output content type {other} is not supported"),
                ));
            }
            None => {}
        }
    }
    Ok(())
}
