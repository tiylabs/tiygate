//! PostgreSQL integration fixture for the target-capability control plane.
//!
//! The test is a no-op when `TIYGATE_DATABASE_URL` is not set, which keeps
//! local SQLite-only development fast. CI runs it against the pinned
//! PostgreSQL service declared in `.github/workflows/quality.yml`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use chrono::Utc;
use serde_json::json;

use tiygate_core::telemetry::{EventPayload, PipelineEvent};
use tiygate_core::{
    capability_shape_hash_from_requirements, CapabilityId, CapabilityRequirement,
    CapabilityRoutingMode, CapabilityValue, EventSink, RequirementStrength, TargetKey,
};
use tiygate_store::capabilities::{
    CapabilityMutationIdempotency, CapabilityRouteAdmission, TargetCapabilityProfile,
};
use tiygate_store::config_store::DbConfigStore;
use tiygate_store::db;
use tiygate_store::encryption::KeyEncryption;
use tiygate_store::log_sink::oltp::{list_capability_shadow_metrics, OltpSink};

#[tokio::test]
async fn postgres_capability_schema_and_recovery_fixture() {
    let Ok(database_url) = std::env::var("TIYGATE_DATABASE_URL") else {
        return;
    };
    assert!(
        database_url.starts_with("postgres://") || database_url.starts_with("postgresql://"),
        "this fixture must run against PostgreSQL"
    );
    let pool = db::open_pool_with_max_connections(&database_url, 8)
        .await
        .expect("open PostgreSQL pool");
    db::run_migrations(&pool).await.expect("run migrations");
    let encryption = Arc::new(KeyEncryption::from_bytes([0x42; 32]));
    let store = DbConfigStore::new(pool.clone(), Some(encryption));
    store
        .ensure_fingerprint_secret()
        .await
        .expect("fingerprint secret");

    let identity = tiygate_core::CanonicalTargetIdentity {
        identity_version: 1,
        provider_id: "postgres-fixture".to_string(),
        credential_scope_fingerprint: "fixture-scope".to_string(),
        canonical_api_base: "https://fixture.example/v1".to_string(),
        egress_protocol_suite: "openai_responses".to_string(),
        egress_endpoint_name: "responses".to_string(),
        egress_endpoint_version: "v1".to_string(),
        egress_dialect_id: "openai-responses-standard".to_string(),
        exact_model_id: "fixture-model".to_string(),
    };
    let target_key = TargetKey(format!("postgres-fixture-{}", uuid::Uuid::now_v7()));
    let profile = TargetCapabilityProfile::pending(&identity, target_key.clone());
    store
        .upsert_capability_profile(&profile)
        .await
        .expect("profile upsert");
    let loaded_profile = store
        .get_capability_profile(&target_key)
        .await
        .expect("profile read")
        .expect("profile row");
    assert_eq!(loaded_profile.registry_version, 1);
    assert_eq!(loaded_profile.baseline_version, 1);
    let job = store
        .enqueue_probe_job(&target_key, &["http.basic".to_string()], 1, 2)
        .await
        .expect("probe job");
    let claimed = store
        .claim_probe_job("postgres-fixture-worker", 60)
        .await
        .expect("claim")
        .expect("claimed job");
    assert_eq!(claimed.id, job.id);
    assert!(store
        .complete_probe_job(&job.id, "postgres-fixture-worker", "partial")
        .await
        .expect("partial completion"));
    let resumed = store
        .claim_probe_job("postgres-fixture-worker-2", 60)
        .await
        .expect("reclaim")
        .expect("reclaimed job");
    assert_eq!(resumed.id, job.id);

    let required_requirements = vec![CapabilityRequirement::with_value(
        "tools.namespace",
        RequirementStrength::Required,
        CapabilityValue::EnumSet(["functions".to_string()].into_iter().collect()),
    )];
    let shape_hash = capability_shape_hash_from_requirements(&required_requirements);
    let now = Utc::now();
    let admission = store
        .upsert_capability_route_admission(
            &CapabilityRouteAdmission {
                route_id: "postgres-fixture-route".to_string(),
                capability_shape_hash: shape_hash,
                required_capabilities: vec![CapabilityId::from("tools.namespace")],
                required_requirements: required_requirements.clone(),
                mode: CapabilityRoutingMode::Shadow,
                gate_policy_version: 1,
                report: json!({"gate_passed": false}),
                approved_by: None,
                approved_at: None,
                expires_at: Some(now + chrono::Duration::hours(1)),
                revision: 0,
                created_at: now,
                updated_at: now,
            },
            None,
        )
        .await
        .expect("admission");
    assert_eq!(admission.revision, 1);
    assert_eq!(admission.required_requirements, required_requirements);

    let payload = json!({"job_id": job.id});
    let reservation = store
        .begin_capability_mutation("postgres-fixture", "key-1", &payload)
        .await
        .expect("idempotency reservation");
    let request_hash = match reservation {
        CapabilityMutationIdempotency::New { request_hash } => request_hash,
        other => panic!("unexpected reservation: {other:?}"),
    };
    store
        .complete_capability_mutation(
            "postgres-fixture",
            "key-1",
            &request_hash,
            200,
            &json!({"ok": true}),
        )
        .await
        .expect("complete idempotency");
    assert!(matches!(
        store
            .begin_capability_mutation("postgres-fixture", "key-1", &payload)
            .await
            .expect("replay idempotency"),
        CapabilityMutationIdempotency::Replay { status: 200, .. }
    ));

    let sink = OltpSink::new(Arc::new(pool.clone()));
    sink.write_event(&PipelineEvent {
        request_id: "postgres-gap-request".to_string(),
        timestamp: Utc::now(),
        stage: "capability_planner".to_string(),
        payload: EventPayload::CapabilityPlan {
            mode: "shadow".to_string(),
            route_id: "postgres-fixture-route".to_string(),
            shape_hash: "shape/v1:postgres".to_string(),
            planning_micros: 5,
            requirements: vec!["tools.function".to_string()],
            target: target_key.as_str().to_string(),
            status: "compatible".to_string(),
            missing: Vec::new(),
            unknown: Vec::new(),
            transform: None,
            evidence: Vec::new(),
        },
    })
    .await
    .expect("postgres capability plan");
    sink.write_event(&PipelineEvent {
        request_id: "postgres-gap-request".to_string(),
        timestamp: Utc::now(),
        stage: "capability_telemetry_gap".to_string(),
        payload: EventPayload::CapabilityTelemetryGap {
            route_id: "postgres-fixture-route".to_string(),
            shape_hash: "shape/v1:postgres".to_string(),
            target: target_key.as_str().to_string(),
            reason: "fixture_gap".to_string(),
            dropped_count: 1,
        },
    })
    .await
    .expect("postgres capability gap");
    let metrics = list_capability_shadow_metrics(
        &pool,
        Some("postgres-fixture-route"),
        Some("shape/v1:postgres"),
        None,
        None,
    )
    .await
    .expect("postgres capability metrics");
    assert_eq!(metrics.len(), 1);
    assert!(metrics[0].telemetry_gap);
}
