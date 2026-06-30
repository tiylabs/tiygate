//! OpenAI-compatible model discovery endpoints.
//!
//! Implements the baseline of `docs/models-endpoint-protocol.md`:
//!   * `GET /v1/models`            — list the tenant-visible virtual models.
//!   * `GET /v1/models/{model_id}` — fetch a single model card.
//!
//! This is the *baseline* surface: model capability / attribute /
//! pricing metadata is not yet sourced, so we only emit the four
//! OpenAI-required fields (`id`, `object`, `created`, `owned_by`).
//! The `Model` struct keeps a `#[serde(flatten)]` `extensions` map so
//! future revisions can attach `display_name`, `capabilities`,
//! `pricing`, `metadata`, … without changing the type or breaking
//! older clients (per protocol §8 "新增可选字段直接添加").
//!
//! The list of models is derived from the live routing table — every
//! virtual model the gateway can route to is discoverable here. When
//! the control plane publishes a new snapshot (admin CRUD), the list
//! reflects it on the next request via `AppState::current_config`.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ingress::AppState;

/// Default `owned_by` when a virtual model has no routing target to
/// infer the upstream owner from. Protocol §8 #5 mandates a non-null
/// fallback for required fields; `self` is the protocol's suggested
/// value for gateway-owned models.
const DEFAULT_OWNED_BY: &str = "self";

/// Hard cap on `limit`, per protocol §2.2 (`1..1000`).
const MAX_LIMIT: usize = 1000;

/// Process-stable registration timestamp (Unix seconds). The baseline
/// has no per-model registration time, so every model reports the
/// instant this process first served a models request. Caching it in a
/// `OnceLock` keeps `created` stable across requests within a process
/// (clients dislike a value that changes every poll) while still being
/// a real timestamp rather than a magic constant.
fn baseline_created() -> i64 {
    static CREATED: OnceLock<i64> = OnceLock::new();
    *CREATED.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    })
}

/// A single model card. Only the OpenAI-required fields are strongly
/// typed in the baseline; everything else flows through `extensions`
/// so future capability/pricing data can be attached without a struct
/// change or a breaking-version bump.
#[derive(Debug, Clone, Serialize)]
pub struct Model {
    /// Unique model id (the virtual model name).
    pub id: String,
    /// Always `"model"`.
    pub object: &'static str,
    /// Registration time (Unix seconds).
    pub created: i64,
    /// Owning party — inferred from the first routing target's
    /// `provider_id`, or `self` when no target is available.
    pub owned_by: String,
    /// Open extension area for future protocol fields (`display_name`,
    /// `capabilities`, `pricing`, `metadata`, …). Flattened into the
    /// top-level object; omitted entirely when empty so the baseline
    /// response stays minimal.
    #[serde(flatten, skip_serializing_if = "Map::is_empty")]
    pub extensions: Map<String, Value>,
}

impl Model {
    fn new(id: String, owned_by: String) -> Self {
        Self {
            id,
            object: "model",
            created: baseline_created(),
            owned_by,
            extensions: Map::new(),
        }
    }
}

/// `GET /v1/models` response envelope (protocol §3).
#[derive(Debug, Serialize)]
pub struct ListModelsResponse {
    /// Always `"list"`.
    pub object: &'static str,
    /// Model cards — always an array, never null.
    pub data: Vec<Model>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_id: Option<String>,
}

/// `GET /v1/models` query parameters (protocol §2.2). All optional;
/// when none are supplied the full visible list is returned (OpenAI
/// compatibility).
#[derive(Debug, Default, Deserialize)]
pub struct ListModelsQuery {
    /// Page size, clamped to `1..=1000`. Absent = return all.
    pub limit: Option<usize>,
    /// Cursor anchor — the `id` after which to start the page.
    pub after: Option<String>,
    /// Sort order by `created` (then `id` as a stable tiebreaker).
    /// `asc` | `desc`; defaults to `desc`.
    pub order: Option<String>,
    /// Filter by `owned_by`.
    pub owned_by: Option<String>,
}

/// OpenAI-compatible error body (protocol §7). Kept separate from the
/// data-plane `AppError` so the models endpoints emit the exact
/// `{message, type, param, code}` shape the protocol mandates without
/// disturbing the existing `/v1/chat/completions` error envelope.
#[derive(Debug)]
pub struct ModelsError {
    status: StatusCode,
    message: String,
    error_type: &'static str,
    param: Option<&'static str>,
    code: &'static str,
}

impl ModelsError {
    fn not_found(model_id: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: format!("Model '{model_id}' not found"),
            error_type: "not_found_error",
            param: Some("model_id"),
            code: "model_not_found",
        }
    }

    fn invalid_param(message: impl Into<String>, param: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            error_type: "invalid_request_error",
            param: Some(param),
            code: "invalid_param",
        }
    }
}

impl IntoResponse for ModelsError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "error": {
                "message": self.message,
                "type": self.error_type,
                "param": self.param,
                "code": self.code,
            }
        });
        (self.status, Json(body)).into_response()
    }
}

/// Collect the current visible models from the live routing table.
/// Each virtual model name becomes one `Model`; `owned_by` is inferred
/// from the first routing target's `provider_id`.
fn collect_models(state: &AppState) -> Vec<Model> {
    let config = state.current_config();
    let catalog = state.model_catalog.as_ref().map(|s| s.snapshot());
    config
        .routing_table
        .routes
        .iter()
        .map(|(virtual_model, entry)| {
            let owned_by = entry
                .targets
                .first()
                .map(|t| t.provider_id.clone())
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| DEFAULT_OWNED_BY.to_string());
            let mut model = Model::new(virtual_model.clone(), owned_by);
            if let Some(catalog) = &catalog {
                if let Some(meta) = catalog.get_model(virtual_model) {
                    model.extensions = meta.to_model_extensions();
                    model.owned_by = meta.lab_id.clone();
                }
            }
            model
        })
        .collect()
}

/// Handle `GET /v1/models`.
pub async fn handle_list_models(
    State(state): State<AppState>,
    Query(query): Query<ListModelsQuery>,
) -> Result<Response, ModelsError> {
    // Validate `order` up front so a typo surfaces as a 400 rather
    // than being silently coerced to the default.
    let descending = match query.order.as_deref() {
        None | Some("desc") => true,
        Some("asc") => false,
        Some(other) => {
            return Err(ModelsError::invalid_param(
                format!("invalid order '{other}', expected 'asc' or 'desc'"),
                "order",
            ));
        }
    };

    let mut models = collect_models(&state);

    // Optional `owned_by` filter.
    if let Some(owner) = query.owned_by.as_deref() {
        models.retain(|m| m.owned_by == owner);
    }

    // Stable ordering. `created` is uniform in the baseline, so `id`
    // is the effective sort key; we still honor the requested
    // direction so the cursor semantics are deterministic.
    models.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
    if descending {
        models.reverse();
    }

    // Cursor: drop everything up to and including the `after` id.
    if let Some(after) = query.after.as_deref() {
        if let Some(pos) = models.iter().position(|m| m.id == after) {
            models.drain(..=pos);
        } else {
            return Err(ModelsError::invalid_param(
                format!("cursor 'after={after}' does not match any model"),
                "after",
            ));
        }
    }

    // Apply `limit` (clamped) and compute `has_more`.
    let mut has_more = false;
    if let Some(limit) = query.limit {
        let limit = limit.clamp(1, MAX_LIMIT);
        if models.len() > limit {
            has_more = true;
            models.truncate(limit);
        }
    }

    let first_id = models.first().map(|m| m.id.clone());
    let last_id = models.last().map(|m| m.id.clone());

    let response = ListModelsResponse {
        object: "list",
        data: models,
        has_more: Some(has_more),
        first_id,
        last_id,
    };

    Ok(Json(response).into_response())
}

/// Handle `GET /v1/models/{model_id}`.
///
/// Lookup is case-insensitive (protocol §2.3) but the response echoes
/// the model's stored id verbatim.
pub async fn handle_get_model(
    State(state): State<AppState>,
    Path(model_id): Path<String>,
) -> Result<Response, ModelsError> {
    let model = collect_models(&state)
        .into_iter()
        .find(|m| m.id.eq_ignore_ascii_case(&model_id));

    match model {
        Some(model) => Ok(Json(model).into_response()),
        None => Err(ModelsError::not_found(&model_id)),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn model_serializes_baseline_fields_only() {
        let m = Model::new("gpt-4o".to_string(), "openai".to_string());
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["id"], "gpt-4o");
        assert_eq!(v["object"], "model");
        assert_eq!(v["owned_by"], "openai");
        assert!(v["created"].is_i64());
        // No extension keys leak into the baseline response.
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 4);
    }

    #[test]
    fn model_extensions_flatten_to_top_level() {
        let mut m = Model::new("claude".to_string(), "anthropic".to_string());
        m.extensions
            .insert("display_name".to_string(), Value::from("Claude"));
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["display_name"], "Claude");
    }

    #[test]
    fn list_response_shape() {
        let resp = ListModelsResponse {
            object: "list",
            data: vec![Model::new("a".to_string(), "self".to_string())],
            has_more: Some(false),
            first_id: Some("a".to_string()),
            last_id: Some("a".to_string()),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["object"], "list");
        assert_eq!(v["data"][0]["id"], "a");
        assert_eq!(v["has_more"], false);
    }

    #[test]
    fn not_found_error_body_matches_protocol() {
        let err = ModelsError::not_found("foo-bar");
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
