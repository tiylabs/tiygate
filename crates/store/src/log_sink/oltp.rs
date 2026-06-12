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

use tiygate_core::{EventSink, PipelineEvent, RequestEvent};

use crate::db::DbPool;

/// An `EventSink` backed by the `request_logs` table.
pub struct OltpSink {
    pool: Arc<DbPool>,
}

impl OltpSink {
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self { pool }
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
        let res = sqlx::query(
            "INSERT OR REPLACE INTO request_logs (\
                request_id, ts, virtual_model, resolved_provider, resolved_model, account_label, \
                trace_id, span_id, traceparent, ingress_protocol, egress_protocol, \
                lossy, cache_hit, status, error_class, http_status, error_source, \
                total_latency_ms, upstream_latency_ms, queue_latency_ms, ttfb_ms, \
                prompt_tokens, completion_tokens, reasoning_tokens, cache_read_tokens, \
                cache_write_tokens, total_tokens, cost, api_key_id, client_ip, user_agent, \
                raw_envelope_json, redacted_headers_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
                     ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33)",
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

    async fn flush(&self) -> Result<(), tiygate_core::Error> {
        Ok(())
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
/// original request body and headers for debugging.
#[derive(Debug, Default, serde::Serialize)]
pub struct RequestReplay {
    pub request_id: String,
    pub raw_envelope_json: Option<String>,
    pub redacted_headers_json: Option<String>,
}

/// Fetch the raw envelope (redacted) for a given request id.
/// Used by the admin replay endpoint for failed/slow request
/// debugging (per §4.4 / §8 acceptance #8).
pub async fn get_request_replay(
    pool: &DbPool,
    request_id: &str,
) -> Result<Option<RequestReplay>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT request_id, raw_envelope_json, redacted_headers_json \
         FROM request_logs WHERE request_id = ?1",
    )
    .bind(request_id)
    .fetch_optional(pool.sqlite())
    .await?;
    if let Some(r) = row {
        Ok(Some(RequestReplay {
            request_id: r.get("request_id"),
            raw_envelope_json: r.get("raw_envelope_json"),
            redacted_headers_json: r.get("redacted_headers_json"),
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
}
