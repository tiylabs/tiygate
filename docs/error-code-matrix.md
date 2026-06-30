# Error Code Matrix

This document describes how TiyGate normalizes upstream and gateway-internal errors into protocol-native error responses.

## Design Principle

**Clients see the same error format they would receive when talking directly to the upstream provider.** The gateway is transparent — standard OpenAI/Anthropic/Gemini SDKs see the expected `error.type`/`error.status` and HTTP status code without needing any gateway-specific knowledge.

## ErrorClass Enum

`ErrorClass` (`crates/core/src/routing/mod.rs`) is the canonical intermediate representation between opaque upstream error codes and protocol-native `error.type`/`error.status` fields.

| Variant | Retry? | HTTP Status | Description |
|---|---|---|---|
| `Transient` | Yes | 502 | Upstream 5xx, timeout, transport error |
| `RateLimited` | Yes (backoff) | 429 | Upstream rate limit |
| `Auth` | Transfer | 401 | Upstream authentication error |
| `BadRequest` | No | 400 | Malformed request rejected by upstream |
| `LossyOrCapability` | No | 400 | Protocol conversion lossy or unsupported |
| `ModelNotFound` | No | 404 | No route found for virtual model |
| `DeadlineExceeded` | Yes | 504 | Gateway deadline exceeded |
| `UpstreamExhausted` | No | 502 | All upstream targets exhausted |
| `AuthMissing` | No | 401 | Inbound API key missing |
| `AuthInvalid` | No | 401 | Inbound API key invalid |
| `AuthDisabled` | No | 403 | Inbound API key disabled |
| `Overloaded` | Yes (backoff) | 503 | Gateway overloaded |

## ErrorClass → Protocol-Native Type Mapping

### OpenAI (ChatCompletions / Responses / Embeddings / Images)

| ErrorClass | `error.type` | HTTP Status |
|---|---|---|
| Transient | `server_error` | 502 |
| RateLimited | `rate_limit_error` | 429 |
| Auth | `authentication_error` | 401 |
| BadRequest | `invalid_request_error` | 400 |
| LossyOrCapability | `invalid_request_error` | 400 |
| ModelNotFound | `not_found_error` | 404 |
| DeadlineExceeded | `server_error` | 504 |
| UpstreamExhausted | `server_error` | 502 |
| AuthMissing | `authentication_error` | 401 |
| AuthInvalid | `authentication_error` | 401 |
| AuthDisabled | `permission_error` | 403 |
| Overloaded | `overloaded_error` | 503 |

JSON body format: `{"error":{"message":"...","type":"...","param":null,"code":"..."}}`

### Anthropic Messages

| ErrorClass | `error.type` | HTTP Status |
|---|---|---|
| Transient | `api_error` | 500 |
| RateLimited | `rate_limit_error` | 429 |
| Auth | `authentication_error` | 401 |
| BadRequest | `invalid_request_error` | 400 |
| LossyOrCapability | `invalid_request_error` | 400 |
| ModelNotFound | `not_found_error` | 404 |
| DeadlineExceeded | `timeout_error` | 504 |
| UpstreamExhausted | `overloaded_error` | 529 |
| AuthMissing | `authentication_error` | 401 |
| AuthInvalid | `authentication_error` | 401 |
| AuthDisabled | `permission_error` | 403 |
| Overloaded | `overloaded_error` | 529 |

JSON body format: `{"type":"error","error":{"type":"...","message":"..."}}`

### Google Gemini

| ErrorClass | `error.status` | HTTP Status |
|---|---|---|
| Transient | `INTERNAL` | 500 |
| RateLimited | `RESOURCE_EXHAUSTED` | 429 |
| Auth | `UNAUTHENTICATED` | 401 |
| BadRequest | `INVALID_ARGUMENT` | 400 |
| LossyOrCapability | `FAILED_PRECONDITION` | 400 |
| ModelNotFound | `NOT_FOUND` | 404 |
| DeadlineExceeded | `DEADLINE_EXCEEDED` | 504 |
| UpstreamExhausted | `UNAVAILABLE` | 503 |
| AuthMissing | `UNAUTHENTICATED` | 401 |
| AuthInvalid | `UNAUTHENTICATED` | 401 |
| AuthDisabled | `PERMISSION_DENIED` | 403 |
| Overloaded | `UNAVAILABLE` | 503 |

JSON body format: `{"error":{"code":<http_status>,"message":"...","status":"...","details":[]}}`

## Upstream Error Normalization

`classify_upstream_error(upstream_status: Option<u16>, upstream_code: Option<&str>) -> ErrorClass` in `crates/core/src/routing/mod.rs` is the single source of truth for upstream error normalization.

Classification priority:
1. **HTTP status exact match**: 429→RateLimited, 401/403→Auth, 400/422→BadRequest, 404→ModelNotFound, 504→DeadlineExceeded, ≥500→Transient
2. **Error code substring match** (case-insensitive): `rate_limit`/`quota`/`429`/`resource_exhausted`→RateLimited, `auth`/`invalid_api_key`/`permission`/`unauthenticated`→Auth, `bad_request`/`invalid_argument`/`invalid_request`→BadRequest, `overloaded`/`service_unavailable`/`529`/`503`/`unavailable`→Overloaded, `timeout`/`deadline`→DeadlineExceeded, `not_found`/`model_not_found`/`404`→ModelNotFound
3. **Default**: Transient

## Architecture

```
Upstream error → classify_upstream_error() → ErrorClass → protocol mapping → native error.type/status
Gateway error  → directly set ErrorClass    → ErrorClass → protocol mapping → native error.type/status
```

- `AppError` carries `error_class: ErrorClass` and `protocol_suite: Option<ProtocolSuite>`.
- `AppError::into_response()` calls `encode_error_body_for_suite()` to generate the protocol-native body.
- `StreamPart::Error` carries `class: ErrorClass` and `upstream_code: Option<String>`.
- `StreamEncoder::encode_error()` and `EndpointCodec::encode_error_body()` use the ErrorClass → type/status mapping.
- `UpstreamStreamError` carries `class: ErrorClass` for telemetry classification.

## Related Documentation

- [Protocol Capability Matrix](protocol-capability-matrix.md) — documents lossy protocol translation behavior.
