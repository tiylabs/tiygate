-- Persist per-hop execution attempts for request log drill-down.
--
-- `request_logs` remains the per-client-request aggregate row. This table
-- stores retry / fallback / circuit-breaker hop details keyed by the same
-- request id plus a monotonic hop number. Pipeline events can arrive out of
-- order, so the OLTP sink upserts rows without requiring a foreign key.

CREATE TABLE IF NOT EXISTS request_attempts (
    request_id TEXT NOT NULL,
    hop INTEGER NOT NULL,
    ts TEXT NOT NULL,
    stage TEXT NOT NULL,
    target TEXT NOT NULL,
    provider TEXT,
    model TEXT,
    egress_protocol TEXT,
    status TEXT NOT NULL,
    error_class TEXT,
    error TEXT,
    latency_ms BIGINT,
    fallback_decision TEXT,
    UNIQUE(request_id, hop)
);

CREATE INDEX IF NOT EXISTS idx_request_attempts_request_id ON request_attempts (request_id);
CREATE INDEX IF NOT EXISTS idx_request_attempts_ts ON request_attempts (ts);
CREATE INDEX IF NOT EXISTS idx_request_attempts_status ON request_attempts (status);
