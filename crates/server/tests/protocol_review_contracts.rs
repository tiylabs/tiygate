//! Original HTTP wire regressions for TG-PROTO-053 through TG-PROTO-061.
use axum::{
    body::{Body, Bytes},
    http::Request,
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tiygate_core::{HealthRegistry, ProtocolSuite, RoutingTable, RoutingTarget};
use tiygate_store::config::ConfigStore;
use tower::ServiceExt;
type TestResult = Result<(), Box<dyn std::error::Error>>;
async fn gateway_call(
    path: &str,
    request: Value,
    egress: ProtocolSuite,
    payload: Vec<u8>,
    streaming: bool,
) -> Result<(u16, String, Value), Box<dyn std::error::Error>> {
    let captured = Arc::new(Mutex::new(Value::Null));
    let upstream_capture = captured.clone();
    let app = Router::new().route(
        "/*path",
        post(move |Json(upstream_request): Json<Value>| {
            let payload = payload.clone();
            let captured = upstream_capture.clone();
            async move {
                if let Ok(mut capture) = captured.lock() {
                    *capture = upstream_request;
                }
                if streaming {
                    let stream =
                        futures::stream::unfold((payload, 0usize), |(bytes, index)| async move {
                            if index >= bytes.len() {
                                None
                            } else {
                                tokio::time::sleep(Duration::from_millis(1)).await;
                                Some((
                                    Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(
                                        &bytes[index..index + 1],
                                    )),
                                    (bytes, index + 1),
                                ))
                            }
                        });
                    let mut response = Body::from_stream(stream).into_response();
                    response.headers_mut().insert(
                        "content-type",
                        http::HeaderValue::from_static("text/event-stream"),
                    );
                    response
                } else {
                    match serde_json::from_slice::<Value>(&payload) {
                        Ok(value) => Json(value).into_response(),
                        Err(_) => http::StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                    }
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let handle = tokio::spawn(async move { axum::serve(listener, app).await });
    let mut table = RoutingTable::new();
    table.insert(
        "m".into(),
        vec![RoutingTarget {
            vendor: None,
            provider_id: "openai-compatible".into(),
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
    let config = tiygate_server::config::ServerConfig {
        require_api_key: false,
        ..Default::default()
    };
    let router = tiygate_server::ingress::router(
        ConfigStore::with_routing_table(table),
        Arc::new(HealthRegistry::with_defaults()),
        &config,
    );
    let response = router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(request.to_string()))?,
        )
        .await?;
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
    handle.abort();
    let captured = captured
        .lock()
        .map_err(|_| "upstream capture poisoned")?
        .clone();
    Ok((status, String::from_utf8(bytes.to_vec())?, captured))
}
fn chat_reply() -> Vec<u8> {
    json!({"id":"r","choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]})
        .to_string()
        .into_bytes()
}
fn messages_tool_wire(arguments: &str, reason: &str) -> Vec<u8> {
    let frames = [
        json!({"type":"message_start","message":{"id":"r"}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c","name":"f","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":arguments}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":reason}}),
        json!({"type":"message_stop"}),
    ];
    frames
        .iter()
        .map(|p| format!("data: {p}\n\n"))
        .collect::<String>()
        .into_bytes()
}
#[tokio::test]
async fn http_messages_refusal_nonstream_preserves_text() -> TestResult {
    let upstream = json!({"id":"r","choices":[{"message":{"role":"assistant","content":null,"refusal":"拒绝内容"},"finish_reason":"content_filter"}]});
    let (status, out, _) = gateway_call(
        "/v1/messages",
        json!({"model":"m","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}),
        ProtocolSuite::OpenAiCompatible,
        upstream.to_string().into_bytes(),
        false,
    )
    .await?;
    assert_eq!(status, 200);
    assert!(out.contains("拒绝内容"));
    Ok(())
}
#[tokio::test]
async fn http_incomplete_response_tool_preserves_length() -> TestResult {
    let upstream = json!({"id":"r","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"function_call","id":"fc","call_id":"c","name":"f","arguments":"{}","status":"incomplete"}]});
    let (status, out, _) = gateway_call(
        "/v1/chat/completions",
        json!({"model":"m","messages":[{"role":"user","content":"hi"}]}),
        ProtocolSuite::OpenAiResponses,
        upstream.to_string().into_bytes(),
        false,
    )
    .await?;
    assert_eq!(status, 200);
    let out: Value = serde_json::from_str(&out)?;
    assert_eq!(out["choices"][0]["finish_reason"], "length");
    Ok(())
}
#[tokio::test]
async fn http_malformed_messages_tool_never_response_completed() -> TestResult {
    let (status, out, _) = gateway_call(
        "/v1/responses",
        json!({"model":"m","stream":true,"input":"hi"}),
        ProtocolSuite::AnthropicMessages,
        messages_tool_wire("{\"x\":", "tool_use"),
        true,
    )
    .await?;
    assert_eq!(status, 200);
    assert!(!out.contains("\"type\":\"response.completed\""));
    assert!(out.contains("\"type\":\"error\""));
    Ok(())
}
#[tokio::test]
async fn http_responses_refusal_uses_native_content_part() -> TestResult {
    let upstream = json!({"id":"r","choices":[{"message":{"content":null,"refusal":"拒绝内容"},"finish_reason":"content_filter"}]});
    let (status, out, _) = gateway_call(
        "/v1/responses",
        json!({"model":"m","input":"hi"}),
        ProtocolSuite::OpenAiCompatible,
        upstream.to_string().into_bytes(),
        false,
    )
    .await?;
    assert_eq!(status, 200);
    let out: Value = serde_json::from_str(&out)?;
    assert_eq!(out["output"][0]["type"], "message");
    assert_eq!(out["output"][0]["content"][0]["type"], "refusal");
    Ok(())
}
#[tokio::test]
async fn http_messages_hosted_tool_rejected_before_upstream() -> TestResult {
    let(status,_,captured)=gateway_call("/v1/messages",json!({"model":"m","max_tokens":100,"messages":[{"role":"user","content":"search"}],"tools":[{"type":"web_search_20250305","name":"web_search","max_uses":1}]}),ProtocolSuite::OpenAiCompatible,chat_reply(),false).await?;
    assert_eq!(status, 400);
    assert!(captured.is_null());
    Ok(())
}
#[tokio::test]
async fn http_valid_messages_stream_completes() -> TestResult {
    let (status, out, _) = gateway_call(
        "/v1/responses",
        json!({"model":"m","stream":true,"input":"hi"}),
        ProtocolSuite::AnthropicMessages,
        messages_tool_wire("{\"x\":1}", "tool_use"),
        true,
    )
    .await?;
    assert_eq!(status, 200);
    assert!(out.contains("\"type\":\"response.completed\""));
    assert!(out.contains("{\\\"x\\\":1}"));
    Ok(())
}
#[tokio::test]
async fn http_messages_length_truncation_keeps_incomplete() -> TestResult {
    let (status, out, _) = gateway_call(
        "/v1/responses",
        json!({"model":"m","stream":true,"input":"hi"}),
        ProtocolSuite::AnthropicMessages,
        messages_tool_wire("{\"x\":", "max_tokens"),
        true,
    )
    .await?;
    assert_eq!(status, 200);
    assert!(out.contains("\"type\":\"response.incomplete\""));
    assert!(!out.contains("\"type\":\"response.completed\""));
    Ok(())
}
#[tokio::test]
async fn http_responses_system_remains_instruction() -> TestResult {
    let upstream = json!({"id":"r","type":"message","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"});
    let(status,_,captured)=gateway_call("/v1/responses",json!({"model":"m","input":[{"role":"developer","content":"中文指令"},{"role":"user","content":"hi"}]}),ProtocolSuite::AnthropicMessages,upstream.to_string().into_bytes(),false).await?;
    assert_eq!(status, 200);
    assert!(captured["system"].to_string().contains("中文指令"));
    assert!(!captured["messages"].to_string().contains("中文指令"));
    Ok(())
}
#[tokio::test]
async fn http_gemini_uppercase_thinking_level_reaches_responses() -> TestResult {
    let upstream = json!({"id":"r","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}]});
    let(status,_,captured)=gateway_call("/v1beta/models/m:generateContent",json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingLevel":"HIGH"}}}),ProtocolSuite::OpenAiResponses,upstream.to_string().into_bytes(),false).await?;
    assert_eq!(status, 200);
    assert_eq!(captured["reasoning"]["effort"], "high");
    Ok(())
}
#[tokio::test]
async fn http_messages_raw_preserves_native_thinking_and_cache() -> TestResult {
    let upstream = json!({"id":"r","type":"message","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"});
    let system =
        json!([{"type":"text","text":"rules","cache_control":{"type":"ephemeral","ttl":"1h"}}]);
    let thinking = json!({"type":"adaptive"});
    let(status,_,captured)=gateway_call("/v1/messages",json!({"model":"m","max_tokens":2048,"system":system.clone(),"thinking":thinking.clone(),"messages":[{"role":"user","content":"hi"}]}),ProtocolSuite::AnthropicMessages,upstream.to_string().into_bytes(),false).await?;
    assert_eq!(status, 200);
    assert_eq!(captured["thinking"], thinking);
    assert_eq!(captured["system"], system);
    Ok(())
}
