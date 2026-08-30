//! Compiled capability registry and protocol/dialect baselines.
//!
//! The human-maintained source files live under `protocol-specs`.  This
//! module keeps the request path independent from filesystem access while
//! exposing a small static registry to the server and admin layers.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use tiygate_core::{
    BaselineSupport, CapabilityDescriptor, CapabilityId, ExchangeRequirements, PlannedTransform,
    ProtocolRequirementProvider, ProtocolTransformProvider, RequirementExpr, TransformId,
    WireProfileId,
};

include!(concat!(env!("OUT_DIR"), "/registry_generated.rs"));
include!(concat!(env!("OUT_DIR"), "/baselines_generated.rs"));
include!(concat!(env!("OUT_DIR"), "/probes_generated.rs"));
include!(concat!(env!("OUT_DIR"), "/contract_summary_generated.rs"));

fn build_registry() -> Vec<CapabilityDescriptor> {
    generated_registry()
}

/// Standard wire adapter. It contributes no extension requirements and is
/// useful for protocol families whose request contract is fully represented
/// by the canonical IR.
#[derive(Debug, Default, Clone, Copy)]
pub struct StandardCapabilityProvider;

impl ProtocolRequirementProvider for StandardCapabilityProvider {
    fn derive_wire_requirements(
        &self,
        request: &tiygate_core::IrRequest,
        _ingress_profile: &WireProfileId,
    ) -> ExchangeRequirements {
        // Responses-specific wire requirements are intentionally extracted
        // from the opaque extension by the provider below; all other fields
        // remain in the canonical IR requirement derivation.
        let _ = request;
        ExchangeRequirements::default()
    }
}

/// Responses adapter for private/opaque carriers that cannot live in core's
/// IR. The decoder stores both the legacy ID list and the typed
/// `responses_wire_requirement_exprs` list so this provider can expose the
/// complete wire contract to the generic planner without importing a
/// Responses JSON type into core.
#[derive(Debug, Default, Clone, Copy)]
pub struct ResponsesCapabilityProvider;

impl ProtocolRequirementProvider for ResponsesCapabilityProvider {
    fn derive_wire_requirements(
        &self,
        request: &tiygate_core::IrRequest,
        ingress_profile: &WireProfileId,
    ) -> ExchangeRequirements {
        if ingress_profile.dialect != "openai-responses-codex-lite"
            && ingress_profile.dialect != "openai-responses-standard"
            && ingress_profile.dialect != "auto"
        {
            return ExchangeRequirements::default();
        }
        // Newer Responses decoders retain the complete typed requirement
        // expressions (including namespace-path constraints).  Fall back to
        // the legacy ID list so older IR snapshots remain readable.
        let typed_requirements = request
            .extensions
            .get("responses_wire_requirement_exprs")
            .and_then(serde_json::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| serde_json::from_value::<RequirementExpr>(item.clone()).ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let requirements = if typed_requirements.is_empty() {
            request
                .extensions
                .get("responses_wire_requirements")
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(RequirementExpr::required)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        } else {
            typed_requirements
        };
        ExchangeRequirements {
            request: RequirementExpr::all(requirements),
            response_contract: RequirementExpr::all([]),
            continuation: RequirementExpr::all([]),
        }
    }
}

impl ProtocolTransformProvider for ResponsesCapabilityProvider {
    fn plan_transforms(
        &self,
        request: &tiygate_core::IrRequest,
        ingress_profile: &WireProfileId,
        egress_profile: &WireProfileId,
    ) -> Vec<PlannedTransform> {
        let has_crl = request
            .extensions
            .get("responses_wire_requirements")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.as_str() == Some("tools.crl.additional_tools"))
            });
        if !has_crl
            || !matches!(
                egress_profile.suite.as_str(),
                "openairesponses" | "openai_responses" | "openai-responses"
            )
            || !matches!(
                egress_profile.dialect.as_str(),
                "openai-responses-standard" | "openai-responses-codex-lite" | "auto"
            )
        {
            return Vec::new();
        }
        let id = if egress_profile.dialect == "openai-responses-codex-lite" {
            "responses.pass_through"
        } else {
            "responses.promote_crl_additional_tools"
        };
        let required_ids = request
            .extensions
            .get("responses_wire_requirements")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(CapabilityId::from)
            .collect::<Vec<_>>();
        let (preserves, consumes, produces) = if id == "responses.pass_through" {
            (required_ids, Vec::new(), Vec::new())
        } else {
            (
                required_ids
                    .into_iter()
                    .filter(|capability| capability.as_str() != "tools.crl.additional_tools")
                    .collect(),
                vec![CapabilityId::from("tools.crl.additional_tools")],
                Vec::new(),
            )
        };
        vec![PlannedTransform {
            id: TransformId(id.to_string()),
            preserves,
            consumes,
            produces,
            notes: vec![format!(
                "{} -> {}",
                ingress_profile.dialect, egress_profile.dialect
            )],
        }]
    }
}

/// Return the provider used for the supplied ingress wire profile.
pub fn requirement_provider_for(profile: &WireProfileId) -> Box<dyn ProtocolRequirementProvider> {
    if matches!(
        profile.suite.as_str(),
        "openairesponses" | "openai_responses" | "openai-responses"
    ) {
        Box::new(ResponsesCapabilityProvider)
    } else {
        Box::new(StandardCapabilityProvider)
    }
}

pub fn transform_provider_for(profile: &WireProfileId) -> Box<dyn ProtocolTransformProvider> {
    if matches!(
        profile.suite.as_str(),
        "openairesponses" | "openai_responses" | "openai-responses"
    ) {
        Box::new(ResponsesCapabilityProvider)
    } else {
        Box::new(StandardCapabilityProvider)
    }
}

impl ProtocolTransformProvider for StandardCapabilityProvider {
    fn plan_transforms(
        &self,
        _request: &tiygate_core::IrRequest,
        _ingress_profile: &WireProfileId,
        _egress_profile: &WireProfileId,
    ) -> Vec<PlannedTransform> {
        Vec::new()
    }
}

/// Return the compiled, validated registry.
pub fn registry() -> &'static [CapabilityDescriptor] {
    static REGISTRY: OnceLock<Vec<CapabilityDescriptor>> = OnceLock::new();
    REGISTRY.get_or_init(build_registry)
}

/// First-release capability IDs allowed to participate in an Enforce
/// admission. The list is generated from `registry.toml`; callers must not
/// infer rollout scope solely from a descriptor's maturity field.
pub fn enforce_eligible_ids() -> &'static [&'static str] {
    GENERATED_ENFORCE_ELIGIBLE_IDS
}

/// Build-time counts for the audited registry/baseline/matrix/probe contract.
/// This is exposed to Admin diagnostics without reading `protocol-specs` on
/// the request path.
pub fn contract_summary() -> &'static [(&'static str, usize)] {
    CAPABILITY_CONTRACT_SUMMARY
}

/// Return the compile-time audited probe manifest. Runtime jobs may only
/// select IDs from this list; request shape, timeout and budget metadata are
/// not loaded from an operator-controlled file.
pub fn probe_manifest() -> &'static [GeneratedProbeManifestEntry] {
    generated_probe_manifest()
}

/// Look up one descriptor by ID.
pub fn descriptor_for(id: &CapabilityId) -> Option<&'static CapabilityDescriptor> {
    registry().iter().find(|descriptor| &descriptor.id == id)
}

/// Return the descriptor-declared matcher map for the generic resolver. This
/// keeps matcher semantics in the static protocol registry while allowing the
/// zero-I/O core resolver to remain protocol agnostic.
pub fn matcher_map() -> BTreeMap<CapabilityId, tiygate_core::CapabilityMatcher> {
    registry()
        .iter()
        .map(|descriptor| (descriptor.id.clone(), descriptor.matcher))
        .collect()
}

fn baseline_entries(
    tools_namespace: BaselineSupport,
    tools_custom: BaselineSupport,
    crl_additional_tools: BaselineSupport,
) -> BTreeMap<CapabilityId, BaselineSupport> {
    [
        ("transport.http", BaselineSupport::Supported),
        ("transport.sse", BaselineSupport::Supported),
        ("tools.function", BaselineSupport::Supported),
        ("tools.function.continuation", BaselineSupport::Supported),
        ("tools.choice.required", BaselineSupport::Supported),
        ("tools.choice.specific", BaselineSupport::Supported),
        ("tools.namespace", tools_namespace),
        ("tools.custom", tools_custom),
        ("tools.crl.additional_tools", crl_additional_tools),
    ]
    .into_iter()
    .map(|(id, support)| (CapabilityId::from(id), support))
    .collect()
}

/// Return the static baseline for a supported wire profile.
pub fn baseline_for(profile: &WireProfileId) -> BTreeMap<CapabilityId, BaselineSupport> {
    let dialect = if profile.dialect == "auto" {
        auto_dialect_for(profile)
    } else {
        profile.dialect.as_str()
    };
    if let Some(baseline) = generated_baseline(dialect) {
        return baseline;
    }
    match dialect {
        "openai-responses-codex-lite" => baseline_entries(
            BaselineSupport::Supported,
            BaselineSupport::Supported,
            BaselineSupport::Supported,
        ),
        "openai-chat-standard" => baseline_entries(
            BaselineSupport::Forbidden,
            BaselineSupport::Supported,
            BaselineSupport::Forbidden,
        ),
        "anthropic-messages-standard" | "gemini-generate-content-standard" => baseline_entries(
            BaselineSupport::Forbidden,
            BaselineSupport::Forbidden,
            BaselineSupport::Forbidden,
        ),
        "openai-embeddings-standard" => baseline_entries(
            BaselineSupport::Forbidden,
            BaselineSupport::Forbidden,
            BaselineSupport::Forbidden,
        ),
        _ => baseline_entries(
            BaselineSupport::Supported,
            BaselineSupport::Supported,
            BaselineSupport::ExtensionUnknown,
        ),
    }
}

fn auto_dialect_for(profile: &WireProfileId) -> &str {
    match profile.suite.as_str() {
        "openai_responses" | "openairesponses" | "openai-responses" => "openai-responses-standard",
        "anthropic_messages" | "anthropicmessages" | "anthropic-messages" => {
            "anthropic-messages-standard"
        }
        "google_gemini" | "googlegemini" | "google-gemini" => "gemini-generate-content-standard",
        "openai_compatible" | "openaicompatible" | "openai-compatible" => {
            if profile.endpoint.contains("embedding") {
                "openai-embeddings-standard"
            } else if profile.endpoint.contains("response") {
                "openai-responses-standard"
            } else {
                "openai-chat-standard"
            }
        }
        _ => "openai-responses-standard",
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_valid_and_contains_complete_catalog() {
        let descriptors = registry();
        assert_eq!(descriptors.len(), GENERATED_REGISTRY_COUNT);
        let enforce_ids = enforce_eligible_ids();
        assert_eq!(
            descriptors
                .iter()
                .filter(|item| item.routing_eligibility
                    == tiygate_core::RoutingEligibility::EnforceEligible)
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            enforce_ids
        );
        assert!(descriptors
            .iter()
            .any(|item| item.id.as_str() == "tools.crl.additional_tools"));
        assert!(descriptors
            .iter()
            .any(|item| item.id.as_str() == "media.input.video.inline"));
    }

    #[test]
    fn baselines_keep_crl_as_extension_for_standard_responses() {
        let profile = WireProfileId::new(
            "openai_responses",
            "responses",
            "v1",
            "openai-responses-standard",
        );
        assert_eq!(
            baseline_for(&profile)[&CapabilityId::from("tools.crl.additional_tools")],
            BaselineSupport::ExtensionUnknown
        );
    }

    #[test]
    fn auto_dialect_uses_suite_and_endpoint_baseline() {
        let chat = baseline_for(&WireProfileId::new(
            "openai_compatible",
            "chat-completions",
            "v1",
            "auto",
        ));
        assert_eq!(
            chat[&CapabilityId::from("tools.namespace")],
            BaselineSupport::Forbidden
        );
        let responses = baseline_for(&WireProfileId::new(
            "openai_responses",
            "responses",
            "v1",
            "auto",
        ));
        assert_eq!(
            responses[&CapabilityId::from("tools.crl.additional_tools")],
            BaselineSupport::ExtensionUnknown
        );
        assert_eq!(
            baseline_for(&WireProfileId::new(
                "anthropic_messages",
                "messages",
                "2023-06-01",
                "auto",
            ))[&CapabilityId::from("media.input.file_id")],
            BaselineSupport::Forbidden
        );
        assert_eq!(
            baseline_for(&WireProfileId::new(
                "openai_compatible",
                "embeddings",
                "v1",
                "auto",
            ))[&CapabilityId::from("embeddings.dimensions")],
            BaselineSupport::Supported
        );
    }

    #[test]
    fn responses_provider_exposes_wire_requirements_and_transform() {
        let mut request = tiygate_core::IrRequest {
            model: "m".to_string(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            stream: false,
            ingress_protocol: tiygate_core::ProtocolEndpoint::new(
                tiygate_core::ProtocolSuite::OpenAiResponses,
                "responses",
                "v1",
            ),
            metadata: None,
            extensions: std::collections::HashMap::new(),
        };
        request.extensions.insert(
            "responses_wire_requirements".to_string(),
            serde_json::json!(["tools.crl.additional_tools", "tools.namespace"]),
        );
        let ingress = WireProfileId::new(
            "openairesponses",
            "responses",
            "v1",
            "openai-responses-standard",
        );
        let provider = ResponsesCapabilityProvider;
        let requirements = provider.derive_wire_requirements(&request, &ingress);
        assert!(requirements
            .request
            .contains_required(&CapabilityId::from("tools.namespace")));
        let transforms = provider.plan_transforms(
            &request,
            &ingress,
            &WireProfileId::new(
                "openairesponses",
                "responses",
                "v1",
                "openai-responses-standard",
            ),
        );
        assert_eq!(transforms[0].id.0, "responses.promote_crl_additional_tools");
    }

    #[test]
    fn protocol_specs_sources_are_valid_toml() {
        let sources = [
            include_str!("../../../protocol-specs/capabilities/registry.toml"),
            include_str!("../../../protocol-specs/capabilities/baselines/chat-completions.toml"),
            include_str!("../../../protocol-specs/capabilities/baselines/messages.toml"),
            include_str!("../../../protocol-specs/capabilities/baselines/responses.toml"),
            include_str!(
                "../../../protocol-specs/capabilities/baselines/responses-codex-lite.toml"
            ),
            include_str!("../../../protocol-specs/capabilities/baselines/gemini.toml"),
            include_str!("../../../protocol-specs/capabilities/baselines/embeddings.toml"),
            include_str!("../../../protocol-specs/capabilities/probes/core.toml"),
            include_str!("../../../protocol-specs/capabilities/probes/tools.toml"),
            include_str!("../../../protocol-specs/capabilities/matrix.toml"),
        ];
        for source in sources {
            let _: toml::Value = toml::from_str(source).expect("capability source TOML");
        }
    }

    #[test]
    fn probe_manifest_contains_audited_metadata() {
        assert!(probe_manifest().iter().all(|probe| {
            !probe.id.is_empty()
                && !probe.wire_profiles.is_empty()
                && probe.timeout_secs > 0
                && probe.max_output_tokens > 0
                && probe.budget_weight > 0
                && probe.probe_suite_version > 0
                && probe.judge_version > 0
        }));
        assert!(probe_manifest()
            .iter()
            .any(|probe| probe.id == "tools.crl.additional_tools"));
    }
}
