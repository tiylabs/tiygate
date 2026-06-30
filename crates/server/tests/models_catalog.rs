#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use tiygate_core::protocol::{ProtocolEndpoint, ProtocolSuite};
use tiygate_core::routing::{HealthRegistry, RoutingTable};
use tiygate_server::config::ServerConfig;
use tiygate_server::ingress;
use tiygate_store::config::ConfigStore;
use tiygate_store::model_catalog::{ModelCatalog, ModelCatalogStore};

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
