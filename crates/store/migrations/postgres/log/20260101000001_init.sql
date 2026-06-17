-- Log schema (PostgreSQL): aggregated request log.

CREATE TABLE IF NOT EXISTS request_logs (
    request_id TEXT PRIMARY KEY,
    ts TEXT NOT NULL,
    virtual_model TEXT NOT NULL,
    resolved_provider TEXT,
    resolved_model TEXT,
    account_label TEXT,
    trace_id TEXT,
    span_id TEXT,
    traceparent TEXT,
    ingress_protocol TEXT NOT NULL,
    egress_protocol TEXT,
    lossy INTEGER NOT NULL DEFAULT 0,
    cache_hit TEXT,
    status TEXT NOT NULL,
    error_class TEXT,
    http_status INTEGER,
    error_source TEXT,
    total_latency_ms INTEGER NOT NULL DEFAULT 0,
    upstream_latency_ms INTEGER NOT NULL DEFAULT 0,
    queue_latency_ms INTEGER NOT NULL DEFAULT 0,
    ttfb_ms INTEGER,
    prompt_tokens INTEGER,
    completion_tokens INTEGER,
    reasoning_tokens INTEGER,
    cache_read_tokens INTEGER,
    cache_write_tokens INTEGER,
    total_tokens INTEGER,
    cost INTEGER,
    api_key_id TEXT,
    client_ip TEXT,
    user_agent TEXT,
    raw_envelope_json TEXT,
    redacted_headers_json TEXT
);

CREATE INDEX IF NOT EXISTS idx_request_logs_ts ON request_logs (ts);
CREATE INDEX IF NOT EXISTS idx_request_logs_virtual_model ON request_logs (virtual_model);
CREATE INDEX IF NOT EXISTS idx_request_logs_provider ON request_logs (resolved_provider);
CREATE INDEX IF NOT EXISTS idx_request_logs_api_key ON request_logs (api_key_id);
CREATE INDEX IF NOT EXISTS idx_request_logs_status ON request_logs (status);
