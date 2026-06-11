//! Admin API OAuth 2.0 authorization-code flow handlers.
//!
//! Implements the three HTTP endpoints the design doc §4.5 calls
//! out for the OAuth admin callback surface:
//!
//! * `POST /admin/v1/oauth/start` — accept a `provider_id`, look
//!   up the provider's OAuth config, mint a `state` CSRF nonce +
//!   PKCE code-verifier, and return the authorization URL the
//!   caller must redirect the user-agent to. The `state` is
//!   stashed in `AdminState::oauth_pending` so the callback can
//!   validate the round-trip.
//!
//! * `GET /admin/v1/oauth/callback` — receive the provider's
//!   redirect (with `code` + `state` query params), look up the
//!   pending flow, exchange the code for an access token, persist
//!   the encrypted refresh-token metadata to the provider row,
//!   and return a small JSON summary.
//!
//! * `POST /admin/v1/oauth/refresh` — for a provider already
//!   configured with a stored refresh token, run the OAuth
//!   refresh flow to mint a new access token without
//!   user-interaction. Used for background jobs and long-running
//!   admin operations.
//!
//! ## CSRF / state hygiene
//!
//! The `state` nonce is single-use: the entry is removed from
//! `oauth_pending` as soon as the callback validates the
//! incoming query param.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use oauth2::PkceCodeVerifier;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use tiygate_providers::oauth::{OAuthAuthApplier, OAuthConfig, OAuthOutcome};
use tiygate_store::models::AuthMode;

use crate::state::{AdminState, OAuthPendingFlow};

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/admin/v1/oauth/start", post(start_oauth))
        .route("/admin/v1/oauth/callback", get(callback_oauth))
        .route("/admin/v1/oauth/refresh", post(refresh_oauth))
}

/// In-memory map of pending OAuth flows. Replaces the
/// `parking_lot::RwLock<HashMap<...>>` from earlier drafts
/// because the admin handlers are async and the lock must be
/// `Send + Sync` across `.await` points. The map is process-local;
/// multi-replica deployments must place an external store
/// (Redis, DB) behind this — Phase 5+.
pub type PendingFlowMap = Arc<Mutex<HashMap<String, OAuthPendingFlow>>>;

// `OAuthPendingFlow` is defined in `crate::state` and re-used
// here so the same struct flows through `state.oauth_pending`,
// the `start` handler, and the `callback` handler. Keeping a
// single definition avoids accidental drift between the writer
// (start) and the reader (callback).

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

    // Build the OAuthConfig from the persisted metadata.
    let oauth_meta = provider
        .metadata_json
        .get("oauth")
        .cloned()
        .ok_or_else(|| ApiError::not_found("provider has no oauth metadata"))?;
    let config: OAuthConfig = serde_json::from_value(oauth_meta)
        .map_err(|e| ApiError::bad_request(format!("invalid oauth metadata: {e}")))?;

    // Build the applier and mint the auth URL + PKCE verifier.
    let applier = OAuthAuthApplier::new(config);
    let (url, pkce_verifier) = applier
        .start()
        .await
        .map_err(|e| ApiError::internal(format!("oauth start failed: {e}")))?;

    // Mint a `state` CSRF nonce and stash the pending flow.
    let csrf_state = mint_state();
    state.oauth_pending.lock().await.insert(
        csrf_state.clone(),
        OAuthPendingFlow {
            provider_id: req.provider_id.clone(),
            verifier: pkce_verifier.secret().to_string(),
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

/// `GET /admin/v1/oauth/callback?code=…&state=…` — receive the
/// provider redirect and complete the flow.
async fn callback_oauth(
    State(state): State<AdminState>,
    Query(q): Query<OauthCallbackQuery>,
) -> Result<Json<OauthCallbackResponse>, ApiError> {
    // Validate `state` against the pending-flow map. The lookup
    // is single-shot: a second callback with the same `state`
    // will be rejected (replay protection).
    let pending = state.oauth_pending.lock().await.remove(&q.state);
    let pending = pending.ok_or_else(|| ApiError::bad_request("invalid or expired `state`"))?;

    // Build a synthetic `RoutingTarget` so we can reuse
    // `OAuthAuthApplier::exchange`. The label defaults to the
    // provider id; that is sufficient for the applier to find
    // the in-memory token cache.
    let target = tiygate_core::routing::RoutingTarget {
        provider_id: pending.provider_id.clone(),
        model_id: pending.provider_id.clone(),
        api_base: String::new(),
        api_key: String::new(),
        api_protocol: tiygate_core::ProtocolEndpoint::new(
            tiygate_core::ProtocolSuite::OpenAiCompatible,
            "chat-completions",
            "v1",
        ),
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        weight: 1.0,
    };

    // Reconstruct the PKCE verifier. The applier's `exchange` takes
    // ownership of the verifier, so we need to recreate the
    // `PkceCodeVerifier` from the secret string.
    let pkce_verifier = PkceCodeVerifier::new(pending.verifier.clone());

    // Look up the provider's OAuth config and build the applier
    // (the same one used by `start_oauth`).
    let provider = state
        .store
        .get_provider(&pending.provider_id)
        .await
        .map_err(|e| ApiError::internal(format!("lookup provider: {e}")))?
        .ok_or_else(|| ApiError::not_found("provider vanished during oauth flow"))?;
    let oauth_meta = provider
        .metadata_json
        .get("oauth")
        .cloned()
        .ok_or_else(|| ApiError::not_found("provider oauth metadata vanished"))?;
    let config: OAuthConfig = serde_json::from_value(oauth_meta)
        .map_err(|e| ApiError::bad_request(format!("invalid oauth metadata: {e}")))?;
    let applier = OAuthAuthApplier::new(config);

    let outcome = applier
        .exchange(&target, &q.code, pkce_verifier)
        .await
        .map_err(|e| ApiError::internal(format!("oauth exchange failed: {e}")))?;

    let (access_token, refresh_token, expires_in) = match outcome {
        OAuthOutcome::Token {
            access_token,
            refresh_token,
            expires_in,
        } => (access_token, refresh_token, expires_in),
        OAuthOutcome::RedirectUrl(_) => {
            return Err(ApiError::internal(
                "oauth applier returned RedirectUrl from exchange (unexpected)",
            ));
        }
    };

    // Persist the refresh-token metadata (encrypted at rest by
    // the `DbConfigStore`). The access token itself is *not*
    // persisted — it lives in the in-memory cache of the
    // `OAuthAuthApplier` instance held by the data plane.
    let meta_json = json!({
        "refresh_token": refresh_token,
        "expires_in_s": expires_in.map(|d| d.as_secs()),
    });
    let meta_str = serde_json::to_string(&meta_json)
        .map_err(|e| ApiError::internal(format!("serialise oauth meta: {e}")))?;
    state
        .store
        .set_provider_oauth_meta(&pending.provider_id, &meta_str)
        .await
        .map_err(|e| ApiError::internal(format!("persist oauth meta: {e}")))?;

    Ok(Json(OauthCallbackResponse {
        provider_id: pending.provider_id,
        access_token: Some(access_token),
        expires_in_s: expires_in.map(|d| d.as_secs()),
    }))
}

#[derive(Debug, Deserialize)]
struct OauthCallbackQuery {
    code: String,
    state: String,
}

#[derive(Debug, Serialize)]
struct OauthCallbackResponse {
    provider_id: String,
    access_token: Option<String>,
    expires_in_s: Option<u64>,
}

/// `POST /admin/v1/oauth/refresh` — refresh an existing OAuth
/// access token. Body: `{ "provider_id": "..." }`. Response:
/// `{ "provider_id": "...", "access_token": "...",
/// "expires_in_s": ... }`.
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
    let oauth_meta = provider
        .metadata_json
        .get("oauth")
        .cloned()
        .ok_or_else(|| ApiError::not_found("provider has no oauth metadata"))?;
    let config: OAuthConfig = serde_json::from_value(oauth_meta)
        .map_err(|e| ApiError::bad_request(format!("invalid oauth metadata: {e}")))?;
    let applier = OAuthAuthApplier::new(config);
    let target = tiygate_core::routing::RoutingTarget {
        provider_id: req.provider_id.clone(),
        model_id: req.provider_id.clone(),
        api_base: String::new(),
        api_key: String::new(),
        api_protocol: tiygate_core::ProtocolEndpoint::new(
            tiygate_core::ProtocolSuite::OpenAiCompatible,
            "chat-completions",
            "v1",
        ),
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        weight: 1.0,
    };
    let outcome = applier
        .refresh(&target)
        .await
        .map_err(|e| ApiError::internal(format!("oauth refresh failed: {e}")))?;
    let (access_token, _refresh, expires_in) = match outcome {
        OAuthOutcome::Token {
            access_token,
            refresh_token,
            expires_in,
        } => (access_token, refresh_token, expires_in),
        OAuthOutcome::RedirectUrl(_) => {
            return Err(ApiError::internal(
                "oauth applier returned RedirectUrl from refresh (unexpected)",
            ));
        }
    };
    Ok(Json(RefreshOauthResponse {
        provider_id: req.provider_id,
        access_token: Some(access_token),
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
    access_token: Option<String>,
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
