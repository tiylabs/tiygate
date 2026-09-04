//! Request-specific capability planning for the Responses ingress.
//!
//! The planner is pure with respect to the request path: it reads the
//! atomically published snapshot and never performs a database or network
//! operation. Protocol-specific wire transforms are delegated to
//! `tiygate-protocols`.

use std::collections::HashMap;

use chrono::Utc;
use serde_json::Value;

use tiygate_core::{
    capability_shape_hash, compatibility_report, derive_ir_requirements, is_flat_required_shape,
    merge_exchange_requirements, CapabilityId, CapabilityRoutingMode, CompatibilityReport,
    RequirementExpr, RequirementStrength, ResolvedTargetCapabilities, RoutingTarget, WireProfileId,
};
use tiygate_protocols::responses::{
    crl_required_capability_ids, crl_required_capability_requirements, promote_crl_additional_tools,
};
use tiygate_store::capabilities::wire_profile_for_target;

use super::capabilities::CapabilitySnapshot;
use super::AppState;

/// A target-specific compatibility decision used by telemetry and Admin.
#[derive(Debug, Clone)]
pub struct TargetPlanDiagnostic {
    pub target_key: Option<String>,
    pub health_key: String,
    /// Stable plan status used by telemetry.  `planner_error` is kept
    /// separate from capability `unknown` so an implementation failure is
    /// never interpreted as evidence that a target lacks a capability.
    pub status: String,
    pub missing: Vec<CapabilityId>,
    pub unknown: Vec<CapabilityId>,
    pub transform: Option<String>,
    pub planner_error: Option<String>,
    /// Bounded evidence summary (`capability_id:source`) used for internal
    /// request drill-down. It never contains raw upstream payloads.
    pub evidence: Vec<String>,
}

/// Result of planning a Responses request for a route.
#[derive(Debug, Clone)]
pub struct ResponsesPlan {
    pub targets: Vec<RoutingTarget>,
    /// Target-specific capability and transform plans.  The legacy `targets`
    /// field is retained for the routing strategy API; this vector is the
    /// authoritative per-target planning record used by diagnostics and
    /// future executors.
    pub planned_targets: Vec<tiygate_core::PlannedTarget>,
    /// Target health key → body override. `None` means use the normal IR
    /// encoder; `Some` is either the original raw body or a promoted body.
    pub raw_body_by_health_key: HashMap<String, Option<String>>,
    pub diagnostics: Vec<TargetPlanDiagnostic>,
    pub requirements: Vec<CapabilityId>,
    pub shape_hash: String,
    pub enforce: bool,
}

/// Generic plan for non-CRL requests. It applies the same requirement/profile
/// filtering while leaving protocol conversion to the existing codec path.
#[derive(Debug, Clone)]
pub struct GenericPlan {
    pub targets: Vec<RoutingTarget>,
    pub planned_targets: Vec<tiygate_core::PlannedTarget>,
    pub raw_body_by_health_key: HashMap<String, Option<String>>,
    pub diagnostics: Vec<TargetPlanDiagnostic>,
    pub requirements: Vec<CapabilityId>,
    pub shape_hash: String,
    pub enforce: bool,
}

/// Error returned when enforce mode cannot preserve a required exchange.
#[derive(Debug, Clone)]
pub struct NoCompatibleTarget {
    pub required: Vec<CapabilityId>,
    pub diagnostics: Vec<TargetPlanDiagnostic>,
    pub shape_hash: String,
}

/// Return the route's stable persistence identifier. Legacy in-memory routes
/// do not have one, so the virtual model is used as a non-persistent scope.
pub(crate) fn route_scope(state: &AppState, virtual_model: &str) -> String {
    state
        .current_config()
        .routing_table
        .resolve_entry(virtual_model)
        .and_then(|entry| entry.route_id.clone())
        .unwrap_or_else(|| virtual_model.to_string())
}

/// Capability routing requires the durable profile/snapshot/admission store.
/// Legacy in-memory routers intentionally remain off-only; accepting an
/// `enforce` value from a test or startup config without persistent evidence
/// would otherwise fail open or manufacture an all-Unknown profile.
pub fn effective_mode(state: &AppState, requested: CapabilityRoutingMode) -> CapabilityRoutingMode {
    if state.db_store.is_some() && state.capabilities.load().loaded {
        requested
    } else {
        CapabilityRoutingMode::Off
    }
}

/// Build the first-phase Responses plan.  `original_body` is retained so a
/// native target can receive the same JSON bytes and a promotion target can
/// be transformed without an IR round trip.
pub fn plan_responses(
    state: &AppState,
    mode: CapabilityRoutingMode,
    request: &tiygate_core::IrRequest,
    original_body: &Value,
    original_body_text: &str,
    targets: &[RoutingTarget],
) -> Result<ResponsesPlan, NoCompatibleTarget> {
    let exchange = derive_protocol_requirements(request, "openai-responses-standard");
    let crl_ids = crl_required_capability_ids(original_body);
    let crl_requirements = crl_required_capability_requirements(original_body);
    let mut required_ids = collect_requirement_ids(&exchange.request);
    required_ids.extend(collect_requirement_ids(&exchange.response_contract));
    required_ids.extend(collect_requirement_ids(&exchange.continuation));
    required_ids.extend(crl_ids.iter().map(|id| CapabilityId::from(id.as_str())));
    required_ids.sort();
    required_ids.dedup();
    let shape_hash = if crl_ids.is_empty() {
        capability_shape_hash(&exchange)
    } else {
        let mut shape_exchange = exchange.clone();
        let carrier_requirements = crl_requirements.clone();
        shape_exchange.request = RequirementExpr::all([
            shape_exchange.request,
            RequirementExpr::all(carrier_requirements),
        ]);
        capability_shape_hash(&shape_exchange)
    };

    if mode == CapabilityRoutingMode::Off || required_ids.is_empty() {
        return Ok(ResponsesPlan {
            targets: targets.to_vec(),
            planned_targets: Vec::new(),
            raw_body_by_health_key: HashMap::new(),
            diagnostics: Vec::new(),
            requirements: required_ids,
            shape_hash,
            enforce: false,
        });
    }

    let snapshot = state.capabilities.load_full();
    let promotion_enabled = state.tunables().crl_tool_promotion_enabled;
    let route_id = route_scope(state, request.model.as_str());
    let enforce = mode == CapabilityRoutingMode::Enforce
        && is_flat_required_shape(&exchange)
        && snapshot.shape_is_enforced(&route_id, &shape_hash);
    let mut compatible = Vec::new();
    let mut planned_targets = Vec::new();
    let mut raw_bodies = HashMap::new();
    let mut diagnostics = Vec::new();
    let mut planner_internal_error = false;

    for target in targets {
        let (target_key, capabilities, resolution_error) =
            resolve_target_capabilities(state, &snapshot, target);
        if let Some(error) = resolution_error {
            planner_internal_error = true;
            diagnostics.push(TargetPlanDiagnostic {
                target_key: target_key.map(|key| key.0),
                health_key: target.health_key(),
                status: "planner_error".to_string(),
                missing: Vec::new(),
                unknown: Vec::new(),
                transform: None,
                planner_error: Some(error),
                evidence: Vec::new(),
            });
            continue;
        }
        let mut report = evaluate_exchange(&capabilities, &exchange);
        let mut transform = None;
        let mut planned_transforms = Vec::new();
        let has_crl = !crl_ids.is_empty();

        if has_crl {
            if target.api_protocol.suite != tiygate_core::ProtocolSuite::OpenAiResponses {
                report
                    .missing
                    .push(CapabilityId::from("tools.crl.additional_tools"));
            } else if capabilities
                .satisfies(&RequirementExpr::required("tools.crl.additional_tools"))
            {
                transform = Some("responses.pass_through".to_string());
                planned_transforms.push(tiygate_core::PlannedTransform {
                    id: tiygate_core::TransformId("responses.pass_through".to_string()),
                    preserves: crl_ids
                        .iter()
                        .map(|id| CapabilityId::from(id.as_str()))
                        .collect(),
                    consumes: Vec::new(),
                    produces: Vec::new(),
                    notes: vec!["native CRL carrier passthrough".to_string()],
                });
                raw_bodies.insert(target.health_key(), Some(original_body_text.to_string()));
            } else if !promotion_enabled {
                report
                    .missing
                    .push(CapabilityId::from("tools.crl.additional_tools"));
            } else {
                let egress_dialect_allowed = matches!(
                    target.effective_egress_dialect_id(),
                    "auto" | "openai-responses-standard" | "openai-responses-codex-lite"
                );
                if !egress_dialect_allowed {
                    report
                        .missing
                        .push(CapabilityId::from("tools.crl.additional_tools"));
                } else {
                    let promotion_requirements = crl_requirements
                        .iter()
                        .filter(|requirement| {
                            !requirement.contains_required(&CapabilityId::from(
                                "tools.crl.additional_tools",
                            ))
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    let promotion = RequirementExpr::all(promotion_requirements);
                    let promotion_report = compatibility_report(&capabilities, &promotion);
                    if promotion_report.compatible {
                        match promote_crl_additional_tools(original_body) {
                            Ok(promoted) => match serde_json::to_string(&promoted) {
                                Ok(body) => {
                                    let ingress_profile = WireProfileId::new(
                                        format!("{:?}", request.ingress_protocol.suite)
                                            .to_lowercase(),
                                        request.ingress_protocol.name.clone(),
                                        request.ingress_protocol.version.clone(),
                                        "openai-responses-codex-lite",
                                    );
                                    let egress_profile = wire_profile_for_target(target);
                                    let provider =
                                        tiygate_protocols::capabilities::transform_provider_for(
                                            &egress_profile,
                                        );
                                    let provider_plan = provider.plan_transforms(
                                        request,
                                        &ingress_profile,
                                        &egress_profile,
                                    );
                                    if let Some(planned) = provider_plan.first() {
                                        transform = Some(planned.id.0.clone());
                                        planned_transforms.push(planned.clone());
                                    } else {
                                        transform = Some(
                                            "responses.promote_crl_additional_tools".to_string(),
                                        );
                                        planned_transforms.push(tiygate_core::PlannedTransform {
                                            id: tiygate_core::TransformId(
                                                "responses.promote_crl_additional_tools"
                                                    .to_string(),
                                            ),
                                            preserves: crl_ids
                                                .iter()
                                                .filter(|id| {
                                                    id.as_str() != "tools.crl.additional_tools"
                                                })
                                                .map(|id| CapabilityId::from(id.as_str()))
                                                .collect(),
                                            consumes: vec![CapabilityId::from(
                                                "tools.crl.additional_tools",
                                            )],
                                            produces: Vec::new(),
                                            notes: vec!["promote CRL carrier to top-level tools"
                                                .to_string()],
                                        });
                                    }
                                    report
                                        .missing
                                        .retain(|id| id.as_str() != "tools.crl.additional_tools");
                                    report
                                        .unknown
                                        .retain(|id| id.as_str() != "tools.crl.additional_tools");
                                    raw_bodies.insert(target.health_key(), Some(body));
                                }
                                Err(_) => report
                                    .missing
                                    .push(CapabilityId::from("tools.crl.additional_tools")),
                            },
                            Err(_) => report
                                .missing
                                .push(CapabilityId::from("tools.crl.additional_tools")),
                        }
                    } else {
                        report.missing.extend(promotion_report.missing);
                        report.unknown.extend(promotion_report.unknown);
                    }
                }
            }
        }
        report.missing.sort();
        report.missing.dedup();
        report.unknown.sort();
        report.unknown.dedup();
        let is_compatible = report.missing.is_empty() && report.unknown.is_empty();
        diagnostics.push(TargetPlanDiagnostic {
            target_key: target_key.map(|key| key.0),
            health_key: target.health_key(),
            status: if report.missing.is_empty() && report.unknown.is_empty() {
                "compatible".to_string()
            } else if !report.unknown.is_empty() {
                "unknown".to_string()
            } else {
                "incompatible".to_string()
            },
            missing: report.missing.clone(),
            unknown: report.unknown.clone(),
            transform: transform.clone(),
            planner_error: None,
            evidence: evidence_summary(&capabilities),
        });
        let transforms = if planned_transforms.is_empty() {
            transform
                .as_deref()
                .map(|id| {
                    vec![tiygate_core::PlannedTransform {
                        id: tiygate_core::TransformId(id.to_string()),
                        preserves: Vec::new(),
                        consumes: Vec::new(),
                        produces: Vec::new(),
                        notes: Vec::new(),
                    }]
                })
                .unwrap_or_default()
        } else {
            planned_transforms
        };
        if is_compatible {
            if transform.is_none()
                && target.api_protocol.suite == tiygate_core::ProtocolSuite::OpenAiResponses
            {
                raw_bodies.insert(target.health_key(), Some(original_body_text.to_string()));
            }
            compatible.push(target.clone());
            planned_targets.push(tiygate_core::PlannedTarget {
                target: target.clone(),
                capabilities,
                transforms,
            });
        } else {
            raw_bodies.remove(&target.health_key());
        }
    }

    if enforce && planner_internal_error {
        return Ok(ResponsesPlan {
            targets: targets.to_vec(),
            planned_targets: Vec::new(),
            raw_body_by_health_key: HashMap::new(),
            diagnostics,
            requirements: required_ids,
            shape_hash,
            enforce: false,
        });
    }
    if enforce && compatible.is_empty() {
        return Err(NoCompatibleTarget {
            required: required_ids,
            diagnostics,
            shape_hash,
        });
    }

    Ok(ResponsesPlan {
        targets: if enforce {
            compatible
        } else {
            targets.to_vec()
        },
        planned_targets: if enforce { planned_targets } else { Vec::new() },
        raw_body_by_health_key: if enforce { raw_bodies } else { HashMap::new() },
        diagnostics,
        requirements: required_ids,
        shape_hash,
        enforce,
    })
}

/// Plan a request for any protocol without applying a CRL-specific transform.
pub fn plan_generic(
    state: &AppState,
    mode: CapabilityRoutingMode,
    request: &tiygate_core::IrRequest,
    ingress_protocol: &tiygate_core::ProtocolEndpoint,
    original_body_text: &str,
    targets: &[RoutingTarget],
) -> Result<GenericPlan, NoCompatibleTarget> {
    let exchange = derive_protocol_requirements(request, "auto");
    let shape_hash = capability_shape_hash(&exchange);
    let mut required_ids = collect_requirement_ids(&exchange.request);
    required_ids.extend(collect_requirement_ids(&exchange.response_contract));
    required_ids.extend(collect_requirement_ids(&exchange.continuation));
    required_ids.sort();
    required_ids.dedup();
    if mode == CapabilityRoutingMode::Off || required_ids.is_empty() {
        return Ok(GenericPlan {
            targets: targets.to_vec(),
            planned_targets: Vec::new(),
            raw_body_by_health_key: HashMap::new(),
            diagnostics: Vec::new(),
            requirements: required_ids,
            shape_hash,
            enforce: false,
        });
    }

    let snapshot = state.capabilities.load_full();
    let route_id = route_scope(state, request.model.as_str());
    let enforce = mode == CapabilityRoutingMode::Enforce
        && is_flat_required_shape(&exchange)
        && snapshot.shape_is_enforced(&route_id, &shape_hash);
    let mut compatible = Vec::new();
    let mut planned_targets = Vec::new();
    let mut raw_bodies = HashMap::new();
    let mut diagnostics = Vec::new();
    let mut planner_internal_error = false;
    for target in targets {
        let (target_key, capabilities, resolution_error) =
            resolve_target_capabilities(state, &snapshot, target);
        if let Some(error) = resolution_error {
            planner_internal_error = true;
            diagnostics.push(TargetPlanDiagnostic {
                target_key: target_key.map(|key| key.0),
                health_key: target.health_key(),
                status: "planner_error".to_string(),
                missing: Vec::new(),
                unknown: Vec::new(),
                transform: None,
                planner_error: Some(error),
                evidence: Vec::new(),
            });
            continue;
        }
        let report = evaluate_exchange(&capabilities, &exchange);
        let is_compatible = report.missing.is_empty() && report.unknown.is_empty();
        if is_compatible && target.api_protocol.suite == ingress_protocol.suite {
            raw_bodies.insert(target.health_key(), Some(original_body_text.to_string()));
        }
        diagnostics.push(TargetPlanDiagnostic {
            target_key: target_key.map(|key| key.0),
            health_key: target.health_key(),
            status: if report.missing.is_empty() && report.unknown.is_empty() {
                "compatible".to_string()
            } else if !report.unknown.is_empty() {
                "unknown".to_string()
            } else {
                "incompatible".to_string()
            },
            missing: report.missing.clone(),
            unknown: report.unknown.clone(),
            transform: if is_compatible {
                Some("protocol.pass_through_or_codec".to_string())
            } else {
                None
            },
            planner_error: None,
            evidence: evidence_summary(&capabilities),
        });
        if is_compatible {
            compatible.push(target.clone());
            planned_targets.push(tiygate_core::PlannedTarget {
                target: target.clone(),
                capabilities,
                transforms: vec![tiygate_core::PlannedTransform {
                    id: tiygate_core::TransformId("protocol.pass_through_or_codec".to_string()),
                    preserves: Vec::new(),
                    consumes: Vec::new(),
                    produces: Vec::new(),
                    notes: Vec::new(),
                }],
            });
        }
    }
    if enforce && planner_internal_error {
        return Ok(GenericPlan {
            targets: targets.to_vec(),
            planned_targets: Vec::new(),
            raw_body_by_health_key: HashMap::new(),
            diagnostics,
            requirements: required_ids,
            shape_hash,
            enforce: false,
        });
    }
    if enforce && compatible.is_empty() {
        return Err(NoCompatibleTarget {
            required: required_ids,
            diagnostics,
            shape_hash,
        });
    }
    Ok(GenericPlan {
        targets: if enforce {
            compatible
        } else {
            targets.to_vec()
        },
        planned_targets: if enforce { planned_targets } else { Vec::new() },
        raw_body_by_health_key: if enforce { raw_bodies } else { HashMap::new() },
        diagnostics,
        requirements: required_ids,
        shape_hash,
        enforce,
    })
}

fn evidence_summary(capabilities: &ResolvedTargetCapabilities) -> Vec<String> {
    capabilities
        .capabilities
        .iter()
        .filter_map(|(id, capability)| {
            capability
                .observation
                .as_ref()
                .map(|observation| format!("{}:{}", id, evidence_source_name(observation.source)))
        })
        .take(128)
        .collect()
}

fn evidence_source_name(source: tiygate_core::EvidenceSource) -> &'static str {
    match source {
        tiygate_core::EvidenceSource::ExplicitOverride => "explicit_override",
        tiygate_core::EvidenceSource::SemanticProbe => "semantic_probe",
        tiygate_core::EvidenceSource::SuccessfulTraffic => "successful_traffic",
        tiygate_core::EvidenceSource::ExactModelCatalog => "exact_model_catalog",
        tiygate_core::EvidenceSource::ProviderDocumentation => "provider_documentation",
        tiygate_core::EvidenceSource::ProtocolDefault => "protocol_default",
        tiygate_core::EvidenceSource::Unknown => "unknown",
    }
}

fn collect_requirement_ids(expression: &RequirementExpr) -> Vec<CapabilityId> {
    let mut ids = Vec::new();
    collect_ids(expression, &mut ids);
    ids
}

fn derive_protocol_requirements(
    request: &tiygate_core::IrRequest,
    dialect: &str,
) -> tiygate_core::ExchangeRequirements {
    let ir = derive_ir_requirements(request);
    let profile = WireProfileId::new(
        format!("{:?}", request.ingress_protocol.suite).to_lowercase(),
        request.ingress_protocol.name.clone(),
        request.ingress_protocol.version.clone(),
        dialect,
    );
    let provider = tiygate_protocols::capabilities::requirement_provider_for(&profile);
    merge_exchange_requirements(ir, provider.derive_wire_requirements(request, &profile))
}

fn collect_ids(expression: &RequirementExpr, ids: &mut Vec<CapabilityId>) {
    match expression {
        RequirementExpr::AllOf(items) | RequirementExpr::AnyOf(items) => {
            for item in items {
                collect_ids(item, ids);
            }
        }
        RequirementExpr::Not(item) => collect_ids(item, ids),
        RequirementExpr::Capability(requirement) => {
            if requirement.strength == RequirementStrength::Required {
                ids.push(requirement.id.clone());
            }
        }
    }
}

fn evaluate_exchange(
    capabilities: &ResolvedTargetCapabilities,
    exchange: &tiygate_core::ExchangeRequirements,
) -> CompatibilityReport {
    let mut report = CompatibilityReport::default();
    for expression in [
        &exchange.request,
        &exchange.response_contract,
        &exchange.continuation,
    ] {
        let current = compatibility_report(capabilities, expression);
        report.missing.extend(current.missing);
        report.unknown.extend(current.unknown);
        report.preferred_missing.extend(current.preferred_missing);
    }
    report.missing.sort();
    report.missing.dedup();
    report.unknown.sort();
    report.unknown.dedup();
    report.compatible = report.missing.is_empty() && report.unknown.is_empty();
    report
}

fn resolve_target_capabilities(
    state: &AppState,
    snapshot: &CapabilitySnapshot,
    target: &RoutingTarget,
) -> (
    Option<tiygate_core::TargetKey>,
    ResolvedTargetCapabilities,
    Option<String>,
) {
    let Some(store) = state.db_store.as_ref() else {
        return (None, ResolvedTargetCapabilities::default(), None);
    };
    let (key, _instance) = match store.target_key_for(target) {
        Ok(value) => value,
        Err(error) => {
            return (
                None,
                ResolvedTargetCapabilities::default(),
                Some(format!("target identity resolution failed: {error}")),
            );
        }
    };
    let Some(profile) = snapshot.profile(&key) else {
        return (Some(key), ResolvedTargetCapabilities::default(), None);
    };
    let now = Utc::now();
    let fresh_expired = profile.fresh_until.is_none_or(|until| until <= now);
    let stale_grace_valid = profile.stale_until.is_some_and(|until| until > now);
    let has_active_override =
        profile
            .resolved_capabilities
            .capabilities
            .values()
            .any(|capability| {
                capability.observation.as_ref().is_some_and(|observation| {
                    observation.source == tiygate_core::EvidenceSource::ExplicitOverride
                })
            });
    let probe_version_valid = profile.last_probe_suite_version
        == Some(tiygate_store::capabilities::PROBE_SUITE_VERSION)
        && profile.last_probe_judge_version
            == Some(tiygate_store::capabilities::PROBE_JUDGE_VERSION);
    if (!probe_version_valid || (fresh_expired && !stale_grace_valid)) && !has_active_override {
        return (Some(key), ResolvedTargetCapabilities::default(), None);
    }
    let filtered = profile
        .resolved_capabilities
        .capabilities
        .iter()
        .filter(|(_, capability)| {
            // A profile produced by an unknown registry/probe judge version
            // is not valid evidence.  Keep only independently validated
            // explicit overrides; never let an old probe result authorize
            // enforce merely because an override exists for another field.
            probe_version_valid
                || capability.observation.as_ref().is_some_and(|observation| {
                    observation.source == tiygate_core::EvidenceSource::ExplicitOverride
                })
        })
        .filter(|(id, _)| {
            tiygate_protocols::capabilities::descriptor_for(id).is_some_and(|descriptor| {
                descriptor.implementation_status == tiygate_core::ImplementationStatus::Implemented
                    && descriptor.routing_eligibility != tiygate_core::RoutingEligibility::Disabled
            })
        })
        .map(|(id, capability)| (id.clone(), capability.clone()))
        .collect();
    (
        Some(key),
        ResolvedTargetCapabilities {
            capabilities: filtered,
        },
        None,
    )
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn requirement_ids_are_deterministic() {
        let expression = RequirementExpr::all([
            RequirementExpr::required("tools.custom"),
            RequirementExpr::required("tools.function"),
        ]);
        let mut ids = collect_requirement_ids(&expression);
        ids.sort();
        assert_eq!(
            ids,
            vec![
                CapabilityId::from("tools.custom"),
                CapabilityId::from("tools.function")
            ]
        );
    }
}
