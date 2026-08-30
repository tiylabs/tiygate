//! Validate the checked-in capability contract and generate the immutable
//! runtime registry.  The request path never reads `protocol-specs` from the
//! filesystem; all source changes are consumed at build time.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use serde::Deserialize;

type BuildResult<T> = Result<T, String>;

#[derive(Debug, Deserialize)]
struct RegistryFile {
    schema_version: u32,
    #[serde(default)]
    catalog_ids: Vec<String>,
    #[serde(default)]
    enforce_eligible_ids: Vec<String>,
    #[serde(default)]
    capability: Vec<RegistryCapability>,
}

#[derive(Debug, Deserialize)]
struct RegistryCapability {
    id: String,
    value_kind: String,
    matcher: String,
    scope: String,
    implementation_status: String,
    #[serde(default)]
    discovery_methods: Vec<String>,
    routing_eligibility: String,
    #[serde(default)]
    dependencies: Vec<String>,
    conversion_relevant: bool,
    #[serde(default)]
    probe_id: Option<String>,
    owner: String,
}

#[derive(Debug, Deserialize)]
struct MatrixFile {
    schema_version: u32,
    #[serde(default)]
    mapping: Vec<MatrixMapping>,
}

#[derive(Debug, Deserialize)]
struct MatrixMapping {
    id: String,
    matrix_ref: String,
}

#[derive(Debug, Deserialize)]
struct BaselineFile {
    schema_version: u32,
    wire_profile: String,
    #[serde(default)]
    capability: Vec<BaselineCapability>,
}

#[derive(Debug, Deserialize)]
struct BaselineCapability {
    id: String,
    support: String,
}

#[derive(Debug, Deserialize)]
struct ProbeFile {
    schema_version: u32,
    #[serde(default)]
    probe: Vec<ProbeDefinition>,
}

#[derive(Debug, Deserialize)]
struct ProbeDefinition {
    id: String,
    kind: String,
    wire_profiles: Vec<String>,
    input_schema: String,
    unique_variable: String,
    control: String,
    timeout_secs: u64,
    max_output_tokens: u32,
    budget_weight: u32,
    probe_suite_version: u32,
    judge_version: u32,
}

#[derive(Debug, Clone)]
struct ProbeManifestEntry {
    id: String,
    kind: String,
    wire_profiles: Vec<String>,
    input_schema: String,
    unique_variable: String,
    control: String,
    timeout_secs: u64,
    max_output_tokens: u32,
    budget_weight: u32,
    probe_suite_version: u32,
    judge_version: u32,
}

#[derive(Debug, Clone)]
struct Descriptor {
    id: String,
    value_kind: String,
    matcher: String,
    scope: String,
    implementation_status: String,
    discovery_methods: Vec<String>,
    routing_eligibility: String,
    dependencies: Vec<String>,
    conversion_relevant: bool,
    probe_id: Option<String>,
    owner: String,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cargo:warning=capability contract validation failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> BuildResult<()> {
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR")
            .map_err(|error| format!("CARGO_MANIFEST_DIR unavailable: {error}"))?,
    );
    let specs_dir = manifest_dir.join("../../protocol-specs/capabilities");
    let repo_root = manifest_dir.join("../..");
    let registry_path = specs_dir.join("registry.toml");
    let matrix_path = specs_dir.join("matrix.toml");
    let registry: RegistryFile = parse_toml(&registry_path)?;
    if registry.schema_version != 1 {
        return Err(format!(
            "{} has unsupported schema_version {}",
            registry_path.display(),
            registry.schema_version
        ));
    }

    let mut descriptors = registry
        .capability
        .into_iter()
        .map(|entry| Descriptor {
            id: entry.id,
            value_kind: entry.value_kind,
            matcher: entry.matcher,
            scope: entry.scope,
            implementation_status: entry.implementation_status,
            discovery_methods: entry.discovery_methods,
            routing_eligibility: entry.routing_eligibility,
            dependencies: entry.dependencies,
            conversion_relevant: entry.conversion_relevant,
            probe_id: entry.probe_id,
            owner: entry.owner,
        })
        .collect::<Vec<_>>();
    let explicit_ids = descriptors
        .iter()
        .map(|descriptor| descriptor.id.clone())
        .collect::<BTreeSet<_>>();
    for id in registry.catalog_ids {
        if explicit_ids.contains(&id) {
            return Err(format!("catalog id duplicates capability entry: {id}"));
        }
        descriptors.push(Descriptor {
            id,
            value_kind: "opaque".to_string(),
            matcher: "opaque".to_string(),
            scope: "target".to_string(),
            implementation_status: "cataloged".to_string(),
            discovery_methods: Vec::new(),
            routing_eligibility: "disabled".to_string(),
            dependencies: Vec::new(),
            conversion_relevant: false,
            probe_id: None,
            owner: "protocols".to_string(),
        });
    }
    validate_descriptors(&descriptors)?;
    validate_enforce_allow_list(&descriptors, &registry.enforce_eligible_ids)?;

    let matrix: MatrixFile = parse_toml(&matrix_path)?;
    if matrix.schema_version != 1 {
        return Err(format!(
            "{} has unsupported schema_version {}",
            matrix_path.display(),
            matrix.schema_version
        ));
    }
    let mut matrix_ids = BTreeSet::new();
    for mapping in matrix.mapping {
        if mapping.id.trim().is_empty() || mapping.matrix_ref.trim().is_empty() {
            return Err(format!(
                "{} contains an empty capability id or matrix reference",
                matrix_path.display()
            ));
        }
        if !ids_for_descriptors(&descriptors).contains(mapping.id.as_str()) {
            return Err(format!(
                "{} references unknown capability {}",
                matrix_path.display(),
                mapping.id
            ));
        }
        if !matrix_ids.insert(mapping.id) {
            return Err(format!(
                "{} repeats capability mapping",
                matrix_path.display()
            ));
        }
        let reference_path = mapping
            .matrix_ref
            .split_once('#')
            .map_or(mapping.matrix_ref.as_str(), |(path, _)| path);
        let reference_path = reference_path.trim();
        if !repo_root.join(reference_path).is_file() {
            return Err(format!(
                "{} references missing documentation file {}",
                matrix_path.display(),
                reference_path
            ));
        }
    }
    for descriptor in &descriptors {
        if descriptor.conversion_relevant && !matrix_ids.contains(descriptor.id.as_str()) {
            return Err(format!(
                "conversion-relevant capability {} has no matrix mapping",
                descriptor.id
            ));
        }
    }

    let ids = descriptors
        .iter()
        .map(|descriptor| descriptor.id.as_str())
        .collect::<BTreeSet<_>>();
    let conversion_ids = descriptors
        .iter()
        .filter(|descriptor| descriptor.conversion_relevant)
        .map(|descriptor| descriptor.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut baseline_profiles = BTreeSet::new();
    let mut baselines = Vec::new();
    for relative in [
        "baselines/chat-completions.toml",
        "baselines/messages.toml",
        "baselines/responses.toml",
        "baselines/responses-codex-lite.toml",
        "baselines/gemini.toml",
        "baselines/embeddings.toml",
    ] {
        let path = specs_dir.join(relative);
        let baseline: BaselineFile = parse_toml(&path)?;
        if baseline.schema_version != 1 {
            return Err(format!(
                "{} has unsupported schema_version {}",
                path.display(),
                baseline.schema_version
            ));
        }
        if !baseline_profiles.insert(baseline.wire_profile.clone()) {
            return Err(format!(
                "duplicate baseline wire_profile: {}",
                baseline.wire_profile
            ));
        }
        let mut baseline_ids = BTreeSet::new();
        let mut entries = Vec::new();
        for entry in baseline.capability {
            if !ids.contains(entry.id.as_str()) {
                return Err(format!(
                    "{} references unknown capability {}",
                    path.display(),
                    entry.id
                ));
            }
            if !matches!(
                entry.support.as_str(),
                "supported" | "forbidden" | "extension_unknown"
            ) {
                return Err(format!(
                    "{} has invalid support value {}",
                    path.display(),
                    entry.support
                ));
            }
            if !baseline_ids.insert(entry.id.clone()) {
                return Err(format!(
                    "{} repeats capability {}",
                    path.display(),
                    entry.id
                ));
            }
            entries.push((entry.id, entry.support));
        }
        for id in &conversion_ids {
            if !baseline_ids.contains(*id) {
                return Err(format!(
                    "{} omits conversion-relevant capability {}",
                    path.display(),
                    id
                ));
            }
        }
        baselines.push((baseline.wire_profile, entries));
    }

    let mut probe_ids = BTreeMap::new();
    let mut probe_manifest = Vec::new();
    for relative in ["probes/core.toml", "probes/tools.toml"] {
        let path = specs_dir.join(relative);
        let probes: ProbeFile = parse_toml(&path)?;
        if probes.schema_version != 1 {
            return Err(format!(
                "{} has unsupported schema_version {}",
                path.display(),
                probes.schema_version
            ));
        }
        for probe in probes.probe {
            if probe.id.trim().is_empty()
                || probe.kind.trim().is_empty()
                || probe.wire_profiles.is_empty()
                || probe
                    .wire_profiles
                    .iter()
                    .any(|profile| profile.trim().is_empty())
                || probe.input_schema.trim().is_empty()
                || probe.unique_variable.trim().is_empty()
                || probe.control.trim().is_empty()
                || probe.timeout_secs == 0
                || probe.max_output_tokens == 0
                || probe.budget_weight == 0
                || probe.timeout_secs > 600
                || probe.max_output_tokens > 8192
                || probe.budget_weight > 100
                || probe.probe_suite_version == 0
                || probe.judge_version == 0
            {
                return Err(format!(
                    "{} contains incomplete probe metadata",
                    path.display()
                ));
            }
            for wire_profile in &probe.wire_profiles {
                if !matches!(
                    wire_profile.as_str(),
                    "*" | "*generation"
                        | "*embeddings"
                        | "openai-chat-standard"
                        | "openai-responses-standard"
                        | "openai-responses-codex-lite"
                        | "anthropic-messages-standard"
                        | "gemini-generate-content-standard"
                        | "openai-embeddings-standard"
                ) {
                    return Err(format!(
                        "{} references unsupported wire profile {}",
                        path.display(),
                        wire_profile
                    ));
                }
            }
            if probe_ids
                .insert(probe.id.clone(), probe.kind.clone())
                .is_some()
            {
                return Err(format!("duplicate probe id: {}", probe.id));
            }
            probe_manifest.push(ProbeManifestEntry {
                id: probe.id,
                kind: probe.kind,
                wire_profiles: probe.wire_profiles,
                input_schema: probe.input_schema,
                unique_variable: probe.unique_variable,
                control: probe.control,
                timeout_secs: probe.timeout_secs,
                max_output_tokens: probe.max_output_tokens,
                budget_weight: probe.budget_weight,
                probe_suite_version: probe.probe_suite_version,
                judge_version: probe.judge_version,
            });
        }
    }
    for descriptor in &descriptors {
        let active_probe = descriptor
            .discovery_methods
            .iter()
            .any(|method| method == "active_probe");
        if active_probe {
            let probe_id = descriptor.probe_id.as_deref().ok_or_else(|| {
                format!("active_probe capability has no probe_id: {}", descriptor.id)
            })?;
            if !probe_ids.contains_key(probe_id) {
                return Err(format!(
                    "capability {} references unknown probe {}",
                    descriptor.id, probe_id
                ));
            }
        }
        if let Some(probe_id) = descriptor.probe_id.as_deref() {
            if !active_probe {
                return Err(format!(
                    "capability {} has probe_id without active_probe",
                    descriptor.id
                ));
            }
            if !probe_ids.contains_key(probe_id) {
                return Err(format!(
                    "capability {} references unknown probe {}",
                    descriptor.id, probe_id
                ));
            }
        }
    }

    let out_dir = PathBuf::from(
        std::env::var("OUT_DIR").map_err(|error| format!("OUT_DIR unavailable: {error}"))?,
    );
    let generated = generate_registry(&descriptors, &registry.enforce_eligible_ids)?;
    fs::write(out_dir.join("registry_generated.rs"), generated)
        .map_err(|error| format!("failed to write generated registry: {error}"))?;
    let generated_baselines = generate_baselines(&baselines)?;
    fs::write(out_dir.join("baselines_generated.rs"), generated_baselines)
        .map_err(|error| format!("failed to write generated baselines: {error}"))?;
    let generated_probes = generate_probe_manifest(&probe_manifest)?;
    fs::write(out_dir.join("probes_generated.rs"), generated_probes)
        .map_err(|error| format!("failed to write generated probes: {error}"))?;
    let summary = generate_contract_summary(
        descriptors.len(),
        registry.enforce_eligible_ids.len(),
        matrix_ids.len(),
        baseline_profiles.len(),
        probe_ids.len(),
    );
    fs::write(out_dir.join("contract_summary_generated.rs"), summary)
        .map_err(|error| format!("failed to write contract summary: {error}"))?;

    println!("cargo:rerun-if-changed={}", registry_path.display());
    for relative in [
        "baselines/chat-completions.toml",
        "baselines/messages.toml",
        "baselines/responses.toml",
        "baselines/responses-codex-lite.toml",
        "baselines/gemini.toml",
        "baselines/embeddings.toml",
        "probes/core.toml",
        "probes/tools.toml",
        "matrix.toml",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            specs_dir.join(relative).display()
        );
    }
    Ok(())
}

fn ids_for_descriptors(descriptors: &[Descriptor]) -> BTreeSet<&str> {
    descriptors
        .iter()
        .map(|descriptor| descriptor.id.as_str())
        .collect()
}

fn generate_probe_manifest(entries: &[ProbeManifestEntry]) -> BuildResult<String> {
    let mut output = String::from(
        "#[derive(Debug, Clone, Copy)]\npub struct GeneratedProbeManifestEntry {\n    pub id: &'static str,\n    pub kind: &'static str,\n    pub wire_profiles: &'static [&'static str],\n    pub input_schema: &'static str,\n    pub unique_variable: &'static str,\n    pub control: &'static str,\n    pub timeout_secs: u64,\n    pub max_output_tokens: u32,\n    pub budget_weight: u32,\n    pub probe_suite_version: u32,\n    pub judge_version: u32,\n}\n\npub fn generated_probe_manifest() -> &'static [GeneratedProbeManifestEntry] {\n    &[\n",
    );
    for entry in entries {
        output.push_str("        GeneratedProbeManifestEntry {\n");
        output.push_str(&format!("            id: {},\n", rust_string(&entry.id)));
        output.push_str(&format!(
            "            kind: {},\n",
            rust_string(&entry.kind)
        ));
        output.push_str("            wire_profiles: &[\n");
        for profile in &entry.wire_profiles {
            output.push_str(&format!("                {},\n", rust_string(profile)));
        }
        output.push_str("            ],\n");
        output.push_str(&format!(
            "            input_schema: {},\n            unique_variable: {},\n            control: {},\n            timeout_secs: {},\n            max_output_tokens: {},\n            budget_weight: {},\n            probe_suite_version: {},\n            judge_version: {},\n        }},\n",
            rust_string(&entry.input_schema),
            rust_string(&entry.unique_variable),
            rust_string(&entry.control),
            entry.timeout_secs,
            entry.max_output_tokens,
            entry.budget_weight,
            entry.probe_suite_version,
            entry.judge_version,
        ));
    }
    output.push_str("    ]\n}\n");
    Ok(output)
}

fn generate_contract_summary(
    descriptor_count: usize,
    enforce_eligible_count: usize,
    matrix_mapping_count: usize,
    baseline_count: usize,
    probe_count: usize,
) -> String {
    format!(
        "pub const CAPABILITY_CONTRACT_SCHEMA_VERSION: u32 = 1;\n\
         pub const CAPABILITY_CONTRACT_SUMMARY: &[(&str, usize)] = &[\n\
             (\"descriptors\", {descriptor_count}),\n\
             (\"enforce_eligible_ids\", {enforce_eligible_count}),\n\
             (\"matrix_mappings\", {matrix_mapping_count}),\n\
             (\"baseline_profiles\", {baseline_count}),\n\
             (\"probe_manifest_entries\", {probe_count}),\n\
         ];\n"
    )
}

fn parse_toml<T: for<'de> Deserialize<'de>>(path: &PathBuf) -> BuildResult<T> {
    let source = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    toml::from_str(&source).map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

fn validate_descriptors(descriptors: &[Descriptor]) -> BuildResult<()> {
    let mut ids = BTreeSet::new();
    for descriptor in descriptors {
        if descriptor.id.trim().is_empty() {
            return Err("capability id must not be empty".to_string());
        }
        if !ids.insert(descriptor.id.as_str()) {
            return Err(format!("duplicate capability id: {}", descriptor.id));
        }
        if !matches!(
            descriptor.value_kind.as_str(),
            "bool"
                | "enum_set"
                | "string_set"
                | "integer_range"
                | "decimal_range"
                | "schema_keyword_set"
                | "opaque"
        ) {
            return Err(format!("invalid value_kind for {}", descriptor.id));
        }
        if !matches!(
            descriptor.matcher.as_str(),
            "boolean" | "set_contains" | "range_contains" | "exact_match" | "opaque"
        ) {
            return Err(format!("invalid matcher for {}", descriptor.id));
        }
        if descriptor.matcher == "boolean" && descriptor.value_kind != "bool" {
            return Err(format!(
                "boolean matcher has non-bool value kind: {}",
                descriptor.id
            ));
        }
        if descriptor.matcher == "set_contains"
            && !matches!(
                descriptor.value_kind.as_str(),
                "enum_set" | "string_set" | "schema_keyword_set"
            )
        {
            return Err(format!(
                "set matcher has incompatible value kind: {}",
                descriptor.id
            ));
        }
        if descriptor.matcher == "range_contains"
            && !matches!(
                descriptor.value_kind.as_str(),
                "integer_range" | "decimal_range"
            )
        {
            return Err(format!(
                "range matcher has incompatible value kind: {}",
                descriptor.id
            ));
        }
        if descriptor.matcher == "opaque" && descriptor.value_kind != "opaque" {
            return Err(format!(
                "opaque matcher has non-opaque value kind: {}",
                descriptor.id
            ));
        }
        if !matches!(
            descriptor.scope.as_str(),
            "endpoint" | "model" | "dialect" | "target" | "request"
        ) {
            return Err(format!("invalid scope for {}", descriptor.id));
        }
        if !matches!(
            descriptor.implementation_status.as_str(),
            "cataloged" | "implemented"
        ) {
            return Err(format!(
                "invalid implementation_status for {}",
                descriptor.id
            ));
        }
        if !matches!(
            descriptor.routing_eligibility.as_str(),
            "disabled" | "shadow_eligible" | "enforce_eligible"
        ) {
            return Err(format!("invalid routing_eligibility for {}", descriptor.id));
        }
        if descriptor.implementation_status == "cataloged"
            && descriptor.routing_eligibility != "disabled"
        {
            return Err(format!(
                "cataloged capability must be disabled: {}",
                descriptor.id
            ));
        }
        if descriptor.routing_eligibility == "enforce_eligible"
            && descriptor.implementation_status != "implemented"
        {
            return Err(format!(
                "enforce capability must be implemented: {}",
                descriptor.id
            ));
        }
        if descriptor.routing_eligibility == "enforce_eligible"
            && descriptor.discovery_methods.is_empty()
        {
            return Err(format!(
                "enforce capability needs a discovery method: {}",
                descriptor.id
            ));
        }
        let active_probe = descriptor
            .discovery_methods
            .iter()
            .any(|method| method == "active_probe");
        if active_probe != descriptor.probe_id.is_some() {
            return Err(format!(
                "active probe metadata mismatch for {}",
                descriptor.id
            ));
        }
        for method in &descriptor.discovery_methods {
            if !matches!(
                method.as_str(),
                "explicit_override"
                    | "exact_model_catalog"
                    | "provider_documentation"
                    | "passive_traffic"
                    | "active_probe"
            ) {
                return Err(format!(
                    "invalid discovery method {method} for {}",
                    descriptor.id
                ));
            }
        }
    }

    for descriptor in descriptors {
        for dependency in &descriptor.dependencies {
            if !ids.contains(dependency.as_str()) {
                return Err(format!(
                    "capability {} references unknown dependency {}",
                    descriptor.id, dependency
                ));
            }
        }
    }
    let graph = descriptors
        .iter()
        .map(|descriptor| (descriptor.id.as_str(), descriptor.dependencies.as_slice()))
        .collect::<HashMap<_, _>>();
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for id in graph.keys() {
        visit(id, &graph, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn validate_enforce_allow_list(
    descriptors: &[Descriptor],
    allow_list: &[String],
) -> BuildResult<()> {
    let mut allowed = BTreeSet::new();
    for id in allow_list {
        if id.trim().is_empty() || !allowed.insert(id.as_str()) {
            return Err(format!("invalid or duplicate enforce allow-list ID: {id}"));
        }
        let Some(descriptor) = descriptors.iter().find(|descriptor| descriptor.id == *id) else {
            return Err(format!(
                "enforce allow-list references unknown capability: {id}"
            ));
        };
        if descriptor.implementation_status != "implemented"
            || descriptor.routing_eligibility != "enforce_eligible"
        {
            return Err(format!(
                "enforce allow-list capability is not implemented/enforce_eligible: {id}"
            ));
        }
    }
    for descriptor in descriptors {
        if descriptor.routing_eligibility == "enforce_eligible"
            && !allowed.contains(descriptor.id.as_str())
        {
            return Err(format!(
                "enforce-eligible capability is outside the first-release allow-list: {}",
                descriptor.id
            ));
        }
    }
    Ok(())
}

fn visit<'a>(
    id: &'a str,
    graph: &HashMap<&'a str, &'a [String]>,
    visiting: &mut HashSet<&'a str>,
    visited: &mut HashSet<&'a str>,
) -> BuildResult<()> {
    if visited.contains(id) {
        return Ok(());
    }
    if !visiting.insert(id) {
        return Err(format!("cyclic capability dependency at {id}"));
    }
    if let Some(dependencies) = graph.get(id) {
        for dependency in *dependencies {
            visit(dependency.as_str(), graph, visiting, visited)?;
        }
    }
    visiting.remove(id);
    visited.insert(id);
    Ok(())
}

fn generate_registry(
    descriptors: &[Descriptor],
    enforce_eligible_ids: &[String],
) -> BuildResult<String> {
    let mut output = format!(
        "pub const GENERATED_REGISTRY_COUNT: usize = {};\n\npub const GENERATED_ENFORCE_ELIGIBLE_IDS: &[&str] = &[\n",
        descriptors.len()
    );
    for id in enforce_eligible_ids {
        output.push_str(&format!("    {},\n", rust_string(id)));
    }
    output.push_str("];\n\npub fn generated_registry() -> Vec<tiygate_core::CapabilityDescriptor> {\n    vec![\n");
    for descriptor in descriptors {
        output.push_str("        tiygate_core::CapabilityDescriptor {\n");
        output.push_str(&format!(
            "            id: tiygate_core::CapabilityId::from({}),\n",
            rust_string(&descriptor.id)
        ));
        output.push_str(&format!(
            "            value_kind: tiygate_core::CapabilityValueKind::{},\n",
            value_kind_variant(&descriptor.value_kind)?
        ));
        output.push_str(&format!(
            "            matcher: tiygate_core::CapabilityMatcher::{},\n",
            matcher_variant(&descriptor.matcher)?
        ));
        output.push_str(&format!(
            "            scope: tiygate_core::CapabilityScope::{},\n",
            scope_variant(&descriptor.scope)?
        ));
        output.push_str(&format!(
            "            implementation_status: tiygate_core::ImplementationStatus::{},\n",
            status_variant(&descriptor.implementation_status)?
        ));
        output.push_str("            discovery_methods: [\n");
        for method in &descriptor.discovery_methods {
            output.push_str(&format!(
                "                tiygate_core::DiscoveryMethod::{},\n",
                discovery_variant(method)?
            ));
        }
        output.push_str("            ].into_iter().collect(),\n");
        output.push_str(&format!(
            "            routing_eligibility: tiygate_core::RoutingEligibility::{},\n",
            eligibility_variant(&descriptor.routing_eligibility)?
        ));
        output.push_str("            dependencies: vec![");
        for dependency in &descriptor.dependencies {
            output.push_str(&format!(
                "tiygate_core::CapabilityId::from({}),",
                rust_string(dependency)
            ));
        }
        output.push_str("],\n");
        output.push_str(&format!(
            "            conversion_relevant: {},\n",
            descriptor.conversion_relevant
        ));
        match &descriptor.probe_id {
            Some(probe_id) => output.push_str(&format!(
                "            probe_id: Some({}.to_string()),\n",
                rust_string(probe_id)
            )),
            None => output.push_str("            probe_id: None,\n"),
        }
        output.push_str(&format!(
            "            owner: {}.to_string(),\n",
            rust_string(&descriptor.owner)
        ));
        output.push_str("        },\n");
    }
    output.push_str("    ]\n}\n");
    Ok(output)
}

fn generate_baselines(baselines: &[(String, Vec<(String, String)>)]) -> BuildResult<String> {
    let mut output = String::from(
        "pub fn generated_baseline(\n    profile: &str,\n) -> Option<std::collections::BTreeMap<tiygate_core::CapabilityId, tiygate_core::BaselineSupport>> {\n    match profile {\n",
    );
    for (profile, entries) in baselines {
        output.push_str(&format!("        {} => Some([\n", rust_string(profile)));
        for (id, support) in entries {
            output.push_str(&format!(
                "            (tiygate_core::CapabilityId::from({}), tiygate_core::BaselineSupport::{}),\n",
                rust_string(id),
                baseline_variant(support)?
            ));
        }
        output.push_str("        ].into_iter().collect()),\n");
    }
    output.push_str("        _ => None,\n    }\n}\n");
    Ok(output)
}

fn baseline_variant(value: &str) -> BuildResult<&'static str> {
    match value {
        "supported" => Ok("Supported"),
        "forbidden" => Ok("Forbidden"),
        "extension_unknown" => Ok("ExtensionUnknown"),
        _ => Err(format!("invalid baseline support {value}")),
    }
}

fn rust_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn value_kind_variant(value: &str) -> BuildResult<&'static str> {
    match value {
        "bool" => Ok("Bool"),
        "enum_set" => Ok("EnumSet"),
        "string_set" => Ok("StringSet"),
        "integer_range" => Ok("IntegerRange"),
        "decimal_range" => Ok("DecimalRange"),
        "schema_keyword_set" => Ok("SchemaKeywordSet"),
        "opaque" => Ok("Opaque"),
        _ => Err(format!("invalid value kind {value}")),
    }
}

fn matcher_variant(value: &str) -> BuildResult<&'static str> {
    match value {
        "boolean" => Ok("Boolean"),
        "set_contains" => Ok("SetContains"),
        "range_contains" => Ok("RangeContains"),
        "exact_match" => Ok("ExactMatch"),
        "opaque" => Ok("Opaque"),
        _ => Err(format!("invalid matcher {value}")),
    }
}

fn scope_variant(value: &str) -> BuildResult<&'static str> {
    match value {
        "endpoint" => Ok("Endpoint"),
        "model" => Ok("Model"),
        "dialect" => Ok("Dialect"),
        "target" => Ok("Target"),
        "request" => Ok("Request"),
        _ => Err(format!("invalid scope {value}")),
    }
}

fn status_variant(value: &str) -> BuildResult<&'static str> {
    match value {
        "cataloged" => Ok("Cataloged"),
        "implemented" => Ok("Implemented"),
        _ => Err(format!("invalid implementation status {value}")),
    }
}

fn discovery_variant(value: &str) -> BuildResult<&'static str> {
    match value {
        "explicit_override" => Ok("ExplicitOverride"),
        "exact_model_catalog" => Ok("ExactModelCatalog"),
        "provider_documentation" => Ok("ProviderDocumentation"),
        "passive_traffic" => Ok("PassiveTraffic"),
        "active_probe" => Ok("ActiveProbe"),
        _ => Err(format!("invalid discovery method {value}")),
    }
}

fn eligibility_variant(value: &str) -> BuildResult<&'static str> {
    match value {
        "disabled" => Ok("Disabled"),
        "shadow_eligible" => Ok("ShadowEligible"),
        "enforce_eligible" => Ok("EnforceEligible"),
        _ => Err(format!("invalid routing eligibility {value}")),
    }
}
