//! Build-time model catalog baseline generator.
//!
//! The generator reads the checked-in `data/models_dev_api.json` snapshot and
//! emits two deterministic artifacts into `OUT_DIR`:
//! - `models_dev_api.generated.json`: canonicalized source JSON for runtime
//!   parsing and checksum stability;
//! - `models_catalog.generated.json`: a normalized summary proving the build
//!   step has applied the catalog normalization rules before embedding.
//!
//! Runtime still rebuilds the rich Rust `ModelCatalog` from the canonical JSON
//! so the parsing rules have one implementation, but the generated summary is
//! embedded and validated by tests as the compile-time normalized artifact.
//!
//! Error handling policy: AGENTS.md forbids `panic!()` in production code and
//! forbids `#[allow(clippy::panic)]` workarounds. Build scripts are compiled
//! with the crate's lints, so this script returns `Result` and exits non-zero
//! with a `cargo:warning=` message instead of panicking.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::{json, Value};

type BuildResult<T> = std::result::Result<T, String>;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("cargo:warning={message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> BuildResult<()> {
    println!("cargo:rerun-if-changed=data/models_dev_api.json");

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let input = manifest_dir.join("data/models_dev_api.json");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let source_output = out_dir.join("models_dev_api.generated.json");
    let catalog_output = out_dir.join("models_catalog.generated.json");

    let raw = fs::read_to_string(&input).map_err(|e| {
        format!(
            "failed to read models.dev baseline {}: {e}",
            input.display()
        )
    })?;
    let parsed: Value = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "models.dev baseline is not valid JSON {}: {e}",
            input.display()
        )
    })?;
    let root = parsed
        .as_object()
        .ok_or("models.dev baseline root must be a provider object")?;

    let generated = serde_json::to_string(&parsed)
        .map_err(|e| format!("failed to serialize generated models.dev baseline: {e}"))?;
    fs::write(&source_output, &generated).map_err(|e| {
        format!(
            "failed to write generated models.dev baseline {}: {e}",
            source_output.display()
        )
    })?;

    let summary = normalized_summary(root);
    let summary_json = serde_json::to_string(&summary)
        .map_err(|e| format!("failed to serialize generated catalog summary: {e}"))?;
    fs::write(&catalog_output, summary_json).map_err(|e| {
        format!(
            "failed to write generated model catalog summary {}: {e}",
            catalog_output.display()
        )
    })?;

    Ok(())
}

fn normalized_summary(root: &serde_json::Map<String, Value>) -> Value {
    let mut labs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut official_aliases: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut model_count = 0usize;

    for provider in root.values().filter_map(Value::as_object) {
        let provider_id = provider
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let lab = normalized_lab_id(provider_id);
        if is_official_provider_alias(provider_id) {
            official_aliases
                .entry(lab.clone())
                .or_default()
                .insert(provider_id.to_string());
        }
        let Some(models) = provider.get("models").and_then(Value::as_object) else {
            continue;
        };
        for model in models.values().filter_map(Value::as_object) {
            let id = model.get("id").and_then(Value::as_str).unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            labs.entry(lab.clone())
                .or_default()
                .insert(canonical_model_id(id));
            model_count += 1;
        }
    }

    let labs = labs
        .into_iter()
        .map(|(id, models)| {
            json!({
                "id": id,
                "official_provider_aliases": official_aliases
                    .remove(&id)
                    .unwrap_or_default()
                    .into_iter()
                    .collect::<Vec<_>>(),
                "canonical_models": models.into_iter().collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();

    json!({
        "schema": "tiygate.model_catalog.summary.v1",
        "provider_count": root.len(),
        "source_model_count": model_count,
        "labs": labs,
    })
}

fn canonical_model_id(id: &str) -> String {
    match id.split_once('/') {
        Some((prefix, rest)) if !prefix.is_empty() && !rest.is_empty() => {
            format!("{}/{}", normalized_lab_id(prefix), rest)
        }
        _ => id.to_string(),
    }
}

fn normalized_lab_id(raw: &str) -> String {
    match raw {
        "zhipuai" | "zai" | "zhipu" | "zhipuai-coding-plan" => "zhipuai".to_string(),
        "minimax" | "minimax-cn" | "minimax-coding-plan" | "minimax-cn-coding-plan" => {
            "minimax".to_string()
        }
        "tencent-tokenhub" | "tencent-coding-plan" | "tencent" => "tencent".to_string(),
        "google-vertex" => "google".to_string(),
        "google-vertex-anthropic" => "anthropic".to_string(),
        other => other
            .strip_suffix("-coding-plan")
            .unwrap_or(other)
            .to_string(),
    }
}

fn is_official_provider_alias(provider_id: &str) -> bool {
    matches!(
        provider_id,
        "anthropic"
            | "deepseek"
            | "google"
            | "google-vertex"
            | "minimax"
            | "minimax-cn"
            | "moonshotai"
            | "openai"
            | "tencent-tokenhub"
            | "xai"
            | "zhipuai"
    )
}
