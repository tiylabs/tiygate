#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Map};
use tower::ServiceExt;

use tiygate_core::protocol::{ProtocolEndpoint, ProtocolSuite};
use tiygate_core::routing::{HealthRegistry, RoutingTable};
use tiygate_server::config::ServerConfig;
use tiygate_server::ingress;
use tiygate_store::config::ConfigStore;
use tiygate_store::model_catalog::{ModelCatalog, ModelCatalogStore, ModelMetadata};
use tiygate_store::models::{AuthMode, ConfigSnapshot, Provider, Route, RouteTarget};

fn model_metadata(id: &str, lab_id: &str, display_name: &str) -> ModelMetadata {
    ModelMetadata {
        id: id.to_string(),
        lab_id: lab_id.to_string(),
        display_name: display_name.to_string(),
        family: Some("saved-family".to_string()),
        context_window: Some(42_000),
        max_input_tokens: Some(40_000),
        max_output_tokens: Some(2_000),
        capabilities: Map::new(),
        modalities: None,
        pricing: None,
        metadata: Map::new(),
    }
}

#[tokio::test]
async fn models_endpoint_enriches_visible_routes_from_catalog() {
    let mut routing_table = RoutingTable::new();
    routing_table.insert(
        "zai/glm-test".to_string(),
        vec![tiygate_core::RoutingTarget {
            provider_id: "zai".to_string(),
            model_id: "zai/glm-test".to_string(),
            api_base: "https://example.invalid/v1".to_string(),
            api_key: String::new(),
            api_protocol: ProtocolEndpoint::new(
                ProtocolSuite::OpenAiCompatible,
                "chat-completions",
                "v1",
            ),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
            oauth: None,
        }],
    );
    let catalog = ModelCatalog::from_models_dev_json(
        r#"{"zhipuai":{"id":"zhipuai","name":"Zhipu AI","models":{"zhipuai/glm-test":{"id":"zhipuai/glm-test","name":"GLM Test","family":"glm","tool_call":true,"structured_output":true,"modalities":{"input":["text","image"],"output":["text"]},"limit":{"context":128000,"output":4096},"cost":{"input":1.0,"output":2.0}}}}}"#,
        "test",
    )
    .expect("catalog");
    let model_catalog = Arc::new(ModelCatalogStore::new(catalog));
    let cfg = ServerConfig {
        require_api_key: false,
        ..Default::default()
    };
    let app = ingress::router_with_telemetry_full(
        ConfigStore::with_routing_table(routing_table),
        Arc::new(HealthRegistry::with_defaults()),
        &cfg,
        Arc::new(tiygate_server::telemetry::ChannelTelemetryBus::spawn(
            Arc::new(tiygate_store::log_sink::stdout::StdoutSink::new()),
            64,
        )),
        None,
        None,
        None,
        Some(model_catalog),
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/models")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let model = &json["data"][0];
    assert_eq!(model["id"], json!("zai/glm-test"));
    assert_eq!(model["owned_by"], json!("zhipuai"));
    assert_eq!(model["display_name"], json!("GLM Test"));
    assert_eq!(model["context_window"], json!(128000));
    assert_eq!(model["capabilities"]["vision"], json!(true));
    assert_eq!(model["pricing"]["source_provider"], json!("zhipuai"));
}

#[tokio::test]
async fn models_endpoint_prefers_persisted_route_metadata_over_catalog() {
    let now = chrono::Utc::now();
    let provider = Provider {
        id: "openai".to_string(),
        name: "OpenAI".to_string(),
        vendor: "openai".to_string(),
        api_base: "https://api.openai.com/v1".to_string(),
        models_endpoint: String::new(),
        encrypted_api_key: String::new(),
        auth_mode: AuthMode::None,
        encrypted_oauth_meta: String::new(),
        metadata_json: json!({}),
        enabled: true,
        created_at: now,
        updated_at: now,
        api_key_cleartext: None,
        oauth_meta_cleartext: None,
    };
    let route = Route {
        id: "route-1".to_string(),
        virtual_model: "virtual/gpt".to_string(),
        targets: vec![RouteTarget {
            provider_id: "openai".to_string(),
            model_id: "zhipuai/glm-test".to_string(),
            weight: 1.0,
            enabled: true,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
        }],
        routing_strategy: None,
        model_metadata: Some(model_metadata(
            "virtual/gpt",
            "custom-lab",
            "Saved Virtual GPT",
        )),
        enabled: true,
        created_at: now,
        updated_at: now,
    };
    let cfg_store = ConfigStore::from_snapshot(ConfigSnapshot {
        epoch: 1,
        providers: HashMap::from([("openai".to_string(), provider)]),
        routes: HashMap::from([("virtual/gpt".to_string(), route)]),
    });
    let catalog = ModelCatalog::from_models_dev_json(
        r#"{"zhipuai":{"id":"zhipuai","name":"Zhipu AI","models":{"zhipuai/glm-test":{"id":"zhipuai/glm-test","name":"Catalog GLM","limit":{"context":128000}}}}}"#,
        "test",
    )
    .expect("catalog");
    let app = ingress::router_with_telemetry_full(
        cfg_store,
        Arc::new(HealthRegistry::with_defaults()),
        &ServerConfig {
            require_api_key: false,
            ..Default::default()
        },
        Arc::new(tiygate_server::telemetry::ChannelTelemetryBus::spawn(
            Arc::new(tiygate_store::log_sink::stdout::StdoutSink::new()),
            64,
        )),
        None,
        None,
        None,
        Some(Arc::new(ModelCatalogStore::new(catalog))),
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/models")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let model = &json["data"][0];
    assert_eq!(model["id"], json!("virtual/gpt"));
    assert_eq!(model["owned_by"], json!("custom-lab"));
    assert_eq!(model["display_name"], json!("Saved Virtual GPT"));
    assert_eq!(model["context_window"], json!(42000));
    assert_eq!(model["family"], json!("saved-family"));
}
