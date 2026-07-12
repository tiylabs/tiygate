-- Shared OAuth access-token state. Refresh tokens remain in the encrypted
-- providers.encrypted_oauth_meta blob for backwards compatibility.
CREATE TABLE IF NOT EXISTS oauth_access_tokens (
    provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
    encrypted_access_token TEXT NOT NULL DEFAULT '',
    access_expires_at TEXT,
    credential_version BIGINT NOT NULL DEFAULT 0,
    last_refresh_at TEXT,
    next_keepalive_at TEXT,
    next_retry_at TEXT,
    failure_count INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_oauth_access_tokens_keepalive
    ON oauth_access_tokens (next_keepalive_at);
