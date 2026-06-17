-- Per-request full exchange payloads (PostgreSQL).

CREATE TABLE IF NOT EXISTS request_payloads (
    request_id TEXT PRIMARY KEY,
    egress_headers_json TEXT,
    egress_body TEXT,
    egress_body_truncated INTEGER NOT NULL DEFAULT 0,
    upstream_status INTEGER,
    upstream_resp_headers_json TEXT,
    upstream_resp_body TEXT,
    upstream_resp_body_truncated INTEGER NOT NULL DEFAULT 0,
    client_resp_headers_json TEXT,
    client_resp_body TEXT,
    client_resp_body_truncated INTEGER NOT NULL DEFAULT 0,
    is_stream INTEGER NOT NULL DEFAULT 0,
    sse_parsed_json TEXT,
    captured_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_request_payloads_captured_at ON request_payloads (captured_at);
