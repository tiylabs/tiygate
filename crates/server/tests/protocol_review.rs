//! Original synthetic wire regressions from the 2026-10-02 protocol review.
//! Assertions inspect native wire fields independently of the target decoder.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{json, Value};
fn events(wire: &[u8]) -> Vec<Value> {
    String::from_utf8(wire.to_vec())
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect()
}
use axum::{
    body::{Body, Bytes},
    http::{Request, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use std::{sync::Arc, time::Duration};
use tiygate_core::{HealthRegistry, ProtocolSuite, RoutingTable, RoutingTarget};
use tiygate_store::config::ConfigStore;
use tower::ServiceExt;
async fn http_transcode(
    chunks: Vec<Vec<u8>>,
    egress: ProtocolSuite,
    ingress_path: &str,
    body: Value,
) -> String {
    let chunks = Arc::new(chunks);
    let app = Router::new().route(
        "/*path",
        post(move || {
            let chunks = chunks.clone();
            async move {
                let s = futures::stream::unfold((chunks, 0usize), |(chunks, i)| async move {
                    if i >= chunks.len() {
                        None
                    } else {
                        tokio::time::sleep(Duration::from_millis(35)).await;
                        Some((
                            Ok::<Bytes, std::io::Error>(Bytes::from(chunks[i].clone())),
                            (chunks, i + 1),
                        ))
                    }
                });
                Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(Body::from_stream(s))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut table = RoutingTable::new();
    table.insert(
        "m".into(),
        vec![RoutingTarget {
            vendor: None,
            provider_id: if egress == ProtocolSuite::AnthropicMessages {
                "anthropic"
            } else {
                "openai-compatible"
            }
            .into(),
            model_id: "upstream".into(),
            api_base: url,
            api_key: "test".into(),
            api_protocol: egress.default_endpoint(),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
            oauth: None,
        }],
    );
    let cfg = tiygate_server::config::ServerConfig {
        require_api_key: false,
        ..Default::default()
    };
    let gateway = tiygate_server::ingress::router(
        ConfigStore::with_routing_table(table),
        Arc::new(HealthRegistry::with_defaults()),
        &cfg,
    );
    let resp = gateway
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(ingress_path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    handle.abort();
    String::from_utf8(bytes.to_vec()).unwrap()
}
#[tokio::test]
async fn http_utf8_byte_split_preserves_text() {
    let wire="data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"你好😀\"}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n";
    let b = wire.as_bytes();
    let cut = wire.find('你').unwrap() + 1;
    let chunks = vec![b[..cut].to_vec(), b[cut..].to_vec()];
    let out = http_transcode(
        chunks,
        ProtocolSuite::AnthropicMessages,
        "/v1/chat/completions",
        json!({"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    println!("{out}");
    let text: String = events(out.as_bytes())
        .iter()
        .filter_map(|e| e["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "你好😀");
}
#[tokio::test]
async fn http_multiline_sse_data_is_one_event() {
    let wire="data: {\"type\":\"content_block_delta\",\n data: \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n".replace("\n data:","\ndata:");
    let out = http_transcode(
        vec![wire.into_bytes()],
        ProtocolSuite::AnthropicMessages,
        "/v1/chat/completions",
        json!({"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    println!("{out}");
    let text: String = events(out.as_bytes())
        .iter()
        .filter_map(|e| e["choices"][0]["delta"]["content"].as_str())
        .collect();
    assert_eq!(text, "hi");
}
#[tokio::test]
async fn http_chat_error_done_has_no_success_completion() {
    let wire =
        b"data: {\"error\":{\"message\":\"boom\",\"type\":\"server_error\"}}\n\ndata: [DONE]\n\n"
            .to_vec();
    let out = http_transcode(
        vec![wire],
        ProtocolSuite::OpenAiCompatible,
        "/v1/responses",
        json!({"model":"m","stream":true,"input":"hi"}),
    )
    .await;
    println!("{out}");
    assert!(!events(out.as_bytes())
        .iter()
        .any(|e| e["type"] == "response.completed" && e["response"]["status"] == "completed"));
}

#[tokio::test]
async fn http_interleaved_tools_preserve_block_references() {
    let wire = concat!(
      "data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"f\",\"arguments\":\"\"}},{\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"g\",\"arguments\":\"\"}}]}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{}\"}},{\"index\":1,\"function\":{\"arguments\":\"{ \"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n"
    );
    let out = http_transcode(vec![wire.as_bytes().to_vec()], ProtocolSuite::OpenAiCompatible, "/v1/messages",
        json!({"model":"m","max_tokens":100,"stream":true,"messages":[{"role":"user","content":"hi"}]})).await;
    let es = events(out.as_bytes());
    let mut opened = std::collections::HashMap::new();
    for event in &es {
        let index = event["index"].as_u64().unwrap_or(0);
        match event["type"].as_str() {
            Some("content_block_start") => {
                opened.insert(
                    index,
                    event["content_block"]["id"].as_str().unwrap().to_string(),
                );
            }
            Some("content_block_delta") if event["delta"]["type"] == "input_json_delta" => {
                let id = opened.get(&index).expect("delta for an open tool block");
                if event["delta"]["partial_json"] == "{}" {
                    assert_eq!(id, "a");
                } else {
                    assert_eq!(id, "b");
                }
            }
            Some("content_block_stop") => {
                assert!(opened.remove(&index).is_some());
            }
            _ => {}
        }
    }
    assert!(opened.is_empty(), "all tool blocks must close");
}

#[tokio::test]
async fn http_malformed_frame_stops_before_success() {
    let out = http_transcode(
        vec![b"data: not-json\n\ndata: {\"type\":\"message_stop\"}\n\n".to_vec()],
        ProtocolSuite::AnthropicMessages,
        "/v1/chat/completions",
        json!({"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}),
    )
    .await;
    let es = events(out.as_bytes());
    assert!(es.iter().any(|e| e.get("error").is_some()));
    assert!(!es
        .iter()
        .any(|e| e["choices"][0]["finish_reason"] == "stop"));
}
