# TiyGate Log Schema

The `request_logs` table in the `log` migration sequence is the
canonical store of per-request observability data. This document
is the source-of-truth field reference.

## Storage layout

| Backend | Status | Notes |
| --- | --- | --- |
| **SQLite** | Phase 4 default | Single table with a `ts` index; `journal_mode=WAL` |
| **PostgreSQL** | Reserved for Phase 5 | `PARTITION BY RANGE(ts)`; trait interface ready |

A single retention cleanup task periodically deletes rows
older than `TIYGATE_LOG_RETENTION_DAYS` (default 30, set to 0 to
disable).

## Columns

| Column | Type | Description |
| --- | --- | --- |
| `request_id` | TEXT PK | Gateway-side request id (UUID v7) |
| `ts` | TEXT (RFC-3339) | Request completion time (UTC) |
| `virtual_model` | TEXT | Model name from the client |
| `resolved_provider` | TEXT | Provider id used by the gateway |
| `resolved_model` | TEXT | Model name sent to the upstream |
| `account_label` | TEXT | Multi-account routing label, if any |
| `tenant_id` | TEXT | Reserved (§3.9), always NULL in Phase 4 |
| `trace_id` | TEXT | W3C trace id, when present |
| `span_id` | TEXT | W3C span id, when present |
| `traceparent` | TEXT | The full `traceparent` header value |
| `ingress_protocol` | TEXT | `suite/name/version` triple of the ingress codec |
| `egress_protocol` | TEXT | Same triple for the upstream call |
| `lossy` | INTEGER (0/1) | Whether the conversion dropped fields |
| `cache_hit` | TEXT | `hit` / `miss` / `n/a` (embeddings only) |
| `status` | TEXT | `ok` / `error` / `cancelled` |
| `error_class` | TEXT | `transient` / `rate_limited` / `auth` / `bad_request` / `lossy` |
| `http_status` | INTEGER | Upstream HTTP status (if any) |
| `error_source` | TEXT | `gateway` / `upstream` (per §3.5) |
| `total_latency_ms` | INTEGER | End-to-end latency in milliseconds |
| `upstream_latency_ms` | INTEGER | Time spent on the upstream call |
| `queue_latency_ms` | INTEGER | Time spent waiting on the concurrency semaphore |
| `ttfb_ms` | INTEGER | Time-to-first-byte for streaming responses |
| `prompt_tokens` | INTEGER | Input tokens (chat / completion) |
| `completion_tokens` | INTEGER | Output tokens |
| `reasoning_tokens` | INTEGER | Reasoning / thinking tokens (o1-style) |
| `cache_read_tokens` | INTEGER | Prompt-cache read tokens |
| `cache_write_tokens` | INTEGER | Prompt-cache write tokens |
| `total_tokens` | INTEGER | Sum of the above |
| `cost` | INTEGER | Micro-USD; always `NULL` until a `PriceProvider` is wired |
| `api_key_id` | TEXT | The caller-side API key id (from `api_keys.id`) |
| `client_ip` | TEXT | Downstream client IP (best-effort) |
| `user_agent` | TEXT | Downstream client user agent |
| `raw_envelope_json` | TEXT | Full `RawEnvelope` snapshot (JSON) |
| `redacted_headers_json` | TEXT | Already-redacted header set (JSON) |

## Indexes

| Index | Columns | Notes |
| --- | --- | --- |
| `idx_request_logs_ts` | `(ts)` | Drives retention cleanup and stats time-window queries |
| `idx_request_logs_virtual_model` | `(virtual_model)` | Stats / dashboard by model |
| `idx_request_logs_provider` | `(resolved_provider)` | Stats by provider |
| `idx_request_logs_api_key` | `(api_key_id)` | Stats by API key |
| `idx_request_logs_status` | `(status)` | Filter ok vs. error in the dashboard |

## Event model (Rust type)

The Rust [`RequestEvent`](../../crates/core/src/telemetry/mod.rs)
mirrors these columns. The `OltpSink` in
`crates/store/src/log_sink/oltp.rs` performs the row conversion.

## Retention semantics

The retention task runs every `log_retention_interval_secs`
(default 3600). It executes:

```sql
DELETE FROM request_logs WHERE ts < ?1
```

where `?1` is `now - log_retention_days` (default 30 days). The
task is started automatically on `app::new()` and aborted on
`SIGTERM` as part of the graceful drain.

## Redaction contract

`raw_envelope.headers` is **always** redacted before insertion;
the `Authorization`, `Cookie`, and any header matching the
substring list (`token`, `secret`, `password`, `credential`) is
replaced with `[REDACTED]`. The cleartext is never persisted to
disk.

Body bytes larger than `TIYGATE_RAW_ENVELOPE_MAX_BYTES` (default
256 KiB) are truncated and the `truncated` flag is set on the
sibling `raw_envelope_json` payload. Inline base64 media is
stripped by default; switch on
`TIYGATE_RAW_ENVELOPE_CAPTURE_MEDIA=1` to keep the full payload
(useful for deep debugging at the cost of storage).
