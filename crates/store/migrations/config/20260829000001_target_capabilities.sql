-- Target capability discovery state. Capability values and evidence remain
-- JSON so new registry entries do not require a schema migration.
CREATE TABLE IF NOT EXISTS target_capability_profiles (
    target_key TEXT PRIMARY KEY,
    identity_version INTEGER NOT NULL,
    provider_id TEXT NOT NULL,
    credential_scope_fingerprint TEXT NOT NULL,
    canonical_api_base TEXT NOT NULL,
    protocol_suite TEXT NOT NULL,
    endpoint_name TEXT NOT NULL,
    endpoint_version TEXT NOT NULL,
    dialect_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    profile_status TEXT NOT NULL,
    resolved_capabilities_json TEXT NOT NULL,
    observations_json TEXT NOT NULL,
    last_probe_suite_version INTEGER,
    last_successful_probe_at TEXT,
    last_probe_error_class TEXT,
    last_probe_error_redacted TEXT,
    fresh_until TEXT,
    stale_until TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS target_capability_overrides (
    target_key TEXT NOT NULL,
    capability_id TEXT NOT NULL,
    state TEXT NOT NULL,
    value_json TEXT,
    reason TEXT NOT NULL,
    actor TEXT NOT NULL,
    expires_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (target_key, capability_id)
);

CREATE TABLE IF NOT EXISTS target_probe_jobs (
    id TEXT PRIMARY KEY,
    target_key TEXT NOT NULL,
    probe_set_json TEXT NOT NULL,
    probe_set_hash TEXT NOT NULL,
    status TEXT NOT NULL,
    priority INTEGER NOT NULL,
    attempt_count INTEGER NOT NULL,
    max_attempts INTEGER NOT NULL,
    next_attempt_at TEXT NOT NULL,
    lease_owner TEXT,
    lease_until TEXT,
    last_error_class TEXT,
    last_error_redacted TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (target_key, probe_set_hash)
);

CREATE INDEX IF NOT EXISTS idx_target_probe_jobs_ready
    ON target_probe_jobs (status, next_attempt_at, priority);

CREATE TABLE IF NOT EXISTS capability_epoch (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    epoch INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);

-- Installation-scoped HMAC material for stable credential-scope
-- fingerprints. The value is always encrypted with the dedicated
-- target-key-hmac/v1 purpose; no raw secret is persisted.
CREATE TABLE IF NOT EXISTS installation_secrets (
    name TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    encrypted_value TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS capability_probe_budgets (
    scope TEXT NOT NULL,
    day TEXT NOT NULL,
    used INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (scope, day)
);
