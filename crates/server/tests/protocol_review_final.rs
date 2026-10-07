//! Original local HTTP regressions for the final protocol review.
use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tiygate_core::{HealthRegistry, ProtocolSuite, RoutingTable, RoutingTarget};
use tiygate_store::config::ConfigStore;
use tower::ServiceExt;
fn events(wire: &str) -> Vec<Value> {
    wire.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str(data).ok())
        .collect()
}
async fn run(
    egress: ProtocolSuite,
    path: &str,
    request: Value,
    upstream: &str,
    streaming: bool,
) -> Result<(Value, String, StatusCode), Box<dyn std::error::Error>> {
    run_with_base(egress, path, request, upstream, streaming, "").await
}
async fn run_with_base(
    egress: ProtocolSuite,
    path: &str,
    request: Value,
    upstream: &str,
    streaming: bool,
    base_suffix: &str,
) -> Result<(Value, String, StatusCode), Box<dyn std::error::Error>> {
    let received = Arc::new(Mutex::new(Value::Null));
    let captured = received.clone();
    let response = upstream.to_string();
    let app = Router::new().route(
        "/*path",
        post(
            move |uri: axum::http::Uri, axum::Json(body): axum::Json<Value>| {
                let captured = captured.clone();
                let response = response.clone();
                async move {
                    if let Ok(mut captured) = captured.lock() {
                        *captured = body;
                        captured["__actual_path"] = json!(uri.path());
                    }
                    let mut response = Response::new(Body::from(response));
                    response.headers_mut().insert(
                        "content-type",
                        axum::http::HeaderValue::from_static(if streaming {
                            "text/event-stream"
                        } else {
                            "application/json"
                        }),
                    );
                    response
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}{base_suffix}", listener.local_addr()?);
    let handle = tokio::spawn(async move { axum::serve(listener, app).await });
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
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(request.to_string()))?,
        )
        .await?;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await?;
    handle.abort();
    let output = String::from_utf8(bytes.to_vec())?;
    let captured = received
        .lock()
        .map_err(|_| "capture mutex poisoned")?
        .clone();
    println!("status={status} upstream_request={captured} client_output={output}");
    Ok((captured, output, status))
}
#[tokio::test]
async fn http_chat_native_custom_definition_to_responses() -> Result<(), Box<dyn std::error::Error>>
{
    let (req,_,status)=run(ProtocolSuite::OpenAiResponses,"/v1/chat/completions",json!({"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"custom","custom":{"name":"patch","format":{"type":"text"}}}]}),r#"{"id":"r","status":"completed","output":[]}"#,false).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(req["tools"][0]["name"], "patch");
    Ok(())
}
#[tokio::test]
async fn http_gemini_native_schema_to_responses() -> Result<(), Box<dyn std::error::Error>> {
    let (req,_,status)=run(ProtocolSuite::OpenAiResponses,"/v1beta/models/m:generateContent",json!({"contents":[{"parts":[{"text":"hi"}]}],"generationConfig":{"responseMimeType":"application/json","responseSchema":{"type":"OBJECT","properties":{"x":{"type":"STRING","nullable":true}},"required":["x"]}}}),r#"{"id":"r","status":"completed","output":[]}"#,false).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(req["text"]["format"]["schema"]["type"], "object");
    Ok(())
}
#[tokio::test]
async fn http_gemini_code_execution_not_silently_removed() -> Result<(), Box<dyn std::error::Error>>
{
    let (req,_,status)=run(ProtocolSuite::OpenAiCompatible,"/v1beta/models/m:generateContent",json!({"contents":[{"parts":[{"text":"run code"}]}],"tools":[{"codeExecution":{}}]}),r#"{"id":"r","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,false).await?;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "hosted tool disappeared before dispatch: {req}"
    );
    Ok(())
}
#[tokio::test]
async fn http_repeated_chat_tool_name_does_not_abort() -> Result<(), Box<dyn std::error::Error>> {
    let (_,out,status)=run(ProtocolSuite::OpenAiCompatible,"/v1/messages",json!({"model":"m","max_tokens":100,"stream":true,"messages":[{"role":"user","content":"hi"}]}),concat!(
      "data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call\",\"function\":{\"name\":\"f\",\"arguments\":\"{\"}}]}}]}\n\n",
      "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"f\",\"arguments\":\"}\"}}]}}]}\n\n",
      "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
      "data: [DONE]\n\n"),true).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !events(&out).iter().any(|e| e["type"] == "error"),
        "valid repeated identity caused error: {out}"
    );
    Ok(())
}
#[tokio::test]
async fn http_gemini_versioned_base_uses_one_version() -> Result<(), Box<dyn std::error::Error>> {
    for suffix in ["", "/v1beta", "/v1beta/", "/v1"] {
        for streaming in [false, true] {
            let response = if streaming {
                "data: {\"responseId\":\"r\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}\n\n"
            } else {
                r#"{"responseId":"r","candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#
            };
            let (req, _, status) = run_with_base(
                ProtocolSuite::GoogleGemini,
                "/v1/chat/completions",
                json!({"model":"m","stream":streaming,"messages":[{"role":"user","content":"hi"}]}),
                response,
                streaming,
                suffix,
            )
            .await?;
            assert_eq!(status, StatusCode::OK);
            let version = if suffix == "/v1" { "v1" } else { "v1beta" };
            let method = if streaming {
                "streamGenerateContent"
            } else {
                "generateContent"
            };
            assert_eq!(
                req["__actual_path"],
                format!("/{version}/models/upstream:{method}")
            );
        }
    }
    Ok(())
}
#[tokio::test]
async fn http_responses_function_result_to_gemini() -> Result<(), Box<dyn std::error::Error>> {
    let (req,out,status)=run(ProtocolSuite::GoogleGemini,"/v1/responses",json!({"model":"m","input":[{"type":"function_call","id":"fc_item","call_id":"call_fn","name":"f","arguments":"{}"},{"type":"function_call_output","call_id":"call_fn","output":"ok"}]}),r#"{"responseId":"r","candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#,false).await?;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(
        req["contents"][1]["parts"][0]["functionResponse"]["name"],
        "f"
    );
    Ok(())
}
#[tokio::test]
async fn http_specific_tool_choice_chat_to_responses() -> Result<(), Box<dyn std::error::Error>> {
    let (req,_,status)=run(ProtocolSuite::OpenAiResponses,"/v1/chat/completions",json!({"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object"}}}],"tool_choice":{"type":"function","function":{"name":"f"}}}),r#"{"id":"r","status":"completed","output":[]}"#,false).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(req["tool_choice"], json!({"type":"function","name":"f"}));
    Ok(())
}
#[tokio::test]
async fn http_tool_result_image_not_textualized() -> Result<(), Box<dyn std::error::Error>> {
    let (req,_,status)=run(ProtocolSuite::OpenAiResponses,"/v1/messages",json!({"model":"m","max_tokens":100,"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"call","name":"f","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1n"}}]}]}]}),r#"{"id":"r","status":"completed","output":[]}"#,false).await?;
    if status == StatusCode::OK {
        assert!(
            req["input"][1]["output"].is_array(),
            "image became text: {req}"
        );
    } else {
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    Ok(())
}
#[tokio::test]
async fn http_stream_malformed_tool_does_not_complete() -> Result<(), Box<dyn std::error::Error>> {
    let (_,out,status)=run(ProtocolSuite::OpenAiCompatible,"/v1/responses",json!({"model":"m","input":"hi","stream":true}),concat!(
      "data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call\",\"function\":{\"name\":\"f\",\"arguments\":\"{\\\"x\\\":\"}}]},\"finish_reason\":null}]}\n\n",
      "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
      "data: [DONE]\n\n"),true).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(!events(&out)
        .iter()
        .any(|e| e["type"] == "response.completed"));
    Ok(())
}
#[tokio::test]
async fn http_nonstream_refusal_to_gemini_not_lost() -> Result<(), Box<dyn std::error::Error>> {
    let (_,out,status)=run(ProtocolSuite::OpenAiResponses,"/v1beta/models/m:generateContent",json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]}),r#"{"id":"r","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"Cannot assist"}]}]}"#,false).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(out.contains("Cannot assist"));
    Ok(())
}

#[tokio::test]
async fn http_named_function_to_messages_never_panics() -> Result<(), Box<dyn std::error::Error>> {
    let (req,_,status)=run(ProtocolSuite::AnthropicMessages,"/v1/responses",json!({"model":"m","input":"hi","tools":[{"type":"function","name":"f","strict":false,"parameters":{"type":"object"}}],"tool_choice":{"type":"function","name":"f"}}),r#"{"id":"m","type":"message","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}"#,false).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(req["tool_choice"]["name"], "f");
    Ok(())
}
#[tokio::test]
async fn http_history_malformed_parameters_rejected_before_dispatch(
) -> Result<(), Box<dyn std::error::Error>> {
    let (req,_,status)=run(ProtocolSuite::AnthropicMessages,"/v1/responses",json!({"model":"m","input":[{"type":"function_call","call_id":"call","name":"f","arguments":"{\"x\":"}]}),r#"{"id":"m","content":[],"stop_reason":"end_turn"}"#,false).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(req.is_null());
    Ok(())
}
#[tokio::test]
async fn http_large_indices_emit_error_without_success() -> Result<(), Box<dyn std::error::Error>> {
    for (egress, payload) in [
        (
            ProtocolSuite::OpenAiCompatible,
            json!({"id":"r","choices":[{"index":0,"delta":{"tool_calls":[{"index":u64::MAX,"id":"call","function":{"name":"f","arguments":"{}"}}]}}]}),
        ),
        (
            ProtocolSuite::AnthropicMessages,
            json!({"type":"content_block_start","index":u64::MAX,"content_block":{"type":"tool_use","id":"call","name":"f","input":{}}}),
        ),
    ] {
        let wire = format!("data: {payload}\n\n");
        let (_, out, _) = run(
            egress,
            "/v1/responses",
            json!({"model":"m","input":"hi","stream":true}),
            &wire,
            true,
        )
        .await?;
        assert!(events(&out).iter().any(|e| e["type"] == "error"));
        assert!(!events(&out)
            .iter()
            .any(|e| e["type"] == "response.completed"));
    }
    Ok(())
}
#[tokio::test]
async fn http_chat_custom_call_is_native_responses_item() -> Result<(), Box<dyn std::error::Error>>
{
    let wire=concat!("data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call\",\"type\":\"custom\",\"custom\":{\"name\":\"patch\",\"input\":\"hello\"}}]}}]}\n\n","data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n","data: [DONE]\n\n");
    let (_, out, _) = run(
        ProtocolSuite::OpenAiCompatible,
        "/v1/responses",
        json!({"model":"m","input":"hi","stream":true}),
        wire,
        true,
    )
    .await?;
    let es = events(&out);
    assert!(es
        .iter()
        .any(|e| e["item"]["type"] == "custom_tool_call" && e["item"]["input"] == "hello"));
    assert_eq!(
        es.iter()
            .filter(|e| e["type"] == "response.completed")
            .count(),
        1
    );
    Ok(())
}
#[tokio::test]
async fn http_refusal_reason_stays_refusal() -> Result<(), Box<dyn std::error::Error>> {
    let (_,out,_)=run(ProtocolSuite::AnthropicMessages,"/v1/chat/completions",json!({"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}),"data: {\"type\":\"message_start\",\"message\":{\"id\":\"r\"}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n",true).await?;
    assert!(events(&out)
        .iter()
        .any(|e| e["choices"][0]["finish_reason"] == "content_filter"));
    Ok(())
}

#[tokio::test]
async fn http_late_identity_is_buffered_until_open() -> Result<(), Box<dyn std::error::Error>> {
    let wire=concat!(
      "data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"f\",\"arguments\":\"{\"}}]}}]}\n\n",
      "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call\",\"function\":{\"arguments\":\"}\"}}]}}]}\n\n",
      "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n");
    let (_,out,_)=run(ProtocolSuite::OpenAiCompatible,"/v1/messages",json!({"model":"m","max_tokens":100,"stream":true,"messages":[{"role":"user","content":"hi"}]}),wire,true).await?;
    let es = events(&out);
    assert!(!es.iter().any(|e| e["type"] == "error"));
    assert_eq!(
        es.iter()
            .filter(|e| e["type"] == "content_block_start")
            .count(),
        1
    );
    let arguments: String = es
        .iter()
        .filter_map(|e| e["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(arguments, "{}");
    Ok(())
}
#[tokio::test]
async fn http_gemini_result_text_order_survives() -> Result<(), Box<dyn std::error::Error>> {
    let (req,_,status)=run(ProtocolSuite::OpenAiCompatible,"/v1beta/models/m:generateContent",json!({"contents":[{"role":"model","parts":[{"functionCall":{"id":"call","name":"f","args":{}}}]},{"role":"user","parts":[{"text":"before"},{"functionResponse":{"id":"call","name":"f","response":{"output":"result"}}},{"text":"after"}]}]}),r#"{"id":"r","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,false).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(req["messages"][1]["content"], "before");
    assert_eq!(req["messages"][2]["role"], "tool");
    assert_eq!(req["messages"][2]["tool_call_id"], "call");
    assert_eq!(req["messages"][3]["content"], "after");
    Ok(())
}

/// TG-PROTO-051: an OpenAI-compatible upstream that streams a no-argument
/// function call the way OpenAI documents it (first chunk carries
/// `arguments: ""`, no further fragments) must still produce a successful
/// Messages stream for the client.
#[tokio::test]
async fn http_noarg_tool_stream_completes() -> Result<(), Box<dyn std::error::Error>> {
    let (_, out, status) = run(
        ProtocolSuite::OpenAiCompatible,
        "/v1/messages",
        json!({"model":"m","max_tokens":100,"stream":true,"messages":[{"role":"user","content":"hi"}]}),
        concat!(
            "data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"get_time\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n"
        ),
        true,
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "valid no-arg tool call rejected: {out}"
    );
    assert!(
        events(&out)
            .iter()
            .any(|e| e["type"] == "message_delta" && e["delta"]["stop_reason"] == "tool_use")
            && events(&out).iter().any(|e| e["type"] == "message_stop"),
        "no-arg tool call did not complete: {out}"
    );
    assert!(
        !events(&out).iter().any(|e| e["type"] == "error"),
        "no-arg tool call produced an error event: {out}"
    );
    Ok(())
}

#[tokio::test]
async fn http_review_fix_implicit_strict_schema() -> Result<(), Box<dyn std::error::Error>> {
    let (request, _, status) = run(
        ProtocolSuite::OpenAiCompatible,
        "/v1/responses",
        json!({"model":"m","input":"hi","tools":[
            {"type":"function","name":"f","parameters":{
                "type":"object","properties":{"x":{"type":"string"}}
            }}
        ]}),
        r#"{"id":"r","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
        false,
    ).await?;
    assert_eq!(status, StatusCode::OK);
    let function = &request["tools"][0]["function"];
    assert_eq!(function["strict"], true);
    assert_eq!(function["parameters"]["additionalProperties"], false);
    assert_eq!(function["parameters"]["required"], json!(["x"]));
    Ok(())
}

