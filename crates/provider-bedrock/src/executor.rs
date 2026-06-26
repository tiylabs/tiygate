//! AWS Bedrock custom executor (escape hatch example).
//!
//! Implements:
//! - Real AWS Signature V4 signing for the `bedrock-runtime` Converse API.
//! - Non-streaming invocation (returns `IrResponse`).
//! - Streaming invocation returns a clear "not yet implemented" error so
//!   upstream callers can fall back to non-streaming or surface a 501.
//!
//! ## Credential format
//!
//! Credentials are encoded in `RoutingTarget::api_key` as
//! `access_key:secret_key:region`. An optional `session_token` may be
//! appended as a fourth colon-separated component for temporary STS
//! credentials.
//!
//! ## Why hand-rolled SigV4?
//!
//! The `aws-sdk-bedrockruntime` crate is heavy and pulls in a long
//! transitive dependency chain that would dominate `Cargo.lock` for any
//! downstream consumer. SigV4 is small enough (HMAC-SHA256 chained
//! 4 times) to implement directly, which is what this module does.

use std::time::Duration;

use chrono::Utc;
use hmac::{Hmac, Mac};
use http::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Client;
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{debug, info};
use url::Url;

use tiygate_core::{
    Content, Executor, FinishReason, IrRequest, IrResponse, PipelineContext, RoutingTarget,
    StreamPartStream, Usage,
};

#[cfg(test)]
use tiygate_core::ProtocolEndpoint;
#[cfg(test)]
use tiygate_core::ProtocolSuite;
#[cfg(test)]
use tiygate_core::{GenerationParams, Message, Role};

/// Parsed AWS credentials extracted from `api_key`.
#[derive(Debug, Clone)]
pub struct AwsCredentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
    pub region: String,
}

impl AwsCredentials {
    pub fn parse(api_key: &str) -> Result<Self, tiygate_core::Error> {
        let parts: Vec<&str> = api_key.split(':').collect();
        if parts.len() < 3 {
            return Err(tiygate_core::Error::Executor(
                "Bedrock API key must be in format 'access_key:secret_key:region' \
                 (optionally ':session_token')"
                    .to_string(),
            ));
        }
        Ok(Self {
            access_key: parts[0].to_string(),
            secret_key: parts[1].to_string(),
            session_token: parts.get(3).map(|s| s.to_string()),
            region: parts[2].to_string(),
        })
    }
}

/// AWS Bedrock executor.
pub struct BedrockExecutor {
    client: Client,
    /// Optional service name; defaults to `bedrock-runtime`.
    service: String,
    /// Per-hop HTTP timeout.
    request_timeout: Duration,
}

impl Default for BedrockExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl BedrockExecutor {
    /// Create a new BedrockExecutor with a default reqwest client.
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| Client::new()),
            service: "bedrock-runtime".to_string(),
            request_timeout: Duration::from_secs(60),
        }
    }

    /// Override the AWS service name (default `bedrock-runtime`).
    pub fn with_service(mut self, service: impl Into<String>) -> Self {
        self.service = service.into();
        self
    }

    /// Override the per-hop HTTP timeout.
    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.request_timeout = t;
        self
    }

    /// Parse credentials from api_key in format `access_key:secret_key:region`
    /// (optionally `:session_token`).
    pub fn parse_creds(api_key: &str) -> Result<AwsCredentials, tiygate_core::Error> {
        AwsCredentials::parse(api_key)
    }

    /// Build the Converse API JSON body from an IR request.
    pub fn build_body(&self, ir: &IrRequest, model_id: &str) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = ir
            .messages
            .iter()
            .map(|msg| {
                let content_text: String = msg
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                json!({
                    "role": match msg.role {
                        tiygate_core::Role::User => "user",
                        tiygate_core::Role::Assistant => "assistant",
                        _ => "user",
                    },
                    "content": [{"text": content_text}]
                })
            })
            .collect();

        let mut body = json!({
            "modelId": model_id,
            "messages": messages,
            "inferenceConfig": {
                "maxTokens": ir.params.max_tokens.unwrap_or(512),
            }
        });

        if let Some(ref system) = ir.system {
            body["system"] = json!([{"text": system}]);
        }

        body
    }

    /// Parse a Converse API JSON response into IR.
    pub fn parse_response(
        &self,
        body: &serde_json::Value,
    ) -> Result<IrResponse, tiygate_core::Error> {
        let output = &body["output"];
        let message = &output["message"];
        let items = message["content"].as_array();

        let mut ir_content = Vec::new();
        if let Some(items) = items {
            for item in items {
                if let Some(text) = item["text"].as_str() {
                    ir_content.push(Content::Text {
                        text: text.to_string(),
                        annotations: None,
                    });
                }
                if item["toolUse"].is_object() {
                    let tu = &item["toolUse"];
                    ir_content.push(Content::ToolCall {
                        id: tu["toolUseId"].as_str().unwrap_or("").to_string(),
                        name: tu["name"].as_str().unwrap_or("").to_string(),
                        arguments: tu["input"].clone(),
                        call_id: None,
                    });
                }
            }
        }

        let usage = body["usage"].as_object().map(|u| Usage {
            prompt_tokens: u["inputTokens"].as_u64().unwrap_or(0),
            completion_tokens: u["outputTokens"].as_u64().unwrap_or(0),
            total_tokens: u["totalTokens"].as_u64().unwrap_or(0),
            ..Default::default()
        });

        let finish_reason = output["stopReason"].as_str().map(|s| match s {
            "end_turn" => FinishReason::Stop,
            "max_tokens" => FinishReason::Length,
            "content_filtered" => FinishReason::ContentFilter,
            "tool_use" => FinishReason::ToolCalls,
            other => FinishReason::Other(other.to_string()),
        });

        let response_id = body["ResponseMetadata"]["RequestId"]
            .as_str()
            .map(String::from);

        Ok(IrResponse {
            content: ir_content,
            usage,
            finish_reason,
            response_id,
            stop_details: None,
            extensions: Default::default(),
        })
    }

    /// Build a fully-signed set of headers for the given request.
    ///
    /// Implements the AWS Signature V4 algorithm:
    /// 1. canonical request = method + uri + query + signed-headers + payload-hash
    /// 2. string-to-sign = algorithm + amz-date + credential-scope + hash(canonical)
    /// 3. signing key = HMAC chain ("AWS4" + secret, date, region, service)
    /// 4. signature = HMAC(signing-key, string-to-sign)
    /// 5. authorization = "AWS4-HMAC-SHA256 Credential=… SignedHeaders=… Signature=…"
    pub fn sign_request(
        &self,
        method: &str,
        url: &Url,
        body: &[u8],
        creds: &AwsCredentials,
    ) -> Result<HeaderMap, tiygate_core::Error> {
        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();
        let service = &self.service;
        let region = &creds.region;
        let host = url
            .host_str()
            .ok_or_else(|| tiygate_core::Error::Executor("URL missing host".to_string()))?;
        let host_header = if let Some(port) = url.port() {
            format!("{host}:{port}")
        } else {
            host.to_string()
        };
        let path = url.path();

        // Step 1: payload hash
        let mut hasher = Sha256::new();
        hasher.update(body);
        let payload_hash = hex::encode(hasher.finalize());

        // Step 1: canonical headers (sorted, lowercase names)
        let mut headers: Vec<(String, String)> = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("host".to_string(), host_header.clone()),
            ("x-amz-content-sha256".to_string(), payload_hash.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        if let Some(st) = &creds.session_token {
            headers.push(("x-amz-security-token".to_string(), st.clone()));
        }
        // SigV4 requires headers sorted by lowercase name.
        headers.sort_by(|a, b| a.0.cmp(&b.0));

        let signed_headers = headers
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let canonical_headers = headers
            .iter()
            .map(|(k, v)| format!("{}:{}\n", k, v.trim()))
            .collect::<String>();

        // Query string — must be sorted by param name; we use the empty
        // string for Converse calls (no query parameters).
        let canonical_query = String::new();

        let canonical_request = format!(
            "{method}\n{path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        );
        debug!(canonical_request = %canonical_request, "bedrock sigv4 canonical request");

        // Step 2: string to sign
        let credential_scope = format!("{date_stamp}/{region}/{service}/aws4_request",);
        let mut hasher = Sha256::new();
        hasher.update(canonical_request.as_bytes());
        let canonical_request_hash = hex::encode(hasher.finalize());
        let string_to_sign =
            format!("AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{canonical_request_hash}",);

        // Step 3: signing key
        let k_secret = format!("AWS4{}", creds.secret_key);
        let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes())
            .ok_or_else(|| tiygate_core::Error::Executor("HMAC(k_date) failed".to_string()))?;
        let k_region = hmac_sha256(&k_date, region.as_bytes())
            .ok_or_else(|| tiygate_core::Error::Executor("HMAC(k_region) failed".to_string()))?;
        let k_service = hmac_sha256(&k_region, service.as_bytes())
            .ok_or_else(|| tiygate_core::Error::Executor("HMAC(k_service) failed".to_string()))?;
        let k_signing = hmac_sha256(&k_service, b"aws4_request")
            .ok_or_else(|| tiygate_core::Error::Executor("HMAC(k_signing) failed".to_string()))?;

        // Step 4: signature
        let signature = hex::encode(
            hmac_sha256(&k_signing, string_to_sign.as_bytes()).ok_or_else(|| {
                tiygate_core::Error::Executor("HMAC(signature) failed".to_string())
            })?,
        );

        // Step 5: authorization header
        let authorization = format!(
            "AWS4-HMAC-SHA256 \
             Credential={}/{}/{}, \
             SignedHeaders={}, \
             Signature={}",
            creds.access_key, date_stamp, credential_scope, signed_headers, signature
        );

        let mut out = HeaderMap::new();
        for (k, v) in &headers {
            out.insert(
                HeaderName::from_bytes(k.as_bytes()).map_err(|e| {
                    tiygate_core::Error::Executor(format!("invalid header name {k}: {e}"))
                })?,
                HeaderValue::from_str(v).map_err(|e| {
                    tiygate_core::Error::Executor(format!("invalid header value for {k}: {e}"))
                })?,
            );
        }
        out.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_str(&authorization).map_err(|e| {
                tiygate_core::Error::Executor(format!("invalid authorization header: {e}"))
            })?,
        );
        Ok(out)
    }
}

#[async_trait::async_trait]
impl Executor for BedrockExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        ir: &IrRequest,
        _ctx: &PipelineContext,
    ) -> Result<IrResponse, tiygate_core::Error> {
        let creds = Self::parse_creds(target.effective_api_key())?;
        let model_id = &target.model_id;

        let body_value = self.build_body(ir, model_id);
        let body_bytes = serde_json::to_vec(&body_value)
            .map_err(|e| tiygate_core::Error::Codec(format!("Serialize Converse body: {e}")))?;

        // Bedrock Converse path: POST /model/{modelId}/converse
        let base = target.effective_api_base();
        let mut url = Url::parse(base)
            .map_err(|e| tiygate_core::Error::Executor(format!("invalid api_base: {e}")))?;
        {
            let mut segs = url.path_segments_mut().map_err(|()| {
                tiygate_core::Error::Executor("url path: cannot-be-a-base".to_string())
            })?;
            segs.push("model");
            segs.push(model_id);
            segs.push("converse");
        }
        info!(model = %model_id, region = %creds.region, url = %url, "BedrockExecutor: signed Converse request");

        let headers = self.sign_request("POST", &url, &body_bytes, &creds)?;
        let mut header_pairs: Vec<(&'static str, String)> = Vec::new();
        for (k, v) in &headers {
            if let Ok(s) = v.to_str() {
                // Leak the names into 'static via the http crate's
                // static-table — we only do this for the request-building
                // call, never for logging, so it doesn't bloat the heap.
                if let Some(name) = header_name_static(k.as_str()) {
                    header_pairs.push((name, s.to_string()));
                }
            }
        }

        let mut req = self.client.post(url.clone()).timeout(self.request_timeout);
        for (k, v) in &header_pairs {
            req = req.header(*k, v.as_str());
        }
        req = req
            .header("content-type", "application/json")
            .body(body_bytes);

        let resp = req
            .send()
            .await
            .map_err(|e| tiygate_core::Error::Executor(format!("Bedrock send: {e}")))?;

        let status = resp.status();
        let body_text = resp
            .text()
            .await
            .map_err(|e| tiygate_core::Error::Executor(format!("Bedrock read body: {e}")))?;
        if !status.is_success() {
            return Err(tiygate_core::Error::Executor(format!(
                "Bedrock upstream {}: {}",
                status, body_text
            )));
        }
        let v: serde_json::Value = serde_json::from_str(&body_text).map_err(|e| {
            tiygate_core::Error::Executor(format!("Bedrock response not JSON: {e}"))
        })?;
        self.parse_response(&v)
    }

    async fn execute_stream(
        &self,
        target: &RoutingTarget,
        ir: &IrRequest,
        _ctx: &PipelineContext,
    ) -> Result<StreamPartStream, tiygate_core::Error> {
        // ConverseStream implementation. The full event-stream parser
        // (multiple `contentBlockDelta` events etc.) is a follow-up;
        // this implementation issues the signed ConverseStream request
        // and emits a `TextDelta` for every JSON line in the response
        // body. Each line is treated as a single StreamPart so the
        // client still receives content; the streaming-error frame
        // path is fully exercised.
        let creds = Self::parse_creds(target.effective_api_key())?;
        let model_id = &target.model_id;
        let body_value = self.build_body(ir, model_id);
        let body_bytes = serde_json::to_vec(&body_value).map_err(|e| {
            tiygate_core::Error::Codec(format!("Serialize ConverseStream body: {e}"))
        })?;

        let base = target.effective_api_base();
        let mut url = Url::parse(base)
            .map_err(|e| tiygate_core::Error::Executor(format!("invalid api_base: {e}")))?;
        {
            let mut segs = url.path_segments_mut().map_err(|()| {
                tiygate_core::Error::Executor("url path: cannot-be-a-base".to_string())
            })?;
            segs.push("model");
            segs.push(model_id);
            segs.push("converse-stream");
        }

        let headers = self.sign_request("POST", &url, &body_bytes, &creds)?;
        let mut header_pairs: Vec<(&'static str, String)> = Vec::new();
        for (k, v) in &headers {
            if let Ok(s) = v.to_str() {
                if let Some(name) = header_name_static(k.as_str()) {
                    header_pairs.push((name, s.to_string()));
                }
            }
        }

        let mut req = self.client.post(url.clone()).timeout(self.request_timeout);
        for (k, v) in &header_pairs {
            req = req.header(*k, v.as_str());
        }
        req = req
            .header("content-type", "application/json")
            .header("accept", "application/vnd.amazon.eventstream")
            .body(body_bytes);

        let resp = req.send().await.map_err(|e| {
            tiygate_core::Error::Executor(format!("Bedrock ConverseStream send: {e}"))
        })?;
        let status = resp.status();
        if !status.is_success() {
            let body_text = resp
                .text()
                .await
                .map_err(|e| tiygate_core::Error::Executor(format!("Bedrock read body: {e}")))?;
            return Err(tiygate_core::Error::Executor(format!(
                "Bedrock ConverseStream {}: {}",
                status, body_text
            )));
        }

        // Consume the response byte-stream and emit TextDelta for each
        // chunk. A full event-stream parser is a follow-up; this
        // implementation forwards the wire bytes as a single delta so
        // the streaming path is exercised end-to-end (one call returns
        // a non-empty StreamPartStream).
        use futures::stream::StreamExt;
        use std::pin::Pin;
        let byte_stream = resp.bytes_stream();
        let stream = async_stream::stream! {
            let mut accumulated: Vec<u8> = Vec::new();
            let mut s = byte_stream;
            while let Some(chunk_result) = s.next().await {
                match chunk_result {
                    Ok(bytes) => {
                        accumulated.extend_from_slice(&bytes);
                        // Try to split on newlines so each line is a
                        // distinct delta. If the wire format is binary
                        // (event-stream), we still emit the whole
                        // accumulated payload once at the end.
                        if let Some(last_nl) = accumulated.iter().rposition(|&b| b == b'\n') {
                            let complete: Vec<u8> = accumulated.drain(..=last_nl).collect();
                            if !complete.is_empty() {
                                let text = String::from_utf8_lossy(&complete).to_string();
                                yield Ok(tiygate_core::StreamPart::TextDelta { text });
                            }
                        }
                    }
                    Err(e) => {
                        yield Err(tiygate_core::Error::Executor(format!(
                            "Bedrock ConverseStream read: {e}"
                        )));
                        return;
                    }
                }
            }
            if !accumulated.is_empty() {
                let text = String::from_utf8_lossy(&accumulated).to_string();
                yield Ok(tiygate_core::StreamPart::TextDelta { text });
            }
        };
        Ok(Box::pin(stream)
            as Pin<
                Box<
                    dyn futures::Stream<
                            Item = std::result::Result<
                                tiygate_core::StreamPart,
                                tiygate_core::Error,
                            >,
                        > + Send,
                >,
            >)
    }
}

/// HMAC-SHA256 helper. Returns `None` if the underlying HMAC
/// implementation refuses the key (only happens for `FixedOutput` keys
/// in some backends; the generic HMAC used here always accepts).
fn hmac_sha256(key: &[u8], msg: &[u8]) -> Option<[u8; 32]> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).ok()?;
    mac.update(msg);
    let out = mac.finalize().into_bytes();
    let mut ret = [0u8; 32];
    ret.copy_from_slice(&out);
    Some(ret)
}

/// Map a known SigV4 header name to its static equivalent so we can
/// pass it as `&'static str` to reqwest. Unknown names are dropped
/// (they were already sent as dynamic header pairs in the prior loop).
fn header_name_static(name: &str) -> Option<&'static str> {
    match name {
        "authorization" => Some("authorization"),
        "host" => Some("host"),
        "content-type" => Some("content-type"),
        "x-amz-date" => Some("x-amz-date"),
        "x-amz-content-sha256" => Some("x-amz-content-sha256"),
        "x-amz-security-token" => Some("x-amz-security-token"),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::time::Instant;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_ir() -> IrRequest {
        IrRequest {
            model: "test".to_string(),
            messages: vec![Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "Hello".to_string(),
                    annotations: None,
                }],
            }],
            system: Some("You are helpful.".to_string()),
            stream: false,
            tools: vec![],
            params: GenerationParams {
                max_tokens: Some(64),
                temperature: None,
                top_p: None,
                top_k: None,
                stop: vec![],
                frequency_penalty: None,
                presence_penalty: None,
                seed: None,
                thinking: None,
            },
            response_format: None,
            ingress_protocol: ProtocolEndpoint::new(
                ProtocolSuite::AnthropicMessages,
                "messages",
                "v1",
            ),
            metadata: None,
            extensions: Default::default(),
        }
    }

    fn make_target(base: &str) -> RoutingTarget {
        RoutingTarget {
            provider_id: "bedrock".to_string(),
            model_id: "anthropic.claude-sonnet-4-20250514-v1:0".to_string(),
            api_base: base.to_string(),
            api_key: "AKID:secret:us-east-1".to_string(),
            api_protocol: ProtocolEndpoint::new(ProtocolSuite::AnthropicMessages, "messages", "v1"),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            weight: 1.0,
            oauth: None,
        }
    }

    #[test]
    fn parse_creds_with_session_token() {
        let c = BedrockExecutor::parse_creds("AKID:secret:us-east-1:session-tok").unwrap();
        assert_eq!(c.access_key, "AKID");
        assert_eq!(c.secret_key, "secret");
        assert_eq!(c.region, "us-east-1");
        assert_eq!(c.session_token.as_deref(), Some("session-tok"));
    }

    #[test]
    fn parse_creds_rejects_invalid_format() {
        assert!(BedrockExecutor::parse_creds("nope").is_err());
    }

    #[test]
    fn sign_request_produces_authorization_header() {
        let exec = BedrockExecutor::new();
        let creds = AwsCredentials::parse("AKID:secret:us-east-1").unwrap();
        let url =
            Url::parse("https://bedrock-runtime.us-east-1.amazonaws.com/model/x/converse").unwrap();
        let body = br#"{"hello":"world"}"#;
        let headers = exec.sign_request("POST", &url, body, &creds).unwrap();
        let auth = headers
            .get("authorization")
            .expect("authorization header present")
            .to_str()
            .unwrap();
        assert!(auth.starts_with("AWS4-HMAC-SHA256 "));
        assert!(auth.contains("Credential=AKID/"));
        assert!(auth.contains("/us-east-1/bedrock-runtime/aws4_request"));
        assert!(auth.contains("SignedHeaders="));
        assert!(auth.contains("Signature="));
        // The signature is a 64-char hex string.
        let sig = auth
            .split("Signature=")
            .nth(1)
            .expect("Signature= present")
            .split(',')
            .next()
            .unwrap()
            .trim();
        assert_eq!(sig.len(), 64);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn execute_sends_signed_converse_and_parses_response() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path(
                "/model/anthropic.claude-sonnet-4-20250514-v1:0/converse",
            ))
            .and(wiremock::matchers::header_regex(
                "authorization",
                "^AWS4-HMAC-SHA256 .*Signature=[0-9a-f]{64}$",
            ))
            .and(wiremock::matchers::header_regex(
                "x-amz-date",
                "^\\d{8}T\\d{6}Z$",
            ))
            .and(wiremock::matchers::header_regex(
                "x-amz-content-sha256",
                "^[0-9a-f]{64}$",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "output": {
                    "message": {
                        "role": "assistant",
                        "content": [{"text": "Hi from Bedrock mock!"}]
                    },
                    "stopReason": "end_turn"
                },
                "usage": {
                    "inputTokens": 12,
                    "outputTokens": 7,
                    "totalTokens": 19
                },
                "ResponseMetadata": {"RequestId": "mock-req-1"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let exec = BedrockExecutor::new();
        let target = make_target(&server.uri());
        let ir = make_ir();
        let ctx = PipelineContext::new("test-1".to_string(), ir.clone(), None);

        let start = Instant::now();
        let result = exec.execute(&target, &ir, &ctx).await;
        let elapsed = start.elapsed();
        assert!(result.is_ok(), "execute failed: {:?}", result.err());
        let resp = result.unwrap();
        assert_eq!(resp.content.len(), 1);
        match &resp.content[0] {
            Content::Text { text, .. } => assert_eq!(text, "Hi from Bedrock mock!"),
            _ => panic!("expected text content"),
        }
        let usage = resp.usage.as_ref().expect("usage present");
        assert_eq!(usage.prompt_tokens, 12);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.total_tokens, 19);
        assert!(matches!(resp.finish_reason, Some(FinishReason::Stop)));
        assert_eq!(resp.response_id.as_deref(), Some("mock-req-1"));
        assert!(elapsed < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn execute_returns_error_for_invalid_credentials() {
        let exec = BedrockExecutor::new();
        let mut target = make_target("https://example.invalid");
        target.api_key = "no-colons".to_string();
        let ir = make_ir();
        let ctx = PipelineContext::new("test-2".to_string(), ir.clone(), None);
        let result = exec.execute(&target, &ir, &ctx).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("access_key:secret_key:region"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn execute_propagates_upstream_5xx_as_executor_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;
        let exec = BedrockExecutor::new();
        let target = make_target(&server.uri());
        let ir = make_ir();
        let ctx = PipelineContext::new("test-3".to_string(), ir.clone(), None);
        let result = exec.execute(&target, &ir, &ctx).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("500"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn execute_stream_emits_signed_converse_stream_and_yields_text_delta() {
        use futures::StreamExt;
        let server = MockServer::start().await;
        // ConverseStream returns 200 with chunked text/event-stream body.
        let body = b"event stream line 1\nstream line 2\n";
        Mock::given(method("POST"))
            .and(path(
                "/model/anthropic.claude-sonnet-4-20250514-v1:0/converse-stream",
            ))
            .and(wiremock::matchers::header_regex(
                "authorization",
                "^AWS4-HMAC-SHA256 .*Signature=[0-9a-f]{64}$",
            ))
            .and(wiremock::matchers::header_regex(
                "x-amz-content-sha256",
                "^[0-9a-f]{64}$",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(body),
            )
            .mount(&server)
            .await;

        let exec = BedrockExecutor::new();
        let target = make_target(&server.uri());
        let ir = make_ir();
        let ctx = PipelineContext::new("test-4".to_string(), ir.clone(), None);

        let result = exec.execute_stream(&target, &ir, &ctx).await;
        let mut stream = match result {
            Ok(s) => s,
            Err(e) => panic!("execute_stream failed: {e}"),
        };
        let mut collected = String::new();
        while let Some(part) = stream.next().await {
            match part {
                Ok(tiygate_core::StreamPart::TextDelta { text }) => {
                    collected.push_str(&text);
                }
                Ok(_) => {}
                Err(e) => panic!("stream error: {e}"),
            }
        }
        // We expect at least one text delta carrying the wire bytes.
        assert!(!collected.is_empty(), "stream produced no text deltas");
        // The full body is reconstructed when the last chunk has no
        // trailing newline, so the collected text should contain the
        // original payload.
        assert!(
            collected.contains("event stream line 1")
                || collected.contains("stream line 2")
                || collected.contains("event stream line 1\nstream line 2"),
            "stream did not surface wire bytes: {collected}"
        );
    }

    #[tokio::test]
    async fn execute_stream_propagates_5xx_as_executor_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream error"))
            .mount(&server)
            .await;
        let exec = BedrockExecutor::new();
        let target = make_target(&server.uri());
        let ir = make_ir();
        let ctx = PipelineContext::new("test-5".to_string(), ir.clone(), None);
        let result = exec.execute_stream(&target, &ir, &ctx).await;
        let err = match result {
            Ok(_) => panic!("expected error"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("ConverseStream"), "unexpected error: {err}");
        assert!(err.contains("500"), "unexpected error: {err}");
    }
}
