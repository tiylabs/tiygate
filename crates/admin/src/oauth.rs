//! Admin API OAuth 2.0 authorization-code flow handlers.
//!
//! Implements the three HTTP endpoints the design doc §4.5 calls
//! out for the OAuth admin callback surface:
//!
//! * `POST /admin/v1/oauth/start` — accept a `provider_id`, look
//!   up the provider's OAuth preset by vendor, mint a `state` CSRF
//!   nonce + PKCE code-verifier, and return the authorization URL
//!   the caller must redirect the user-agent to. The `state` is
//!   stashed in `AdminState::oauth_pending` so the callback can
//!   validate the round-trip.
//!
//! * `GET /admin/v1/oauth/callback` — receive the provider's
//!   redirect (with `code` + `state` query params), look up the
//!   pending flow, exchange the code for tokens via the
//!   provider-specific token endpoint (form or JSON body), persist
//!   the encrypted refresh-token metadata to the provider row,
//!   and return a small JSON summary.
//!
//! * `POST /admin/v1/oauth/refresh` — for a provider already
//!   configured with a stored refresh token, run the OAuth
//!   refresh flow to mint a new access token without
//!   user-interaction.
//!
//! ## CSRF / state hygiene
//!
//! The `state` nonce is single-use: the entry is removed from
//! `oauth_pending` as soon as the callback validates the
//! incoming query param.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use tiygate_auth::provider_oauth::{
    build_authorize_url, classify_refresh_failure, do_refresh_token, exchange_code, generate_pkce,
    preset_for_vendor, OAuthRefreshFailureKind,
};
use tiygate_store::models::{AuthMode, OAuthCredentialStatus};

use crate::state::{AdminState, OAuthPendingFlow};

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/admin/v1/oauth/start", post(start_oauth))
        .route("/admin/v1/oauth/callback", post(callback_oauth))
        .route("/admin/v1/oauth/refresh", post(refresh_oauth))
}

/// In-memory map of pending OAuth flows. Replaces the
/// `parking_lot::RwLock<HashMap<...>>` from earlier drafts
/// because the admin handlers are async and the lock must be
/// `Send + Sync` across `.await` points. The map is process-local;
/// multi-replica deployments must place an external store
/// (Redis, DB) behind this — Phase 5+.
pub type PendingFlowMap = Arc<Mutex<HashMap<String, OAuthPendingFlow>>>;

/// `POST /admin/v1/oauth/start` — kick off the auth-code flow.
///
/// Request body: `{ "provider_id": "..." }`. Response: `{ "url":
/// "...", "state": "..." }` — the operator should redirect the
/// user-agent to `url`. The `state` is already stashed
/// server-side; we echo it back to help the client correlate.
async fn start_oauth(
    State(state): State<AdminState>,
    Json(req): Json<StartOauthRequest>,
) -> Result<Json<StartOauthResponse>, ApiError> {
    // Look up the provider row.
    let provider = state
        .store
        .get_provider(&req.provider_id)
        .await
        .map_err(|e| ApiError::internal(format!("lookup provider: {e}")))?
        .ok_or_else(|| ApiError::not_found("provider not found"))?;

    // Only OAuth providers can kick off the flow.
    if !matches!(provider.auth_mode, AuthMode::OAuth) {
        return Err(ApiError::bad_request(
            "provider is not configured for OAuth",
        ));
    }

    // Look up the OAuth preset for the provider's vendor.
    let preset = preset_for_vendor(&provider.vendor).ok_or_else(|| {
        ApiError::bad_request(format!(
            "no built-in OAuth preset for vendor '{}'; \
             supported vendors: openai, anthropic, xai",
            provider.vendor
        ))
    })?;

    // Generate PKCE verifier + challenge.
    let (verifier, challenge) = generate_pkce();

    // Mint a `state` CSRF nonce.
    let csrf_state = mint_state();

    // Build the authorization URL.
    let url = build_authorize_url(&preset, &csrf_state, &challenge);

    // Stash the pending flow (provider_id + PKCE verifier).
    state.oauth_pending.lock().await.insert(
        csrf_state.clone(),
        OAuthPendingFlow {
            provider_id: req.provider_id.clone(),
            verifier,
        },
    );

    Ok(Json(StartOauthResponse {
        url,
        state: csrf_state,
    }))
}

#[derive(Debug, Deserialize)]
struct StartOauthRequest {
    provider_id: String,
}

#[derive(Debug, Serialize)]
struct StartOauthResponse {
    url: String,
    state: String,
}

/// `POST /admin/v1/oauth/callback` — complete the OAuth flow after
/// the user manually pastes the authorization code from the
/// provider's redirect URL.
///
/// The provider redirects the user's browser to the `redirect_url`
/// registered in the preset (e.g.
/// `http://localhost:1455/auth/callback?code=…&state=…`). Because
/// TiyGate does not serve that callback URL, the page will 404 —
/// the user copies the `code` (and `state`) query parameters from
/// the address bar and pastes them into the Admin Console, which
/// then calls this endpoint via the normal admin-API auth path.
async fn callback_oauth(
    State(state): State<AdminState>,
    Json(req): Json<OauthCallbackRequest>,
) -> Result<Json<OauthCallbackResponse>, ApiError> {
    callback_oauth_inner(state, req.code, req.state)
        .await
        .map(Json)
        .map_err(|e| ApiError::from_status_msg(e.0, &e.1))
}

#[derive(Debug, Deserialize)]
struct OauthCallbackRequest {
    code: String,
    state: String,
}

#[derive(Debug, Serialize)]
struct OauthCallbackResponse {
    provider_id: String,
    expires_in_s: Option<u64>,
}

/// Shared inner logic: validates `state`, exchanges the code for
/// tokens, and persists the refresh-token metadata.
async fn callback_oauth_inner(
    state: AdminState,
    code: String,
    csrf_state: String,
) -> Result<OauthCallbackResponse, (StatusCode, String)> {
    // Validate `state` against the pending-flow map. The lookup
    // is single-shot: a second callback with the same `state`
    // will be rejected (replay protection).
    let pending = state.oauth_pending.lock().await.remove(&csrf_state);
    let pending = pending.ok_or((
        StatusCode::BAD_REQUEST,
        "invalid or expired `state`".to_string(),
    ))?;

    // Look up the provider and its OAuth preset.
    let provider = state
        .store
        .get_provider(&pending.provider_id)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("lookup provider: {e}"),
            )
        })?
        .ok_or((
            StatusCode::NOT_FOUND,
            "provider vanished during oauth flow".to_string(),
        ))?;

    let preset = preset_for_vendor(&provider.vendor).ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        format!(
            "provider vendor '{}' has no built-in OAuth preset",
            provider.vendor
        ),
    ))?;

    // Exchange the authorization code for tokens.
    let http_client = reqwest::Client::new();
    let result = exchange_code(&preset, &code, &pending.verifier, &http_client)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("oauth exchange failed: {e}"),
            )
        })?;

    let access_token = result.access_token;
    let refresh_token = result.refresh_token.ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "oauth exchange returned no refresh_token".to_string(),
    ))?;
    let expires_in = result.expires_in;
    let account_id = result.account_id;
    let account_email = result.account_email;
    if provider.vendor == "openai" && account_id.is_none() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "OpenAI OAuth token did not include a ChatGPT account/workspace id".to_string(),
        ));
    }

    // Persist the refresh-token metadata (encrypted at rest by
    // the `DbConfigStore`). The access token itself is *not*
    // persisted — it lives in the in-memory cache of the
    // `OAuthTokenCache` held by the data plane.
    let meta_json = json!({
        "refresh_token": &refresh_token,
        "account_id": account_id.as_deref(),
        "account_email": account_email.as_deref(),
        "expires_in_s": expires_in.map(|d| d.as_secs()),
        "status": OAuthCredentialStatus::Healthy.as_str(),
        "status_checked_at": chrono::Utc::now().to_rfc3339(),
    });
    let meta_str = serde_json::to_string(&meta_json).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("serialise oauth meta: {e}"),
        )
    })?;
    state
        .store
        .set_provider_oauth_meta(&pending.provider_id, &meta_str)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("persist oauth meta: {e}"),
            )
        })?;

    // Keep the freshly exchanged access token hot. Without this seed the
    // first model discovery/API request immediately spends the refresh token.
    let cache_label = account_id.as_deref().unwrap_or("__provider__");
    tiygate_auth::provider_oauth::OAuthTokenCache::global().seed_tokens_with_identity(
        &pending.provider_id,
        cache_label,
        &access_token,
        &refresh_token,
        expires_in,
        tiygate_auth::provider_oauth::OAuthTokenIdentity {
            account_id: account_id.clone(),
            account_email: account_email.clone(),
        },
    );

    Ok(OauthCallbackResponse {
        provider_id: pending.provider_id,
        expires_in_s: expires_in.map(|d| d.as_secs()),
    })
}

/// `POST /admin/v1/oauth/refresh` — refresh an existing OAuth
/// access token. Body: `{ "provider_id": "..." }`. Response:
/// `{ "provider_id": "...", "expires_in_s": ... }`. Access tokens remain
/// server-side and are never returned to the Admin Console.
async fn refresh_oauth(
    State(state): State<AdminState>,
    Json(req): Json<RefreshOauthRequest>,
) -> Result<Json<RefreshOauthResponse>, ApiError> {
    let provider = state
        .store
        .get_provider(&req.provider_id)
        .await
        .map_err(|e| ApiError::internal(format!("lookup provider: {e}")))?
        .ok_or_else(|| ApiError::not_found("provider not found"))?;
    if !matches!(provider.auth_mode, AuthMode::OAuth) {
        return Err(ApiError::bad_request(
            "provider is not configured for OAuth",
        ));
    }

    let preset = preset_for_vendor(&provider.vendor).ok_or_else(|| {
        ApiError::bad_request(format!(
            "provider vendor '{}' has no built-in OAuth preset",
            provider.vendor
        ))
    })?;

    // Decrypt the stored OAuth metadata to get the refresh token.
    let oauth_meta_str = provider
        .oauth_meta_cleartext
        .as_deref()
        .filter(|meta| !meta.trim().is_empty())
        .ok_or_else(|| ApiError::bad_request("provider has no stored OAuth metadata"))?;
    let oauth_meta: serde_json::Value = serde_json::from_str(oauth_meta_str)
        .map_err(|e| ApiError::bad_request(format!("invalid OAuth metadata: {e}")))?;
    let refresh_token = oauth_meta
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ApiError::bad_request("OAuth metadata has no refresh_token"))?;

    // Perform the refresh.
    let http_client = reqwest::Client::new();
    let result = do_refresh_token(
        &preset.token_url,
        &preset.client_id,
        refresh_token,
        &preset.refresh_scopes,
        &preset.refresh_request_style,
        &http_client,
    )
    .await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            let kind = classify_refresh_failure(&error);
            let status = match kind {
                OAuthRefreshFailureKind::CredentialInvalid => OAuthCredentialStatus::Invalid,
                OAuthRefreshFailureKind::Transient => OAuthCredentialStatus::Error,
            };
            if let Err(status_error) = state
                .store
                .set_provider_oauth_status(&req.provider_id, status, Some(kind.status_reason()))
                .await
            {
                tracing::warn!(
                    provider = %req.provider_id,
                    error = %status_error,
                    "persisting OAuth refresh failure status failed"
                );
            }
            return Err(ApiError::internal(format!("oauth refresh failed: {error}")));
        }
    };

    let access_token = result.access_token;
    let new_refresh_token = result.refresh_token;
    let expires_in = result.expires_in;
    let stored_account_id = oauth_meta.get("account_id").and_then(|v| v.as_str());
    let account_id = result.account_id.as_deref().or(stored_account_id);
    let stored_account_email = oauth_meta.get("account_email").and_then(|v| v.as_str());
    let account_email = result.account_email.as_deref().or(stored_account_email);
    let effective_refresh_token = new_refresh_token.as_deref().unwrap_or(refresh_token);

    // Persist both rotated tokens and account identity. Keeping the identity
    // alongside the refresh token makes cold-start request scoping reliable.
    let meta_json = json!({
        "refresh_token": effective_refresh_token,
        "account_id": account_id,
        "account_email": account_email,
        "expires_in_s": expires_in.map(|d| d.as_secs()),
        "status": OAuthCredentialStatus::Healthy.as_str(),
        "status_checked_at": chrono::Utc::now().to_rfc3339(),
    });
    let meta_str = serde_json::to_string(&meta_json)
        .map_err(|e| ApiError::internal(format!("serialise oauth meta: {e}")))?;
    state
        .store
        .set_provider_oauth_meta(&req.provider_id, &meta_str)
        .await
        .map_err(|e| ApiError::internal(format!("persist oauth meta: {e}")))?;

    tiygate_auth::provider_oauth::OAuthTokenCache::global().seed_tokens_with_identity(
        &req.provider_id,
        account_id.unwrap_or("__provider__"),
        &access_token,
        effective_refresh_token,
        expires_in,
        tiygate_auth::provider_oauth::OAuthTokenIdentity {
            account_id: account_id.map(str::to_string),
            account_email: account_email.map(str::to_string),
        },
    );

    Ok(Json(RefreshOauthResponse {
        provider_id: req.provider_id,
        expires_in_s: expires_in.map(|d| d.as_secs()),
    }))
}

#[derive(Debug, Deserialize)]
struct RefreshOauthRequest {
    provider_id: String,
}

#[derive(Debug, Serialize)]
struct RefreshOauthResponse {
    provider_id: String,
    expires_in_s: Option<u64>,
}

// ---- error type ----

/// JSON-friendly error returned by the OAuth admin handlers.
/// We can't use `(StatusCode, String)` because axum already
/// has a blanket `IntoResponse` impl for tuples; instead we
/// own the conversion and return a structured `{ "error":
/// "..." }` body.
#[derive(Debug)]
enum ApiError {
    NotFound(String),
    BadRequest(String),
    Internal(String),
}

impl ApiError {
    fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }
    fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }
    fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
    /// Build an `ApiError` from a raw status code and message.
    /// Used by the JSON callback handler to convert the
    /// `(StatusCode, String)` tuple returned by
    /// [`callback_oauth_inner`].
    fn from_status_msg(status: StatusCode, msg: &str) -> Self {
        match status {
            StatusCode::NOT_FOUND => Self::NotFound(msg.to_string()),
            StatusCode::BAD_REQUEST => Self::BadRequest(msg.to_string()),
            _ => Self::Internal(msg.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}

/// Mint a random 16-byte URL-safe state nonce using the OS
/// CSPRNG. The nonce is the CSRF-protection token for the
/// authorization-code flow; the server stashes the matching
/// pending flow under the same value.
fn mint_state() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    let mut out = String::with_capacity(32);
    for b in buf.iter() {
        out.push_str(&format!("{:02x}", b));
    }
    out
}
