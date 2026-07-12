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

use tiygate_store::archive::{gzip_decompress, sha256_hex, PayloadArchiveManifest};
use tiygate_store::config_store::StoreError;
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
    used_percent: Option<f64>,
    reset_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct ProviderUsageResponse {
    provider_id: String,
    state: String,
    reason: Option<String>,
    checked_at: Option<String>,
    five_hour: Option<ProviderUsageWindow>,
    seven_day: Option<ProviderUsageWindow>,
    account_email: Option<String>,
    plan_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsageResponse {
    plan_type: Option<String>,
    rate_limit: Option<OpenAiRateLimit>,
}

#[derive(Debug, Clone)]
struct ParsedOpenAiUsage {
    plan_type: Option<String>,
    five_hour: Option<ProviderUsageWindow>,
    seven_day: Option<ProviderUsageWindow>,
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
}

fn provider_usage_response(
    provider_id: &str,
    state: &str,
    reason: Option<&str>,
    five_hour: Option<ProviderUsageWindow>,
    seven_day: Option<ProviderUsageWindow>,
    account_email: Option<&str>,
) -> ProviderUsageResponse {
    ProviderUsageResponse {
        provider_id: provider_id.to_string(),
        state: state.to_string(),
        reason: reason.map(str::to_string),
        checked_at: Some(chrono::Utc::now().to_rfc3339()),
        five_hour,
        seven_day,
        account_email: account_email.map(str::to_string),
        plan_type: None,
    }
}

fn map_openai_usage_window(
    window: Option<OpenAiUsageWindow>,
    now_unix: i64,
) -> Option<ProviderUsageWindow> {
    window.map(|window| ProviderUsageWindow {
        used_percent: window.used_percent.map(|value| value.clamp(0.0, 100.0)),
        reset_at: window.reset_at.or_else(|| {
            window
                .reset_after_seconds
                .map(|seconds| now_unix.saturating_add(seconds))
        }),
    })
}

fn parse_openai_usage(body: &str, now_unix: i64) -> Result<ParsedOpenAiUsage, String> {
    let response: OpenAiUsageResponse =
        serde_json::from_str(body).map_err(|error| format!("invalid usage response: {error}"))?;
    let plan_type = response
        .plan_type
        .filter(|plan_type| !plan_type.trim().is_empty());
    let Some(rate_limit) = response.rate_limit else {
        return Err("usage response has no rate_limit".to_string());
    };
    Ok(ParsedOpenAiUsage {
        plan_type,
        five_hour: map_openai_usage_window(rate_limit.primary_window, now_unix),
        seven_day: map_openai_usage_window(rate_limit.secondary_window, now_unix),
    })
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

/// Fetch the ChatGPT/Codex subscription windows for one OpenAI OAuth
/// provider. The OAuth cache is keyed by provider/account, so multiple
/// providers can safely use different ChatGPT accounts in one process.
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

    if provider.vendor != "openai" || !matches!(provider.auth_mode, AuthMode::OAuth) {
        return Ok(Json(provider_usage_response(
            &id,
            "unsupported",
            Some("openai_oauth_only"),
            None,
            None,
            stored_account_email.as_deref(),
        ))
        .into_response());
    }

    let Some(oauth_config) = tiygate_store::config_store::build_oauth_target_config(&provider)
    else {
        return Ok(Json(provider_usage_response(
            &id,
            "not_connected",
            Some("oauth_metadata_unavailable"),
            None,
            None,
            stored_account_email.as_deref(),
        ))
        .into_response());
    };
    if oauth_config.refresh_token.is_empty() {
        return Ok(Json(provider_usage_response(
            &id,
            "not_connected",
            Some("refresh_token_missing"),
            None,
            None,
            stored_account_email.as_deref(),
        ))
        .into_response());
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| AdminError::Internal(format!("http client build: {error}")))?;
    let cache = tiygate_auth::provider_oauth::OAuthTokenCache::global();
    let label = oauth_config.cache_label();
    let mut headers = reqwest::header::HeaderMap::new();
    let coordinated = state.oauth_service.is_some();
    let apply_result = if let Some(service) = state.oauth_service.as_ref() {
        service.apply_provider_headers(&id, &mut headers).await
    } else {
        cache.seed(&id, label, &oauth_config.refresh_token);
        cache
            .apply(&mut headers, &id, label, &oauth_config, &client)
            .await
    };
    if let Err(error) = apply_result {
        record_oauth_refresh_failure(&state, &id, &error).await;
        tracing::warn!(provider = %id, error = %error, "OpenAI OAuth usage token unavailable");
        return Ok(Json(provider_usage_response(
            &id,
            "unavailable",
            Some("oauth_token_unavailable"),
            None,
            None,
            stored_account_email.as_deref(),
        ))
        .into_response());
    }

    let account_email = cache
        .get_account_email(&id, label)
        .or_else(|| stored_account_email.clone());
    if !coordinated {
        if let Some(cached_refresh_token) = cache.get_refresh_token(&id, label) {
            match oauth_meta_after_cache_update(
                &provider,
                &oauth_config.refresh_token,
                &cached_refresh_token,
                account_email.as_deref(),
            ) {
                Ok(Some(meta)) => {
                    if let Err(error) = state.store.set_provider_oauth_meta(&id, &meta).await {
                        tracing::warn!(
                            provider = %id,
                            error = %error,
                            "persisting OpenAI OAuth identity after usage request failed"
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        provider = %id,
                        error = %error,
                        "preparing OpenAI OAuth identity metadata failed"
                    );
                }
            }
        }
    }

    let mut request = client.get(OPENAI_CODEX_USAGE_URL);
    for (name, value) in &headers {
        request = request.header(name, value);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(provider = %id, error = %error, "OpenAI usage request failed");
            return Ok(Json(provider_usage_response(
                &id,
                "unavailable",
                Some("upstream_request_failed"),
                None,
                None,
                account_email.as_deref(),
            ))
            .into_response());
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            record_oauth_status(
                &state,
                &id,
                OAuthCredentialStatus::Invalid,
                Some("usage_auth_rejected"),
            )
            .await;
        }
        tracing::warn!(provider = %id, status = %status, "OpenAI usage endpoint returned an error");
        return Ok(Json(provider_usage_response(
            &id,
            "unavailable",
            Some("upstream_http_error"),
            None,
            None,
            account_email.as_deref(),
        ))
        .into_response());
    }

    let body = response
        .text()
        .await
        .map_err(|error| AdminError::Internal(format!("read usage response: {error}")))?;
    let parsed_usage = match parse_openai_usage(&body, chrono::Utc::now().timestamp()) {
        Ok(usage) => usage,
        Err(error) => {
            tracing::warn!(provider = %id, error = %error, "OpenAI usage response parse failed");
            return Ok(Json(provider_usage_response(
                &id,
                "unavailable",
                Some("invalid_upstream_response"),
                None,
                None,
                account_email.as_deref(),
            ))
            .into_response());
        }
    };
    let mut usage = provider_usage_response(
        &id,
        "available",
        None,
        parsed_usage.five_hour,
        parsed_usage.seven_day,
        account_email.as_deref(),
    );
    usage.plan_type = parsed_usage.plan_type;
    Ok(Json(usage).into_response())
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

/// Build a JSON snapshot of a route, including full target details.
fn route_snapshot(r: &Route) -> serde_json::Value {
    json!({
        "id": r.id,
        "virtual_model": r.virtual_model,
        "targets": r.targets,
        "routing_strategy": r.routing_strategy,
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

#[derive(Debug, Deserialize)]
struct RouteRequest {
    id: Option<String>,
    virtual_model: String,
    targets: Vec<RouteTarget>,
    #[serde(default)]
    routing_strategy: Option<tiygate_core::routing::RoutingStrategyName>,
    #[serde(default)]
    model_metadata: Option<ModelMetadata>,
    enabled: Option<bool>,
}

#[derive(Debug, Serialize)]
struct RouteView {
    id: String,
    virtual_model: String,
    targets: Vec<RouteTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    routing_strategy: Option<tiygate_core::routing::RoutingStrategyName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_metadata: Option<ModelMetadata>,
    enabled: bool,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<Route> for RouteView {
    fn from(r: Route) -> Self {
        Self {
            id: r.id,
            virtual_model: r.virtual_model,
            targets: r.targets,
            routing_strategy: r.routing_strategy,
            model_metadata: r.model_metadata,
            enabled: r.enabled,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// Query parameters for `GET /admin/v1/routes` (paginated list).
#[derive(Debug, Deserialize)]
struct RouteListQuery {
    limit: Option<u32>,
    offset: Option<u32>,
}

async fn list_routes(
    State(state): State<AdminState>,
    axum::extract::Query(q): axum::extract::Query<RouteListQuery>,
) -> Result<Response, AdminError> {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0);
    let (routes, total) = state.store.list_routes_paginated(limit, offset).await?;
    let entries: Vec<RouteView> = routes.into_iter().map(Into::into).collect();
    Ok(Json(json!({
        "total": total,
        "limit": limit,
        "offset": offset,
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
    Ok(Json(RouteView::from(r)).into_response())
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
    Json(req): Json<RouteRequest>,
) -> Result<Response, AdminError> {
    let id = req.id.unwrap_or_else(|| Uuid::now_v7().to_string());
    let model_metadata = match req.model_metadata {
        Some(m) => Some(m),
        None => auto_resolve_model_metadata(&state, &req.virtual_model, &req.targets),
    };
    let r = state
        .store
        .upsert_route(
            &id,
            &req.virtual_model,
            &req.targets,
            req.routing_strategy,
            model_metadata.as_ref(),
            req.enabled.unwrap_or(true),
        )
        .await?;
    let snap = route_snapshot(&r);
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "upsert",
        "route",
        &r.id,
        &audit_details(None, Some(&snap)),
    )
    .await;
    Ok((StatusCode::CREATED, Json(RouteView::from(r))).into_response())
}

async fn update_route(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    Json(req): Json<RouteRequest>,
) -> Result<Response, AdminError> {
    let before = state
        .store
        .get_route(&id)
        .await
        .ok()
        .flatten()
        .map(|r| route_snapshot(&r));
    let model_metadata = match req.model_metadata {
        Some(m) => Some(m),
        None => auto_resolve_model_metadata(&state, &req.virtual_model, &req.targets),
    };
    let r = state
        .store
        .upsert_route(
            &id,
            &req.virtual_model,
            &req.targets,
            req.routing_strategy,
            model_metadata.as_ref(),
            req.enabled.unwrap_or(true),
        )
        .await?;
    let snap = route_snapshot(&r);
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "upsert",
        "route",
        &r.id,
        &audit_details(before.as_ref(), Some(&snap)),
    )
    .await;
    Ok(Json(RouteView::from(r)).into_response())
}

async fn delete_route(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Result<Response, AdminError> {
    let before = state
        .store
        .get_route(&id)
        .await
        .ok()
        .flatten()
        .map(|r| route_snapshot(&r));
    state.store.delete_route(&id).await?;
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "delete",
        "route",
        &id,
        &audit_details(before.as_ref(), None),
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
    let report = state
        .store
        .import_config(&req.config, &req.master_key, &req.selection)
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
        }),
    )
    .await;
    Ok(Json(report).into_response())
}

// ---- settings ----

fn settings_response(state: &AdminState, rows: Vec<(String, String)>) -> Response {
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
    Json(json!({
        "settings": map,
        "database": {
            "kind": database_kind,
        },
    }))
    .into_response()
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

#[derive(Debug, Deserialize)]
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

    for (key, val) in &req.settings {
        let s = match val {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
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

        if is_encrypted_key(key) {
            state.store.set_setting_encrypted(key, &s).await?;
        } else {
            state.store.set_setting(key, &s).await?;
        }
    }

    let before_val = serde_json::Value::Object(before_map);
    let after_val = serde_json::Value::Object(after_map);
    let details = audit_details(Some(&before_val), Some(&after_val));
    let _ = tiygate_store::audit::record(
        state.pool.as_ref(),
        "admin",
        "upsert",
        "settings",
        "bulk",
        &details,
    )
    .await;
    // Return the fresh redacted view.
    let rows = state.store.list_settings().await?;
    Ok(settings_response(&state, rows))
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
        assert_eq!(
            parsed
                .five_hour
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(27.0)
        );
        assert_eq!(
            parsed.five_hour.as_ref().and_then(|window| window.reset_at),
            Some(1_782_770_922)
        );
        assert_eq!(
            parsed
                .seven_day
                .as_ref()
                .and_then(|window| window.used_percent),
            Some(4.0)
        );
        assert_eq!(
            parsed.seven_day.as_ref().and_then(|window| window.reset_at),
            Some(1_000_600)
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
        assert_eq!(
            parsed.five_hour.and_then(|window| window.used_percent),
            Some(100.0)
        );
        assert_eq!(
            parsed.seven_day.and_then(|window| window.used_percent),
            Some(0.0)
        );
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
