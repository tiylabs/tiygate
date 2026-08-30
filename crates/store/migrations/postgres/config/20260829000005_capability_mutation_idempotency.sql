-- Idempotency records for capability-control mutations. The response is a
-- bounded, already-redacted JSON envelope; credentials and request bodies are
-- never stored here.
CREATE TABLE IF NOT EXISTS capability_mutation_idempotency (
    operation TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    response_status INTEGER,
    response_json TEXT,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    PRIMARY KEY (operation, idempotency_key)
);

CREATE INDEX IF NOT EXISTS idx_capability_mutation_idempotency_expiry
    ON capability_mutation_idempotency (expires_at);
