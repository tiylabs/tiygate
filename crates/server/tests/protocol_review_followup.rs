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
    let received = Arc::new(Mutex::new(Value::Null));
    let captured = received.clone();
    let response = upstream.to_string();
    let app = Router::new().route(
        "/*path",
        post(move |axum::Json(body): axum::Json<Value>| {
            let captured = captured.clone();
            let response = response.clone();
            async move {
                if let Ok(mut captured) = captured.lock() {
                    *captured = body;
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
async fn http_failed_response_with_output_not_accepted() -> Result<(), Box<dyn std::error::Error>> {
    let (_,out,status)=run(ProtocolSuite::OpenAiResponses,"/v1/chat/completions",json!({"model":"m","messages":[{"role":"user","content":"hi"}]}),r#"{"id":"r","status":"failed","error":{"code":"server_error","message":"boom-failed-response"},"output":[]}"#,false).await?;
    assert!(status.is_server_error());
    assert!(
        out.contains("boom-failed-response") && out.contains("\"error\""),
        "failed response with empty output must retain native failure"
    );
    Ok(())
}
#[tokio::test]
async fn http_string_stop_must_survive() -> Result<(), Box<dyn std::error::Error>> {
    let (req,_,status)=run(ProtocolSuite::AnthropicMessages,"/v1/chat/completions",json!({"model":"m","messages":[{"role":"user","content":"hi"}],"stop":"END"}),r#"{"id":"r","type":"message","role":"assistant","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn"}"#,false).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(req["stop_sequences"], json!(["END"]));
    Ok(())
}
#[tokio::test]
async fn http_strict_tool_must_survive() -> Result<(), Box<dyn std::error::Error>> {
    let (req,_,status)=run(ProtocolSuite::OpenAiResponses,"/v1/chat/completions",json!({"model":"m","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"f","strict":false,"parameters":{"type":"object","properties":{"x":{"type":"integer"}}}}}]}),r#"{"id":"r","status":"completed","output":[]}"#,false).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(req["tools"][0]["strict"], false);
    Ok(())
}
#[tokio::test]
async fn http_previous_response_id_must_not_be_erased() -> Result<(), Box<dyn std::error::Error>> {
    let (req,_,status)=run(ProtocolSuite::AnthropicMessages,"/v1/responses",json!({"model":"m","input":"continue","previous_response_id":"resp_context"}),r#"{"id":"r","type":"message","role":"assistant","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn"}"#,false).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        req.is_null(),
        "context-only continuation must be rejected before dispatch"
    );
    Ok(())
}
#[tokio::test]
async fn http_refusal_delta_must_survive() -> Result<(), Box<dyn std::error::Error>> {
    let (_,out,_)=run(ProtocolSuite::OpenAiResponses,"/v1/chat/completions",json!({"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}),concat!(
      "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
      "data: {\"type\":\"response.refusal.delta\",\"item_id\":\"msg\",\"output_index\":0,\"content_index\":0,\"delta\":\"I cannot assist\"}\n\n",
      "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"refusal\",\"refusal\":\"I cannot assist\"}]}]}}\n\n"),true).await?;
    assert!(out.contains("I cannot assist"));
    Ok(())
}
#[tokio::test]
async fn http_unterminated_error_does_not_become_success() -> Result<(), Box<dyn std::error::Error>>
{
    let (_,out,_)=run(ProtocolSuite::AnthropicMessages,"/v1/chat/completions",json!({"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}),concat!(
      "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"boom\"}}\n\n",
      "data: {\"type\":\"message_stop\"}"),true).await?;
    assert!(
        !out.contains("\"finish_reason\":\"stop\""),
        "EOF flush emitted success after already reported error"
    );
    Ok(())
}
#[tokio::test]
async fn http_incomplete_uses_correct_event() -> Result<(), Box<dyn std::error::Error>> {
    let (_,out,_)=run(ProtocolSuite::OpenAiCompatible,"/v1/responses",json!({"model":"m","input":"hi","stream":true}),concat!(
      "data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":\"length\"}]}\n\n",
      "data: [DONE]\n\n"),true).await?;
    assert!(out.contains("\"type\":\"response.incomplete\""));
    assert!(!out.contains("\"type\":\"response.completed\""));
    Ok(())
}
#[tokio::test]
async fn http_post_terminal_content_not_emitted() -> Result<(), Box<dyn std::error::Error>> {
    let (_,out,_)=run(ProtocolSuite::OpenAiResponses,"/v1/chat/completions",json!({"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}),concat!(
      "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
      "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\"}}\n\n",
      "data: {\"type\":\"response.output_text.delta\",\"delta\":\"late-content\"}\n\n"),true).await?;
    assert!(!out.contains("late-content"));
    Ok(())
}

#[tokio::test]
async fn http_final_gemini_usage_overrides_earlier_cumulative_count(
) -> Result<(), Box<dyn std::error::Error>> {
    let (_, out, status) = run(ProtocolSuite::GoogleGemini, "/v1/responses",
        json!({"model":"m","input":"hi","stream":true}), concat!(
        "data: {\"responseId\":\"r\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":1,\"totalTokenCount\":11}}\n\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" more\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":5,\"totalTokenCount\":15}}\n\n"
    ), true).await?;
    assert_eq!(status, StatusCode::OK);
    let es = events(&out);
    let terminal = es
        .iter()
        .find(|event| event["type"] == "response.completed")
        .ok_or("missing completed event")?;
    assert_eq!(terminal["response"]["usage"]["output_tokens"], 5);
    assert_eq!(terminal["response"]["usage"]["total_tokens"], 15);
    Ok(())
}

#[tokio::test]
async fn http_document_and_parallel_limit_survive_request_conversion(
) -> Result<(), Box<dyn std::error::Error>> {
    let (request, _, status) = run(ProtocolSuite::OpenAiResponses, "/v1/messages",
        json!({"model":"m","max_tokens":100,"messages":[{"role":"user","content":[{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBERi0x"}}]}],
        "tools":[{"name":"f","input_schema":{"type":"object"}}],"tool_choice":{"type":"auto","disable_parallel_tool_use":true}}),
        r#"{"id":"r","status":"completed","output":[]}"#, false).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(request["parallel_tool_calls"], false);
    assert_eq!(request["input"][0]["content"][0]["type"], "input_file");
    assert_eq!(
        request["input"][0]["content"][0]["file_data"],
        "data:application/pdf;base64,JVBERi0x"
    );
    Ok(())
}

#[tokio::test]
async fn http_multiple_choices_and_malformed_arguments_never_succeed(
) -> Result<(), Box<dyn std::error::Error>> {
    for body in [
        r#"{"id":"r","choices":[{"index":0,"message":{"role":"assistant","content":"A"},"finish_reason":"stop"},{"index":1,"message":{"role":"assistant","content":"B"},"finish_reason":"stop"}]}"#,
        r#"{"id":"r","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"a","type":"function","function":{"name":"f","arguments":"{\"x\":"}}]},"finish_reason":"tool_calls"}]}"#,
    ] {
        let (_, out, status) = run(
            ProtocolSuite::OpenAiCompatible,
            "/v1/messages",
            json!({"model":"m","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}),
            body,
            false,
        )
        .await?;
        assert!(status.is_server_error(), "{status}: {out}");
        assert!(out.contains("error"));
    }
    Ok(())
}

#[tokio::test]
async fn http_done_only_function_arguments_survive() -> Result<(), Box<dyn std::error::Error>> {
    let (_, out, status) = run(ProtocolSuite::OpenAiResponses, "/v1/messages",
        json!({"model":"m","max_tokens":100,"stream":true,"messages":[{"role":"user","content":"hi"}]}), concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc\",\"call_id\":\"a\",\"name\":\"f\",\"arguments\":\"\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc\",\"arguments\":\"{\\\"x\\\":1}\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\",\"output\":[]}}\n\n"
    ), true).await?;
    assert_eq!(status, StatusCode::OK);
    let arguments: String = events(&out)
        .iter()
        .filter_map(|event| event["delta"]["partial_json"].as_str())
        .collect();
    assert_eq!(arguments, "{\"x\":1}");
    Ok(())
}
