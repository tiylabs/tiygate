-- Config schema: providers, routes, api_keys, config_epoch, settings.
-- Each table lives in the *config* migration sequence; the log
-- sequence (../log) is independent (design doc §4.3 "配置表与
-- 日志表逻辑分离").

-- Providers: registered upstream providers.
CREATE TABLE IF NOT EXISTS providers (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    -- 'openai', 'anthropic', 'google', 'openai_compatible', 'bedrock', ...
    vendor TEXT NOT NULL,
    -- Default API base URL (overridable per route).
    api_base TEXT NOT NULL,
    -- Encrypted API key (AES-256-GCM, base64). Empty if OAuth-only.
    encrypted_api_key TEXT NOT NULL DEFAULT '',
    -- Per-provider authentication mode: 'api_key', 'oauth', 'iam', 'none'.
    auth_mode TEXT NOT NULL DEFAULT 'api_key',
    -- Optional OAuth metadata (encrypted JSON blob, base64).
    encrypted_oauth_meta TEXT NOT NULL DEFAULT '',
    -- Free-form metadata (json-as-text, e.g. organisation, project).
    metadata_json TEXT NOT NULL DEFAULT '{}',
    -- Reserved for §3.9 tenant scoping. Always NULL in Phase 4.
    tenant_scope TEXT,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Routes: virtual model name → ordered chain of provider targets.
CREATE TABLE IF NOT EXISTS routes (
    id TEXT PRIMARY KEY,
    virtual_model TEXT NOT NULL,
    -- JSON array of {provider_id, model_id, weight, account_label, ...}.
    targets_json TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    tenant_scope TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_routes_virtual_model ON routes (virtual_model);

-- API keys: caller-side credentials used to authenticate against
-- TiyGate. Storage holds a *hash* of the key; the cleartext key is
-- returned exactly once at creation time. §4.5.
CREATE TABLE IF NOT EXISTS api_keys (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    -- SHA-256 hash of the secret (hex, lowercase). Verification is
    -- constant-time over the hash; the cleartext never leaves the
    -- admin handler.
    key_hash TEXT NOT NULL UNIQUE,
    -- Optional quota spec (JSON).
    quota_json TEXT NOT NULL DEFAULT '{}',
    -- 'active' | 'disabled' (Phase 4: 创建-启用-删除 三态; 删除即失效).
    status TEXT NOT NULL DEFAULT 'active',
    tenant_id TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Config epoch: monotonically increasing version counter. Bumped on
-- every successful config write so the data plane can poll for
-- changes (design doc §5 "epoch 轮询").
CREATE TABLE IF NOT EXISTS config_epoch (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    epoch INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);

-- Settings: small key/value store for gateway-wide tunables
-- (log_retention_days, embedding_cache_ttl, etc.).
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Audit log: every admin write is recorded for traceability.
CREATE TABLE IF NOT EXISTS audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    target_type TEXT NOT NULL,
    target_id TEXT NOT NULL,
    details_json TEXT NOT NULL DEFAULT '{}',
    ts TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_audit_log_ts ON audit_log (ts);
