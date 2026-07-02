//! Model catalog sourced from models.dev.
//!
//! The catalog has two layers:
//! - an embedded baseline JSON snapshot committed with the source tree;
//! - a runtime [`ModelCatalogStore`] that can refresh from
//!   `https://models.dev/api.json` and atomically swap in the new snapshot.
//!
//! Readers use lock-free `ArcSwap` snapshots. Refreshes are guarded by an
//! async mutex, so startup warmup, periodic ticks, and manual admin refreshes
//! cannot rebuild concurrently.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const EMBEDDED_MODELS_DEV_JSON: &str =
    include_str!(concat!(env!("OUT_DIR"), "/models_dev_api.generated.json"));
const EMBEDDED_MODEL_CATALOG_SUMMARY_JSON: &str =
    include_str!(concat!(env!("OUT_DIR"), "/models_catalog.generated.json"));
const DEFAULT_MODELS_DEV_URL: &str = "https://models.dev/api.json";
const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const EMBEDDED_SOURCE: &str = "embedded:models.dev/api.json";

/// Version and provenance details for a catalog snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CatalogVersion {
    pub source: String,
    pub checksum: String,
    pub generated_at_unix: i64,
    pub provider_count: usize,
    pub model_count: usize,
}

/// Official pricing normalized to the `/v1/models` extension schema.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelPricing {
    pub currency: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_token_usd_per_million: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_token_usd_per_million: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_token_usd_per_million: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_write_token_usd_per_million: Option<f64>,
    pub source_provider: String,
    pub source_kind: PricingSourceKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PricingSourceKind {
    Official,
    OpenRouterFallback,
    AggregatorFallback,
}

/// Metadata attached to a canonical model id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelMetadata {
    pub id: String,
    pub lab_id: String,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub capabilities: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modalities: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ModelPricing>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub metadata: Map<String, Value>,
}

impl ModelMetadata {
    /// Convert catalog metadata into optional top-level `/v1/models`
    /// extension fields. Unknown fields are omitted, never serialized as null.
    pub fn to_model_extensions(&self) -> Map<String, Value> {
        let mut extensions = Map::new();
        insert_string(&mut extensions, "display_name", &self.display_name);
        insert_string(&mut extensions, "status", "active");
        if let Some(family) = &self.family {
            insert_string(&mut extensions, "family", family);
        }
        if let Some(context) = self.context_window {
            extensions.insert("context_window".to_string(), Value::from(context));
        }
        if let Some(input) = self.max_input_tokens {
            extensions.insert("max_input_tokens".to_string(), Value::from(input));
            extensions.insert("input_token_limit".to_string(), Value::from(input));
        }
        if let Some(output) = self.max_output_tokens {
            extensions.insert("max_output_tokens".to_string(), Value::from(output));
            extensions.insert("output_token_limit".to_string(), Value::from(output));
        }
        if let Some(modalities) = &self.modalities {
            extensions.insert("modalities".to_string(), modalities.clone());
        }
        if !self.capabilities.is_empty() {
            extensions.insert(
                "capabilities".to_string(),
                Value::Object(self.capabilities.clone()),
            );
        }
        if let Some(pricing) = &self.pricing {
            if let Ok(value) = serde_json::to_value(pricing) {
                extensions.insert("pricing".to_string(), value);
            }
        }
        if !self.metadata.is_empty() {
            extensions.insert("metadata".to_string(), Value::Object(self.metadata.clone()));
        }
        extensions
    }
}

/// Models grouped under a normalized lab.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LabCatalog {
    pub id: String,
    pub display_name: String,
    pub official_provider_aliases: Vec<String>,
    pub canonical_models: Vec<String>,
}

/// Immutable snapshot read by data-plane and admin handlers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelCatalog {
    pub version: CatalogVersion,
    pub labs: BTreeMap<String, LabCatalog>,
    pub models: BTreeMap<String, ModelMetadata>,
}

impl ModelCatalog {
    pub fn load_embedded() -> Result<Self, CatalogError> {
        Self::from_models_dev_json(EMBEDDED_MODELS_DEV_JSON, EMBEDDED_SOURCE)
    }

    pub fn load_embedded_summary() -> Result<Value, CatalogError> {
        Ok(serde_json::from_str(EMBEDDED_MODEL_CATALOG_SUMMARY_JSON)?)
    }

    pub fn from_models_dev_json(raw: &str, source: &str) -> Result<Self, CatalogError> {
        let value: Value = serde_json::from_str(raw)?;
        build_catalog(value, source, raw)
    }

    pub fn get_model(&self, id: &str) -> Option<&ModelMetadata> {
        let canonical = canonical_model_id_str(id);
        // 1. Exact match on canonical id
        self.models
            .get(&canonical)
            .or_else(|| {
                self.models
                    .values()
                    .find(|m| m.id.eq_ignore_ascii_case(&canonical))
            })
            .or_else(|| {
                // 2. Suffix match: strip lab prefix (e.g. "openai/gpt-image-2" → "gpt-image-2")
                match canonical.rsplit_once('/') {
                    Some((_, suffix)) if !suffix.is_empty() => {
                        self.models.get(suffix).or_else(|| {
                            self.models
                                .values()
                                .find(|m| m.id.eq_ignore_ascii_case(suffix))
                        })
                    }
                    None => None,
                    Some(_) => None,
                }
            })
            .or_else(|| {
                // 3. Fingerprint match: normalize separators, strip suffixes,
                // lowercase — handles variants like "MiniMax-M3" vs "minimax-m3",
                // "glm-5.2" vs "glm-5-2", "kimi-k2.6" vs "kimi-k-2-6",
                // "claude-opus-4.6" vs "claude-opus-4-6",
                // "deepseek-v4-flash-free" vs "deepseek-v4-flash"
                let target = model_fingerprint(&canonical);
                self.models
                    .values()
                    .find(|m| model_fingerprint(&m.id) == target)
            })
    }

    pub fn list_models(&self) -> impl Iterator<Item = &ModelMetadata> {
        self.models.values()
    }
}

/// Runtime holder for the current catalog snapshot.
pub struct ModelCatalogStore {
    current: arc_swap::ArcSwap<ModelCatalog>,
    refresh_lock: Mutex<()>,
    client: reqwest::Client,
    source_url: String,
}

impl ModelCatalogStore {
    pub fn load_embedded() -> Result<Arc<Self>, CatalogError> {
        let catalog = ModelCatalog::load_embedded()?;
        Ok(Arc::new(Self::new(catalog)))
    }

    pub fn new(catalog: ModelCatalog) -> Self {
        Self::new_with_source_url(catalog, DEFAULT_MODELS_DEV_URL)
    }

    pub fn new_with_source_url(catalog: ModelCatalog, source_url: impl Into<String>) -> Self {
        Self {
            current: arc_swap::ArcSwap::from_pointee(catalog),
            refresh_lock: Mutex::new(()),
            client: reqwest::Client::new(),
            source_url: source_url.into(),
        }
    }

    pub fn snapshot(&self) -> Arc<ModelCatalog> {
        self.current.load_full()
    }

    pub fn current_version(&self) -> CatalogVersion {
        self.snapshot().version.clone()
    }

    pub fn get_model(&self, id: &str) -> Option<ModelMetadata> {
        self.snapshot().get_model(id).cloned()
    }

    pub fn list_models(&self) -> Vec<ModelMetadata> {
        self.snapshot().list_models().cloned().collect()
    }

    pub async fn refresh_async(&self) -> Result<CatalogVersion, CatalogError> {
        let _guard = self.refresh_lock.lock().await;
        let body = self
            .client
            .get(&self.source_url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let catalog = ModelCatalog::from_models_dev_json(&body, &self.source_url)?;
        let version = catalog.version.clone();
        self.current.store(Arc::new(catalog));
        info!(
            checksum = %version.checksum,
            providers = version.provider_count,
            models = version.model_count,
            "model catalog refreshed"
        );
        Ok(version)
    }

    pub fn spawn_refresh(self: &Arc<Self>) -> ModelCatalogRefreshHandle {
        self.spawn_refresh_with_interval(DEFAULT_REFRESH_INTERVAL)
    }

    pub fn spawn_refresh_with_interval(
        self: &Arc<Self>,
        interval: Duration,
    ) -> ModelCatalogRefreshHandle {
        let store = self.clone();
        let handle = tokio::spawn(async move {
            // Warm up once immediately after startup. Failures keep the embedded
            // baseline active and are retried on the regular 24h cadence.
            if let Err(e) = store.refresh_async().await {
                warn!(error = %e, "model catalog warmup refresh failed; using embedded baseline");
            }
            let mut tick = tokio::time::interval(interval);
            loop {
                tick.tick().await;
                if let Err(e) = store.refresh_async().await {
                    warn!(error = %e, "model catalog periodic refresh failed; keeping previous snapshot");
                }
            }
        });
        ModelCatalogRefreshHandle { handle }
    }
}

pub struct ModelCatalogRefreshHandle {
    handle: JoinHandle<()>,
}

impl ModelCatalogRefreshHandle {
    pub async fn stop(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("models.dev JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("models.dev HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("models.dev payload must be a provider object")]
    InvalidRoot,
}

#[derive(Debug, Clone)]
struct CandidateModel {
    provider_id: String,
    model: SourceModel,
    is_official: bool,
    is_openrouter: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct SourceProvider {
    id: String,
    name: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, SourceModel>,
}

#[derive(Debug, Clone, Deserialize)]
struct SourceModel {
    id: String,
    name: Option<String>,
    family: Option<String>,
    attachment: Option<bool>,
    reasoning: Option<bool>,
    tool_call: Option<bool>,
    structured_output: Option<bool>,
    temperature: Option<bool>,
    knowledge: Option<String>,
    release_date: Option<String>,
    last_updated: Option<String>,
    modalities: Option<Value>,
    open_weights: Option<bool>,
    limit: Option<SourceLimit>,
    cost: Option<SourceCost>,
}

#[derive(Debug, Clone, Deserialize)]
struct SourceLimit {
    context: Option<u64>,
    input: Option<u64>,
    output: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct SourceCost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

fn build_catalog(value: Value, source: &str, raw: &str) -> Result<ModelCatalog, CatalogError> {
    let root = value.as_object().ok_or(CatalogError::InvalidRoot)?;
    let mut providers = BTreeMap::new();
    for provider_value in root.values() {
        let provider: SourceProvider = serde_json::from_value(provider_value.clone())?;
        providers.insert(provider.id.clone(), provider);
    }

    let mut candidates: BTreeMap<String, Vec<CandidateModel>> = BTreeMap::new();
    let mut lab_display_names: BTreeMap<String, String> = BTreeMap::new();
    let mut lab_aliases: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for provider in providers.values() {
        let lab_id = normalized_lab_id(&provider.id);
        lab_display_names.entry(lab_id.clone()).or_insert_with(|| {
            provider
                .name
                .clone()
                .unwrap_or_else(|| titleize_lab(&lab_id))
        });
        let is_official = is_official_provider_alias(&provider.id);
        if is_official {
            lab_aliases
                .entry(lab_id.clone())
                .or_default()
                .insert(provider.id.clone());
        }
        let is_openrouter = provider.id == "openrouter";
        for model in provider.models.values() {
            // Use fingerprint as the grouping key so that variants
            // like "claude-sonnet-4.6" and "claude-sonnet-4-6" merge
            // into one candidate list. The original id is preserved
            // on the SourceModel for display purposes.
            let canonical_id = canonical_model_id(model);
            let fingerprint = model_fingerprint(&canonical_id);
            candidates
                .entry(fingerprint)
                .or_default()
                .push(CandidateModel {
                    provider_id: provider.id.clone(),
                    model: model.clone(),
                    is_official,
                    is_openrouter,
                });
        }
    }

    let mut models = BTreeMap::new();
    let mut lab_models: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (canonical_id, entries) in candidates {
        if let Some(metadata) = build_model_metadata(&canonical_id, &entries) {
            lab_models
                .entry(metadata.lab_id.clone())
                .or_default()
                .insert(metadata.id.clone());
            models.insert(metadata.id.clone(), metadata);
        }
    }

    let mut labs = BTreeMap::new();
    for (lab_id, canonical_models) in lab_models {
        let aliases = lab_aliases
            .remove(&lab_id)
            .unwrap_or_default()
            .into_iter()
            .collect();
        labs.insert(
            lab_id.clone(),
            LabCatalog {
                display_name: lab_display_names
                    .remove(&lab_id)
                    .unwrap_or_else(|| titleize_lab(&lab_id)),
                id: lab_id,
                official_provider_aliases: aliases,
                canonical_models: canonical_models.into_iter().collect(),
            },
        );
    }

    let checksum = sha256_hex(raw.as_bytes());
    let version = CatalogVersion {
        source: source.to_string(),
        checksum,
        generated_at_unix: unix_now(),
        provider_count: providers.len(),
        model_count: models.len(),
    };
    debug!(
        providers = version.provider_count,
        models = version.model_count,
        checksum = %version.checksum,
        "model catalog built"
    );
    Ok(ModelCatalog {
        version,
        labs,
        models,
    })
}

fn build_model_metadata(fingerprint: &str, entries: &[CandidateModel]) -> Option<ModelMetadata> {
    let preferred = choose_metadata_candidate(entries)?;
    let pricing_candidate = choose_pricing_candidate(entries);
    let lab_id = normalized_lab_id_from_model_or_provider(&preferred.model, &preferred.provider_id);

    // Use the official provider's original model id as the catalog id
    // when available; otherwise fall back to the preferred candidate's
    // id or the fingerprint. This preserves human-readable ids like
    // "claude-sonnet-4-6" instead of the fingerprint "claude-sonnet-46".
    let official_entry = entries.iter().find(|e| e.is_official);
    let catalog_id = official_entry
        .or(Some(preferred))
        .map(|e| canonical_model_id_str(&e.model.id))
        .unwrap_or_else(|| fingerprint.to_string());

    let limit = preferred.model.limit.as_ref();
    let mut metadata = Map::new();
    if let Some(knowledge) = &preferred.model.knowledge {
        insert_string(&mut metadata, "knowledge_cutoff", knowledge);
    }
    if let Some(release) = &preferred.model.release_date {
        insert_string(&mut metadata, "release_date", release);
    }
    if let Some(updated) = &preferred.model.last_updated {
        insert_string(&mut metadata, "last_updated", updated);
    }
    if let Some(open_weights) = preferred.model.open_weights {
        metadata.insert("open_weights".to_string(), Value::from(open_weights));
    }

    Some(ModelMetadata {
        id: catalog_id.clone(),
        lab_id,
        display_name: preferred
            .model
            .name
            .clone()
            .unwrap_or_else(|| catalog_id.clone()),
        family: preferred.model.family.clone(),
        context_window: limit.and_then(|l| l.context),
        max_input_tokens: limit.and_then(|l| l.input.or(l.context)),
        max_output_tokens: limit.and_then(|l| l.output),
        capabilities: capabilities_from_model(&preferred.model),
        modalities: preferred.model.modalities.clone(),
        pricing: pricing_candidate.and_then(pricing_from_candidate),
        metadata,
    })
}

fn choose_metadata_candidate(entries: &[CandidateModel]) -> Option<&CandidateModel> {
    entries.iter().max_by_key(|candidate| {
        let score = if candidate.is_official {
            3
        } else if candidate.is_openrouter {
            1
        } else {
            2
        };
        (score, metadata_score(&candidate.model))
    })
}

fn choose_pricing_candidate(entries: &[CandidateModel]) -> Option<&CandidateModel> {
    // Priority: official → openrouter → any remaining with cost data
    let with_cost: Vec<&CandidateModel> =
        entries.iter().filter(|c| c.model.cost.is_some()).collect();

    // 1. Official
    if let Some(best) = with_cost
        .iter()
        .filter(|c| c.is_official)
        .max_by_key(|c| metadata_score(&c.model))
    {
        return Some(best);
    }
    // 2. OpenRouter
    if let Some(best) = with_cost
        .iter()
        .filter(|c| c.is_openrouter)
        .max_by_key(|c| metadata_score(&c.model))
    {
        return Some(best);
    }
    // 3. Any remaining provider with cost data
    with_cost.first().copied()
}

fn metadata_score(model: &SourceModel) -> usize {
    let mut score = 0;
    if model.name.is_some() {
        score += 1;
    }
    if model.family.is_some() {
        score += 1;
    }
    if model.limit.is_some() {
        score += 1;
    }
    if model.cost.is_some() {
        score += 1;
    }
    if model.modalities.is_some() {
        score += 1;
    }
    score
}

fn pricing_from_candidate(candidate: &CandidateModel) -> Option<ModelPricing> {
    let cost = candidate.model.cost.as_ref()?;
    Some(ModelPricing {
        currency: "USD".to_string(),
        input_token_usd_per_million: cost.input,
        output_token_usd_per_million: cost.output,
        cached_input_token_usd_per_million: cost.cache_read,
        cached_write_token_usd_per_million: cost.cache_write,
        source_provider: candidate.provider_id.clone(),
        source_kind: if candidate.is_official {
            PricingSourceKind::Official
        } else if candidate.is_openrouter {
            PricingSourceKind::OpenRouterFallback
        } else {
            PricingSourceKind::AggregatorFallback
        },
    })
}

fn capabilities_from_model(model: &SourceModel) -> Map<String, Value> {
    let mut capabilities = Map::new();
    insert_bool(&mut capabilities, "tools", model.tool_call);
    insert_bool(&mut capabilities, "function_calling", model.tool_call);
    insert_bool(&mut capabilities, "tool_choice", model.tool_call);
    insert_bool(&mut capabilities, "reasoning", model.reasoning);
    insert_bool(
        &mut capabilities,
        "structured_outputs",
        model.structured_output,
    );
    insert_bool(&mut capabilities, "json_mode", model.structured_output);
    insert_bool(&mut capabilities, "temperature", model.temperature);
    insert_bool(&mut capabilities, "streaming", Some(true));
    insert_bool(&mut capabilities, "system_messages", Some(true));
    if let Some(modalities) = model.modalities.as_ref().and_then(Value::as_object) {
        if let Some(input) = modalities.get("input").and_then(Value::as_array) {
            let has = |needle: &str| input.iter().any(|v| v.as_str() == Some(needle));
            insert_bool(&mut capabilities, "vision", Some(has("image")));
            insert_bool(&mut capabilities, "audio_input", Some(has("audio")));
            insert_bool(&mut capabilities, "video_input", Some(has("video")));
        }
        if let Some(output) = modalities.get("output").and_then(Value::as_array) {
            let has = |needle: &str| output.iter().any(|v| v.as_str() == Some(needle));
            insert_bool(&mut capabilities, "audio_output", Some(has("audio")));
            insert_bool(&mut capabilities, "image_generation", Some(has("image")));
            insert_bool(&mut capabilities, "video_generation", Some(has("video")));
            insert_bool(&mut capabilities, "embeddings", Some(has("embedding")));
        }
    }
    if let Some(attachment) = model.attachment {
        insert_bool(&mut capabilities, "file_search", Some(attachment));
    }
    capabilities
}

fn normalized_lab_id_from_model_or_provider(model: &SourceModel, provider_id: &str) -> String {
    // 1. If model id has a lab prefix (e.g. "openai/gpt-4o"), use it
    if let Some(prefix) = model.id.split('/').next() {
        if model.id.contains('/') && !prefix.is_empty() {
            return normalized_lab_id(prefix);
        }
    }
    // 2. Try to infer lab from model id prefix (e.g. "doubao-" → bytedance)
    if let Some(lab) = infer_lab_from_model_id(&model.id) {
        return lab;
    }
    // 3. Fall back to provider id
    normalized_lab_id(provider_id)
}

/// Infer the lab from a model id's naming prefix. This covers models
/// whose official provider is not in models.dev but whose model id
/// follows a recognizable naming convention (e.g. "doubao-xxx" → bytedance).
fn infer_lab_from_model_id(id: &str) -> Option<String> {
    let lower = id.to_lowercase();
    let prefixes: &[(&str, &str)] = &[
        ("doubao", "bytedance"),
        ("glm-", "zhipuai"),
        ("kimi-", "moonshotai"),
        ("claude-", "anthropic"),
        ("gpt-", "openai"),
        ("gpt-image", "openai"),
        ("chatgpt-", "openai"),
        ("gemini-", "google"),
        ("o1", "openai"),
        ("o3", "openai"),
        ("o4", "openai"),
        ("deepseek", "deepseek"),
        ("minimax", "minimax"),
        ("qwen", "alibaba"),
        ("yi-", "01ai"),
        ("llama-", "meta"),
        ("mistral-", "mistralai"),
        ("codestral", "mistralai"),
        ("hy3", "tencent"),
        ("hy-", "tencent"),
    ];
    for (prefix, lab) in prefixes {
        if lower.starts_with(prefix) {
            return Some(lab.to_string());
        }
    }
    None
}

fn canonical_model_id(model: &SourceModel) -> String {
    canonical_model_id_str(&model.id)
}

/// Produce a normalized fingerprint for fuzzy model id matching.
///
/// Handles common variations across providers:
/// - Strip lab/org prefixes (`openai/gpt-image-2` → `gpt-image-2`)
/// - Strip provider path segments (`accounts/fireworks/models/glm-5p2` → `glm-5p2`)
/// - Strip `:suffix` tags (`kimi-k2:thinking` → `kimi-k2`)
/// - Strip `-free` / `:free` suffixes (`deepseek-v4-flash-free` → `deepseek-v4-flash`)
/// - Lowercase
/// - Normalize `.` ↔ `-` between alphanumeric segments
///   (`glm-5.2` ↔ `glm-5-2`, `claude-opus-4.6` ↔ `claude-opus-4-6`)
/// - Collapse `k-2-6` ↔ `k2.6` ↔ `k2-6`
///   (`kimi-k2.6` ↔ `kimi-k-2-6`)
fn model_fingerprint(id: &str) -> String {
    // Take the last path segment after any '/'.
    let base = id.rsplit('/').next().unwrap_or(id);

    // Strip `:tag` suffix (e.g. ":thinking", ":free", ":1t").
    let base = base.split(':').next().unwrap_or(base);

    // Strip known decorative suffixes.
    let base = strip_decorative_suffix(base);

    // Lowercase.
    let lower = base.to_lowercase();

    // Normalize separators: treat '.' and '-' between alphanumeric chars as
    // interchangeable. We replace '.' with '-' first, then collapse patterns
    // like "k-2" → "k2" when the surrounding chars are alnum.
    let normalized = lower.replace('.', "-");
    collapse_separators(&normalized)
}

/// Remove trailing decorative suffixes like `-free`, `-latest`, `-tee`,
/// `-fp8`, `-6bit` that some providers append. Suffixes like `-turbo`,
/// `-fast`, `-thinking`, `-preview`, `-highspeed`, `-her`, `-lightning`,
/// `-cheaper` are **not** stripped because they are part of legitimate model
/// names (e.g. `gpt-3.5-turbo`, `gemini-2.0-flash`, `kimi-k2-thinking`,
/// `claude-opus-4-6-preview`, `qwen-plus-highspeed`, `llama-her`,
/// `gpt-4o-lightning`, `deepseek-chat-cheaper`).
fn strip_decorative_suffix(s: &str) -> &str {
    const SUFFIXES: &[&str] = &[
        "-free", "-latest", "-tee", "-fp8", "-6bit", "-0711", "-0905",
    ];
    for suffix in SUFFIXES {
        if s.ends_with(suffix) {
            return &s[..s.len() - suffix.len()];
        }
    }
    s
}

/// Collapse redundant separators: `k-2-6` → `k2-6` → eventually `k26`.
/// More precisely, we remove '-' when it sits between a letter and a digit,
/// so `kimi-k-2-6` → `kimi-k26` and `kimi-k2.6` → `kimi-k26`.
fn collapse_separators(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '-' && i > 0 && i + 1 < chars.len() {
            let prev = chars[i - 1];
            let next = chars[i + 1];
            // Collapse '-' between letter and digit: "k-2" → "k2"
            if (prev.is_ascii_alphabetic() && next.is_ascii_digit())
                || (prev.is_ascii_digit() && next.is_ascii_alphabetic())
            {
                // skip the '-'
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

fn canonical_model_id_str(id: &str) -> String {
    match id.split_once('/') {
        Some((prefix, rest)) if !prefix.is_empty() && !rest.is_empty() => {
            let lab = normalized_lab_id(prefix);
            if is_known_lab(&lab) {
                rest.to_string()
            } else {
                format!("{lab}/{rest}")
            }
        }
        _ => id.to_string(),
    }
}

/// Whether a string is a recognized lab id (i.e. it appears as a value
/// in the `normalized_lab_id` mapping). Used to decide whether to strip
/// a `prefix/model` prefix during canonicalization.
fn is_known_lab(lab: &str) -> bool {
    matches!(
        lab,
        "anthropic"
            | "deepseek"
            | "google"
            | "meta"
            | "minimax"
            | "moonshotai"
            | "openai"
            | "tencent"
            | "xai"
            | "zhipuai"
            | "mistralai"
            | "cohere"
            | "alibaba"
            | "amazon"
            | "microsoft"
    )
}

/// Normalize models.dev provider/lab aliases to one canonical lab id.
pub fn normalized_lab_id(raw: &str) -> String {
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

/// Whether a models.dev provider entry represents a direct official source.
pub fn is_official_provider_alias(provider_id: &str) -> bool {
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

fn insert_string(map: &mut Map<String, Value>, key: &str, value: &str) {
    if !value.is_empty() {
        map.insert(key.to_string(), Value::String(value.to_string()));
    }
}

fn insert_bool(map: &mut Map<String, Value>, key: &str, value: Option<bool>) {
    if let Some(value) = value {
        map.insert(key.to_string(), Value::Bool(value));
    }
}

fn titleize_lab(lab_id: &str) -> String {
    lab_id
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn embedded_catalog_loads() {
        let catalog = ModelCatalog::load_embedded().expect("embedded catalog");
        assert!(catalog.version.provider_count > 0);
        assert!(catalog.version.model_count > 0);
        assert!(catalog.get_model("gpt-4o").is_some() || !catalog.models.is_empty());
    }

    #[test]
    fn embedded_generated_summary_contains_normalized_catalog() {
        let summary = ModelCatalog::load_embedded_summary().expect("summary");
        assert_eq!(summary["schema"], "tiygate.model_catalog.summary.v1");
        assert!(summary["provider_count"].as_u64().unwrap_or_default() > 0);
        let labs = summary["labs"].as_array().expect("labs array");
        assert!(labs.iter().any(|lab| {
            lab["id"] == "zhipuai"
                && lab["official_provider_aliases"]
                    .as_array()
                    .is_some_and(|aliases| aliases.iter().any(|v| v == "zhipuai"))
        }));
    }

    #[test]
    fn canonical_model_id_merges_lab_alias_prefixes() {
        let model = SourceModel {
            id: "zai/glm-test".to_string(),
            name: None,
            family: None,
            attachment: None,
            reasoning: None,
            tool_call: None,
            structured_output: None,
            temperature: None,
            knowledge: None,
            release_date: None,
            last_updated: None,
            modalities: None,
            open_weights: None,
            limit: None,
            cost: None,
        };
        // Known lab prefix "zai" → "zhipuai" is stripped, suffix is canonical
        assert_eq!(canonical_model_id(&model), "glm-test");
    }

    #[test]
    fn checksum_changes_when_model_payload_changes() {
        let one = r#"{"openai":{"id":"openai","models":{"gpt-4o":{"id":"gpt-4o","cost":{"input":1.0}}}}}"#;
        let two = r#"{"openai":{"id":"openai","models":{"gpt-4o":{"id":"gpt-4o","cost":{"input":2.0}}}}}"#;
        let c1 = ModelCatalog::from_models_dev_json(one, "test").expect("catalog one");
        let c2 = ModelCatalog::from_models_dev_json(two, "test").expect("catalog two");
        assert_ne!(c1.version.checksum, c2.version.checksum);
    }

    #[tokio::test]
    async fn refresh_failure_keeps_previous_snapshot() {
        let catalog = ModelCatalog::from_models_dev_json(
            r#"{"openai":{"id":"openai","models":{"gpt-4o":{"id":"gpt-4o","name":"GPT-4o"}}}}"#,
            "test",
        )
        .expect("catalog");
        let store = ModelCatalogStore::new_with_source_url(
            catalog.clone(),
            "http://127.0.0.1:1/models-dev-unavailable",
        );
        let before = store.current_version();
        assert!(store.refresh_async().await.is_err());
        assert_eq!(store.current_version().checksum, before.checksum);
        assert!(store.get_model("gpt-4o").is_some());
    }

    #[tokio::test]
    async fn refresh_success_swaps_to_new_version() {
        let before = ModelCatalog::from_models_dev_json(
            r#"{"openai":{"id":"openai","models":{"gpt-4o":{"id":"gpt-4o","name":"Old"}}}}"#,
            "before",
        )
        .expect("before");
        let after_raw = r#"{"openai":{"id":"openai","models":{"gpt-4o":{"id":"gpt-4o","name":"New","limit":{"context":42}}}}}"#;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(after_raw))
            .mount(&server)
            .await;
        let store = ModelCatalogStore::new_with_source_url(before, server.uri());
        let old_checksum = store.current_version().checksum;
        let version = store.refresh_async().await.expect("refresh");
        assert_ne!(version.checksum, old_checksum);
        let refreshed = store.get_model("gpt-4o").expect("refreshed model");
        assert_eq!(refreshed.display_name, "New");
        assert_eq!(refreshed.context_window, Some(42));
    }

    #[test]
    fn fingerprint_matches_user_variants() {
        // minimax-m3 vs MiniMax-M3
        assert_eq!(
            model_fingerprint("minimax-m3"),
            model_fingerprint("MiniMax-M3")
        );
        // glm-5.2 vs glm-5-2
        assert_eq!(model_fingerprint("glm-5.2"), model_fingerprint("glm-5-2"));
        // kimi-k2.6 vs kimi-k-2-6
        assert_eq!(
            model_fingerprint("kimi-k2.6"),
            model_fingerprint("kimi-k-2-6")
        );
        // claude-opus-4-6 vs claude-opus-4.6
        assert_eq!(
            model_fingerprint("claude-opus-4-6"),
            model_fingerprint("claude-opus-4.6")
        );
        // deepseek-v4-flash vs deepseek-v4-flash-free vs deepseek-v4-flash:free
        assert_eq!(
            model_fingerprint("deepseek-v4-flash"),
            model_fingerprint("deepseek-v4-flash-free")
        );
        assert_eq!(
            model_fingerprint("deepseek-v4-flash"),
            model_fingerprint("deepseek-v4-flash:free")
        );
        // Prefixed ids should also match
        assert_eq!(
            model_fingerprint("openai/gpt-image-2"),
            model_fingerprint("gpt-image-2")
        );
    }

    #[test]
    fn get_model_matches_fuzzy_variants() {
        let raw = json!({
            "minimax": {
                "id": "minimax",
                "name": "MiniMax",
                "models": {
                    "MiniMax-M3": {
                        "id": "MiniMax-M3",
                        "name": "MiniMax M3",
                        "cost": {"input": 1.0, "output": 2.0}
                    }
                }
            },
            "zhipuai": {
                "id": "zhipuai",
                "name": "Zhipu AI",
                "models": {
                    "glm-5.2": {
                        "id": "glm-5.2",
                        "name": "GLM 5.2",
                        "cost": {"input": 0.5, "output": 1.0}
                    }
                }
            }
        });
        let catalog = build_catalog(raw.clone(), "test", &raw.to_string()).expect("catalog");

        // minimax-m3 should match MiniMax-M3
        let m = catalog.get_model("minimax-m3").expect("minimax-m3");
        assert_eq!(m.display_name, "MiniMax M3");

        // glm-5-2 should match glm-5.2
        let m = catalog.get_model("glm-5-2").expect("glm-5-2");
        assert_eq!(m.display_name, "GLM 5.2");
    }

    #[test]
    fn lab_aliases_are_normalized() {
        assert_eq!(normalized_lab_id("zhipuai"), "zhipuai");
        assert_eq!(normalized_lab_id("zai"), "zhipuai");
        assert_eq!(normalized_lab_id("minimax-cn"), "minimax");
        assert_eq!(normalized_lab_id("tencent-tokenhub"), "tencent");
    }

    #[test]
    fn official_pricing_preferred_over_openrouter() {
        let raw = json!({
            "openrouter": {
                "id": "openrouter",
                "name": "OpenRouter",
                "models": {
                    "zhipuai/glm-test": {
                        "id": "zhipuai/glm-test",
                        "name": "GLM Test Router",
                        "limit": {"context": 10, "output": 2},
                        "cost": {"input": 9.0, "output": 10.0}
                    }
                }
            },
            "zhipuai": {
                "id": "zhipuai",
                "name": "Zhipu AI",
                "models": {
                    "zhipuai/glm-test": {
                        "id": "zhipuai/glm-test",
                        "name": "GLM Test",
                        "family": "glm",
                        "reasoning": true,
                        "tool_call": true,
                        "structured_output": true,
                        "modalities": {"input": ["text", "image"], "output": ["text"]},
                        "limit": {"context": 100, "output": 20},
                        "cost": {"input": 1.0, "output": 2.0, "cache_read": 0.5}
                    }
                }
            }
        });
        let catalog = build_catalog(raw.clone(), "test", &raw.to_string()).expect("catalog");
        let model = catalog.get_model("zhipuai/glm-test").expect("model");
        assert_eq!(model.lab_id, "zhipuai");
        assert_eq!(model.context_window, Some(100));
        let pricing = model.pricing.as_ref().expect("pricing");
        assert_eq!(pricing.source_provider, "zhipuai");
        assert_eq!(pricing.source_kind, PricingSourceKind::Official);
        assert_eq!(pricing.input_token_usd_per_million, Some(1.0));
        assert_eq!(model.capabilities["vision"], true);
    }

    #[test]
    fn openrouter_cost_is_fallback_when_no_official_price_exists() {
        let raw = json!({
            "openrouter": {
                "id": "openrouter",
                "name": "OpenRouter",
                "models": {
                    "acme/model-a": {
                        "id": "acme/model-a",
                        "name": "Model A",
                        "limit": {"context": 64, "output": 8},
                        "cost": {"input": 0.25, "output": 0.75}
                    }
                }
            },
            "some-aggregator": {
                "id": "some-aggregator",
                "name": "Aggregator",
                "models": {
                    "acme/model-a": {
                        "id": "acme/model-a",
                        "name": "Model A Alt",
                        "limit": {"context": 32, "output": 4}
                    }
                }
            }
        });
        let catalog = build_catalog(raw.clone(), "test", &raw.to_string()).expect("catalog");
        let pricing = catalog
            .get_model("acme/model-a")
            .and_then(|m| m.pricing.as_ref())
            .expect("fallback pricing");
        assert_eq!(pricing.source_provider, "openrouter");
        assert_eq!(pricing.source_kind, PricingSourceKind::OpenRouterFallback);
        assert_eq!(pricing.input_token_usd_per_million, Some(0.25));
    }

    #[test]
    fn non_official_cost_used_as_fallback_when_no_official_or_openrouter() {
        let raw = json!({
            "some-aggregator": {
                "id": "some-aggregator",
                "name": "Aggregator",
                "models": {
                    "acme/model-a": {
                        "id": "acme/model-a",
                        "name": "Model A",
                        "cost": {"input": 99.0, "output": 100.0}
                    }
                }
            }
        });
        let catalog = build_catalog(raw.clone(), "test", &raw.to_string()).expect("catalog");
        // When no official or OpenRouter pricing exists, aggregator cost
        // is used as fallback (labeled OpenRouterFallback).
        let pricing = catalog
            .get_model("model-a")
            .and_then(|m| m.pricing.as_ref())
            .expect("fallback pricing should exist");
        assert_eq!(pricing.source_provider, "some-aggregator");
        assert_eq!(pricing.source_kind, PricingSourceKind::AggregatorFallback);
        assert_eq!(pricing.input_token_usd_per_million, Some(99.0));
    }

    #[test]
    fn tencent_tokenhub_is_official_alias() {
        let raw = json!({
            "tencent-tokenhub": {
                "id": "tencent-tokenhub",
                "name": "Tencent TokenHub",
                "models": {
                    "hy3-preview": {
                        "id": "hy3-preview",
                        "name": "Hy3 preview",
                        "limit": {"context": 256000, "output": 64000},
                        "cost": {"input": 0.0, "output": 0.0}
                    }
                }
            }
        });
        let catalog = build_catalog(raw.clone(), "test", &raw.to_string()).expect("catalog");
        let model = catalog.get_model("hy3-preview").expect("model");
        assert_eq!(model.lab_id, "tencent");
        let pricing = model.pricing.as_ref().expect("pricing");
        assert_eq!(pricing.source_provider, "tencent-tokenhub");
        assert_eq!(pricing.source_kind, PricingSourceKind::Official);
    }
}
