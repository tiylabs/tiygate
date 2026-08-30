//! Generic target-capability primitives.
//!
//! This module deliberately contains no protocol-specific wire handling and
//! performs no I/O.  Concrete protocol/dialect adapters live in
//! `tiygate-protocols`; the server supplies observations and consumes the
//! compatibility report on the request path.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{de::Error as DeError, Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use url::Url;

type HmacSha256 = Hmac<Sha256>;

/// Stable, extensible capability identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilityId(pub String);

impl CapabilityId {
    /// Construct an identifier after trimming surrounding whitespace.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into().trim().to_string())
    }

    /// Borrow the canonical identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for CapabilityId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for CapabilityId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Display for CapabilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Protocol baseline ceiling for one capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaselineSupport {
    /// The protocol/dialect can express the capability.
    Supported,
    /// The protocol/dialect cannot express the capability.
    Forbidden,
    /// The carrier is extensible, but support is not guaranteed by the
    /// standard baseline.
    #[serde(other)]
    ExtensionUnknown,
}

/// Runtime state observed for a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Supported,
    Unsupported,
    Constrained,
    #[serde(other)]
    Unknown,
}

/// Typed values carried by constrained capability observations.
#[derive(Debug, Clone, PartialEq)]
pub enum CapabilityValue {
    Bool(bool),
    EnumSet(BTreeSet<String>),
    StringSet(BTreeSet<String>),
    IntegerRange {
        min: Option<i64>,
        max: Option<i64>,
    },
    DecimalRange {
        min: Option<f64>,
        max: Option<f64>,
    },
    SchemaKeywordSet(BTreeSet<String>),
    Opaque(serde_json::Value),
    /// A value kind introduced by a newer registry version. Keeping the raw
    /// kind/value pair allows old binaries to round-trip it without making it
    /// routable; it is treated exactly like `Opaque` by matchers.
    Unknown {
        kind: String,
        value: serde_json::Value,
    },
}

impl Serialize for CapabilityValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let value = match self {
            Self::Bool(value) => serde_json::json!({"kind": "bool", "value": value}),
            Self::EnumSet(value) => serde_json::json!({"kind": "enum_set", "value": value}),
            Self::StringSet(value) => {
                serde_json::json!({"kind": "string_set", "value": value})
            }
            Self::IntegerRange { min, max } => {
                serde_json::json!({"kind": "integer_range", "value": {"min": min, "max": max}})
            }
            Self::DecimalRange { min, max } => {
                serde_json::json!({"kind": "decimal_range", "value": {"min": min, "max": max}})
            }
            Self::SchemaKeywordSet(value) => {
                serde_json::json!({"kind": "schema_keyword_set", "value": value})
            }
            Self::Opaque(value) => serde_json::json!({"kind": "opaque", "value": value}),
            Self::Unknown { kind, value } => serde_json::json!({"kind": kind, "value": value}),
        };
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CapabilityValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = serde_json::Value::deserialize(deserializer)?;
        let object = raw
            .as_object()
            .ok_or_else(|| D::Error::custom("capability value must be an object"))?;
        let kind = object
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| D::Error::custom("capability value requires a kind"))?;
        let value = object
            .get("value")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        match kind {
            "bool" => serde_json::from_value(value)
                .map(Self::Bool)
                .map_err(D::Error::custom),
            "enum_set" => serde_json::from_value(value)
                .map(Self::EnumSet)
                .map_err(D::Error::custom),
            "string_set" => serde_json::from_value(value)
                .map(Self::StringSet)
                .map_err(D::Error::custom),
            "integer_range" => {
                #[derive(Deserialize)]
                struct Range {
                    min: Option<i64>,
                    max: Option<i64>,
                }
                serde_json::from_value::<Range>(value)
                    .map(|range| Self::IntegerRange {
                        min: range.min,
                        max: range.max,
                    })
                    .map_err(D::Error::custom)
            }
            "decimal_range" => {
                #[derive(Deserialize)]
                struct Range {
                    min: Option<f64>,
                    max: Option<f64>,
                }
                serde_json::from_value::<Range>(value)
                    .map(|range| Self::DecimalRange {
                        min: range.min,
                        max: range.max,
                    })
                    .map_err(D::Error::custom)
            }
            "schema_keyword_set" => serde_json::from_value(value)
                .map(Self::SchemaKeywordSet)
                .map_err(D::Error::custom),
            "opaque" => Ok(Self::Opaque(value)),
            _ => Ok(Self::Unknown {
                kind: kind.to_string(),
                value,
            }),
        }
    }
}

impl CapabilityValue {
    pub fn kind(&self) -> CapabilityValueKind {
        match self {
            Self::Bool(_) => CapabilityValueKind::Bool,
            Self::EnumSet(_) => CapabilityValueKind::EnumSet,
            Self::StringSet(_) => CapabilityValueKind::StringSet,
            Self::IntegerRange { .. } => CapabilityValueKind::IntegerRange,
            Self::DecimalRange { .. } => CapabilityValueKind::DecimalRange,
            Self::SchemaKeywordSet(_) => CapabilityValueKind::SchemaKeywordSet,
            Self::Opaque(_) | Self::Unknown { .. } => CapabilityValueKind::Opaque,
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Self::Bool(_) => false,
            Self::EnumSet(values) | Self::StringSet(values) | Self::SchemaKeywordSet(values) => {
                values.is_empty()
            }
            Self::IntegerRange { min, max } => min.is_none() && max.is_none(),
            Self::DecimalRange { min, max } => min.is_none() && max.is_none(),
            Self::Opaque(value) | Self::Unknown { value, .. } => value.is_null(),
        }
    }
}

/// Matching semantics for a capability descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityMatcher {
    Boolean,
    SetContains,
    RangeContains,
    ExactMatch,
    #[serde(other)]
    Opaque,
}

impl CapabilityMatcher {
    /// Match typed values using the descriptor-declared semantics. Opaque
    /// values are intentionally non-routable and therefore never satisfy an
    /// automatic requirement.
    pub fn matches(
        self,
        actual: Option<&CapabilityValue>,
        required: Option<&CapabilityValue>,
    ) -> bool {
        let (Some(actual), Some(required)) = (actual, required) else {
            return required.is_none() && actual.is_some() && self == Self::Boolean;
        };
        if matches!(
            actual,
            CapabilityValue::Opaque(_) | CapabilityValue::Unknown { .. }
        ) || matches!(
            required,
            CapabilityValue::Opaque(_) | CapabilityValue::Unknown { .. }
        ) {
            return false;
        }
        match self {
            Self::Boolean => {
                matches!((actual, required), (CapabilityValue::Bool(a), CapabilityValue::Bool(r)) if a == r)
            }
            Self::SetContains => exact_or_superset(actual, required),
            Self::RangeContains => exact_or_superset(actual, required),
            Self::ExactMatch => actual == required,
            Self::Opaque => false,
        }
    }
}

/// Value shape declared by a capability descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityValueKind {
    Bool,
    EnumSet,
    StringSet,
    IntegerRange,
    DecimalRange,
    SchemaKeywordSet,
    #[serde(other)]
    Opaque,
}

/// Scope at which a capability observation is valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityScope {
    Endpoint,
    Model,
    Dialect,
    Target,
    Request,
}

/// Permitted evidence sources for one capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryMethod {
    ExplicitOverride,
    ExactModelCatalog,
    ProviderDocumentation,
    PassiveTraffic,
    ActiveProbe,
}

/// Whether a capability is ready to participate in runtime routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingEligibility {
    ShadowEligible,
    EnforceEligible,
    #[serde(other)]
    Disabled,
}

/// Runtime rollout mode for capability-aware routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityRoutingMode {
    /// Compute and record plans without changing the selected target.
    Shadow,
    /// Filter incompatible targets and execute their individual plans.
    Enforce,
    /// Preserve legacy target ordering and request encoding. Unknown future
    /// mode values deserialize to this safe default.
    #[default]
    #[serde(other)]
    Off,
}

impl CapabilityRoutingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Enforce => "enforce",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "shadow" => Some(Self::Shadow),
            "enforce" => Some(Self::Enforce),
            _ => None,
        }
    }
}

/// Implementation maturity of a registry entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImplementationStatus {
    Implemented,
    #[serde(other)]
    Cataloged,
}

/// Static descriptor shared by protocol registries and the generic resolver.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityDescriptor {
    pub id: CapabilityId,
    pub value_kind: CapabilityValueKind,
    pub matcher: CapabilityMatcher,
    pub scope: CapabilityScope,
    pub implementation_status: ImplementationStatus,
    pub discovery_methods: BTreeSet<DiscoveryMethod>,
    pub routing_eligibility: RoutingEligibility,
    #[serde(default)]
    pub dependencies: Vec<CapabilityId>,
    pub conversion_relevant: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_id: Option<String>,
    pub owner: String,
}

/// Validate static descriptor invariants before publishing a registry.
pub fn validate_capability_descriptors(descriptors: &[CapabilityDescriptor]) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    for descriptor in descriptors {
        if descriptor.id.as_str().is_empty() {
            return Err("capability id must not be empty".to_string());
        }
        if !ids.insert(descriptor.id.clone()) {
            return Err(format!("duplicate capability id: {}", descriptor.id));
        }
        if descriptor.implementation_status == ImplementationStatus::Cataloged
            && descriptor.routing_eligibility != RoutingEligibility::Disabled
        {
            return Err(format!(
                "cataloged capability {} must be disabled",
                descriptor.id
            ));
        }
        if descriptor.routing_eligibility == RoutingEligibility::EnforceEligible
            && descriptor.implementation_status != ImplementationStatus::Implemented
        {
            return Err(format!(
                "enforce-eligible capability {} must be implemented",
                descriptor.id
            ));
        }
        if descriptor.routing_eligibility == RoutingEligibility::EnforceEligible
            && descriptor.discovery_methods.is_empty()
        {
            return Err(format!(
                "enforce-eligible capability {} needs a discovery method",
                descriptor.id
            ));
        }
        let active_probe = descriptor
            .discovery_methods
            .contains(&DiscoveryMethod::ActiveProbe);
        if active_probe != descriptor.probe_id.is_some() {
            return Err(format!(
                "capability {} has inconsistent active-probe metadata",
                descriptor.id
            ));
        }
        if descriptor.matcher == CapabilityMatcher::Boolean
            && descriptor.value_kind != CapabilityValueKind::Bool
        {
            return Err(format!(
                "boolean matcher has non-bool value kind: {}",
                descriptor.id
            ));
        }
    }
    for descriptor in descriptors {
        for dependency in &descriptor.dependencies {
            if !ids.contains(dependency) {
                return Err(format!(
                    "capability {} references unknown dependency {}",
                    descriptor.id, dependency
                ));
            }
        }
    }
    let graph: HashMap<&CapabilityId, &[CapabilityId]> = descriptors
        .iter()
        .map(|descriptor| (&descriptor.id, descriptor.dependencies.as_slice()))
        .collect();
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for id in graph.keys() {
        validate_dependency_path(id, &graph, &mut visiting, &mut visited)?;
    }
    Ok(())
}

/// Validate the state/value shape of one runtime observation against its
/// descriptor. Unknown IDs remain round-trippable but must be rejected by a
/// caller before they are used for automatic routing.
pub fn validate_capability_observation(
    descriptor: &CapabilityDescriptor,
    observation: &CapabilityObservation,
) -> Result<(), String> {
    if observation.capability_id != descriptor.id {
        return Err(format!(
            "observation capability {} does not match descriptor {}",
            observation.capability_id, descriptor.id
        ));
    }
    match observation.state {
        CapabilityState::Unknown => {
            if observation.value.is_some() {
                return Err(format!(
                    "unknown capability {} cannot carry a value",
                    descriptor.id
                ));
            }
        }
        CapabilityState::Supported | CapabilityState::Unsupported => {
            if descriptor.value_kind == CapabilityValueKind::Bool && observation.value.is_some() {
                return Err(format!(
                    "boolean capability {} cannot carry a value for {:?}",
                    descriptor.id, observation.state
                ));
            }
            if let Some(value) = &observation.value {
                if matches!(value, CapabilityValue::Unknown { .. }) {
                    // Preserve a value kind introduced by a newer registry
                    // version for diagnostics/import-export. It is mapped to
                    // the non-routable opaque kind and can never satisfy a
                    // requirement automatically.
                    return Ok(());
                }
                if value.kind() != descriptor.value_kind {
                    return Err(format!(
                        "capability {} value kind {:?} does not match {:?}",
                        descriptor.id,
                        value.kind(),
                        descriptor.value_kind
                    ));
                }
            }
        }
        CapabilityState::Constrained => {
            let Some(value) = observation.value.as_ref() else {
                return Err(format!(
                    "constrained capability {} requires a value",
                    descriptor.id
                ));
            };
            if value.is_empty() {
                return Err(format!(
                    "constrained capability {} requires a non-empty value",
                    descriptor.id
                ));
            }
            if matches!(value, CapabilityValue::Unknown { .. }) {
                return Ok(());
            }
            if value.kind() != descriptor.value_kind {
                return Err(format!(
                    "capability {} value kind {:?} does not match {:?}",
                    descriptor.id,
                    value.kind(),
                    descriptor.value_kind
                ));
            }
        }
    }
    Ok(())
}

fn validate_dependency_path<'a>(
    id: &'a CapabilityId,
    graph: &HashMap<&'a CapabilityId, &'a [CapabilityId]>,
    visiting: &mut HashSet<&'a CapabilityId>,
    visited: &mut HashSet<&'a CapabilityId>,
) -> Result<(), String> {
    if visited.contains(id) {
        return Ok(());
    }
    if !visiting.insert(id) {
        return Err(format!("cyclic capability dependency at {id}"));
    }
    if let Some(dependencies) = graph.get(id) {
        for dependency in *dependencies {
            let dependency_id = graph
                .keys()
                .find(|candidate| ***candidate == *dependency)
                .copied()
                .ok_or_else(|| format!("unknown capability dependency: {dependency}"))?;
            validate_dependency_path(dependency_id, graph, visiting, visited)?;
        }
    }
    visiting.remove(id);
    visited.insert(id);
    Ok(())
}

/// Requiredness of a request capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementStrength {
    Required,
    Preferred,
    #[serde(other)]
    Ignorable,
}

/// A single capability requirement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityRequirement {
    pub id: CapabilityId,
    pub strength: RequirementStrength,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<CapabilityValue>,
}

impl CapabilityRequirement {
    /// Build a boolean required capability.
    pub fn required(id: impl Into<CapabilityId>) -> Self {
        Self {
            id: id.into(),
            strength: RequirementStrength::Required,
            value: None,
        }
    }

    /// Build a requirement with a typed constraint.
    pub fn with_value(
        id: impl Into<CapabilityId>,
        strength: RequirementStrength,
        value: CapabilityValue,
    ) -> Self {
        Self {
            id: id.into(),
            strength,
            value: Some(value),
        }
    }
}

/// Boolean composition of capability requirements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", content = "items", rename_all = "snake_case")]
pub enum RequirementExpr {
    AllOf(Vec<RequirementExpr>),
    AnyOf(Vec<RequirementExpr>),
    Not(Box<RequirementExpr>),
    Capability(CapabilityRequirement),
}

impl RequirementExpr {
    /// A conjunction of requirements.
    pub fn all(items: impl IntoIterator<Item = RequirementExpr>) -> Self {
        Self::AllOf(items.into_iter().collect())
    }

    /// A single required capability expression.
    pub fn required(id: impl Into<CapabilityId>) -> Self {
        Self::Capability(CapabilityRequirement::required(id))
    }

    /// Whether the expression contains a required leaf with the given ID.
    /// This is primarily useful for protocol adapters and deterministic test
    /// assertions; matching still evaluates the full boolean expression.
    pub fn contains_required(&self, id: &CapabilityId) -> bool {
        match self {
            Self::AllOf(items) | Self::AnyOf(items) => {
                items.iter().any(|item| item.contains_required(id))
            }
            Self::Not(item) => item.contains_required(id),
            Self::Capability(requirement) => {
                requirement.strength == RequirementStrength::Required && requirement.id == *id
            }
        }
    }
}

/// Request/response/continuation requirements for one exchange.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExchangeRequirements {
    pub request: RequirementExpr,
    pub response_contract: RequirementExpr,
    pub continuation: RequirementExpr,
}

/// Versioned namespace for the canonical required-capability shape hash.
/// Changing matcher/constraint canonicalization requires a new version and
/// invalidates existing Route × shape admissions.
pub const CAPABILITY_SHAPE_HASH_VERSION: &str = "shape/v1";

/// Compute a stable, non-content-bearing identifier for a request capability
/// shape. Only required capability IDs and typed constraints are included;
/// prompts, tool descriptions and credentials never enter the hash.
pub fn capability_shape_hash(requirements: &ExchangeRequirements) -> String {
    let mut leaves = Vec::new();
    for expression in [
        &requirements.request,
        &requirements.response_contract,
        &requirements.continuation,
    ] {
        collect_required_shapes(expression, &mut leaves);
    }
    if !is_flat_required_shape(requirements) {
        // Keep the v1 hash namespace while ensuring that an AnyOf/Not or a
        // Preferred leaf cannot collide with an admissible AllOf shape that
        // happens to contain the same capability IDs.
        leaves.push((CapabilityId::from("__non_admissible_shape__"), None));
    }
    leaves.sort_by(|left, right| {
        serde_json::to_string(left)
            .unwrap_or_default()
            .cmp(&serde_json::to_string(right).unwrap_or_default())
    });
    leaves.dedup();
    let payload = serde_json::to_vec(&leaves).unwrap_or_default();
    let digest = Sha256::digest(payload);
    format!("{CAPABILITY_SHAPE_HASH_VERSION}:{}", hex::encode(digest))
}

/// Whether a request shape is representable by the first admission API. The
/// initial gate stores a normalized set of Required leaves, so boolean
/// alternatives, negations and Preferred-only requirements remain Shadow-only
/// until a future admission schema can carry their full expression.
pub fn is_flat_required_shape(requirements: &ExchangeRequirements) -> bool {
    [
        &requirements.request,
        &requirements.response_contract,
        &requirements.continuation,
    ]
    .into_iter()
    .all(is_flat_required_expression)
}

fn is_flat_required_expression(expression: &RequirementExpr) -> bool {
    match expression {
        RequirementExpr::AllOf(items) => items.iter().all(is_flat_required_expression),
        RequirementExpr::Capability(requirement) => {
            requirement.strength == RequirementStrength::Required
        }
        RequirementExpr::AnyOf(_) | RequirementExpr::Not(_) => false,
    }
}

/// Compute the same shape identifier for an already-normalized list of
/// required IDs. This is used by the Admin admission API when a shadow report
/// carries the shape but no request body or prompt is available.
pub fn capability_shape_hash_from_ids(ids: &[CapabilityId]) -> String {
    capability_shape_hash_from_requirements(
        &ids.iter()
            .cloned()
            .map(CapabilityRequirement::required)
            .collect::<Vec<_>>(),
    )
}

/// Compute the same stable shape identifier for a normalized list of typed
/// required leaves.  Unlike [`capability_shape_hash_from_ids`], this helper
/// retains request constraints (for example a namespace path) so an
/// admission for one constrained shape cannot be reused for another.
pub fn capability_shape_hash_from_requirements(requirements: &[CapabilityRequirement]) -> String {
    let expression = RequirementExpr::all(
        requirements
            .iter()
            .cloned()
            .map(RequirementExpr::Capability)
            .collect::<Vec<_>>(),
    );
    capability_shape_hash(&ExchangeRequirements {
        request: expression,
        response_contract: RequirementExpr::all([]),
        continuation: RequirementExpr::all([]),
    })
}

fn collect_required_shapes(
    expression: &RequirementExpr,
    output: &mut Vec<(CapabilityId, Option<CapabilityValue>)>,
) {
    match expression {
        RequirementExpr::AllOf(items) | RequirementExpr::AnyOf(items) => {
            for item in items {
                collect_required_shapes(item, output);
            }
        }
        RequirementExpr::Not(item) => collect_required_shapes(item, output),
        RequirementExpr::Capability(requirement)
            if requirement.strength == RequirementStrength::Required =>
        {
            output.push((requirement.id.clone(), requirement.value.clone()));
        }
        RequirementExpr::Capability(_) => {}
    }
}

impl Default for RequirementExpr {
    fn default() -> Self {
        Self::AllOf(Vec::new())
    }
}

/// Derive protocol-independent requirements from canonical IR fields.
/// Protocol crates add wire-carrier requirements (for example CRL opaque
/// items) before passing the exchange to the planner.
pub fn derive_ir_requirements(request: &crate::ir::IrRequest) -> ExchangeRequirements {
    // Every supported egress in the first-phase gateway is an HTTP exchange;
    // make this prerequisite explicit so an enforce gate cannot select a
    // target whose endpoint/auth transport was never verified.
    let mut request_requirements = vec![RequirementExpr::required("transport.http")];
    if request.stream {
        request_requirements.push(RequirementExpr::required("transport.sse"));
    }
    if request.tools.iter().any(crate::ir::Tool::is_function) {
        request_requirements.push(RequirementExpr::required("tools.function"));
    }
    if request.tools.iter().any(crate::ir::Tool::is_custom) {
        request_requirements.push(RequirementExpr::required("tools.custom"));
    }
    for tool in request.tools.iter().filter(|tool| tool.is_hosted()) {
        let capability_id = match tool.tool_type.as_deref() {
            Some("namespace") => "tools.namespace",
            Some("web_search" | "web_search_preview") => "tools.hosted.web_search",
            Some("file_search") => "tools.hosted.file_search",
            Some("code_interpreter" | "code_interpreter_preview") => {
                "tools.hosted.code_interpreter"
            }
            Some("computer_use" | "computer_use_preview") => "tools.hosted.computer_use",
            Some("mcp" | "remote_mcp") => "tools.remote_mcp",
            Some("program" | "programmatic") => "tools.programmatic",
            Some(other) => {
                // Keep an unregistered provider extension visible to the
                // planner as an unknown required capability. Enforce mode
                // must not route it to a target merely because it has ordinary
                // function support.
                request_requirements
                    .push(RequirementExpr::required(format!("tools.hosted.{other}")));
                continue;
            }
            None => continue,
        };
        request_requirements.push(RequirementExpr::required(capability_id));
    }
    if request.tools.iter().any(|tool| {
        tool.config
            .as_ref()
            .and_then(|config| config.get("caller"))
            .is_some()
    }) {
        request_requirements.push(RequirementExpr::required("tools.programmatic"));
    }
    if request.extensions.values().any(|value| {
        value
            .get("parallel_tool_calls")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    }) {
        request_requirements.push(RequirementExpr::required("tools.parallel"));
    }
    // Protocol codecs normalize native `tool_choice` into this extension so
    // the generic planner can enforce required/specific semantics without
    // importing a protocol-specific JSON shape into core.
    if let Some(tool_choice) = request.extensions.get("tool_choice") {
        if tool_choice.as_str() == Some("required") {
            request_requirements.push(RequirementExpr::required("tools.choice.required"));
        } else if tool_choice.is_object()
            && tool_choice.get("type").and_then(serde_json::Value::as_str) == Some("function")
        {
            request_requirements.push(RequirementExpr::required("tools.choice.specific"));
        }
    }
    if request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .any(|content| matches!(content, crate::ir::Content::Reasoning { text, .. } if !text.is_empty()))
    {
        request_requirements.push(RequirementExpr::required("reasoning.plaintext"));
    }
    for content in request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
    {
        if matches!(
            content,
            crate::ir::Content::Text {
                prompt_cache_breakpoint: Some(_),
                ..
            } | crate::ir::Content::Media {
                prompt_cache_breakpoint: Some(_),
                ..
            }
        ) {
            request_requirements.push(RequirementExpr::required("cache.prompt.breakpoint"));
        }
        match content {
            crate::ir::Content::Reasoning {
                encrypted_content: Some(_),
                ..
            } => request_requirements.push(RequirementExpr::required("reasoning.encrypted_replay")),
            crate::ir::Content::Media {
                source: crate::ir::MediaSource::Inline { .. },
                mime_type,
                ..
            } => {
                let capability = if mime_type.starts_with("image/") {
                    "media.input.image.inline"
                } else if mime_type.starts_with("audio/") {
                    "media.input.audio.inline"
                } else if mime_type.starts_with("video/") {
                    "media.input.video.inline"
                } else {
                    "media.input.file_id"
                };
                request_requirements.push(RequirementExpr::required(capability));
            }
            crate::ir::Content::Media {
                source: crate::ir::MediaSource::Url { .. },
                mime_type,
                ..
            } => {
                let capability = if mime_type.starts_with("image/") {
                    "media.input.image.url"
                } else {
                    "media.input.file_id"
                };
                request_requirements.push(RequirementExpr::required(capability));
            }
            crate::ir::Content::Media {
                source: crate::ir::MediaSource::FileId { .. },
                ..
            } => request_requirements.push(RequirementExpr::required("media.input.file_id")),
            _ => {}
        }
    }
    if let Some(thinking) = request.params.thinking.as_ref().filter(|thinking| {
        thinking.effort.is_some()
            || thinking.budget_tokens.is_some()
            || thinking.summary.is_some()
            || thinking.mode.is_some()
            || thinking.context.is_some()
    }) {
        if thinking.effort.is_some() {
            request_requirements.push(RequirementExpr::required("reasoning.effort.values"));
        }
        if thinking.budget_tokens.is_some() {
            request_requirements.push(RequirementExpr::required("reasoning.budget_tokens"));
        }
        if thinking.mode.is_some() {
            request_requirements.push(RequirementExpr::required("reasoning.mode"));
        }
        if thinking.context.is_some() {
            request_requirements.push(RequirementExpr::required("reasoning.context"));
        }
        if thinking.summary.is_some() {
            request_requirements.push(RequirementExpr::required("reasoning.summary"));
        }
    }
    let response_requirement = match request.response_format {
        Some(crate::ir::ResponseFormat::JsonSchema {
            strict: Some(true), ..
        }) => RequirementExpr::required("structured.json_schema.strict"),
        Some(crate::ir::ResponseFormat::JsonSchema { .. }) => {
            RequirementExpr::required("structured.json_schema")
        }
        Some(crate::ir::ResponseFormat::JsonObject) => {
            RequirementExpr::required("structured.json_object")
        }
        Some(crate::ir::ResponseFormat::Text) | None => RequirementExpr::all([]),
    };
    let continuation = if request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .any(|content| matches!(content, crate::ir::Content::ToolResult { .. }))
    {
        RequirementExpr::required("tools.function.continuation")
    } else {
        RequirementExpr::all([])
    };
    ExchangeRequirements {
        request: RequirementExpr::all(request_requirements),
        response_contract: response_requirement,
        continuation,
    }
}

/// Evidence origin for a capability observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    ExplicitOverride,
    SemanticProbe,
    SuccessfulTraffic,
    ExactModelCatalog,
    ProviderDocumentation,
    ProtocolDefault,
    #[serde(other)]
    Unknown,
}

/// An observed target capability.  Unknown is an evaluation result and is
/// normally not persisted as a stronger observation than existing evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityObservation {
    pub capability_id: CapabilityId,
    pub state: CapabilityState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<CapabilityValue>,
    pub source: EvidenceSource,
    pub observed_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    pub evidence_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_suite_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted_detail: Option<String>,
}

impl CapabilityObservation {
    /// Whether the observation can currently participate in resolution.
    pub fn is_fresh_at(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_none_or(|expires| expires > now)
    }

    /// Construct a short-lived observation with the current timestamp.
    pub fn now(
        capability_id: impl Into<CapabilityId>,
        state: CapabilityState,
        source: EvidenceSource,
        evidence_version: u32,
    ) -> Self {
        Self {
            capability_id: capability_id.into(),
            state,
            value: None,
            source,
            observed_at: Utc::now(),
            expires_at: None,
            evidence_version,
            probe_suite_version: None,
            reason_code: None,
            redacted_detail: None,
        }
    }
}

/// A resolved capability after baseline and evidence evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedCapability {
    pub state: CapabilityState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<CapabilityValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation: Option<CapabilityObservation>,
    /// Descriptor-declared matcher used for constrained values. Older
    /// persisted profiles omit this field; callers can rehydrate it from the
    /// compiled registry before planning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matcher: Option<CapabilityMatcher>,
}

/// The target capability map consumed by the planner.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ResolvedTargetCapabilities {
    pub capabilities: BTreeMap<CapabilityId, ResolvedCapability>,
}

impl ResolvedTargetCapabilities {
    /// Return a resolved capability, defaulting to Unknown.
    pub fn get(&self, id: &CapabilityId) -> ResolvedCapability {
        self.capabilities
            .get(id)
            .cloned()
            .unwrap_or(ResolvedCapability {
                state: CapabilityState::Unknown,
                value: None,
                observation: None,
                matcher: None,
            })
    }

    /// Whether a requirement expression is satisfied.
    pub fn satisfies(&self, expression: &RequirementExpr) -> bool {
        evaluate_satisfaction(self, expression) == Satisfaction::Satisfied
    }
}

/// Three-valued evaluation used by the resolver. Treating `Unknown` as
/// unsatisfied for `Not(Unknown)` would turn missing evidence into a positive
/// routing decision, so the public boolean API is deliberately backed by this
/// fail-closed state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Satisfaction {
    Satisfied,
    Unsatisfied,
    Unknown,
}

fn evaluate_satisfaction(
    capabilities: &ResolvedTargetCapabilities,
    expression: &RequirementExpr,
) -> Satisfaction {
    match expression {
        RequirementExpr::AllOf(items) => {
            let mut saw_unknown = false;
            for item in items {
                match evaluate_satisfaction(capabilities, item) {
                    Satisfaction::Unsatisfied => return Satisfaction::Unsatisfied,
                    Satisfaction::Unknown => saw_unknown = true,
                    Satisfaction::Satisfied => {}
                }
            }
            if saw_unknown {
                Satisfaction::Unknown
            } else {
                Satisfaction::Satisfied
            }
        }
        RequirementExpr::AnyOf(items) => {
            let mut saw_unknown = false;
            for item in items {
                match evaluate_satisfaction(capabilities, item) {
                    Satisfaction::Satisfied => return Satisfaction::Satisfied,
                    Satisfaction::Unknown => saw_unknown = true,
                    Satisfaction::Unsatisfied => {}
                }
            }
            if saw_unknown {
                Satisfaction::Unknown
            } else {
                Satisfaction::Unsatisfied
            }
        }
        RequirementExpr::Not(item) => match evaluate_satisfaction(capabilities, item) {
            Satisfaction::Satisfied => Satisfaction::Unsatisfied,
            Satisfaction::Unsatisfied => Satisfaction::Satisfied,
            Satisfaction::Unknown => Satisfaction::Unknown,
        },
        RequirementExpr::Capability(requirement) => {
            let resolved = capabilities.get(&requirement.id);
            if resolved.state == CapabilityState::Unknown {
                Satisfaction::Unknown
            } else if requirement_satisfied(&resolved, requirement) {
                Satisfaction::Satisfied
            } else {
                Satisfaction::Unsatisfied
            }
        }
    }
}

fn requirement_satisfied(
    resolved: &ResolvedCapability,
    requirement: &CapabilityRequirement,
) -> bool {
    if resolved.value.as_ref().is_some_and(|value| {
        matches!(
            value,
            CapabilityValue::Opaque(_) | CapabilityValue::Unknown { .. }
        )
    }) {
        // A future/opaque value may be retained for round-trip diagnostics,
        // but it cannot prove even an unconstrained automatic requirement.
        return false;
    }
    match resolved.state {
        CapabilityState::Supported => match (&requirement.value, &resolved.value) {
            (None, _) => true,
            (Some(required), Some(actual)) => resolved
                .matcher
                .unwrap_or_else(|| infer_matcher(actual, required))
                .matches(Some(actual), Some(required)),
            (Some(_), None) => false,
        },
        CapabilityState::Constrained => match (&requirement.value, &resolved.value) {
            (None, Some(actual)) if !actual.is_empty() => true,
            (Some(required), Some(actual)) if !actual.is_empty() => resolved
                .matcher
                .unwrap_or_else(|| infer_matcher(actual, required))
                .matches(Some(actual), Some(required)),
            _ => false,
        },
        CapabilityState::Unsupported | CapabilityState::Unknown => false,
    }
}

fn infer_matcher(actual: &CapabilityValue, required: &CapabilityValue) -> CapabilityMatcher {
    match (actual, required) {
        (CapabilityValue::EnumSet(..), CapabilityValue::EnumSet(..))
        | (CapabilityValue::StringSet(..), CapabilityValue::StringSet(..))
        | (CapabilityValue::SchemaKeywordSet(..), CapabilityValue::SchemaKeywordSet(..)) => {
            CapabilityMatcher::SetContains
        }
        (CapabilityValue::IntegerRange { .. }, CapabilityValue::IntegerRange { .. })
        | (CapabilityValue::DecimalRange { .. }, CapabilityValue::DecimalRange { .. }) => {
            CapabilityMatcher::RangeContains
        }
        (CapabilityValue::Bool(..), CapabilityValue::Bool(..)) => CapabilityMatcher::Boolean,
        _ => CapabilityMatcher::ExactMatch,
    }
}

fn exact_or_superset(actual: &CapabilityValue, required: &CapabilityValue) -> bool {
    if matches!(
        actual,
        CapabilityValue::Opaque(_) | CapabilityValue::Unknown { .. }
    ) || matches!(
        required,
        CapabilityValue::Opaque(_) | CapabilityValue::Unknown { .. }
    ) {
        return false;
    }
    match (actual, required) {
        (CapabilityValue::Bool(a), CapabilityValue::Bool(r)) => a == r,
        (CapabilityValue::EnumSet(a), CapabilityValue::EnumSet(r))
        | (CapabilityValue::StringSet(a), CapabilityValue::StringSet(r))
        | (CapabilityValue::SchemaKeywordSet(a), CapabilityValue::SchemaKeywordSet(r)) => {
            r.is_subset(a)
        }
        (
            CapabilityValue::IntegerRange {
                min: amin,
                max: amax,
            },
            CapabilityValue::IntegerRange {
                min: rmin,
                max: rmax,
            },
        ) => range_contains_i64(*amin, *amax, *rmin, *rmax),
        (
            CapabilityValue::DecimalRange {
                min: amin,
                max: amax,
            },
            CapabilityValue::DecimalRange {
                min: rmin,
                max: rmax,
            },
        ) => range_contains_f64(*amin, *amax, *rmin, *rmax),
        _ => false,
    }
}

fn range_contains_i64(
    amin: Option<i64>,
    amax: Option<i64>,
    rmin: Option<i64>,
    rmax: Option<i64>,
) -> bool {
    rmin.is_none_or(|required| amin.is_some_and(|actual| actual <= required))
        && rmax.is_none_or(|required| amax.is_some_and(|actual| actual >= required))
}

fn range_contains_f64(
    amin: Option<f64>,
    amax: Option<f64>,
    rmin: Option<f64>,
    rmax: Option<f64>,
) -> bool {
    rmin.is_none_or(|required| amin.is_some_and(|actual| actual <= required))
        && rmax.is_none_or(|required| amax.is_some_and(|actual| actual >= required))
}

/// Source precedence used by the pure resolver.
fn source_rank(source: EvidenceSource) -> u8 {
    match source {
        EvidenceSource::ExplicitOverride => 5,
        EvidenceSource::SemanticProbe | EvidenceSource::SuccessfulTraffic => 4,
        EvidenceSource::ExactModelCatalog => 3,
        EvidenceSource::ProviderDocumentation => 2,
        EvidenceSource::ProtocolDefault => 1,
        EvidenceSource::Unknown => 0,
    }
}

/// Resolve observations under a protocol baseline.
pub fn resolve_capabilities(
    baseline: &BTreeMap<CapabilityId, BaselineSupport>,
    observations: impl IntoIterator<Item = CapabilityObservation>,
    now: DateTime<Utc>,
) -> ResolvedTargetCapabilities {
    resolve_capabilities_with_matchers(baseline, &BTreeMap::new(), observations, now)
}

/// Resolve observations while retaining the matcher declared by the static
/// capability registry. The matcher map is intentionally supplied by the
/// protocol adapter so `core` stays independent of any registry source.
pub fn resolve_capabilities_with_matchers(
    baseline: &BTreeMap<CapabilityId, BaselineSupport>,
    matchers: &BTreeMap<CapabilityId, CapabilityMatcher>,
    observations: impl IntoIterator<Item = CapabilityObservation>,
    now: DateTime<Utc>,
) -> ResolvedTargetCapabilities {
    let mut grouped: HashMap<CapabilityId, Vec<CapabilityObservation>> = HashMap::new();
    for observation in observations {
        if observation.state == CapabilityState::Unknown
            || observation.source == EvidenceSource::Unknown
        {
            continue;
        }
        grouped
            .entry(observation.capability_id.clone())
            .or_default()
            .push(observation);
    }

    let mut ids: BTreeSet<CapabilityId> = baseline.keys().cloned().collect();
    ids.extend(grouped.keys().cloned());
    let mut resolved = BTreeMap::new();

    for id in ids {
        if baseline.get(&id) == Some(&BaselineSupport::Forbidden) {
            resolved.insert(
                id.clone(),
                ResolvedCapability {
                    state: CapabilityState::Unsupported,
                    value: None,
                    observation: None,
                    matcher: matchers.get(&id).copied(),
                },
            );
            continue;
        }

        let selected = grouped.get(&id).and_then(|items| {
            items
                .iter()
                .filter(|item| item.is_fresh_at(now))
                .max_by(|left, right| {
                    source_rank(left.source)
                        .cmp(&source_rank(right.source))
                        .then_with(|| left.observed_at.cmp(&right.observed_at))
                })
                .cloned()
        });

        let capability = selected
            .map(|observation| ResolvedCapability {
                state: observation.state,
                value: observation.value.clone(),
                observation: Some(observation),
                matcher: matchers.get(&id).copied(),
            })
            .unwrap_or_else(|| ResolvedCapability {
                state: CapabilityState::Unknown,
                value: None,
                observation: None,
                matcher: matchers.get(&id).copied(),
            });
        resolved.insert(id, capability);
    }

    ResolvedTargetCapabilities {
        capabilities: resolved,
    }
}

/// Difference report returned by compatibility evaluation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompatibilityReport {
    pub compatible: bool,
    pub missing: Vec<CapabilityId>,
    pub unknown: Vec<CapabilityId>,
    pub preferred_missing: Vec<CapabilityId>,
}

/// Evaluate a requirement expression and retain all leaf differences.
pub fn compatibility_report(
    capabilities: &ResolvedTargetCapabilities,
    requirements: &RequirementExpr,
) -> CompatibilityReport {
    let mut report = CompatibilityReport::default();
    collect_report(capabilities, requirements, &mut report);
    report.missing.sort();
    report.missing.dedup();
    report.unknown.sort();
    report.unknown.dedup();
    report.preferred_missing.sort();
    report.preferred_missing.dedup();
    report.compatible = report.missing.is_empty() && report.unknown.is_empty();
    report
}

fn collect_report(
    capabilities: &ResolvedTargetCapabilities,
    expression: &RequirementExpr,
    report: &mut CompatibilityReport,
) {
    match expression {
        RequirementExpr::AllOf(items) => {
            for item in items {
                collect_report(capabilities, item, report);
            }
        }
        RequirementExpr::AnyOf(items) => {
            if evaluate_satisfaction(capabilities, expression) == Satisfaction::Satisfied {
                return;
            }
            let mut alternatives = Vec::new();
            for item in items {
                let mut alternative = CompatibilityReport::default();
                collect_report(capabilities, item, &mut alternative);
                alternatives.push(alternative);
            }
            if alternatives.iter().any(|item| !item.unknown.is_empty()) {
                report
                    .unknown
                    .extend(alternatives.into_iter().flat_map(|item| item.unknown));
            } else {
                report
                    .missing
                    .extend(alternatives.into_iter().flat_map(|item| item.missing));
            }
        }
        RequirementExpr::Not(item) => match evaluate_satisfaction(capabilities, item) {
            Satisfaction::Satisfied => report.missing.push(CapabilityId::from("not")),
            Satisfaction::Unknown => collect_report(capabilities, item, report),
            Satisfaction::Unsatisfied => {}
        },
        RequirementExpr::Capability(requirement) => {
            if requirement.strength != RequirementStrength::Required {
                if !capabilities.satisfies(&RequirementExpr::Capability(requirement.clone())) {
                    report.preferred_missing.push(requirement.id.clone());
                }
                return;
            }
            let resolved = capabilities.get(&requirement.id);
            if resolved.state == CapabilityState::Unknown {
                report.unknown.push(requirement.id.clone());
            } else if !requirement_satisfied(&resolved, requirement) {
                report.missing.push(requirement.id.clone());
            }
        }
    }
}

/// Stable protocol endpoint + dialect identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WireProfileId {
    pub suite: String,
    pub endpoint: String,
    pub version: String,
    pub dialect: String,
}

impl WireProfileId {
    pub fn new(
        suite: impl Into<String>,
        endpoint: impl Into<String>,
        version: impl Into<String>,
        dialect: impl Into<String>,
    ) -> Self {
        Self {
            suite: suite.into(),
            endpoint: endpoint.into(),
            version: version.into(),
            dialect: dialect.into(),
        }
    }
}

/// Canonical identity used to derive a target capability key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalTargetIdentity {
    pub identity_version: u32,
    pub provider_id: String,
    pub credential_scope_fingerprint: String,
    pub canonical_api_base: String,
    pub egress_protocol_suite: String,
    pub egress_endpoint_name: String,
    pub egress_endpoint_version: String,
    pub egress_dialect_id: String,
    pub exact_model_id: String,
}

/// Target key wrapper; only the digest is exposed to callers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TargetKey(pub String);

impl TargetKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Health/circuit-breaker identity wrapper.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TargetInstanceId(pub String);

impl TargetInstanceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Errors from Target identity canonicalization.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetIdentityError {
    #[error("api base is empty")]
    EmptyApiBase,
    #[error("api base is not an absolute URL")]
    RelativeApiBase,
    #[error("api base contains URL userinfo")]
    UserInfo,
    #[error("api base query parameters are not allowed")]
    Query,
    #[error("api base fragment is not allowed")]
    Fragment,
}

/// Canonicalize an API base without making a network request.
pub fn canonicalize_api_base(raw: &str) -> Result<String, TargetIdentityError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(TargetIdentityError::EmptyApiBase);
    }
    let mut url = Url::parse(trimmed).map_err(|_| TargetIdentityError::RelativeApiBase)?;
    if url.host_str().is_none() {
        return Err(TargetIdentityError::RelativeApiBase);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(TargetIdentityError::UserInfo);
    }
    if url.query().is_some() {
        return Err(TargetIdentityError::Query);
    }
    if url.fragment().is_some() {
        return Err(TargetIdentityError::Fragment);
    }
    if let Some(host) = url.host_str().map(str::to_ascii_lowercase) {
        let _ = url.set_host(Some(&host));
    }
    if let Some(port) = url.port() {
        let default =
            (url.scheme() == "http" && port == 80) || (url.scheme() == "https" && port == 443);
        if default {
            let _ = url.set_port(None);
        }
    }
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(if path.is_empty() { "/" } else { &path });
    Ok(url.to_string().trim_end_matches('/').to_string())
}

/// Derive a stable, non-reversible credential scope fingerprint.
pub fn credential_scope_fingerprint(secret: &[u8], scope_material: &str) -> String {
    // HMAC-SHA256 accepts every key length. The error branch is retained as a
    // defensive fallback for future digest implementations and never exposes
    // the key; with the current algorithm it is unreachable.
    let mut mac = match HmacSha256::new_from_slice(secret) {
        Ok(mac) => mac,
        Err(_) => {
            let mut digest = Sha256::new();
            digest.update(b"tiygate/target-scope-fallback/v1\0");
            digest.update(scope_material.as_bytes());
            return hex::encode(digest.finalize());
        }
    };
    mac.update(b"tiygate/target-scope/v1\0");
    mac.update(scope_material.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Derive the public TargetKey from a canonical identity.
pub fn target_key(identity: &CanonicalTargetIdentity) -> TargetKey {
    let encoded = serde_json::to_vec(identity).unwrap_or_default();
    let digest = Sha256::digest(encoded);
    TargetKey(hex::encode(digest))
}

/// Derive a health identity without exposing raw credentials.
pub fn target_instance_id(identity: &CanonicalTargetIdentity) -> TargetInstanceId {
    let digest = Sha256::digest(serde_json::to_vec(identity).unwrap_or_default());
    TargetInstanceId(format!("target:{}", hex::encode(digest)))
}

/// Opaque transform identifier shared by generic planning and concrete
/// protocol providers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TransformId(pub String);

/// A planned conversion step. Concrete executors are owned by `protocols`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedTransform {
    pub id: TransformId,
    #[serde(default)]
    pub preserves: Vec<CapabilityId>,
    #[serde(default)]
    pub consumes: Vec<CapabilityId>,
    #[serde(default)]
    pub produces: Vec<CapabilityId>,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// A target-specific execution plan.  The target, resolved capabilities and
/// transform chain are kept together so a route containing heterogeneous
/// egress dialects cannot accidentally reuse another target's request body or
/// conversion decision.  Raw/materialized body bytes remain owned by the
/// protocol/server layer and are deliberately not part of this core type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedTarget {
    pub target: crate::routing::RoutingTarget,
    pub capabilities: ResolvedTargetCapabilities,
    #[serde(default)]
    pub transforms: Vec<PlannedTransform>,
}

/// Protocol-owned wire requirement extraction boundary. Core owns the
/// exchange contract and generic matcher, while a concrete codec supplies
/// requirements that only exist on its wire carrier (for example CRL opaque
/// input items).
pub trait ProtocolRequirementProvider: Send + Sync {
    fn derive_wire_requirements(
        &self,
        request: &crate::ir::IrRequest,
        ingress_profile: &WireProfileId,
    ) -> ExchangeRequirements;
}

/// Protocol-owned conversion planning boundary. The returned transform IDs
/// are opaque to core; executors resolve them in the concrete protocol crate.
pub trait ProtocolTransformProvider: Send + Sync {
    fn plan_transforms(
        &self,
        request: &crate::ir::IrRequest,
        ingress_profile: &WireProfileId,
        egress_profile: &WireProfileId,
    ) -> Vec<PlannedTransform>;
}

/// Combine independent IR and wire requirement sources without losing any
/// response/continuation contract. Empty expressions are neutral in the
/// conjunction and are retained only as an implementation detail.
pub fn merge_exchange_requirements(
    left: ExchangeRequirements,
    right: ExchangeRequirements,
) -> ExchangeRequirements {
    ExchangeRequirements {
        request: RequirementExpr::all([left.request, right.request]),
        response_contract: RequirementExpr::all([left.response_contract, right.response_contract]),
        continuation: RequirementExpr::all([left.continuation, right.continuation]),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn supported(id: &str, value: Option<CapabilityValue>) -> CapabilityObservation {
        CapabilityObservation {
            capability_id: CapabilityId::from(id),
            state: if value.is_some() {
                CapabilityState::Constrained
            } else {
                CapabilityState::Supported
            },
            value,
            source: EvidenceSource::SemanticProbe,
            observed_at: Utc::now(),
            expires_at: None,
            evidence_version: 1,
            probe_suite_version: Some(1),
            reason_code: None,
            redacted_detail: None,
        }
    }

    #[test]
    fn set_and_range_requirements_match() {
        let mut baseline = BTreeMap::new();
        baseline.insert(
            CapabilityId::from("tools.namespace"),
            BaselineSupport::Supported,
        );
        baseline.insert(
            CapabilityId::from("reasoning.effort.values"),
            BaselineSupport::Supported,
        );
        let caps = resolve_capabilities(
            &baseline,
            [
                supported(
                    "tools.namespace",
                    Some(CapabilityValue::EnumSet(
                        ["functions".to_string(), "code".to_string()]
                            .into_iter()
                            .collect(),
                    )),
                ),
                supported(
                    "reasoning.effort.values",
                    Some(CapabilityValue::EnumSet(
                        ["low".to_string(), "medium".to_string(), "high".to_string()]
                            .into_iter()
                            .collect(),
                    )),
                ),
            ],
            Utc::now(),
        );
        let requirement = RequirementExpr::all([
            RequirementExpr::Capability(CapabilityRequirement::with_value(
                "tools.namespace",
                RequirementStrength::Required,
                CapabilityValue::EnumSet(["functions".to_string()].into_iter().collect()),
            )),
            RequirementExpr::Capability(CapabilityRequirement::with_value(
                "reasoning.effort.values",
                RequirementStrength::Required,
                CapabilityValue::EnumSet(["high".to_string()].into_iter().collect()),
            )),
        ]);
        assert!(caps.satisfies(&requirement));
        assert!(compatibility_report(&caps, &requirement).compatible);
    }

    #[test]
    fn forbidden_cannot_be_overridden() {
        let mut baseline = BTreeMap::new();
        baseline.insert(
            CapabilityId::from("tools.custom"),
            BaselineSupport::Forbidden,
        );
        let observation = supported("tools.custom", None);
        let caps = resolve_capabilities(&baseline, [observation], Utc::now());
        assert_eq!(
            caps.get(&CapabilityId::from("tools.custom")).state,
            CapabilityState::Unsupported
        );
    }

    #[test]
    fn newer_verified_evidence_wins_over_older_catalog() {
        let id = CapabilityId::from("tools.function");
        let old = CapabilityObservation {
            capability_id: id.clone(),
            state: CapabilityState::Unsupported,
            value: None,
            source: EvidenceSource::ExactModelCatalog,
            observed_at: Utc::now() - chrono::Duration::hours(1),
            expires_at: None,
            evidence_version: 1,
            probe_suite_version: None,
            reason_code: None,
            redacted_detail: None,
        };
        let new = supported("tools.function", None);
        let caps = resolve_capabilities(&BTreeMap::new(), [old, new], Utc::now());
        assert_eq!(caps.get(&id).state, CapabilityState::Supported);
    }

    #[test]
    fn api_base_and_target_key_are_stable() {
        assert_eq!(
            canonicalize_api_base("HTTPS://Example.COM:443/v1/").unwrap(),
            "https://example.com/v1"
        );
        let identity = CanonicalTargetIdentity {
            identity_version: 1,
            provider_id: "p".to_string(),
            credential_scope_fingerprint: "scope".to_string(),
            canonical_api_base: "https://example.com/v1".to_string(),
            egress_protocol_suite: "responses".to_string(),
            egress_endpoint_name: "responses".to_string(),
            egress_endpoint_version: "v1".to_string(),
            egress_dialect_id: "auto".to_string(),
            exact_model_id: "m".to_string(),
        };
        assert_eq!(target_key(&identity), target_key(&identity));
        assert_ne!(
            target_key(&identity),
            target_key(&CanonicalTargetIdentity {
                exact_model_id: "other".to_string(),
                ..identity
            })
        );
    }

    #[test]
    fn capability_shape_hash_ignores_order_and_content() {
        let first = ExchangeRequirements {
            request: RequirementExpr::all([
                RequirementExpr::Capability(CapabilityRequirement::with_value(
                    "tools.function",
                    RequirementStrength::Required,
                    CapabilityValue::Bool(true),
                )),
                RequirementExpr::required("transport.sse"),
            ]),
            response_contract: RequirementExpr::all([]),
            continuation: RequirementExpr::all([]),
        };
        let second = ExchangeRequirements {
            request: RequirementExpr::all([
                RequirementExpr::required("transport.sse"),
                RequirementExpr::Capability(CapabilityRequirement::with_value(
                    "tools.function",
                    RequirementStrength::Required,
                    CapabilityValue::Bool(true),
                )),
            ]),
            response_contract: RequirementExpr::all([]),
            continuation: RequirementExpr::all([]),
        };
        assert_eq!(
            capability_shape_hash(&first),
            capability_shape_hash(&second)
        );
        assert_ne!(
            capability_shape_hash(&first),
            capability_shape_hash(&ExchangeRequirements {
                request: RequirementExpr::required("tools.custom"),
                response_contract: RequirementExpr::all([]),
                continuation: RequirementExpr::all([]),
            })
        );
        let namespace = CapabilityRequirement::with_value(
            "tools.namespace",
            RequirementStrength::Required,
            CapabilityValue::EnumSet(["functions".to_string()].into_iter().collect()),
        );
        assert_ne!(
            capability_shape_hash_from_requirements(&[CapabilityRequirement::required(
                "tools.namespace",
            )]),
            capability_shape_hash_from_requirements(&[namespace]),
            "typed constraints must form a distinct admission shape"
        );
        let alternative = ExchangeRequirements {
            request: RequirementExpr::AnyOf(vec![
                RequirementExpr::required("tools.function"),
                RequirementExpr::required("tools.custom"),
            ]),
            response_contract: RequirementExpr::all([]),
            continuation: RequirementExpr::all([]),
        };
        assert!(!is_flat_required_shape(&alternative));
        assert_ne!(
            capability_shape_hash(&alternative),
            capability_shape_hash(&ExchangeRequirements {
                request: RequirementExpr::all([
                    RequirementExpr::required("tools.function"),
                    RequirementExpr::required("tools.custom"),
                ]),
                response_contract: RequirementExpr::all([]),
                continuation: RequirementExpr::all([]),
            })
        );
    }

    #[test]
    fn preferred_requirement_is_reported_without_blocking_compatibility() {
        let mut baseline = BTreeMap::new();
        baseline.insert(
            CapabilityId::from("tools.function"),
            BaselineSupport::Supported,
        );
        let caps = resolve_capabilities(&baseline, [], Utc::now());
        let expression = RequirementExpr::Capability(CapabilityRequirement {
            id: CapabilityId::from("tools.function"),
            strength: RequirementStrength::Preferred,
            value: None,
        });
        assert!(!caps.satisfies(&expression));
        let report = compatibility_report(&caps, &expression);
        assert!(report.compatible);
        assert_eq!(
            report.preferred_missing,
            vec![CapabilityId::from("tools.function")]
        );
    }

    #[test]
    fn ir_requirements_include_required_and_specific_tool_choice() {
        let mut request = crate::ir::IrRequest {
            model: "m".to_string(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            stream: false,
            ingress_protocol: crate::ProtocolEndpoint::new(
                crate::ProtocolSuite::OpenAiResponses,
                "responses",
                "v1",
            ),
            metadata: None,
            extensions: HashMap::new(),
        };
        request
            .extensions
            .insert("tool_choice".to_string(), serde_json::json!("required"));
        let required = derive_ir_requirements(&request);
        assert!(required
            .request
            .contains_required(&CapabilityId::from("tools.choice.required")));
        request.extensions.insert(
            "tool_choice".to_string(),
            serde_json::json!({"type":"function","name":"wait"}),
        );
        let specific = derive_ir_requirements(&request);
        assert!(specific
            .request
            .contains_required(&CapabilityId::from("tools.choice.specific")));
    }

    #[test]
    fn ir_requirements_cover_hosted_tools_media_and_private_replay() {
        let request = crate::ir::IrRequest {
            model: "m".to_string(),
            system: None,
            messages: vec![crate::ir::Message {
                role: crate::ir::Role::User,
                content: vec![
                    crate::ir::Content::Media {
                        source: crate::ir::MediaSource::Inline {
                            data: "abc".to_string(),
                        },
                        mime_type: "image/png".to_string(),
                        metadata: HashMap::new(),
                        prompt_cache_breakpoint: None,
                    },
                    crate::ir::Content::Reasoning {
                        text: String::new(),
                        signature: None,
                        id: None,
                        encrypted_content: Some("opaque".to_string()),
                    },
                ],
            }],
            tools: vec![crate::ir::Tool {
                tool_type: Some("web_search".to_string()),
                ..Default::default()
            }],
            params: Default::default(),
            response_format: None,
            stream: false,
            ingress_protocol: crate::ProtocolEndpoint::new(
                crate::ProtocolSuite::OpenAiResponses,
                "responses",
                "v1",
            ),
            metadata: None,
            extensions: HashMap::new(),
        };
        let requirements = derive_ir_requirements(&request);
        for id in [
            "tools.hosted.web_search",
            "media.input.image.inline",
            "reasoning.encrypted_replay",
        ] {
            assert!(
                requirements
                    .request
                    .contains_required(&CapabilityId::from(id)),
                "missing {id}"
            );
        }
    }

    #[test]
    fn opaque_values_are_not_automatic_route_evidence() {
        let mut baseline = BTreeMap::new();
        baseline.insert(
            CapabilityId::from("opaque.capability"),
            BaselineSupport::ExtensionUnknown,
        );
        let caps = resolve_capabilities(
            &baseline,
            [CapabilityObservation {
                capability_id: CapabilityId::from("opaque.capability"),
                state: CapabilityState::Constrained,
                value: Some(CapabilityValue::Opaque(serde_json::json!({"ok": true}))),
                source: EvidenceSource::SemanticProbe,
                observed_at: Utc::now(),
                expires_at: None,
                evidence_version: 1,
                probe_suite_version: None,
                reason_code: None,
                redacted_detail: None,
            }],
            Utc::now(),
        );
        assert!(!caps.satisfies(&RequirementExpr::Capability(
            CapabilityRequirement::with_value(
                "opaque.capability",
                RequirementStrength::Required,
                CapabilityValue::Opaque(serde_json::json!({"ok": true})),
            ),
        )));
    }

    #[test]
    fn unknown_capability_value_kind_round_trips_as_non_routable() {
        let raw = serde_json::json!({
            "kind": "future_range",
            "value": {"lower": 1, "upper": 3}
        });
        let value: CapabilityValue = serde_json::from_value(raw.clone()).expect("unknown value");
        assert!(matches!(value, CapabilityValue::Unknown { .. }));
        assert_eq!(serde_json::to_value(&value).expect("serialize value"), raw);
        assert_eq!(value.kind(), CapabilityValueKind::Opaque);
        assert!(!CapabilityMatcher::Opaque.matches(Some(&value), Some(&value)));
    }

    #[test]
    fn opaque_value_does_not_satisfy_unconstrained_requirement() {
        let mut baseline = BTreeMap::new();
        baseline.insert(
            CapabilityId::from("future.capability"),
            BaselineSupport::ExtensionUnknown,
        );
        let observation = CapabilityObservation {
            capability_id: CapabilityId::from("future.capability"),
            state: CapabilityState::Supported,
            value: Some(CapabilityValue::Unknown {
                kind: "future".to_string(),
                value: serde_json::json!({"enabled": true}),
            }),
            source: EvidenceSource::ExplicitOverride,
            observed_at: Utc::now(),
            expires_at: None,
            evidence_version: 1,
            probe_suite_version: None,
            reason_code: None,
            redacted_detail: None,
        };
        let capabilities = resolve_capabilities(&baseline, [observation], Utc::now());
        assert!(!capabilities.satisfies(&RequirementExpr::required("future.capability",)));
    }

    #[test]
    fn any_of_with_unknown_remains_unknown_fail_closed() {
        let mut baseline = BTreeMap::new();
        baseline.insert(
            CapabilityId::from("known-but-unsupported"),
            BaselineSupport::Supported,
        );
        baseline.insert(
            CapabilityId::from("unknown-capability"),
            BaselineSupport::ExtensionUnknown,
        );
        let mut observations = Vec::new();
        let mut unsupported = CapabilityObservation::now(
            "known-but-unsupported",
            CapabilityState::Unsupported,
            EvidenceSource::SemanticProbe,
            1,
        );
        unsupported.expires_at = None;
        observations.push(unsupported);
        let caps = resolve_capabilities(&baseline, observations, Utc::now());
        let report = compatibility_report(
            &caps,
            &RequirementExpr::AnyOf(vec![
                RequirementExpr::required("known-but-unsupported"),
                RequirementExpr::required("unknown-capability"),
            ]),
        );
        assert!(!report.compatible);
        assert!(!report.unknown.is_empty());
    }

    #[test]
    fn not_unknown_is_not_treated_as_satisfied() {
        let capabilities = ResolvedTargetCapabilities::default();
        let expression =
            RequirementExpr::Not(Box::new(RequirementExpr::required("tools.function")));
        assert!(!capabilities.satisfies(&expression));
        let report = compatibility_report(&capabilities, &expression);
        assert!(!report.compatible);
        assert_eq!(report.unknown, vec![CapabilityId::from("tools.function")]);
    }

    #[test]
    fn not_explicitly_unsupported_is_satisfied() {
        let mut baseline = BTreeMap::new();
        baseline.insert(
            CapabilityId::from("tools.function"),
            BaselineSupport::Supported,
        );
        let observation = CapabilityObservation::now(
            "tools.function",
            CapabilityState::Unsupported,
            EvidenceSource::SemanticProbe,
            1,
        );
        let capabilities = resolve_capabilities(&baseline, [observation], Utc::now());
        let expression =
            RequirementExpr::Not(Box::new(RequirementExpr::required("tools.function")));
        assert!(capabilities.satisfies(&expression));
        assert!(compatibility_report(&capabilities, &expression).compatible);
    }

    fn descriptor(id: &str) -> CapabilityDescriptor {
        CapabilityDescriptor {
            id: CapabilityId::from(id),
            value_kind: CapabilityValueKind::Bool,
            matcher: CapabilityMatcher::Boolean,
            scope: CapabilityScope::Target,
            implementation_status: ImplementationStatus::Implemented,
            discovery_methods: [DiscoveryMethod::ExplicitOverride].into_iter().collect(),
            routing_eligibility: RoutingEligibility::ShadowEligible,
            dependencies: Vec::new(),
            conversion_relevant: true,
            probe_id: None,
            owner: "test".to_string(),
        }
    }

    #[test]
    fn descriptor_validation_rejects_invalid_registry_fixtures() {
        let duplicate = descriptor("duplicate");
        assert!(validate_capability_descriptors(&[duplicate.clone(), duplicate]).is_err());

        let mut cataloged = descriptor("cataloged");
        cataloged.implementation_status = ImplementationStatus::Cataloged;
        assert!(validate_capability_descriptors(&[cataloged]).is_err());

        let mut enforce = descriptor("enforce");
        enforce.routing_eligibility = RoutingEligibility::EnforceEligible;
        enforce.discovery_methods.clear();
        assert!(validate_capability_descriptors(&[enforce]).is_err());

        let mut probe_mismatch = descriptor("probe");
        probe_mismatch.probe_id = Some("probe.id".to_string());
        assert!(validate_capability_descriptors(&[probe_mismatch]).is_err());

        let mut bad_dependency = descriptor("dependency");
        bad_dependency
            .dependencies
            .push(CapabilityId::from("missing"));
        assert!(validate_capability_descriptors(&[bad_dependency]).is_err());
    }

    #[test]
    fn unknown_enum_values_fail_closed_to_conservative_states() {
        let baseline: BaselineSupport = serde_json::from_str("\"future_support\"")
            .expect("unknown baseline should remain extensible");
        assert_eq!(baseline, BaselineSupport::ExtensionUnknown);
        let state: CapabilityState = serde_json::from_str("\"future_state\"")
            .expect("unknown state should remain routable as unknown");
        assert_eq!(state, CapabilityState::Unknown);
        let eligibility: RoutingEligibility =
            serde_json::from_str("\"future_mode\"").expect("unknown eligibility must fail closed");
        assert_eq!(eligibility, RoutingEligibility::Disabled);
    }
}
