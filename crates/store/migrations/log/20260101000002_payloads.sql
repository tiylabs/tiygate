-- Log schema (expand step): per-request full exchange payloads.
--
-- One-to-one with `request_logs` on `request_id`. Stored in a
-- separate table because payload bodies can be large; keeping them
-- out of the hot `request_logs` row keeps list / aggregate queries
-- cheap and lets the detail view fetch payloads on demand.
--
-- All header/body columns are populated by the OLTP sink AFTER
-- redaction + truncation on the telemetry background task, so the
-- stored values are safe (no cleartext credentials) and bounded by
-- `raw_envelope_max_bytes`.

CREATE TABLE IF NOT EXISTS request_payloads (
    -- Matches request_logs.request_id (UUID v7). No FK constraint so
    -- the payload row can be written independently of the aggregated
    -- request_logs row (they arrive on the same telemetry bus but in
    -- separate messages).
    request_id TEXT PRIMARY KEY,
    -- Gateway -> Provider request (egress).
    egress_headers_json TEXT,
    egress_body TEXT,
    egress_body_truncated INTEGER NOT NULL DEFAULT 0,
    -- Provider -> Gateway response (upstream).
    upstream_status INTEGER,
    upstream_resp_headers_json TEXT,
    upstream_resp_body TEXT,
    upstream_resp_body_truncated INTEGER NOT NULL DEFAULT 0,
    -- Gateway -> Client response.
    client_resp_headers_json TEXT,
    client_resp_body TEXT,
    client_resp_body_truncated INTEGER NOT NULL DEFAULT 0,
    -- Whether the exchange used a streaming (SSE) response.
    is_stream INTEGER NOT NULL DEFAULT 0,
    -- Best-effort merged/structured SSE parse result (JSON text).
    sse_parsed_json TEXT,
    -- When the payload row was captured (RFC-3339, UTC).
    captured_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_request_payloads_captured_at ON request_payloads (captured_at);
