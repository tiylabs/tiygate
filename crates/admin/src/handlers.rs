//! Admin API handlers — providers, routes, api-keys, health, stats.
//!
//! Each handler is a thin shim around the corresponding
//! [`DbConfigStore`] method. The handlers are intentionally small
//! and live in a single file so the route map below is the only
//! thing a new contributor has to read to understand the API
//! surface.

#[allow(unused_imports)]
use axum::routing::{patch, post, put};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use tiygate_store::archive::{gzip_decompress, sha256_hex, PayloadArchiveManifest};
use tiygate_store::capabilities::{
    CapabilityMutationIdempotency, CapabilityProfileSummary, CapabilityRouteAdmission,
    TargetCapabilityOverride,
};
use tiygate_store::config_store::{validate_provider_auth_mode, StoreError};
use tiygate_store::model_catalog::ModelMetadata;
use tiygate_store::models::{
    AuthMode, ConfigExport, ImportSelection, OAuthCredentialStatus, Provider, Route, RouteTarget,
};

use crate::state::AdminState;

const OPENAI_PLATFORM_BASE_URL: &str = "https://api.openai.com/v1";
const OPENAI_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// ChatGPT/Codex subscription usage endpoint. This endpoint is used only for
/// OpenAI OAuth providers; OpenAI API-key providers have platform billing
/// semantics rather than the ChatGPT 5-hour / 7-day windows.
const OPENAI_CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
/// ChatGPT/Codex reset-credit details endpoint. This is a private ChatGPT
/// subscription endpoint and must never be used for API-key providers.
const OPENAI_CODEX_RESET_CREDITS_URL: &str =
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits";
/// ChatGPT/Codex reset-credit consume endpoint. Consuming a credit is an
/// upstream side effect, so it is exposed through a dedicated POST route.
const OPENAI_CODEX_RESET_CREDITS_CONSUME_URL: &str =
    "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume";
const OPENAI_CODEX_BETA: &str = "codex-1";
const OPENAI_CODEX_LANGUAGE: &str = "zh-CN";
const OPENAI_CODEX_SEC_FETCH_SITE: &str = "none";
const OPENAI_CODEX_SEC_FETCH_MODE: &str = "no-cors";
const OPENAI_CODEX_SEC_FETCH_DEST: &str = "empty";
const OPENAI_CODEX_PRIORITY: &str = "u=4, i";
const OPENAI_CODEX_RESET_CREDITS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Claude subscription usage endpoint. Anthropic currently exposes this only
/// for OAuth credentials carrying the `user:profile` scope.
const ANTHROPIC_OAUTH_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// The usage endpoint assigns a substantially more permissive rate-limit
/// bucket to clients using Claude Code's product-token shape. TiyGate's package
/// version keeps the value stable and traceable without tracking CLI releases.
const ANTHROPIC_OAUTH_USAGE_USER_AGENT: &str = concat!("claude-code/", env!("CARGO_PKG_VERSION"));

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/admin/v1/health", get(health))
        .route(
            "/admin/v1/providers",
            get(list_providers).post(create_provider),
        )
        .route(
            "/admin/v1/providers/:id/delete-impact",
            get(provider_delete_impact),
        )
        .route(
            "/admin/v1/providers/:id",
            get(get_provider)
                .put(update_provider)
                .delete(delete_provider),
        )
        .route("/admin/v1/providers/:id/usage", get(provider_usage))
        .route(
            "/admin/v1/providers/:id/usage/reset-credits",
            post(provider_usage_reset_credits),
        )
        .route("/admin/v1/routes", get(list_routes).post(create_route))
        .route(
            "/admin/v1/routes/:id",
            get(get_route).put(update_route).delete(delete_route),
        )
        .route(
            "/admin/v1/routes/:id/capability-admissions",
            get(list_capability_route_admissions).post(upsert_capability_route_admission),
        )
        .route(
            "/admin/v1/routes/:id/capability-admissions/:shape_hash",
            axum::routing::delete(delete_capability_route_admission),
        )
        .route(
            "/admin/v1/api-keys",
            get(list_api_keys).post(create_api_key),
        )
        .route(
            "/admin/v1/api-keys/:id",
            get(get_api_key)
                .delete(delete_api_key)
                .put(disable_api_key)
                .patch(update_api_key_quota),
        )
        .route(
            "/admin/v1/api-keys/:id/model-access",
            patch(update_api_key_model_access),
        )
        .route("/admin/v1/provider-catalog", get(list_provider_catalog))
        .route("/admin/v1/model-catalog", get(get_model_catalog))
        .route(
            "/admin/v1/model-catalog/resolve",
            post(resolve_model_catalog_metadata),
        )
        .route(
            "/admin/v1/model-catalog/refresh",
            post(refresh_model_catalog),
        )
        .route("/admin/v1/stats/by-model", get(stats_by_model))
        .route("/admin/v1/stats/by-provider", get(stats_by_provider))
        .route("/admin/v1/stats/by-api-key", get(stats_by_api_key))
        .route("/admin/v1/stats/by-target", get(stats_by_target))
        .route("/admin/v1/stats/token-activity", get(stats_token_activity))
        .route("/admin/v1/stats/token-summary", get(stats_token_summary))
        .route("/admin/v1/audit", get(list_audit))
        .route("/admin/v1/requests", get(list_requests))
        .route(
            "/admin/v1/requests/filter-options",
            get(request_filter_options),
        )
        .route("/admin/v1/requests/:id/replay", get(replay_request))
        .route("/admin/v1/health/circuit-breakers", get(circuit_breakers))
        .route("/admin/v1/config/export", get(export_config))
        .route("/admin/v1/config/import", post(import_config))
        .route(
            "/admin/v1/settings",
            get(list_settings).put(update_settings),
        )
        .route("/admin/v1/providers/:id/models", get(list_provider_models))
        .route("/admin/v1/info", get(info))
        .route(
            "/admin/v1/target-capabilities",
            get(list_target_capabilities),
        )
        .route(
            "/admin/v1/target-capabilities/:target_key",
            get(get_target_capability),
        )
        .route(
            "/admin/v1/target-capabilities/:target_key/probe",
            post(start_target_capability_probe),
        )
        .route(
            "/admin/v1/target-capabilities/:target_key/probe-runs",
            get(list_target_capability_probe_runs),
        )
        .route(
            "/admin/v1/target-capabilities/:target_key/overrides",
            put(upsert_target_capability_override),
        )
        .route(
            "/admin/v1/target-capabilities/:target_key/overrides/:capability_id",
            axum::routing::delete(delete_target_capability_override),
        )
        .route(
            "/admin/v1/capability-registry",
            get(get_capability_registry),
        )
        .route("/admin/v1/capability-metrics", get(list_capability_metrics))
        .route(
            "/admin/v1/capability-probes",
            axum::routing::put(update_capability_probe_worker),
        )
        .route("/admin/v1/probe-jobs/:job_id", get(get_probe_job))
}

#[derive(Debug, Deserialize)]
struct CapabilityListQuery {
    limit: Option<u32>,
    offset: Option<u32>,
}

#[derive(Debug, Deserialize, Default)]
struct CapabilityRevisionQuery {
    expected_revision: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct CapabilityProbeRequest {
    #[serde(default)]
    probe_set: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct CapabilityOverrideRequest {
    capability_id: String,
    state: String,
    value: Option<Value>,
    reason: String,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Deserialize)]
struct CapabilityAdmissionRequest {
    #[serde(default)]
    shape_hash: Option<String>,
    #[serde(default)]
    required_capabilities: Vec<String>,
    /// Optional typed required leaves.  When omitted, each
    /// `required_capabilities` entry is treated as an unconstrained boolean
    /// requirement for backwards compatibility.
    #[serde(default)]
    required_requirements: Vec<tiygate_core::CapabilityRequirement>,
    mode: tiygate_core::CapabilityRoutingMode,
    #[serde(default)]
    expected_revision: Option<i64>,
    #[serde(default)]
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    low_traffic_exception: bool,
    reason: String,
}

/// Normalize the admission shape supplied by the Admin API.  The legacy
/// `required_capabilities` form remains valid and expands to unconstrained
/// required leaves; callers that need a constrained shape (for example a
/// particular CRL namespace path) provide `required_requirements`.
fn normalize_admission_requirements(
    required_capabilities: &[String],
    supplied_requirements: &[tiygate_core::CapabilityRequirement],
) -> Result<
    (
        Vec<tiygate_core::CapabilityId>,
        Vec<tiygate_core::CapabilityRequirement>,
    ),
    AdminError,
> {
    if required_capabilities.len() > 64 || supplied_requirements.len() > 64 {
        return Err(AdminError::InvalidCapability(
            "required capabilities exceeds the limit".to_string(),
        ));
    }
    let mut requirements = if supplied_requirements.is_empty() {
        required_capabilities
            .iter()
            .map(|value| tiygate_core::CapabilityRequirement::required(value.as_str()))
            .collect::<Vec<_>>()
    } else {
        supplied_requirements.to_vec()
    };
    if requirements.is_empty() {
        return Err(AdminError::InvalidCapability(
            "required_capabilities or required_requirements must not be empty".to_string(),
        ));
    }
    if requirements.iter().any(|requirement| {
        requirement.id.as_str().is_empty()
            || requirement.strength != tiygate_core::RequirementStrength::Required
    }) {
        return Err(AdminError::InvalidCapability(
            "admission requirements must have non-empty IDs and required strength".to_string(),
        ));
    }
    requirements.sort_by(|left, right| {
        let left_key = serde_json::to_string(left).unwrap_or_default();
        let right_key = serde_json::to_string(right).unwrap_or_default();
        left_key.cmp(&right_key)
    });
    requirements.dedup();
    let mut ids = requirements
        .iter()
        .map(|requirement| requirement.id.clone())
        .collect::<Vec<_>>();
    ids.sort();
    ids.dedup();
    if !required_capabilities.is_empty() {
        let mut supplied_ids = required_capabilities
            .iter()
            .map(|value| tiygate_core::CapabilityId::from(value.as_str()))
            .collect::<Vec<_>>();
        supplied_ids.sort();
        supplied_ids.dedup();
        if supplied_ids != ids {
            return Err(AdminError::InvalidCapability(
                "required_capabilities does not match required_requirements".to_string(),
            ));
        }
    }
    Ok((ids, requirements))
}

/// Verify that the capability control-plane migrations are available before a
/// control request proceeds. A missing/partially migrated capability schema is
/// a deployment condition, not an empty all-Unknown profile that an operator
/// could accidentally approve for enforce.
async fn ensure_capability_store_available(state: &AdminState) -> Result<(), AdminError> {
    let required_tables = [
        "target_capability_profiles",
        "target_capability_overrides",
        "target_probe_jobs",
        "capability_epoch",
        "installation_secrets",
        "capability_probe_budgets",
        "capability_route_admissions",
        "request_capability_plans",
        "request_capability_feedback",
        "request_capability_telemetry_gaps",
        "capability_probe_runs",
    ];
    for table in required_tables {
        let present = match state.pool.kind() {
            tiygate_store::db::DbKind::Sqlite => {
                sqlx::query("SELECT 1 FROM sqlite_master WHERE type='table' AND name=$1 LIMIT 1")
                    .bind(table)
                    .fetch_optional(state.pool.any())
                    .await
            }
            tiygate_store::db::DbKind::Postgres => {
                sqlx::query(
                    "SELECT 1 FROM information_schema.tables
                 WHERE table_schema = current_schema() AND table_name = $1 LIMIT 1",
                )
                .bind(table)
                .fetch_optional(state.pool.any())
                .await
            }
        };
        match present {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                return Err(AdminError::CapabilityUnavailable(format!(
                    "capability storage migration is missing table {table}"
                )));
            }
        }
    }
    let required_migrations = [
        ("config", 20260829000001_i64),
        ("config", 20260829000002_i64),
        ("config", 20260829000003_i64),
        ("config", 20260829000005_i64),
        ("config", 20260829000006_i64),
        ("config", 20260829000007_i64),
        ("config", 20260829000008_i64),
        ("log", 20260829000001_i64),
        ("log", 20260829000002_i64),
        ("log", 20260829000003_i64),
        ("log", 20260829000004_i64),
    ];
    for (sequence, version) in required_migrations {
        let present: Option<i64> = sqlx::query_scalar(
            "SELECT version FROM _migrations WHERE sequence = $1 AND version = $2 LIMIT 1",
        )
        .bind(sequence)
        .bind(version)
        .fetch_optional(state.pool.any())
        .await
        .map_err(|_| {
            AdminError::CapabilityUnavailable(
                "capability migration bookkeeping is unavailable".to_string(),
            )
        })?;
        if present != Some(version) {
            return Err(AdminError::CapabilityUnavailable(format!(
                "capability migration {sequence}/{version} is not applied"
            )));
        }
    }
    Ok(())
}

async fn list_target_capabilities(
    State(state): State<AdminState>,
    axum::extract::Query(query): axum::extract::Query<CapabilityListQuery>,
) -> Result<Response, AdminError> {
    ensure_capability_store_available(&state).await?;
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let offset = query.offset.unwrap_or(0);
    let profiles = state.store.list_capability_profiles(limit, offset).await?;
    let total = state.store.count_capability_profiles().await?;
    let summaries: Vec<CapabilityProfileSummary> = profiles.iter().map(Into::into).collect();
    let next_cursor = (offset.saturating_add(summaries.len() as u32) < total as u32)
        .then(|| offset.saturating_add(summaries.len() as u32).to_string());
    Ok(Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "next_cursor": next_cursor,
        "items": summaries,
        "entries": summaries
    }))
    .into_response())
}

async fn get_target_capability(
    State(state): State<AdminState>,
    Path(target_key): Path<String>,
) -> Result<Response, AdminError> {
    ensure_capability_store_available(&state).await?;
    let key = tiygate_core::TargetKey(target_key);
    let profile = state
        .store
        .get_capability_profile(&key)
        .await?
        .ok_or_else(|| AdminError::NotFound("target capability profile".to_string()))?;
    let overrides = state
        .store
        .list_capability_overrides(&key)
        .await?
        .into_iter()
        .map(capability_override_view)
        .collect::<Vec<_>>();
    let probe_job = state.store.latest_probe_job_for_target(&key).await?;
    Ok(Json(json!({
        "profile": capability_profile_view(&profile),
        "overrides": overrides,
        "probe_job": probe_job
    }))
    .into_response())
}

async fn list_target_capability_probe_runs(
    State(state): State<AdminState>,
    Path(target_key): Path<String>,
    axum::extract::Query(query): axum::extract::Query<CapabilityListQuery>,
) -> Result<Response, AdminError> {
    ensure_capability_store_available(&state).await?;
    if state
        .store
        .get_capability_profile(&tiygate_core::TargetKey(target_key.clone()))
        .await?
        .is_none()
    {
        return Err(AdminError::NotFound(
            "target capability profile".to_string(),
        ));
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let offset = query.offset.unwrap_or(0);
    let (items, total) = tiygate_store::log_sink::oltp::list_capability_probe_runs(
        state.pool.as_ref(),
        &target_key,
        limit,
        offset,
    )
    .await
    .map_err(AdminError::Db)?;
    let next_cursor = (offset.saturating_add(items.len() as u32) < total as u32)
        .then(|| offset.saturating_add(items.len() as u32).to_string());
    Ok(Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "next_cursor": next_cursor,
        "items": items,
        "entries": items
    }))
    .into_response())
}

/// Build the capability detail payload exposed to Admin callers. TargetKey is
/// an intentionally opaque correlation identifier; credential scope material
/// and canonical API bases are never returned by this endpoint.
fn capability_profile_view(
    profile: &tiygate_store::capabilities::TargetCapabilityProfile,
) -> Value {
    let now = chrono::Utc::now();
    let profile_status = if profile
        .fresh_until
        .is_some_and(|fresh_until| fresh_until <= now)
    {
        tiygate_store::capabilities::ProfileStatus::Stale
    } else {
        profile.profile_status
    };
    let mut resolved_capabilities =
        serde_json::to_value(&profile.resolved_capabilities).unwrap_or_else(|_| json!({}));
    let mut observations =
        serde_json::to_value(&profile.observations).unwrap_or_else(|_| json!([]));
    sanitize_capability_json(&mut resolved_capabilities, 0);
    sanitize_capability_json(&mut observations, 0);
    let mut view = json!({
        "target_key": profile.target_key,
        "identity_version": profile.identity_version,
        "provider_id": profile.provider_id,
        "protocol_suite": profile.protocol_suite,
        "endpoint_name": profile.endpoint_name,
        "endpoint_version": profile.endpoint_version,
        "dialect_id": profile.dialect_id,
        "model_id": profile.model_id,
        "schema_version": profile.schema_version,
        "registry_version": profile.registry_version,
        "baseline_version": profile.baseline_version,
        "profile_status": profile_status,
        "resolved_capabilities": resolved_capabilities,
        "observations": observations,
        "last_probe_suite_version": profile.last_probe_suite_version,
        "last_probe_judge_version": profile.last_probe_judge_version,
        "last_successful_probe_at": profile.last_successful_probe_at,
        "last_probe_error_class": profile.last_probe_error_class,
        "last_probe_error_redacted": profile.last_probe_error_redacted,
        "fresh_until": profile.fresh_until,
        "stale_until": profile.stale_until,
        "created_at": profile.created_at,
        "updated_at": profile.updated_at
    });
    const MAX_PROFILE_VIEW_BYTES: usize = 256 * 1024;
    if serde_json::to_vec(&view)
        .map(|encoded| encoded.len() > MAX_PROFILE_VIEW_BYTES)
        .unwrap_or(true)
    {
        if let Some(object) = view.as_object_mut() {
            object.insert("observations".to_string(), Value::Array(Vec::new()));
            object.insert(
                "resolved_capabilities".to_string(),
                Value::Object(serde_json::Map::new()),
            );
            object.insert("profile_view_truncated".to_string(), Value::Bool(true));
        }
    }
    view
}

fn sanitize_capability_json(value: &mut Value, depth: usize) {
    const MAX_STRING_BYTES: usize = 4096;
    const MAX_ARRAY_ITEMS: usize = 256;
    if depth > 8 {
        *value = json!("[TRUNCATED]");
        return;
    }
    match value {
        Value::Object(object) => {
            for (key, child) in object.iter_mut() {
                let lower = key.to_ascii_lowercase();
                if [
                    "token",
                    "secret",
                    "password",
                    "api_key",
                    "credential",
                    "authorization",
                ]
                .iter()
                .any(|needle| lower.contains(needle))
                {
                    *child = json!("[REDACTED]");
                } else {
                    sanitize_capability_json(child, depth + 1);
                }
            }
        }
        Value::Array(items) => {
            if items.len() > MAX_ARRAY_ITEMS {
                items.truncate(MAX_ARRAY_ITEMS);
            }
            for child in items {
                sanitize_capability_json(child, depth + 1);
            }
        }
        Value::String(text) if text.len() > MAX_STRING_BYTES => {
            let mut end = MAX_STRING_BYTES;
            while !text.is_char_boundary(end) {
                end = end.saturating_sub(1);
            }
            text.truncate(end);
            text.push('…');
        }
        _ => {}
    }
}

fn capability_override_view(
    override_record: tiygate_store::capabilities::TargetCapabilityOverride,
) -> Value {
    let value = override_record.value.and_then(|value| {
        let mut value = serde_json::to_value(value).ok()?;
        sanitize_capability_json(&mut value, 0);
        serde_json::to_vec(&value)
            .ok()
            .filter(|encoded| encoded.len() <= 16 * 1024)
            .map_or_else(|| Some(json!({"truncated": true})), |_| Some(value))
    });
    json!({
        "target_key": override_record.target_key,
        "capability_id": override_record.capability_id,
        "state": override_record.state,
        "value": value,
        "reason": truncate_admin_text(&override_record.reason, 2048),
        "actor": truncate_admin_text(&override_record.actor, 256),
        "expires_at": override_record.expires_at,
        "created_at": override_record.created_at,
        "updated_at": override_record.updated_at
    })
}

fn truncate_admin_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}…", &value[..end])
}

fn validate_capability_setting(key: &str, value: &str) -> Result<(), String> {
    use tiygate_store::settings_keys as sk;
    let trimmed = value.trim();
    if matches!(
        key,
        sk::CAPABILITY_PROBE_ENABLED | sk::RESPONSES_CRL_TOOL_PROMOTION_ENABLED
    ) {
        if !matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "true" | "false" | "1" | "0" | "yes" | "no" | "on" | "off"
        ) {
            return Err(format!("{key} must be a boolean"));
        }
        return Ok(());
    }
    let bounds = match key {
        sk::CAPABILITY_PROBE_DAILY_BUDGET => Some((1_u64, 10_000_u64)),
        sk::CAPABILITY_PROBE_GLOBAL_BUDGET => Some((1_u64, 100_000_u64)),
        sk::CAPABILITY_PROBE_GLOBAL_CONCURRENCY
        | sk::CAPABILITY_PROBE_PROVIDER_CONCURRENCY
        | sk::CAPABILITY_PROBE_ACCOUNT_CONCURRENCY => Some((1_u64, 64_u64)),
        _ => None,
    };
    if let Some((min, max)) = bounds {
        let parsed = trimmed
            .parse::<u64>()
            .map_err(|_| format!("{key} must be an integer"))?;
        if !(min..=max).contains(&parsed) {
            return Err(format!("{key} must be between {min} and {max}"));
        }
    }
    Ok(())
}

fn capability_idempotency_key(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

async fn begin_capability_idempotency(
    state: &AdminState,
    operation: &str,
    headers: &HeaderMap,
    payload: &Value,
) -> Result<Option<(String, String, Option<Response>)>, AdminError> {
    let Some(key) = capability_idempotency_key(headers) else {
        return Ok(None);
    };
    match state
        .store
        .begin_capability_mutation(operation, key, payload)
        .await?
    {
        CapabilityMutationIdempotency::New { request_hash } => {
            Ok(Some((key.to_string(), request_hash, None)))
        }
        CapabilityMutationIdempotency::Replay { status, response } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            Ok(Some((
                key.to_string(),
                String::new(),
                Some((status, Json(response)).into_response()),
            )))
        }
        CapabilityMutationIdempotency::Conflict(message) => {
            Err(AdminError::IdempotencyConflict(message))
        }
    }
}

async fn start_target_capability_probe(
    State(state): State<AdminState>,
    Path(target_key): Path<String>,
    headers: HeaderMap,
    body: Option<Json<CapabilityProbeRequest>>,
) -> Result<Response, AdminError> {
    ensure_capability_store_available(&state).await?;
    let key = tiygate_core::TargetKey(target_key);
    let profile = state
        .store
        .get_capability_profile(&key)
        .await?
        .ok_or_else(|| AdminError::NotFound("target capability profile".to_string()))?;
    let probe_set = body
        .map(|Json(request)| request.probe_set)
        .filter(|set| !set.is_empty())
        .unwrap_or_else(|| tiygate_store::capabilities::manual_probe_set_for_profile(&profile));
    let allowed = tiygate_protocols::capabilities::registry()
        .iter()
        .filter_map(|descriptor| descriptor.probe_id.as_deref())
        .collect::<std::collections::BTreeSet<_>>();
    let applicable = tiygate_store::capabilities::manual_probe_set_for_profile(&profile)
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    if probe_set.len() > 32
        || probe_set
            .iter()
            .any(|probe| !allowed.contains(probe.as_str()) || !applicable.contains(probe))
    {
        return Err(AdminError::InvalidCapability(
            "probe_set contains an unknown or excessive probe id".to_string(),
        ));
    }
    let idempotency_payload = json!({
        "target_key": key,
        "probe_set": probe_set,
    });
    let reservation = begin_capability_idempotency(
        &state,
        "target_capability_probe",
        &headers,
        &idempotency_payload,
    )
    .await?;
    if let Some((_, _, Some(response))) = reservation {
        return Ok(response);
    }
    let reservation = reservation.map(|(key, hash, _)| (key, hash));
    let audit_details = json!({
        "target_key": key,
        "probe_set": probe_set,
        "reason": "admin_manual_probe"
    });
    let job = match state
        .store
        .enqueue_probe_job_with_audit(&key, &probe_set, 10, 3, "admin", &audit_details)
        .await
    {
        Ok(job) => job,
        Err(error) => {
            if let Some((idempotency_key, request_hash)) = reservation.as_ref() {
                let _ = state
                    .store
                    .release_capability_mutation(
                        "target_capability_probe",
                        idempotency_key,
                        request_hash,
                    )
                    .await;
            }
            return Err(AdminError::Store(error));
        }
    };
    if let Some((idempotency_key, request_hash)) = reservation {
        state
            .store
            .complete_capability_mutation(
                "target_capability_probe",
                &idempotency_key,
                &request_hash,
                StatusCode::ACCEPTED.as_u16(),
                &serde_json::to_value(&job).map_err(|error| {
                    AdminError::Internal(format!("serialize probe job response: {error}"))
                })?,
            )
            .await?;
    }
    Ok((StatusCode::ACCEPTED, Json(job)).into_response())
}

async fn upsert_target_capability_override(
    State(state): State<AdminState>,
    Path(target_key): Path<String>,
    headers: HeaderMap,
    Json(request): Json<CapabilityOverrideRequest>,
) -> Result<Response, AdminError> {
    ensure_capability_store_available(&state).await?;
    let key = tiygate_core::TargetKey(target_key);
    let profile = state
        .store
        .get_capability_profile(&key)
        .await?
        .ok_or_else(|| AdminError::NotFound("target capability profile".to_string()))?;
    let baseline =
        tiygate_protocols::capabilities::baseline_for(&tiygate_core::WireProfileId::new(
            profile.protocol_suite.clone(),
            profile.endpoint_name.clone(),
            profile.endpoint_version.clone(),
            profile.dialect_id.clone(),
        ));
    let state_value = match request.state.trim().to_ascii_lowercase().as_str() {
        "supported" => tiygate_core::CapabilityState::Supported,
        "unsupported" => tiygate_core::CapabilityState::Unsupported,
        "constrained" => tiygate_core::CapabilityState::Constrained,
        "unknown" => tiygate_core::CapabilityState::Unknown,
        _ => {
            return Err(AdminError::InvalidCapability(
                "invalid capability state".to_string(),
            ))
        }
    };
    let capability_id = tiygate_core::CapabilityId::from(request.capability_id.clone());
    if capability_id.as_str().is_empty() || capability_id.as_str().len() > 256 {
        return Err(AdminError::InvalidCapability(
            "capability_id must be between 1 and 256 bytes".to_string(),
        ));
    }
    if baseline.get(&capability_id) == Some(&tiygate_core::BaselineSupport::Forbidden)
        && matches!(
            state_value,
            tiygate_core::CapabilityState::Supported | tiygate_core::CapabilityState::Constrained
        )
    {
        return Err(AdminError::InvalidCapability(
            "a protocol-forbidden capability cannot be overridden as supported".to_string(),
        ));
    }
    let value = request
        .value
        .map(serde_json::from_value::<tiygate_core::CapabilityValue>)
        .transpose()
        .map_err(|error| {
            AdminError::InvalidCapability(format!("invalid capability value: {error}"))
        })?;
    if let Some(descriptor) = tiygate_protocols::capabilities::descriptor_for(&capability_id) {
        if let Some(value) = &value {
            if value.kind() != descriptor.value_kind {
                return Err(AdminError::InvalidCapability(format!(
                    "capability value kind does not match {}",
                    descriptor.id
                )));
            }
        }
        if state_value == tiygate_core::CapabilityState::Constrained && value.is_none() {
            return Err(AdminError::InvalidCapability(
                "constrained capability overrides require a value".to_string(),
            ));
        }
        if state_value == tiygate_core::CapabilityState::Constrained
            && value
                .as_ref()
                .is_some_and(tiygate_core::CapabilityValue::is_empty)
        {
            return Err(AdminError::InvalidCapability(
                "constrained capability overrides require a non-empty value".to_string(),
            ));
        }
        let mut observation = tiygate_core::CapabilityObservation::now(
            capability_id.clone(),
            state_value,
            tiygate_core::EvidenceSource::ExplicitOverride,
            1,
        );
        observation.value = value.clone();
        tiygate_core::validate_capability_observation(descriptor, &observation)
            .map_err(AdminError::InvalidCapability)?;
    }
    if request.reason.trim().is_empty() {
        return Err(AdminError::InvalidCapability(
            "override reason is required".to_string(),
        ));
    }
    if request.reason.len() > 2048 {
        return Err(AdminError::InvalidCapability(
            "override reason exceeds 2048 bytes".to_string(),
        ));
    }
    let now = chrono::Utc::now();
    if request
        .expires_at
        .is_some_and(|expires_at| expires_at <= now)
    {
        return Err(AdminError::InvalidCapability(
            "override expires_at must be in the future".to_string(),
        ));
    }
    if request
        .expires_at
        .is_some_and(|expires_at| expires_at > now + chrono::Duration::days(30))
    {
        return Err(AdminError::InvalidCapability(
            "override expires_at may not exceed 30 days".to_string(),
        ));
    }
    // Hash only caller-supplied fields for idempotency.  Server timestamps
    // are intentionally excluded; otherwise a retry with the same key/body
    // would look like a different mutation a few microseconds later.
    let idempotency_payload = json!({
        "target_key": key,
        "capability_id": capability_id,
        "state": state_value,
        "value": value,
        "reason": request.reason,
        "expires_at": request.expires_at,
    });
    let record = TargetCapabilityOverride {
        target_key: key,
        capability_id,
        state: state_value,
        value,
        reason: request.reason,
        actor: "admin".to_string(),
        expires_at: request.expires_at,
        created_at: now,
        updated_at: now,
    };
    let reservation = begin_capability_idempotency(
        &state,
        "target_capability_override",
        &headers,
        &idempotency_payload,
    )
    .await?;
    if let Some((_, _, Some(response))) = reservation {
        return Ok(response);
    }
    let reservation = reservation.map(|(key, hash, _)| (key, hash));
    let audit_details = match serde_json::to_value(&record) {
        Ok(value) => value,
        Err(_) => json!({"redacted": true}),
    };
    if let Err(error) = state
        .store
        .upsert_capability_override_with_audit(
            &record,
            record.capability_id.as_str(),
            &audit_details,
        )
        .await
    {
        if let Some((idempotency_key, request_hash)) = reservation.as_ref() {
            let _ = state
                .store
                .release_capability_mutation(
                    "target_capability_override",
                    idempotency_key,
                    request_hash,
                )
                .await;
        }
        return Err(error.into());
    }
    if let Some((idempotency_key, request_hash)) = reservation {
        state
            .store
            .complete_capability_mutation(
                "target_capability_override",
                &idempotency_key,
                &request_hash,
                StatusCode::OK.as_u16(),
                &serde_json::to_value(&record).map_err(|error| {
                    AdminError::Internal(format!("serialize override response: {error}"))
                })?,
            )
            .await?;
    }
    Ok((StatusCode::OK, Json(record)).into_response())
}

async fn delete_target_capability_override(
    State(state): State<AdminState>,
    Path((target_key, capability_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    ensure_capability_store_available(&state).await?;
    let idempotency_payload = json!({
        "target_key": target_key,
        "capability_id": capability_id
    });
    let reservation = begin_capability_idempotency(
        &state,
        "target_capability_override_delete",
        &headers,
        &idempotency_payload,
    )
    .await?;
    if let Some((_, _, Some(response))) = reservation {
        return Ok(response);
    }
    let reservation = reservation.map(|(key, hash, _)| (key, hash));
    let audit_details = json!({"target_key": target_key});
    let removed = match state
        .store
        .delete_capability_override_with_audit(
            &tiygate_core::TargetKey(target_key.clone()),
            &tiygate_core::CapabilityId::from(capability_id.clone()),
            "admin",
            &audit_details,
        )
        .await
    {
        Ok(removed) => removed,
        Err(error) => {
            if let Some((idempotency_key, request_hash)) = reservation.as_ref() {
                let _ = state
                    .store
                    .release_capability_mutation(
                        "target_capability_override_delete",
                        idempotency_key,
                        request_hash,
                    )
                    .await;
            }
            return Err(AdminError::Store(error));
        }
    };
    if !removed {
        if let Some((idempotency_key, request_hash)) = reservation.as_ref() {
            let _ = state
                .store
                .release_capability_mutation(
                    "target_capability_override_delete",
                    idempotency_key,
                    request_hash,
                )
                .await;
        }
        return Err(AdminError::NotFound(
            "target capability override".to_string(),
        ));
    }
    if let Some((idempotency_key, request_hash)) = reservation {
        state
            .store
            .complete_capability_mutation(
                "target_capability_override_delete",
                &idempotency_key,
                &request_hash,
                StatusCode::NO_CONTENT.as_u16(),
                &json!({}),
            )
            .await?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn get_capability_registry(
    axum::extract::Query(query): axum::extract::Query<CapabilityListQuery>,
) -> Result<Response, AdminError> {
    let registry = tiygate_protocols::capabilities::registry();
    let limit = query.limit.unwrap_or(100).clamp(1, 500) as usize;
    let offset = query.offset.unwrap_or(0) as usize;
    let items = registry
        .iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    let next_cursor = (offset.saturating_add(items.len()) < registry.len())
        .then(|| offset.saturating_add(items.len()).to_string());
    Ok(Json(json!({
        "total": registry.len(),
        "limit": limit,
        "offset": offset,
        "next_cursor": next_cursor,
        "contract_schema_version": 1,
        "contract_summary": tiygate_protocols::capabilities::contract_summary(),
        "items": items,
        "entries": items
    }))
    .into_response())
}

#[derive(Debug, Deserialize)]
struct CapabilityMetricsQuery {
    route_id: Option<String>,
    shape_hash: Option<String>,
    since: Option<String>,
    until: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
}

async fn list_capability_metrics(
    State(state): State<AdminState>,
    axum::extract::Query(query): axum::extract::Query<CapabilityMetricsQuery>,
) -> Result<Response, AdminError> {
    ensure_capability_store_available(&state).await?;
    for (name, value) in [
        ("since", query.since.as_deref()),
        ("until", query.until.as_deref()),
    ] {
        if value.is_some_and(|value| chrono::DateTime::parse_from_rfc3339(value).is_err()) {
            return Err(AdminError::BadRequest(format!(
                "{name} must be an RFC3339 timestamp"
            )));
        }
    }
    let metrics = tiygate_store::log_sink::oltp::list_capability_shadow_metrics(
        state.pool.as_ref(),
        query.route_id.as_deref(),
        query.shape_hash.as_deref(),
        query.since.as_deref(),
        query.until.as_deref(),
    )
    .await
    .map_err(AdminError::Db)?;
    let total = metrics.len() as u64;
    let limit = query.limit.unwrap_or(100).clamp(1, 500) as usize;
    let offset = query.offset.unwrap_or(0) as usize;
    let items = metrics
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let next_cursor =
        (offset + items.len() < total as usize).then(|| (offset + items.len()).to_string());
    Ok(Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "next_cursor": next_cursor,
        "items": items,
        "entries": items
    }))
    .into_response())
}

#[derive(Debug, Deserialize)]
struct CapabilityProbeWorkerRequest {
    enabled: bool,
    reason: String,
}

async fn update_capability_probe_worker(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(request): Json<CapabilityProbeWorkerRequest>,
) -> Result<Response, AdminError> {
    ensure_capability_store_available(&state).await?;
    if request.reason.trim().is_empty() {
        return Err(AdminError::BadRequest(
            "probe worker reason is required".to_string(),
        ));
    }
    if request.reason.len() > 2048 {
        return Err(AdminError::BadRequest(
            "probe worker reason exceeds 2048 bytes".to_string(),
        ));
    }
    let idempotency_payload = json!({
        "enabled": request.enabled,
        "reason": request.reason
    });
    let reservation = begin_capability_idempotency(
        &state,
        "capability_probe_worker",
        &headers,
        &idempotency_payload,
    )
    .await?;
    if let Some((_, _, Some(response))) = reservation {
        return Ok(response);
    }
    let reservation = reservation.map(|(key, hash, _)| (key, hash));
    let key = tiygate_store::settings_keys::CAPABILITY_PROBE_ENABLED;
    let before = state.store.get_setting(key).await?;
    let details = json!({
        "before": before,
        "after": request.enabled,
        "reason": request.reason
    });
    if let Err(error) = state
        .store
        .set_settings_batch_with_audit(
            &[(
                key.to_string(),
                if request.enabled { "true" } else { "false" }.to_string(),
                false,
            )],
            "admin",
            "capability_probe_worker",
            &details,
        )
        .await
    {
        if let Some((idempotency_key, request_hash)) = reservation.as_ref() {
            let _ = state
                .store
                .release_capability_mutation(
                    "capability_probe_worker",
                    idempotency_key,
                    request_hash,
                )
                .await;
        }
        return Err(AdminError::Store(error));
    }
    if let Some((idempotency_key, request_hash)) = reservation {
        state
            .store
            .complete_capability_mutation(
                "capability_probe_worker",
                &idempotency_key,
                &request_hash,
                StatusCode::OK.as_u16(),
                &json!({"enabled": request.enabled}),
            )
            .await?;
    }
    Ok(Json(json!({"enabled": request.enabled})).into_response())
}

async fn get_probe_job(
    State(state): State<AdminState>,
    Path(job_id): Path<String>,
) -> Result<Response, AdminError> {
    ensure_capability_store_available(&state).await?;
    let job = state
        .store
        .get_probe_job(&job_id)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("probe job {job_id}")))?;
    Ok(Json(job).into_response())
}

async fn list_capability_route_admissions(
    State(state): State<AdminState>,
    Path(route_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<CapabilityListQuery>,
) -> Result<Response, AdminError> {
    ensure_capability_store_available(&state).await?;
    if state.store.get_route(&route_id).await?.is_none() {
        return Err(AdminError::NotFound(format!("route {route_id}")));
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 500);
    let offset = query.offset.unwrap_or(0);
    let entries = state
        .store
        .list_capability_route_admissions(&route_id, limit, offset)
        .await?;
    let total = state
        .store
        .count_capability_route_admissions(&route_id)
        .await?;
    let next_cursor = (offset.saturating_add(entries.len() as u32) < total as u32)
        .then(|| offset.saturating_add(entries.len() as u32).to_string());
    Ok(Json(json!({
        "route_id": route_id,
        "total": total,
        "limit": limit,
        "offset": offset,
        "next_cursor": next_cursor,
        "items": entries,
        "entries": entries
    }))
    .into_response())
}

async fn upsert_capability_route_admission(
    State(state): State<AdminState>,
    Path(route_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<CapabilityAdmissionRequest>,
) -> Result<Response, AdminError> {
    ensure_capability_store_available(&state).await?;
    let route = state
        .store
        .get_route(&route_id)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("route {route_id}")))?;
    let (required_ids, required_requirements) = normalize_admission_requirements(
        &request.required_capabilities,
        &request.required_requirements,
    )?;
    if request.reason.trim().is_empty() {
        return Err(AdminError::InvalidCapability(
            "admission reason is required".to_string(),
        ));
    }
    if request.reason.len() > 2048 {
        return Err(AdminError::InvalidCapability(
            "admission reason exceeds 2048 bytes".to_string(),
        ));
    }
    if request.mode == tiygate_core::CapabilityRoutingMode::Off {
        return Err(AdminError::InvalidCapability(
            "shape admission mode must be shadow or enforce".to_string(),
        ));
    }
    let now = chrono::Utc::now();
    if request
        .expires_at
        .is_some_and(|expires_at| expires_at <= now)
    {
        return Err(AdminError::InvalidCapability(
            "admission expires_at must be in the future".to_string(),
        ));
    }
    if request
        .expires_at
        .is_some_and(|expires_at| expires_at > now + chrono::Duration::days(30))
    {
        return Err(AdminError::InvalidCapability(
            "admission expires_at may not exceed 30 days".to_string(),
        ));
    }
    let expected_shape_hash =
        tiygate_core::capability_shape_hash_from_requirements(&required_requirements);
    let before_admission = state
        .store
        .get_capability_route_admission(route_id.as_str(), &expected_shape_hash)
        .await?;
    if request
        .shape_hash
        .as_deref()
        .is_some_and(|shape_hash| !shape_hash.is_empty() && shape_hash != expected_shape_hash)
    {
        return Err(AdminError::InvalidCapability(
            "shape_hash does not match required_requirements".to_string(),
        ));
    }
    for id in &required_ids {
        let Some(descriptor) = tiygate_protocols::capabilities::descriptor_for(id) else {
            return Err(AdminError::InvalidCapability(format!(
                "unknown capability {}",
                id
            )));
        };
        for requirement in required_requirements
            .iter()
            .filter(|requirement| &requirement.id == id)
        {
            if let Some(value) = requirement.value.as_ref() {
                if value.kind() != descriptor.value_kind {
                    return Err(AdminError::InvalidCapability(format!(
                        "capability {} requirement value kind {:?} does not match {:?}",
                        id,
                        value.kind(),
                        descriptor.value_kind
                    )));
                }
                if descriptor.matcher == tiygate_core::CapabilityMatcher::Boolean
                    && !matches!(value, tiygate_core::CapabilityValue::Bool(_))
                {
                    return Err(AdminError::InvalidCapability(format!(
                        "capability {} requires a boolean value",
                        id
                    )));
                }
            }
        }
        if descriptor.routing_eligibility == tiygate_core::RoutingEligibility::Disabled {
            return Err(AdminError::InvalidCapability(format!(
                "capability {} is not eligible for routing",
                id
            )));
        }
        if request.mode == tiygate_core::CapabilityRoutingMode::Enforce
            && (descriptor.routing_eligibility != tiygate_core::RoutingEligibility::EnforceEligible
                || !tiygate_protocols::capabilities::enforce_eligible_ids().contains(&id.as_str()))
        {
            return Err(AdminError::InvalidCapability(format!(
                "capability {} is not eligible for enforce",
                id
            )));
        }
    }

    let mut report = build_shape_admission_report(&state, &route, &required_requirements).await?;
    let gate_passed = report
        .get("gate_passed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let low_traffic_eligible = report
        .get("low_traffic_eligible")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if request.mode == tiygate_core::CapabilityRoutingMode::Enforce
        && !gate_passed
        && (!request.low_traffic_exception || !low_traffic_eligible)
    {
        return Err(AdminError::AdmissionRequired(
            "capability shape has not passed the admission gate; a low_traffic_exception requires an eligible CRL probe/continuation report"
                .to_string(),
        ));
    }
    if let Some(object) = report.as_object_mut() {
        object.insert(
            "low_traffic_exception".to_string(),
            Value::Bool(request.low_traffic_exception),
        );
        if request.low_traffic_exception {
            object.insert("gate_passed_by_exception".to_string(), Value::Bool(true));
        }
    }
    let expires_at = request.expires_at.or_else(|| {
        (request.mode == tiygate_core::CapabilityRoutingMode::Enforce)
            .then_some(now + chrono::Duration::days(7))
    });
    let admission = CapabilityRouteAdmission {
        route_id: route_id.clone(),
        capability_shape_hash: expected_shape_hash,
        required_capabilities: required_ids,
        required_requirements: required_requirements.clone(),
        mode: request.mode,
        gate_policy_version: 1,
        report,
        approved_by: (request.mode == tiygate_core::CapabilityRoutingMode::Enforce)
            .then(|| "admin".to_string()),
        approved_at: (request.mode == tiygate_core::CapabilityRoutingMode::Enforce).then_some(now),
        expires_at,
        revision: 0,
        created_at: now,
        updated_at: now,
    };
    let idempotency_payload = json!({
        "route_id": route_id,
        "shape_hash": admission.capability_shape_hash,
        "required_capabilities": admission.required_capabilities,
        "required_requirements": admission.required_requirements,
        "mode": admission.mode,
        "expected_revision": request.expected_revision,
        "expires_at": admission.expires_at,
        "low_traffic_exception": request.low_traffic_exception,
        "reason": request.reason
    });
    let reservation = begin_capability_idempotency(
        &state,
        "capability_route_admission",
        &headers,
        &idempotency_payload,
    )
    .await?;
    if let Some((_, _, Some(response))) = reservation {
        return Ok(response);
    }
    let reservation = reservation.map(|(key, hash, _)| (key, hash));
    let admission_audit_details = json!({
        "diff": audit_details(
            before_admission
                .as_ref()
                .and_then(|value| serde_json::to_value(value).ok())
                .as_ref(),
            serde_json::to_value(&admission).ok().as_ref(),
        ),
        "reason": request.reason,
        "low_traffic_exception": request.low_traffic_exception,
    });
    let saved = match state
        .store
        .upsert_capability_route_admission_with_audit(
            &admission,
            request.expected_revision,
            "admin",
            if request.mode == tiygate_core::CapabilityRoutingMode::Enforce {
                "enforce_approval"
            } else {
                "upsert"
            },
            &format!("{route_id}:{}", admission.capability_shape_hash),
            &admission_audit_details,
        )
        .await
    {
        Ok(saved) => saved,
        Err(error) => {
            if let Some((idempotency_key, request_hash)) = reservation.as_ref() {
                let _ = state
                    .store
                    .release_capability_mutation(
                        "capability_route_admission",
                        idempotency_key,
                        request_hash,
                    )
                    .await;
            }
            return Err(match error {
                StoreError::Invalid(message) if message.contains("revision") => {
                    AdminError::Conflict(message)
                }
                other => AdminError::Store(other),
            });
        }
    };
    if let Some((idempotency_key, request_hash)) = reservation {
        state
            .store
            .complete_capability_mutation(
                "capability_route_admission",
                &idempotency_key,
                &request_hash,
                StatusCode::OK.as_u16(),
                &serde_json::to_value(&saved).map_err(|error| {
                    AdminError::Internal(format!("serialize admission response: {error}"))
                })?,
            )
            .await?;
    }
    Ok(Json(saved).into_response())
}

async fn delete_capability_route_admission(
    State(state): State<AdminState>,
    Path((route_id, shape_hash)): Path<(String, String)>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<CapabilityRevisionQuery>,
) -> Result<Response, AdminError> {
    ensure_capability_store_available(&state).await?;
    let idempotency_payload = json!({
        "route_id": route_id,
        "shape_hash": shape_hash,
        "expected_revision": query.expected_revision
    });
    let reservation = begin_capability_idempotency(
        &state,
        "capability_route_admission_delete",
        &headers,
        &idempotency_payload,
    )
    .await?;
    if let Some((_, _, Some(response))) = reservation {
        return Ok(response);
    }
    let reservation = reservation.map(|(key, hash, _)| (key, hash));
    let audit_details = json!({"route_id": route_id, "shape_hash": shape_hash});
    let removed = match state
        .store
        .delete_capability_route_admission_with_audit(
            &route_id,
            &shape_hash,
            query.expected_revision,
            "admin",
            &audit_details,
        )
        .await
    {
        Ok(removed) => removed,
        Err(error) => {
            if let Some((idempotency_key, request_hash)) = reservation.as_ref() {
                let _ = state
                    .store
                    .release_capability_mutation(
                        "capability_route_admission_delete",
                        idempotency_key,
                        request_hash,
                    )
                    .await;
            }
            return Err(AdminError::Store(error));
        }
    };
    if !removed {
        if let Some((idempotency_key, request_hash)) = reservation.as_ref() {
            let _ = state
                .store
                .release_capability_mutation(
                    "capability_route_admission_delete",
                    idempotency_key,
                    request_hash,
                )
                .await;
        }
        return Err(AdminError::NotFound(
            "capability route admission".to_string(),
        ));
    }
    if let Some((idempotency_key, request_hash)) = reservation {
        state
            .store
            .complete_capability_mutation(
                "capability_route_admission_delete",
                &idempotency_key,
                &request_hash,
                StatusCode::NO_CONTENT.as_u16(),
                &json!({}),
            )
            .await?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn build_shape_admission_report(
    state: &AdminState,
    route: &Route,
    required_requirements: &[tiygate_core::CapabilityRequirement],
) -> Result<Value, AdminError> {
    let promotion_enabled = tiygate_store::settings_keys::get_bool(
        state.store.as_ref(),
        tiygate_store::settings_keys::RESPONSES_CRL_TOOL_PROMOTION_ENABLED,
        false,
    )
    .await;
    let runtime = state.store.config_store();
    let targets = runtime
        .routing_table
        .resolve(&route.virtual_model)
        .unwrap_or_default();
    let required = required_requirements
        .iter()
        .map(|requirement| requirement.id.clone())
        .collect::<Vec<_>>();
    let expression = tiygate_core::RequirementExpr::all(
        required_requirements
            .iter()
            .cloned()
            .map(tiygate_core::RequirementExpr::Capability)
            .collect::<Vec<_>>(),
    );
    let mut target_reports = Vec::new();
    let mut resolved_pairs = 0usize;
    let total_pairs = targets.len().saturating_mul(required.len());
    let mut compatible_targets = 0usize;
    let mut probe_error_count = 0usize;
    let mut auth_error_count = 0usize;
    let mut continuation_verified_targets = 0usize;
    let now = chrono::Utc::now();
    for target in targets {
        let (key, _) = state.store.target_key_for(&target)?;
        let profile = state.store.get_capability_profile(&key).await?;
        let (status, missing, unknown) = if let Some(profile) = profile {
            if profile.last_probe_error_class.is_some() {
                probe_error_count += 1;
                if profile.last_probe_error_class.as_deref() == Some("auth") {
                    auth_error_count += 1;
                }
            }
            // Re-apply current overrides instead of trusting the
            // denormalized resolved JSON alone. Overrides are a separate
            // evidence source and may have been written after the last
            // probe result.
            let overrides = state.store.list_capability_overrides(&key).await?;
            let has_active_override = overrides
                .iter()
                .any(|record| record.expires_at.is_none_or(|expires_at| expires_at > now));
            let fresh_valid = profile
                .fresh_until
                .is_some_and(|fresh_until| fresh_until > now);
            let stale_grace_valid = profile
                .stale_until
                .is_some_and(|stale_until| stale_until > now);
            let profile_versions_valid = profile.schema_version
                == tiygate_store::capabilities::CAPABILITY_SCHEMA_VERSION
                && profile.identity_version == 1
                && profile.registry_version
                    == tiygate_store::capabilities::CAPABILITY_REGISTRY_VERSION
                && profile.baseline_version
                    == tiygate_store::capabilities::CAPABILITY_BASELINE_VERSION
                && profile.last_probe_suite_version
                    == Some(tiygate_store::capabilities::PROBE_SUITE_VERSION)
                && profile.last_probe_judge_version
                    == Some(tiygate_store::capabilities::PROBE_JUDGE_VERSION);
            if (!profile_versions_valid || (!fresh_valid && !stale_grace_valid))
                && !has_active_override
            {
                ("stale", required.clone(), Vec::new())
            } else {
                let mut observations = if profile_versions_valid {
                    profile.observations.clone()
                } else {
                    // Future-version probe/registry observations are retained
                    // for diagnostics but cannot participate in a current
                    // admission. Explicit overrides are reapplied below.
                    Vec::new()
                };
                if stale_grace_valid && !fresh_valid {
                    for observation in &mut observations {
                        if observation
                            .expires_at
                            .is_some_and(|expires_at| expires_at <= now)
                        {
                            observation.expires_at = None;
                        }
                    }
                }
                for override_record in overrides {
                    if override_record
                        .expires_at
                        .is_some_and(|expires_at| expires_at <= now)
                    {
                        continue;
                    }
                    let mut observation = tiygate_core::CapabilityObservation::now(
                        override_record.capability_id,
                        override_record.state,
                        tiygate_core::EvidenceSource::ExplicitOverride,
                        1,
                    );
                    observation.value = override_record.value;
                    observation.expires_at = override_record.expires_at;
                    observations.push(observation);
                }
                let baseline = tiygate_protocols::capabilities::baseline_for(
                    &tiygate_core::WireProfileId::new(
                        profile.protocol_suite.clone(),
                        profile.endpoint_name.clone(),
                        profile.endpoint_version.clone(),
                        profile.dialect_id.clone(),
                    ),
                );
                let resolved = tiygate_core::resolve_capabilities_with_matchers(
                    &baseline,
                    &tiygate_protocols::capabilities::matcher_map(),
                    observations,
                    now,
                );
                let report = admission_compatibility_report(
                    &resolved,
                    &expression,
                    required_requirements,
                    &target,
                    promotion_enabled,
                );
                if resolved
                    .get(&tiygate_core::CapabilityId::from(
                        "tools.function.continuation",
                    ))
                    .state
                    == tiygate_core::CapabilityState::Supported
                    && resolved
                        .get(&tiygate_core::CapabilityId::from(
                            "tools.function.continuation",
                        ))
                        .observation
                        .as_ref()
                        .is_some_and(|observation| {
                            matches!(
                                observation.source,
                                tiygate_core::EvidenceSource::SemanticProbe
                                    | tiygate_core::EvidenceSource::SuccessfulTraffic
                            )
                        })
                {
                    continuation_verified_targets = continuation_verified_targets.saturating_add(1);
                }
                let crl_promotable = required.iter().any(|id| {
                    id.as_str() == "tools.crl.additional_tools"
                        && resolved.get(id).state == tiygate_core::CapabilityState::Unknown
                        && promotion_enabled
                        && target.api_protocol.suite == tiygate_core::ProtocolSuite::OpenAiResponses
                        && report.compatible
                });
                let resolved = required
                    .iter()
                    .filter(|id| resolved.get(id).state != tiygate_core::CapabilityState::Unknown)
                    .count()
                    + usize::from(crl_promotable);
                resolved_pairs = resolved_pairs.saturating_add(resolved);
                (
                    if report.compatible {
                        "compatible"
                    } else {
                        "incompatible"
                    },
                    report.missing,
                    report.unknown,
                )
            }
        } else {
            ("unknown", Vec::new(), required.clone())
        };
        if status == "compatible" {
            compatible_targets += 1;
        }
        target_reports.push(json!({
            "target_key": key,
            "status": status,
            "missing": missing.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "unknown": unknown.iter().map(ToString::to_string).collect::<Vec<_>>()
        }));
    }
    let profile_coverage = if total_pairs == 0 {
        0.0
    } else {
        resolved_pairs as f64 / total_pairs as f64
    };
    let shape_hash = tiygate_core::capability_shape_hash_from_requirements(required_requirements);
    let persisted_metrics = tiygate_store::log_sink::oltp::list_capability_shadow_metrics(
        state.pool.as_ref(),
        Some(&route.id),
        Some(&shape_hash),
        None,
        None,
    )
    .await
    .map_err(AdminError::Db)?;
    let metric = persisted_metrics.first();
    let traffic_sample_count = metric.map_or(0, |value| value.relevant_requests);
    let compatible_shape_coverage = metric.map_or(0.0, |value| value.compatible_shape_coverage);
    let planner_unknown_rate = metric.map_or(0.0, |value| value.planner_unknown_rate);
    let verified_success_disagreements =
        metric.map_or(0, |value| value.verified_success_disagreements);
    let verified_success_disagreement_rate =
        metric.map_or(0.0, |value| value.verified_success_disagreement_rate);
    let probe_terminal_error_rate = metric.map_or_else(
        || {
            if target_reports.is_empty() {
                0.0
            } else {
                probe_error_count as f64 / target_reports.len() as f64
            }
        },
        |value| {
            value
                .probe_terminal_error_rate
                .max(if target_reports.is_empty() {
                    0.0
                } else {
                    probe_error_count as f64 / target_reports.len() as f64
                })
        },
    );
    let planner_internal_error_rate = metric.map_or(0.0, |value| value.planner_internal_error_rate);
    let telemetry_gap = metric.is_some_and(|value| value.telemetry_gap);
    let observation_window_seconds = metric.map_or(0, |value| value.observation_window_seconds);
    let observation_window_complete = metric.is_some_and(|value| {
        value.observation_window_complete && observation_window_seconds >= 24 * 60 * 60
    });
    let planning_latency_p95_micros = metric.map_or(0, |value| value.planning_latency_p95_micros);
    let metric_truncated = metric.is_some_and(|value| value.truncated);
    let low_traffic_eligible = required
        .iter()
        .any(|id| id.as_str() == "tools.crl.additional_tools")
        && profile_coverage == 1.0
        && compatible_targets > 0
        && continuation_verified_targets > 0
        && planner_unknown_rate == 0.0
        && planner_internal_error_rate == 0.0
        && !telemetry_gap
        && auth_error_count == 0;
    // The default admission gate follows §9.5: a complete profile, one
    // compatible target, zero unknown planner decisions, and at least 100
    // relevant requests in the observation window. Low-traffic exceptions
    // are explicitly audited by the caller.
    let crl_shape_requires_continuation = required
        .iter()
        .any(|id| id.as_str() == "tools.crl.additional_tools");
    let gate_passed = profile_coverage == 1.0
        && compatible_targets > 0
        && (!crl_shape_requires_continuation || continuation_verified_targets > 0)
        && traffic_sample_count >= 100
        && compatible_shape_coverage == 1.0
        && planner_unknown_rate == 0.0
        && verified_success_disagreements == 0
        && verified_success_disagreement_rate == 0.0
        && probe_terminal_error_rate <= 0.05
        && planner_internal_error_rate == 0.0
        && !telemetry_gap
        && observation_window_complete
        && metric.is_some_and(|value| value.minimum_sample_met)
        && !metric_truncated
        && planning_latency_p95_micros <= 1_000
        && auth_error_count == 0;
    Ok(json!({
        "gate_policy_version": 1,
        "registry_version": tiygate_store::capabilities::CAPABILITY_REGISTRY_VERSION,
        "baseline_version": tiygate_store::capabilities::CAPABILITY_BASELINE_VERSION,
        "shape_hash_version": tiygate_core::CAPABILITY_SHAPE_HASH_VERSION,
        "route_id": route.id,
        "required_capabilities": required.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "target_count": target_reports.len(),
        "compatible_target_count": compatible_targets,
        "profile_resolution_coverage": profile_coverage,
        "compatible_shape_coverage": compatible_shape_coverage,
        "traffic_sample_count": traffic_sample_count,
        "planner_unknown_rate": planner_unknown_rate,
        "verified_success_disagreements": verified_success_disagreements,
        "verified_success_disagreement_rate": verified_success_disagreement_rate,
        "probe_terminal_error_rate": probe_terminal_error_rate,
        "probe_terminal_errors": metric.map_or(0, |value| value.probe_terminal_errors),
        "probe_auth_errors": metric.map_or(auth_error_count as u64, |value| value.probe_auth_errors),
        "planner_internal_errors": metric.map_or(0, |value| value.planner_internal_errors),
        "planner_internal_error_rate": planner_internal_error_rate,
        "has_samples": metric.is_some_and(|value| value.has_samples),
        "minimum_sample_met": metric.is_some_and(|value| value.minimum_sample_met),
        "telemetry_gap": telemetry_gap,
        "probe_auth_error_count": auth_error_count,
        "planning_latency_p95_micros": planning_latency_p95_micros,
        "observation_window_seconds": observation_window_seconds,
        "observation_window_complete": observation_window_complete,
        "metric_truncated": metric_truncated,
        "continuation_verified_target_count": continuation_verified_targets,
        "low_traffic_eligible": low_traffic_eligible,
        "gate_passed": gate_passed,
        "targets": target_reports
    }))
}

fn admission_compatibility_report(
    capabilities: &tiygate_core::ResolvedTargetCapabilities,
    expression: &tiygate_core::RequirementExpr,
    required_requirements: &[tiygate_core::CapabilityRequirement],
    target: &tiygate_core::RoutingTarget,
    promotion_enabled: bool,
) -> tiygate_core::CompatibilityReport {
    let native = tiygate_core::compatibility_report(capabilities, expression);
    let crl_id = tiygate_core::CapabilityId::from("tools.crl.additional_tools");
    let required_ids = required_requirements
        .iter()
        .map(|requirement| requirement.id.clone())
        .collect::<Vec<_>>();
    if !promotion_enabled
        || !required_ids.contains(&crl_id)
        || target.api_protocol.suite != tiygate_core::ProtocolSuite::OpenAiResponses
        || !matches!(
            target.effective_egress_dialect_id(),
            "auto" | "openai-responses-standard" | "openai-responses-codex-lite"
        )
        || capabilities.satisfies(&tiygate_core::RequirementExpr::required(crl_id.clone()))
    {
        return native;
    }
    // A standard Responses target may satisfy a CRL shape through the same
    // promotion transform used by the request planner. Remove only the CRL
    // carrier requirement and require every concrete nested tool capability.
    let promotion = tiygate_core::RequirementExpr::all(
        required_requirements
            .iter()
            .filter(|requirement| requirement.id != crl_id)
            .cloned()
            .map(tiygate_core::RequirementExpr::Capability)
            .collect::<Vec<_>>(),
    );
    let promotion_report = tiygate_core::compatibility_report(capabilities, &promotion);
    if promotion_report.compatible {
        promotion_report
    } else {
        native
    }
}

// ---- provider model discovery ----

#[derive(Debug, Serialize)]
struct ProviderModelEntry {
    id: String,
}

#[derive(Debug, Serialize)]
struct ProviderModelsResponse {
    models: Vec<ProviderModelEntry>,
}

#[derive(Debug, Clone, Serialize)]
struct ProviderUsageWindow {
    label: Option<String>,
    used_percent: Option<f64>,
    reset_at: Option<i64>,
    limit_window_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct ProviderResetCredit {
    expires_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ProviderResetCredits {
    available_count: usize,
    credits: Vec<ProviderResetCredit>,
}

#[derive(Debug, Deserialize)]
struct ProviderResetCreditsConsumeRequest {
    redeem_request_id: String,
}

#[derive(Debug, Clone, Serialize)]
struct ProviderUsageResponse {
    provider_id: String,
    state: String,
    reason: Option<String>,
    checked_at: Option<String>,
    windows: Vec<ProviderUsageWindow>,
    five_hour: Option<ProviderUsageWindow>,
    seven_day: Option<ProviderUsageWindow>,
    account_email: Option<String>,
    plan_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reset_credits: Option<ProviderResetCredits>,
}

#[derive(Debug, Clone, Serialize)]
struct ProviderResetCreditsConsumeResponse {
    provider_id: String,
    code: String,
    windows_reset: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsageResponse {
    plan_type: Option<String>,
    rate_limit: Option<OpenAiRateLimit>,
}

#[derive(Debug, Clone)]
struct ParsedProviderUsage {
    plan_type: Option<String>,
    windows: Vec<ProviderUsageWindow>,
}

#[derive(Debug, Deserialize)]
struct OpenAiRateLimit {
    primary_window: Option<OpenAiUsageWindow>,
    secondary_window: Option<OpenAiUsageWindow>,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsageWindow {
    used_percent: Option<f64>,
    reset_at: Option<i64>,
    reset_after_seconds: Option<i64>,
    limit_window_seconds: Option<i64>,
}

const FIVE_HOURS_SECONDS: i64 = 5 * 60 * 60;
const SEVEN_DAYS_SECONDS: i64 = 7 * 24 * 60 * 60;

fn provider_usage_response(
    provider_id: &str,
    state: &str,
    reason: Option<&str>,
    windows: Vec<ProviderUsageWindow>,
    account_email: Option<&str>,
) -> ProviderUsageResponse {
    let durations_are_unknown = windows
        .iter()
        .all(|window| window.limit_window_seconds.is_none());
    let five_hour = windows
        .iter()
        .find(|window| {
            window.limit_window_seconds == Some(FIVE_HOURS_SECONDS) && window.label.is_none()
        })
        .or_else(|| {
            windows
                .iter()
                .find(|window| window.limit_window_seconds == Some(FIVE_HOURS_SECONDS))
        })
        .cloned()
        .or_else(|| {
            durations_are_unknown
                .then(|| windows.first().cloned())
                .flatten()
        });
    let seven_day = windows
        .iter()
        .find(|window| {
            window.limit_window_seconds == Some(SEVEN_DAYS_SECONDS) && window.label.is_none()
        })
        .or_else(|| {
            windows
                .iter()
                .find(|window| window.limit_window_seconds == Some(SEVEN_DAYS_SECONDS))
        })
        .cloned()
        .or_else(|| {
            durations_are_unknown
                .then(|| windows.get(1).cloned())
                .flatten()
        });
    ProviderUsageResponse {
        provider_id: provider_id.to_string(),
        state: state.to_string(),
        reason: reason.map(str::to_string),
        checked_at: Some(chrono::Utc::now().to_rfc3339()),
        windows,
        five_hour,
        seven_day,
        account_email: account_email.map(str::to_string),
        plan_type: None,
        reset_credits: None,
    }
}

fn map_openai_usage_window(
    window: Option<OpenAiUsageWindow>,
    now_unix: i64,
) -> Option<ProviderUsageWindow> {
    window.and_then(|window| {
        let used_percent = window.used_percent.map(|value| value.clamp(0.0, 100.0));
        let reset_at = window.reset_at.or_else(|| {
            window
                .reset_after_seconds
                .map(|seconds| now_unix.saturating_add(seconds))
        });
        let limit_window_seconds = window.limit_window_seconds.filter(|seconds| *seconds > 0);
        (used_percent.is_some() || reset_at.is_some() || limit_window_seconds.is_some()).then_some(
            ProviderUsageWindow {
                label: None,
                used_percent,
                reset_at,
                limit_window_seconds,
            },
        )
    })
}

fn parse_openai_usage(body: &str, now_unix: i64) -> Result<ParsedProviderUsage, String> {
    let response: OpenAiUsageResponse =
        serde_json::from_str(body).map_err(|error| format!("invalid usage response: {error}"))?;
    let plan_type = response
        .plan_type
        .filter(|plan_type| !plan_type.trim().is_empty());
    let Some(rate_limit) = response.rate_limit else {
        return Err("usage response has no rate_limit".to_string());
    };
    let windows = [rate_limit.primary_window, rate_limit.secondary_window]
        .into_iter()
        .filter_map(|window| map_openai_usage_window(window, now_unix))
        .collect();
    Ok(ParsedProviderUsage { plan_type, windows })
}

fn parse_non_negative_count(value: Option<&Value>) -> Option<usize> {
    let value = value?;
    if let Some(count) = value.as_u64() {
        return usize::try_from(count).ok();
    }
    value
        .as_str()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
}

fn parse_reset_credit_expiration(value: &Value) -> Option<String> {
    let object = value.as_object()?;
    object
        .get("expires_at")
        .or_else(|| object.get("expiresAt"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn parse_reset_credit_list(value: &Value) -> Vec<ProviderResetCredit> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let object = item.as_object()?;
            let status = object
                .get("status")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|status| !status.is_empty());
            if status.is_some_and(|status| !status.eq_ignore_ascii_case("available")) {
                return None;
            }
            let reset_type = object
                .get("reset_type")
                .or_else(|| object.get("resetType"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|reset_type| !reset_type.is_empty());
            if reset_type
                .is_some_and(|reset_type| !reset_type.eq_ignore_ascii_case("codex_rate_limits"))
            {
                return None;
            }
            Some(ProviderResetCredit {
                expires_at: parse_reset_credit_expiration(item),
            })
        })
        .collect()
}

fn parse_reset_credits(value: &Value) -> Option<ProviderResetCredits> {
    if value.is_array() {
        let credits = parse_reset_credit_list(value);
        return Some(ProviderResetCredits {
            available_count: credits.len(),
            credits,
        });
    }

    let object = value.as_object()?;
    let available_count = object
        .get("available_count")
        .or_else(|| object.get("availableCount"))
        .and_then(|value| parse_non_negative_count(Some(value)));
    let credit_payload = ["credits", "items", "data"]
        .into_iter()
        .find_map(|key| object.get(key));
    if let Some(credit_payload) = credit_payload {
        let credits = parse_reset_credit_list(credit_payload);
        return Some(ProviderResetCredits {
            available_count: available_count.unwrap_or(credits.len()),
            credits,
        });
    }

    for key in ["rate_limit_reset_credits", "rateLimitResetCredits"] {
        if let Some(nested) = object.get(key) {
            if let Some(parsed) = parse_reset_credits(nested) {
                return Some(parsed);
            }
        }
    }

    available_count.map(|available_count| ProviderResetCredits {
        available_count,
        credits: Vec::new(),
    })
}

fn parse_usage_reset_at(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| {
            value
                .as_u64()
                .and_then(|timestamp| i64::try_from(timestamp).ok())
        })
        .or_else(|| {
            value.as_str().and_then(|raw| {
                raw.parse::<i64>().ok().or_else(|| {
                    chrono::DateTime::parse_from_rfc3339(raw)
                        .ok()
                        .map(|timestamp| timestamp.timestamp())
                })
            })
        })
}

fn map_anthropic_usage_window(
    value: Option<&Value>,
    label: Option<String>,
    default_window_seconds: i64,
) -> Option<ProviderUsageWindow> {
    let object = value?.as_object()?;
    let used_percent = object
        .get("utilization")
        .or_else(|| object.get("used_percent"))
        .or_else(|| object.get("used_percentage"))
        .and_then(Value::as_f64)
        .map(|value| value.clamp(0.0, 100.0))?;
    let reset_at = parse_usage_reset_at(object.get("resets_at").or_else(|| object.get("reset_at")));
    let limit_window_seconds = object
        .get("limit_window_seconds")
        .and_then(Value::as_i64)
        .filter(|seconds| *seconds > 0)
        .or(Some(default_window_seconds));
    Some(ProviderUsageWindow {
        label,
        used_percent: Some(used_percent),
        reset_at,
        limit_window_seconds,
    })
}

fn push_unique_usage_window(
    windows: &mut Vec<ProviderUsageWindow>,
    window: Option<ProviderUsageWindow>,
) {
    let Some(window) = window else {
        return;
    };
    if windows.iter().any(|existing| {
        existing.label == window.label
            && existing.limit_window_seconds == window.limit_window_seconds
    }) {
        return;
    }
    windows.push(window);
}

fn anthropic_weekly_label(key: &str) -> Option<String> {
    match key {
        "seven_day" => None,
        "seven_day_sonnet" => Some("Sonnet · 7d".to_string()),
        "seven_day_opus" => Some("Opus · 7d".to_string()),
        "seven_day_routines" => Some("Routines · 7d".to_string()),
        "seven_day_cowork" => Some("Cowork · 7d".to_string()),
        _ => key.strip_prefix("seven_day_").map(|scope| {
            let name = scope
                .split('_')
                .map(|part| {
                    let mut chars = part.chars();
                    match chars.next() {
                        Some(first) => first.to_uppercase().chain(chars).collect(),
                        None => String::new(),
                    }
                })
                .collect::<Vec<String>>()
                .join(" ");
            format!("{name} · 7d")
        }),
    }
}

fn parse_anthropic_usage(body: &str) -> Result<ParsedProviderUsage, String> {
    let response: Value =
        serde_json::from_str(body).map_err(|error| format!("invalid usage response: {error}"))?;
    let object = response
        .as_object()
        .ok_or_else(|| "usage response is not an object".to_string())?;
    let plan_type = ["subscription_type", "plan_type", "rate_limit_tier"]
        .into_iter()
        .find_map(|key| object.get(key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let mut windows = Vec::new();
    push_unique_usage_window(
        &mut windows,
        map_anthropic_usage_window(object.get("five_hour"), None, FIVE_HOURS_SECONDS),
    );
    for key in [
        "seven_day",
        "seven_day_sonnet",
        "seven_day_opus",
        "seven_day_routines",
        "seven_day_cowork",
    ] {
        push_unique_usage_window(
            &mut windows,
            map_anthropic_usage_window(
                object.get(key),
                anthropic_weekly_label(key),
                SEVEN_DAYS_SECONDS,
            ),
        );
    }

    // Preserve future model/feature-specific weekly fields without requiring
    // a release for every new `seven_day_*` key Anthropic adds.
    for (key, value) in object {
        if key.starts_with("seven_day_")
            && !matches!(
                key.as_str(),
                "seven_day_sonnet" | "seven_day_opus" | "seven_day_routines" | "seven_day_cowork"
            )
        {
            push_unique_usage_window(
                &mut windows,
                map_anthropic_usage_window(
                    Some(value),
                    anthropic_weekly_label(key),
                    SEVEN_DAYS_SECONDS,
                ),
            );
        }
    }

    // Newer responses may group model-specific limits under
    // `limits[].weekly_scoped`; normalize those into the same UI model.
    if let Some(limits) = object.get("limits").and_then(Value::as_array) {
        for (index, limit) in limits.iter().filter_map(Value::as_object).enumerate() {
            let Some(window) = limit.get("weekly_scoped") else {
                continue;
            };
            let name = ["display_name", "limit_name", "metered_feature", "model"]
                .into_iter()
                .find_map(|key| limit.get(key).and_then(Value::as_str))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("Weekly limit {}", index + 1));
            push_unique_usage_window(
                &mut windows,
                map_anthropic_usage_window(
                    Some(window),
                    Some(format!("{name} · 7d")),
                    SEVEN_DAYS_SECONDS,
                ),
            );
        }
    }

    if windows.is_empty() {
        return Err("usage response has no supported rate-limit windows".to_string());
    }
    Ok(ParsedProviderUsage { plan_type, windows })
}

fn provider_oauth_account_email(provider: &Provider) -> Option<String> {
    provider
        .oauth_meta_cleartext
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|meta| {
            meta.get("account_email")
                .or_else(|| meta.get("email"))
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
}

fn ensure_provider_usage_user_agent(vendor: &str, headers: &mut reqwest::header::HeaderMap) {
    if vendor == "anthropic" {
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static(ANTHROPIC_OAUTH_USAGE_USER_AGENT),
        );
    }
}

fn ensure_openai_codex_usage_headers(vendor: &str, headers: &mut reqwest::header::HeaderMap) {
    if vendor != "openai" {
        return;
    }
    for (name, value) in [
        ("openai-beta", OPENAI_CODEX_BETA),
        ("oai-language", OPENAI_CODEX_LANGUAGE),
        ("sec-fetch-site", OPENAI_CODEX_SEC_FETCH_SITE),
        ("sec-fetch-mode", OPENAI_CODEX_SEC_FETCH_MODE),
        ("sec-fetch-dest", OPENAI_CODEX_SEC_FETCH_DEST),
        ("priority", OPENAI_CODEX_PRIORITY),
    ] {
        let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        headers.insert(name, reqwest::header::HeaderValue::from_static(value));
    }
}

async fn query_openai_reset_credits(
    client: &reqwest::Client,
    headers: &reqwest::header::HeaderMap,
) -> Option<ProviderResetCredits> {
    match tokio::time::timeout(
        OPENAI_CODEX_RESET_CREDITS_TIMEOUT,
        query_openai_reset_credits_inner(client, headers),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            tracing::debug!("OpenAI OAuth reset-credit query timed out; keeping usage response");
            None
        }
    }
}

async fn query_openai_reset_credits_inner(
    client: &reqwest::Client,
    headers: &reqwest::header::HeaderMap,
) -> Option<ProviderResetCredits> {
    let mut request = client
        .get(OPENAI_CODEX_RESET_CREDITS_URL)
        .header(reqwest::header::ACCEPT, "application/json");
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::debug!(error = %error, "OpenAI OAuth reset-credit query failed");
            return None;
        }
    };
    if !response.status().is_success() {
        tracing::debug!(status = %response.status(), "OpenAI OAuth reset-credit query returned an error");
        return None;
    }
    let body = match response.text().await {
        Ok(body) => body,
        Err(error) => {
            tracing::debug!(error = %error, "OpenAI OAuth reset-credit response read failed");
            return None;
        }
    };
    let value = match serde_json::from_str::<Value>(&body) {
        Ok(value) => value,
        Err(error) => {
            tracing::debug!(error = %error, "OpenAI OAuth reset-credit response was not JSON");
            return None;
        }
    };
    parse_reset_credits(&value)
}

fn parse_reset_credits_consume_response(
    provider_id: &str,
    body: &str,
) -> Result<ProviderResetCreditsConsumeResponse, String> {
    let response_body =
        serde_json::from_str::<Value>(body).map_err(|error| format!("invalid JSON: {error}"))?;
    let code = response_body
        .get("code")
        .or_else(|| response_body.get("status"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|code| !code.trim().is_empty())
        .ok_or_else(|| "reset-credit response has no code".to_string())?;
    let windows_reset = response_body
        .get("windows_reset")
        .or_else(|| response_body.get("windowsReset"))
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|raw| raw.trim().parse().ok()))
        });
    Ok(ProviderResetCreditsConsumeResponse {
        provider_id: provider_id.to_string(),
        code,
        windows_reset,
    })
}

fn require_reset_credit_success(
    response: ProviderResetCreditsConsumeResponse,
) -> Result<ProviderResetCreditsConsumeResponse, AdminError> {
    if !response.code.eq_ignore_ascii_case("reset") {
        return Err(AdminError::Conflict(format!(
            "OpenAI reset-credit consume returned code '{}'",
            response.code
        )));
    }
    Ok(response)
}

async fn prepare_oauth_usage_request(
    state: &AdminState,
    provider_id: &str,
    provider: &Provider,
    oauth_config: &tiygate_core::OAuthTargetConfig,
    stored_account_email: Option<&str>,
) -> Result<(reqwest::Client, reqwest::header::HeaderMap, Option<String>), String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("http client build: {error}"))?;
    let cache = tiygate_auth::provider_oauth::OAuthTokenCache::global();
    let label = oauth_config.cache_label();
    let mut headers = reqwest::header::HeaderMap::new();
    let coordinated = state.oauth_service.is_some();
    let apply_result = if let Some(service) = state.oauth_service.as_ref() {
        service
            .apply_provider_headers(provider_id, &mut headers)
            .await
    } else {
        cache.seed(provider_id, label, &oauth_config.refresh_token);
        cache
            .apply(&mut headers, provider_id, label, oauth_config, &client)
            .await
    };
    if let Err(error) = apply_result {
        record_oauth_refresh_failure(state, provider_id, &error).await;
        return Err(error);
    }

    let account_email = cache
        .get_account_email(provider_id, label)
        .or_else(|| stored_account_email.map(str::to_string));
    if !coordinated {
        if let Some(cached_refresh_token) = cache.get_refresh_token(provider_id, label) {
            match oauth_meta_after_cache_update(
                provider,
                &oauth_config.refresh_token,
                &cached_refresh_token,
                account_email.as_deref(),
            ) {
                Ok(Some(meta)) => {
                    if let Err(error) = state
                        .store
                        .set_provider_oauth_meta(provider_id, &meta)
                        .await
                    {
                        tracing::warn!(
                            provider = %provider_id,
                            error = %error,
                            "persisting OAuth identity after usage request failed"
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        provider = %provider_id,
                        error = %error,
                        "preparing OAuth identity metadata failed"
                    );
                }
            }
        }
    }

    ensure_provider_usage_user_agent(&provider.vendor, &mut headers);
    ensure_openai_codex_usage_headers(&provider.vendor, &mut headers);
    Ok((client, headers, account_email))
}

/// Fetch subscription usage windows for one supported OAuth provider. The
/// OAuth cache is keyed by provider/account, so multiple providers can safely
/// use different upstream accounts in one process.
async fn provider_usage(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    let provider = state
        .store
        .get_provider(&id)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("provider {id}")))?;
    let stored_account_email = provider_oauth_account_email(&provider);

    if !matches!(provider.auth_mode, AuthMode::OAuth) {
        return Ok(Json(provider_usage_response(
            &id,
            "unsupported",
            Some("oauth_only"),
            Vec::new(),
            stored_account_email.as_deref(),
        ))
        .into_response());
    }
    let usage_url = match provider.vendor.as_str() {
        "openai" => OPENAI_CODEX_USAGE_URL,
        "anthropic" => ANTHROPIC_OAUTH_USAGE_URL,
        _ => {
            return Ok(Json(provider_usage_response(
                &id,
                "unsupported",
                Some("oauth_usage_unsupported"),
                Vec::new(),
                stored_account_email.as_deref(),
            ))
            .into_response());
        }
    };

    let Some(oauth_config) = tiygate_store::config_store::build_oauth_target_config(&provider)
    else {
        return Ok(Json(provider_usage_response(
            &id,
            "not_connected",
            Some("oauth_metadata_unavailable"),
            Vec::new(),
            stored_account_email.as_deref(),
        ))
        .into_response());
    };
    if oauth_config.refresh_token.is_empty() {
        return Ok(Json(provider_usage_response(
            &id,
            "not_connected",
            Some("refresh_token_missing"),
            Vec::new(),
            stored_account_email.as_deref(),
        ))
        .into_response());
    }

    let (client, headers, account_email) = match prepare_oauth_usage_request(
        &state,
        &id,
        &provider,
        &oauth_config,
        stored_account_email.as_deref(),
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::warn!(provider = %id, vendor = %provider.vendor, error = %error, "OAuth usage token unavailable");
            return Ok(Json(provider_usage_response(
                &id,
                "unavailable",
                Some("oauth_token_unavailable"),
                Vec::new(),
                stored_account_email.as_deref(),
            ))
            .into_response());
        }
    };
    let mut request = client
        .get(usage_url)
        .header(reqwest::header::ACCEPT, "application/json");
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(provider = %id, vendor = %provider.vendor, error = %error, "OAuth usage request failed");
            return Ok(Json(provider_usage_response(
                &id,
                "unavailable",
                Some("upstream_request_failed"),
                Vec::new(),
                account_email.as_deref(),
            ))
            .into_response());
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        // Anthropic may deny the auxiliary usage endpoint for a valid
        // inference credential. Only a 401 proves that credential unusable;
        // retain OpenAI's existing 403 treatment for its ChatGPT surface.
        if status == StatusCode::UNAUTHORIZED
            || (provider.vendor == "openai" && status == StatusCode::FORBIDDEN)
        {
            record_oauth_status(
                &state,
                &id,
                OAuthCredentialStatus::Invalid,
                Some("usage_auth_rejected"),
            )
            .await;
        }
        tracing::warn!(provider = %id, vendor = %provider.vendor, status = %status, "OAuth usage endpoint returned an error");
        return Ok(Json(provider_usage_response(
            &id,
            "unavailable",
            Some("upstream_http_error"),
            Vec::new(),
            account_email.as_deref(),
        ))
        .into_response());
    }

    let body = response
        .text()
        .await
        .map_err(|error| AdminError::Internal(format!("read usage response: {error}")))?;
    let mut reset_credits = None;
    if provider.vendor == "openai" {
        match serde_json::from_str::<Value>(&body) {
            Ok(response) => {
                let rate_limit = response
                    .get("rate_limit")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "<missing>".to_string());
                tracing::debug!(
                    target: "tiygate_admin::usage",
                    provider = %id,
                    rate_limit = %rate_limit,
                    "OpenAI OAuth usage rate-limit response"
                );
                reset_credits = response
                    .get("rate_limit_reset_credits")
                    .or_else(|| response.get("rateLimitResetCredits"))
                    .and_then(parse_reset_credits);
            }
            Err(error) => {
                tracing::debug!(
                    target: "tiygate_admin::usage",
                    provider = %id,
                    error = %error,
                    "OpenAI OAuth usage response was not valid JSON"
                );
            }
        }
        if let Some(credits) = query_openai_reset_credits(&client, &headers).await {
            reset_credits = Some(credits);
        }
    }
    let parsed_result = match provider.vendor.as_str() {
        "openai" => parse_openai_usage(&body, chrono::Utc::now().timestamp()),
        "anthropic" => parse_anthropic_usage(&body),
        _ => Err("unsupported OAuth usage provider".to_string()),
    };
    let parsed_usage = match parsed_result {
        Ok(usage) => usage,
        Err(error) => {
            tracing::warn!(provider = %id, vendor = %provider.vendor, error = %error, "OAuth usage response parse failed");
            return Ok(Json(provider_usage_response(
                &id,
                "unavailable",
                Some("invalid_upstream_response"),
                Vec::new(),
                account_email.as_deref(),
            ))
            .into_response());
        }
    };
    let mut usage = provider_usage_response(
        &id,
        "available",
        None,
        parsed_usage.windows,
        account_email.as_deref(),
    );
    usage.plan_type = parsed_usage.plan_type;
    usage.reset_credits = reset_credits;
    Ok(Json(usage).into_response())
}

async fn provider_usage_reset_credits(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(request): Json<ProviderResetCreditsConsumeRequest>,
) -> Result<Json<ProviderResetCreditsConsumeResponse>, AdminError> {
    let redeem_request_id = request.redeem_request_id.trim();
    if redeem_request_id.is_empty() {
        return Err(AdminError::BadRequest(
            "redeem_request_id must not be empty".to_string(),
        ));
    }
    let provider = state
        .store
        .get_provider(&id)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("provider {id}")))?;
    if provider.vendor != "openai" || !matches!(provider.auth_mode, AuthMode::OAuth) {
        return Err(AdminError::BadRequest(
            "reset credits require an OpenAI OAuth provider".to_string(),
        ));
    }
    let oauth_config = tiygate_store::config_store::build_oauth_target_config(&provider)
        .ok_or_else(|| {
            AdminError::BadRequest("OpenAI OAuth metadata is unavailable".to_string())
        })?;
    if oauth_config.refresh_token.is_empty() {
        return Err(AdminError::BadRequest(
            "OpenAI OAuth refresh token is missing".to_string(),
        ));
    }

    let stored_account_email = provider_oauth_account_email(&provider);
    let (client, headers, _) = prepare_oauth_usage_request(
        &state,
        &id,
        &provider,
        &oauth_config,
        stored_account_email.as_deref(),
    )
    .await
    .map_err(|error| {
        tracing::warn!(provider = %id, error = %error, "OAuth token unavailable for reset-credit consume");
        AdminError::Internal("OAuth token unavailable for reset-credit consume".to_string())
    })?;

    let mut request = client
        .post(OPENAI_CODEX_RESET_CREDITS_CONSUME_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&json!({ "redeem_request_id": redeem_request_id }));
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|error| AdminError::Internal(format!("reset-credit request failed: {error}")))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| AdminError::Internal(format!("read reset-credit response: {error}")))?;
    if !status.is_success() {
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            record_oauth_status(
                &state,
                &id,
                OAuthCredentialStatus::Invalid,
                Some("reset_credit_auth_rejected"),
            )
            .await;
        }
        return Err(AdminError::BadRequest(format!(
            "OpenAI reset-credit consume failed (HTTP {status})"
        )));
    }

    let response_body = parse_reset_credits_consume_response(&id, &body)
        .map_err(|error| AdminError::Internal(format!("invalid reset-credit response: {error}")))?;
    Ok(Json(require_reset_credit_success(response_body)?))
}

/// Discover models available on a provider's upstream API.
///
/// Calls the provider's `models_endpoint` (or falls back to
/// `api_base + /models`) to list available models. The response is
/// normalized to `{ models: [{ id }] }` regardless of upstream format
/// (OpenAI `data[].id`, Gemini `models[].name`, or generic
/// `models[].id`). Any error — network, timeout, non-2xx, parse
/// failure — is logged and returns an empty list with HTTP 200 so the
/// UI silently degrades to a plain input.
async fn list_provider_models(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    // Read the provider from the DB first, then try to get the
    // decrypted API key from the in-memory snapshot (populated by
    // `DbConfigStore::refresh()`). If the snapshot does not have it
    // (e.g. no master key configured), fall back to the encrypted
    // column as-is (cleartext-fallback mode).
    let provider = state
        .store
        .get_provider(&id)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("provider {id}")))?;

    let api_key = if let Some(snap) = state.store.snapshot().snapshot() {
        snap.providers
            .get(&id)
            .and_then(|p| p.api_key_cleartext.clone())
    } else {
        None
    }
    .unwrap_or_else(|| {
        // No master key configured: the encrypted column holds the
        // cleartext verbatim.
        if provider.encrypted_api_key.is_empty() {
            String::new()
        } else {
            provider.encrypted_api_key.clone()
        }
    });

    // Resolve the discovery URL.
    let url = provider_models_url(&provider);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| AdminError::Internal(format!("http client build: {e}")))?;

    let mut req = client.get(&url);

    // For OAuth-mode providers, obtain an access token via the
    // process-global OAuthTokenCache instead of using a static API
    // key (which is empty for OAuth providers).
    if matches!(provider.auth_mode, tiygate_store::models::AuthMode::OAuth) {
        if let Some(service) = state.oauth_service.as_ref() {
            let mut headers = reqwest::header::HeaderMap::new();
            if let Err(error) = service.apply_provider_headers(&id, &mut headers).await {
                tracing::warn!(provider = %id, error = %error, "OAuth token unavailable for model discovery; returning empty list");
                return Ok(Json(ProviderModelsResponse { models: vec![] }).into_response());
            }
            for (name, value) in &headers {
                req = req.header(name, value);
            }
        } else {
            let cache = tiygate_auth::provider_oauth::OAuthTokenCache::global();
            // Build the OAuth target config from the provider's
            // metadata + decrypted refresh token.
            if let Some(oauth_config) =
                tiygate_store::config_store::build_oauth_target_config(&provider)
            {
                // Share one cache entry with the data plane so model discovery
                // cannot race a routed request through refresh-token rotation.
                let label = oauth_config.cache_label();
                cache.seed(&id, label, &oauth_config.refresh_token);

                let mut headers = reqwest::header::HeaderMap::new();
                match cache
                    .apply(&mut headers, &id, label, &oauth_config, &client)
                    .await
                {
                    Ok(()) => {
                        if let Some(cached_refresh_token) = cache.get_refresh_token(&id, label) {
                            match oauth_meta_after_refresh_rotation(
                                &provider,
                                &oauth_config.refresh_token,
                                &cached_refresh_token,
                            ) {
                                Ok(Some(meta)) => {
                                    if let Err(e) =
                                        state.store.set_provider_oauth_meta(&id, &meta).await
                                    {
                                        tracing::warn!(
                                            provider = %id,
                                            error = %e,
                                            "persisting rotated OAuth refresh token after model discovery failed; \
                                             returning empty list"
                                        );
                                        return Ok(Json(ProviderModelsResponse { models: vec![] })
                                            .into_response());
                                    }
                                    tracing::info!(
                                        provider = %id,
                                        "persisted rotated OAuth refresh token after model discovery"
                                    );
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    tracing::warn!(
                                        provider = %id,
                                        error = %e,
                                        "preparing rotated OAuth refresh token metadata failed; \
                                         returning empty list"
                                    );
                                    return Ok(Json(ProviderModelsResponse { models: vec![] })
                                        .into_response());
                                }
                            }
                        }

                        // Merge the injected headers into the reqwest
                        // request builder.
                        for (name, value) in headers.iter() {
                            if let Ok(v) =
                                reqwest::header::HeaderValue::from_bytes(value.as_bytes())
                            {
                                req = req.header(name.as_str(), v);
                            }
                        }
                    }
                    Err(e) => {
                        record_oauth_refresh_failure(&state, &id, &e).await;
                        tracing::warn!(
                            provider = %id,
                            error = %e,
                            "OAuth token refresh failed for model discovery; \
                             returning empty list"
                        );
                        return Ok(Json(ProviderModelsResponse { models: vec![] }).into_response());
                    }
                }
            } else {
                tracing::warn!(
                    provider = %id,
                    "OAuth provider missing OAuth config for model discovery; \
                     returning empty list"
                );
                return Ok(Json(ProviderModelsResponse { models: vec![] }).into_response());
            }
        }
    } else if !api_key.is_empty() {
        req = req.bearer_auth(&api_key);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                provider = %id,
                url = %url,
                error = %e,
                "provider model discovery request failed; returning empty list"
            );
            return Ok(Json(ProviderModelsResponse { models: vec![] }).into_response());
        }
    };

    if !resp.status().is_success() {
        if matches!(
            resp.status(),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            record_oauth_status(
                &state,
                &id,
                OAuthCredentialStatus::Invalid,
                Some("upstream_auth_rejected"),
            )
            .await;
        }
        tracing::warn!(
            provider = %id,
            url = %url,
            status = %resp.status(),
            "provider model discovery returned non-2xx; returning empty list"
        );
        return Ok(Json(ProviderModelsResponse { models: vec![] }).into_response());
    }

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                provider = %id,
                url = %url,
                error = %e,
                "provider model discovery response parse failed; returning empty list"
            );
            return Ok(Json(ProviderModelsResponse { models: vec![] }).into_response());
        }
    };

    let models = parse_model_list(&body);
    if matches!(provider.auth_mode, AuthMode::OAuth)
        && provider_oauth_status(&provider).state != "healthy"
    {
        record_oauth_status(&state, &id, OAuthCredentialStatus::Healthy, None).await;
    }
    Ok(Json(ProviderModelsResponse { models }).into_response())
}

async fn record_oauth_refresh_failure(state: &AdminState, provider_id: &str, error: &str) {
    let kind = tiygate_auth::provider_oauth::classify_refresh_failure(error);
    let status = match kind {
        tiygate_auth::provider_oauth::OAuthRefreshFailureKind::CredentialInvalid => {
            OAuthCredentialStatus::Invalid
        }
        tiygate_auth::provider_oauth::OAuthRefreshFailureKind::Transient => {
            OAuthCredentialStatus::Error
        }
    };
    record_oauth_status(state, provider_id, status, Some(kind.status_reason())).await;
}

async fn record_oauth_status(
    state: &AdminState,
    provider_id: &str,
    status: OAuthCredentialStatus,
    reason: Option<&str>,
) {
    if let Err(e) = state
        .store
        .set_provider_oauth_status(provider_id, status, reason)
        .await
    {
        tracing::warn!(
            provider = %provider_id,
            error = %e,
            "persisting OAuth credential status failed"
        );
    }
}

/// Build the OAuth metadata that must be persisted after the token cache
/// observes refresh-token rotation. Existing fields such as `account_id` and
/// `expires_in_s` are retained so model discovery cannot erase credential
/// context while updating the token.
fn oauth_meta_after_cache_update(
    provider: &Provider,
    stored_refresh_token: &str,
    cached_refresh_token: &str,
    account_email: Option<&str>,
) -> Result<Option<String>, String> {
    let raw = provider
        .oauth_meta_cleartext
        .as_deref()
        .ok_or_else(|| "decrypted OAuth metadata is unavailable".to_string())?;
    let mut meta: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("parsing decrypted OAuth metadata: {e}"))?;
    let object = meta
        .as_object_mut()
        .ok_or_else(|| "decrypted OAuth metadata must be a JSON object".to_string())?;
    let mut refresh_rotated = false;
    if cached_refresh_token != stored_refresh_token {
        object.insert(
            "refresh_token".to_string(),
            serde_json::Value::String(cached_refresh_token.to_string()),
        );
        refresh_rotated = true;
    }
    let email_changed = account_email.is_some_and(|email| {
        object.get("account_email").and_then(|value| value.as_str()) != Some(email)
    });
    if let Some(email) = account_email.filter(|email| !email.is_empty()) {
        if email_changed {
            object.insert(
                "account_email".to_string(),
                serde_json::Value::String(email.to_string()),
            );
        }
    }
    if !refresh_rotated && !email_changed {
        return Ok(None);
    }
    if refresh_rotated {
        object.insert(
            "status".to_string(),
            serde_json::Value::String(OAuthCredentialStatus::Healthy.as_str().to_string()),
        );
        object.insert(
            "status_checked_at".to_string(),
            serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
        );
        object.remove("status_reason");
    }
    serde_json::to_string(&meta)
        .map(Some)
        .map_err(|e| format!("serializing updated OAuth metadata: {e}"))
}

fn oauth_meta_after_refresh_rotation(
    provider: &Provider,
    stored_refresh_token: &str,
    cached_refresh_token: &str,
) -> Result<Option<String>, String> {
    if cached_refresh_token == stored_refresh_token {
        return Ok(None);
    }

    let raw = provider
        .oauth_meta_cleartext
        .as_deref()
        .ok_or_else(|| "decrypted OAuth metadata is unavailable".to_string())?;
    let mut meta: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("parsing decrypted OAuth metadata: {e}"))?;
    let object = meta
        .as_object_mut()
        .ok_or_else(|| "decrypted OAuth metadata must be a JSON object".to_string())?;
    object.insert(
        "refresh_token".to_string(),
        serde_json::Value::String(cached_refresh_token.to_string()),
    );
    object.insert(
        "status".to_string(),
        serde_json::Value::String(OAuthCredentialStatus::Healthy.as_str().to_string()),
    );
    object.insert(
        "status_checked_at".to_string(),
        serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
    );
    object.remove("status_reason");
    serde_json::to_string(&meta)
        .map(Some)
        .map_err(|e| format!("serializing rotated OAuth metadata: {e}"))
}

fn is_openai_codex_oauth(provider: &Provider) -> bool {
    provider.vendor == "openai" && matches!(provider.auth_mode, AuthMode::OAuth)
}

fn effective_provider_api_base(provider: &Provider) -> String {
    if is_openai_codex_oauth(provider)
        && (provider.api_base.trim().is_empty()
            || provider.api_base.trim_end_matches('/') == OPENAI_PLATFORM_BASE_URL)
    {
        OPENAI_CODEX_BASE_URL.to_string()
    } else if provider.api_base.trim().is_empty()
        && provider.vendor == "openai"
        && matches!(provider.auth_mode, AuthMode::ApiKey)
    {
        OPENAI_PLATFORM_BASE_URL.to_string()
    } else {
        provider.api_base.trim_end_matches('/').to_string()
    }
}

fn provider_models_url(provider: &Provider) -> String {
    let configured = provider.models_endpoint.trim();
    let old_platform_default = format!("{OPENAI_PLATFORM_BASE_URL}/models");
    let mut url = if is_openai_codex_oauth(provider)
        && (configured.is_empty() || configured.trim_end_matches('/') == old_platform_default)
    {
        format!("{OPENAI_CODEX_BASE_URL}/models")
    } else if configured.is_empty() {
        format!("{}/models", effective_provider_api_base(provider))
    } else {
        configured.to_string()
    };

    if is_openai_codex_oauth(provider) && !url.contains("client_version=") {
        let separator = if url.contains('?') { '&' } else { '?' };
        url.push(separator);
        url.push_str("client_version=");
        url.push_str(tiygate_auth::provider_oauth::CODEX_CLIENT_VERSION);
    }
    url
}

/// Normalize upstream model-list responses into a sorted list of
/// `ProviderModelEntry`. Supports:
/// - OpenAI: `{ "data": [{ "id": "gpt-4o", ... }] }`
/// - Gemini: `{ "models": [{ "name": "models/gemini-pro", ... }] }`
/// - Generic: `{ "models": [{ "id": "..." }] }`
fn parse_model_list(body: &serde_json::Value) -> Vec<ProviderModelEntry> {
    let mut ids: Vec<String> = Vec::new();

    // OpenAI format: data[].id
    if let Some(data) = body.get("data").and_then(|d| d.as_array()) {
        for item in data {
            if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                if !id.is_empty() {
                    ids.push(id.to_string());
                }
            }
        }
    }

    // Gemini / generic / Codex format: models[].name, models[].id,
    // or Codex models[].slug.
    if ids.is_empty() {
        if let Some(models) = body.get("models").and_then(|m| m.as_array()) {
            for item in models {
                if item
                    .get("visibility")
                    .and_then(|value| value.as_str())
                    .is_some_and(|visibility| visibility != "list")
                {
                    continue;
                }
                if let Some(slug) = item.get("slug").and_then(|n| n.as_str()) {
                    if !slug.is_empty() {
                        ids.push(slug.to_string());
                    }
                // Gemini uses "name" with a "models/" prefix
                } else if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                    let id = name.strip_prefix("models/").unwrap_or(name);
                    if !id.is_empty() {
                        ids.push(id.to_string());
                    }
                } else if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                    if !id.is_empty() {
                        ids.push(id.to_string());
                    }
                }
            }
        }
    }

    // Fallback: if neither data[] nor models[] matched, try to find
    // any array of objects with an "id" field at the top level.
    if ids.is_empty() {
        if let Some(obj) = body.as_object() {
            for (_key, val) in obj {
                if let Some(arr) = val.as_array() {
                    for item in arr {
                        if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                            if !id.is_empty() {
                                ids.push(id.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    ids.sort();
    ids.dedup();
    ids.into_iter()
        .map(|id| ProviderModelEntry { id })
        .collect()
}

// ---- health ----

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

// ---- server info ----

async fn info() -> impl IntoResponse {
    Json(json!({
        "name": "tiygate",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

// ---- audit snapshot / diff helpers ----
//
// Audit `details` follow a stable structured schema so the UI can
// render them predictably:
//   {"snapshot": {redacted full object...}, "changes": [{field,before,after}...]}
// create operations carry only a snapshot; update/upsert carry both;
// delete records the snapshot of the removed object.

/// Build a redacted JSON snapshot of a provider. Sensitive credentials
/// (`api_key`, `oauth_meta`) go through [`KeyEncryption::redact`] so the
/// audit table never stores cleartext secrets.
fn provider_snapshot(p: &Provider) -> serde_json::Value {
    json!({
        "id": p.id,
        "name": p.name,
        "vendor": p.vendor,
        "api_base": p.api_base,
        "models_endpoint": p.models_endpoint,
        "auth_mode": p.auth_mode.as_str(),
        "enabled": p.enabled,
        "metadata": p.metadata_json,
        "api_key": tiygate_store::encryption::KeyEncryption::redact(&p.encrypted_api_key),
        "oauth_meta": tiygate_store::encryption::KeyEncryption::redact(&p.encrypted_oauth_meta),
    })
}

/// Build a redacted JSON snapshot of a route. Target credential and URL
/// override values are represented only by presence flags.
fn route_snapshot(r: &Route) -> serde_json::Value {
    let targets = r
        .targets
        .iter()
        .map(|target| {
            json!({
                "provider_id": target.provider_id,
                "model_id": target.model_id,
                "weight": target.weight,
                "enabled": target.enabled,
                "egress_dialect_id": target.egress_dialect_id,
                "account_label_present": target.account_label.is_some(),
                "api_key_override_configured": target.api_key_override.is_some(),
                "api_base_override_configured": target.api_base_override.is_some()
            })
        })
        .collect::<Vec<_>>();
    json!({
        "id": r.id,
        "virtual_model": r.virtual_model,
        "targets": targets,
        "routing_strategy": r.routing_strategy,
        "capability_routing_mode": r.capability_routing_mode,
        "model_metadata": r.model_metadata,
        "enabled": r.enabled,
    })
}

/// Build a JSON snapshot of an api key. The secret hash is intentionally
/// excluded — only operator-facing metadata is recorded.
fn api_key_snapshot(k: &tiygate_store::models::ApiKey) -> serde_json::Value {
    json!({
        "id": k.id,
        "name": k.name,
        "status": k.status.as_str(),
        "quota": k.quota_json,
        "allowed_models": k.allowed_models,
    })
}

/// Compute field-level changes between two flat JSON object snapshots.
/// Walks the union of keys; any key whose value differs yields a
/// `{field, before, after}` entry. Array/object values are compared as
/// whole JSON (e.g. route `targets`).
fn diff_fields(before: &serde_json::Value, after: &serde_json::Value) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let empty = serde_json::Map::new();
    let before_obj = before.as_object().unwrap_or(&empty);
    let after_obj = after.as_object().unwrap_or(&empty);
    // Stable key order: after's keys first (insertion order), then any
    // before-only keys not already seen.
    let mut keys: Vec<&String> = after_obj.keys().collect();
    for k in before_obj.keys() {
        if !after_obj.contains_key(k) {
            keys.push(k);
        }
    }
    let null = serde_json::Value::Null;
    for k in keys {
        let b = before_obj.get(k).unwrap_or(&null);
        let a = after_obj.get(k).unwrap_or(&null);
        if b != a {
            out.push(json!({"field": k, "before": b, "after": a}));
        }
    }
    out
}

/// Assemble the structured audit `details` payload. `after` is the
/// post-write snapshot (used as `snapshot`); when `before` is present a
/// field-level `changes` list is computed against it.
fn audit_details(
    before: Option<&serde_json::Value>,
    after: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    if let Some(after) = after {
        obj.insert("snapshot".to_string(), after.clone());
        if let Some(before) = before {
            obj.insert(
                "changes".to_string(),
                serde_json::Value::Array(diff_fields(before, after)),
            );
        }
    } else if let Some(before) = before {
        // delete: record the removed object's snapshot.
        obj.insert("snapshot".to_string(), before.clone());
    }
    serde_json::Value::Object(obj)
}

// ---- providers ----

#[derive(Debug, Deserialize)]
struct ProviderRequest {
    id: Option<String>,
    name: String,
    vendor: String,
    api_base: String,
    models_endpoint: Option<String>,
    api_key: Option<String>,
    auth_mode: Option<String>,
    oauth_meta: Option<String>,
    metadata: Option<serde_json::Value>,
    enabled: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ProviderOAuthStatusView {
    state: String,
    reason: Option<String>,
    checked_at: Option<String>,
}

#[derive(Debug, Serialize)]
struct ProviderView {
    id: String,
    name: String,
    vendor: String,
    api_base: String,
    models_endpoint: String,
    auth_mode: String,
    encrypted_api_key: String,
    encrypted_oauth_meta: String,
    oauth_status: Option<ProviderOAuthStatusView>,
    metadata: serde_json::Value,
    enabled: bool,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<Provider> for ProviderView {
    fn from(p: Provider) -> Self {
        let api_base = normalized_api_base(&p.vendor, p.auth_mode, &p.api_base);
        let models_endpoint =
            normalized_models_endpoint(&p.vendor, p.auth_mode, &p.models_endpoint, &api_base);
        let oauth_status =
            matches!(p.auth_mode, AuthMode::OAuth).then(|| provider_oauth_status(&p));
        Self {
            id: p.id,
            name: p.name,
            vendor: p.vendor,
            api_base,
            models_endpoint,
            auth_mode: p.auth_mode.as_str().to_string(),
            encrypted_api_key: tiygate_store::encryption::KeyEncryption::redact(
                &p.encrypted_api_key,
            ),
            encrypted_oauth_meta: tiygate_store::encryption::KeyEncryption::redact(
                &p.encrypted_oauth_meta,
            ),
            oauth_status,
            metadata: p.metadata_json,
            enabled: p.enabled,
            created_at: p.created_at,
            updated_at: p.updated_at,
        }
    }
}

fn provider_oauth_status(provider: &Provider) -> ProviderOAuthStatusView {
    if provider.encrypted_oauth_meta.trim().is_empty() {
        return ProviderOAuthStatusView {
            state: "not_connected".to_string(),
            reason: None,
            checked_at: None,
        };
    }
    let Some(raw) = provider.oauth_meta_cleartext.as_deref() else {
        return ProviderOAuthStatusView {
            state: "error".to_string(),
            reason: Some("metadata_unavailable".to_string()),
            checked_at: None,
        };
    };
    let Ok(meta) = serde_json::from_str::<serde_json::Value>(raw) else {
        return ProviderOAuthStatusView {
            state: "error".to_string(),
            reason: Some("metadata_invalid".to_string()),
            checked_at: None,
        };
    };
    if meta
        .get("refresh_token")
        .and_then(|value| value.as_str())
        .is_none_or(str::is_empty)
    {
        return ProviderOAuthStatusView {
            state: "not_connected".to_string(),
            reason: None,
            checked_at: None,
        };
    }
    let state = meta
        .get("status")
        .and_then(|value| value.as_str())
        .filter(|state| matches!(*state, "healthy" | "invalid" | "error"))
        .unwrap_or("connected")
        .to_string();
    ProviderOAuthStatusView {
        state,
        reason: meta
            .get("status_reason")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        checked_at: meta
            .get("status_checked_at")
            .and_then(|value| value.as_str())
            .map(str::to_string),
    }
}

fn normalized_api_base(vendor: &str, auth_mode: AuthMode, configured: &str) -> String {
    let configured = configured.trim_end_matches('/');
    if vendor == "openai" && matches!(auth_mode, AuthMode::OAuth) {
        if configured.is_empty() || configured == OPENAI_PLATFORM_BASE_URL {
            return OPENAI_CODEX_BASE_URL.to_string();
        }
    } else if vendor == "openai"
        && matches!(auth_mode, AuthMode::ApiKey)
        && (configured.is_empty() || configured == OPENAI_CODEX_BASE_URL)
    {
        return OPENAI_PLATFORM_BASE_URL.to_string();
    }
    configured.to_string()
}

fn normalized_models_endpoint(
    vendor: &str,
    auth_mode: AuthMode,
    configured: &str,
    api_base: &str,
) -> String {
    let configured = configured.trim_end_matches('/');
    let platform_models = format!("{OPENAI_PLATFORM_BASE_URL}/models");
    let codex_models = format!("{OPENAI_CODEX_BASE_URL}/models");
    if vendor == "openai" && matches!(auth_mode, AuthMode::OAuth) {
        if configured.is_empty() || configured == platform_models {
            return codex_models;
        }
    } else if vendor == "openai"
        && matches!(auth_mode, AuthMode::ApiKey)
        && (configured.is_empty() || configured == codex_models)
    {
        return platform_models;
    }
    if configured.is_empty() && !api_base.is_empty() {
        format!("{}/models", api_base.trim_end_matches('/'))
    } else {
        configured.to_string()
    }
}

fn normalized_provider_metadata(
    vendor: &str,
    auth_mode: AuthMode,
    metadata: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut metadata = metadata.unwrap_or_else(|| json!({}));
    if !matches!(auth_mode, AuthMode::OAuth) {
        return metadata;
    }
    let Some(preset) = tiygate_auth::provider_oauth::preset_for_vendor(vendor) else {
        return metadata;
    };
    metadata["oauth"] = json!({
        "token_url": preset.token_url,
        "client_id": preset.client_id,
        "scopes": preset.refresh_scopes,
        "token_request_style": match preset.refresh_request_style {
            tiygate_core::provider::oauth::TokenRequestStyle::Form => "form",
            tiygate_core::provider::oauth::TokenRequestStyle::Json => "json",
        },
    });
    metadata
}

#[derive(Debug, Deserialize)]
struct ListProvidersQuery {
    enabled: Option<bool>,
}

async fn list_providers(
    State(state): State<AdminState>,
    axum::extract::Query(q): axum::extract::Query<ListProvidersQuery>,
) -> Result<Response, AdminError> {
    let providers = state.store.list_providers().await?;
    let filtered: Vec<Provider> = match q.enabled {
        Some(e) => providers.into_iter().filter(|p| p.enabled == e).collect(),
        None => providers,
    };
    let views: Vec<ProviderView> = filtered.into_iter().map(Into::into).collect();
    Ok(Json(views).into_response())
}

async fn get_provider(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    let p = state
        .store
        .get_provider(&id)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("provider {id}")))?;
    Ok(Json(ProviderView::from(p)).into_response())
}

async fn create_provider(
    State(state): State<AdminState>,
    Json(req): Json<ProviderRequest>,
) -> Result<Response, AdminError> {
    let id = req.id.unwrap_or_else(|| Uuid::now_v7().to_string());
    let auth_mode = req
        .auth_mode
        .as_deref()
        .and_then(AuthMode::parse)
        .unwrap_or(AuthMode::ApiKey);
    validate_provider_auth_mode(&req.vendor, auth_mode)
        .map_err(|message| AdminError::BadRequest(message.to_string()))?;
    let api_base = normalized_api_base(&req.vendor, auth_mode, &req.api_base);
    let models_endpoint = normalized_models_endpoint(
        &req.vendor,
        auth_mode,
        req.models_endpoint.as_deref().unwrap_or(""),
        &api_base,
    );
    let metadata = normalized_provider_metadata(&req.vendor, auth_mode, req.metadata);
    let p = state
        .store
        .upsert_provider(
            &id,
            &req.name,
            &req.vendor,
            &api_base,
            &models_endpoint,
            req.api_key.as_deref(),
            auth_mode,
            req.oauth_meta.as_deref(),
            metadata,
            req.enabled.unwrap_or(true),
        )
        .await?;
    enqueue_provider_capability_jobs(&state, &p.id).await?;
    let snap = provider_snapshot(&p);
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "upsert",
        "provider",
        &p.id,
        &audit_details(None, Some(&snap)),
    )
    .await;
    Ok((StatusCode::CREATED, Json(ProviderView::from(p))).into_response())
}

async fn update_provider(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(req): Json<ProviderRequest>,
) -> Result<Response, AdminError> {
    let auth_mode = req
        .auth_mode
        .as_deref()
        .and_then(AuthMode::parse)
        .unwrap_or(AuthMode::ApiKey);
    validate_provider_auth_mode(&req.vendor, auth_mode)
        .map_err(|message| AdminError::BadRequest(message.to_string()))?;
    let api_base = normalized_api_base(&req.vendor, auth_mode, &req.api_base);
    let models_endpoint = normalized_models_endpoint(
        &req.vendor,
        auth_mode,
        req.models_endpoint.as_deref().unwrap_or(""),
        &api_base,
    );
    let metadata = normalized_provider_metadata(&req.vendor, auth_mode, req.metadata);
    // This read now also decides whether OAuth refresh coordination is needed,
    // so a database error must not be treated as a missing provider.
    let before_provider = state.store.get_provider(&id).await?;
    let before = before_provider.as_ref().map(provider_snapshot);
    let credential_changed = before_provider.as_ref().is_some_and(|previous| {
        previous.auth_mode != auth_mode
            || previous.vendor != req.vendor
            || previous.metadata_json.get("oauth") != metadata.get("oauth")
            || req.oauth_meta.is_some()
    });

    let p = if credential_changed {
        if let Some(service) = state.oauth_service.as_ref().cloned() {
            let store = state.store.clone();
            let mutation_id = id.clone();
            let name = req.name.clone();
            let vendor = req.vendor.clone();
            let api_base = api_base.clone();
            let models_endpoint = models_endpoint.clone();
            let api_key = req.api_key.clone();
            let oauth_meta = req.oauth_meta.clone();
            let metadata = metadata.clone();
            let enabled = req.enabled.unwrap_or(true);
            service
                .mutate_provider_credentials(
                    &id,
                    Box::new(move || {
                        Box::pin(async move {
                            store
                                .upsert_provider(
                                    &mutation_id,
                                    &name,
                                    &vendor,
                                    &api_base,
                                    &models_endpoint,
                                    api_key.as_deref(),
                                    auth_mode,
                                    oauth_meta.as_deref(),
                                    metadata,
                                    enabled,
                                )
                                .await
                                .map(|_| ())
                                .map_err(|error| error.to_string())
                        })
                    }),
                )
                .await
                .map_err(AdminError::Internal)?;
            state
                .store
                .get_provider(&id)
                .await?
                .ok_or_else(|| AdminError::NotFound(format!("provider {id}")))?
        } else {
            let provider = state
                .store
                .upsert_provider(
                    &id,
                    &req.name,
                    &req.vendor,
                    &api_base,
                    &models_endpoint,
                    req.api_key.as_deref(),
                    auth_mode,
                    req.oauth_meta.as_deref(),
                    metadata,
                    req.enabled.unwrap_or(true),
                )
                .await?;
            state
                .store
                .oauth_token_store()
                .reset(&id, req.oauth_meta.as_deref())
                .await?;
            state.store.refresh().await?;
            provider
        }
    } else {
        state
            .store
            .upsert_provider(
                &id,
                &req.name,
                &req.vendor,
                &api_base,
                &models_endpoint,
                req.api_key.as_deref(),
                auth_mode,
                req.oauth_meta.as_deref(),
                metadata,
                req.enabled.unwrap_or(true),
            )
            .await?
    };
    enqueue_provider_capability_jobs(&state, &p.id).await?;
    let snap = provider_snapshot(&p);
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "upsert",
        "provider",
        &p.id,
        &audit_details(before.as_ref(), Some(&snap)),
    )
    .await;
    Ok(Json(ProviderView::from(p)).into_response())
}

async fn provider_delete_impact(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    let impact = state.store.provider_route_impact(&id).await?;
    Ok(Json(impact).into_response())
}

async fn delete_provider(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    let before = state
        .store
        .get_provider(&id)
        .await
        .ok()
        .flatten()
        .map(|p| provider_snapshot(&p));
    let outcome = state
        .store
        .delete_provider_cascade_route_targets(&id)
        .await?;
    let mut details = audit_details(before.as_ref(), None);
    if let serde_json::Value::Object(ref mut obj) = details {
        obj.insert(
            "route_target_cleanup".to_string(),
            serde_json::json!({
                "provider_id": outcome.impact.provider_id,
                "route_count": outcome.impact.route_count,
                "target_count": outcome.impact.target_count,
                "delete_route_count": outcome.impact.delete_route_count,
                "routes": outcome.impact.routes,
            }),
        );
    }
    let mut route_audit_records = Vec::new();
    for cleanup in &outcome.route_cleanups {
        let before = route_snapshot(&cleanup.before);
        let after = cleanup.after.as_ref().map(route_snapshot);
        let action = if after.is_some() { "upsert" } else { "delete" };
        let details = audit_details(Some(&before), after.as_ref());
        route_audit_records.push((action, cleanup.before.id.clone(), details));
    }
    for (action, route_id, details) in route_audit_records {
        let _ = tiygate_store::audit::record(
            state.pool.as_ref(),
            "admin",
            action,
            "route",
            &route_id,
            &details,
        )
        .await;
    }
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "delete",
        "provider",
        &id,
        &details,
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---- model catalog ----

#[derive(Debug, Serialize)]
struct ModelCatalogStatus {
    source: String,
    checksum: String,
    generated_at_unix: i64,
    provider_count: usize,
    model_count: usize,
}

async fn get_model_catalog(State(state): State<AdminState>) -> Result<Response, AdminError> {
    let catalog = state
        .model_catalog
        .as_ref()
        .ok_or_else(|| AdminError::NotFound("model catalog not available".to_string()))?;
    let version = catalog.current_version();
    Ok(Json(ModelCatalogStatus {
        source: version.source,
        checksum: version.checksum,
        generated_at_unix: version.generated_at_unix,
        provider_count: version.provider_count,
        model_count: version.model_count,
    })
    .into_response())
}

async fn refresh_model_catalog(State(state): State<AdminState>) -> Result<Response, AdminError> {
    let catalog = state
        .model_catalog
        .as_ref()
        .ok_or_else(|| AdminError::NotFound("model catalog not available".to_string()))?;
    let version = catalog
        .refresh_async()
        .await
        .map_err(|e| AdminError::Internal(format!("model catalog refresh failed: {e}")))?;
    Ok((
        StatusCode::ACCEPTED,
        Json(ModelCatalogStatus {
            source: version.source,
            checksum: version.checksum,
            generated_at_unix: version.generated_at_unix,
            provider_count: version.provider_count,
            model_count: version.model_count,
        }),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
struct ModelCatalogResolveRequest {
    virtual_model: String,
    #[serde(default)]
    target_model_id: Option<String>,
}

async fn resolve_model_catalog_metadata(
    State(state): State<AdminState>,
    Json(req): Json<ModelCatalogResolveRequest>,
) -> Result<Response, AdminError> {
    let catalog = state
        .model_catalog
        .as_ref()
        .ok_or_else(|| AdminError::NotFound("model catalog disabled".into()))?
        .snapshot();
    if let Some(meta) = catalog.get_model(&req.virtual_model) {
        return Ok(Json(meta.clone()).into_response());
    }
    if let Some(target_model_id) = req
        .target_model_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())
    {
        if let Some(meta) = catalog.get_model(target_model_id) {
            return Ok(Json(meta.clone()).into_response());
        }
    }
    Err(AdminError::NotFound(format!(
        "model metadata for {}",
        req.virtual_model
    )))
}

// ---- provider catalog (server-side registered providers) ----

/// One entry of the server-side provider catalog. Unlike
/// [`ProviderView`] (which describes a *configured* DB provider row),
/// this describes a provider that is *registered and compiled into the
/// binary* via `inventory`. The set therefore reflects the active
/// feature flags / linked crates at build time.
#[derive(Debug, Serialize)]
struct ProviderCatalogEntry {
    /// Registration id (e.g. "openai"); used as the `vendor` value when
    /// creating a DB provider.
    id: String,
    /// Human-readable name from the provider metadata.
    display_name: String,
    /// Default base URL the provider ships with.
    default_base_url: String,
    /// Normalized auth mode, aligned with the DB-layer `auth_mode`
    /// values the UI uses (api_key | oauth | iam).
    auth_mode: String,
}

/// Normalize the core [`tiygate_core::provider::AuthMode`] enum into the
/// DB-layer `auth_mode` string the UI understands. This is intentionally
/// lossy (5 core variants → 3 UI values); it only drives the create-form
/// default, which the operator can still override.
fn map_auth_mode(mode: &tiygate_core::provider::AuthMode) -> &'static str {
    use tiygate_core::provider::AuthMode;
    match mode {
        AuthMode::Bearer | AuthMode::ApiKey { .. } | AuthMode::Custom => "api_key",
        AuthMode::OAuth2 => "oauth",
        AuthMode::AwsSigV4 => "iam",
    }
}

/// GET /admin/v1/provider-catalog — the read-only catalog of providers
/// the gateway supports, derived at runtime from the `inventory`
/// registry. No store access or side effects.
async fn list_provider_catalog() -> Result<Response, AdminError> {
    let mut entries: Vec<ProviderCatalogEntry> = tiygate_core::provider::all_providers()
        .iter()
        .map(|p| {
            let m = p.metadata();
            ProviderCatalogEntry {
                id: p.id().to_string(),
                display_name: m.display_name.clone(),
                default_base_url: m.base_url.clone(),
                auth_mode: map_auth_mode(&m.auth_mode).to_string(),
            }
        })
        .collect();
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(entries).into_response())
}

// ---- routes ----

#[derive(Debug, Serialize, Deserialize)]
struct RouteRequest {
    id: Option<String>,
    virtual_model: String,
    targets: Vec<RouteTarget>,
    #[serde(default)]
    routing_strategy: Option<tiygate_core::routing::RoutingStrategyName>,
    #[serde(default)]
    capability_routing_mode: Option<tiygate_core::CapabilityRoutingMode>,
    #[serde(default)]
    model_metadata: Option<ModelMetadata>,
    enabled: Option<bool>,
}

#[derive(Debug, Serialize)]
struct RouteView {
    id: String,
    virtual_model: String,
    targets: Vec<RouteTargetView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    routing_strategy: Option<tiygate_core::routing::RoutingStrategyName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capability_routing_mode: Option<tiygate_core::CapabilityRoutingMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_metadata: Option<ModelMetadata>,
    enabled: bool,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

/// Route target representation safe for the Admin read surface. Secrets and
/// URL override values are accepted on writes but never echoed back; only
/// presence flags are returned so the UI can explain that an override exists.
#[derive(Debug, Serialize)]
struct RouteTargetView {
    provider_id: String,
    model_id: String,
    weight: f64,
    enabled: bool,
    account_label_present: bool,
    api_key_override_configured: bool,
    api_base_override_configured: bool,
    egress_dialect_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile_status: Option<tiygate_store::capabilities::ProfileStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    probe_job_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    capability_summary: Option<Value>,
}

impl From<Route> for RouteView {
    fn from(r: Route) -> Self {
        Self {
            id: r.id,
            virtual_model: r.virtual_model,
            targets: r
                .targets
                .into_iter()
                .map(|target| RouteTargetView {
                    provider_id: target.provider_id,
                    model_id: target.model_id,
                    weight: target.weight,
                    enabled: target.enabled,
                    account_label_present: target.account_label.is_some(),
                    api_key_override_configured: target.api_key_override.is_some(),
                    api_base_override_configured: target.api_base_override.is_some(),
                    egress_dialect_id: target.egress_dialect_id,
                    target_key: None,
                    profile_status: None,
                    probe_job_status: None,
                    capability_summary: None,
                })
                .collect(),
            routing_strategy: r.routing_strategy,
            capability_routing_mode: r.capability_routing_mode,
            model_metadata: r.model_metadata,
            enabled: r.enabled,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

async fn enrich_route_view(state: &AdminState, route: &Route, view: &mut RouteView) {
    let runtime_targets = state
        .store
        .config_store()
        .routing_table
        .resolve(&route.virtual_model)
        .unwrap_or_default();
    let mut used_runtime_targets = std::collections::HashSet::new();
    for target_view in &mut view.targets {
        let Some((runtime_index, target)) =
            runtime_targets.iter().enumerate().find(|(index, target)| {
                !used_runtime_targets.contains(index)
                    && target.provider_id == target_view.provider_id
                    && target.model_id == target_view.model_id
                    && target.effective_egress_dialect_id()
                        == target_view.egress_dialect_id.as_deref().unwrap_or("auto")
                    && target.account_label.as_ref().is_some_and(|account| {
                        target_view.account_label_present && !account.is_empty()
                    }) == target_view.account_label_present
            })
        else {
            continue;
        };
        used_runtime_targets.insert(runtime_index);
        let Ok((key, _)) = state.store.target_key_for(target) else {
            continue;
        };
        target_view.target_key = Some(key.0.clone());
        if let Ok(Some(profile)) = state.store.get_capability_profile(&key).await {
            let summary = tiygate_store::capabilities::CapabilityProfileSummary::from(&profile);
            target_view.profile_status = Some(summary.profile_status);
            target_view.capability_summary = Some(json!({
                "supported": summary.supported,
                "unsupported": summary.unsupported,
                "constrained": summary.constrained,
                "unknown": summary.unknown,
                "fresh_until": summary.fresh_until,
                "stale_until": summary.stale_until
            }));
        }
        if let Ok(Some(job)) = state.store.latest_probe_job_for_target(&key).await {
            target_view.probe_job_status = Some(job.status);
        }
    }
}

/// Query parameters for `GET /admin/v1/routes` (paginated list).
#[derive(Debug, Deserialize)]
struct RouteListQuery {
    limit: Option<u32>,
    offset: Option<u32>,
}

/// Enqueue the minimal capability bundle for every enabled runtime Target in
/// a route. The DB-backed profile/job methods are idempotent, so route updates
/// and repeated refreshes do not create duplicate work.
async fn enqueue_route_capability_jobs(
    state: &AdminState,
    route: &Route,
) -> Result<(), AdminError> {
    let runtime = state.store.config_store();
    let Some(targets) = runtime.routing_table.resolve(&route.virtual_model) else {
        return Ok(());
    };
    for target in targets {
        let probe_set = tiygate_store::capabilities::default_probe_set_for_target(&target);
        state
            .store
            .ensure_target_capability(&target, &probe_set)
            .await?;
    }
    Ok(())
}

async fn enqueue_provider_capability_jobs(
    state: &AdminState,
    provider_id: &str,
) -> Result<(), AdminError> {
    let (routes, _) = state.store.list_routes_paginated(500, 0).await?;
    for route in routes {
        if route
            .targets
            .iter()
            .any(|target| target.provider_id == provider_id)
        {
            enqueue_route_capability_jobs(state, &route).await?;
        }
    }
    Ok(())
}

async fn list_routes(
    State(state): State<AdminState>,
    axum::extract::Query(q): axum::extract::Query<RouteListQuery>,
) -> Result<Response, AdminError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0);
    let (routes, total) = state.store.list_routes_paginated(limit, offset).await?;
    let mut entries = Vec::with_capacity(routes.len());
    for route in routes {
        let mut view = RouteView::from(route.clone());
        enrich_route_view(&state, &route, &mut view).await;
        entries.push(view);
    }
    let next_cursor = (offset.saturating_add(entries.len() as u32) < total as u32)
        .then(|| offset.saturating_add(entries.len() as u32).to_string());
    Ok(Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "next_cursor": next_cursor,
        "items": entries,
        "entries": entries
    }))
    .into_response())
}

async fn get_route(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    let r = state
        .store
        .get_route(&id)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("route {id}")))?;
    let mut view = RouteView::from(r.clone());
    enrich_route_view(&state, &r, &mut view).await;
    Ok(Json(view).into_response())
}

/// Best-effort initialization of virtual-model metadata for `create_route`
/// and `update_route`. When the caller doesn't submit `model_metadata`,
/// this mirrors the lookup behind `POST /admin/v1/model-catalog/resolve`:
/// it first tries an exact match on `virtual_model` in the runtime model
/// catalog, then falls back to the first configured target's `model_id`.
///
/// Failure to find a match — or the model catalog being disabled — is
/// intentionally non-fatal: it only logs a `warn` and returns `None`, so
/// `create_route`/`update_route` still succeed with metadata left unset
/// (the data-plane `/v1/models` handler already falls back to the runtime
/// catalog in that case).
fn auto_resolve_model_metadata(
    state: &AdminState,
    virtual_model: &str,
    targets: &[RouteTarget],
) -> Option<ModelMetadata> {
    let catalog = match state.model_catalog.as_ref() {
        Some(c) => c.snapshot(),
        None => {
            tracing::warn!(
                virtual_model,
                "route model_metadata auto-init skipped: model catalog disabled"
            );
            return None;
        }
    };
    if let Some(meta) = catalog.get_model(virtual_model) {
        return Some(meta.clone());
    }
    if let Some(target_model_id) = targets
        .iter()
        .map(|t| t.model_id.as_str())
        .find(|id| !id.trim().is_empty())
    {
        if let Some(meta) = catalog.get_model(target_model_id) {
            return Some(meta.clone());
        }
    }
    tracing::warn!(
        virtual_model,
        "route model_metadata auto-init skipped: no catalog match for virtual model or target"
    );
    None
}

async fn create_route(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(req): Json<RouteRequest>,
) -> Result<Response, AdminError> {
    let id = req.id.clone().unwrap_or_else(|| Uuid::now_v7().to_string());
    let idempotency_payload = json!({
        "route_id": id,
        "request": serde_json::to_value(&req).map_err(|error| {
            AdminError::Internal(format!("serialize route request: {error}"))
        })?
    });
    let reservation =
        begin_capability_idempotency(&state, "route_create", &headers, &idempotency_payload)
            .await?;
    if let Some((_, _, Some(response))) = reservation {
        return Ok(response);
    }
    let reservation = reservation.map(|(key, hash, _)| (key, hash));
    if let Err(error) = ensure_route_mode_allowed(&state, &id, req.capability_routing_mode).await {
        if let Some((key, hash)) = reservation.as_ref() {
            let _ = state
                .store
                .release_capability_mutation("route_create", key, hash)
                .await;
        }
        return Err(error);
    }
    let model_metadata = match req.model_metadata {
        Some(m) => Some(m),
        None => auto_resolve_model_metadata(&state, &req.virtual_model, &req.targets),
    };
    let audit_after = json!({
        "id": id,
        "virtual_model": req.virtual_model,
        "target_count": req.targets.len(),
        "routing_strategy": req.routing_strategy,
        "capability_routing_mode": req.capability_routing_mode,
        "enabled": req.enabled.unwrap_or(true),
    });
    let audit = audit_details(None, Some(&audit_after));
    let r = match state
        .store
        .upsert_route_with_mode_with_audit(
            tiygate_store::config_store::RouteUpsert {
                id: &id,
                virtual_model: &req.virtual_model,
                targets: &req.targets,
                routing_strategy: req.routing_strategy,
                capability_routing_mode: req.capability_routing_mode,
                model_metadata: model_metadata.as_ref(),
                enabled: req.enabled.unwrap_or(true),
            },
            "admin",
            "upsert",
            &id,
            &audit,
        )
        .await
    {
        Ok(route) => route,
        Err(error) => {
            if let Some((key, hash)) = reservation.as_ref() {
                let _ = state
                    .store
                    .release_capability_mutation("route_create", key, hash)
                    .await;
            }
            return Err(error.into());
        }
    };
    let mut view = RouteView::from(r.clone());
    enrich_route_view(&state, &r, &mut view).await;
    if let Some((key, hash)) = reservation {
        let response = serde_json::to_value(&view)
            .map_err(|error| AdminError::Internal(format!("serialize route response: {error}")))?;
        state
            .store
            .complete_capability_mutation(
                "route_create",
                &key,
                &hash,
                StatusCode::CREATED.as_u16(),
                &response,
            )
            .await?;
    }
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

async fn update_route(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<RouteRequest>,
) -> Result<Response, AdminError> {
    let idempotency_payload = json!({
        "route_id": id,
        "request": serde_json::to_value(&req).map_err(|error| {
            AdminError::Internal(format!("serialize route request: {error}"))
        })?
    });
    let reservation =
        begin_capability_idempotency(&state, "route_update", &headers, &idempotency_payload)
            .await?;
    if let Some((_, _, Some(response))) = reservation {
        return Ok(response);
    }
    let reservation = reservation.map(|(key, hash, _)| (key, hash));
    let existing = match state.store.get_route(&id).await {
        Ok(route) => route,
        Err(error) => {
            if let Some((key, hash)) = reservation.as_ref() {
                let _ = state
                    .store
                    .release_capability_mutation("route_update", key, hash)
                    .await;
            }
            return Err(error.into());
        }
    };
    if let Err(error) = ensure_route_mode_allowed(&state, &id, req.capability_routing_mode).await {
        if let Some((key, hash)) = reservation.as_ref() {
            let _ = state
                .store
                .release_capability_mutation("route_update", key, hash)
                .await;
        }
        return Err(error);
    }
    let before = existing.as_ref().map(route_snapshot);
    let targets = preserve_route_target_secrets(existing.as_ref(), req.targets);
    let model_metadata = match req.model_metadata {
        Some(m) => Some(m),
        None => auto_resolve_model_metadata(&state, &req.virtual_model, &targets),
    };
    let audit_after = json!({
        "id": id,
        "virtual_model": req.virtual_model,
        "target_count": targets.len(),
        "routing_strategy": req.routing_strategy,
        "capability_routing_mode": req.capability_routing_mode,
        "enabled": req.enabled.unwrap_or(true),
    });
    let audit = audit_details(before.as_ref(), Some(&audit_after));
    let r = match state
        .store
        .upsert_route_with_mode_with_audit(
            tiygate_store::config_store::RouteUpsert {
                id: &id,
                virtual_model: &req.virtual_model,
                targets: &targets,
                routing_strategy: req.routing_strategy,
                capability_routing_mode: req.capability_routing_mode,
                model_metadata: model_metadata.as_ref(),
                enabled: req.enabled.unwrap_or(true),
            },
            "admin",
            "upsert",
            &id,
            &audit,
        )
        .await
    {
        Ok(route) => route,
        Err(error) => {
            if let Some((key, hash)) = reservation.as_ref() {
                let _ = state
                    .store
                    .release_capability_mutation("route_update", key, hash)
                    .await;
            }
            return Err(error.into());
        }
    };
    let mut view = RouteView::from(r.clone());
    enrich_route_view(&state, &r, &mut view).await;
    if let Some((key, hash)) = reservation {
        let response = serde_json::to_value(&view)
            .map_err(|error| AdminError::Internal(format!("serialize route response: {error}")))?;
        state
            .store
            .complete_capability_mutation(
                "route_update",
                &key,
                &hash,
                StatusCode::OK.as_u16(),
                &response,
            )
            .await?;
    }
    Ok(Json(view).into_response())
}

async fn ensure_route_mode_allowed(
    state: &AdminState,
    route_id: &str,
    mode: Option<tiygate_core::CapabilityRoutingMode>,
) -> Result<(), AdminError> {
    if mode != Some(tiygate_core::CapabilityRoutingMode::Enforce) {
        return Ok(());
    }
    let admissions = state
        .store
        .list_capability_route_admissions(route_id, 500, 0)
        .await?;
    if admissions.into_iter().any(is_valid_enforce_admission) {
        return Ok(());
    }
    Err(AdminError::AdmissionRequired(
        "route enforce requires at least one valid capability-shape admission".to_string(),
    ))
}

fn is_valid_enforce_admission(
    admission: tiygate_store::capabilities::CapabilityRouteAdmission,
) -> bool {
    let requirements = if admission.required_requirements.is_empty() {
        admission
            .required_capabilities
            .iter()
            .cloned()
            .map(tiygate_core::CapabilityRequirement::required)
            .collect::<Vec<_>>()
    } else {
        admission.required_requirements.clone()
    };
    let legacy_shape = admission.required_requirements.is_empty();
    admission.mode == tiygate_core::CapabilityRoutingMode::Enforce
        && admission.gate_policy_version == 1
        && admission
            .expires_at
            .is_none_or(|expires_at| expires_at > chrono::Utc::now())
        && !admission
            .report
            .get("telemetry_gap")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        && tiygate_core::capability_shape_hash_from_requirements(&requirements)
            == admission.capability_shape_hash
        && (admission
            .report
            .get("registry_version")
            .and_then(Value::as_u64)
            .map_or(legacy_shape, |version| {
                version == u64::from(tiygate_store::capabilities::CAPABILITY_REGISTRY_VERSION)
            }))
        && (admission
            .report
            .get("baseline_version")
            .and_then(Value::as_u64)
            .map_or(legacy_shape, |version| {
                version == u64::from(tiygate_store::capabilities::CAPABILITY_BASELINE_VERSION)
            }))
        && (admission
            .report
            .get("shape_hash_version")
            .and_then(Value::as_str)
            .map_or(legacy_shape, |version| {
                version == tiygate_core::CAPABILITY_SHAPE_HASH_VERSION
            }))
        && requirements.iter().all(|requirement| {
            tiygate_protocols::capabilities::enforce_eligible_ids()
                .contains(&requirement.id.as_str())
        })
        && (admission
            .report
            .get("gate_passed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || admission
                .report
                .get("gate_passed_by_exception")
                .and_then(Value::as_bool)
                .unwrap_or(false))
}

fn preserve_route_target_secrets(
    existing: Option<&Route>,
    mut requested: Vec<RouteTarget>,
) -> Vec<RouteTarget> {
    let Some(existing) = existing else {
        return requested;
    };
    for (index, target) in requested.iter_mut().enumerate() {
        let previous = existing
            .targets
            .iter()
            .find(|candidate| {
                candidate.provider_id == target.provider_id
                    && candidate.model_id == target.model_id
                    && candidate.egress_dialect_id == target.egress_dialect_id
            })
            .or_else(|| existing.targets.get(index));
        let Some(previous) = previous else {
            continue;
        };
        if target.account_label.is_none() {
            target.account_label = previous.account_label.clone();
        }
        if target.api_key_override.is_none() {
            target.api_key_override = previous.api_key_override.clone();
        }
        if target.api_base_override.is_none() {
            target.api_base_override = previous.api_base_override.clone();
        }
    }
    requested
}

async fn delete_route(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    let reservation =
        begin_capability_idempotency(&state, "route_delete", &headers, &json!({"route_id": id}))
            .await?;
    if let Some((_, _, Some(response))) = reservation {
        return Ok(response);
    }
    let reservation = reservation.map(|(key, hash, _)| (key, hash));
    let before = state
        .store
        .get_route(&id)
        .await
        .ok()
        .flatten()
        .map(|r| route_snapshot(&r));
    let audit = audit_details(before.as_ref(), None);
    if let Err(error) = state
        .store
        .delete_route_with_audit(&id, "admin", &audit)
        .await
    {
        if let Some((key, hash)) = reservation.as_ref() {
            let _ = state
                .store
                .release_capability_mutation("route_delete", key, hash)
                .await;
        }
        return Err(error.into());
    }
    if let Some((key, hash)) = reservation {
        state
            .store
            .complete_capability_mutation(
                "route_delete",
                &key,
                &hash,
                StatusCode::NO_CONTENT.as_u16(),
                &json!({}),
            )
            .await?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---- api keys ----

#[derive(Debug, Deserialize)]
struct CreateApiKeyRequest {
    name: String,
    /// Optional explicit secret; if absent we generate a random one.
    secret: Option<String>,
    /// Optional quota (forwarded to the column as JSON).
    quota: Option<serde_json::Value>,
    /// Optional exact virtual-model allow-list. Omitted/null means
    /// unrestricted; an empty list denies every model.
    allowed_models: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
struct CreateApiKeyResponse {
    id: String,
    name: String,
    secret: String,
    quota: serde_json::Value,
    allowed_models: Option<Vec<String>>,
    status: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
struct ApiKeyView {
    id: String,
    name: String,
    key_hash: String,
    quota: serde_json::Value,
    allowed_models: Option<Vec<String>>,
    status: String,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<tiygate_store::models::ApiKey> for ApiKeyView {
    fn from(k: tiygate_store::models::ApiKey) -> Self {
        Self {
            id: k.id,
            name: k.name,
            key_hash: k.key_hash,
            quota: k.quota_json,
            allowed_models: k.allowed_models,
            status: k.status.as_str().to_string(),
            created_at: k.created_at,
            updated_at: k.updated_at,
        }
    }
}

async fn list_api_keys(State(state): State<AdminState>) -> Result<Response, AdminError> {
    let keys = state.store.list_api_keys().await?;
    let views: Vec<ApiKeyView> = keys.into_iter().map(Into::into).collect();
    Ok(Json(views).into_response())
}

async fn create_api_key(
    State(state): State<AdminState>,
    Json(req): Json<CreateApiKeyRequest>,
) -> Result<Response, AdminError> {
    let secret = req.secret.unwrap_or_else(|| {
        // 32 random bytes → hex (64 chars). Plenty for a non-jwt
        // gateway secret; entropy is the same as the embedded
        // SHA-256 hash.
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        format!("tg-{}", hex::encode(bytes))
    });
    let (key, plain) = state
        .store
        .create_api_key(
            &req.name,
            &secret,
            req.quota.unwrap_or_else(|| serde_json::json!({})),
            req.allowed_models,
        )
        .await?;
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "create",
        "api_key",
        &key.id,
        &audit_details(None, Some(&api_key_snapshot(&key))),
    )
    .await;
    let resp = CreateApiKeyResponse {
        id: key.id,
        name: key.name,
        secret: plain,
        quota: key.quota_json,
        allowed_models: key.allowed_models,
        status: key.status.as_str().to_string(),
        created_at: key.created_at,
    };
    Ok((StatusCode::CREATED, Json(resp)).into_response())
}

async fn delete_api_key(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    let before = state
        .store
        .get_api_key(&id)
        .await
        .ok()
        .flatten()
        .map(|k| api_key_snapshot(&k));
    state.store.delete_api_key(&id).await?;
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "delete",
        "api_key",
        &id,
        &audit_details(before.as_ref(), None),
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn disable_api_key(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    let before = state
        .store
        .get_api_key(&id)
        .await
        .ok()
        .flatten()
        .map(|k| api_key_snapshot(&k));
    state.store.disable_api_key(&id).await?;
    // Record the status transition by diffing the post-disable snapshot
    // against the pre-disable one when available.
    let after = state
        .store
        .get_api_key(&id)
        .await
        .ok()
        .flatten()
        .map(|k| api_key_snapshot(&k));
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "disable",
        "api_key",
        &id,
        &audit_details(before.as_ref(), after.as_ref()),
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Single-key GET. Returns the key's metadata plus, when a live
/// quota counter is wired in, its real-time usage per bucket
/// (`requests_per_minute`, `requests_per_day`, ...). When no quota
/// backend is available the `usage` map is empty.
#[derive(Debug, Serialize)]
struct ApiKeyDetailView {
    #[serde(flatten)]
    key: ApiKeyView,
    usage: serde_json::Value,
}

async fn get_api_key(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    let key = state
        .store
        .get_api_key(&id)
        .await?
        .ok_or_else(|| AdminError::NotFound(format!("api key {id}")))?;
    let usage = match &state.quota {
        Some(counter) => match counter.current_usage(&key.id).await {
            Ok(map) => {
                let mut obj = serde_json::Map::new();
                for (kind, used) in map {
                    obj.insert(quota_kind_key(kind).to_string(), json!(used));
                }
                serde_json::Value::Object(obj)
            }
            Err(_) => json!({}),
        },
        None => json!({}),
    };
    let view = ApiKeyDetailView {
        key: ApiKeyView::from(key),
        usage,
    };
    Ok(Json(view).into_response())
}

/// Maps a [`tiygate_core::quota::QuotaKind`] to the JSON field name
/// used by [`tiygate_core::quota::QuotaSpec`], so the usage map keys
/// line up with the quota spec keys the UI edits.
fn quota_kind_key(kind: tiygate_core::quota::QuotaKind) -> &'static str {
    use tiygate_core::quota::QuotaKind;
    match kind {
        QuotaKind::RequestsPerMinute => "requests_per_minute",
        QuotaKind::RequestsPerDay => "requests_per_day",
        QuotaKind::TokensPerMinute => "tokens_per_minute",
        QuotaKind::TokensPerDay => "tokens_per_day",
    }
}

#[derive(Debug, Deserialize)]
struct UpdateQuotaRequest {
    quota: serde_json::Value,
}

/// PATCH /admin/v1/api-keys/:id — update the quota JSON only. This
/// is deliberately separate from the PUT verb (which disables the
/// key) so the two operations never collide.
async fn update_api_key_quota(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateQuotaRequest>,
) -> Result<Response, AdminError> {
    let before = state
        .store
        .get_api_key(&id)
        .await
        .ok()
        .flatten()
        .map(|k| api_key_snapshot(&k));
    let key = state.store.update_api_key_quota(&id, req.quota).await?;
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "update_quota",
        "api_key",
        &key.id,
        &audit_details(before.as_ref(), Some(&api_key_snapshot(&key))),
    )
    .await;
    Ok(Json(ApiKeyView::from(key)).into_response())
}

/// PATCH /admin/v1/api-keys/:id/model-access — replace the key's exact
/// virtual-model allow-list without changing its quota or status.
async fn update_api_key_model_access(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(mut req): Json<serde_json::Map<String, serde_json::Value>>,
) -> Result<Response, AdminError> {
    let raw_allowed_models = req.remove("allowed_models").ok_or_else(|| {
        AdminError::BadRequest(
            "allowed_models is required; use null for unrestricted access".into(),
        )
    })?;
    let allowed_models = if raw_allowed_models.is_null() {
        None
    } else {
        Some(
            serde_json::from_value::<Vec<String>>(raw_allowed_models).map_err(|_| {
                AdminError::BadRequest("allowed_models must be an array or null".into())
            })?,
        )
    };
    let before = state
        .store
        .get_api_key(&id)
        .await
        .ok()
        .flatten()
        .map(|k| api_key_snapshot(&k));
    let key = state
        .store
        .update_api_key_model_access(&id, allowed_models)
        .await?;
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "update_model_access",
        "api_key",
        &key.id,
        &audit_details(before.as_ref(), Some(&api_key_snapshot(&key))),
    )
    .await;
    Ok(Json(ApiKeyView::from(key)).into_response())
}

// ---- stats ----

#[derive(Debug, Deserialize)]
struct StatsQuery {
    /// RFC-3339 timestamp. Defaults to 24h ago.
    since: Option<String>,
    /// RFC-3339 timestamp. Defaults to now.
    until: Option<String>,
}

async fn stats_by_model(
    State(state): State<AdminState>,
    axum::extract::Query(q): axum::extract::Query<StatsQuery>,
) -> Result<Response, AdminError> {
    let now = chrono::Utc::now();
    let since = q
        .since
        .unwrap_or_else(|| (now - chrono::Duration::hours(24)).to_rfc3339());
    let until = q.until.unwrap_or_else(|| now.to_rfc3339());
    let rows = match tiygate_store::log_sink::oltp::aggregate_by_model(
        state.pool.as_ref(),
        &since,
        &until,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return Err(AdminError::Db(e)),
    };
    Ok(Json(json!({"since": since, "until": until, "buckets": rows})).into_response())
}

async fn stats_by_provider(
    State(state): State<AdminState>,
    axum::extract::Query(q): axum::extract::Query<StatsQuery>,
) -> Result<Response, AdminError> {
    let now = chrono::Utc::now();
    let since = q
        .since
        .unwrap_or_else(|| (now - chrono::Duration::hours(24)).to_rfc3339());
    let until = q.until.unwrap_or_else(|| now.to_rfc3339());
    let rows = match tiygate_store::log_sink::oltp::aggregate_by_provider(
        state.pool.as_ref(),
        &since,
        &until,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return Err(AdminError::Db(e)),
    };
    Ok(Json(json!({"since": since, "until": until, "buckets": rows})).into_response())
}

async fn stats_by_api_key(
    State(state): State<AdminState>,
    axum::extract::Query(q): axum::extract::Query<StatsQuery>,
) -> Result<Response, AdminError> {
    let now = chrono::Utc::now();
    let since = q
        .since
        .unwrap_or_else(|| (now - chrono::Duration::hours(24)).to_rfc3339());
    let until = q.until.unwrap_or_else(|| now.to_rfc3339());
    let rows = match tiygate_store::log_sink::oltp::aggregate_by_api_key(
        state.pool.as_ref(),
        &since,
        &until,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return Err(AdminError::Db(e)),
    };
    Ok(Json(json!({"since": since, "until": until, "buckets": rows})).into_response())
}

async fn stats_by_target(
    State(state): State<AdminState>,
    axum::extract::Query(q): axum::extract::Query<StatsQuery>,
) -> Result<Response, AdminError> {
    let now = chrono::Utc::now();
    let since = q
        .since
        .unwrap_or_else(|| (now - chrono::Duration::hours(24)).to_rfc3339());
    let until = q.until.unwrap_or_else(|| now.to_rfc3339());
    let rows = match tiygate_store::log_sink::oltp::aggregate_by_target(
        state.pool.as_ref(),
        &since,
        &until,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return Err(AdminError::Db(e)),
    };
    Ok(Json(json!({"since": since, "until": until, "buckets": rows})).into_response())
}

// ---- token stats (pre-aggregated) ----

#[derive(Debug, Deserialize)]
struct TokenActivityQuery {
    /// Number of days to return (default 365).
    days: Option<u32>,
}

async fn stats_token_activity(
    State(state): State<AdminState>,
    axum::extract::Query(q): axum::extract::Query<TokenActivityQuery>,
) -> Result<Response, AdminError> {
    let days = q.days.unwrap_or(365).clamp(1, 730);
    let activity =
        match tiygate_store::token_stats::get_token_activity(state.pool.as_ref(), days).await {
            Ok(v) => v,
            Err(e) => return Err(AdminError::Db(e)),
        };
    Ok(Json(json!({"days": activity})).into_response())
}

async fn stats_token_summary(State(state): State<AdminState>) -> Result<Response, AdminError> {
    let summary = match tiygate_store::token_stats::get_token_summary(state.pool.as_ref()).await {
        Ok(v) => v,
        Err(e) => return Err(AdminError::Db(e)),
    };
    Ok(Json(summary).into_response())
}

// ---- audit ----

async fn list_audit(
    State(state): State<AdminState>,
    axum::extract::Query(q): axum::extract::Query<AuditQuery>,
) -> Result<Response, AdminError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0).max(0);
    let (entries, total) =
        match tiygate_store::audit::list_page(state.pool.as_ref(), limit, offset).await {
            Ok(v) => v,
            Err(e) => return Err(AdminError::Internal(e.to_string())),
        };
    Ok(Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
        "entries": entries
    }))
    .into_response())
}

#[derive(Debug, Deserialize)]
struct AuditQuery {
    limit: Option<i64>,
    offset: Option<i64>,
}

// ---- request drill-down & replay (§4.4 / §8 acceptance #8) ----

#[derive(Debug, Deserialize)]
struct RequestListQuery {
    request_id: Option<String>,
    since: Option<String>,
    until: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    status: Option<String>,
    error_class: Option<String>,
    min_latency_ms: Option<u64>,
    max_latency_ms: Option<u64>,
    limit: Option<u32>,
    offset: Option<u32>,
}

async fn list_requests(
    State(state): State<AdminState>,
    axum::extract::Query(q): axum::extract::Query<RequestListQuery>,
) -> Result<Response, AdminError> {
    // Normalise the error_class filter so legacy PascalCase values
    // (e.g. "RateLimited", "BadRequest") are mapped to the canonical
    // snake_case form stored in the DB. Without this, old filter URLs
    // or scripts would silently match nothing after the migration.
    let error_class = q
        .error_class
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(tiygate_core::telemetry::RequestErrorClass::parse_str)
        .map(|c| c.as_str().to_string());

    let filter = tiygate_store::log_sink::oltp::RequestFilter {
        request_id: q.request_id,
        since: q.since,
        until: q.until,
        model: q.model,
        provider: q.provider,
        status: q.status,
        error_class,
        min_latency_ms: q.min_latency_ms,
        max_latency_ms: q.max_latency_ms,
        limit: q.limit,
        offset: q.offset,
    };
    let (entries, total) =
        match tiygate_store::log_sink::oltp::list_requests(state.pool.as_ref(), &filter).await {
            Ok(v) => v,
            Err(e) => return Err(AdminError::Db(e)),
        };
    Ok(Json(json!({
        "total": total,
        "limit": filter.limit.unwrap_or(50),
        "offset": filter.offset.unwrap_or(0),
        "entries": entries
    }))
    .into_response())
}

async fn request_filter_options(
    State(state): State<AdminState>,
    axum::extract::Query(q): axum::extract::Query<RequestListQuery>,
) -> Result<Response, AdminError> {
    let filter = tiygate_store::log_sink::oltp::RequestFilter {
        request_id: None,
        since: q.since,
        until: q.until,
        model: None,
        provider: None,
        status: None,
        error_class: None,
        min_latency_ms: None,
        max_latency_ms: None,
        limit: None,
        offset: None,
    };
    let options =
        tiygate_store::log_sink::oltp::list_request_filter_options(state.pool.as_ref(), &filter)
            .await
            .map_err(AdminError::Db)?;
    Ok(Json(options).into_response())
}

async fn replay_request(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    let mut replay =
        match tiygate_store::log_sink::oltp::get_request_replay(state.pool.as_ref(), &id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Err(AdminError::NotFound(format!(
                    "request {id} not found in logs"
                )))
            }
            Err(e) => return Err(AdminError::Db(e)),
        };
    if replay.payload_archive_status.as_deref() == Some("uploaded") {
        hydrate_archived_replay(&mut replay, &state).await?;
    }
    refresh_replay_sse_parsed(&mut replay);
    Ok(Json(replay).into_response())
}

fn refresh_replay_sse_parsed(replay: &mut tiygate_store::log_sink::oltp::RequestReplay) {
    if !replay.is_stream {
        return;
    }
    if let Some(parsed) = replay
        .upstream_resp_body
        .as_deref()
        .and_then(tiygate_store::log_sink::oltp::parse_sse_to_json)
    {
        replay.sse_parsed_json = Some(parsed);
    }
    if let Some(parsed) = replay
        .client_resp_body
        .as_deref()
        .and_then(tiygate_store::log_sink::oltp::parse_sse_to_json)
    {
        replay.client_sse_parsed_json = Some(parsed);
    }
}

fn archived_json_field_text(text: &str, field: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let field_value = value.get(field)?;
    field_value
        .as_str()
        .map(ToString::to_string)
        .or_else(|| Some(field_value.to_string()))
}

fn archived_json_field_non_empty_text(text: &str, field: &str) -> Option<String> {
    archived_json_field_text(text, field).and_then(|value| {
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    })
}

fn archived_json_field_u16(text: &str, field: &str) -> Option<u16> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let field_value = value.get(field)?;
    field_value
        .as_u64()
        .and_then(|v| u16::try_from(v).ok())
        .or_else(|| field_value.as_str()?.parse::<u16>().ok())
}

async fn hydrate_archived_replay(
    replay: &mut tiygate_store::log_sink::oltp::RequestReplay,
    state: &AdminState,
) -> Result<(), AdminError> {
    let Some(client) = state.payload_archive.as_ref() else {
        return Err(AdminError::Internal(
            "payload archive is uploaded but archive client is not configured".to_string(),
        ));
    };
    let Some(raw_manifest) = replay.payload_archive_manifest_json.as_ref() else {
        return Err(AdminError::Internal(
            "payload archive is uploaded but manifest is missing".to_string(),
        ));
    };
    let manifest: PayloadArchiveManifest = serde_json::from_str(raw_manifest)
        .map_err(|e| AdminError::Internal(format!("invalid payload archive manifest: {e}")))?;
    for (kind, object) in &manifest.objects {
        let compressed = client
            .get_object(&object.key)
            .await
            .map_err(|e| AdminError::Internal(format!("payload archive read failed: {e}")))?;
        if compressed.len() != object.compressed_size {
            return Err(AdminError::Internal(format!(
                "payload archive compressed size mismatch for {}",
                object.key
            )));
        }
        let original = gzip_decompress(&compressed).map_err(|e| {
            AdminError::Internal(format!("payload archive gzip decode failed: {e}"))
        })?;
        if original.len() != object.original_size || sha256_hex(&original) != object.sha256_hex {
            return Err(AdminError::Internal(format!(
                "payload archive checksum mismatch for {}",
                object.key
            )));
        }
        let text = String::from_utf8(original).map_err(|e| {
            AdminError::Internal(format!("payload archive utf-8 decode failed: {e}"))
        })?;
        match kind.as_str() {
            "cg_req_raw" => replay.raw_envelope_json = Some(text),
            "cg_req_parsed" => {
                replay.redacted_headers_json = archived_json_field_text(&text, "headers")
            }
            "gp_req_raw" => replay.egress_body = Some(text),
            "gp_req_parsed" => {
                replay.egress_headers_json = archived_json_field_text(&text, "headers");
                replay.egress_method = archived_json_field_non_empty_text(&text, "method");
                replay.egress_path = archived_json_field_non_empty_text(&text, "path");
            }
            "pg_rsp_raw" => replay.upstream_resp_body = Some(text),
            "pg_rsp_parsed" => {
                replay.upstream_resp_headers_json = archived_json_field_text(&text, "headers");
                replay.sse_parsed_json = archived_json_field_text(&text, "body");
                replay.upstream_status = archived_json_field_u16(&text, "status");
            }
            "gc_rsp_raw" => replay.client_resp_body = Some(text),
            "gc_rsp_parsed" => {
                replay.client_resp_headers_json = archived_json_field_text(&text, "headers");
                replay.client_sse_parsed_json = archived_json_field_text(&text, "body");
            }
            "req_raw" => replay.egress_body = Some(text),
            "req_parsed" => replay.egress_headers_json = Some(text),
            "rsp_raw" => replay.upstream_resp_body = Some(text),
            "rsp_parsed" => replay.sse_parsed_json = Some(text),
            _ => {}
        }
    }
    Ok(())
}

// ---- circuit breakers (§4.4) ----

async fn circuit_breakers(State(state): State<AdminState>) -> Result<Response, AdminError> {
    let targets = match &state.health {
        Some(health) => health.list_targets(),
        None => {
            return Ok(
                Json(json!({ "targets": [], "note": "health registry not available" }))
                    .into_response(),
            )
        }
    };
    // Resolve provider_id -> provider.name so the UI can show a friendly
    // label instead of a raw id. We swallow store errors here (the breaker
    // feed is best-effort) and fall back to the id when a provider has
    // been deleted out from under the health registry.
    let provider_names: std::collections::HashMap<String, String> =
        match state.store.list_providers().await {
            Ok(providers) => providers.into_iter().map(|p| (p.id, p.name)).collect(),
            Err(_) => std::collections::HashMap::new(),
        };
    let summary: Vec<serde_json::Value> = targets
        .into_iter()
        .map(|t| {
            let status = state
                .health
                .as_ref()
                .map(|h| h.health_status(&t))
                .unwrap_or(tiygate_core::RoutingTargetHealth::Healthy);
            let target_str = t.to_string();
            // RoutingTarget::to_string() formats as "{provider_id}:{model_id}".
            // We split on the first ":" so provider ids containing colons
            // (rare but legal) still keep their tail.
            let (provider_id, model_id) = match target_str.split_once(':') {
                Some((p, m)) => (p.to_string(), m.to_string()),
                None => (target_str.clone(), String::new()),
            };
            let provider_name = provider_names
                .get(&provider_id)
                .cloned()
                .unwrap_or_else(|| provider_id.clone());
            let health = state.health.as_ref();
            let consecutive_failures = health.map(|h| h.consecutive_failures(&t)).unwrap_or(0);
            let cooling_reason = health.and_then(|h| h.cooling_reason(&t));
            let failure_threshold = health.map(|h| h.failure_threshold()).unwrap_or(0);
            let (status_kind, remaining_seconds) = match &status {
                tiygate_core::RoutingTargetHealth::Healthy => ("healthy".to_string(), None),
                tiygate_core::RoutingTargetHealth::CircuitBroken { until } => {
                    let remaining = until
                        .checked_duration_since(std::time::Instant::now())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    ("circuit_broken".to_string(), Some(remaining))
                }
                tiygate_core::RoutingTargetHealth::Cooling { until } => {
                    let remaining = until
                        .checked_duration_since(std::time::Instant::now())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    ("cooling".to_string(), Some(remaining))
                }
            };
            json!({
                "target": target_str,
                "provider_id": provider_id,
                "provider_name": provider_name,
                "model_id": model_id,
                "healthy": matches!(status, tiygate_core::RoutingTargetHealth::Healthy),
                "status": format!("{:?}", status),
                "status_kind": status_kind,
                "remaining_seconds": remaining_seconds,
                "cooling_reason": cooling_reason,
                "consecutive_failures": consecutive_failures,
                "failure_threshold": failure_threshold,
            })
        })
        .collect();
    Ok(Json(json!({ "targets": summary })).into_response())
}

// ---- config export / import ----

/// GET /admin/v1/config/export — serializes all providers, routes,
/// api keys, and settings into a single JSON bundle. Provider and
/// encrypted-setting secrets travel as their on-disk encrypted
/// blobs; the response carries an `encrypted` flag so the importer
/// knows whether a master key is required to decode them. A
/// `Content-Disposition` header nudges browsers into a download flow.
async fn export_config(State(state): State<AdminState>) -> Result<Response, AdminError> {
    let bundle = state.store.export_config().await?;
    let body = Json(&bundle);
    Ok((
        [(
            axum::http::header::CONTENT_DISPOSITION,
            axum::http::HeaderValue::from_static("attachment; filename=\"tiygate-config.json\""),
        )],
        body,
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
struct ImportRequest {
    /// The master key of the instance that produced the export.
    /// Required when the export's `encrypted` flag is `true`;
    /// ignored otherwise.
    master_key: String,
    config: ConfigExport,
    /// Operator-selected subset of the bundle. Each vec carries the
    /// ids (or setting keys) the user explicitly chose to import.
    /// An empty selection imports nothing — the frontend pre-selects
    /// new ids and leaves existing ids unchecked by default.
    #[serde(default)]
    selection: ImportSelection,
}

/// POST /admin/v1/config/import — upserts every entity the
/// operator selected from the supplied bundle. Provider and setting
/// secrets are decrypted with `master_key` and re-encrypted with
/// this instance's key. Returns an [`ImportReport`] summarizing the
/// imported / skipped counts.
async fn import_config(
    State(state): State<AdminState>,
    Json(req): Json<ImportRequest>,
) -> Result<Response, AdminError> {
    // The store deliberately does not depend on concrete protocol crates.
    // Inject the current registry/baseline validator so portable capability
    // overrides cannot bypass the wire contract during import.
    let capability_validator = |target: &tiygate_core::RoutingTarget,
                                capability_id: &tiygate_core::CapabilityId,
                                capability_state: tiygate_core::CapabilityState,
                                value: Option<&tiygate_core::CapabilityValue>|
     -> Result<(), String> {
        let Some(descriptor) = tiygate_protocols::capabilities::descriptor_for(capability_id)
        else {
            // Unknown IDs remain round-trippable but are never eligible for
            // automatic routing in the current registry.
            return Ok(());
        };
        if value.is_some_and(|candidate| candidate.kind() != descriptor.value_kind) {
            return Err(format!(
                "capability value kind does not match {}",
                descriptor.id
            ));
        }
        if capability_state == tiygate_core::CapabilityState::Constrained
            && value.is_none_or(tiygate_core::CapabilityValue::is_empty)
        {
            return Err(format!(
                "constrained capability {} requires a non-empty value",
                descriptor.id
            ));
        }
        let baseline = tiygate_protocols::capabilities::baseline_for(
            &tiygate_store::capabilities::wire_profile_for_target(target),
        );
        if baseline.get(capability_id) == Some(&tiygate_core::BaselineSupport::Forbidden)
            && matches!(
                capability_state,
                tiygate_core::CapabilityState::Supported
                    | tiygate_core::CapabilityState::Constrained
            )
        {
            return Err(format!(
                "capability {} is forbidden by the target wire baseline",
                descriptor.id
            ));
        }
        let mut observation = tiygate_core::CapabilityObservation::now(
            capability_id.clone(),
            capability_state,
            tiygate_core::EvidenceSource::ExplicitOverride,
            1,
        );
        observation.value = value.cloned();
        tiygate_core::validate_capability_observation(descriptor, &observation)
    };
    let report = state
        .store
        .import_config_with_capability_validator(
            &req.config,
            &req.master_key,
            &req.selection,
            Some(&capability_validator),
        )
        .await?;
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "import",
        "config",
        "bulk",
        &json!({
            "providers_imported": report.providers_imported,
            "providers_skipped": report.providers_skipped,
            "routes_imported": report.routes_imported,
            "routes_skipped": report.routes_skipped,
            "api_keys_imported": report.api_keys_imported,
            "api_keys_skipped": report.api_keys_skipped,
            "settings_imported": report.settings_imported,
            "settings_skipped": report.settings_skipped,
            "token_stats_imported": report.token_stats_imported,
            "token_stats_skipped": report.token_stats_skipped,
            "capability_overrides_imported": report.capability_overrides_imported,
            "capability_overrides_skipped": report.capability_overrides_skipped,
        }),
    )
    .await;
    Ok(Json(report).into_response())
}

// ---- settings ----

fn settings_response_value(state: &AdminState, rows: Vec<(String, String)>) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in rows {
        let value = if tiygate_store::settings_keys::is_encrypted_key(&k) {
            serde_json::Value::String(tiygate_store::encryption::KeyEncryption::redact(&v))
        } else {
            serde_json::Value::String(v)
        };
        map.insert(k, value);
    }
    let database_kind = match state.pool.kind() {
        tiygate_store::db::DbKind::Sqlite => "sqlite",
        tiygate_store::db::DbKind::Postgres => "postgres",
    };
    json!({
        "settings": map,
        "database": {
            "kind": database_kind,
        },
    })
}

fn settings_response(state: &AdminState, rows: Vec<(String, String)>) -> Response {
    Json(settings_response_value(state, rows)).into_response()
}

/// GET /admin/v1/settings — returns every setting as a flat
/// `{ "settings": { "<key>": "<value>", ... }, "database": { "kind": "sqlite" | "postgres" } }`
/// object. Encrypted keys are redacted via [`KeyEncryption::redact`]
/// so the response never leaks a secret, mirroring the provider
/// API-key view path.
async fn list_settings(State(state): State<AdminState>) -> Result<Response, AdminError> {
    let rows = state.store.list_settings().await?;
    Ok(settings_response(&state, rows))
}

#[derive(Debug, Serialize, Deserialize)]
struct UpdateSettingsRequest {
    /// A flat map of `key → value`. Every value is treated as a
    /// string (matching the `settings` table schema). Encrypted keys
    /// with an empty value are skipped (leave unchanged).
    settings: serde_json::Map<String, serde_json::Value>,
}

/// PUT /admin/v1/settings — bulk upsert settings. Encrypted keys are
/// routed through [`DbConfigStore::set_setting_encrypted`]; an empty
/// value for an encrypted key is treated as "leave unchanged". After
/// the write the response returns the full redacted view (same shape
/// as `GET`).
async fn update_settings(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(req): Json<UpdateSettingsRequest>,
) -> Result<Response, AdminError> {
    use tiygate_store::encryption::KeyEncryption;
    use tiygate_store::settings_keys::is_encrypted_key;

    /// Redact a setting value for safe inclusion in an audit snapshot.
    /// Encrypted keys carry ciphertext on disk; we pass it through
    /// [`KeyEncryption::redact`] so the audit table never stores the
    /// full blob. Non-encrypted keys are recorded as-is.
    fn redact_setting(key: &str, value: &str) -> serde_json::Value {
        if is_encrypted_key(key) {
            serde_json::Value::String(KeyEncryption::redact(value))
        } else {
            serde_json::Value::String(value.to_string())
        }
    }

    let mut before_map = serde_json::Map::new();
    let mut after_map = serde_json::Map::new();
    let mut updates = Vec::<(String, String, bool)>::new();
    let mut requires_global_capability_enforce = false;

    for (key, val) in &req.settings {
        let s = match val {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if key.starts_with("gateway.capabilities.")
            || key == tiygate_store::settings_keys::RESPONSES_CRL_TOOL_PROMOTION_ENABLED
        {
            validate_capability_setting(key, &s).map_err(AdminError::BadRequest)?;
        }
        if key == tiygate_store::settings_keys::CAPABILITY_ROUTING_MODE
            && tiygate_core::CapabilityRoutingMode::parse(&s).is_none()
        {
            return Err(AdminError::BadRequest(
                "capability routing mode must be off, shadow, or enforce".to_string(),
            ));
        }
        if key == tiygate_store::settings_keys::CAPABILITY_ROUTING_MODE
            && tiygate_core::CapabilityRoutingMode::parse(&s)
                == Some(tiygate_core::CapabilityRoutingMode::Enforce)
        {
            requires_global_capability_enforce = true;
        }
        if is_encrypted_key(key) && s.trim().is_empty() {
            // Leave the stored secret untouched.
            continue;
        }
        // Read the previous value (if any) before overwriting, so the
        // audit entry carries a field-level before/after diff.
        let old = state.store.get_setting(key).await?;
        if let Some(prev) = &old {
            before_map.insert(key.clone(), redact_setting(key, prev));
        } else {
            before_map.insert(key.clone(), serde_json::Value::Null);
        }
        after_map.insert(key.clone(), redact_setting(key, &s));
        updates.push((key.clone(), s, is_encrypted_key(key)));
    }

    let idempotency_payload = serde_json::to_value(&req)
        .map_err(|error| AdminError::Internal(format!("serialize settings request: {error}")))?;
    let reservation =
        begin_capability_idempotency(&state, "settings_update", &headers, &idempotency_payload)
            .await?;
    if let Some((_, _, Some(response))) = reservation {
        return Ok(response);
    }
    let reservation = reservation.map(|(key, hash, _)| (key, hash));
    if requires_global_capability_enforce {
        if let Err(error) = ensure_global_capability_enforce_allowed(&state).await {
            if let Some((key, hash)) = reservation.as_ref() {
                let _ = state
                    .store
                    .release_capability_mutation("settings_update", key, hash)
                    .await;
            }
            return Err(error);
        }
    }
    let before_val = serde_json::Value::Object(before_map);
    let after_val = serde_json::Value::Object(after_map);
    let details = audit_details(Some(&before_val), Some(&after_val));
    if let Err(error) = state
        .store
        .set_settings_batch_with_audit(&updates, "admin", "bulk", &details)
        .await
    {
        if let Some((key, hash)) = reservation.as_ref() {
            let _ = state
                .store
                .release_capability_mutation("settings_update", key, hash)
                .await;
        }
        return Err(error.into());
    }

    // Return the fresh redacted view.
    let rows = state.store.list_settings().await?;
    let response_value = settings_response_value(&state, rows);
    if let Some((key, hash)) = reservation {
        state
            .store
            .complete_capability_mutation(
                "settings_update",
                &key,
                &hash,
                StatusCode::OK.as_u16(),
                &response_value,
            )
            .await?;
    }
    Ok(Json(response_value).into_response())
}

async fn ensure_global_capability_enforce_allowed(state: &AdminState) -> Result<(), AdminError> {
    let (routes, total) = state.store.list_routes_paginated(500, 0).await?;
    if total > routes.len() as u64 {
        return Err(AdminError::Conflict(
            "global capability enforce requires checking every route; reduce the route count or enable per-route gates first"
                .to_string(),
        ));
    }
    for route in routes.into_iter().filter(|route| route.enabled) {
        let admissions = state
            .store
            .list_capability_route_admissions(&route.id, 500, 0)
            .await?;
        let has_valid_enforce = admissions.into_iter().any(is_valid_enforce_admission);
        if !has_valid_enforce {
            return Err(AdminError::AdmissionRequired(format!(
                "route {} has no valid capability-shape enforce admission",
                route.id
            )));
        }
    }
    Ok(())
}

// ---- error type ----

#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("database error: {0}")]
    Db(sqlx::Error),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("capability admission required: {0}")]
    AdmissionRequired(String),
    #[error("invalid capability: {0}")]
    InvalidCapability(String),
    #[error("idempotency conflict: {0}")]
    IdempotencyConflict(String),
    #[error("revision conflict: {0}")]
    RevisionConflict(String),
    #[error("capability unavailable: {0}")]
    CapabilityUnavailable(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let public_message = self.public_message();
        let (status, mut body) = match &self {
            AdminError::Db(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": {"message": public_message, "type": "db", "code": "internal_error", "source": "gateway"}}),
            ),
            AdminError::Store(e) => match e {
                StoreError::NotFound(_) => (
                    StatusCode::NOT_FOUND,
                    json!({"error": {"message": e.to_string(), "type": "not_found", "code": "not_found", "source": "gateway"}}),
                ),
                StoreError::Invalid(_) => (
                    StatusCode::BAD_REQUEST,
                    json!({"error": {"message": e.to_string(), "type": "bad_request", "code": "bad_request", "source": "gateway"}}),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": {"message": public_message, "type": "store", "code": "internal_error", "source": "gateway"}}),
                ),
            },
            AdminError::NotFound(_) => (
                StatusCode::NOT_FOUND,
                json!({"error": {"message": public_message, "type": "not_found", "code": "not_found", "source": "gateway"}}),
            ),
            AdminError::BadRequest(_) => (
                StatusCode::BAD_REQUEST,
                json!({"error": {"message": public_message, "type": "bad_request", "code": "bad_request", "source": "gateway"}}),
            ),
            AdminError::Conflict(_) => (
                StatusCode::CONFLICT,
                json!({"error": {"message": public_message, "type": "conflict", "code": "conflict", "source": "gateway"}}),
            ),
            AdminError::AdmissionRequired(_) => (
                StatusCode::CONFLICT,
                json!({"error": {"message": public_message, "type": "conflict", "code": "admission_required", "source": "gateway"}}),
            ),
            AdminError::InvalidCapability(_) => (
                StatusCode::BAD_REQUEST,
                json!({"error": {"message": public_message, "type": "bad_request", "code": "invalid_capability", "source": "gateway"}}),
            ),
            AdminError::IdempotencyConflict(_) => (
                StatusCode::CONFLICT,
                json!({"error": {"message": public_message, "type": "conflict", "code": "idempotency_conflict", "source": "gateway"}}),
            ),
            AdminError::RevisionConflict(_) => (
                StatusCode::CONFLICT,
                json!({"error": {"message": public_message, "type": "conflict", "code": "revision_conflict", "source": "gateway"}}),
            ),
            AdminError::CapabilityUnavailable(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error": {"message": public_message, "type": "capability_unavailable", "code": "capability_unavailable", "source": "gateway"}}),
            ),
            AdminError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": {"message": public_message, "type": "internal", "code": "internal_error", "source": "gateway"}}),
            ),
        };
        if let Some(error) = body.get_mut("error").and_then(Value::as_object_mut) {
            error.insert(
                "request_id".to_string(),
                Value::String(Uuid::now_v7().to_string()),
            );
        }
        (status, Json(body)).into_response()
    }
}

impl AdminError {
    fn public_message(&self) -> String {
        match self {
            // SQLx can include bound values, table names or connection URLs in
            // its Display implementation. Keep those details in server logs,
            // not in the Admin response body.
            Self::Db(_) => "database operation failed".to_string(),
            Self::Store(StoreError::Db(_)) | Self::Store(StoreError::DbLayer(_)) => {
                "store operation failed".to_string()
            }
            Self::Store(StoreError::Decrypt(_)) => "credential operation failed".to_string(),
            Self::Store(error) => truncate_admin_text(&error.to_string(), 1024),
            Self::Internal(_) => "internal admin operation failed".to_string(),
            Self::NotFound(message) => truncate_admin_text(message, 1024),
            Self::BadRequest(message)
            | Self::Conflict(message)
            | Self::AdmissionRequired(message)
            | Self::InvalidCapability(message)
            | Self::IdempotencyConflict(message)
            | Self::RevisionConflict(message)
            | Self::CapabilityUnavailable(message) => truncate_admin_text(message, 1024),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::items_after_test_module
)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use tiygate_store::archive::{
        build_object_meta, gzip_compress, object_key, ArchiveObject, ArchiveObjectKind,
        ClientError, PayloadArchiveClient,
    };

    fn openai_provider(auth_mode: AuthMode, api_base: &str, models_endpoint: &str) -> Provider {
        let now = chrono::Utc::now();
        Provider {
            id: "openai-provider".to_string(),
            name: "OpenAI".to_string(),
            vendor: "openai".to_string(),
            api_base: api_base.to_string(),
            models_endpoint: models_endpoint.to_string(),
            encrypted_api_key: String::new(),
            auth_mode,
            encrypted_oauth_meta: String::new(),
            metadata_json: json!({}),
            enabled: true,
            created_at: now,
            updated_at: now,
            api_key_cleartext: None,
            oauth_meta_cleartext: None,
        }
    }

    #[test]
    fn openai_urls_split_platform_and_codex_products() {
        assert_eq!(
            normalized_api_base("openai", AuthMode::OAuth, OPENAI_PLATFORM_BASE_URL),
            OPENAI_CODEX_BASE_URL
        );
        assert_eq!(
            normalized_api_base("openai", AuthMode::ApiKey, OPENAI_CODEX_BASE_URL),
            OPENAI_PLATFORM_BASE_URL
        );
        assert_eq!(
            normalized_models_endpoint("openai", AuthMode::OAuth, "", OPENAI_CODEX_BASE_URL),
            format!("{OPENAI_CODEX_BASE_URL}/models")
        );
    }

    #[test]
    fn anthropic_usage_request_uses_claude_code_user_agent() {
        let mut headers = reqwest::header::HeaderMap::new();
        ensure_provider_usage_user_agent("anthropic", &mut headers);

        assert_eq!(
            headers
                .get(reqwest::header::USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some(ANTHROPIC_OAUTH_USAGE_USER_AGENT)
        );

        let custom = reqwest::header::HeaderValue::from_static("tiygate/custom");
        headers.insert(reqwest::header::USER_AGENT, custom);
        ensure_provider_usage_user_agent("anthropic", &mut headers);
        assert_eq!(
            headers
                .get(reqwest::header::USER_AGENT)
                .and_then(|value| value.to_str().ok()),
            Some(ANTHROPIC_OAUTH_USAGE_USER_AGENT)
        );

        let mut openai_headers = reqwest::header::HeaderMap::new();
        ensure_provider_usage_user_agent("openai", &mut openai_headers);
        assert!(!openai_headers.contains_key(reqwest::header::USER_AGENT));
    }

    #[test]
    fn codex_models_url_has_client_version_and_migrates_old_default() {
        let provider = openai_provider(
            AuthMode::OAuth,
            OPENAI_PLATFORM_BASE_URL,
            &format!("{OPENAI_PLATFORM_BASE_URL}/models"),
        );
        assert_eq!(
            provider_models_url(&provider),
            format!(
                "{OPENAI_CODEX_BASE_URL}/models?client_version={}",
                tiygate_auth::provider_oauth::CODEX_CLIENT_VERSION
            )
        );
        assert_ne!(
            tiygate_auth::provider_oauth::CODEX_CLIENT_VERSION,
            env!("CARGO_PKG_VERSION"),
            "Codex protocol compatibility must not follow TiyGate's package version"
        );
    }

    #[test]
    fn parses_visible_codex_model_slugs() {
        let body = json!({
            "models": [
                {"slug": "gpt-visible", "visibility": "list", "supported_in_api": false},
                {"slug": "gpt-hidden", "visibility": "hide", "supported_in_api": true}
            ]
        });
        let models = parse_model_list(&body);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-visible");
    }

    #[test]
    fn parses_openai_usage_windows() {
        let body = json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 27,
                    "limit_window_seconds": 18000,
                    "reset_at": 1_782_770_922
                },
                "secondary_window": {
                    "used_percent": 4,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 600
                }
            }
        })
        .to_string();

        let parsed = parse_openai_usage(&body, 1_000_000).expect("usage JSON");
        assert_eq!(parsed.plan_type.as_deref(), Some("plus"));
        assert_eq!(parsed.windows.len(), 2);
        let five_hour = parsed
            .windows
            .iter()
            .find(|window| window.limit_window_seconds == Some(FIVE_HOURS_SECONDS));
        let seven_day = parsed
            .windows
            .iter()
            .find(|window| window.limit_window_seconds == Some(SEVEN_DAYS_SECONDS));
        assert_eq!(five_hour.and_then(|window| window.used_percent), Some(27.0));
        assert_eq!(
            five_hour.and_then(|window| window.reset_at),
            Some(1_782_770_922)
        );
        assert_eq!(seven_day.and_then(|window| window.used_percent), Some(4.0));
        assert_eq!(
            seven_day.and_then(|window| window.reset_at),
            Some(1_000_600)
        );
    }

    #[test]
    fn parses_openai_reset_credits_and_filters_unavailable_entries() {
        let body = json!({
            "available_count": "2",
            "credits": [
                {"reset_type": "codex_rate_limits", "status": "available", "expires_at": "2026-08-30T00:00:00Z"},
                {"reset_type": "codex_rate_limits", "status": "available", "expiresAt": "2026-09-01T00:00:00Z"},
                {"reset_type": "codex_rate_limits", "status": "redeemed", "expires_at": "2026-08-29T00:00:00Z"},
                {"reset_type": "other_grant", "status": "available", "expires_at": "2026-09-02T00:00:00Z"}
            ]
        });

        let parsed = parse_reset_credits(&body).expect("reset credits JSON");
        assert_eq!(parsed.available_count, 2);
        assert_eq!(parsed.credits.len(), 2);
        assert_eq!(
            parsed.credits[0].expires_at.as_deref(),
            Some("2026-08-30T00:00:00Z")
        );
        assert_eq!(
            parsed.credits[1].expires_at.as_deref(),
            Some("2026-09-01T00:00:00Z")
        );
    }

    #[test]
    fn parses_nested_reset_credit_array_and_derives_count() {
        let body = json!({
            "rate_limit_reset_credits": [
                {"status": "available", "expires_at": "2026-08-30T00:00:00Z"},
                {"status": "available", "expires_at": "2026-09-01T00:00:00Z"}
            ]
        });

        let parsed = parse_reset_credits(&body).expect("nested reset credits JSON");
        assert_eq!(parsed.available_count, 2);
        assert_eq!(parsed.credits.len(), 2);

        let empty = parse_reset_credits(&json!([])).expect("empty reset credits JSON");
        assert_eq!(empty.available_count, 0);
        assert!(empty.credits.is_empty());
    }

    #[test]
    fn parses_reset_credit_consume_outcomes() {
        let reset = parse_reset_credits_consume_response(
            "openai-provider",
            &json!({"code": "reset", "windows_reset": 2}).to_string(),
        )
        .expect("reset-credit consume JSON");
        assert_eq!(reset.provider_id, "openai-provider");
        assert_eq!(reset.code, "reset");
        assert_eq!(reset.windows_reset, Some(2));

        let no_credit = parse_reset_credits_consume_response(
            "openai-provider",
            &json!({"code": "no_credit", "windows_reset": 0}).to_string(),
        )
        .expect("no-credit consume JSON");
        assert_eq!(no_credit.code, "no_credit");
        assert_eq!(no_credit.windows_reset, Some(0));
        let error = require_reset_credit_success(no_credit).expect_err("no-credit must fail");
        assert!(matches!(error, AdminError::Conflict(message) if message.contains("no_credit")));
        assert!(parse_reset_credits_consume_response("openai-provider", "{}").is_err());
    }

    #[test]
    fn openai_usage_headers_are_provider_scoped() {
        let mut openai_headers = reqwest::header::HeaderMap::new();
        ensure_openai_codex_usage_headers("openai", &mut openai_headers);
        assert_eq!(
            openai_headers.get("openai-beta").unwrap(),
            OPENAI_CODEX_BETA
        );
        assert_eq!(
            openai_headers.get("oai-language").unwrap(),
            OPENAI_CODEX_LANGUAGE
        );
        assert_eq!(
            openai_headers.get("sec-fetch-mode").unwrap(),
            OPENAI_CODEX_SEC_FETCH_MODE
        );

        let mut anthropic_headers = reqwest::header::HeaderMap::new();
        ensure_openai_codex_usage_headers("anthropic", &mut anthropic_headers);
        assert!(anthropic_headers.is_empty());
    }

    #[test]
    fn maps_single_primary_weekly_window_by_duration() {
        let body = json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 42,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 900
                },
                "secondary_window": null
            }
        })
        .to_string();

        let parsed = parse_openai_usage(&body, 1_000_000).expect("usage JSON");
        assert_eq!(parsed.windows.len(), 1);
        assert_eq!(
            parsed.windows[0].limit_window_seconds,
            Some(SEVEN_DAYS_SECONDS)
        );
        let response =
            provider_usage_response("openai-oauth", "available", None, parsed.windows, None);
        assert_eq!(response.windows.len(), 1);
        assert!(response.five_hour.is_none());
        assert_eq!(
            response
                .seven_day
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(42.0)
        );
        assert_eq!(
            response
                .seven_day
                .as_ref()
                .and_then(|window| window.reset_at),
            Some(1_000_900)
        );
    }

    #[test]
    fn parses_anthropic_main_and_scoped_usage_windows() {
        let body = json!({
            "subscription_type": "max",
            "five_hour": {
                "utilization": 12.5,
                "resets_at": "2026-07-16T12:30:00Z"
            },
            "seven_day": {
                "utilization": 34,
                "resets_at": "2026-07-20T08:00:00Z"
            },
            "seven_day_sonnet": {
                "utilization": 56,
                "resets_at": "2026-07-20T09:00:00Z"
            },
            "seven_day_opus": null,
            "seven_day_haiku": {
                "utilization": 7,
                "resets_at": "2026-07-20T10:00:00Z"
            },
            "limits": [{
                "limit_name": "Research",
                "weekly_scoped": {
                    "utilization": 8,
                    "resets_at": "2026-07-20T11:00:00Z"
                }
            }],
            "extra_usage": {
                "is_enabled": true,
                "used_credits": 10
            }
        })
        .to_string();

        let parsed = parse_anthropic_usage(&body).expect("Anthropic usage JSON");
        assert_eq!(parsed.plan_type.as_deref(), Some("max"));
        assert_eq!(parsed.windows.len(), 5);
        assert_eq!(parsed.windows[0].label, None);
        assert_eq!(
            parsed.windows[0].limit_window_seconds,
            Some(FIVE_HOURS_SECONDS)
        );
        assert_eq!(parsed.windows[0].used_percent, Some(12.5));
        assert_eq!(
            parsed.windows[0].reset_at,
            chrono::DateTime::parse_from_rfc3339("2026-07-16T12:30:00Z")
                .ok()
                .map(|timestamp| timestamp.timestamp())
        );
        assert_eq!(parsed.windows[1].label, None);
        assert_eq!(
            parsed.windows[1].limit_window_seconds,
            Some(SEVEN_DAYS_SECONDS)
        );
        assert!(parsed
            .windows
            .iter()
            .any(|window| window.label.as_deref() == Some("Sonnet · 7d")));
        assert!(parsed
            .windows
            .iter()
            .any(|window| window.label.as_deref() == Some("Haiku · 7d")));
        assert!(parsed
            .windows
            .iter()
            .any(|window| window.label.as_deref() == Some("Research · 7d")));
    }

    #[test]
    fn anthropic_usage_omits_windows_without_utilization() {
        let body = json!({
            "rate_limit_tier": "default_claude_max_5x",
            "five_hour": {
                "utilization": null,
                "resets_at": "2026-07-16T12:30:00Z"
            },
            "seven_day": {
                "utilization": 9,
                "resets_at": "2026-07-20T08:00:00Z"
            }
        })
        .to_string();

        let parsed = parse_anthropic_usage(&body).expect("Anthropic usage JSON");
        assert_eq!(parsed.plan_type.as_deref(), Some("default_claude_max_5x"));
        assert_eq!(parsed.windows.len(), 1);
        assert_eq!(
            parsed.windows[0].limit_window_seconds,
            Some(SEVEN_DAYS_SECONDS)
        );
    }

    #[test]
    fn clamps_openai_usage_percent_to_display_range() {
        let body = r#"{
            "rate_limit": {
                "primary_window": {"used_percent": 120},
                "secondary_window": {"used_percent": -5}
            }
        }"#;

        let parsed = parse_openai_usage(body, 1_000_000).expect("usage JSON");
        assert_eq!(parsed.windows[0].used_percent, Some(100.0));
        assert_eq!(parsed.windows[1].used_percent, Some(0.0));
    }

    #[test]
    fn rotated_model_discovery_token_preserves_oauth_metadata() {
        let mut provider = openai_provider(AuthMode::OAuth, OPENAI_CODEX_BASE_URL, "");
        provider.oauth_meta_cleartext = Some(
            json!({
                "refresh_token": "refresh-old",
                "account_id": "workspace-123",
                "expires_in_s": 864_000,
                "future_field": { "preserved": true }
            })
            .to_string(),
        );

        let updated =
            oauth_meta_after_refresh_rotation(&provider, "refresh-old", "refresh-rotated")
                .expect("valid OAuth metadata")
                .expect("rotated token must produce an update");
        let updated: serde_json::Value = serde_json::from_str(&updated).expect("JSON");

        assert_eq!(updated["refresh_token"], "refresh-rotated");
        assert_eq!(updated["account_id"], "workspace-123");
        assert_eq!(updated["expires_in_s"], 864_000);
        assert_eq!(updated["future_field"]["preserved"], true);
        assert_eq!(updated["status"], "healthy");
        assert!(updated["status_checked_at"].is_string());
        assert!(updated.get("status_reason").is_none());
    }

    #[test]
    fn unchanged_model_discovery_token_skips_oauth_metadata_write() {
        let mut provider = openai_provider(AuthMode::OAuth, OPENAI_CODEX_BASE_URL, "");
        provider.oauth_meta_cleartext = Some(json!({ "refresh_token": "same" }).to_string());

        assert!(oauth_meta_after_refresh_rotation(&provider, "same", "same")
            .expect("valid OAuth metadata")
            .is_none());
    }

    #[test]
    fn provider_view_exposes_sanitized_invalid_oauth_status() {
        let mut provider = openai_provider(AuthMode::OAuth, OPENAI_CODEX_BASE_URL, "");
        provider.encrypted_oauth_meta = "encrypted-secret".to_string();
        provider.oauth_meta_cleartext = Some(
            json!({
                "refresh_token": "never-return-this",
                "status": "invalid",
                "status_reason": "credential_rejected",
                "status_checked_at": "2026-07-12T06:00:00Z",
            })
            .to_string(),
        );

        let view = ProviderView::from(provider);
        let status = view.oauth_status.as_ref().expect("OAuth status");
        assert_eq!(status.state, "invalid");
        assert_eq!(status.reason.as_deref(), Some("credential_rejected"));
        assert_eq!(status.checked_at.as_deref(), Some("2026-07-12T06:00:00Z"));
        let serialized = serde_json::to_string(&view).expect("serialize provider view");
        assert!(!serialized.contains("never-return-this"));
    }

    #[derive(Default)]
    struct MemoryArchiveClient {
        objects: BTreeMap<String, Bytes>,
    }

    impl PayloadArchiveClient for MemoryArchiveClient {
        fn bucket(&self) -> &str {
            "test-bucket"
        }

        fn prefix(&self) -> &str {
            "archive-prefix"
        }

        fn timeout(&self) -> Duration {
            Duration::from_secs(1)
        }

        fn put_object<'a>(
            &'a self,
            _key: &'a str,
            _body: Bytes,
            _content_type: &'a str,
            _content_encoding: &'a str,
            _metadata: Vec<(&'a str, &'a str)>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ClientError>> + Send + 'a>>
        {
            Box::pin(async { Ok(()) })
        }

        fn get_object<'a>(
            &'a self,
            key: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Bytes, ClientError>> + Send + 'a>,
        > {
            Box::pin(async move {
                self.objects
                    .get(key)
                    .cloned()
                    .ok_or(ClientError::InvalidObjectUrl)
            })
        }
    }

    fn archive_entry(
        kind: Option<ArchiveObjectKind>,
        key: String,
        text: &str,
    ) -> (ArchiveObject, Bytes) {
        let compressed = gzip_compress(text.as_bytes()).expect("compress");
        let meta_kind = kind.unwrap_or_else(|| {
            if key.ends_with(".txt") {
                ArchiveObjectKind::GpReqRaw
            } else {
                ArchiveObjectKind::CgReqParsed
            }
        });
        let object = build_object_meta(meta_kind, text.as_bytes(), &compressed, key);
        (object, Bytes::from(compressed))
    }

    #[tokio::test]
    async fn hydrate_archived_replay_supports_new_and_legacy_manifests() {
        let pool = tiygate_store::db::open_pool("sqlite::memory:")
            .await
            .expect("pool");
        tiygate_store::db::run_migrations(&pool)
            .await
            .expect("migrate");
        let store = Arc::new(tiygate_store::config_store::DbConfigStore::new(
            pool.clone(),
            None,
        ));
        let pool = Arc::new(pool);

        let mut objects = BTreeMap::new();
        let mut payloads = BTreeMap::new();
        let mut insert =
            |manifest_kind: &str, kind: Option<ArchiveObjectKind>, key: String, text: &str| {
                let (object, compressed) = archive_entry(kind, key, text);
                payloads.insert(object.key.clone(), compressed);
                objects.insert(manifest_kind.to_string(), object);
            };

        insert(
            "cg_req_raw",
            Some(ArchiveObjectKind::CgReqRaw),
            object_key("archive-prefix", "req-1", ArchiveObjectKind::CgReqRaw),
            r#"{"raw":true}"#,
        );
        insert(
            "cg_req_parsed",
            Some(ArchiveObjectKind::CgReqParsed),
            object_key("archive-prefix", "req-1", ArchiveObjectKind::CgReqParsed),
            r#"{"headers":{"authorization":"[REDACTED]"}}"#,
        );
        insert(
            "gp_req_raw",
            Some(ArchiveObjectKind::GpReqRaw),
            object_key("archive-prefix", "req-1", ArchiveObjectKind::GpReqRaw),
            "provider request",
        );
        insert(
            "gp_req_parsed",
            Some(ArchiveObjectKind::GpReqParsed),
            object_key("archive-prefix", "req-1", ArchiveObjectKind::GpReqParsed),
            r#"{"headers":{"x-gp":"1"},"method":"POST","path":"/v1/chat"}"#,
        );
        insert(
            "pg_rsp_raw",
            Some(ArchiveObjectKind::PgRspRaw),
            object_key("archive-prefix", "req-1", ArchiveObjectKind::PgRspRaw),
            "provider response",
        );
        insert(
            "pg_rsp_parsed",
            Some(ArchiveObjectKind::PgRspParsed),
            object_key("archive-prefix", "req-1", ArchiveObjectKind::PgRspParsed),
            r#"{"headers":{"x-pg":"1"},"status":"201","body":{"delta":"ok"}}"#,
        );
        insert(
            "gc_rsp_raw",
            Some(ArchiveObjectKind::GcRspRaw),
            object_key("archive-prefix", "req-1", ArchiveObjectKind::GcRspRaw),
            "client response",
        );
        insert(
            "gc_rsp_parsed",
            Some(ArchiveObjectKind::GcRspParsed),
            object_key("archive-prefix", "req-1", ArchiveObjectKind::GcRspParsed),
            r#"{"headers":{"x-gc":"1"},"body":{"client":"ok"}}"#,
        );
        insert(
            "req_raw",
            None,
            "archive-prefix/req-1/legacy_req_raw.txt".to_string(),
            "legacy request",
        );
        insert(
            "req_parsed",
            None,
            "archive-prefix/req-1/legacy_req_parsed.json".to_string(),
            r#"{"legacy":"request-headers"}"#,
        );
        insert(
            "rsp_raw",
            None,
            "archive-prefix/req-1/legacy_rsp_raw.txt".to_string(),
            "legacy response",
        );
        insert(
            "rsp_parsed",
            None,
            "archive-prefix/req-1/legacy_rsp_parsed.json".to_string(),
            r#"{"legacy":"parsed"}"#,
        );

        let manifest = PayloadArchiveManifest {
            request_id: "req-1".to_string(),
            objects,
        };
        let archive = Arc::new(MemoryArchiveClient { objects: payloads });
        let state = AdminState::new(store, pool, None).with_payload_archive(Some(archive));
        let mut replay = tiygate_store::log_sink::oltp::RequestReplay {
            request_id: "req-1".to_string(),
            payload_archive_status: Some("uploaded".to_string()),
            payload_archive_manifest_json: Some(
                serde_json::to_string(&manifest).expect("manifest"),
            ),
            ..Default::default()
        };

        hydrate_archived_replay(&mut replay, &state)
            .await
            .expect("hydrate");

        assert_eq!(replay.raw_envelope_json.as_deref(), Some(r#"{"raw":true}"#));
        assert_eq!(
            replay.redacted_headers_json.as_deref(),
            Some(r#"{"authorization":"[REDACTED]"}"#)
        );
        assert_eq!(replay.egress_body.as_deref(), Some("legacy request"));
        assert_eq!(
            replay.egress_headers_json.as_deref(),
            Some(r#"{"legacy":"request-headers"}"#)
        );
        assert_eq!(replay.egress_method.as_deref(), Some("POST"));
        assert_eq!(replay.egress_path.as_deref(), Some("/v1/chat"));
        assert_eq!(
            replay.upstream_resp_body.as_deref(),
            Some("legacy response")
        );
        assert_eq!(
            replay.upstream_resp_headers_json.as_deref(),
            Some(r#"{"x-pg":"1"}"#)
        );
        assert_eq!(
            replay.sse_parsed_json.as_deref(),
            Some(r#"{"legacy":"parsed"}"#)
        );
        assert_eq!(replay.upstream_status, Some(201));
        assert_eq!(replay.client_resp_body.as_deref(), Some("client response"));
        assert_eq!(
            replay.client_resp_headers_json.as_deref(),
            Some(r#"{"x-gc":"1"}"#)
        );
        assert_eq!(
            replay.client_sse_parsed_json.as_deref(),
            Some(r#"{"client":"ok"}"#)
        );
    }

    #[test]
    fn refresh_replay_sse_parsed_recomputes_from_raw_bodies() {
        let raw_sse = "\
data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\",\"status\":\"in_progress\"}}\n\
data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"call_A\",\"name\":\"read\",\"arguments\":\"\"}}\n\
data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"id\":\"call_A\"}]}}\n\
data: [DONE]\n";
        let mut replay = tiygate_store::log_sink::oltp::RequestReplay {
            is_stream: true,
            upstream_resp_body: Some(raw_sse.to_string()),
            client_resp_body: Some(raw_sse.to_string()),
            sse_parsed_json: Some(
                r#"{"event_count":3,"finish_reason":"stop","protocol":"openai_responses"}"#
                    .to_string(),
            ),
            client_sse_parsed_json: Some(
                r#"{"event_count":3,"finish_reason":"stop","protocol":"openai_responses"}"#
                    .to_string(),
            ),
            ..Default::default()
        };

        refresh_replay_sse_parsed(&mut replay);

        let parsed = replay
            .client_sse_parsed_json
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .expect("parsed refresh");
        assert_eq!(parsed["protocol"], "openai_responses");
        assert_eq!(parsed["finish_reason"], "tool_calls");
        assert_eq!(parsed["tool_call_count"], 1);
        assert_eq!(parsed["tool_calls"][0]["id"], "call_A");
        assert_eq!(parsed["tool_calls"][0]["name"], "read");

        let upstream_parsed = replay
            .sse_parsed_json
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .expect("parsed upstream refresh");
        assert_eq!(upstream_parsed["finish_reason"], "tool_calls");
        assert_eq!(upstream_parsed["tool_call_count"], 1);
    }
}

// Suppress the dead-code warning for unused utility helpers.
#[allow(dead_code)]
fn _unused(_: &dyn std::fmt::Debug) {}
