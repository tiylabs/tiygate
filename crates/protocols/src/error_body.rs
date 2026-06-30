//! Protocol-native error body generation without a codec instance.
//!
//! Provides a standalone function that maps `(ProtocolSuite, ErrorClass)`
//! to the protocol-native error JSON body. This is used by the HTTP error
//! response path (`AppError::into_response`) where a codec instance is not
//! available but the `ProtocolSuite` is known from the ingress endpoint.
//!
//! The mapping tables here mirror the `error_type_for_class` / `error_status_for_class`
//! functions in each protocol codec module. They are the single source of truth
//! for the non-streaming error body format.

use serde_json::{json, Value};
use tiygate_core::{ErrorClass, ProtocolSuite};

/// Map an `ErrorClass` to the OpenAI-native `error.type` string.
fn openai_error_type(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Transient => "server_error",
        ErrorClass::RateLimited => "rate_limit_error",
        ErrorClass::Auth => "authentication_error",
        ErrorClass::BadRequest => "invalid_request_error",
        ErrorClass::LossyOrCapability => "invalid_request_error",
        ErrorClass::ModelNotFound => "not_found_error",
        ErrorClass::DeadlineExceeded => "server_error",
        ErrorClass::UpstreamExhausted => "server_error",
        ErrorClass::AuthMissing => "authentication_error",
        ErrorClass::AuthInvalid => "authentication_error",
        ErrorClass::AuthDisabled => "permission_error",
        ErrorClass::Overloaded => "overloaded_error",
    }
}

/// Map an `ErrorClass` to the Anthropic-native `error.type` string.
fn anthropic_error_type(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Transient => "api_error",
        ErrorClass::RateLimited => "rate_limit_error",
        ErrorClass::Auth => "authentication_error",
        ErrorClass::BadRequest => "invalid_request_error",
        ErrorClass::LossyOrCapability => "invalid_request_error",
        ErrorClass::ModelNotFound => "not_found_error",
        ErrorClass::DeadlineExceeded => "timeout_error",
        ErrorClass::UpstreamExhausted => "overloaded_error",
        ErrorClass::AuthMissing => "authentication_error",
        ErrorClass::AuthInvalid => "authentication_error",
        ErrorClass::AuthDisabled => "permission_error",
        ErrorClass::Overloaded => "overloaded_error",
    }
}

/// Map an `ErrorClass` to the Gemini-native `error.status` string.
fn gemini_error_status(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Transient => "INTERNAL",
        ErrorClass::RateLimited => "RESOURCE_EXHAUSTED",
        ErrorClass::Auth => "UNAUTHENTICATED",
        ErrorClass::BadRequest => "INVALID_ARGUMENT",
        ErrorClass::LossyOrCapability => "FAILED_PRECONDITION",
        ErrorClass::ModelNotFound => "NOT_FOUND",
        ErrorClass::DeadlineExceeded => "DEADLINE_EXCEEDED",
        ErrorClass::UpstreamExhausted => "UNAVAILABLE",
        ErrorClass::AuthMissing => "UNAUTHENTICATED",
        ErrorClass::AuthInvalid => "UNAUTHENTICATED",
        ErrorClass::AuthDisabled => "PERMISSION_DENIED",
        ErrorClass::Overloaded => "UNAVAILABLE",
    }
}

/// Generate a protocol-native error JSON body for the given suite.
///
/// This function produces the same JSON structure that the corresponding
/// codec's `encode_error_body` method would produce, but without requiring
/// a codec instance. It is used by the HTTP error response path where only
/// the `ProtocolSuite` is known.
///
/// - **OpenAI** (ChatCompletions / Responses / Embeddings / Images):
///   `{"error":{"message":"...","type":"...","param":null,"code":"..."}}`
/// - **Anthropic**: `{"type":"error","error":{"type":"...","message":"..."}}`
/// - **Gemini**: `{"error":{"code":<http_status>,"message":"...","status":"...","details":[]}}`
pub fn encode_error_body_for_suite(
    suite: ProtocolSuite,
    message: &str,
    class: ErrorClass,
    http_status: u16,
    upstream_code: Option<&str>,
) -> Value {
    match suite {
        ProtocolSuite::OpenAiCompatible | ProtocolSuite::OpenAiResponses => {
            let mut err = json!({
                "message": message,
                "type": openai_error_type(class),
                "param": null,
            });
            if let Some(c) = upstream_code {
                err["code"] = json!(c);
            }
            json!({"error": err})
        }
        ProtocolSuite::AnthropicMessages => {
            json!({
                "type": "error",
                "error": {
                    "type": anthropic_error_type(class),
                    "message": message,
                }
            })
        }
        ProtocolSuite::GoogleGemini => {
            json!({
                "error": {
                    "code": http_status,
                    "message": message,
                    "status": gemini_error_status(class),
                    "details": []
                }
            })
        }
    }
}
