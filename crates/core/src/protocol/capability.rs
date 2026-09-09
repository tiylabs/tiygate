//! Declarative provider capability profiles and a generic request-body sanitizer.
//!
//! Replaces provider-specific capabilities hard-coded in the server ingress
//! layer with a data-driven profile: a new capability gap is added by declaring
//! a [`FieldRule`], not by adding an `if` branch. See
//! [`docs/adr/0001-declarative-provider-capability-profile.md`].
//!
//! Design split (ADR-0001):
//! - [`FieldRule`] covers uniform top-level / nested object fields
//!   (`strip` / `reject` / `convert` + a match condition).
//! - [`StructureValidator`] covers the parts that do not fit a pure field rule
//!   — tool allow-lists, per-input-item content checks and transforms, and
//!   conditional boolean rejections.
//! - [`sanitize_with_profile`] runs the rules in order, returns a
//!   [`SanitizeOutcome`] carrying both a `mutated` flag (drives passthrough vs
//!   re-serialization) and the structured [`CapabilityDecision`]s for logging.

use serde::{Deserialize, Serialize};

use crate::protocol::ProtocolEndpoint;

/// How a field (or node) should be handled for a given provider endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldAction {
    /// Keep the field as-is (explicitly supported).
    Support,
    /// Remove the field (the provider silently ignores it).
    Strip,
    /// Reject the request because the field is not supported.
    Reject,
    /// Rewrite the field into the provider's supported shape.
    Convert,
}

impl FieldAction {
    /// Stable lowercase label used in logs and error payloads.
    pub fn label(self) -> &'static str {
        match self {
            Self::Support => "support",
            Self::Strip => "strip",
            Self::Reject => "reject",
            Self::Convert => "convert",
        }
    }
}

/// The concrete transformation applied by a [`FieldAction::Convert`] rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvertKind {
    /// Convert an OpenAI-style reasoning `summary`/`text` to DeepSeek
    /// `reasoning_text` content and drop `summary`/`text`/`encrypted_content`.
    SummariesToReasoningText,
    /// Drop the whole node (e.g. an encrypted-only reasoning shell).
    DropItem,
}

/// When a rule fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "cond")]
pub enum MatchCond {
    /// Fire whenever the field is present (any value).
    Present,
    /// Fire when the field is present and "meaningful" (non-empty
    /// string/array/object, boolean true, or a number).
    #[default]
    Meaningful,
    /// Fire when the field is present and equals the given boolean.
    EqBool(bool),
    /// Fire when the field is present and its string value is NOT in the
    /// allowed set (e.g. `truncation` only permits `"disabled"`).
    #[serde(rename = "value_in")]
    ValueIn(Vec<String>),
}

/// A single declarative field rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldRule {
    /// Dotted object path, e.g. `"prompt_cache_key"` or `"text.verbosity"`.
    /// Array-item paths belong in a [`StructureValidator`], not here.
    pub path: String,
    pub action: FieldAction,
    /// The conversion to apply when `action == Convert`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub convert: Option<ConvertKind>,
    #[serde(default)]
    pub cond: MatchCond,
    /// Stable human-readable reason used in logs and the error `detail`.
    pub reason: String,
    /// Optional suffix appended to `reason` for the rendered `detail`
    /// (e.g. `"only apply_patch"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<String>,
    /// For `Strip` on a nested object field: drop the parent container once it
    /// becomes empty after removal (mirrors the previous
    /// `strip_ignored_deepseek_control` behaviour).
    #[serde(default)]
    pub drop_container_when_empty: bool,
}

impl FieldRule {
    /// Rendered human-readable detail for a reject/decision payload.
    pub fn rendered_detail(&self) -> String {
        match &self.extra {
            Some(extra) => format!("{}: {}", self.reason, extra),
            None => self.reason.clone(),
        }
    }
}

/// A structured capability decision, captured for observability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityDecision {
    pub field: String,
    pub action: FieldAction,
    pub reason: String,
    pub profile_endpoint: String,
}

/// A structured capability rejection carrying a machine-readable payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityReject {
    pub field: String,
    pub action: FieldAction,
    pub reason: String,
    /// Human-readable detail (for display; no stable prefix dependency).
    pub detail: String,
}

impl std::fmt::Display for CapabilityReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

/// Result of running a [`CapabilityProfile`] against a request body.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SanitizeOutcome {
    /// Whether the body was mutated — drives passthrough vs re-serialization.
    pub mutated: bool,
    /// Structured decisions, for logging / observability.
    pub decisions: Vec<CapabilityDecision>,
    /// Set when a `Reject` rule fired; the request must be refused.
    pub reject: Option<CapabilityReject>,
}

/// Provider-specific structural validation that declarative field rules cannot
/// express — e.g. tool allow-lists, input-item content-type checks, and
/// per-item transforms. Returns whether the body was mutated, or a
/// [`CapabilityReject`].
pub trait StructureValidator: Send + Sync {
    fn validate(&self, body: &mut serde_json::Value) -> Result<bool, CapabilityReject>;
}

/// A provider capability profile, keyed by `(protocol endpoint, provider)`.
pub struct CapabilityProfile {
    /// The protocol endpoint this profile applies to.
    pub endpoint: ProtocolEndpoint,
    /// Ordered field rules; the first rule matching a given path wins.
    pub fields: Vec<FieldRule>,
    /// Optional structural validator (tool allow-lists, input items, etc.).
    pub structure: Option<Box<dyn StructureValidator>>,
    /// Whether a built-in profile may be overridden via a DB overlay.
    pub allow_overrides: bool,
}

/// Whether a JSON value is "meaningful" — non-empty and non-null-ish. Mirrors
/// the previous `is_meaningful` used by the DeepSeek ingress profile.
pub fn is_meaningful(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::String(value) => !value.is_empty(),
        serde_json::Value::Array(value) => !value.is_empty(),
        serde_json::Value::Object(value) => !value.is_empty(),
        serde_json::Value::Number(_) => true,
    }
}

/// Convert a single reasoning item to DeepSeek's `reasoning_text` form:
/// materialise `content` from a `summary`/`text`, then drop
/// `summary`/`text`/`encrypted_content`. Returns whether the node changed.
///
/// Reusable by a [`StructureValidator`] that walks `input[]` items.
pub fn convert_reasoning_to_text(item: &mut serde_json::Value) -> bool {
    let mut mutated = false;
    let summary_text = item
        .get("summary")
        .and_then(serde_json::Value::as_array)
        .map(|summary| {
            summary
                .iter()
                .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .filter(|text| !text.is_empty())
        .or_else(|| {
            item.get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
    let has_content = item
        .get("content")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|parts| !parts.is_empty());
    if !has_content {
        if let Some(text) = summary_text {
            item["content"] = serde_json::json!([{ "type": "reasoning_text", "text": text }]);
            mutated = true;
        }
    }
    if let Some(object) = item.as_object_mut() {
        mutated |= object.remove("summary").is_some();
        mutated |= object.remove("text").is_some();
        mutated |= object.remove("encrypted_content").is_some();
    }
    mutated
}

/// Whether a reasoning input item is an encrypted-only shell with nothing a
/// provider that cannot decrypt can consume. Reusable by a structure validator.
pub fn is_encrypted_only_reasoning(item: &serde_json::Value) -> bool {
    let has_plaintext = item
        .get("content")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|parts| !parts.is_empty())
        || item
            .get("summary")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|parts| !parts.is_empty())
        || item
            .get("text")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| !text.is_empty());
    !has_plaintext
}

/// Run a [`CapabilityProfile`] against a request body, applying strips,
/// converts, and rejects in rule order. Returns the outcome, or the first
/// rejection.
pub fn sanitize_with_profile(
    body: &mut serde_json::Value,
    profile: &CapabilityProfile,
) -> Result<SanitizeOutcome, CapabilityReject> {
    let mut outcome = SanitizeOutcome::default();
    let endpoint = profile.endpoint.canonical();

    // A given path is decided by its first matching rule; later rules on the
    // same path do not re-run.
    let mut decided_paths: std::collections::HashSet<String> = std::collections::HashSet::new();

    for rule in &profile.fields {
        if decided_paths.contains(&rule.path) {
            continue; // an earlier rule already decided this path
        }
        let Some(node) = resolve_path(body, &rule.path) else {
            continue; // field absent
        };
        if !cond_matches(&rule.cond, node) {
            continue; // condition not met on the resolved node
        }
        decided_paths.insert(rule.path.clone());

        match rule.action {
            FieldAction::Support => {}
            FieldAction::Strip => {
                if remove_path(body, &rule.path, rule.drop_container_when_empty) {
                    outcome.mutated = true;
                    outcome.decisions.push(CapabilityDecision {
                        field: rule.path.clone(),
                        action: FieldAction::Strip,
                        reason: rule.reason.clone(),
                        profile_endpoint: endpoint.clone(),
                    });
                }
            }
            FieldAction::Reject => {
                return Err(CapabilityReject {
                    field: rule.path.clone(),
                    action: FieldAction::Reject,
                    reason: rule.reason.clone(),
                    detail: rule.rendered_detail(),
                });
            }
            FieldAction::Convert => {
                let converted = match rule.convert.unwrap_or(ConvertKind::SummariesToReasoningText)
                {
                    ConvertKind::SummariesToReasoningText => match resolve_path_mut(body, &rule.path)
                    {
                        Some(node) => convert_reasoning_to_text(node),
                        None => false,
                    },
                    ConvertKind::DropItem => remove_path(body, &rule.path, false),
                };
                if converted {
                    outcome.mutated = true;
                    outcome.decisions.push(CapabilityDecision {
                        field: rule.path.clone(),
                        action: FieldAction::Convert,
                        reason: rule.reason.clone(),
                        profile_endpoint: endpoint.clone(),
                    });
                }
            }
        }
    }

    if let Some(structure) = &profile.structure {
        outcome.mutated |= structure.validate(body)?;
    }

    Ok(outcome)
}

fn cond_matches(cond: &MatchCond, node: &serde_json::Value) -> bool {
    match cond {
        MatchCond::Present => true,
        MatchCond::Meaningful => is_meaningful(node),
        MatchCond::EqBool(expected) => node.as_bool() == Some(*expected),
        MatchCond::ValueIn(allowed) => match node.as_str() {
            Some(value) => !allowed.iter().any(|allowed| allowed == value),
            // Present but not a string → the rule does not fire (mirrors the
            // previous `truncation` check which required a string value).
            None => false,
        },
    }
}

/// Resolve a dotted object path against a JSON value. Returns `None` when a
/// segment is missing or refers to an array (array-item paths are the province
/// of [`StructureValidator`]).
fn resolve_path<'a>(root: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = root;
    for segment in path.split('.') {
        if segment.is_empty() {
            return None;
        }
        current = current.get(segment)?;
    }
    Some(current)
}

/// Mutable counterpart of [`resolve_path`].
fn resolve_path_mut<'a>(
    root: &'a mut serde_json::Value,
    path: &str,
) -> Option<&'a mut serde_json::Value> {
    let mut current = root;
    for segment in path.split('.') {
        if segment.is_empty() {
            return None;
        }
        current = current.get_mut(segment)?;
    }
    Some(current)
}

/// Remove the value at a dotted path. When `drop_empty_container` is set and the
/// immediate parent object becomes empty after removal, the parent is removed too.
fn remove_path(root: &mut serde_json::Value, path: &str, drop_empty_container: bool) -> bool {
    let segments: Vec<&str> = path.split('.').filter(|segment| !segment.is_empty()).collect();
    remove_path_inner(root, &segments, drop_empty_container)
}

fn remove_path_inner(
    node: &mut serde_json::Value,
    segments: &[&str],
    drop_empty_container: bool,
) -> bool {
    match segments {
        [] => false,
        [single] => node
            .as_object_mut()
            .map(|object| object.remove(*single).is_some())
            .unwrap_or(false),
        [head, rest @ ..] => {
            let removed = {
                let Some(child) = node.get_mut(*head) else {
                    return false;
                };
                remove_path_inner(child, rest, drop_empty_container)
            };
            if removed && drop_empty_container {
                if let Some(object) = node.as_object_mut() {
                    let child_empty = object
                        .get(*head)
                        .and_then(serde_json::Value::as_object)
                        .is_some_and(|child| child.is_empty());
                    if child_empty {
                        object.remove(*head);
                    }
                }
            }
            removed
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::protocol::ProtocolSuite;

    fn endpoint() -> ProtocolEndpoint {
        ProtocolSuite::OpenAiResponses.default_endpoint()
    }

    fn profile(fields: Vec<FieldRule>) -> CapabilityProfile {
        CapabilityProfile {
            endpoint: endpoint(),
            fields,
            structure: None,
            allow_overrides: false,
        }
    }

    #[test]
    fn strips_top_level_field_and_reports_mutation() {
        let mut body = serde_json::json!({ "prompt_cache_key": "k", "model": "m" });
        let outcome = sanitize_with_profile(
            &mut body,
            &profile(vec![FieldRule {
                path: "prompt_cache_key".to_string(),
                action: FieldAction::Strip,
                convert: None,
                cond: MatchCond::Meaningful,
                reason: "provider ignores it".to_string(),
                extra: None,
                drop_container_when_empty: false,
            }]),
        )
        .unwrap();
        assert!(outcome.mutated);
        assert_eq!(outcome.decisions.len(), 1);
        assert_eq!(outcome.decisions[0].field, "prompt_cache_key");
        assert_eq!(outcome.decisions[0].action, FieldAction::Strip);
        assert!(body.get("prompt_cache_key").is_none());
        assert_eq!(body["model"], "m");
    }

    #[test]
    fn rejects_when_condition_matches() {
        let mut body = serde_json::json!({ "previous_response_id": "resp_1" });
        let err = sanitize_with_profile(
            &mut body,
            &profile(vec![FieldRule {
                path: "previous_response_id".to_string(),
                action: FieldAction::Reject,
                convert: None,
                cond: MatchCond::Meaningful,
                reason: "previous_response_id is not supported".to_string(),
                extra: None,
                drop_container_when_empty: false,
            }]),
        )
        .unwrap_err();
        assert_eq!(err.field, "previous_response_id");
        assert_eq!(err.action, FieldAction::Reject);
        assert_eq!(err.detail, "previous_response_id is not supported");
    }

    #[test]
    fn eq_bool_condition_only_fires_on_matching_boolean() {
        let store_false = FieldRule {
            path: "store".to_string(),
            action: FieldAction::Reject,
            convert: None,
            cond: MatchCond::EqBool(true),
            reason: "store=true is not supported".to_string(),
            extra: None,
            drop_container_when_empty: false,
        };
        let mut body = serde_json::json!({ "store": true });
        assert!(
            sanitize_with_profile(&mut body, &profile(vec![store_false.clone()])).is_err()
        );

        // store:false must NOT fire the rule.
        let mut body = serde_json::json!({ "store": false });
        let outcome = sanitize_with_profile(&mut body, &profile(vec![store_false])).unwrap();
        assert!(!outcome.mutated);
        assert!(body["store"].as_bool() == Some(false));
    }

    #[test]
    fn value_in_condition_rejects_non_allowed() {
        let rule = FieldRule {
            path: "truncation".to_string(),
            action: FieldAction::Reject,
            convert: None,
            cond: MatchCond::ValueIn(vec!["disabled".to_string()]),
            reason: "automatic truncation is not supported".to_string(),
            extra: None,
            drop_container_when_empty: false,
        };
        // auto → fires
        let mut body = serde_json::json!({ "truncation": "auto" });
        assert!(sanitize_with_profile(&mut body, &profile(vec![rule.clone()])).is_err());
        // disabled → does not fire
        let mut body = serde_json::json!({ "truncation": "disabled" });
        let outcome = sanitize_with_profile(&mut body, &profile(vec![rule])).unwrap();
        assert!(!outcome.mutated);
    }

    #[test]
    fn present_condition_fires_even_on_null() {
        let rule = FieldRule {
            path: "metadata".to_string(),
            action: FieldAction::Strip,
            convert: None,
            cond: MatchCond::Present,
            reason: "ignored".to_string(),
            extra: None,
            drop_container_when_empty: false,
        };
        let mut body = serde_json::json!({ "metadata": null });
        let outcome = sanitize_with_profile(&mut body, &profile(vec![rule])).unwrap();
        assert!(outcome.mutated);
        assert!(body.get("metadata").is_none());
    }

    #[test]
    fn nested_strip_drops_empty_container() {
        let mut body = serde_json::json!({ "text": { "verbosity": "high" } });
        let outcome = sanitize_with_profile(
            &mut body,
            &profile(vec![FieldRule {
                path: "text.verbosity".to_string(),
                action: FieldAction::Strip,
                convert: None,
                cond: MatchCond::Meaningful,
                reason: "ignored".to_string(),
                extra: None,
                drop_container_when_empty: true,
            }]),
        )
        .unwrap();
        assert!(outcome.mutated);
        assert!(body.get("text").is_none(), "empty container should be dropped");
    }

    #[test]
    fn nested_strip_keeps_container_when_not_empty() {
        let mut body = serde_json::json!({ "text": { "verbosity": "high", "format": "text" } });
        let outcome = sanitize_with_profile(
            &mut body,
            &profile(vec![FieldRule {
                path: "text.verbosity".to_string(),
                action: FieldAction::Strip,
                convert: None,
                cond: MatchCond::Meaningful,
                reason: "ignored".to_string(),
                extra: None,
                drop_container_when_empty: true,
            }]),
        )
        .unwrap();
        assert!(outcome.mutated);
        assert!(body["text"].get("verbosity").is_none());
        assert_eq!(body["text"]["format"], "text");
    }

    #[test]
    fn meaningful_condition_ignores_empty_string() {
        let rule = FieldRule {
            path: "seed".to_string(),
            action: FieldAction::Strip,
            convert: None,
            cond: MatchCond::Meaningful,
            reason: "ignored".to_string(),
            extra: None,
            drop_container_when_empty: false,
        };
        let mut body = serde_json::json!({ "seed": "" });
        let outcome = sanitize_with_profile(&mut body, &profile(vec![rule])).unwrap();
        assert!(!outcome.mutated, "empty seed is not meaningful");
        assert!(body["seed"].as_str() == Some(""));
    }

    #[test]
    fn convert_reasoning_materialises_content_and_drops_summary() {
        let mut item = serde_json::json!({
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": "call weather"}]
        });
        assert!(convert_reasoning_to_text(&mut item));
        assert_eq!(item["content"][0]["type"], "reasoning_text");
        assert_eq!(item["content"][0]["text"], "call weather");
        assert!(item.get("summary").is_none());
        assert!(item.get("encrypted_content").is_none());
    }

    #[test]
    fn is_encrypted_only_reasoning_detects_shell() {
        let shell = serde_json::json!({ "type": "reasoning", "encrypted_content": "blob", "summary": [] });
        assert!(is_encrypted_only_reasoning(&shell));
        let with_text = serde_json::json!({
            "type": "reasoning",
            "encrypted_content": "blob",
            "content": [{"type": "reasoning_text", "text": "x"}]
        });
        assert!(!is_encrypted_only_reasoning(&with_text));
    }

    #[test]
    fn empty_profile_is_noop() {
        let mut body = serde_json::json!({ "a": 1 });
        let outcome = sanitize_with_profile(&mut body, &profile(vec![])).unwrap();
        assert!(!outcome.mutated);
        assert!(outcome.decisions.is_empty());
        assert!(outcome.reject.is_none());
    }

    #[test]
    fn first_rule_wins_on_overlapping_path() {
        let mut body = serde_json::json!({ "seed": "x" });
        let outcome = sanitize_with_profile(
            &mut body,
            &profile(vec![
                FieldRule {
                    path: "seed".to_string(),
                    action: FieldAction::Strip,
                    convert: None,
                    cond: MatchCond::Meaningful,
                    reason: "a".to_string(),
                    extra: None,
                    drop_container_when_empty: false,
                },
                FieldRule {
                    path: "seed".to_string(),
                    action: FieldAction::Reject,
                    convert: None,
                    cond: MatchCond::Meaningful,
                    reason: "b".to_string(),
                    extra: None,
                    drop_container_when_empty: false,
                },
            ]),
        )
        .unwrap();
        assert!(outcome.mutated, "first strip rule wins over the reject");
        assert!(body.get("seed").is_none());
    }
}
