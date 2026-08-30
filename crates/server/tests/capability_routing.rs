//! Capability-aware routing integration tests.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use tiygate_core::{
    capability_shape_hash_from_ids, capability_shape_hash_from_requirements, resolve_capabilities,
    CapabilityId, CapabilityObservation, CapabilityRequirement, CapabilityState, CapabilityValue,
    EvidenceSource, HealthRegistry, RequirementStrength,
};
use tiygate_server::config::ServerConfig;
use tiygate_server::ingress;
use tiygate_store::capabilities::{
    wire_profile_for_target, CapabilityRouteAdmission, ProfileStatus,
};
use tiygate_store::config::ConfigStore;
use tiygate_store::config_store::{DbConfigStore, RouteUpsert};
use tiygate_store::db;
use tiygate_store::models::AuthMode;

fn request(body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("request")
}

fn supported(id: &str) -> CapabilityObservation {
    CapabilityObservation {
        capability_id: CapabilityId::from(id),
        state: CapabilityState::Supported,
        value: None,
        source: EvidenceSource::SemanticProbe,
        observed_at: chrono::Utc::now(),
        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(24)),
        evidence_version: 1,
        probe_suite_version: Some(1),
        reason_code: None,
        redacted_detail: None,
    }
}

fn supported_namespace(path: &str) -> CapabilityObservation {
    let mut observation = supported("tools.namespace");
    observation.state = CapabilityState::Constrained;
    observation.value = Some(tiygate_core::CapabilityValue::EnumSet(
        [path.to_string()].into_iter().collect(),
    ));
    observation
}

fn crl_requirements() -> Vec<CapabilityRequirement> {
    vec![
        CapabilityRequirement::required("transport.http"),
        CapabilityRequirement::required("tools.crl.additional_tools"),
        CapabilityRequirement::with_value(
            "tools.namespace",
            RequirementStrength::Required,
            CapabilityValue::EnumSet(["functions".to_string()].into_iter().collect()),
        ),
        CapabilityRequirement::required("tools.function"),
    ]
}

fn unsupported(id: &str) -> CapabilityObservation {
    let mut observation = supported(id);
    observation.state = CapabilityState::Unsupported;
    observation.reason_code = Some("controlled_probe".to_string());
    observation
}

#[tokio::test]
async fn crl_request_is_promoted_for_standard_responses_target() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_string_contains("\"tools\""))
        .and(body_string_contains("\"namespace\""))
        .and(body_string_contains("\"additional_tools\""))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": {"type": "unexpected_test_body", "message": "carrier remained"}
        })))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_string_contains("\"tools\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "resp_test",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "id": "fc_test",
                "call_id": "call_test",
                "name": "wait",
                "arguments": "{}"
            }]
        })))
        .mount(&upstream)
        .await;

    let pool = Arc::new(db::open_pool("sqlite::memory:").await.expect("pool"));
    db::run_migrations(&pool).await.expect("migrations");
    let store = Arc::new(DbConfigStore::new((*pool).clone(), None));
    store.refresh().await.expect("refresh");
    store
        .upsert_provider(
            "provider",
            "provider",
            "openai",
            &upstream.uri(),
            "",
            Some("sk-test"),
            AuthMode::ApiKey,
            None,
            json!({}),
            true,
        )
        .await
        .expect("provider");
    let route = store
        .upsert_route_with_mode(RouteUpsert {
            id: "route",
            virtual_model: "virtual-model",
            targets: &[tiygate_store::models::RouteTarget {
                provider_id: "provider".to_string(),
                model_id: "gpt-4o".to_string(),
                weight: 1.0,
                enabled: true,
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                egress_dialect_id: Some("openai-responses-standard".to_string()),
            }],
            routing_strategy: None,
            capability_routing_mode: Some(tiygate_core::CapabilityRoutingMode::Enforce),
            model_metadata: None,
            enabled: true,
        })
        .await
        .expect("route");
    let runtime_target = store
        .config_store()
        .routing_table
        .resolve("virtual-model")
        .expect("runtime target")
        .remove(0);
    let (target_key, _) = store.target_key_for(&runtime_target).expect("target key");
    let profile = store
        .get_capability_profile(&target_key)
        .await
        .expect("profile")
        .expect("profile row");
    let observations = vec![
        supported("transport.http"),
        supported("tools.function"),
        supported("tools.function.continuation"),
        supported_namespace("functions"),
        supported("tools.custom"),
        unsupported("tools.crl.additional_tools"),
    ];
    let baseline =
        tiygate_protocols::capabilities::baseline_for(&wire_profile_for_target(&runtime_target));
    let mut profile = profile;
    profile.observations = observations.clone();
    profile.resolved_capabilities =
        resolve_capabilities(&baseline, observations, chrono::Utc::now());
    profile.profile_status = ProfileStatus::Ready;
    profile.fresh_until = Some(chrono::Utc::now() + chrono::Duration::hours(24));
    profile.stale_until = Some(chrono::Utc::now() + chrono::Duration::days(7));
    profile.last_probe_suite_version = Some(tiygate_store::capabilities::PROBE_SUITE_VERSION);
    profile.last_probe_judge_version = Some(tiygate_store::capabilities::PROBE_JUDGE_VERSION);
    store
        .upsert_capability_profile(&profile)
        .await
        .expect("profile update");
    let required_requirements = crl_requirements();
    let shape_hash = capability_shape_hash_from_requirements(&required_requirements);
    let now = chrono::Utc::now();
    store
        .upsert_capability_route_admission(
            &CapabilityRouteAdmission {
                route_id: route.id.clone(),
                capability_shape_hash: shape_hash.clone(),
                required_capabilities: vec![
                    CapabilityId::from("transport.http"),
                    CapabilityId::from("tools.crl.additional_tools"),
                    CapabilityId::from("tools.namespace"),
                    CapabilityId::from("tools.function"),
                ],
                required_requirements,
                mode: tiygate_core::CapabilityRoutingMode::Enforce,
                gate_policy_version: 1,
                report: json!({
                    "gate_passed": true,
                    "registry_version": tiygate_store::capabilities::CAPABILITY_REGISTRY_VERSION,
                    "baseline_version": tiygate_store::capabilities::CAPABILITY_BASELINE_VERSION,
                    "shape_hash_version": "shape/v1"
                }),
                approved_by: Some("test".to_string()),
                approved_at: Some(now),
                expires_at: Some(now + chrono::Duration::hours(1)),
                revision: 0,
                created_at: now,
                updated_at: now,
            },
            None,
        )
        .await
        .expect("shape admission");
    let admissions = store
        .list_all_capability_route_admissions()
        .await
        .expect("list admissions");
    assert_eq!(admissions.len(), 1);
    assert_eq!(admissions[0].route_id, route.id);
    assert_eq!(admissions[0].capability_shape_hash, shape_hash);
    store
        .set_setting(
            tiygate_store::settings_keys::CAPABILITY_PROBE_ENABLED,
            "false",
        )
        .await
        .expect("pause probes");
    store
        .set_setting(
            tiygate_store::settings_keys::RESPONSES_CRL_TOOL_PROMOTION_ENABLED,
            "true",
        )
        .await
        .expect("enable CRL promotion for test");
    store
        .set_setting(
            tiygate_store::settings_keys::INGRESS_REQUIRE_API_KEY,
            "false",
        )
        .await
        .expect("disable ingress auth");
    let _ = route;

    let config = ConfigStore::from_snapshot(store.config_store().snapshot().expect("snapshot"));
    let server_config = ServerConfig {
        require_api_key: false,
        ..ServerConfig::default()
    };
    let telemetry = Arc::new(tiygate_server::telemetry::ChannelTelemetryBus::spawn(
        Arc::new(tiygate_store::log_sink::stdout::StdoutSink::new()),
        32,
    ));
    let router = ingress::router_with_telemetry_full(
        config,
        Arc::new(HealthRegistry::with_defaults()),
        &server_config,
        telemetry,
        None,
        None,
        Some(store),
        None,
    );
    // The data-plane router publishes the DB-backed capability snapshot from
    // the independent epoch watcher; wait for that first atomic load.
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    let response = router
        .oneshot(request(json!({
            "model": "virtual-model",
            "input": [
                {"role": "user", "content": "run"},
                {"type": "additional_tools", "role": "developer", "tools": [{
                    "type": "namespace", "name": "functions", "tools": [
                        {"type": "function", "name": "wait", "parameters": {"type": "object"}}
                    ]
                }]}
            ],
            "tool_choice": "auto"
        })))
        .await
        .expect("response");
    let response_status = response.status();
    let response_body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    assert_eq!(
        response_status,
        StatusCode::OK,
        "gateway response: {}",
        String::from_utf8_lossy(&response_body)
    );
}

#[tokio::test]
async fn enforce_shape_filters_incompatible_target_before_upstream() {
    let upstream = MockServer::start().await;
    // No mock is mounted intentionally: any upstream request would fail the
    // test's no-compatible-target guarantee.
    let pool = Arc::new(db::open_pool("sqlite::memory:").await.expect("pool"));
    db::run_migrations(&pool).await.expect("migrations");
    let store = Arc::new(DbConfigStore::new((*pool).clone(), None));
    store.refresh().await.expect("refresh");
    store
        .upsert_provider(
            "provider-filter",
            "provider-filter",
            "openai",
            &upstream.uri(),
            "",
            Some("sk-filter"),
            AuthMode::ApiKey,
            None,
            json!({}),
            true,
        )
        .await
        .expect("provider");
    let route = store
        .upsert_route_with_mode(RouteUpsert {
            id: "route-filter",
            virtual_model: "filter-model",
            targets: &[tiygate_store::models::RouteTarget {
                provider_id: "provider-filter".to_string(),
                model_id: "gpt-filter".to_string(),
                weight: 1.0,
                enabled: true,
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                egress_dialect_id: Some("openai-responses-standard".to_string()),
            }],
            routing_strategy: None,
            capability_routing_mode: Some(tiygate_core::CapabilityRoutingMode::Enforce),
            model_metadata: None,
            enabled: true,
        })
        .await
        .expect("route");
    let target = store
        .config_store()
        .routing_table
        .resolve("filter-model")
        .expect("target")
        .remove(0);
    let (target_key, _) = store.target_key_for(&target).expect("target key");
    let mut profile = store
        .get_capability_profile(&target_key)
        .await
        .expect("profile")
        .expect("profile row");
    let observations = vec![supported("transport.http"), unsupported("tools.function")];
    let baseline = tiygate_protocols::capabilities::baseline_for(&wire_profile_for_target(&target));
    profile.observations = observations.clone();
    profile.resolved_capabilities =
        resolve_capabilities(&baseline, observations, chrono::Utc::now());
    profile.profile_status = ProfileStatus::Ready;
    let now = chrono::Utc::now();
    profile.fresh_until = Some(now + chrono::Duration::hours(1));
    profile.stale_until = Some(now + chrono::Duration::days(1));
    profile.last_probe_suite_version = Some(tiygate_store::capabilities::PROBE_SUITE_VERSION);
    profile.last_probe_judge_version = Some(tiygate_store::capabilities::PROBE_JUDGE_VERSION);
    store
        .upsert_capability_profile(&profile)
        .await
        .expect("profile update");
    let shape_hash = capability_shape_hash_from_ids(&[
        CapabilityId::from("transport.http"),
        CapabilityId::from("tools.function"),
    ]);
    store
        .upsert_capability_route_admission(
            &CapabilityRouteAdmission {
                route_id: route.id,
                capability_shape_hash: shape_hash,
                required_capabilities: vec![
                    CapabilityId::from("transport.http"),
                    CapabilityId::from("tools.function"),
                ],
                required_requirements: Vec::new(),
                mode: tiygate_core::CapabilityRoutingMode::Enforce,
                gate_policy_version: 1,
                report: json!({
                    "gate_passed": true,
                    "registry_version": tiygate_store::capabilities::CAPABILITY_REGISTRY_VERSION,
                    "baseline_version": tiygate_store::capabilities::CAPABILITY_BASELINE_VERSION,
                    "shape_hash_version": "shape/v1"
                }),
                approved_by: Some("test".to_string()),
                approved_at: Some(now),
                expires_at: Some(now + chrono::Duration::hours(1)),
                revision: 0,
                created_at: now,
                updated_at: now,
            },
            None,
        )
        .await
        .expect("admission");
    store
        .set_setting(
            tiygate_store::settings_keys::CAPABILITY_PROBE_ENABLED,
            "false",
        )
        .await
        .expect("pause probes");
    store
        .set_setting(
            tiygate_store::settings_keys::INGRESS_REQUIRE_API_KEY,
            "false",
        )
        .await
        .expect("disable ingress auth");
    let config = ConfigStore::from_snapshot(store.config_store().snapshot().expect("snapshot"));
    let server_config = ServerConfig {
        require_api_key: false,
        ..ServerConfig::default()
    };
    let telemetry = Arc::new(tiygate_server::telemetry::ChannelTelemetryBus::spawn(
        Arc::new(tiygate_store::log_sink::stdout::StdoutSink::new()),
        32,
    ));
    let router = ingress::router_with_telemetry_full(
        config,
        Arc::new(HealthRegistry::with_defaults()),
        &server_config,
        telemetry,
        None,
        None,
        Some(store),
        None,
    );
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    let response = router
        .oneshot(request(json!({
            "model": "filter-model",
            "input": "hello",
            "tools": [{"type":"function","name":"wait","parameters":{"type":"object"}}]
        })))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(json["error"]["code"], "no_compatible_target");
    assert!(json["error"]["details"]["shape_hash"].is_null());
    assert!(json.to_string().find("api_base").is_none());
}

#[tokio::test]
async fn mixed_native_and_promotion_targets_use_independent_bodies() {
    let native = MockServer::start().await;
    let promoted = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_string_contains("\"stream\":true"))
        .and(body_string_contains("additional_tools"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"error\":{\"type\":\"invalid_request_error\",\"param\":\"additional_tools\",\"message\":\"additional_tools are not supported\"}}\n\n",
                ),
        )
        .with_priority(1)
        .mount(&native)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_string_contains("additional_tools"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"type": "invalid_request_error", "param": "tools", "message": "tools are not supported"}
        })))
        .with_priority(10)
        .mount(&native)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .and(body_string_contains("\"stream\":true"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-stream\",\"status\":\"in_progress\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-stream\",\"status\":\"completed\",\"output\":[]}}\n\n",
                ),
        )
        .with_priority(1)
        .mount(&promoted)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "promoted-response",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "id": "fc-promoted",
                "call_id": "call-promoted",
                "name": "wait",
                "arguments": "{}"
            }]
        })))
        .with_priority(10)
        .mount(&promoted)
        .await;

    let pool = Arc::new(db::open_pool("sqlite::memory:").await.expect("pool"));
    db::run_migrations(&pool).await.expect("migrations");
    let store = Arc::new(DbConfigStore::new((*pool).clone(), None));
    store.refresh().await.expect("refresh");
    for (id, base, key) in [
        ("native-provider", native.uri(), "sk-native"),
        ("promoted-provider", promoted.uri(), "sk-promoted"),
    ] {
        store
            .upsert_provider(
                id,
                id,
                "unknown-compatible",
                &base,
                "",
                Some(key),
                AuthMode::ApiKey,
                None,
                json!({}),
                true,
            )
            .await
            .expect("provider");
    }
    let route = store
        .upsert_route_with_mode(RouteUpsert {
            id: "mixed-route",
            virtual_model: "mixed-model",
            targets: &[
                tiygate_store::models::RouteTarget {
                    provider_id: "native-provider".to_string(),
                    model_id: "gpt-native".to_string(),
                    weight: 2.0,
                    enabled: true,
                    account_label: None,
                    api_key_override: None,
                    api_base_override: None,
                    egress_dialect_id: Some("openai-responses-codex-lite".to_string()),
                },
                tiygate_store::models::RouteTarget {
                    provider_id: "promoted-provider".to_string(),
                    model_id: "gpt-promoted".to_string(),
                    weight: 1.0,
                    enabled: true,
                    account_label: None,
                    api_key_override: None,
                    api_base_override: None,
                    egress_dialect_id: Some("openai-responses-standard".to_string()),
                },
            ],
            routing_strategy: Some(tiygate_core::RoutingStrategyName::Priority),
            capability_routing_mode: Some(tiygate_core::CapabilityRoutingMode::Enforce),
            model_metadata: None,
            enabled: true,
        })
        .await
        .expect("route");
    let targets = store
        .config_store()
        .routing_table
        .resolve("mixed-model")
        .expect("targets");
    assert_eq!(targets.len(), 2);
    let mut profiles = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        let (key, _) = store.target_key_for(target).expect("target key");
        let mut profile = store
            .get_capability_profile(&key)
            .await
            .expect("profile")
            .expect("profile row");
        let mut observations = vec![
            supported("transport.http"),
            supported("transport.sse"),
            supported("tools.function"),
            supported("tools.function.continuation"),
            supported_namespace("functions"),
        ];
        if index == 0 {
            observations.push(supported("tools.crl.additional_tools"));
        } else {
            observations.push(unsupported("tools.crl.additional_tools"));
        }
        let baseline =
            tiygate_protocols::capabilities::baseline_for(&wire_profile_for_target(target));
        profile.observations = observations.clone();
        profile.resolved_capabilities =
            resolve_capabilities(&baseline, observations, chrono::Utc::now());
        profile.profile_status = ProfileStatus::Ready;
        let now = chrono::Utc::now();
        profile.fresh_until = Some(now + chrono::Duration::hours(1));
        profile.stale_until = Some(now + chrono::Duration::days(1));
        profile.last_probe_suite_version = Some(tiygate_store::capabilities::PROBE_SUITE_VERSION);
        profile.last_probe_judge_version = Some(tiygate_store::capabilities::PROBE_JUDGE_VERSION);
        profiles.push(profile);
    }
    for profile in profiles {
        store
            .upsert_capability_profile(&profile)
            .await
            .expect("profile update");
    }
    let required = vec![
        CapabilityId::from("transport.http"),
        CapabilityId::from("tools.crl.additional_tools"),
        CapabilityId::from("tools.namespace"),
        CapabilityId::from("tools.function"),
    ];
    let required_requirements = crl_requirements();
    let now = chrono::Utc::now();
    store
        .upsert_capability_route_admission(
            &CapabilityRouteAdmission {
                route_id: route.id.clone(),
                capability_shape_hash: capability_shape_hash_from_requirements(
                    &required_requirements,
                ),
                required_capabilities: required,
                required_requirements,
                mode: tiygate_core::CapabilityRoutingMode::Enforce,
                gate_policy_version: 1,
                report: json!({
                    "gate_passed": true,
                    "registry_version": tiygate_store::capabilities::CAPABILITY_REGISTRY_VERSION,
                    "baseline_version": tiygate_store::capabilities::CAPABILITY_BASELINE_VERSION,
                    "shape_hash_version": "shape/v1"
                }),
                approved_by: Some("test".to_string()),
                approved_at: Some(now),
                expires_at: Some(now + chrono::Duration::hours(1)),
                revision: 0,
                created_at: now,
                updated_at: now,
            },
            None,
        )
        .await
        .expect("admission");
    let mut streaming_requirements = crl_requirements();
    streaming_requirements.push(CapabilityRequirement::required("transport.sse"));
    store
        .upsert_capability_route_admission(
            &CapabilityRouteAdmission {
                route_id: route.id,
                capability_shape_hash: capability_shape_hash_from_requirements(
                    &streaming_requirements,
                ),
                required_capabilities: vec![
                    CapabilityId::from("transport.http"),
                    CapabilityId::from("transport.sse"),
                    CapabilityId::from("tools.crl.additional_tools"),
                    CapabilityId::from("tools.namespace"),
                    CapabilityId::from("tools.function"),
                ],
                required_requirements: streaming_requirements,
                mode: tiygate_core::CapabilityRoutingMode::Enforce,
                gate_policy_version: 1,
                report: json!({
                    "gate_passed": true,
                    "registry_version": tiygate_store::capabilities::CAPABILITY_REGISTRY_VERSION,
                    "baseline_version": tiygate_store::capabilities::CAPABILITY_BASELINE_VERSION,
                    "shape_hash_version": "shape/v1"
                }),
                approved_by: Some("test".to_string()),
                approved_at: Some(now),
                expires_at: Some(now + chrono::Duration::hours(1)),
                revision: 0,
                created_at: now,
                updated_at: now,
            },
            None,
        )
        .await
        .expect("streaming admission");
    store
        .set_setting(
            tiygate_store::settings_keys::CAPABILITY_PROBE_ENABLED,
            "false",
        )
        .await
        .expect("pause probes");
    store
        .set_setting(
            tiygate_store::settings_keys::RESPONSES_CRL_TOOL_PROMOTION_ENABLED,
            "true",
        )
        .await
        .expect("enable promotion");
    store
        .set_setting(
            tiygate_store::settings_keys::INGRESS_REQUIRE_API_KEY,
            "false",
        )
        .await
        .expect("disable auth");
    let config = ConfigStore::from_snapshot(store.config_store().snapshot().expect("snapshot"));
    let server_config = ServerConfig {
        require_api_key: false,
        ..ServerConfig::default()
    };
    let telemetry = Arc::new(tiygate_server::telemetry::ChannelTelemetryBus::spawn(
        Arc::new(tiygate_store::log_sink::stdout::StdoutSink::new()),
        32,
    ));
    let router = ingress::router_with_telemetry_full(
        config,
        Arc::new(HealthRegistry::with_defaults()),
        &server_config,
        telemetry,
        None,
        None,
        Some(store),
        None,
    );
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    let streaming_response = router
        .clone()
        .oneshot(request(json!({
            "model": "mixed-model",
            "stream": true,
            "input": [
                {"role": "user", "content": "run"},
                {"type": "additional_tools", "role": "developer", "tools": [{
                    "type": "namespace", "name": "functions", "tools": [
                        {"type": "function", "name": "wait", "parameters": {"type": "object"}}
                    ]
                }]}
            ],
            "tool_choice": "auto"
        })))
        .await
        .expect("streaming response");
    assert_eq!(streaming_response.status(), StatusCode::OK);
    let streaming_body = axum::body::to_bytes(streaming_response.into_body(), usize::MAX)
        .await
        .expect("streaming body");
    assert!(String::from_utf8_lossy(&streaming_body).contains("resp-stream"));
    assert!(
        !String::from_utf8_lossy(&streaming_body).contains("additional_tools are not supported")
    );

    let response = router
        .oneshot(request(json!({
            "model": "mixed-model",
            "input": [
                {"role": "user", "content": "run"},
                {"type": "additional_tools", "role": "developer", "tools": [{
                    "type": "namespace", "name": "functions", "tools": [
                        {"type": "function", "name": "wait", "parameters": {"type": "object"}}
                    ]
                }]}
            ],
            "tool_choice": "auto"
        })))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let native_requests = native.received_requests().await.expect("native requests");
    assert_eq!(native_requests.len(), 2);
    assert!(String::from_utf8_lossy(&native_requests[0].body).contains("additional_tools"));
    let promoted_requests = promoted
        .received_requests()
        .await
        .expect("promoted requests");
    assert_eq!(promoted_requests.len(), 2);
    let promoted_body: serde_json::Value = promoted_requests
        .iter()
        .filter_map(|request| serde_json::from_slice(&request.body).ok())
        .find(|body: &serde_json::Value| body["stream"] != true)
        .expect("promoted non-stream json");
    assert!(promoted_body["input"]
        .as_array()
        .expect("promoted input")
        .iter()
        .all(|item| item["type"] != "additional_tools"));
    assert_eq!(promoted_body["tools"][0]["type"], "namespace");
}
