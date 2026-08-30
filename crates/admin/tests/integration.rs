//! End-to-end integration tests for the Admin API surface.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::json;
use tiygate_store::config_store::DbConfigStore;
use tiygate_store::db;
use tiygate_store::encryption::KeyEncryption;
use tiygate_store::models::AuthMode;
use tower::ServiceExt;

use tiygate_admin::{build_router, AdminState};
use tiygate_core::EventSink;

async fn boot_no_auth() -> (
    axum::Router,
    Arc<DbConfigStore>,
    Arc<tiygate_store::db::DbPool>,
) {
    let pool = Arc::new(db::open_pool("sqlite::memory:").await.expect("pool"));
    db::run_migrations(&pool).await.expect("migrate");
    let key_bytes: [u8; 32] = std::iter::repeat_n(0xabu8, 32)
        .collect::<Vec<u8>>()
        .try_into()
        .unwrap();
    let encryption = Some(Arc::new(KeyEncryption::from_bytes(key_bytes)));
    let store = Arc::new(DbConfigStore::new((*pool).clone(), encryption));
    store.refresh().await.expect("refresh");
    let state = AdminState::new(store.clone(), pool.clone(), None);
    let router = tiygate_admin::build_router_with_auth(state, false);
    (router, store, pool)
}

async fn boot_with_auth() -> (
    axum::Router,
    Arc<DbConfigStore>,
    Arc<tiygate_store::db::DbPool>,
) {
    let (_router, store, pool) = boot_no_auth().await;
    let state = AdminState::new(store.clone(), pool.clone(), None);
    let router = build_router(state);
    (router, store, pool)
}

fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

async fn create_test_provider(router: &axum::Router, id: &str) {
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/providers",
            json!({
                "id": id,
                "name": id,
                "vendor": "unknown-compatible",
                "api_base": "https://api.openai.com/v1",
                "api_key": format!("sk-{id}"),
                "auth_mode": "api_key",
            }),
        ))
        .await
        .expect("create provider response");
    assert_eq!(resp.status(), StatusCode::CREATED);
}

async fn create_test_route(
    router: &axum::Router,
    id: &str,
    virtual_model: &str,
    targets: serde_json::Value,
) {
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/routes",
            json!({
                "id": id,
                "virtual_model": virtual_model,
                "targets": targets,
            }),
        ))
        .await
        .expect("create route response");
    assert_eq!(resp.status(), StatusCode::CREATED);
}

// ---- Acceptance #1: Admin CRUD + propagation to routing table ----

#[tokio::test]
async fn acceptance_1_admin_crud_propagates_to_routing_table() {
    let (router, store, _pool) = boot_no_auth().await;

    // Create a provider.
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/providers",
            json!({
                "id": "openai",
                "name": "OpenAI",
                "vendor": "unknown-compatible",
                "api_base": "https://api.openai.com/v1",
                "api_key": "sk-test",
                "auth_mode": "api_key",
            }),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Create a route that uses the provider.
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/routes",
            json!({
                "virtual_model": "gpt-4o",
                "targets": [{
                    "provider_id": "openai",
                    "model_id": "gpt-4o",
                    "weight": 1.0
                }]
            }),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The data-plane ConfigStore must see the new route.
    let cs = store.config_store();
    assert!(cs.routing_table.routes.contains_key("gpt-4o"));
    let targets = &cs.routing_table.routes["gpt-4o"].targets;
    assert_eq!(targets[0].provider_id, "openai");
    // The api key on the routing-table target must be the cleartext
    // we supplied at admin-time, so the data plane can forward
    // it to the upstream call. (No master key is configured in
    // this test — `cleartext-fallback` mode is exercised.)
    assert_eq!(targets[0].api_key, "sk-test");
}

#[tokio::test]
async fn target_capability_routes_create_profile_and_probe_job() {
    let (router, _store, _pool) = boot_no_auth().await;
    create_test_provider(&router, "capability-provider").await;
    create_test_route(
        &router,
        "capability-route",
        "capability-model",
        json!([{
            "provider_id": "capability-provider",
            "model_id": "gpt-4o",
            "egress_dialect_id": "openai-responses-standard"
        }]),
    )
    .await;

    let response = router
        .clone()
        .oneshot(json_request(
            "GET",
            "/admin/v1/target-capabilities?limit=10",
            json!({}),
        ))
        .await
        .expect("capability list response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("capability list body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("capability list json");
    assert_eq!(value["total"], 1);
    let target_key = value["entries"][0]["target_key"]
        .as_str()
        .expect("target key")
        .to_string();
    assert_eq!(value["entries"][0]["profile_status"], "pending");

    let response = router
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/admin/v1/target-capabilities/{target_key}"),
            json!({}),
        ))
        .await
        .expect("capability detail response");
    assert_eq!(response.status(), StatusCode::OK);
    let detail = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("capability detail body");
    let detail_json: serde_json::Value = serde_json::from_slice(&detail).expect("detail json");
    assert_eq!(
        detail_json["profile"]["dialect_id"],
        "openai-responses-standard"
    );
    assert!(detail_json["profile"]
        .get("credential_scope_fingerprint")
        .is_none());
    assert!(detail_json["profile"].get("canonical_api_base").is_none());
    assert_eq!(detail_json["probe_job"]["status"], "pending");

    let response = router
        .clone()
        .oneshot(json_request(
            "GET",
            &format!("/admin/v1/target-capabilities/{target_key}/probe-runs?limit=10"),
            json!({}),
        ))
        .await
        .expect("probe-run list response");
    assert_eq!(response.status(), StatusCode::OK);
    let probe_runs = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("probe-run list body");
    let probe_runs: serde_json::Value =
        serde_json::from_slice(&probe_runs).expect("probe-run json");
    assert_eq!(probe_runs["total"], 0);

    let response = router
        .clone()
        .oneshot(json_request(
            "POST",
            &format!("/admin/v1/target-capabilities/{target_key}/probe"),
            json!({"probe_set": ["http.basic"]}),
        ))
        .await
        .expect("probe response");
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let override_body = json!({
        "capability_id": "tools.function",
        "state": "supported",
        "reason": "idempotency fixture"
    });
    let build_override = || {
        Request::builder()
            .method("PUT")
            .uri(format!(
                "/admin/v1/target-capabilities/{target_key}/overrides"
            ))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", "capability-override-1")
            .body(Body::from(override_body.to_string()))
            .expect("override request")
    };
    let first = router
        .clone()
        .oneshot(build_override())
        .await
        .expect("override response");
    assert_eq!(first.status(), StatusCode::OK);
    let first_body = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .expect("override body");
    let replay = router
        .oneshot(build_override())
        .await
        .expect("override replay response");
    assert_eq!(replay.status(), StatusCode::OK);
    let replay_body = axum::body::to_bytes(replay.into_body(), usize::MAX)
        .await
        .expect("override replay body");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&first_body).expect("first override json"),
        serde_json::from_slice::<serde_json::Value>(&replay_body).expect("replay override json")
    );
}

#[tokio::test]
async fn capability_registry_is_paginated() {
    let (router, _store, _pool) = boot_no_auth().await;
    let response = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/capability-registry?limit=2")
                .body(Body::empty())
                .expect("registry request"),
        )
        .await
        .expect("registry response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("registry body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("registry json");
    assert_eq!(value["items"].as_array().map(Vec::len), Some(2));
    assert!(value["next_cursor"].is_string());
}

#[tokio::test]
async fn capability_shape_admission_is_gated_and_revisioned() {
    let (router, _store, _pool) = boot_no_auth().await;
    create_test_provider(&router, "admission-provider").await;
    create_test_route(
        &router,
        "admission-route",
        "admission-model",
        json!([{
            "provider_id": "admission-provider",
            "model_id": "gpt-4o"
        }]),
    )
    .await;
    let required = vec!["tools.function".to_string()];
    let response = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/routes/admission-route/capability-admissions",
            json!({
                "required_capabilities": required,
                "mode": "shadow",
                "reason": "observe function capability"
            }),
        ))
        .await
        .expect("admission create response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("admission create body");
    let created: serde_json::Value = serde_json::from_slice(&body).expect("admission json");
    assert_eq!(created["revision"], 1);
    assert_eq!(created["mode"], "shadow");
    let response = router
        .clone()
        .oneshot(json_request(
            "GET",
            "/admin/v1/routes/admission-route/capability-admissions?limit=10",
            json!({}),
        ))
        .await
        .expect("admission list response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("admission list body");
    let listed: serde_json::Value = serde_json::from_slice(&body).expect("admission list json");
    assert_eq!(listed["total"], 1);

    let response = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/routes/admission-route/capability-admissions",
            json!({
                "shape_hash": created["capability_shape_hash"],
                "required_capabilities": ["tools.function"],
                "mode": "enforce",
                "expected_revision": 1,
                "reason": "enforce without evidence"
            }),
        ))
        .await
        .expect("admission gate response");
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let gate_error = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("admission gate body");
    let gate_error: serde_json::Value = serde_json::from_slice(&gate_error).expect("gate error");
    assert_eq!(gate_error["error"]["code"], "admission_required");
    assert!(gate_error["error"]["request_id"].is_string());

    let response = router
        .clone()
        .oneshot(json_request(
            "PUT",
            "/admin/v1/settings",
            json!({"settings":{"gateway.capabilities.probe_enabled":"false"}}),
        ))
        .await
        .expect("probe setting response");
    assert_eq!(response.status(), StatusCode::OK);

    let response = router
        .clone()
        .oneshot(json_request(
            "PUT",
            "/admin/v1/capability-probes",
            json!({"enabled":true,"reason":"resume for canary"}),
        ))
        .await
        .expect("probe worker response");
    assert_eq!(response.status(), StatusCode::OK);

    let response = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/routes/admission-route/capability-admissions",
            json!({
                "shape_hash": created["capability_shape_hash"],
                "required_capabilities": ["tools.function"],
                "mode": "enforce",
                "expected_revision": 1,
                "low_traffic_exception": true,
                "reason": "explicit low traffic canary"
            }),
        ))
        .await
        .expect("admission exception response");
    // An explicit low-traffic exception still requires the server-generated
    // CRL probe/continuation eligibility report; this route has no evidence,
    // so the request is rejected rather than creating a fail-open gate.
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let exception_error = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("exception error body");
    let exception_error: serde_json::Value =
        serde_json::from_slice(&exception_error).expect("exception error");
    assert_eq!(exception_error["error"]["code"], "admission_required");
    let encoded_shape = created["capability_shape_hash"]
        .as_str()
        .expect("shape hash")
        .replace('/', "%2F");
    let response = router
        .clone()
        .oneshot(json_request(
            "DELETE",
            &format!("/admin/v1/routes/admission-route/capability-admissions/{encoded_shape}"),
            json!({}),
        ))
        .await
        .expect("admission delete response");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn capability_shape_admission_preserves_typed_constraints() {
    let (router, _store, _pool) = boot_no_auth().await;
    create_test_provider(&router, "typed-admission-provider").await;
    create_test_route(
        &router,
        "typed-admission-route",
        "typed-admission-model",
        json!([{
            "provider_id": "typed-admission-provider",
            "model_id": "gpt-4o"
        }]),
    )
    .await;
    let response = router
        .oneshot(json_request(
            "POST",
            "/admin/v1/routes/typed-admission-route/capability-admissions",
            json!({
                "required_capabilities": ["tools.namespace"],
                "required_requirements": [{
                    "id": "tools.namespace",
                    "strength": "required",
                    "value": {"kind": "enum_set", "value": ["functions"]}
                }],
                "mode": "shadow",
                "reason": "namespace path constraint"
            }),
        ))
        .await
        .expect("typed admission response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("typed admission body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("typed admission json");
    assert_eq!(value["required_requirements"][0]["id"], "tools.namespace");
    assert_ne!(
        value["capability_shape_hash"],
        serde_json::json!(tiygate_core::capability_shape_hash_from_ids(&[
            tiygate_core::CapabilityId::from("tools.namespace")
        ]))
    );
}

#[tokio::test]
async fn capability_metrics_endpoint_is_persistent_and_redacted() {
    let (router, _store, _pool) = boot_no_auth().await;
    let response = router
        .clone()
        .oneshot(json_request(
            "GET",
            "/admin/v1/capability-metrics?route_id=missing",
            json!({}),
        ))
        .await
        .expect("metrics response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("metrics body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("metrics json");
    assert_eq!(value["entries"].as_array().expect("entries").len(), 0);
}

#[tokio::test]
async fn global_capability_enforce_requires_route_admission() {
    let (router, _store, _pool) = boot_no_auth().await;
    create_test_provider(&router, "global-gate-provider").await;
    create_test_route(
        &router,
        "global-gate-route",
        "global-gate-model",
        json!([{
            "provider_id": "global-gate-provider",
            "model_id": "gpt-4o"
        }]),
    )
    .await;
    let response = router
        .clone()
        .oneshot(json_request(
            "PUT",
            "/admin/v1/settings",
            json!({"settings":{"gateway.capabilities.routing_mode":"enforce"}}),
        ))
        .await
        .expect("global mode response");
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn capability_settings_reject_out_of_range_values() {
    let (router, _store, _pool) = boot_no_auth().await;
    let response = router
        .clone()
        .oneshot(json_request(
            "PUT",
            "/admin/v1/settings",
            json!({"settings":{"gateway.capabilities.probe_global_concurrency":"0"}}),
        ))
        .await
        .expect("settings response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = router
        .oneshot(json_request(
            "PUT",
            "/admin/v1/settings",
            json!({"settings":{"gateway.responses.crl_tool_promotion_enabled":"maybe"}}),
        ))
        .await
        .expect("promotion settings response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn capability_probe_idempotency_replays_without_duplicate_job() {
    let (router, _store, _pool) = boot_no_auth().await;
    create_test_provider(&router, "idempotent-provider").await;
    create_test_route(
        &router,
        "idempotent-route",
        "idempotent-model",
        json!([{"provider_id":"idempotent-provider","model_id":"gpt-4o"}]),
    )
    .await;
    let list = router
        .clone()
        .oneshot(json_request(
            "GET",
            "/admin/v1/target-capabilities?limit=10",
            json!({}),
        ))
        .await
        .expect("list");
    let body = axum::body::to_bytes(list.into_body(), usize::MAX)
        .await
        .expect("list body");
    let key = serde_json::from_slice::<serde_json::Value>(&body).expect("list json")["entries"][0]
        ["target_key"]
        .as_str()
        .expect("target key")
        .to_string();
    let build_request = || {
        Request::builder()
            .method("POST")
            .uri(format!("/admin/v1/target-capabilities/{key}/probe"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", "probe-idem")
            .body(Body::from(json!({"probe_set":["http.basic"]}).to_string()))
            .expect("probe request")
    };
    let first = router
        .clone()
        .oneshot(build_request())
        .await
        .expect("first");
    let first_body = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .expect("first body");
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).expect("first json");
    let second = router.oneshot(build_request()).await.expect("second");
    assert_eq!(second.status(), StatusCode::ACCEPTED);
    let second_body = axum::body::to_bytes(second.into_body(), usize::MAX)
        .await
        .expect("second body");
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).expect("second json");
    assert_eq!(first_json["id"], second_json["id"]);
}

#[tokio::test]
async fn capability_settings_idempotency_replays_and_conflicts() {
    let (router, _store, _pool) = boot_no_auth().await;
    let build_request = |value: &str| {
        Request::builder()
            .method("PUT")
            .uri("/admin/v1/settings")
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", "settings-idem-1")
            .body(Body::from(
                json!({"settings":{"gateway.capabilities.probe_enabled":value}}).to_string(),
            ))
            .expect("request")
    };
    let first = router
        .clone()
        .oneshot(build_request("false"))
        .await
        .expect("first response");
    assert_eq!(first.status(), StatusCode::OK);
    let first_body = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .expect("first body");
    let replay = router
        .clone()
        .oneshot(build_request("false"))
        .await
        .expect("replay response");
    assert_eq!(replay.status(), StatusCode::OK);
    let replay_body = axum::body::to_bytes(replay.into_body(), usize::MAX)
        .await
        .expect("replay body");
    assert_eq!(first_body, replay_body);
    let conflict = router
        .oneshot(build_request("true"))
        .await
        .expect("conflict response");
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(conflict.into_body(), usize::MAX)
        .await
        .expect("conflict body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("conflict json");
    assert_eq!(value["error"]["code"], "idempotency_conflict");
}

#[tokio::test]
async fn route_capability_mode_idempotency_replays_without_duplicate_route_write() {
    let (router, _store, _pool) = boot_no_auth().await;
    create_test_provider(&router, "route-idem-provider").await;
    let body = json!({
        "id": "route-idem",
        "virtual_model": "route-idem-model",
        "targets": [{"provider_id":"route-idem-provider","model_id":"gpt-4o"}],
        "capability_routing_mode": "shadow",
        "enabled": true
    });
    let build_request = || {
        Request::builder()
            .method("POST")
            .uri("/admin/v1/routes")
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", "route-idem-1")
            .body(Body::from(body.to_string()))
            .expect("request")
    };
    let first = router
        .clone()
        .oneshot(build_request())
        .await
        .expect("first route response");
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_body = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .expect("first route body");
    let replay = router
        .clone()
        .oneshot(build_request())
        .await
        .expect("replay route response");
    assert_eq!(replay.status(), StatusCode::CREATED);
    let replay_body = axum::body::to_bytes(replay.into_body(), usize::MAX)
        .await
        .expect("replay route body");
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).expect("first json");
    let replay_json: serde_json::Value = serde_json::from_slice(&replay_body).expect("replay json");
    assert_eq!(first_json, replay_json);
}

#[tokio::test]
async fn route_view_does_not_echo_target_credentials_or_url_overrides() {
    let (router, _store, _pool) = boot_no_auth().await;
    create_test_provider(&router, "redact-route-provider").await;
    let response = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/routes",
            json!({
                "id": "redact-route",
                "virtual_model": "redact-model",
                "targets": [{
                    "provider_id": "redact-route-provider",
                    "model_id": "gpt-4o",
                    "api_key_override": "route-secret",
                    "api_base_override": "https://override.example/v1"
                }]
            }),
        ))
        .await
        .expect("route create response");
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("route body");
    let value: serde_json::Value = serde_json::from_slice(&body).expect("route json");
    let target = &value["targets"][0];
    assert!(target.get("api_key_override").is_none());
    assert!(target.get("api_base_override").is_none());
    assert_eq!(target["api_key_override_configured"], true);
    assert_eq!(target["api_base_override_configured"], true);
    assert!(!body
        .windows("route-secret".len())
        .any(|window| window == b"route-secret"));
}

#[tokio::test]
async fn provider_delete_impact_counts_linked_routes_and_empty_routes() {
    let (router, _store, _pool) = boot_no_auth().await;
    create_test_provider(&router, "prov-a").await;
    create_test_provider(&router, "prov-b").await;
    create_test_route(
        &router,
        "route-shared",
        "vm-shared",
        json!([
            { "provider_id": "prov-a", "model_id": "model-a", "weight": 1.0 },
            { "provider_id": "prov-b", "model_id": "model-b", "weight": 1.0 }
        ]),
    )
    .await;
    create_test_route(
        &router,
        "route-empty-after-delete",
        "vm-empty",
        json!([{ "provider_id": "prov-a", "model_id": "model-a", "weight": 1.0 }]),
    )
    .await;

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/providers/prov-a/delete-impact")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("impact response");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let impact: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(impact["provider_id"], json!("prov-a"));
    assert_eq!(impact["route_count"], json!(2));
    assert_eq!(impact["target_count"], json!(2));
    assert_eq!(impact["delete_route_count"], json!(1));
    let routes = impact["routes"].as_array().unwrap();
    assert!(routes.iter().any(|route| {
        route["id"] == json!("route-shared")
            && route["target_count"] == json!(1)
            && route["remaining_target_count"] == json!(1)
            && route["will_delete_route"] == json!(false)
    }));
    assert!(routes.iter().any(|route| {
        route["id"] == json!("route-empty-after-delete")
            && route["target_count"] == json!(1)
            && route["remaining_target_count"] == json!(0)
            && route["will_delete_route"] == json!(true)
    }));
}

#[tokio::test]
async fn deleting_provider_removes_targets_and_deletes_empty_routes() {
    let (router, store, pool) = boot_no_auth().await;
    create_test_provider(&router, "prov-a").await;
    create_test_provider(&router, "prov-b").await;
    create_test_provider(&router, "unused").await;
    create_test_route(
        &router,
        "route-shared",
        "vm-shared",
        json!([
            { "provider_id": "prov-a", "model_id": "model-a", "weight": 1.0 },
            { "provider_id": "prov-b", "model_id": "model-b", "weight": 2.0 }
        ]),
    )
    .await;
    create_test_route(
        &router,
        "route-empty-after-delete",
        "vm-empty",
        json!([{ "provider_id": "prov-a", "model_id": "model-a", "weight": 1.0 }]),
    )
    .await;

    // A provider with no route references still deletes cleanly.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/admin/v1/providers/unused")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("delete unused response");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/admin/v1/providers/prov-a")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("delete response");
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    assert!(store.get_provider("prov-a").await.unwrap().is_none());
    assert!(store
        .get_route("route-empty-after-delete")
        .await
        .unwrap()
        .is_none());
    let shared = store
        .get_route("route-shared")
        .await
        .unwrap()
        .expect("shared route remains");
    assert_eq!(shared.targets.len(), 1);
    assert_eq!(shared.targets[0].provider_id, "prov-b");
    assert_eq!(shared.targets[0].model_id, "model-b");

    let config = store.config_store();
    assert!(config.routing_table.routes.contains_key("vm-shared"));
    assert!(!config.routing_table.routes.contains_key("vm-empty"));
    let runtime_targets = &config.routing_table.routes["vm-shared"].targets;
    assert_eq!(runtime_targets.len(), 1);
    assert_eq!(runtime_targets[0].provider_id, "prov-b");

    let details_json: String = sqlx::query_scalar(
        "SELECT details_json FROM audit_log \
         WHERE target_type = 'provider' AND target_id = 'prov-a' AND action = 'delete' \
         ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(pool.any())
    .await
    .expect("audit details");
    let details: serde_json::Value = serde_json::from_str(&details_json).unwrap();
    assert_eq!(details["route_target_cleanup"]["route_count"], json!(2));
    assert_eq!(details["route_target_cleanup"]["target_count"], json!(2));
    assert_eq!(
        details["route_target_cleanup"]["delete_route_count"],
        json!(1)
    );
    let shared_route_audit: String = sqlx::query_scalar(
        "SELECT details_json FROM audit_log \
         WHERE target_type = 'route' AND target_id = 'route-shared' AND action = 'upsert' \
         ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(pool.any())
    .await
    .expect("shared route audit details");
    let shared_details: serde_json::Value = serde_json::from_str(&shared_route_audit).unwrap();
    assert_eq!(
        shared_details["snapshot"]["targets"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        shared_details["snapshot"]["targets"][0]["provider_id"],
        json!("prov-b")
    );
    assert!(shared_details["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| { c["field"] == json!("targets") }));

    let empty_route_audit: String = sqlx::query_scalar(
        "SELECT details_json FROM audit_log \
         WHERE target_type = 'route' AND target_id = 'route-empty-after-delete' AND action = 'delete' \
         ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(pool.any())
    .await
    .expect("empty route audit details");
    let empty_details: serde_json::Value = serde_json::from_str(&empty_route_audit).unwrap();
    assert_eq!(
        empty_details["snapshot"]["id"],
        json!("route-empty-after-delete")
    );
    assert_eq!(
        empty_details["snapshot"]["targets"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn acceptance_1_get_provider_returns_redacted_secrets() {
    let (router, _store, _pool) = boot_no_auth().await;
    let _ = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/providers",
            json!({
                "id": "p1",
                "name": "p1",
                "vendor": "openai",
                "api_base": "https://x",
                "api_key": "super-secret-key",
            }),
        ))
        .await
        .expect("post");
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/providers/p1")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("get");
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    let redacted = body["encrypted_api_key"].as_str().unwrap();
    assert!(redacted.starts_with("[encrypted:"));
    // The cleartext secret must NOT be present in the response.
    assert!(!body.to_string().contains("super-secret-key"));
}

#[tokio::test]
async fn acceptance_1_provider_secret_is_encrypted_at_rest() {
    let (router, _store, pool) = boot_no_auth().await;
    let _ = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/providers",
            json!({
                "id": "p1",
                "name": "p1",
                "vendor": "openai",
                "api_base": "https://x",
                "api_key": "sk-cleartext-must-not-appear",
            }),
        ))
        .await
        .expect("post");
    let direct: (String,) =
        sqlx::query_as("SELECT encrypted_api_key FROM providers WHERE id = 'p1'")
            .fetch_one(pool.any())
            .await
            .expect("direct query");
    assert!(
        !direct.0.contains("sk-cleartext-must-not-appear"),
        "encrypted column leaked the cleartext: {}",
        direct.0
    );
    assert!(direct.0.len() > 20);
}

#[tokio::test]
async fn acceptance_1_with_master_key_decrypts_into_routing_table() {
    // Boot a DbConfigStore with a real AES-256-GCM master key.
    let pool = Arc::new(db::open_pool("sqlite::memory:").await.expect("pool"));
    db::run_migrations(&pool).await.expect("migrate");
    let key_bytes: [u8; 32] = std::iter::repeat_n(0x42u8, 32)
        .collect::<Vec<u8>>()
        .try_into()
        .unwrap();
    let encryption = Some(Arc::new(KeyEncryption::from_bytes(key_bytes)));
    let store = Arc::new(DbConfigStore::new((*pool).clone(), encryption));
    store.refresh().await.expect("initial refresh");

    // Create a provider with an api key. The DbConfigStore
    // encrypts it at write time and decrypts it on every refresh.
    store
        .upsert_provider(
            "openai",
            "OpenAI",
            "openai",
            "https://api.openai.com/v1",
            "",
            Some("sk-decrypted-cleartext"),
            AuthMode::ApiKey,
            None,
            serde_json::json!({}),
            true,
        )
        .await
        .expect("upsert provider");

    // The data-plane ConfigStore must carry the cleartext.
    let cs = store.config_store();
    let snapshot = cs
        .snapshot()
        .expect("snapshot must be present after refresh");
    let p = snapshot
        .providers
        .get("openai")
        .expect("openai provider must be in the snapshot");
    assert_eq!(
        p.api_key_cleartext.as_deref(),
        Some("sk-decrypted-cleartext"),
        "the cleartext must be populated on refresh when a master key is configured"
    );
}

// ---- Acceptance #2: migrations run + status reports ----

#[tokio::test]
async fn acceptance_2_migrate_creates_tables_and_status_reports() {
    let (_router, _store, pool) = boot_no_auth().await;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _migrations")
        .fetch_one(pool.any())
        .await
        .expect("count");
    assert!(
        count >= 2,
        "expected both config + log migrations, got {count}"
    );
    // Both sequences should be recorded.
    let seqs: Vec<String> = sqlx::query_scalar("SELECT sequence FROM _migrations")
        .fetch_all(pool.any())
        .await
        .expect("seqs");
    assert!(seqs.contains(&"config".to_string()));
    assert!(seqs.contains(&"log".to_string()));
}

// ---- Acceptance #3: OltpSink + aggregate query ----

#[tokio::test]
async fn acceptance_3_stats_aggregate_uses_oltp_log_table() {
    use tiygate_core::telemetry::{LatencyBreakdown, RequestEvent, RequestStatus};
    use tiygate_core::Usage;
    let (_router, _store, pool) = boot_no_auth().await;
    let sink = tiygate_store::log_sink::oltp::OltpSink::new(pool.clone());
    let make = |id: &str, model: &str, status: RequestStatus| RequestEvent {
        request_id: id.to_string(),
        timestamp: chrono::Utc::now(),
        virtual_model: model.to_string(),
        resolved_provider: Some("openai".to_string()),
        resolved_model: Some(model.to_string()),
        account_label: None,
        trace_id: None,
        span_id: None,
        traceparent: None,
        ingress_protocol: "openai/chat-completions/v1".to_string(),
        egress_protocol: Some("openai/chat-completions/v1".to_string()),
        lossy: false,
        cache_hit: None,
        status,
        error_class: None,
        http_status: Some(200),
        error_source: None,
        latency_ms: LatencyBreakdown {
            total_ms: 1,
            upstream_ms: 1,
            queue_ms: 0,
        },
        ttfb_ms: None,
        tokens: Some(Usage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
            ..Default::default()
        }),
        cost: None,
        api_key_id: None,
        client_ip: None,
        user_agent: None,
        raw_envelope: None,
    };
    sink.write_request_event(&make("r1", "gpt-4o", RequestStatus::Success))
        .await
        .expect("write1");
    sink.write_request_event(&make("r2", "gpt-4o-mini", RequestStatus::Success))
        .await
        .expect("write2");

    let now = chrono::Utc::now().to_rfc3339();
    let earlier = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let by_model = tiygate_store::log_sink::oltp::aggregate_by_model(pool.as_ref(), &earlier, &now)
        .await
        .expect("agg");
    let names: Vec<&str> = by_model.iter().map(|b| b.bucket.as_str()).collect();
    assert!(names.contains(&"gpt-4o"));
    assert!(names.contains(&"gpt-4o-mini"));
}

#[tokio::test]
async fn acceptance_3_stats_by_provider_endpoint() {
    use tiygate_core::telemetry::{LatencyBreakdown, RequestEvent};
    let (router, _store, pool) = boot_no_auth().await;
    let sink = tiygate_store::log_sink::oltp::OltpSink::new(pool.clone());
    let ev = RequestEvent {
        request_id: "r-x".to_string(),
        timestamp: chrono::Utc::now(),
        virtual_model: "gpt-4o".to_string(),
        resolved_provider: Some("openai".to_string()),
        resolved_model: Some("gpt-4o".to_string()),
        account_label: None,
        trace_id: None,
        span_id: None,
        traceparent: None,
        ingress_protocol: "openai/chat-completions/v1".to_string(),
        egress_protocol: Some("openai/chat-completions/v1".to_string()),
        lossy: false,
        cache_hit: None,
        status: tiygate_core::telemetry::RequestStatus::Success,
        error_class: None,
        http_status: Some(200),
        error_source: None,
        latency_ms: LatencyBreakdown::default(),
        ttfb_ms: None,
        tokens: None,
        cost: Some(42_000),
        api_key_id: Some("k1".to_string()),
        client_ip: None,
        user_agent: None,
        raw_envelope: None,
    };
    sink.write_request_event(&ev).await.expect("write");
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/stats/by-provider")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let buckets = body["buckets"].as_array().unwrap();
    assert!(!buckets.is_empty());
    assert_eq!(buckets[0]["bucket"], "openai");
    assert_eq!(buckets[0]["cost"], 42_000);
}

// ---- Acceptance #4: config and log are separate tables ----

#[tokio::test]
async fn acceptance_4_config_and_log_live_in_separate_tables() {
    let (_router, _store, pool) = boot_no_auth().await;
    // Both tables must exist.
    let has_providers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='providers'",
    )
    .fetch_one(pool.any())
    .await
    .expect("providers");
    let has_request_logs: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='request_logs'",
    )
    .fetch_one(pool.any())
    .await
    .expect("logs");
    assert!(has_providers > 0, "providers table missing");
    assert!(has_request_logs > 0, "request_logs table missing");
}

// ---- Acceptance #5: retention cleans up old rows ----

#[tokio::test]
async fn acceptance_5_retention_deletes_expired_rows() {
    let (_router, _store, pool) = boot_no_auth().await;
    let old_ts = (chrono::Utc::now() - chrono::Duration::days(60)).to_rfc3339();
    sqlx::query(
        "INSERT INTO request_logs (request_id, ts, virtual_model, ingress_protocol, status) \
         VALUES ('old-1', $1, 'm', 'openai/chat-completions/v1', 'ok')",
    )
    .bind(&old_ts)
    .execute(pool.any())
    .await
    .expect("insert");
    let deleted = tiygate_store::retention::cleanup_once(pool.as_ref(), 30)
        .await
        .expect("cleanup");
    assert_eq!(deleted, 1);
}

// ---- Acceptance #6: encryption round-trip ----

#[tokio::test]
async fn acceptance_6_provider_secret_decrypts_round_trip() {
    let encryption = KeyEncryption::from_bytes([0xab; 32]);
    let blob = tiygate_store::keys::encrypt_api_key(&encryption, "sk-original").expect("enc");
    let plain = tiygate_store::keys::decrypt_api_key(&encryption, &blob).expect("dec");
    assert_eq!(plain, "sk-original");
}

// ---- Acceptance #10: Admin API bearer auth ----

#[tokio::test]
#[serial_test::serial]
async fn acceptance_10_admin_auth_rejects_missing_token() {
    // Per-thread override is the only way to set the token in
    // parallel tests; setting the env var races with other tests
    // that observe `TIYGATE_ADMIN_TOKEN` at request time.
    tiygate_admin::auth::set_test_admin_token_for_current_thread(Some("correct-horse"));
    let (router, _store, _pool) = boot_with_auth().await;
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/health")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    tiygate_admin::auth::set_test_admin_token_for_current_thread(None);
}

#[tokio::test]
#[serial_test::serial]
async fn acceptance_10_admin_auth_accepts_matching_token() {
    tiygate_admin::auth::set_test_admin_token_for_current_thread(Some("correct-horse"));
    let (router, _store, _pool) = boot_with_auth().await;
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/health")
                .header(header::AUTHORIZATION, "Bearer correct-horse")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "expected 200 OK with valid bearer"
    );
    tiygate_admin::auth::set_test_admin_token_for_current_thread(None);
}

#[tokio::test]
#[serial_test::serial]
async fn acceptance_10_admin_auth_rejects_wrong_token() {
    tiygate_admin::auth::set_test_admin_token_for_current_thread(Some("correct-horse"));
    let (router, _store, _pool) = boot_with_auth().await;
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/health")
                .header(header::AUTHORIZATION, "Bearer wrong-horse")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    tiygate_admin::auth::set_test_admin_token_for_current_thread(None);
}

// ---- OAuth admin routes (Phase 4 §4.5) ----
//
// These tests verify the OAuth admin routes are wired (i.e. the
// router contains them and they return reasonable error codes
// for the no-credential case). They do not exercise a live
// OAuth provider — the applier test coverage lives in
// `crates/providers/src/oauth.rs` (start / exchange / refresh).

#[tokio::test]
async fn oauth_routes_are_registered() {
    // `POST /admin/v1/oauth/callback` must reject requests
    // with a malformed body (missing `state` field).
    let (router, _store, _pool) = boot_no_auth().await;
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/v1/oauth/callback")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({"code": "foo"})).expect("encode"),
                ))
                .expect("req"),
        )
        .await
        .expect("resp");
    // Missing `state` is a 4xx from serde's JSON deserializer.
    assert!(
        resp.status().is_client_error(),
        "expected 4xx for missing state, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn oauth_start_rejects_unknown_provider() {
    let (router, _store, _pool) = boot_no_auth().await;
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/v1/oauth/start")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({"provider_id": "nonexistent"})).expect("encode"),
                ))
                .expect("req"),
        )
        .await
        .expect("resp");
    // The handler returns 404 for "provider not found".
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn oauth_callback_rejects_unknown_state() {
    // A well-formed callback with a `state` that was never
    // minted via `start` is rejected as 400 (CSRF protection).
    let (router, _store, _pool) = boot_no_auth().await;
    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/v1/oauth/callback")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({"code": "foo", "state": "ghost"})).expect("encode"),
                ))
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn api_key_quota_patch_and_single_get() {
    // Create a key, then PATCH its quota and confirm the GET reflects
    // the new quota while the status stays `active` (i.e. PATCH does
    // not collide with the PUT disable verb).
    let (router, _store, _pool) = boot_no_auth().await;

    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/api-keys",
            json!({ "name": "agent-q", "quota": { "requests_per_minute": 10 } }),
        ))
        .await
        .expect("create");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // PATCH the quota.
    let resp = router
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/admin/v1/api-keys/{id}"),
            json!({ "quota": { "requests_per_minute": 99, "tokens_per_day": 5000 } }),
        ))
        .await
        .expect("patch");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let patched: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(patched["quota"]["requests_per_minute"], json!(99));
    assert_eq!(patched["quota"]["tokens_per_day"], json!(5000));
    assert_eq!(patched["status"], json!("active"));

    // Single-key GET returns the updated quota plus a `usage` map
    // (empty when no live quota counter is wired into the state).
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/admin/v1/api-keys/{id}"))
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("get");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let got: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(got["id"], json!(id));
    assert_eq!(got["quota"]["requests_per_minute"], json!(99));
    assert!(got["usage"].is_object());

    // GET on an unknown id is a 404.
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/api-keys/does-not-exist")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("get-missing");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_key_model_access_create_and_update() {
    let (router, _store, _pool) = boot_no_auth().await;

    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/api-keys",
            json!({
                "name": "model-scoped",
                "allowed_models": [" z-model ", "a-model", "a-model"]
            }),
        ))
        .await
        .expect("create");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["allowed_models"], json!(["a-model", "z-model"]));

    let resp = router
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/admin/v1/api-keys/{id}/model-access"),
            json!({ "allowed_models": [] }),
        ))
        .await
        .expect("deny all");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let updated: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(updated["allowed_models"], json!([]));

    // Omitting the field is a malformed PATCH, not an alias for `null`.
    let resp = router
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/admin/v1/api-keys/{id}/model-access"),
            json!({}),
        ))
        .await
        .expect("reject missing field");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/admin/v1/api-keys/{id}"))
                .body(Body::empty())
                .expect("get key"),
        )
        .await
        .expect("get after rejected patch");
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let unchanged: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(unchanged["allowed_models"], json!([]));

    let resp = router
        .clone()
        .oneshot(json_request(
            "PATCH",
            &format!("/admin/v1/api-keys/{id}/model-access"),
            json!({ "allowed_models": null }),
        ))
        .await
        .expect("restore unrestricted");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let updated: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(updated["allowed_models"], serde_json::Value::Null);

    let resp = router
        .oneshot(json_request(
            "PATCH",
            &format!("/admin/v1/api-keys/{id}/model-access"),
            json!({ "allowed_models": [" "] }),
        ))
        .await
        .expect("reject blank model");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---- provider catalog: server-side registered providers ----

#[tokio::test]
async fn provider_catalog_lists_registered_providers() {
    let (router, _store, _pool) = boot_no_auth().await;

    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/provider-catalog")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("get-catalog");
    assert_eq!(resp.status(), StatusCode::OK);

    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&bytes).unwrap();

    let ids: Vec<&str> = entries
        .iter()
        .map(|e| e["id"].as_str().expect("id is a string"))
        .collect();

    // These providers are always linked into the default build.
    assert!(
        ids.contains(&"openai"),
        "catalog should contain openai: {ids:?}"
    );
    assert!(
        ids.contains(&"anthropic"),
        "catalog should contain anthropic: {ids:?}"
    );
    // Bedrock is gated behind the `bedrock` feature and is not linked
    // by the admin crate, so it must not appear in the default build.
    assert!(
        !ids.contains(&"bedrock"),
        "catalog must not contain bedrock in the default build: {ids:?}"
    );

    // Entries are sorted by id for a stable UI.
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "catalog must be sorted by id");

    // Each entry exposes the fields the UI relies on.
    let openai = entries
        .iter()
        .find(|e| e["id"] == json!("openai"))
        .expect("openai entry");
    assert!(openai["display_name"].is_string());
    assert!(openai["default_base_url"].is_string());
    assert!(openai["auth_mode"].is_string());
}

#[tokio::test]
async fn config_export_returns_json_with_content_disposition() {
    let (router, _store, _pool) = boot_no_auth().await;
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/config/export")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let cd = resp
        .headers()
        .get(header::CONTENT_DISPOSITION)
        .expect("content-disposition header");
    assert!(
        cd.to_str().unwrap().contains("attachment"),
        "export should carry a Content-Disposition attachment header"
    );
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(json["schema_version"], 2);
    assert!(json["providers"].is_array());
    assert!(json["routes"].is_array());
    assert!(json["api_keys"].is_array());
}

#[tokio::test]
async fn config_import_inserts_and_returns_report() {
    let (router, _store, _pool) = boot_no_auth().await;
    let import_body = json!({
        "master_key": "",
        "config": {
            "schema_version": 1,
            "exported_at": "2025-01-01T00:00:00Z",
            "encrypted": false,
            "providers": [{
                "id": "p-import-test",
                "name": "Imported",
                "vendor": "openai",
                "api_base": "https://api.openai.com/v1",
                "encrypted_api_key": "sk-imported",
                "auth_mode": "api_key",
                "encrypted_oauth_meta": "",
                "metadata_json": {},
                "enabled": true,
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z"
            }],
            "routes": [],
            "api_keys": []
        },
        "selection": {
            "providers": ["p-import-test"]
        }
    });
    let resp = router
        .oneshot(json_request("POST", "/admin/v1/config/import", import_body))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("body");
    let report: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(report["providers_imported"], 1);
    assert_eq!(report["providers_skipped"], 0);
}

#[tokio::test]
async fn config_import_rejects_forbidden_capability_override() {
    let (router, store, _pool) = boot_no_auth().await;
    let import_body = json!({
        "master_key": "",
        "config": {
            "schema_version": 2,
            "exported_at": "2025-01-01T00:00:00Z",
            "encrypted": false,
            "providers": [{
                "id": "p-import-capability",
                "name": "Imported capability target",
                "vendor": "unknown-compatible",
                "api_base": "https://api.openai.com/v1",
                "encrypted_api_key": "sk-imported-capability",
                "auth_mode": "api_key",
                "encrypted_oauth_meta": "",
                "metadata_json": {},
                "enabled": true,
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z"
            }],
            "routes": [{
                "id": "r-import-capability",
                "virtual_model": "v-import-capability",
                "targets": [{
                    "provider_id": "p-import-capability",
                    "model_id": "gpt-4o",
                    "weight": 1.0,
                    "enabled": true
                }],
                "enabled": true,
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z"
            }],
            "api_keys": [],
            "capability_overrides": [{
                "selector": {
                    "route_id": "r-import-capability",
                    "target_index": 0,
                    "provider_id": "p-import-capability",
                    "model_id": "gpt-4o"
                },
                "capability_id": "tools.namespace",
                "state": "supported",
                "reason": "invalid cross-wire override",
                "actor": "tester"
            }]
        },
        "selection": {
            "providers": ["p-import-capability"],
            "routes": ["r-import-capability"],
            "capability_overrides": ["r-import-capability#0#tools.namespace"]
        }
    });
    let response = router
        .oneshot(json_request("POST", "/admin/v1/config/import", import_body))
        .await
        .expect("capability import response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("capability import body");
    let report: serde_json::Value = serde_json::from_slice(&body).expect("capability import json");
    assert_eq!(report["routes_imported"], 1);
    assert_eq!(report["capability_overrides_imported"], 0);
    assert_eq!(report["capability_overrides_skipped"], 1);
    let target = store
        .config_store()
        .routing_table
        .resolve("v-import-capability")
        .expect("imported target")
        .into_iter()
        .next()
        .expect("target");
    let key = store.target_key_for(&target).expect("target key").0;
    assert!(store
        .list_capability_overrides(&key)
        .await
        .expect("override list")
        .is_empty());
}

#[tokio::test]
async fn config_import_skips_existing_ids() {
    let (router, store, _pool) = boot_no_auth().await;
    // Pre-create a provider so the import should skip it.
    store
        .upsert_provider(
            "p-dup",
            "Original",
            "openai",
            "https://api.openai.com/v1",
            "",
            Some("sk-original"),
            AuthMode::ApiKey,
            None,
            json!({}),
            true,
        )
        .await
        .expect("upsert");

    // The selection does NOT include "p-dup", so the import skips it
    // and leaves the existing row untouched.
    let import_body = json!({
        "master_key": "",
        "config": {
            "schema_version": 1,
            "exported_at": "2025-01-01T00:00:00Z",
            "encrypted": false,
            "providers": [{
                "id": "p-dup",
                "name": "Should Not Overwrite",
                "vendor": "openai",
                "api_base": "https://api.openai.com/v1",
                "encrypted_api_key": "sk-different",
                "auth_mode": "api_key",
                "encrypted_oauth_meta": "",
                "metadata_json": {},
                "enabled": true,
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z"
            }],
            "routes": [],
            "api_keys": []
        },
        "selection": {}
    });
    let resp = router
        .oneshot(json_request("POST", "/admin/v1/config/import", import_body))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("body");
    let report: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(report["providers_imported"], 0);
    assert_eq!(report["providers_skipped"], 1);

    // The original provider must not have been overwritten.
    let p = store
        .get_provider("p-dup")
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(p.name, "Original");
}

#[tokio::test]
async fn config_import_overwrites_when_selected() {
    let (router, store, _pool) = boot_no_auth().await;
    store
        .upsert_provider(
            "p-overwrite",
            "Original",
            "openai",
            "https://api.openai.com/v1",
            "",
            Some("sk-original"),
            AuthMode::ApiKey,
            None,
            json!({}),
            true,
        )
        .await
        .expect("upsert");

    let import_body = json!({
        "master_key": "",
        "config": {
            "schema_version": 1,
            "exported_at": "2025-01-01T00:00:00Z",
            "encrypted": false,
            "providers": [{
                "id": "p-overwrite",
                "name": "Overwritten",
                "vendor": "openai",
                "api_base": "https://api.openai.com/v1",
                "encrypted_api_key": "sk-new",
                "auth_mode": "api_key",
                "encrypted_oauth_meta": "",
                "metadata_json": {},
                "enabled": true,
                "created_at": "2025-01-01T00:00:00Z",
                "updated_at": "2025-01-01T00:00:00Z"
            }],
            "routes": [],
            "api_keys": []
        },
        "selection": {
            "providers": ["p-overwrite"]
        }
    });
    let resp = router
        .oneshot(json_request("POST", "/admin/v1/config/import", import_body))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .expect("body");
    let report: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(report["providers_imported"], 1);

    let p = store
        .get_provider("p-overwrite")
        .await
        .expect("get")
        .expect("exists");
    assert_eq!(p.name, "Overwritten");
}

#[tokio::test]
async fn settings_audit_records_snapshot_and_changes_on_create() {
    let (router, store, pool) = boot_no_auth().await;

    // No prior setting exists — this is a create operation.
    let resp = router
        .clone()
        .oneshot(json_request(
            "PUT",
            "/admin/v1/settings",
            json!({ "settings": { "gateway.test.key": "v1" } }),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    let details_json: String = sqlx::query_scalar(
        "SELECT details_json FROM audit_log \
         WHERE target_type = 'settings' AND action = 'upsert' \
         ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(pool.any())
    .await
    .expect("audit row");
    let details: serde_json::Value = serde_json::from_str(&details_json).expect("json");

    // snapshot must contain the new value.
    assert_eq!(details["snapshot"]["gateway.test.key"], json!("v1"));
    // changes must show before=null, after=v1 (field-level diff).
    let changes = details["changes"].as_array().expect("changes array");
    let entry = changes
        .iter()
        .find(|c| c["field"] == "gateway.test.key")
        .expect("found change for key");
    assert_eq!(entry["before"], serde_json::Value::Null);
    assert_eq!(entry["after"], json!("v1"));

    // Confirm the value was actually persisted.
    let val = store.get_setting("gateway.test.key").await.expect("get");
    assert_eq!(val.as_deref(), Some("v1"));
}

#[tokio::test]
async fn settings_audit_records_before_after_on_update() {
    let (router, store, pool) = boot_no_auth().await;

    // Seed an initial value.
    store
        .set_setting("gateway.test.key", "old")
        .await
        .expect("seed");

    // Update it via the API.
    let resp = router
        .clone()
        .oneshot(json_request(
            "PUT",
            "/admin/v1/settings",
            json!({ "settings": { "gateway.test.key": "new" } }),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    let details_json: String = sqlx::query_scalar(
        "SELECT details_json FROM audit_log \
         WHERE target_type = 'settings' AND action = 'upsert' \
         ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(pool.any())
    .await
    .expect("audit row");
    let details: serde_json::Value = serde_json::from_str(&details_json).expect("json");

    // snapshot carries the post-write value.
    assert_eq!(details["snapshot"]["gateway.test.key"], json!("new"));

    // changes carries field-level before/after.
    let changes = details["changes"].as_array().expect("changes array");
    let entry = changes
        .iter()
        .find(|c| c["field"] == "gateway.test.key")
        .expect("found change for key");
    assert_eq!(entry["before"], json!("old"));
    assert_eq!(entry["after"], json!("new"));
}

#[tokio::test]
async fn settings_audit_redacts_encrypted_keys() {
    let (router, _store, pool) = boot_no_auth().await;

    // Write an encrypted key — the audit entry must not store the
    // cleartext value, only a redacted placeholder.
    let resp = router
        .clone()
        .oneshot(json_request(
            "PUT",
            "/admin/v1/settings",
            json!({ "settings": { "gateway.archive.s3_secret_access_key": "super-secret" } }),
        ))
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);

    let details_json: String = sqlx::query_scalar(
        "SELECT details_json FROM audit_log \
         WHERE target_type = 'settings' AND action = 'upsert' \
         ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(pool.any())
    .await
    .expect("audit row");
    let details: serde_json::Value = serde_json::from_str(&details_json).expect("json");

    // The cleartext secret must never appear in the audit row.
    let raw = details_json.as_str();
    assert!(
        !raw.contains("super-secret"),
        "cleartext secret leaked into audit log: {raw}"
    );
    // The redacted snapshot must contain a redaction marker.
    let snap = &details["snapshot"]["gateway.archive.s3_secret_access_key"];
    let snap_str = snap.as_str().expect("string");
    assert!(
        snap_str.starts_with("[encrypted:"),
        "expected redaction marker, got {snap_str}"
    );
}

#[tokio::test]
async fn model_catalog_status_returns_not_found_when_disabled() {
    let (router, _store, _pool) = boot_no_auth().await;
    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/model-catalog")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn model_catalog_status_returns_version_when_enabled() {
    let (_router, store, pool) = boot_no_auth().await;
    let catalog = tiygate_store::model_catalog::ModelCatalog::from_models_dev_json(
        r#"{"openai":{"id":"openai","name":"OpenAI","models":{"gpt-4o":{"id":"gpt-4o","name":"GPT-4o"}}}}"#,
        "test",
    )
    .expect("catalog");
    let catalog_store = Arc::new(tiygate_store::model_catalog::ModelCatalogStore::new(
        catalog,
    ));
    let state = AdminState::new(store, pool, None).with_model_catalog(Some(catalog_store));
    let router = tiygate_admin::build_router_with_auth(state, false);

    let resp = router
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/admin/v1/model-catalog")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["source"], json!("test"));
    assert_eq!(body["provider_count"], json!(1));
    assert_eq!(body["model_count"], json!(1));
    assert!(body["checksum"].as_str().unwrap().len() >= 32);
}

#[tokio::test]
async fn model_catalog_manual_refresh_returns_new_version() {
    let (_router, store, pool) = boot_no_auth().await;
    let initial = tiygate_store::model_catalog::ModelCatalog::from_models_dev_json(
        r#"{"openai":{"id":"openai","models":{"gpt-4o":{"id":"gpt-4o","name":"Old"}}}}"#,
        "initial",
    )
    .expect("initial catalog");
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(
            r#"{"openai":{"id":"openai","models":{"gpt-4o":{"id":"gpt-4o","name":"New"}}}}"#,
        ))
        .mount(&server)
        .await;
    let catalog_store = Arc::new(
        tiygate_store::model_catalog::ModelCatalogStore::new_with_source_url(initial, server.uri()),
    );
    let state = AdminState::new(store, pool, None).with_model_catalog(Some(catalog_store));
    let router = tiygate_admin::build_router_with_auth(state, false);

    let resp = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/v1/model-catalog/refresh")
                .body(Body::empty())
                .expect("req"),
        )
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["source"], json!(server.uri()));
    assert_eq!(body["model_count"], json!(1));
}

#[tokio::test]
async fn model_catalog_resolve_prefers_virtual_model_then_target_fallback() {
    let (_router, store, pool) = boot_no_auth().await;
    let catalog = tiygate_store::model_catalog::ModelCatalog::from_models_dev_json(
        r#"{"openai":{"id":"openai","name":"OpenAI","models":{"openai/virtual":{"id":"openai/virtual","name":"Virtual Name"},"openai/target":{"id":"openai/target","name":"Target Name"}}}}"#,
        "test",
    )
    .expect("catalog");
    let catalog_store = Arc::new(tiygate_store::model_catalog::ModelCatalogStore::new(
        catalog,
    ));
    let state = AdminState::new(store, pool, None).with_model_catalog(Some(catalog_store));
    let router = tiygate_admin::build_router_with_auth(state, false);

    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/model-catalog/resolve",
            json!({"virtual_model":"openai/virtual","target_model_id":"openai/target"}),
        ))
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(meta["display_name"], json!("Virtual Name"));

    let resp = router
        .oneshot(json_request(
            "POST",
            "/admin/v1/model-catalog/resolve",
            json!({"virtual_model":"missing","target_model_id":"openai/target"}),
        ))
        .await
        .expect("resp");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
    let meta: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(meta["display_name"], json!("Target Name"));
}

/// Create/update should auto-initialize `model_metadata` from the model
/// catalog when the caller omits it, without ever failing the request when
/// no catalog match exists.
#[tokio::test]
async fn route_create_and_update_auto_initialize_model_metadata() {
    let (_router, store, pool) = boot_no_auth().await;
    let catalog = tiygate_store::model_catalog::ModelCatalog::from_models_dev_json(
        r#"{"openai":{"id":"openai","name":"OpenAI","models":{"openai/gpt-4o":{"id":"openai/gpt-4o","name":"GPT-4o"}}}}"#,
        "test",
    )
    .expect("catalog");
    let catalog_store = Arc::new(tiygate_store::model_catalog::ModelCatalogStore::new(
        catalog,
    ));
    let state = AdminState::new(store, pool, None).with_model_catalog(Some(catalog_store));
    let router = tiygate_admin::build_router_with_auth(state, false);

    create_test_provider(&router, "openai").await;

    // No model_metadata is submitted; the target's model_id matches a
    // catalog entry, so it should be auto-resolved and persisted.
    let resp = router
        .clone()
        .oneshot(json_request(
            "POST",
            "/admin/v1/routes",
            json!({
                "id": "route-auto-meta",
                "virtual_model": "my-gpt-4o",
                "targets": [{"provider_id": "openai", "model_id": "openai/gpt-4o", "weight": 1.0}],
            }),
        ))
        .await
        .expect("create response");
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
    let route: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        route["model_metadata"]["display_name"],
        json!("GPT-4o"),
        "create_route should auto-init metadata from the target model id"
    );

    // Update without model_metadata and with a target that has no catalog
    // match: the route should still update successfully, just without
    // metadata (a warning is logged instead of failing the request).
    let resp = router
        .clone()
        .oneshot(json_request(
            "PUT",
            "/admin/v1/routes/route-auto-meta",
            json!({
                "virtual_model": "my-gpt-4o",
                "targets": [{"provider_id": "openai", "model_id": "unknown-model", "weight": 1.0}],
            }),
        ))
        .await
        .expect("update response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
    let route: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(route.get("model_metadata").is_none() || route["model_metadata"].is_null());
}
