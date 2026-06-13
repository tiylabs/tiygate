//! OLTP sink — writes completed `RequestEvent`s to the
//! `request_logs` table. Pipeline events are dropped (they are
//! in-flight lifecycle markers; the aggregated `RequestEvent` is
//! what Phase 4 stores for analysis).
//!
//! ## Aggregation
//!
//! The sink is the source of truth for the dashboard. It accepts
//! the same [`RequestEvent`] as the legacy stdout sink; the
//! conversion to the row layout is a single straight-line function.
//!
//! ## Performance
//!
//! SQLite is single-writer by default; with `journal_mode=WAL` and
//! a per-request `INSERT`, the gateway is bounded by the disk
//! latency. Phase 4 keeps the simple single-row path; Phase 5 may
//! introduce a batching layer.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::Row;
use tracing::warn;

use tiygate_core::ir::Usage;
use tiygate_core::redaction::Redactor;
use tiygate_core::{EventSink, ExchangeCapture, PipelineEvent, RequestEvent};

use crate::db::DbPool;

/// Default payload body byte cap when none is supplied. Mirrors the
/// server's `raw_envelope_max_bytes` default (256 KiB). Bodies larger
/// than this are truncated and flagged.
const DEFAULT_PAYLOAD_MAX_BYTES: usize = 256 * 1024;

/// An `EventSink` backed by the `request_logs` table.
pub struct OltpSink {
    pool: Arc<DbPool>,
    /// Redactor applied to captured headers + JSON bodies on the
    /// background telemetry task before persistence. Defaults to the
    /// standard credential set.
    redactor: Redactor,
    /// Byte cap for each captured body before truncation.
    payload_max_bytes: usize,
}

impl OltpSink {
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self {
            pool,
            redactor: Redactor::with_defaults(),
            payload_max_bytes: DEFAULT_PAYLOAD_MAX_BYTES,
        }
    }

    /// Override the per-body byte cap used when persisting captured
    /// payloads. Keep this aligned with the server's
    /// `raw_envelope_max_bytes` so detail-view bodies and the request
    /// envelope share the same truncation budget.
    pub fn with_payload_max_bytes(mut self, max: usize) -> Self {
        self.payload_max_bytes = max;
        self
    }
}

#[async_trait]
impl EventSink for OltpSink {
    async fn write_event(&self, _event: &PipelineEvent) -> Result<(), tiygate_core::Error> {
        // Pipeline events are lifecycle markers — we only persist
        // the aggregated `RequestEvent` from the request hot path.
        // Silently dropping pipeline events here keeps the OLTP
        // table focused on analysis.
        Ok(())
    }

    async fn write_request_event(&self, event: &RequestEvent) -> Result<(), tiygate_core::Error> {
        let row = request_event_to_row(event);
        // Use an upsert that preserves the token/cost columns written
        // by `update_request_tokens` (capture stage) when this
        // `RequestEvent` arrives after the capture. The hot path always
        // emits `tokens: None`, so on a normal hot-path-only insert we
        // fall through to the `excluded` (NULL) values which the
        // COALESCE bridges back to the existing row. On a fresh insert
        // (`update_request_tokens` did not run first) the row did not
        // exist, so `request_logs.<col>` is NULL and we accept the
        // `excluded` value.
        let res = sqlx::query(
            "INSERT INTO request_logs (\
                request_id, ts, virtual_model, resolved_provider, resolved_model, account_label, \
                trace_id, span_id, traceparent, ingress_protocol, egress_protocol, \
                lossy, cache_hit, status, error_class, http_status, error_source, \
                total_latency_ms, upstream_latency_ms, queue_latency_ms, ttfb_ms, \
                prompt_tokens, completion_tokens, reasoning_tokens, cache_read_tokens, \
                cache_write_tokens, total_tokens, cost, api_key_id, client_ip, user_agent, \
                raw_envelope_json, redacted_headers_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
                     ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33) \
             ON CONFLICT(request_id) DO UPDATE SET \
                ts = excluded.ts, \
                virtual_model = excluded.virtual_model, \
                resolved_provider = excluded.resolved_provider, \
                resolved_model = excluded.resolved_model, \
                account_label = excluded.account_label, \
                trace_id = excluded.trace_id, \
                span_id = excluded.span_id, \
                traceparent = excluded.traceparent, \
                ingress_protocol = excluded.ingress_protocol, \
                egress_protocol = excluded.egress_protocol, \
                lossy = excluded.lossy, \
                cache_hit = excluded.cache_hit, \
                status = excluded.status, \
                error_class = excluded.error_class, \
                http_status = excluded.http_status, \
                error_source = excluded.error_source, \
                total_latency_ms = excluded.total_latency_ms, \
                upstream_latency_ms = excluded.upstream_latency_ms, \
                queue_latency_ms = excluded.queue_latency_ms, \
                ttfb_ms = excluded.ttfb_ms, \
                prompt_tokens = COALESCE(excluded.prompt_tokens, request_logs.prompt_tokens), \
                completion_tokens = COALESCE(excluded.completion_tokens, request_logs.completion_tokens), \
                reasoning_tokens = COALESCE(excluded.reasoning_tokens, request_logs.reasoning_tokens), \
                cache_read_tokens = COALESCE(excluded.cache_read_tokens, request_logs.cache_read_tokens), \
                cache_write_tokens = COALESCE(excluded.cache_write_tokens, request_logs.cache_write_tokens), \
                total_tokens = COALESCE(excluded.total_tokens, request_logs.total_tokens), \
                cost = COALESCE(excluded.cost, request_logs.cost), \
                api_key_id = excluded.api_key_id, \
                client_ip = excluded.client_ip, \
                user_agent = excluded.user_agent, \
                raw_envelope_json = excluded.raw_envelope_json, \
                redacted_headers_json = excluded.redacted_headers_json",
        )
        .bind(&row.request_id)
        .bind(&row.ts)
        .bind(&row.virtual_model)
        .bind(&row.resolved_provider)
        .bind(&row.resolved_model)
        .bind(&row.account_label)
        .bind(&row.trace_id)
        .bind(&row.span_id)
        .bind(&row.traceparent)
        .bind(&row.ingress_protocol)
        .bind(&row.egress_protocol)
        .bind(row.lossy as i32)
        .bind(&row.cache_hit)
        .bind(&row.status)
        .bind(&row.error_class)
        .bind(row.http_status.map(|n| n as i32))
        .bind(&row.error_source)
        .bind(row.total_latency_ms as i64)
        .bind(row.upstream_latency_ms as i64)
        .bind(row.queue_latency_ms as i64)
        .bind(row.ttfb_ms.map(|n| n as i64))
        .bind(row.prompt_tokens.map(|n| n as i64))
        .bind(row.completion_tokens.map(|n| n as i64))
        .bind(row.reasoning_tokens.map(|n| n as i64))
        .bind(row.cache_read_tokens.map(|n| n as i64))
        .bind(row.cache_write_tokens.map(|n| n as i64))
        .bind(row.total_tokens.map(|n| n as i64))
        .bind(row.cost.map(|n| n as i64))
        .bind(&row.api_key_id)
        .bind(&row.client_ip)
        .bind(&row.user_agent)
        .bind(&row.raw_envelope_json)
        .bind(&row.redacted_headers_json)
        .execute(self.pool.sqlite())
        .await;
        if let Err(e) = res {
            warn!(error = %e, request_id = %event.request_id, "oltp sink: insert failed");
            return Err(tiygate_core::Error::Telemetry(format!("oltp insert: {e}")));
        }
        Ok(())
    }

    async fn write_capture(&self, capture: &ExchangeCapture) -> Result<(), tiygate_core::Error> {
        let row = self.capture_to_row(capture);
        let res = sqlx::query(
            "INSERT OR REPLACE INTO request_payloads (\
                request_id, egress_method, egress_path, \
                egress_headers_json, egress_body, egress_body_truncated, \
                upstream_status, upstream_resp_headers_json, upstream_resp_body, \
                upstream_resp_body_truncated, client_resp_headers_json, client_resp_body, \
                client_resp_body_truncated, is_stream, sse_parsed_json, \
                client_sse_parsed_json, captured_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        )
        .bind(&row.request_id)
        .bind(&row.egress_method)
        .bind(&row.egress_path)
        .bind(&row.egress_headers_json)
        .bind(&row.egress_body)
        .bind(row.egress_body_truncated as i32)
        .bind(row.upstream_status.map(|n| n as i32))
        .bind(&row.upstream_resp_headers_json)
        .bind(&row.upstream_resp_body)
        .bind(row.upstream_resp_body_truncated as i32)
        .bind(&row.client_resp_headers_json)
        .bind(&row.client_resp_body)
        .bind(row.client_resp_body_truncated as i32)
        .bind(row.is_stream as i32)
        .bind(&row.sse_parsed_json)
        .bind(&row.client_sse_parsed_json)
        .bind(&row.captured_at)
        .execute(self.pool.sqlite())
        .await;
        if let Err(e) = res {
            warn!(error = %e, request_id = %capture.request_id, "oltp sink: payload insert failed");
            return Err(tiygate_core::Error::Telemetry(format!(
                "oltp payload insert: {e}"
            )));
        }

        // Write real upstream token usage back onto the request_logs
        // row. The request hot path always emits `tokens: None` (the
        // streaming path only estimates from chars and never feeds it
        // back), so this background capture is the single point where
        // accurate usage — covering every protocol, stream and
        // non-stream — is recovered and persisted. A missing row
        // (capture racing ahead of the RequestEvent insert) is a
        // silent no-op.
        if let Some(usage) = extract_usage_from_capture(capture) {
            if let Err(e) = self.update_request_tokens(&capture.request_id, &usage).await {
                warn!(
                    error = %e,
                    request_id = %capture.request_id,
                    "oltp sink: token write-back failed"
                );
            }
        }
        Ok(())
    }

    async fn flush(&self) -> Result<(), tiygate_core::Error> {
        Ok(())
    }
}

/// Mirror of a `request_payloads` row after redaction + truncation.
#[derive(Debug, Default)]
struct RequestPayloadsRow {
    request_id: String,
    egress_method: String,
    egress_path: String,
    egress_headers_json: Option<String>,
    egress_body: Option<String>,
    egress_body_truncated: bool,
    upstream_status: Option<u16>,
    upstream_resp_headers_json: Option<String>,
    upstream_resp_body: Option<String>,
    upstream_resp_body_truncated: bool,
    client_resp_headers_json: Option<String>,
    client_resp_body: Option<String>,
    client_resp_body_truncated: bool,
    is_stream: bool,
    sse_parsed_json: Option<String>,
    client_sse_parsed_json: Option<String>,
    captured_at: String,
}

impl OltpSink {
    /// Convert a raw `ExchangeCapture` into a persisted row, applying
    /// header + JSON-body redaction, byte-cap truncation, and (for
    /// streaming responses) best-effort SSE merge parsing. This runs
    /// on the telemetry background task, never on the request hot
    /// path.
    fn capture_to_row(&self, capture: &ExchangeCapture) -> RequestPayloadsRow {
        let egress_headers_json = redact_headers_json(&self.redactor, &capture.egress_headers);
        let upstream_resp_headers_json =
            redact_headers_json(&self.redactor, &capture.upstream_resp_headers);
        let client_resp_headers_json =
            redact_headers_json(&self.redactor, &capture.client_resp_headers);

        let (egress_body, egress_body_truncated) =
            self.prepare_body(capture.egress_body.as_deref());
        let (upstream_resp_body, upstream_resp_body_truncated) =
            self.prepare_body(capture.upstream_resp_body.as_deref());
        let (client_resp_body, client_resp_body_truncated) =
            self.prepare_body(capture.client_resp_body.as_deref());

        // For streaming responses, attempt to merge the SSE chunks
        // (we parse from the *upstream* body which carries the raw
        // SSE stream) into a structured JSON result for easier
        // reading. Best-effort: failures leave the field None.
        let sse_parsed_json = if capture.is_stream {
            capture
                .upstream_resp_body
                .as_deref()
                .and_then(parse_sse_to_json)
        } else {
            None
        };

        // Same best-effort merge for the Gateway -> Client (g->c)
        // response direction. For same-protocol streaming the client
        // body is byte-identical to the upstream SSE; once cross-
        // protocol re-encoding lands the two will diverge, so we parse
        // and store the client side independently.
        let client_sse_parsed_json = if capture.is_stream {
            capture
                .client_resp_body
                .as_deref()
                .and_then(parse_sse_to_json)
        } else {
            None
        };

        RequestPayloadsRow {
            request_id: capture.request_id.clone(),
            egress_method: capture.egress_method.clone(),
            egress_path: capture.egress_path.clone(),
            egress_headers_json,
            egress_body,
            egress_body_truncated,
            upstream_status: capture.upstream_status,
            upstream_resp_headers_json,
            upstream_resp_body,
            upstream_resp_body_truncated,
            client_resp_headers_json,
            client_resp_body,
            client_resp_body_truncated,
            is_stream: capture.is_stream,
            sse_parsed_json,
            client_sse_parsed_json,
            captured_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Persist the recovered token usage onto the `request_logs` row,
    /// keyed by `request_id`. Runs on the telemetry background task
    /// after a capture is persisted.
    ///
    /// Capture and the `RequestEvent` insert are dispatched over the
    /// same channel and may interleave: a capture whose `write_capture`
    /// runs before `write_request_event` would otherwise UPDATE a
    /// row that does not exist yet (rows_affected = 0) and the
    /// subsequent `INSERT OR REPLACE` from `write_request_event`
    /// would re-create the row with `token` columns reset to NULL.
    /// To make the writeback order-independent we use an upsert that
    /// inserts a minimal placeholder when the row is missing and
    /// updates only the token columns when it is already present —
    /// the later `INSERT OR REPLACE` from `write_request_event` is
    /// itself rewritten to `INSERT ... ON CONFLICT DO UPDATE` so it
    /// does not clobber the token columns.
    async fn update_request_tokens(
        &self,
        request_id: &str,
        usage: &Usage,
    ) -> Result<(), sqlx::Error> {
        let now = chrono::Utc::now().to_rfc3339();
        // Placeholder values for the NOT-NULL columns when we have to
        // insert a row that `write_request_event` has not yet
        // produced. `write_request_event`'s later
        // `ON CONFLICT DO UPDATE` will overwrite every column except
        // the token group and `cost`, preserving the usage we
        // recovered here.
        sqlx::query(
            "INSERT INTO request_logs (\
                request_id, ts, virtual_model, ingress_protocol, status, \
                total_latency_ms, upstream_latency_ms, queue_latency_ms, lossy, \
                prompt_tokens, completion_tokens, reasoning_tokens, \
                cache_read_tokens, cache_write_tokens, total_tokens) \
             VALUES (?1, ?2, '', '', 'pending', 0, 0, 0, 0, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(request_id) DO UPDATE SET \
                prompt_tokens = excluded.prompt_tokens, \
                completion_tokens = excluded.completion_tokens, \
                reasoning_tokens = excluded.reasoning_tokens, \
                cache_read_tokens = excluded.cache_read_tokens, \
                cache_write_tokens = excluded.cache_write_tokens, \
                total_tokens = excluded.total_tokens",
        )
        .bind(request_id)
        .bind(now)
        .bind(usage.prompt_tokens as i64)
        .bind(usage.completion_tokens as i64)
        .bind(usage.reasoning_tokens.map(|n| n as i64))
        .bind(usage.cache_read_tokens.map(|n| n as i64))
        .bind(usage.cache_write_tokens.map(|n| n as i64))
        .bind(usage.total_tokens as i64)
        .execute(self.pool.sqlite())
        .await?;
        Ok(())
    }

    /// Redact a JSON body string (best-effort) and apply byte-cap
    /// truncation. Returns `(stored_body, truncated)`.
    fn prepare_body(&self, body: Option<&str>) -> (Option<String>, bool) {
        let Some(raw) = body else {
            return (None, false);
        };
        // Redact known credential keys when the body is valid JSON;
        // otherwise keep the raw text (e.g. SSE streams, error pages).
        let redacted = match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(mut value) => {
                self.redactor.redact_value(&mut value);
                serde_json::to_string(&value).unwrap_or_else(|_| raw.to_string())
            }
            Err(_) => raw.to_string(),
        };
        if redacted.len() > self.payload_max_bytes {
            let mut truncated = redacted;
            truncated.truncate(self.payload_max_bytes);
            (Some(truncated), true)
        } else {
            (Some(redacted), false)
        }
    }
}

/// Redact a header list and serialize it to a JSON object string.
/// Returns `None` only when serialization fails (never expected).
fn redact_headers_json(redactor: &Redactor, headers: &[(String, String)]) -> Option<String> {
    if headers.is_empty() {
        return None;
    }
    let redacted = redactor.redact_headers(headers.iter().cloned());
    let map: std::collections::BTreeMap<String, String> = redacted.into_iter().collect();
    serde_json::to_string(&map).ok()
}

/// Best-effort merge of an SSE stream into a single structured JSON
/// string. Parses `data:` lines, decodes each as JSON, detects the
/// protocol family, and runs the corresponding merge routine:
///
///   * OpenAI `chat.completion.chunk` — concatenates
///     `choices[].delta.content` into a single assistant message and
///     carries the final `usage` when present.
///   * OpenAI Responses — concatenates `response.output_text.delta`
///     payloads, picks up `response.created.response.model`, and
///     carries the last `response.completed.response.usage`. Maps the
///     terminal `response.completed.response.status` to a normalized
///     `finish_reason`.
///   * Anthropic Messages — concatenates `content_block_delta` text
///     deltas and carries the `message_delta` usage / stop reason.
///   * Google Gemini — concatenates `candidates[].content.parts[].text`
///     and `parts[].thought` separately, counts `parts[].functionCall`
///     tool calls, carries `usageMetadata` and the last
///     `candidates[].finishReason`.
///
/// Returns `None` when no `data:` JSON lines are found. Falls back to
/// a `protocol: "unknown"` envelope carrying the raw event array when
/// no family is recognized, so the detail view can still show the raw
/// stream.
pub fn parse_sse_to_json(raw: &str) -> Option<String> {
    let events = parse_data_lines(raw);
    if events.is_empty() {
        return None;
    }
    let family = detect_family(&events);
    let event_count = events.len();
    let merged = match family {
        Family::OpenAiChat => merge_openai_chat(&events),
        Family::OpenAiResponses => merge_openai_responses(&events),
        Family::Anthropic => merge_anthropic(&events),
        Family::Gemini => merge_gemini(&events),
        Family::Unknown => {
            let obj = serde_json::json!({
                "protocol": "unknown",
                "events": events,
                "event_count": event_count,
            });
            return serde_json::to_string_pretty(&obj).ok();
        }
    };
    let view = build_view(merged, event_count);
    serde_json::to_string_pretty(&view).ok()
}

/// Parse every `data:` line out of an SSE buffer and decode it as
/// JSON. Lines that do not start with `data:`, are empty, equal
/// `[DONE]`, or fail to parse as JSON are silently skipped — this
/// matches the lenient behavior the prior implementation had and
/// keeps TCP packet boundary handling consistent with the
/// `split_sse_lines` helper in `ingress.rs`.
fn parse_data_lines(raw: &str) -> Vec<serde_json::Value> {
    let mut events: Vec<serde_json::Value> = Vec::new();
    for line in raw.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
            events.push(v);
        }
    }
    events
}

/// Protocol family. Adding a new SSE family means adding a variant
/// here, a `detect_family` arm, and a merge fn — `parse_sse_to_json`
/// then dispatches to it automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    OpenAiChat,
    OpenAiResponses,
    Anthropic,
    Gemini,
    Unknown,
}

/// Decide which protocol family produced this SSE stream. Order
/// matters: `OpenAiChat` is checked first because the OpenAI-compatible
/// family (DeepSeek / Moonshot / Zhipu) is the most common, then
/// `OpenAiResponses` before `Anthropic` because both carry a top-level
/// `type` field and the only thing that disambiguates them is the
/// `response.` / `message_` / `content_block_` prefix. Gemini uses
/// its own keys (`candidates` / `usageMetadata`) so it can never be
/// confused with the `type`-based families.
fn detect_family(events: &[serde_json::Value]) -> Family {
    for ev in events {
        // OpenAI Chat Completions (and OpenAI-compatible providers
        // that reuse the same envelope). `object == "chat.completion.chunk"`
        // is the canonical marker; some providers omit it but still
        // emit a top-level `choices` array — that is also accepted.
        if ev.get("object").and_then(|o| o.as_str()) == Some("chat.completion.chunk")
            || ev.get("choices").is_some()
        {
            return Family::OpenAiChat;
        }
        // OpenAI Responses: `type` is namespaced under `response.*`.
        if let Some(ty) = ev.get("type").and_then(|t| t.as_str()) {
            if ty.starts_with("response.") {
                return Family::OpenAiResponses;
            }
            if ty.starts_with("message_") || ty.starts_with("content_block_") {
                return Family::Anthropic;
            }
        }
        // Gemini: no `type` field, but a `candidates` or `usageMetadata`
        // block is present.
        if ev.get("candidates").is_some() || ev.get("usageMetadata").is_some() {
            return Family::Gemini;
        }
    }
    Family::Unknown
}

/// Canonical merged view produced by every per-family merge fn.
/// `tool_calls` and `reasoning` default to empty/zero so a family that
/// does not have a concept (e.g. OpenAI Chat has no `thought` deltas)
/// can simply leave them unset and `build_view` will omit them.
#[derive(Debug, Default)]
struct Merged {
    protocol: &'static str,
    model: Option<String>,
    text: String,
    reasoning: String,
    finish_reason: Option<String>,
    usage: Option<serde_json::Value>,
    tool_calls: usize,
}

fn merge_openai_chat(events: &[serde_json::Value]) -> Merged {
    let mut m = Merged {
        protocol: "openai",
        ..Default::default()
    };
    for ev in events {
        if m.model.is_none() {
            m.model = ev
                .get("model")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
        if let Some(choices) = ev.get("choices").and_then(|c| c.as_array()) {
            for ch in choices {
                if let Some(c) = ch
                    .get("delta")
                    .and_then(|d| d.get("content"))
                    .and_then(|c| c.as_str())
                {
                    m.text.push_str(c);
                }
                if let Some(fr) = ch.get("finish_reason").and_then(|f| f.as_str()) {
                    m.finish_reason = Some(fr.to_string());
                }
            }
        }
        if let Some(u) = ev.get("usage") {
            if !u.is_null() {
                m.usage = Some(u.clone());
            }
        }
    }
    m
}

fn merge_anthropic(events: &[serde_json::Value]) -> Merged {
    let mut m = Merged {
        protocol: "anthropic",
        ..Default::default()
    };
    for ev in events {
        let Some(ty) = ev.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        match ty {
            "content_block_delta" => {
                if let Some(t) = ev
                    .get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(|t| t.as_str())
                {
                    m.text.push_str(t);
                }
            }
            "message_start" => {
                if m.model.is_none() {
                    m.model = ev
                        .get("message")
                        .and_then(|m| m.get("model"))
                        .and_then(|m| m.as_str())
                        .map(|s| s.to_string());
                }
            }
            "message_delta" => {
                if let Some(u) = ev.get("usage") {
                    if !u.is_null() {
                        m.usage = Some(u.clone());
                    }
                }
                if let Some(sr) = ev
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                {
                    m.finish_reason = Some(sr.to_string());
                }
            }
            _ => {}
        }
    }
    m
}

fn merge_openai_responses(events: &[serde_json::Value]) -> Merged {
    let mut m = Merged {
        protocol: "openai_responses",
        ..Default::default()
    };
    for ev in events {
        let Some(ty) = ev.get("type").and_then(|t| t.as_str()) else {
            continue;
        };
        let response = ev.get("response");
        match ty {
            "response.output_text.delta" => {
                // The delta can be an empty string on the very first
                // frame; the OpenAI Responses stream emits a no-op
                // delta to establish the item context. Always run the
                // push (no-op for empty string) so behavior matches
                // the upstream wire format.
                if let Some(d) = ev.get("delta").and_then(|d| d.as_str()) {
                    m.text.push_str(d);
                }
            }
            "response.created" => {
                if m.model.is_none() {
                    m.model = response
                        .and_then(|r| r.get("model"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                }
            }
            "response.completed" => {
                // Usage is overwritten on every terminal frame so the
                // final value wins (the OpenAI Responses stream emits
                // one `response.completed` per response, and any
                // intermediate `response.incomplete` carries the most
                // accurate counts).
                if let Some(u) = response.and_then(|r| r.get("usage")) {
                    if !u.is_null() {
                        m.usage = Some(u.clone());
                    }
                }
                if let Some(status) =
                    response.and_then(|r| r.get("status")).and_then(|s| s.as_str())
                {
                    m.finish_reason = Some(match status {
                        "completed" => "stop".to_string(),
                        "incomplete" => "length".to_string(),
                        "failed" => "error".to_string(),
                        other => other.to_string(),
                    });
                }
            }
            "response.function_call_arguments.done" => {
                m.tool_calls += 1;
            }
            _ => {}
        }
    }
    m
}

fn merge_gemini(events: &[serde_json::Value]) -> Merged {
    let mut m = Merged {
        protocol: "gemini",
        ..Default::default()
    };
    for ev in events {
        if m.model.is_none() {
            m.model = ev
                .get("modelVersion")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
        if let Some(candidates) = ev.get("candidates").and_then(|c| c.as_array()) {
            for c in candidates {
                if let Some(parts) = c
                    .get("content")
                    .and_then(|co| co.get("parts"))
                    .and_then(|p| p.as_array())
                {
                    for part in parts {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            m.text.push_str(t);
                        }
                        if let Some(t) = part.get("thought").and_then(|t| t.as_str()) {
                            m.reasoning.push_str(t);
                        }
                        if part.get("functionCall").is_some() {
                            m.tool_calls += 1;
                        }
                    }
                }
                if let Some(fr) = c.get("finishReason").and_then(|f| f.as_str()) {
                    if !fr.is_empty() {
                        m.finish_reason = Some(fr.to_string());
                    }
                }
            }
        }
        if let Some(u) = ev.get("usageMetadata") {
            if !u.is_null() {
                m.usage = Some(u.clone());
            }
        }
    }
    m
}

/// Assemble the final detail-view JSON. Mirrors the prior
/// implementation's "omit unset" rule: `model`, `finish_reason`,
/// `usage`, `reasoning`, and `tool_calls` are only emitted when they
/// actually carry data, so the detail view never shows a stray
/// `null` field for families that don't track that dimension.
fn build_view(m: Merged, event_count: usize) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "protocol".to_string(),
        serde_json::Value::String(m.protocol.to_string()),
    );
    if let Some(model) = m.model {
        obj.insert("model".to_string(), serde_json::Value::String(model));
    }
    if !m.text.is_empty() {
        obj.insert("text".to_string(), serde_json::Value::String(m.text));
    }
    if !m.reasoning.is_empty() {
        obj.insert(
            "reasoning".to_string(),
            serde_json::Value::String(m.reasoning),
        );
    }
    if let Some(fr) = m.finish_reason {
        obj.insert("finish_reason".to_string(), serde_json::Value::String(fr));
    }
    if let Some(u) = m.usage {
        obj.insert("usage".to_string(), u);
    }
    if m.tool_calls > 0 {
        obj.insert(
            "tool_call_count".to_string(),
            serde_json::Value::Number(m.tool_calls.into()),
        );
    }
    obj.insert(
        "event_count".to_string(),
        serde_json::Value::Number(event_count.into()),
    );
    serde_json::Value::Object(obj)
}

/// Returns true when a usage struct carries no meaningful token data
/// (all fields zero/None). We never write such results back so that a
/// missing/garbage upstream body doesn't clobber a previously-written
/// row with zeros.
fn usage_is_empty(u: &Usage) -> bool {
    u.prompt_tokens == 0
        && u.completion_tokens == 0
        && u.total_tokens == 0
        && u.reasoning_tokens.unwrap_or(0) == 0
        && u.cache_read_tokens.unwrap_or(0) == 0
        && u.cache_write_tokens.unwrap_or(0) == 0
}

/// Extract a structured [`Usage`] from a non-streaming upstream JSON
/// response body. Mirrors the protocol-specific field mappings in the
/// `protocols` crate (chat_completions / responses / messages /
/// gemini) without taking a dependency on it — we sniff the protocol
/// from the response shape. Returns `None` when no usage block is
/// found.
fn extract_usage_from_json(body: &serde_json::Value) -> Option<Usage> {
    // Gemini uses `usageMetadata`, all others use `usage`.
    if let Some(u) = body.get("usageMetadata") {
        return Some(Usage {
            prompt_tokens: u["promptTokenCount"].as_u64().unwrap_or(0),
            completion_tokens: u["candidatesTokenCount"].as_u64().unwrap_or(0),
            total_tokens: u["totalTokenCount"].as_u64().unwrap_or(0),
            reasoning_tokens: u["thoughtsTokenCount"].as_u64(),
            cache_read_tokens: u["cachedContentTokenCount"].as_u64(),
            cache_write_tokens: None,
        });
    }

    let u = body.get("usage")?;
    if u.is_null() {
        return None;
    }

    // OpenAI Responses API: input_tokens / output_tokens.
    if u.get("input_tokens").is_some() || u.get("output_tokens").is_some() {
        // Anthropic Messages also uses input_tokens/output_tokens but
        // additionally carries cache_creation/cache_read_input_tokens
        // and has no total_tokens — disambiguate on those fields.
        let is_anthropic = u.get("cache_creation_input_tokens").is_some()
            || u.get("cache_read_input_tokens").is_some()
            || u.get("total_tokens").is_none();
        if is_anthropic {
            let input = u["input_tokens"].as_u64().unwrap_or(0);
            let output = u["output_tokens"].as_u64().unwrap_or(0);
            let cache_creation = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            let cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
            // Anthropic has no total_tokens; derive it identically to
            // protocols/messages.rs: input + cache_creation + cache_read + output.
            let total = u["total_tokens"]
                .as_u64()
                .unwrap_or(input + cache_creation + cache_read + output);
            return Some(Usage {
                prompt_tokens: input,
                completion_tokens: output,
                total_tokens: total,
                reasoning_tokens: u["output_tokens_details"]["thinking_tokens"].as_u64(),
                cache_read_tokens: u
                    .get("cache_read_input_tokens")
                    .is_some()
                    .then_some(cache_read),
                cache_write_tokens: u
                    .get("cache_creation_input_tokens")
                    .is_some()
                    .then_some(cache_creation),
            });
        }
        // OpenAI Responses API.
        return Some(Usage {
            prompt_tokens: u["input_tokens"].as_u64().unwrap_or(0),
            completion_tokens: u["output_tokens"].as_u64().unwrap_or(0),
            total_tokens: u["total_tokens"].as_u64().unwrap_or(0),
            reasoning_tokens: u["output_tokens_details"]["reasoning_tokens"].as_u64(),
            cache_read_tokens: u["input_tokens_details"]["cached_tokens"].as_u64(),
            cache_write_tokens: None,
        });
    }

    // OpenAI chat.completions / embeddings: prompt_tokens / completion_tokens.
    Some(Usage {
        prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
        completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
        total_tokens: u["total_tokens"].as_u64().unwrap_or(0),
        reasoning_tokens: u["completion_tokens_details"]["reasoning_tokens"].as_u64(),
        cache_read_tokens: u["prompt_tokens_details"]["cached_tokens"].as_u64(),
        cache_write_tokens: None,
    })
}

/// Extract a structured [`Usage`] from a raw SSE stream body. Walks
/// every `data:` JSON frame and accumulates per-protocol usage:
///   * OpenAI chat.completion.chunk — last frame's `usage`.
///   * OpenAI Responses — `response.completed` frame's `response.usage`.
///   * Anthropic — `message_start.message.usage` (input/cache) merged
///     with `message_delta.usage` (output).
///   * Gemini — last frame's `usageMetadata`.
fn extract_usage_from_sse(raw: &str) -> Option<Usage> {
    let mut frames: Vec<serde_json::Value> = Vec::new();
    for line in raw.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = rest.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
            frames.push(v);
        }
    }
    if frames.is_empty() {
        return None;
    }

    // Anthropic accumulates across two frame types; track separately.
    let mut anthropic_input: Option<u64> = None;
    let mut anthropic_output: u64 = 0;
    let mut anthropic_cache_read: Option<u64> = None;
    let mut anthropic_cache_creation: Option<u64> = None;
    let mut anthropic_reasoning: Option<u64> = None;
    let mut saw_anthropic = false;

    // Last-seen usage for OpenAI chat / Responses / Gemini.
    let mut last_json_usage: Option<Usage> = None;

    for ev in &frames {
        // Gemini frames carry usageMetadata.
        if ev.get("usageMetadata").is_some() {
            if let Some(u) = extract_usage_from_json(ev) {
                last_json_usage = Some(u);
            }
            continue;
        }
        // OpenAI chat.completion.chunk usage (sent on the final frame
        // when stream_options.include_usage is set).
        if ev.get("object").and_then(|o| o.as_str()) == Some("chat.completion.chunk") {
            if let Some(u) = ev.get("usage") {
                if !u.is_null() {
                    last_json_usage = Some(Usage {
                        prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0),
                        completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
                        total_tokens: u["total_tokens"].as_u64().unwrap_or(0),
                        reasoning_tokens: u["completion_tokens_details"]["reasoning_tokens"]
                            .as_u64(),
                        cache_read_tokens: u["prompt_tokens_details"]["cached_tokens"].as_u64(),
                        cache_write_tokens: None,
                    });
                }
            }
            continue;
        }
        // Frames discriminated by `type`: Anthropic + Responses.
        match ev.get("type").and_then(|t| t.as_str()) {
            Some("response.completed") => {
                if let Some(u) = ev.get("response").and_then(|r| r.get("usage")) {
                    if !u.is_null() {
                        last_json_usage = Some(Usage {
                            prompt_tokens: u["input_tokens"].as_u64().unwrap_or(0),
                            completion_tokens: u["output_tokens"].as_u64().unwrap_or(0),
                            total_tokens: u["total_tokens"].as_u64().unwrap_or(0),
                            reasoning_tokens: u["output_tokens_details"]["reasoning_tokens"]
                                .as_u64(),
                            cache_read_tokens: u["input_tokens_details"]["cached_tokens"].as_u64(),
                            cache_write_tokens: None,
                        });
                    }
                }
            }
            Some("message_start") => {
                if let Some(u) = ev["message"]["usage"].as_object() {
                    saw_anthropic = true;
                    anthropic_input =
                        Some(u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0));
                    if let Some(o) = u.get("output_tokens").and_then(|v| v.as_u64()) {
                        anthropic_output = o;
                    }
                    if let Some(cc) = u.get("cache_creation_input_tokens").and_then(|v| v.as_u64()) {
                        anthropic_cache_creation = Some(cc);
                    }
                    if let Some(cr) = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()) {
                        anthropic_cache_read = Some(cr);
                    }
                    if let Some(rt) = u
                        .get("output_tokens_details")
                        .and_then(|d| d.get("thinking_tokens"))
                        .and_then(|v| v.as_u64())
                    {
                        anthropic_reasoning = Some(rt);
                    }
                }
            }
            Some("message_delta") => {
                if let Some(u) = ev["usage"].as_object() {
                    saw_anthropic = true;
                    if let Some(o) = u.get("output_tokens").and_then(|v| v.as_u64()) {
                        anthropic_output = o;
                    }
                    if let Some(rt) = u
                        .get("output_tokens_details")
                        .and_then(|d| d.get("thinking_tokens"))
                        .and_then(|v| v.as_u64())
                    {
                        anthropic_reasoning = Some(rt);
                    }
                }
            }
            _ => {}
        }
    }

    if saw_anthropic {
        let input = anthropic_input.unwrap_or(0);
        let cache_creation = anthropic_cache_creation.unwrap_or(0);
        let cache_read = anthropic_cache_read.unwrap_or(0);
        let total = input + cache_creation + cache_read + anthropic_output;
        return Some(Usage {
            prompt_tokens: input,
            completion_tokens: anthropic_output,
            total_tokens: total,
            reasoning_tokens: anthropic_reasoning,
            cache_read_tokens: anthropic_cache_read,
            cache_write_tokens: anthropic_cache_creation,
        });
    }

    last_json_usage
}

/// Extract real upstream usage from an `ExchangeCapture`. For
/// streaming exchanges the upstream body is raw SSE; for non-stream
/// it is the full JSON response. Returns `None` when no meaningful
/// usage can be recovered (so callers can skip the write-back).
fn extract_usage_from_capture(capture: &ExchangeCapture) -> Option<Usage> {
    let body = capture.upstream_resp_body.as_deref()?;
    let usage = if capture.is_stream {
        extract_usage_from_sse(body)
    } else {
        serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|v| extract_usage_from_json(&v))
    }?;
    if usage_is_empty(&usage) {
        None
    } else {
        Some(usage)
    }
}

#[derive(Debug, Default)]
struct RequestEventRow {
    request_id: String,
    ts: String,
    virtual_model: String,
    resolved_provider: Option<String>,
    resolved_model: Option<String>,
    account_label: Option<String>,
    trace_id: Option<String>,
    span_id: Option<String>,
    traceparent: Option<String>,
    ingress_protocol: String,
    egress_protocol: Option<String>,
    lossy: bool,
    cache_hit: Option<String>,
    status: String,
    error_class: Option<String>,
    http_status: Option<u16>,
    error_source: Option<String>,
    total_latency_ms: u64,
    upstream_latency_ms: u64,
    queue_latency_ms: u64,
    ttfb_ms: Option<u64>,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    total_tokens: Option<u64>,
    cost: Option<u64>,
    api_key_id: Option<String>,
    client_ip: Option<String>,
    user_agent: Option<String>,
    raw_envelope_json: Option<String>,
    redacted_headers_json: Option<String>,
}

fn request_event_to_row(event: &RequestEvent) -> RequestEventRow {
    let tokens = event.tokens.clone();
    RequestEventRow {
        request_id: event.request_id.clone(),
        ts: event.timestamp.to_rfc3339(),
        virtual_model: event.virtual_model.clone(),
        resolved_provider: event.resolved_provider.clone(),
        resolved_model: event.resolved_model.clone(),
        account_label: event.account_label.clone(),
        trace_id: event.trace_id.clone(),
        span_id: event.span_id.clone(),
        traceparent: event.traceparent.clone(),
        ingress_protocol: event.ingress_protocol.clone(),
        egress_protocol: event.egress_protocol.clone(),
        lossy: event.lossy,
        cache_hit: event.cache_hit.clone(),
        status: event.status.clone(),
        error_class: event.error_class.clone(),
        http_status: event.http_status,
        error_source: event.error_source.clone(),
        total_latency_ms: event.latency_ms.total_ms,
        upstream_latency_ms: event.latency_ms.upstream_ms,
        queue_latency_ms: event.latency_ms.queue_ms,
        ttfb_ms: event.ttfb_ms,
        prompt_tokens: tokens.as_ref().map(|t| t.prompt_tokens),
        completion_tokens: tokens.as_ref().map(|t| t.completion_tokens),
        reasoning_tokens: tokens.as_ref().and_then(|t| t.reasoning_tokens),
        cache_read_tokens: tokens.as_ref().and_then(|t| t.cache_read_tokens),
        cache_write_tokens: tokens.as_ref().and_then(|t| t.cache_write_tokens),
        total_tokens: tokens.as_ref().map(|t| t.total_tokens),
        cost: event.cost,
        api_key_id: event.api_key_id.clone(),
        client_ip: event.client_ip.clone(),
        user_agent: event.user_agent.clone(),
        raw_envelope_json: event
            .raw_envelope
            .as_ref()
            .and_then(|env| serde_json::to_string(env).ok()),
        redacted_headers_json: event
            .raw_envelope
            .as_ref()
            .and_then(|env| serde_json::to_string(&env.headers).ok()),
    }
}

// ---------------------------------------------------------------------
// Aggregated query helpers (used by admin/stats handlers)
// ---------------------------------------------------------------------

/// Aggregated counts keyed by `virtual_model` for a time window.
#[derive(Debug, Default, serde::Serialize)]
pub struct StatsBucket {
    pub bucket: String,
    pub count: u64,
    pub error_count: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
}

/// Aggregate per `virtual_model` for events with `ts` in
/// `[since, until]`. `since`/`until` are RFC-3339 strings. Used by
/// the admin dashboard endpoint.
pub async fn aggregate_by_model(
    pool: &DbPool,
    since: &str,
    until: &str,
) -> Result<Vec<StatsBucket>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT virtual_model, COUNT(*) AS c, \
                SUM(CASE WHEN status != 'ok' THEN 1 ELSE 0 END) AS e, \
                COALESCE(SUM(prompt_tokens), 0) AS pt, \
                COALESCE(SUM(completion_tokens), 0) AS ct, \
                COALESCE(SUM(reasoning_tokens), 0) AS rt, \
                COALESCE(SUM(cache_read_tokens), 0) AS crt, \
                COALESCE(SUM(cache_write_tokens), 0) AS cwt, \
                COALESCE(SUM(total_tokens), 0) AS tt \
         FROM request_logs \
         WHERE ts >= ?1 AND ts < ?2 \
         GROUP BY virtual_model \
         ORDER BY c DESC",
    )
    .bind(since)
    .bind(until)
    .fetch_all(pool.sqlite())
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(StatsBucket {
            bucket: r.get("virtual_model"),
            count: r.get::<i64, _>("c") as u64,
            error_count: r.get::<i64, _>("e") as u64,
            prompt_tokens: r.get::<i64, _>("pt") as u64,
            completion_tokens: r.get::<i64, _>("ct") as u64,
            reasoning_tokens: r.get::<i64, _>("rt") as u64,
            cache_read_tokens: r.get::<i64, _>("crt") as u64,
            cache_write_tokens: r.get::<i64, _>("cwt") as u64,
            total_tokens: r.get::<i64, _>("tt") as u64,
        });
    }
    Ok(out)
}

/// Aggregate by `resolved_provider` (or `unknown` if missing).
pub async fn aggregate_by_provider(
    pool: &DbPool,
    since: &str,
    until: &str,
) -> Result<Vec<StatsBucket>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT COALESCE(resolved_provider, 'unknown') AS provider, COUNT(*) AS c, \
                SUM(CASE WHEN status != 'ok' THEN 1 ELSE 0 END) AS e, \
                COALESCE(SUM(prompt_tokens), 0) AS pt, \
                COALESCE(SUM(completion_tokens), 0) AS ct, \
                COALESCE(SUM(reasoning_tokens), 0) AS rt, \
                COALESCE(SUM(cache_read_tokens), 0) AS crt, \
                COALESCE(SUM(cache_write_tokens), 0) AS cwt, \
                COALESCE(SUM(total_tokens), 0) AS tt \
         FROM request_logs \
         WHERE ts >= ?1 AND ts < ?2 \
         GROUP BY provider \
         ORDER BY c DESC",
    )
    .bind(since)
    .bind(until)
    .fetch_all(pool.sqlite())
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(StatsBucket {
            bucket: r.get("provider"),
            count: r.get::<i64, _>("c") as u64,
            error_count: r.get::<i64, _>("e") as u64,
            prompt_tokens: r.get::<i64, _>("pt") as u64,
            completion_tokens: r.get::<i64, _>("ct") as u64,
            reasoning_tokens: r.get::<i64, _>("rt") as u64,
            cache_read_tokens: r.get::<i64, _>("crt") as u64,
            cache_write_tokens: r.get::<i64, _>("cwt") as u64,
            total_tokens: r.get::<i64, _>("tt") as u64,
        });
    }
    Ok(out)
}

/// Aggregate by `api_key_id` (or `anonymous` if missing).
pub async fn aggregate_by_api_key(
    pool: &DbPool,
    since: &str,
    until: &str,
) -> Result<Vec<StatsBucket>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT COALESCE(api_key_id, 'anonymous') AS api_key, COUNT(*) AS c, \
                SUM(CASE WHEN status != 'ok' THEN 1 ELSE 0 END) AS e, \
                COALESCE(SUM(prompt_tokens), 0) AS pt, \
                COALESCE(SUM(completion_tokens), 0) AS ct, \
                COALESCE(SUM(reasoning_tokens), 0) AS rt, \
                COALESCE(SUM(cache_read_tokens), 0) AS crt, \
                COALESCE(SUM(cache_write_tokens), 0) AS cwt, \
                COALESCE(SUM(total_tokens), 0) AS tt \
         FROM request_logs \
         WHERE ts >= ?1 AND ts < ?2 \
         GROUP BY api_key \
         ORDER BY c DESC",
    )
    .bind(since)
    .bind(until)
    .fetch_all(pool.sqlite())
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(StatsBucket {
            bucket: r.get("api_key"),
            count: r.get::<i64, _>("c") as u64,
            error_count: r.get::<i64, _>("e") as u64,
            prompt_tokens: r.get::<i64, _>("pt") as u64,
            completion_tokens: r.get::<i64, _>("ct") as u64,
            reasoning_tokens: r.get::<i64, _>("rt") as u64,
            cache_read_tokens: r.get::<i64, _>("crt") as u64,
            cache_write_tokens: r.get::<i64, _>("cwt") as u64,
            total_tokens: r.get::<i64, _>("tt") as u64,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------
// Request log drill-down & replay (Phase 4 analysis / §8 acceptance #8)
// ---------------------------------------------------------------------

/// A single row from `request_logs`, returned for drill-down queries.
#[derive(Debug, Default, serde::Serialize)]
pub struct RequestLogEntry {
    pub request_id: String,
    pub ts: String,
    pub virtual_model: String,
    pub resolved_provider: Option<String>,
    pub resolved_model: Option<String>,
    pub account_label: Option<String>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    pub traceparent: Option<String>,
    pub ingress_protocol: String,
    pub egress_protocol: Option<String>,
    pub lossy: bool,
    pub cache_hit: Option<String>,
    pub status: String,
    pub error_class: Option<String>,
    pub http_status: Option<u16>,
    pub error_source: Option<String>,
    pub total_latency_ms: u64,
    pub upstream_latency_ms: u64,
    pub queue_latency_ms: u64,
    pub ttfb_ms: Option<u64>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub cost: Option<u64>,
    pub api_key_id: Option<String>,
    pub client_ip: Option<String>,
    pub user_agent: Option<String>,
}

fn row_to_entry(row: &sqlx::sqlite::SqliteRow) -> RequestLogEntry {
    RequestLogEntry {
        request_id: row.get("request_id"),
        ts: row.get("ts"),
        virtual_model: row.get("virtual_model"),
        resolved_provider: row.get("resolved_provider"),
        resolved_model: row.get("resolved_model"),
        account_label: row.get("account_label"),
        trace_id: row.get("trace_id"),
        span_id: row.get("span_id"),
        traceparent: row.get("traceparent"),
        ingress_protocol: row.get("ingress_protocol"),
        egress_protocol: row.get("egress_protocol"),
        lossy: row.get::<i32, _>("lossy") != 0,
        cache_hit: row.get("cache_hit"),
        status: row.get("status"),
        error_class: row.get("error_class"),
        http_status: row.get::<Option<i32>, _>("http_status").map(|n| n as u16),
        error_source: row.get("error_source"),
        total_latency_ms: row.get::<i64, _>("total_latency_ms") as u64,
        upstream_latency_ms: row.get::<i64, _>("upstream_latency_ms") as u64,
        queue_latency_ms: row.get::<i64, _>("queue_latency_ms") as u64,
        ttfb_ms: row.get::<Option<i64>, _>("ttfb_ms").map(|n| n as u64),
        prompt_tokens: row.get::<Option<i64>, _>("prompt_tokens").map(|n| n as u64),
        completion_tokens: row
            .get::<Option<i64>, _>("completion_tokens")
            .map(|n| n as u64),
        reasoning_tokens: row
            .get::<Option<i64>, _>("reasoning_tokens")
            .map(|n| n as u64),
        cache_read_tokens: row
            .get::<Option<i64>, _>("cache_read_tokens")
            .map(|n| n as u64),
        cache_write_tokens: row
            .get::<Option<i64>, _>("cache_write_tokens")
            .map(|n| n as u64),
        total_tokens: row.get::<Option<i64>, _>("total_tokens").map(|n| n as u64),
        cost: row.get::<Option<i64>, _>("cost").map(|n| n as u64),
        api_key_id: row.get("api_key_id"),
        client_ip: row.get("client_ip"),
        user_agent: row.get("user_agent"),
    }
}

/// Filter parameters for request log drill-down.
#[derive(Debug, Default, Clone)]
pub struct RequestFilter {
    /// RFC-3339 timestamp for lower bound (inclusive).
    pub since: Option<String>,
    /// RFC-3339 timestamp for upper bound (exclusive).
    pub until: Option<String>,
    /// Filter by virtual model name.
    pub model: Option<String>,
    /// Filter by provider id.
    pub provider: Option<String>,
    /// Filter by status: "ok", "error".
    pub status: Option<String>,
    /// Filter by error class.
    pub error_class: Option<String>,
    /// Only return requests with latency >= this threshold (ms).
    pub min_latency_ms: Option<u64>,
    /// Only return requests with latency <= this threshold (ms).
    pub max_latency_ms: Option<u64>,
    /// Maximum number of entries to return (default 50, max 500).
    pub limit: Option<u32>,
    /// Offset for pagination (default 0).
    pub offset: Option<u32>,
}

/// List individual request log entries matching the given filter.
/// Ordered by `ts DESC` (most recent first).
/// Returns `(entries, total_count)` for pagination.
pub async fn list_requests(
    pool: &DbPool,
    filter: &RequestFilter,
) -> Result<(Vec<RequestLogEntry>, u64), sqlx::Error> {
    let limit = filter.limit.unwrap_or(50).clamp(1, 500) as i64;
    let offset = filter.offset.unwrap_or(0) as i64;

    let now = chrono::Utc::now();
    let default_since = (now - chrono::Duration::hours(24)).to_rfc3339();
    let since = filter.since.as_deref().unwrap_or(&default_since);
    let now_rfc = now.to_rfc3339();
    let until = filter.until.as_deref().unwrap_or(&now_rfc);

    // Build WHERE clauses.
    let mut clauses = vec!["ts >= ?1".to_string(), "ts < ?2".to_string()];
    // We'll track param index and use a prefix approach for count.
    let mut param_idx = 3i32;

    // For the simple list, we can use a builder pattern with dynamic query building.
    // Since sqlx doesn't support dynamic WHERE via format strings with bound params,
    // we use a simpler approach: default 24h window + optional filters appended
    // as additional WHERE clauses in sorted order.

    // Track which optional filters are active for the count query too.
    let mut active_model: Option<String> = None;
    let mut active_provider: Option<String> = None;
    let mut active_status: Option<String> = None;
    let mut active_error_class: Option<String> = None;
    let mut active_min_latency: Option<u64> = None;
    let mut active_max_latency: Option<u64> = None;

    if let Some(ref m) = filter.model {
        clauses.push(format!("virtual_model = ?{param_idx}"));
        active_model = Some(m.clone());
        param_idx += 1;
    }
    if let Some(ref p) = filter.provider {
        clauses.push(format!("resolved_provider = ?{param_idx}"));
        active_provider = Some(p.clone());
        param_idx += 1;
    }
    if let Some(ref s) = filter.status {
        clauses.push(format!("status = ?{param_idx}"));
        active_status = Some(s.clone());
        param_idx += 1;
    }
    if let Some(ref ec) = filter.error_class {
        clauses.push(format!("error_class = ?{param_idx}"));
        active_error_class = Some(ec.clone());
        param_idx += 1;
    }
    if let Some(min_l) = filter.min_latency_ms {
        clauses.push(format!("total_latency_ms >= ?{param_idx}"));
        active_min_latency = Some(min_l);
        param_idx += 1;
    }
    if let Some(max_l) = filter.max_latency_ms {
        clauses.push(format!("total_latency_ms <= ?{param_idx}"));
        active_max_latency = Some(max_l);
        param_idx += 1;
    }

    let where_str = clauses.join(" AND ");

    // Count query — manually bind
    let count_sql = format!("SELECT COUNT(*) FROM request_logs WHERE {where_str}");
    let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql)
        .bind(since.to_string())
        .bind(until.to_string());
    if let Some(ref m) = active_model {
        count_query = count_query.bind(m.clone());
    }
    if let Some(ref p) = active_provider {
        count_query = count_query.bind(p.clone());
    }
    if let Some(ref s) = active_status {
        count_query = count_query.bind(s.clone());
    }
    if let Some(ref ec) = active_error_class {
        count_query = count_query.bind(ec.clone());
    }
    if let Some(v) = active_min_latency {
        count_query = count_query.bind(v as i64);
    }
    if let Some(v) = active_max_latency {
        count_query = count_query.bind(v as i64);
    }
    let total: i64 = count_query.fetch_one(pool.sqlite()).await?;

    // Data query
    let data_sql = format!(
        "SELECT request_id, ts, virtual_model, resolved_provider, resolved_model, \
                account_label, trace_id, span_id, traceparent, \
                ingress_protocol, egress_protocol, lossy, cache_hit, \
                status, error_class, http_status, error_source, \
                total_latency_ms, upstream_latency_ms, queue_latency_ms, ttfb_ms, \
                prompt_tokens, completion_tokens, reasoning_tokens, \
                cache_read_tokens, cache_write_tokens, total_tokens, \
                cost, api_key_id, client_ip, user_agent \
         FROM request_logs \
         WHERE {where_str} \
         ORDER BY ts DESC \
         LIMIT ?{param_idx} OFFSET ?{p1}",
        p1 = param_idx + 1
    );

    let mut data_query = sqlx::query(&data_sql)
        .bind(since.to_string())
        .bind(until.to_string());
    if let Some(ref m) = active_model {
        data_query = data_query.bind(m.clone());
    }
    if let Some(ref p) = active_provider {
        data_query = data_query.bind(p.clone());
    }
    if let Some(ref s) = active_status {
        data_query = data_query.bind(s.clone());
    }
    if let Some(ref ec) = active_error_class {
        data_query = data_query.bind(ec.clone());
    }
    if let Some(v) = active_min_latency {
        data_query = data_query.bind(v as i64);
    }
    if let Some(v) = active_max_latency {
        data_query = data_query.bind(v as i64);
    }
    data_query = data_query.bind(limit).bind(offset);

    let rows = data_query.fetch_all(pool.sqlite()).await?;
    let entries: Vec<RequestLogEntry> = rows.iter().map(row_to_entry).collect();
    Ok((entries, total as u64))
}

/// Result for a single request replay: raw envelope JSON and
/// redacted headers JSON, so an operator can reconstruct the
/// original request body and headers for debugging. Phase 5 extends
/// this with the full exchange payload (egress request, upstream
/// response, client response) joined from `request_payloads`.
#[derive(Debug, Default, serde::Serialize)]
pub struct RequestReplay {
    pub request_id: String,
    pub raw_envelope_json: Option<String>,
    pub redacted_headers_json: Option<String>,
    // ---- full exchange payload (LEFT JOIN request_payloads) ----
    /// HTTP method used for the gateway → provider request
    /// (e.g. "POST"). Empty when the exchange was not captured.
    pub egress_method: Option<String>,
    /// URL path used for the gateway → provider request
    /// (e.g. "/v1/chat/completions"). Empty when the exchange
    /// was not captured.
    pub egress_path: Option<String>,
    pub egress_headers_json: Option<String>,
    pub egress_body: Option<String>,
    pub egress_body_truncated: bool,
    pub upstream_status: Option<u16>,
    pub upstream_resp_headers_json: Option<String>,
    pub upstream_resp_body: Option<String>,
    pub upstream_resp_body_truncated: bool,
    pub client_resp_headers_json: Option<String>,
    pub client_resp_body: Option<String>,
    pub client_resp_body_truncated: bool,
    pub is_stream: bool,
    pub sse_parsed_json: Option<String>,
    pub client_sse_parsed_json: Option<String>,
}

/// Fetch the raw envelope (redacted) for a given request id.
/// Used by the admin replay endpoint for failed/slow request
/// debugging (per §4.4 / §8 acceptance #8).
pub async fn get_request_replay(
    pool: &DbPool,
    request_id: &str,
) -> Result<Option<RequestReplay>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT l.request_id AS request_id, l.raw_envelope_json AS raw_envelope_json, \
                l.redacted_headers_json AS redacted_headers_json, \
                p.egress_method AS egress_method, p.egress_path AS egress_path, \
                p.egress_headers_json AS egress_headers_json, \
                p.egress_body AS egress_body, \
                p.egress_body_truncated AS egress_body_truncated, \
                p.upstream_status AS upstream_status, \
                p.upstream_resp_headers_json AS upstream_resp_headers_json, \
                p.upstream_resp_body AS upstream_resp_body, \
                p.upstream_resp_body_truncated AS upstream_resp_body_truncated, \
                p.client_resp_headers_json AS client_resp_headers_json, \
                p.client_resp_body AS client_resp_body, \
                p.client_resp_body_truncated AS client_resp_body_truncated, \
                p.is_stream AS is_stream, \
                p.sse_parsed_json AS sse_parsed_json, \
                p.client_sse_parsed_json AS client_sse_parsed_json \
         FROM request_logs l \
         LEFT JOIN request_payloads p ON p.request_id = l.request_id \
         WHERE l.request_id = ?1",
    )
    .bind(request_id)
    .fetch_optional(pool.sqlite())
    .await?;
    if let Some(r) = row {
        Ok(Some(RequestReplay {
            request_id: r.get("request_id"),
            raw_envelope_json: r.get("raw_envelope_json"),
            redacted_headers_json: r.get("redacted_headers_json"),
            egress_method: r.get("egress_method"),
            egress_path: r.get("egress_path"),
            egress_headers_json: r.get("egress_headers_json"),
            egress_body: r.get("egress_body"),
            egress_body_truncated: r
                .get::<Option<i32>, _>("egress_body_truncated")
                .unwrap_or(0)
                != 0,
            upstream_status: r.get::<Option<i32>, _>("upstream_status").map(|n| n as u16),
            upstream_resp_headers_json: r.get("upstream_resp_headers_json"),
            upstream_resp_body: r.get("upstream_resp_body"),
            upstream_resp_body_truncated: r
                .get::<Option<i32>, _>("upstream_resp_body_truncated")
                .unwrap_or(0)
                != 0,
            client_resp_headers_json: r.get("client_resp_headers_json"),
            client_resp_body: r.get("client_resp_body"),
            client_resp_body_truncated: r
                .get::<Option<i32>, _>("client_resp_body_truncated")
                .unwrap_or(0)
                != 0,
            is_stream: r.get::<Option<i32>, _>("is_stream").unwrap_or(0) != 0,
            sse_parsed_json: r.get("sse_parsed_json"),
            client_sse_parsed_json: r.get("client_sse_parsed_json"),
        }))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use chrono::Utc;
    use tiygate_core::telemetry::LatencyBreakdown;

    fn dummy_request_event() -> RequestEvent {
        RequestEvent {
            request_id: "req-1".to_string(),
            timestamp: Utc::now(),
            virtual_model: "gpt-4o".to_string(),
            resolved_provider: Some("openai".to_string()),
            resolved_model: Some("gpt-4o".to_string()),
            account_label: None,
            trace_id: Some("4bf92f3577b34da6a3ce929d0e0e4736".to_string()),
            span_id: Some("00f067aa0ba902b7".to_string()),
            traceparent: Some(
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_string(),
            ),
            ingress_protocol: "openai/chat-completions/v1".to_string(),
            egress_protocol: Some("openai/chat-completions/v1".to_string()),
            lossy: false,
            cache_hit: None,
            status: "ok".to_string(),
            error_class: None,
            http_status: Some(200),
            error_source: None,
            latency_ms: LatencyBreakdown {
                total_ms: 123,
                upstream_ms: 100,
                queue_ms: 5,
            },
            ttfb_ms: Some(50),
            tokens: Some(tiygate_core::Usage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                ..Default::default()
            }),
            cost: None,
            api_key_id: Some("key-1".to_string()),
            client_ip: Some("127.0.0.1".to_string()),
            user_agent: Some("test".to_string()),
            raw_envelope: None,
        }
    }

    #[tokio::test]
    async fn write_request_event_persists_row() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(pool.sqlite()).await.expect("migrate");
        let sink = OltpSink::new(Arc::new(pool.clone()));
        sink.write_request_event(&dummy_request_event())
            .await
            .expect("write");
        let now = Utc::now().to_rfc3339();
        let earlier = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let by_model = aggregate_by_model(&pool, &earlier, &now)
            .await
            .expect("agg");
        assert!(!by_model.is_empty());
        assert_eq!(by_model[0].bucket, "gpt-4o");
        assert_eq!(by_model[0].prompt_tokens, 10);
    }

    #[tokio::test]
    async fn aggregate_by_provider_groups_unknown_when_null() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(pool.sqlite()).await.expect("migrate");
        let sink = OltpSink::new(Arc::new(pool.clone()));
        let mut ev = dummy_request_event();
        ev.resolved_provider = None;
        sink.write_request_event(&ev).await.expect("write");
        let now = Utc::now().to_rfc3339();
        let earlier = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let by_provider = aggregate_by_provider(&pool, &earlier, &now)
            .await
            .expect("agg");
        assert!(!by_provider.is_empty());
        assert_eq!(by_provider[0].bucket, "unknown");
    }

    #[tokio::test]
    async fn write_request_event_persists_raw_envelope() {
        use chrono::Utc;
        use tiygate_core::telemetry::LatencyBreakdown;
        use tiygate_core::RawEnvelope;

        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(pool.sqlite()).await.expect("migrate");
        let sink = OltpSink::new(Arc::new(pool.clone()));

        let envelope = RawEnvelope {
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            headers: [("authorization".to_string(), "Bearer sk-test".to_string())]
                .into_iter()
                .collect(),
            body: Some("{\"model\":\"gpt-4o\"}".to_string()),
            truncated: false,
            original_body_size: 18,
            timestamp: Utc::now(),
        };
        let mut ev = dummy_request_event();
        ev.raw_envelope = Some(envelope.clone());
        sink.write_request_event(&ev).await.expect("write");

        let row: Option<String> =
            sqlx::query_scalar("SELECT raw_envelope_json FROM request_logs WHERE request_id = ?1")
                .bind(&ev.request_id)
                .fetch_optional(pool.sqlite())
                .await
                .expect("query");
        let stored = row.expect("raw_envelope_json should be persisted");
        let parsed: RawEnvelope = serde_json::from_str(&stored).expect("parse");
        assert_eq!(parsed.method, envelope.method);
        assert_eq!(parsed.path, envelope.path);
        assert_eq!(parsed.body, envelope.body);
        assert_eq!(parsed.headers, envelope.headers);
    }

    #[tokio::test]
    async fn list_requests_with_filter() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(pool.sqlite()).await.expect("migrate");
        let sink = OltpSink::new(Arc::new(pool.clone()));
        sink.write_request_event(&dummy_request_event())
            .await
            .expect("write");

        let now = chrono::Utc::now();
        let since = (now - chrono::Duration::hours(1)).to_rfc3339();
        let until = (now + chrono::Duration::hours(1)).to_rfc3339();

        let (entries, total) = list_requests(
            &pool,
            &RequestFilter {
                since: Some(since.clone()),
                until: Some(until.clone()),
                model: Some("gpt-4o".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("list");
        assert_eq!(total, 1);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].virtual_model, "gpt-4o");
        assert_eq!(entries[0].status, "ok");
    }

    #[tokio::test]
    async fn get_request_replay_returns_envelope() {
        use chrono::Utc;
        use tiygate_core::telemetry::LatencyBreakdown;
        use tiygate_core::RawEnvelope;

        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(pool.sqlite()).await.expect("migrate");
        let sink = OltpSink::new(Arc::new(pool.clone()));

        let envelope = RawEnvelope {
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            headers: [("content-type".to_string(), "application/json".to_string())]
                .into_iter()
                .collect(),
            body: Some("{\"model\":\"gpt-4o\"}".to_string()),
            truncated: false,
            original_body_size: 18,
            timestamp: Utc::now(),
        };
        let mut ev = dummy_request_event();
        ev.raw_envelope = Some(envelope);
        sink.write_request_event(&ev).await.expect("write");

        let replay = get_request_replay(&pool, "req-1")
            .await
            .expect("replay")
            .expect("should exist");
        assert_eq!(replay.request_id, "req-1");
        assert!(replay.raw_envelope_json.is_some());
        // The envelope JSON should contain model name (exact format
        // depends on serde_json serialization).
        assert!(replay.raw_envelope_json.unwrap().contains("gpt-4o"));
    }

    #[tokio::test]
    async fn write_capture_persists_and_replay_joins_payload() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(pool.sqlite()).await.expect("migrate");
        let sink = OltpSink::new(Arc::new(pool.clone()));

        // The payload row references a request_logs row via request_id;
        // write the request event first so the LEFT JOIN has a left row.
        sink.write_request_event(&dummy_request_event())
            .await
            .expect("write event");

        let capture = ExchangeCapture {
            request_id: "req-1".to_string(),
            egress_method: "POST".to_string(),
            egress_path: "/v1/chat/completions".to_string(),
            egress_headers: vec![
                ("authorization".to_string(), "Bearer sk-secret".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            egress_body: Some("{\"model\":\"gpt-4o\",\"api_key\":\"sk-leak\"}".to_string()),
            upstream_status: Some(200),
            upstream_resp_headers: vec![("x-req-id".to_string(), "abc".to_string())],
            upstream_resp_body: Some("{\"id\":\"chatcmpl-1\"}".to_string()),
            client_resp_headers: vec![("content-type".to_string(), "application/json".to_string())],
            client_resp_body: Some("{\"id\":\"chatcmpl-1\"}".to_string()),
            is_stream: false,
        };
        sink.write_capture(&capture).await.expect("write capture");

        let replay = get_request_replay(&pool, "req-1")
            .await
            .expect("replay")
            .expect("exists");
        // Header redaction: authorization must be masked.
        let eh = replay.egress_headers_json.expect("egress headers");
        assert!(eh.contains("[REDACTED]"), "authorization not redacted: {eh}");
        // Body redaction: api_key value must be masked.
        let eb = replay.egress_body.expect("egress body");
        assert!(eb.contains("[REDACTED]"), "api_key not redacted: {eb}");
        assert!(eb.contains("gpt-4o"));
        assert_eq!(replay.upstream_status, Some(200));
        assert!(replay.upstream_resp_body.unwrap().contains("chatcmpl-1"));
        assert!(!replay.is_stream);
    }

    #[test]
    fn parse_sse_merges_openai_chunks() {
        let raw = "data: {\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4o\",\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\
                   data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\
                   data: [DONE]\n";
        let parsed = parse_sse_to_json(raw).expect("should parse");
        let v: serde_json::Value = serde_json::from_str(&parsed).unwrap();
        assert_eq!(v["protocol"], "openai");
        assert_eq!(v["text"], "Hello");
        assert_eq!(v["finish_reason"], "stop");
        assert_eq!(v["model"], "gpt-4o");
    }

    #[test]
    fn parse_sse_merges_anthropic_deltas() {
        let raw = "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-3\"}}\n\
                   data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"Hi \"}}\n\
                   data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"there\"}}\n\
                   data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n";
        let parsed = parse_sse_to_json(raw).expect("should parse");
        let v: serde_json::Value = serde_json::from_str(&parsed).unwrap();
        assert_eq!(v["protocol"], "anthropic");
        assert_eq!(v["text"], "Hi there");
        assert_eq!(v["finish_reason"], "end_turn");
        assert_eq!(v["model"], "claude-3");
        assert_eq!(v["usage"]["output_tokens"], 5);
    }

    #[test]
    fn parse_sse_returns_none_on_garbage() {
        assert!(parse_sse_to_json("not an sse stream").is_none());
        assert!(parse_sse_to_json("").is_none());
        assert!(parse_sse_to_json("data: [DONE]\n").is_none());
    }

    #[test]
    fn parse_sse_merges_openai_responses() {
        // 7-frame sample lifted from the production request log —
        // matches the G→C transcode case that was previously being
        // mislabeled as Anthropic.
        let raw = "data: {\"response\":{\"id\":\"r1\",\"object\":\"response\",\"status\":\"in_progress\"},\"type\":\"response.created\"}\n\
                   data: {\"response\":{\"id\":\"r1\",\"usage\":{\"input_tokens\":55,\"output_tokens\":0,\"total_tokens\":169}},\"type\":\"response.completed\"}\n\
                   data: {\"content_index\":0,\"delta\":\"\",\"item_id\":\"r1_msg\",\"output_index\":0,\"type\":\"response.output_text.delta\"}\n\
                   data: {\"content_index\":0,\"delta\":\"P\",\"item_id\":\"r1_msg\",\"output_index\":1,\"type\":\"response.output_text.delta\"}\n\
                   data: {\"content_index\":0,\"delta\":\"ong! 👋 TiyCode, got your ping loud and clear.\",\"item_id\":\"r1_msg\",\"output_index\":2,\"type\":\"response.output_text.delta\"}\n\
                   data: {\"response\":{\"id\":\"r1\",\"usage\":{\"input_tokens\":55,\"output_tokens\":16,\"total_tokens\":185}},\"type\":\"response.completed\"}\n\
                   data: {\"response\":{\"id\":\"r1\",\"status\":\"incomplete\"},\"type\":\"response.completed\"}\n\
                   data: [DONE]\n";
        let parsed = parse_sse_to_json(raw).expect("should parse");
        let v: serde_json::Value = serde_json::from_str(&parsed).unwrap();
        assert_eq!(v["protocol"], "openai_responses");
        assert_eq!(v["text"], "Pong! 👋 TiyCode, got your ping loud and clear.");
        // Last `response.completed` wins → input_tokens=55, output_tokens=16, total_tokens=185.
        assert_eq!(v["usage"]["input_tokens"], 55);
        assert_eq!(v["usage"]["output_tokens"], 16);
        assert_eq!(v["usage"]["total_tokens"], 185);
        // Last `response.completed.response.status` is "incomplete" → "length".
        assert_eq!(v["finish_reason"], "length");
        // [DONE] is not counted.
        assert_eq!(v["event_count"], 7);
        // No `response.created` in this sample, so model is absent.
        assert!(v.get("model").is_none());
    }

    #[test]
    fn parse_sse_does_not_mislabel_responses_as_anthropic() {
        // Regression guard: a Responses stream whose first event is
        // `response.created` must never be classified as Anthropic,
        // even though both protocols use a top-level `type` field.
        let raw = "data: {\"response\":{\"id\":\"r2\",\"model\":\"gpt-4o\",\"object\":\"response\",\"status\":\"in_progress\"},\"type\":\"response.created\"}\n\
                   data: {\"content_index\":0,\"delta\":\"hi\",\"item_id\":\"r2_msg\",\"output_index\":0,\"type\":\"response.output_text.delta\"}\n\
                   data: {\"response\":{\"id\":\"r2\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}},\"type\":\"response.completed\"}\n";
        let parsed = parse_sse_to_json(raw).expect("should parse");
        let v: serde_json::Value = serde_json::from_str(&parsed).unwrap();
        assert_eq!(v["protocol"], "openai_responses");
        assert_eq!(v["model"], "gpt-4o");
        assert_eq!(v["text"], "hi");
        assert_eq!(v["finish_reason"], "stop");
    }

    #[test]
    fn parse_sse_merges_gemini_stream() {
        let raw = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hel\"}]}}],\"modelVersion\":\"gemini-1.5-pro\"}\n\
                   data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"lo\"}]}}]}\n\
                   data: {\"candidates\":[{\"content\":{\"parts\":[]},\"finishReason\":\"STOP\"}]}\n\
                   data: {\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2,\"totalTokenCount\":7}}\n";
        let parsed = parse_sse_to_json(raw).expect("should parse");
        let v: serde_json::Value = serde_json::from_str(&parsed).unwrap();
        assert_eq!(v["protocol"], "gemini");
        assert_eq!(v["model"], "gemini-1.5-pro");
        assert_eq!(v["text"], "Hello");
        assert_eq!(v["finish_reason"], "STOP");
        assert_eq!(v["usage"]["totalTokenCount"], 7);
    }

    #[test]
    fn parse_sse_merges_gemini_reasoning_and_tool() {
        // Reasoning (thought) deltas must land in `reasoning`, not
        // `text`. functionCall parts bump `tool_call_count` and must
        // not contribute to the visible text.
        let raw = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"thought\":\"Plan: call tool\"}]}}]}\n\
                   data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Calling \"}]}}]}\n\
                   data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"lookup\",\"args\":{}}}]}}]}\n\
                   data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"done.\"}]},\"finishReason\":\"STOP\"}]}\n";
        let parsed = parse_sse_to_json(raw).expect("should parse");
        let v: serde_json::Value = serde_json::from_str(&parsed).unwrap();
        assert_eq!(v["protocol"], "gemini");
        assert_eq!(v["text"], "Calling done.");
        assert_eq!(v["reasoning"], "Plan: call tool");
        assert_eq!(v["tool_call_count"], 1);
        assert_eq!(v["finish_reason"], "STOP");
    }

    #[test]
    fn extract_usage_json_openai_chat() {
        let body = serde_json::json!({
            "usage": {
                "prompt_tokens": 11,
                "completion_tokens": 22,
                "total_tokens": 33,
                "prompt_tokens_details": {"cached_tokens": 4},
                "completion_tokens_details": {"reasoning_tokens": 7}
            }
        });
        let u = extract_usage_from_json(&body).expect("usage");
        assert_eq!(u.prompt_tokens, 11);
        assert_eq!(u.completion_tokens, 22);
        assert_eq!(u.total_tokens, 33);
        assert_eq!(u.cache_read_tokens, Some(4));
        assert_eq!(u.reasoning_tokens, Some(7));
        assert_eq!(u.cache_write_tokens, None);
    }

    #[test]
    fn extract_usage_json_responses() {
        let body = serde_json::json!({
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "total_tokens": 150,
                "input_tokens_details": {"cached_tokens": 20},
                "output_tokens_details": {"reasoning_tokens": 30}
            }
        });
        let u = extract_usage_from_json(&body).expect("usage");
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.completion_tokens, 50);
        assert_eq!(u.total_tokens, 150);
        assert_eq!(u.cache_read_tokens, Some(20));
        assert_eq!(u.reasoning_tokens, Some(30));
    }

    #[test]
    fn extract_usage_json_anthropic_derives_total() {
        let body = serde_json::json!({
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_creation_input_tokens": 3,
                "cache_read_input_tokens": 2
            }
        });
        let u = extract_usage_from_json(&body).expect("usage");
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.completion_tokens, 5);
        // total = input + cache_creation + cache_read + output = 10+3+2+5
        assert_eq!(u.total_tokens, 20);
        assert_eq!(u.cache_read_tokens, Some(2));
        assert_eq!(u.cache_write_tokens, Some(3));
    }

    #[test]
    fn extract_usage_json_gemini() {
        let body = serde_json::json!({
            "usageMetadata": {
                "promptTokenCount": 8,
                "candidatesTokenCount": 12,
                "totalTokenCount": 20,
                "thoughtsTokenCount": 4,
                "cachedContentTokenCount": 2
            }
        });
        let u = extract_usage_from_json(&body).expect("usage");
        assert_eq!(u.prompt_tokens, 8);
        assert_eq!(u.completion_tokens, 12);
        assert_eq!(u.total_tokens, 20);
        assert_eq!(u.reasoning_tokens, Some(4));
        assert_eq!(u.cache_read_tokens, Some(2));
    }

    #[test]
    fn extract_usage_sse_openai_chat() {
        let raw = "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\
                   data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3,\"total_tokens\":8}}\n\
                   data: [DONE]\n";
        let u = extract_usage_from_sse(raw).expect("usage");
        assert_eq!(u.prompt_tokens, 5);
        assert_eq!(u.completion_tokens, 3);
        assert_eq!(u.total_tokens, 8);
    }

    #[test]
    fn extract_usage_sse_anthropic_merges_frames() {
        let raw = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":2,\"cache_creation_input_tokens\":1}}}\n\
                   data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"x\"}}\n\
                   data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n";
        let u = extract_usage_from_sse(raw).expect("usage");
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.completion_tokens, 7);
        assert_eq!(u.cache_read_tokens, Some(2));
        assert_eq!(u.cache_write_tokens, Some(1));
        // total = 10 + 1 + 2 + 7
        assert_eq!(u.total_tokens, 20);
    }

    #[test]
    fn extract_usage_sse_responses() {
        let raw = "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r1\"}}\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"usage\":{\"input_tokens\":40,\"output_tokens\":10,\"total_tokens\":50}}}\n";
        let u = extract_usage_from_sse(raw).expect("usage");
        assert_eq!(u.prompt_tokens, 40);
        assert_eq!(u.completion_tokens, 10);
        assert_eq!(u.total_tokens, 50);
    }

    #[test]
    fn extract_usage_sse_gemini() {
        let raw = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"a\"}]}}]}\n\
                   data: {\"usageMetadata\":{\"promptTokenCount\":3,\"candidatesTokenCount\":2,\"totalTokenCount\":5}}\n";
        let u = extract_usage_from_sse(raw).expect("usage");
        assert_eq!(u.prompt_tokens, 3);
        assert_eq!(u.completion_tokens, 2);
        assert_eq!(u.total_tokens, 5);
    }

    /// A request event that carries NO token data, mirroring the
    /// production hot path (RequestEvent.tokens is always None).
    fn dummy_request_event_no_tokens() -> RequestEvent {
        let mut ev = dummy_request_event();
        ev.tokens = None;
        ev
    }

    #[tokio::test]
    async fn write_capture_backfills_tokens_non_stream() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(pool.sqlite()).await.expect("migrate");
        let sink = OltpSink::new(Arc::new(pool.clone()));

        // Hot path writes the row with no token data.
        sink.write_request_event(&dummy_request_event_no_tokens())
            .await
            .expect("write event");

        let capture = ExchangeCapture {
            request_id: "req-1".to_string(),
            egress_method: "POST".to_string(),
            egress_path: "/v1/chat/completions".to_string(),
            egress_headers: vec![],
            egress_body: None,
            upstream_status: Some(200),
            upstream_resp_headers: vec![],
            upstream_resp_body: Some(
                "{\"id\":\"chatcmpl-1\",\"usage\":{\"prompt_tokens\":15,\"completion_tokens\":25,\"total_tokens\":40,\"prompt_tokens_details\":{\"cached_tokens\":5}}}"
                    .to_string(),
            ),
            client_resp_headers: vec![],
            client_resp_body: None,
            is_stream: false,
        };
        sink.write_capture(&capture).await.expect("write capture");

        let row =
            sqlx::query("SELECT prompt_tokens, completion_tokens, total_tokens, cache_read_tokens FROM request_logs WHERE request_id = ?1")
                .bind("req-1")
                .fetch_one(pool.sqlite())
                .await
                .expect("query");
        assert_eq!(row.get::<Option<i64>, _>("prompt_tokens"), Some(15));
        assert_eq!(row.get::<Option<i64>, _>("completion_tokens"), Some(25));
        assert_eq!(row.get::<Option<i64>, _>("total_tokens"), Some(40));
        assert_eq!(row.get::<Option<i64>, _>("cache_read_tokens"), Some(5));

        // Aggregates should now report the backfilled tokens.
        let now = Utc::now().to_rfc3339();
        let earlier = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let by_model = aggregate_by_model(&pool, &earlier, &now)
            .await
            .expect("agg");
        assert_eq!(by_model[0].prompt_tokens, 15);
        assert_eq!(by_model[0].completion_tokens, 25);
        assert_eq!(by_model[0].total_tokens, 40);
        assert_eq!(by_model[0].cache_read_tokens, 5);
    }

    /// Capture arrives BEFORE the `RequestEvent` (the ordering that
    /// broke before the upsert refactor: the placeholder `UPDATE`
    /// affected zero rows and the later `INSERT OR REPLACE` reset
    /// the token columns to NULL). After the fix, the recovered
    /// tokens must survive the eventual `RequestEvent` insert.
    #[tokio::test]
    async fn write_capture_then_request_event_preserves_tokens() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(pool.sqlite()).await.expect("migrate");
        let sink = OltpSink::new(Arc::new(pool.clone()));

        let capture = ExchangeCapture {
            request_id: "req-2".to_string(),
            egress_method: "POST".to_string(),
            egress_path: "/v1/chat/completions".to_string(),
            egress_headers: vec![],
            egress_body: None,
            upstream_status: Some(200),
            upstream_resp_headers: vec![],
            upstream_resp_body: Some(
                "{\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":11,\"total_tokens\":18}}"
                    .to_string(),
            ),
            client_resp_headers: vec![],
            client_resp_body: None,
            is_stream: false,
        };
        sink.write_capture(&capture).await.expect("write capture");

        // Now the terminal RequestEvent arrives with no token data
        // (mirrors the production hot path).
        let mut ev = dummy_request_event_no_tokens();
        ev.request_id = "req-2".to_string();
        sink.write_request_event(&ev).await.expect("write event");

        let row = sqlx::query(
            "SELECT prompt_tokens, completion_tokens, total_tokens \
             FROM request_logs WHERE request_id = ?1",
        )
        .bind("req-2")
        .fetch_one(pool.sqlite())
        .await
        .expect("query");
        assert_eq!(row.get::<Option<i64>, _>("prompt_tokens"), Some(7));
        assert_eq!(row.get::<Option<i64>, _>("completion_tokens"), Some(11));
        assert_eq!(row.get::<Option<i64>, _>("total_tokens"), Some(18));
    }

    #[tokio::test]
    async fn write_capture_backfills_tokens_streaming() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(pool.sqlite()).await.expect("migrate");
        let sink = OltpSink::new(Arc::new(pool.clone()));

        sink.write_request_event(&dummy_request_event_no_tokens())
            .await
            .expect("write event");

        let sse = "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\
                   data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{}}],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":6,\"total_tokens\":15}}\n\
                   data: [DONE]\n";
        let capture = ExchangeCapture {
            request_id: "req-1".to_string(),
            egress_method: "POST".to_string(),
            egress_path: "/v1/chat/completions".to_string(),
            egress_headers: vec![],
            egress_body: None,
            upstream_status: Some(200),
            upstream_resp_headers: vec![],
            upstream_resp_body: Some(sse.to_string()),
            client_resp_headers: vec![],
            client_resp_body: None,
            is_stream: true,
        };
        sink.write_capture(&capture).await.expect("write capture");

        let row =
            sqlx::query("SELECT prompt_tokens, completion_tokens, total_tokens FROM request_logs WHERE request_id = ?1")
                .bind("req-1")
                .fetch_one(pool.sqlite())
                .await
                .expect("query");
        assert_eq!(row.get::<Option<i64>, _>("prompt_tokens"), Some(9));
        assert_eq!(row.get::<Option<i64>, _>("completion_tokens"), Some(6));
        assert_eq!(row.get::<Option<i64>, _>("total_tokens"), Some(15));
    }

    #[tokio::test]
    async fn write_capture_missing_row_is_noop() {
        let pool = db::open_pool("sqlite::memory:").await.expect("pool");
        db::run_migrations(pool.sqlite()).await.expect("migrate");
        let sink = OltpSink::new(Arc::new(pool.clone()));

        // No request_logs row exists yet (capture racing ahead of
        // the RequestEvent). The token writeback is an upsert: it
        // inserts a minimal placeholder row carrying the recovered
        // usage so the subsequent `write_request_event` (when it
        // arrives) does not clobber the token columns.
        let capture = ExchangeCapture {
            request_id: "ghost".to_string(),
            egress_method: "POST".to_string(),
            egress_path: "/v1/chat/completions".to_string(),
            egress_headers: vec![],
            egress_body: None,
            upstream_status: Some(200),
            upstream_resp_headers: vec![],
            upstream_resp_body: Some(
                "{\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}"
                    .to_string(),
            ),
            client_resp_headers: vec![],
            client_resp_body: None,
            is_stream: false,
        };
        sink.write_capture(&capture).await.expect("write capture");
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_logs WHERE request_id = ?1")
                .bind("ghost")
                .fetch_one(pool.sqlite())
                .await
                .expect("count");
        assert_eq!(count, 1, "capture-stage upsert should have inserted the row");
        let prompt: Option<i64> =
            sqlx::query_scalar("SELECT prompt_tokens FROM request_logs WHERE request_id = ?1")
                .bind("ghost")
                .fetch_one(pool.sqlite())
                .await
                .expect("prompt");
        assert_eq!(
            prompt,
            Some(1),
            "recovered prompt_tokens must survive the upsert before RequestEvent arrives"
        );
    }
}
