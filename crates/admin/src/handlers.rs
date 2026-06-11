//! Admin API handlers — providers, routes, api-keys, health, stats.
//!
//! Each handler is a thin shim around the corresponding
//! [`DbConfigStore`] method. The handlers are intentionally small
//! and live in a single file so the route map below is the only
//! thing a new contributor has to read to understand the API
//! surface.

#[allow(unused_imports)]
use axum::routing::{post, put};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use tiygate_store::config_store::StoreError;
use tiygate_store::models::{AuthMode, Provider, Route, RouteTarget};

use crate::state::AdminState;

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/admin/v1/health", get(health))
        .route(
            "/admin/v1/providers",
            get(list_providers).post(create_provider),
        )
        .route(
            "/admin/v1/providers/:id",
            get(get_provider)
                .put(update_provider)
                .delete(delete_provider),
        )
        .route("/admin/v1/routes", get(list_routes).post(create_route))
        .route(
            "/admin/v1/routes/:id",
            get(get_route).put(update_route).delete(delete_route),
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
        .route("/admin/v1/stats/by-model", get(stats_by_model))
        .route("/admin/v1/stats/by-provider", get(stats_by_provider))
        .route("/admin/v1/stats/by-api-key", get(stats_by_api_key))
        .route("/admin/v1/audit", get(list_audit))
        .route("/admin/v1/requests", get(list_requests))
        .route("/admin/v1/requests/:id/replay", get(replay_request))
        .route("/admin/v1/health/circuit-breakers", get(circuit_breakers))
}

// ---- health ----

async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

// ---- providers ----

#[derive(Debug, Deserialize)]
struct ProviderRequest {
    id: Option<String>,
    name: String,
    vendor: String,
    api_base: String,
    api_key: Option<String>,
    auth_mode: Option<String>,
    oauth_meta: Option<String>,
    metadata: Option<serde_json::Value>,
    tenant_scope: Option<String>,
    enabled: Option<bool>,
}

#[derive(Debug, Serialize)]
struct ProviderView {
    id: String,
    name: String,
    vendor: String,
    api_base: String,
    auth_mode: String,
    encrypted_api_key: String,
    encrypted_oauth_meta: String,
    metadata: serde_json::Value,
    tenant_scope: Option<String>,
    enabled: bool,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<Provider> for ProviderView {
    fn from(p: Provider) -> Self {
        Self {
            id: p.id,
            name: p.name,
            vendor: p.vendor,
            api_base: p.api_base,
            auth_mode: p.auth_mode.as_str().to_string(),
            encrypted_api_key: tiygate_store::encryption::KeyEncryption::redact(
                &p.encrypted_api_key,
            ),
            encrypted_oauth_meta: tiygate_store::encryption::KeyEncryption::redact(
                &p.encrypted_oauth_meta,
            ),
            metadata: p.metadata_json,
            tenant_scope: p.tenant_scope,
            enabled: p.enabled,
            created_at: p.created_at,
            updated_at: p.updated_at,
        }
    }
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
    let p = state
        .store
        .upsert_provider(
            &id,
            &req.name,
            &req.vendor,
            &req.api_base,
            req.api_key.as_deref(),
            auth_mode,
            req.oauth_meta.as_deref(),
            req.metadata.unwrap_or_else(|| serde_json::json!({})),
            req.tenant_scope.as_deref(),
            req.enabled.unwrap_or(true),
        )
        .await?;
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "upsert",
        "provider",
        &p.id,
        &json!({"name": p.name}),
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
    let p = state
        .store
        .upsert_provider(
            &id,
            &req.name,
            &req.vendor,
            &req.api_base,
            req.api_key.as_deref(),
            auth_mode,
            req.oauth_meta.as_deref(),
            req.metadata.unwrap_or_else(|| serde_json::json!({})),
            req.tenant_scope.as_deref(),
            req.enabled.unwrap_or(true),
        )
        .await?;
    Ok(Json(ProviderView::from(p)).into_response())
}

async fn delete_provider(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    state.store.delete_provider(&id).await?;
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "delete",
        "provider",
        &id,
        &json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---- routes ----

#[derive(Debug, Deserialize)]
struct RouteRequest {
    id: Option<String>,
    virtual_model: String,
    targets: Vec<RouteTarget>,
    enabled: Option<bool>,
    tenant_scope: Option<String>,
}

#[derive(Debug, Serialize)]
struct RouteView {
    id: String,
    virtual_model: String,
    targets: Vec<RouteTarget>,
    enabled: bool,
    tenant_scope: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<Route> for RouteView {
    fn from(r: Route) -> Self {
        Self {
            id: r.id,
            virtual_model: r.virtual_model,
            targets: r.targets,
            enabled: r.enabled,
            tenant_scope: r.tenant_scope,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

async fn list_routes(State(state): State<AdminState>) -> Result<Response, AdminError> {
    let routes = state.store.list_routes().await?;
    let views: Vec<RouteView> = routes.into_iter().map(Into::into).collect();
    Ok(Json(views).into_response())
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
    Ok(Json(RouteView::from(r)).into_response())
}

async fn create_route(
    State(state): State<AdminState>,
    Json(req): Json<RouteRequest>,
) -> Result<Response, AdminError> {
    let id = req.id.unwrap_or_else(|| Uuid::now_v7().to_string());
    let r = state
        .store
        .upsert_route(
            &id,
            &req.virtual_model,
            &req.targets,
            req.enabled.unwrap_or(true),
            req.tenant_scope.as_deref(),
        )
        .await?;
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "upsert",
        "route",
        &r.id,
        &json!({"virtual_model": r.virtual_model, "targets": r.targets.len()}),
    )
    .await;
    Ok((StatusCode::CREATED, Json(RouteView::from(r))).into_response())
}

async fn update_route(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(req): Json<RouteRequest>,
) -> Result<Response, AdminError> {
    let r = state
        .store
        .upsert_route(
            &id,
            &req.virtual_model,
            &req.targets,
            req.enabled.unwrap_or(true),
            req.tenant_scope.as_deref(),
        )
        .await?;
    Ok(Json(RouteView::from(r)).into_response())
}

async fn delete_route(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    state.store.delete_route(&id).await?;
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "delete",
        "route",
        &id,
        &json!({}),
    )
    .await;
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
    tenant_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct CreateApiKeyResponse {
    id: String,
    name: String,
    secret: String,
    quota: serde_json::Value,
    status: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize)]
struct ApiKeyView {
    id: String,
    name: String,
    key_hash: String,
    quota: serde_json::Value,
    status: String,
    tenant_id: Option<String>,
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
            status: k.status.as_str().to_string(),
            tenant_id: k.tenant_id,
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
            req.tenant_id.as_deref(),
        )
        .await?;
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "create",
        "api_key",
        &key.id,
        &json!({"name": key.name}),
    )
    .await;
    let resp = CreateApiKeyResponse {
        id: key.id,
        name: key.name,
        secret: plain,
        quota: key.quota_json,
        status: key.status.as_str().to_string(),
        created_at: key.created_at,
    };
    Ok((StatusCode::CREATED, Json(resp)).into_response())
}

async fn delete_api_key(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    state.store.delete_api_key(&id).await?;
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "delete",
        "api_key",
        &id,
        &json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn disable_api_key(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    state.store.disable_api_key(&id).await?;
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "disable",
        "api_key",
        &id,
        &json!({}),
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
    let key = state.store.update_api_key_quota(&id, req.quota).await?;
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "update_quota",
        "api_key",
        &key.id,
        &json!({"quota": key.quota_json}),
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

// ---- audit ----

async fn list_audit(
    State(state): State<AdminState>,
    axum::extract::Query(q): axum::extract::Query<AuditQuery>,
) -> Result<Response, AdminError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let entries = match tiygate_store::audit::list_recent(state.pool.as_ref(), limit).await {
        Ok(v) => v,
        Err(e) => return Err(AdminError::Internal(e.to_string())),
    };
    Ok(Json(entries).into_response())
}

#[derive(Debug, Deserialize)]
struct AuditQuery {
    limit: Option<i64>,
}

// ---- request drill-down & replay (§4.4 / §8 acceptance #8) ----

#[derive(Debug, Deserialize)]
struct RequestListQuery {
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
    let filter = tiygate_store::log_sink::oltp::RequestFilter {
        since: q.since,
        until: q.until,
        model: q.model,
        provider: q.provider,
        status: q.status,
        error_class: q.error_class,
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

async fn replay_request(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    let replay =
        match tiygate_store::log_sink::oltp::get_request_replay(state.pool.as_ref(), &id).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Err(AdminError::NotFound(format!(
                    "request {id} not found in logs"
                )))
            }
            Err(e) => return Err(AdminError::Db(e)),
        };
    Ok(Json(replay).into_response())
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
    let summary: Vec<serde_json::Value> = targets
        .into_iter()
        .map(|t| {
            let status = state
                .health
                .as_ref()
                .map(|h| h.health_status(&t))
                .unwrap_or(tiygate_core::RoutingTargetHealth::Healthy);
            json!({
                "target": t.to_string(),
                "healthy": matches!(status, tiygate_core::RoutingTargetHealth::Healthy),
                "status": format!("{:?}", status),
            })
        })
        .collect();
    Ok(Json(json!({ "targets": summary })).into_response())
}

// ---- error type ----// ---- error type ----

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
    #[error("internal: {0}")]
    Internal(String),
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let (status, body) = match &self {
            AdminError::Db(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": {"message": e.to_string(), "type": "db", "source": "gateway"}}),
            ),
            AdminError::Store(e) => match e {
                StoreError::NotFound(_) => (
                    StatusCode::NOT_FOUND,
                    json!({"error": {"message": e.to_string(), "type": "not_found", "source": "gateway"}}),
                ),
                StoreError::Invalid(_) => (
                    StatusCode::BAD_REQUEST,
                    json!({"error": {"message": e.to_string(), "type": "bad_request", "source": "gateway"}}),
                ),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": {"message": e.to_string(), "type": "store", "source": "gateway"}}),
                ),
            },
            AdminError::NotFound(_) => (
                StatusCode::NOT_FOUND,
                json!({"error": {"message": self.to_string(), "type": "not_found", "source": "gateway"}}),
            ),
            AdminError::BadRequest(_) => (
                StatusCode::BAD_REQUEST,
                json!({"error": {"message": self.to_string(), "type": "bad_request", "source": "gateway"}}),
            ),
            AdminError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": {"message": self.to_string(), "type": "internal", "source": "gateway"}}),
            ),
        };
        (status, Json(body)).into_response()
    }
}

// Suppress the dead-code warning for unused utility helpers.
#[allow(dead_code)]
fn _unused(_: &dyn std::fmt::Debug) {}
